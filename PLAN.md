# Linux → Windows Remote Desktop (custom replacement for xfreerdp)

## Why

Current setup (`~/git/xfreerdp-aad`) uses stock `xfreerdp` (`/sec:aad /cert:tofu /multimon /sound:sys:pulse`) to
connect from this Nobara Linux workstation to a Windows host ("cwtrow") that sits behind a network KVM. It works,
but general-purpose RDP isn't tuned for this specific setup, and we want to see if a purpose-built client/host pair
can beat it on latency/smoothness while keeping (or improving) multi-monitor, audio, and clipboard support.

This is a **from-scratch custom protocol**, not a FreeRDP fork — we're intentionally not bound to RDP semantics.

## Hard constraints (from requirements discussion)

- **Performance**: must feel at least as responsive as current RDP. This is the primary bar — if it's not
  noticeably better or at parity, the project isn't worth maintaining.
- **Multi-monitor fullscreen**: client is a triple-monitor Linux desktop —
  - `DP-3`: 5120×1440 (ultrawide)
  - `DP-2`: 2560×1440
  - `HDMI-1`: 1080×1920 (portrait)
  All three must be filled, borderless, matching physical arrangement.
- **Host display topology**: the host is only physically connected to a **network KVM with a single captured
  output**. It has no real multi-monitor hardware. The host must synthesize **3 virtual monitors** at the client's
  exact resolutions/positions so Windows apps can be dragged across them normally, the same way `xfreerdp /multimon`
  does today.
- **Audio**: host → client playback (system/app audio), not client → host (no mic requirement stated).
- **Clipboard**: bidirectional text, images, and files.

## Known unknowns — Phase 0 findings (resolved 2026-08-11)

These weren't established in the planning conversation and materially changed the design. Findings below; the
resulting architecture changes are folded into the sections after this one.

1. **Host GPU / hardware encode.** Not pinned down — the host (`cwtrow`, 192.168.1.55) is only reachable over
   RDP (port 3389; no SSH/WinRM open), so it can't be queried directly without an interactive AAD login, and the
   explicit direction from this discussion is to **not** hardcode to specific hardware anyway: the encoder must be
   auto-detected at runtime on whatever host it happens to run on. Resolution: use Media Foundation's `MFTEnumEx`
   (category `MFT_CATEGORY_VIDEO_ENCODER`) to enumerate available hardware encoder MFTs at startup, try in priority
   order NVENC → Quick Sync (MFX/VPL) → AMF → software x264, and log whichever was selected. No blocking unknown
   left — this is now a design requirement, not a research question.
2. **Windows version on the host — confirmed Windows 11.** This settles the capture-API question (below).
3. **Network path — confirmed LAN-only, single-user**, per this discussion. No NAT traversal, no roaming, no WAN
   fallback needed. This directly informed the transport decision below.
4. **Auth/security model** — unchanged from the original plan: QUIC/TLS with a pre-shared client certificate.
   Not revisited in this Phase 0 pass.
5. **Windows driver signing** — resolved by reusing an already-signed community IDD rather than writing one; see
   "Virtual display driver" below. No test-signing mode needed on the host.

### Capture API — Desktop Duplication API, not Windows.Graphics.Capture

Windows 11 makes both available, but DDA wins for this use case:

- **WGC's yellow capture-indicator border is disqualifying here.** For virtual/headless monitors, the *only*
  viewer of that output is our own client — the border isn't a harmless on-screen hint on a monitor someone's
  looking at, it gets composited straight into the stream. Suppressing it (`IsBorderRequired = false`) requires
  MSIX package identity and a one-time user-consent prompt (`GraphicsCaptureAccess.RequestAccessAsync` with
  `GraphicsCaptureAccessKind.Borderless`) — real friction for a background Win32 agent.
- **DDA is the same choice Sunshine makes** for the identical IDD-capture-encode pipeline, which is a good proxy
  for "this combination is well-trodden in practice."
- DDA's GPU-affinity requirement (capturing app must run on the same adapter driving the display) isn't a problem
  since this is a single-GPU host and the IDD-created virtual monitors are driven by that same adapter.
- WGC's main advantage (cross-GPU capture with no setup) isn't needed here.

Decision: capture each synthetic monitor via `IDXGIOutputDuplication` (DDA), one duplication session per virtual
monitor.

### Virtual display driver — reuse, don't write one

[VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) (successor to
itsmikethetech's original, MIT-licensed) fits the "3 virtual monitors matching client topology" requirement almost
exactly:

- Signed via SignPath (free open-source code signing) — installs cleanly on Windows 11 without test-signing mode.
- Supports arbitrary custom resolutions/refresh rates per virtual monitor (640×480 up to 8K, refresh rates
  including odd ones), configured via `vdd_settings.xml` plus a `qres`-based CLI path for changing resolution at
  runtime — this is exactly the hook the host agent needs to reconfigure monitors on topology-negotiation from the
  client, without touching driver code.
- Already has a documented Sunshine integration path (`SunshineIntegration.bat` + `qres`) that does the same
  "set virtual monitor resolution to match what the remote client just told me" job we need — good template to
  copy rather than design from scratch.
- Known caveat: a still-open issue on Windows 11 24H2/25H2 where an IDD virtual display can't be set as the
  **primary** display (desktop mode doesn't apply until the user manually touches the refresh-rate dropdown in
  Settings). Doesn't block us — none of the 3 virtual monitors need to be primary — but worth re-testing once the
  host agent exists, since a real physical/RDP-session default display still needs to exist for the host to boot
  headless.

Decision: build on this driver instead of writing a custom IddCx implementation. Revisit only if the runtime
resolution-change path proves too slow/unreliable for topology renegotiation in practice.

### Decode-to-scanout on GNOME/Wayland — achievable, no DRM lease needed

The client's actual Mutter version (GNOME Shell 50.3) is well past the relevant fix: GNOME 49 landed improved
direct-scanout handling for fullscreen dmabuf/YUV surfaces via `wp_viewporter` (opaque-format substitution on the
primary plane), meaning a fullscreen video-shaped surface can hit the direct scanout path without the compositor
ever compositing it through the 3D engine. Mutter's DRM lease protocol support (which would allow bypassing the
compositor's output entirely) is still a draft/WIP merge request — not something to depend on.

Practical approach: present each monitor's decoded frame as a fullscreen `wl_surface` using YUV dma-buf import,
sized/scaled via `wp_viewporter`, and avoid anything that forces compositing (no blur/shadow/rounded-corner
effects on that surface, true unredirected fullscreen). This should get close to decode-to-scanout without needing
a dedicated DRM-lease/session-takeover path — worth a small Phase 1 spike to confirm in practice, but no longer an
open architectural question.

**Blocking gap found on this client, not a Wayland problem**: NVIDIA doesn't expose VA-API natively — decode via
VAAPI requires the `nvidia-vaapi-driver` shim (upstream project: elFarto/nvidia-vaapi-driver), packaged for
Nobara's own repos (not RPM Fusion) as `libva-nvidia-driver` (confirmed available: `libva-nvidia-driver-0.0.17-1.fc44`
via `dnf list --available`). **Not currently installed** on this machine — `rpm -qa` shows no VAAPI packages and no `vainfo` binary.
Driver 610.57.04 is well past the versions that introduced (555.58) and stabilized (580.x) explicit sync on
Wayland via `linux-drm-syncobj-v1`, so no explicit-sync concerns. Action item for Phase 1: `dnf install
libva-nvidia-driver` before attempting the decode spike.

### Transport — reconsidered: still QUIC, but datagrams for media, not streams

The original plan's "QUIC, per-monitor streams for video" needs one correction. Research into QUIC-for-real-time-media
turned up a specific, well-documented problem: QUIC's default congestion control (NewReno in most implementations)
actively fights an application-level real-time congestion controller — bitrate oscillates as the two controllers
react to each other, and it gets materially worse under any real congestion. This is very likely *why* the
established prior art here (Moonlight/Sunshine, Parsec) all use raw/custom UDP protocols instead of QUIC or TCP for
their video path, not an oversight on their part.

Two options considered:
- **Drop QUIC entirely**, go TCP (control) + raw UDP with custom framing (media), matching Moonlight/Sunshine/Parsec
  exactly. Most latency-predictable, most implementation work (custom framing, ordering, loss handling, and our own
  encryption layer since we lose "TLS for free").
- **Keep QUIC, but move video/audio onto QUIC unreliable datagrams (RFC 9221) instead of streams**, and keep
  reliable ordered streams only for control/clipboard/file-transfer. This sidesteps the specific problem (stream
  retransmission head-of-line-blocking) without losing single-connection/single-library/TLS-for-free simplicity.

Decision: **keep QUIC via `quinn`** (pure Rust, mature, has first-class datagram support — `msquic`'s Rust bindings
are comparatively immature, and pure-Rust avoids an FFI/build-complexity tax on both host and client). Send
video/audio over datagrams, not streams. Given the confirmed LAN-only/single-user scope, default congestion control
is very unlikely to be a real bottleneck (LAN bandwidth headroom vs. our target bitrate is large) — leave it on
defaults for the Phase 1 MVP and only invest in a custom/pluggable congestion controller if benchmarking against
`xfreerdp` in Phase 1 shows it's actually limiting.

## Architecture

Two independent applications, talking a custom protocol over the LAN:

```
┌─────────────────────────┐                      ┌──────────────────────────┐
│   Host agent (Windows)   │                      │   Client app (Linux)      │
│                          │                      │                          │
│  Virtual Display Driver  │◄── topology nego ────│  Monitor topology probe  │
│  (3 synthetic monitors)  │                      │  (xrandr/wlr-output-mgmt)│
│                          │                      │                          │
│  Per-monitor capture     │                      │  Per-monitor window      │
│  (Desktop Duplication)   │                      │  (fullscreen, borderless)│
│         │                │                      │         ▲                │
│         ▼                │                      │         │                │
│  HW encode (auto-detect: │── video (unreliable, │  HW decode (VAAPI via    │
│   NVENC/QSV/AMF/x264)    │  per-monitor datagram)│  nvidia-vaapi-driver)   │
│                          │                      │                          │
│  WASAPI loopback         │── audio (unreliable) ►  PipeWire playback       │
│  → Opus encode           │                      │                          │
│                          │                      │                          │
│  SendInput injection     │◄── input (reliable) ──  evdev/libinput capture  │
│                          │                      │                          │
│  Clipboard/file server   │◄─► clipboard/file ───►  Clipboard/file client   │
│                          │    (reliable, chunked)│                         │
└─────────────────────────┘                      └──────────────────────────┘
```

### Transport

**QUIC via `quinn`** (pure Rust) as the single transport — see "Transport — reconsidered" under Phase 0 findings
above for the full reasoning:

- One QUIC connection: reliable ordered streams for control (handshake, topology negotiation, clipboard, file
  transfer), unreliable **datagrams** (RFC 9221, not streams) for per-monitor video and for audio, to avoid
  stream-retransmission head-of-line-blocking fighting the real-time bitrate.
- Gets us encryption (TLS 1.3 is mandatory in QUIC) "for free" — solves part of the auth/security gap left by
  dropping AAD.
- `quinn` over `msquic`: pure Rust, no FFI/build tax on either host or client, more mature Rust bindings.
- Default congestion control to start; revisit only if Phase 1 benchmarking shows it's limiting on the LAN.

### Video pipeline

- **Per-monitor stream**, not one giant composited canvas — keeps encode/decode resolution sane (avoids ever
  needing to encode a single 5120+2560+1920-wide frame) and lets each monitor's stream independently adapt
  bitrate/framerate to its content.
- Host: capture via **Desktop Duplication API** (per synthetic monitor — confirmed over Windows.Graphics.Capture,
  see Phase 0 findings) → hardware encoder, auto-detected at runtime via Media Foundation `MFTEnumEx` in priority
  order NVENC → Quick Sync → AMF → software x264 fallback → H.264 or AV1 depending on encoder support and client
  decode capability.
- Client: hardware decode (VAAPI — requires the `nvidia-vaapi-driver` shim on NVIDIA, packaged as
  `libva-nvidia-driver` in Nobara's repos; **not yet installed on this client**, install before the Phase 1
  decode spike) → present directly to the relevant monitor's surface with minimal buffering. Decode-to-scanout is
  achievable via a fullscreen `wl_surface` + YUV dma-buf + `wp_viewporter`, riding Mutter's GNOME 49+ direct-scanout
  path (confirmed present on this client's GNOME Shell 50.3) — no DRM lease needed (still WIP in Mutter). See Phase
  0 findings for detail; still worth a small Phase 1 spike to confirm in practice.

### Display topology negotiation

On connect, client enumerates its outputs (resolution, position, refresh rate, DPI) and sends them to the host. Host
driver creates/reconfigures 3 virtual monitors to match. This is the same problem `/multimon` already solves in RDP
— we're reimplementing it, not inventing new UX behavior. Reuse over reinvention: build on
[VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) (MIT, signed,
Windows 10/11) rather than writing a custom IddCx driver — see Phase 0 findings above for why and its runtime
resolution-change hook (`vdd_settings.xml` + `qres`-based CLI, same mechanism it already uses for Sunshine
integration).

### Clipboard & file transfer

- Text and images: mime-typed payload over the reliable control stream, host and client each watch their local
  clipboard for changes and push updates.
- Files: chunked transfer over the same reliable stream (or a dedicated stream to avoid blocking clipboard text
  updates behind a large file), with a simple progress/cancel protocol. Treat this as its own subsystem — it's
  the least "solved by existing libraries" part of the whole project.

### Security

Dropping AAD auth means we need an explicit replacement. Since this is a single-user, LAN-scoped tool (not an
enterprise deployment), recommend the simplest thing that isn't actually insecure:

- QUIC/TLS with a pre-shared client certificate (host only accepts connections presenting a specific cert we
  provision once) — avoids building any kind of account/password system.
- Alternative if WAN access is ever needed later (network path is confirmed LAN-only for now — see Phase 0
  findings #3): tunnel over WireGuard and trust the tunnel, skip app-level auth entirely.

## Phase 1 findings — decode-to-scanout spike (2026-08-11)

The Phase 0 assessment that decode-to-scanout is "achievable, no DRM lease needed" was based on Mutter's
compositor-side support (confirmed present) but didn't account for a **driver-side gap specific to this NVIDIA
setup**: dma-buf/DRM-prime export from a VAAPI decode surface does not currently work with
`libva-nvidia-driver` 0.0.17 (elFarto/nvidia-vaapi-driver, latest upstream release) + NVIDIA driver 610.57.04 on
this RTX 4070 Ti.

Confirmed via three independent code paths, all failing the same underlying operation:
- **GStreamer** (`va` plugin, `vah264dec`): negotiating `video/x-raw(memory:DMABuf)` fails outright — the decoder's
  dynamic caps never advertise the DMABuf memory feature for this backend. Plain `waylandsink` silently falls back
  to SHM (system-memory copy) buffers, which by construction can never be scanned out — confirmed via `drm_info`
  plane snapshots (primary plane stayed at desktop XR24/XB24 format and desktop resolution throughout playback,
  never switched to NV12 at video resolution).
- **mpv** (`--vo=dmabuf-wayland --hwdec=vaapi`): surface-format probing fails for every format (NV12, P010, P012,
  YUV444P) with `vaExportSurfaceHandle() failed (invalid VASurfaceID)`, falls back to `drmprime` hwdec, which also
  fails, ultimately aborting with no video decoded at all.
- **ffmpeg** (`-vf hwmap=derive_device=drm`, i.e. `av_hwdevice_ctx_create_derived`): fails with
  `-38 Function not implemented`.

This isn't a KMS/atomic-modesetting problem (confirmed working independently via `drm_info` — full plane/CRTC/
connector enumeration succeeds, `nvidia-drm.modeset` is on, as it must be for this Wayland session to run at all)
and it isn't a stale-driver problem (0.0.17 is the current upstream release). Plain VAAPI decode itself works fine
(`vainfo` lists full H.264/HEVC/AV1/VP9 VLD decode entrypoints, GStreamer's `vah264dec` decodes correctly to NV12)
— only the surface **export** step (`vaExportSurfaceHandle`, needed to hand the decoded frame to Wayland as a
dma-buf for `zwp_linux_dmabuf_v1` / direct scanout) fails.

**Resolution for Phase 1 MVP**: don't block on this. Use hardware-accelerated VAAPI decode + **composited**
presentation (copy decoded NV12 frame back to system memory / SHM, as GStreamer's `waylandsink` already does
successfully) rather than true unredirected direct scanout. This still gets HW decode acceleration for the
expensive part (H.264/HEVC/AV1 decode), costs one memcpy per frame (~3MB for 1080p60, cheap on modern hardware),
and is a proven-working path (confirmed via the `waylandsink` SHM fallback actually presenting frames). True
decode-to-scanout becomes a stretch optimization for a later phase (revisit once end-to-end latency is measured
against `xfreerdp-aad` — if composited presentation already meets the performance bar, direct scanout may not be
worth the yak-shave). Track the underlying export bug upstream against elFarto/nvidia-vaapi-driver separately.

## Phase 1 findings — Session 0 isolation blocks SSH-launched capture (2026-08-11)

First live end-to-end test (real client on this Linux machine, real host on cwtrow over the LAN)
got through the full QUIC handshake and control-plane exchange (`ClientHello`/`ServerHello`,
`Topology`/`TopologyAck`) successfully, then `IDXGIOutputDuplication::DuplicateOutput` failed with
`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` (0x887A0022). Root cause: the host process was launched via
SSH, and Windows' OpenSSH server runs as a service — anything it spawns lands in **Session 0**
(`Get-Process` showed `SI 0`), which has no attached display by design (Session 0 isolation, since
Vista). DDA (and `SendInput`/input injection) fundamentally require running in an interactive
session (the console session, or an RDP session), not Session 0.

This is a testing-methodology artifact of using raw SSH exec, not a code defect — but it points at
a real Phase 5 packaging requirement: a production host agent needs the well-known
service-launches-into-interactive-session pattern (`WTSQueryUserToken` + `CreateProcessAsUser`,
same approach Sunshine/Parsec use) rather than running as a plain SSH-spawned or Session-0 service
process. Deferred to Phase 5 ("host as a Windows service/startup app") per the existing phase plan
— not a Phase 1 blocker, just means **Phase 1 testing needs the host binary launched from within an
actual interactive session** (the existing RDP session, or the console), not over SSH.

**Follow-up (2026-08-11, later same day) — confirms it's specifically about the console/physical
session, not just "any interactive session":** running the host from within a separate Windows RDP
session (`rdp-tcp#0`, a distinct logical session from the console) got past Session-0 isolation but
DDA still failed (`IMFActivate::ActivateObject` / capture errors depending on which encoder). It only
started working once the host was launched while the user was connected through the **network KVM**
(mirroring the actual physical/console session, session 1) — confirmed by real encoded H.264
datagrams arriving at the client. Once that KVM connection was closed, frames stopped again (console
session no longer has an active interactive user). This matches the DDA/Sunshine-style constraint
exactly: capture requires an active, unlocked **console** session specifically, not just any
interactive Windows session. Reinforces the Phase 5 auto-logon-or-service-launch requirement noted
above — for now, Phase 1 testing needs the KVM (or console) connection held open, not the separate
RDP session.

Also found and fixed two real bugs surfaced by this same test, both in the Media Foundation encode
path (`host/src/encode.rs`): (1) encoder auto-detection now falls through to the next candidate on
`ActivateObject`/`SetInputType` failure instead of hard-failing on the first pick — needed in
practice since this host's Quick Sync MFT fails to activate and its AMD MFT is async-only (neither
of which `MFTEnumEx` surfaces up front); and (2) `ProcessOutput` needs a caller-allocated output
sample when the MFT doesn't set `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`, which the software H.264
Encoder MFT (the working fallback) requires — omitting it produced `E_INVALIDARG` on every frame and,
once the encoder's internal buffer backed up from never being drained, cascading `ProcessInput`
failures too.

## Phase 1 findings — client decode/present working end-to-end (2026-08-11)

Built the client side (`client/src/decode.rs`): `gstreamer-rs` with an `appsrc` fed from reassembled
QUIC datagrams, `h264parse ! vah264dec ! waylandsink` (composited presentation, per the earlier
decode-to-scanout finding), `queue` added between `appsrc` and `h264parse` per GStreamer's own
"add queues" warning for live sources. Confirmed the host's H.264 output is Annex-B byte-stream
(`00 00 00 01` start codes) by inspecting real captured bytes rather than guessing.

Hit and fixed three more real bugs before getting a picture on screen, in order:
1. **No periodic keyframes.** The encoder only emitted its first IDR/SPS/PPS at startup; any client
   connecting later (or any datagram loss) had no reference frame to decode against. Fixed by
   setting `MF_MT_MAX_KEYFRAME_SPACING` to force a keyframe every ~2s.
2. **Resolution mismatch (false lead, but a real bug worth keeping fixed).** Encoder media types
   were hardcoded to 1920x1080. A diagnostic query for the "real" resolution returned 1024x768, but
   that was measured from a *different, non-interactive SSH session* than the one actually running
   DDA — `System.Windows.Forms.Screen` reports a fallback size for sessions with no real attached
   display. The true console-session resolution genuinely was 1920x1080, so this wasn't the black-
   screen cause — but hardcoding was still wrong in principle, so `capture.rs` now queries the real
   resolution via `IDXGIOutput::GetDesc()` from within the actual capturing process/session, which
   self-corrects regardless of what any external diagnostic reports.
3. **The real cause: `ReleaseFrame()` called before the CPU actually read the data.** The original
   capture path did `CopyResource` (source → intermediate texture) → `Flush()` → `ReleaseFrame()`,
   then read the intermediate texture *later*, in a separate function call. `Flush()` only submits
   queued GPU work, it does not wait for completion — so `ReleaseFrame()` could let DDA reclaim the
   source texture before the copy actually executed on the GPU, and the later read consistently came
   back as all-zero (confirmed via raw-byte diagnostics: `min=max=avg=0` across 10 consecutive
   frames, with active window drags happening on screen). Fixed by consolidating to a single
   DDA-source → staging-texture copy, with the actual `Map()`-based CPU read (the real
   synchronization point) happening *before* `ReleaseFrame()` — not just before a `Flush()`. Also
   ruled out in the process: multi-GPU/wrong-adapter selection (this host has only one real
   adapter+output, `AMD Radeon(TM) Graphics` / `\\.\DISPLAY1`, confirmed via full `IDXGIFactory1`
   enumeration) and HDR/Advanced-Color format mismatch (confirmed `desc.Format` is
   `DXGI_FORMAT_B8G8R8A8_UNORM` as assumed).

End-to-end confirmed working: real screen content captured on cwtrow, encoded, streamed over QUIC,
decoded via VAAPI, and displayed fullscreen on this client's DP-2 monitor.

## Suggested tech stack

**Rust** for both sides is the recommendation: one language across host (Windows) and client (Linux), strong crates
for everything needed (`windows-rs` for Win32/DDA/SendInput, `quinn` for QUIC, `ffmpeg-next` or `gstreamer-rs` for
encode/decode pipelines, `pipewire-rs` for Linux audio), and memory safety for code that's parsing network input.
C++ is the fallback if a specific Windows capture/IDD API turns out to need it and doesn't have usable Rust bindings.

## Phased plan

- **Phase 0 — Research spike (no shippable code) — DONE (2026-08-11)**: see "Known unknowns — Phase 0 findings"
  above. Encoder is a runtime auto-detect (no host GPU assumption needed), capture API is Desktop Duplication API,
  IDD reuses VirtualDrivers/Virtual-Display-Driver, transport is QUIC/`quinn` with datagrams for media, and
  decode-to-scanout looks achievable on this client's Mutter version without a DRM lease. Two concrete carry-overs
  into Phase 1: install `libva-nvidia-driver` on the client (missing — VAAPI decode won't work without it), and
  spike the fullscreen-dmabuf-scanout path to confirm it in practice rather than just on paper.
- **Phase 1 — Single-monitor MVP — streaming + input DONE (2026-08-11), benchmark pending**: host streams one
  real display, client shows it fullscreen on one monitor (confirmed working end-to-end on real hardware — see
  Phase 1 findings above), keyboard/mouse input works (real evdev capture on the client forwarding relative mouse
  motion + key events to `SendInput` on the host; requires the client's running user to be in the Linux `input`
  group for `/dev/input/event*` access, not exclusive-grabbed for now — input still reaches the local desktop too,
  see `client/src/input_capture.rs`). Still open: benchmark latency against current `xfreerdp-aad` setup as the
  go/no-go gate for continuing to Phase 2.
- **Phase 2 — Multi-monitor**: extend to 3 synthetic host monitors matching client topology exactly, independent
  per-monitor streams.
- **Phase 3 — Audio**: WASAPI loopback → Opus → PipeWire playback.
- **Phase 4 — Clipboard & file transfer**: bidirectional text/image/file sync.
- **Phase 5 — Hardening**: reconnect/resilience, adaptive bitrate, auth/security finalization, packaging
  (host as a Windows service/startup app, client as a normal desktop app), replace `xfreerdp-aad` wrapper usage
  once at parity.

## Repo layout (proposed)

```
/host     Windows agent (capture, encode, virtual display driver integration, input injection)
/client   Linux client (window management, decode, input capture, clipboard)
/proto    Shared wire protocol definitions (message schemas for the control stream)
/docs     This plan, plus ADRs for decisions made during Phase 0
```

## Non-goals (for now)

- Enterprise/multi-user auth (AAD-equivalent) — single pre-shared-cert trust model only.
- WAN/NAT traversal — LAN-only until proven necessary.
- Mobile or non-Linux clients.
- Gamepad/controller passthrough (not in the stated requirements).
