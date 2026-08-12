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

## Architecture pivot (2026-08-12): Sunshine (host) + Moonlight protocol (client), custom shell

Phase 1's custom QUIC-based AV+input pipeline (capture/encode on `host/`, decode/present/input on
`client/`) got to a working end-to-end state on real hardware (see Phase 1 findings above), but hit
a wall that isn't fixable in our own code: `client/src/input_surface.rs`'s merged video+input
surface triggers a reproducible Mutter/GNOME Shell 50.3 compositor assertion failure
(`meta_window_set_stack_position_no_sync: assertion 'window->stack_position >= 0' failed`) whenever
`xdg_toplevel::set_fullscreen()` is called — confirmed via `journalctl`, confirmed not
output-specific (`set_fullscreen(None)` still fails) and not commit-timing-specific (deferring the
call past the first `configure`/`ack_configure` cycle still fails). This is a genuine compositor bug,
not a defect in our Wayland usage.

Decision: **stop building our own AV+input transport/capture/encode/decode stack.** Use
[Sunshine](https://github.com/LizardByte/Sunshine) on the host (cwtrow) for capture + encode +
streaming, and the Moonlight/GameStream wire protocol for the client, while still building a
**custom client application** ("shell") around it rather than using stock `moonlight-qt` — so we
keep control of presentation (our own Wayland surface, once workable), input capture, and whatever
future features (clipboard/file transfer) still make sense to layer on top.

Why this is a reasonable pivot, not just giving up:
- Sunshine independently arrived at the same host-side architecture this plan's Phase 0 already
  chose on its own: Desktop Duplication API capture, a virtual-display-driver integration (in fact
  the same [VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
  this plan picked in Phase 0 has a documented Sunshine integration path already), and
  runtime hardware-encoder auto-detection (NVENC/QuickSync/AMF/software). It's a mature, widely
  deployed implementation of exactly the host-side design this project independently converged on —
  offloading to it retires our DDA/Media-Foundation code (`host/src/capture.rs`, `host/src/encode.rs`)
  entirely, including the encoder-activation quirks we had to work around by hand
  (`host/src/encode.rs`'s fallthrough logic for QSV/AMD failures).
- Moonlight-protocol clients (`moonlight-qt` in particular) have already solved fullscreen
  presentation on GNOME/Mutter — the exact class of problem that stopped us. We don't need to
  reverse-engineer their fix; we get it by riding the same protocol/client lineage.

### Client foundation decision: FFI bindings to `moonlight-common-c`, not a fork or a Rust wrapper crate

Three options considered for the "custom shell":

1. **Fork `moonlight-qt`** — mature C++/Qt/SDL2 client, already solves fullscreen/pairing/reconnect.
   Rejected: full language switch, discards all already-validated Rust code
   (`client/src/decode.rs`'s GStreamer VAAPI pipeline, `client/src/input_surface.rs`'s Wayland
   surface management).
2. **[`moonlight-common-rust`](https://github.com/MrCreativ3001/moonlight-common-rust)** — a
   Sans-IO Rust reimplementation of the protocol. Rejected: v0.1.0, 0% documented, doesn't yet
   support GameStream-style pairing — too immature to build on confidently.
3. **Write our own Rust FFI bindings to [`moonlight-common-c`](https://github.com/moonlight-stream/moonlight-common-c)**
   (the actual reference C library moonlight-qt itself uses) — **chosen**. Its public API
   (`src/Limelight.h`) is a clean, deliberately FFI-friendly C API: `LiStartConnection` takes
   `DECODER_RENDERER_CALLBACKS`/`AUDIO_RENDERER_CALLBACKS`/`CONNECTION_LISTENER_CALLBACKS` structs;
   video arrives via a `submitDecodeUnit` callback carrying **raw Annex-B H.264/HEVC NAL data** —
   byte-for-byte the same format our existing `client/src/decode.rs` GStreamer pipeline
   (`h264parse` with `stream-format=byte-stream, alignment=au`) already consumes unchanged. Input is
   simple synchronous calls (`LiSendMouseMoveEvent`, `LiSendKeyboardEvent`, ...) that our existing
   `wl_pointer`/`wl_keyboard` handlers in `input_surface.rs` can call directly in place of our own
   QUIC control-stream sends. This keeps almost all of our already-debugged decode/presentation code
   and only replaces the transport/protocol layer. Tradeoff: `moonlight-common-c` doesn't implement
   the GameStream/Sunshine PIN-pairing HTTPS handshake itself (that's application-level in every
   client) — we implement that small piece ourselves — and we take on vendoring the C library plus
   its patched ENet fork ([cgutman/enet](https://github.com/cgutman/enet)) and building them via the
   `cc` crate.

### What gets retired vs kept

- **Retired**: `host/` entirely (Sunshine replaces DDA capture, MF encode, and `SendInput`
  injection), `proto/` and `netcommon/`'s QUIC/datagram video-and-input protocol (Sunshine/Moonlight
  has its own wire protocol — ENet control channel + RTP video/audio). `proto`/`netcommon` may still
  be revived later for Phase 4 clipboard/file-transfer if that ends up not fitting into the
  Moonlight protocol's channels — decide when Phase 4 is reached, not now.
- **Kept**: `client/src/decode.rs` (GStreamer VAAPI decode pipeline, unchanged — just fed from a
  different source), `client/src/input_surface.rs` (Wayland surface/presentation/input-capture,
  input send call sites swapped from our control-stream to `Li*` FFI calls), all the hard-won
  operational knowledge in the Phase 1 findings above (session-0/console-session constraints, VAAPI
  dma-buf export gap, release-build performance, etc.) — still true regardless of transport.

### Implementation status (2026-08-12): pivot complete and validated end-to-end on real hardware

All of the following is done and confirmed working against the real cwtrow/Sunshine setup, not
just compiled:

- **Sunshine installed on cwtrow** — silent MSI install via SSH (`msiexec /quiet`), runs as a
  Windows service. One real gotcha: its web UI has CSRF protection that rejects any origin other
  than `localhost` by default — accessing it via the LAN IP (`https://192.168.1.55:47990`) needs
  `csrf_allowed_origins = https://192.168.1.55:47990` added to `sunshine.conf`, then a service
  restart.
- **`client/moonlight-sys/`** — vendors `moonlight-common-c` + its `enet` fork + `nanors` (Reed-
  Solomon FEC) flat under `vendor/` (not a live git submodule, matching this repo's existing
  non-submodule structure). `build.rs` compiles the C sources via the `cc` crate and generates
  bindings for `Limelight.h` via the `bindgen` crate. One environment-specific snag: this
  machine's libclang has no unversioned resource-dir symlink, so bindgen couldn't find its own
  freestanding headers (`stdbool.h` etc.) until `build.rs` explicitly passes
  `-resource-dir=/usr/lib/clang/<version>`.
- **`client/src/pairing.rs`** — the GameStream PIN-pairing handshake (not implemented by
  `moonlight-common-c` itself). **Confirmed working against real Sunshine**: full RSA/AES
  challenge-response completed correctly on the first real attempt, verified via Sunshine's own
  `sunshine_state.json` showing our client cert stored under `named_devices`. One protocol
  behavior worth remembering: Sunshine's server *holds the first HTTP request open* (long-polls)
  until a human submits the matching PIN via its web UI — there's no server-side timeout, so the
  client must not set a request timeout on that call either.
- **`client/src/gamestream.rs`** — post-pairing HTTP session setup (`/applist`, `/launch`,
  `/resume`, ported from moonlight-qt's `nvhttp.cpp`). Picks Sunshine's built-in "Desktop" app
  automatically. **Real gotcha hit and fixed**: killing the client without calling
  `LiStopConnection()` (e.g. `SIGTERM` from `pkill`, which the process wasn't catching) leaves
  Sunshine's session marked active, and the next `/launch` fails with "an app is already running
  on this host" — `gamestream.rs` now retries via `/resume` on that specific error, and
  `main.rs` now catches `SIGTERM` in addition to `SIGINT`/Ctrl+C so this shouldn't recur in normal
  use. (`/resume`'s exact query-parameter contract is still not 100% nailed down — it worked via a
  Sunshine service restart clearing state in practice, but the `/resume` fallback path itself
  errored once with a connection reset; revisit if "already running" recurs.)
- **`client/src/stream.rs`** — the FFI glue: builds `SERVER_INFORMATION`/`STREAM_CONFIGURATION`,
  wires `DECODER_RENDERER_CALLBACKS.submitDecodeUnit` to reassemble `DECODE_UNIT`'s `LENTRY`
  buffer chain into contiguous Annex-B bytes and forward them into the *same* channel
  `input_surface.rs` already read from pre-pivot — so `decode.rs` needed zero changes. Also wires
  `CONNECTION_LISTENER_CALLBACKS` (stage/termination/connection-quality logging; `logMessage` is
  C-variadic and can't be implemented from stable Rust, so that one specific log source is lost).
  Audio callbacks are left null (Phase 3, not started).
- **`client/src/input_capture.rs`** — replaced the old evdev→PS/2-scancode table (for our retired
  `SendInput`-based host) with an evdev→Win32-VK-code table, since `LiSendKeyboardEvent2` expects
  VK codes, not scancodes.
- **`client/src/input_surface.rs`** — `wl_pointer`/`wl_keyboard` handlers now call
  `LiSendMousePositionEvent`/`LiSendMouseButtonEvent`/`LiSendHighRes(H)ScrollEvent`/
  `LiSendKeyboardEvent2` directly instead of forwarding over a channel to a QUIC sender — no more
  manual remote-resolution scaling needed either, since `LiSendMousePositionEvent` takes a
  reference-plane size and the host/library handle scaling.

**End-to-end real-hardware confirmation**: pairing succeeds, session launch succeeds, Sunshine
activates real **AMD hardware encoding** (`h264_amf` via AMF/D3D11) — something our own
Media-Foundation-driving code in the now-retired `host/src/encode.rs` structurally could not do
(its AMD path is async-only; our code only drove synchronous MFTs). `LiStartConnection` completes
all 12 stages, real decoded frames flow through the unchanged `decode.rs` VAAPI pipeline at the
correct resolution/format, and the window is visible and shows real video with real focus-scoped
input capture working.

**Old Mutter fullscreen-assertion finding, revisited**: re-enabling `toplevel.set_fullscreen()`
(previously disabled as a diagnostic — see the now-superseded finding above) turned out to be the
actual fix for the window-never-visible problem. The `meta_window_set_stack_position_no_sync`
assertion still appears in `journalctl` when it's called, but GLib assertions of this form log a
critical warning and continue rather than aborting — it was never actually the cause of the
invisible window, and skipping fullscreen entirely (the prior workaround) turned out to be the
actual bug, not the fix. Lesson: don't assume a logged "assertion failed" means the code path
crashed; check whether the assertion macro is fatal before working around it.

**Remaining known issue — composited-presentation stutter under compositor load** (tracked, not
yet fixed): video freezes for multi-second stretches specifically when something compositor-heavy
is also happening (video playback, window minimize/maximize animations) but stays smooth for
cheap operations (dragging a window). Diagnosed via three independent signals ruling out
network/host causes: `moonlight-common-c`'s own `connectionStatusUpdate` callback never once
fired (`CONN_STATUS_POOR` never seen) during a reproduced freeze; GStreamer's `appsink` kept
decoding at a steady ~50-60fps throughout the freeze (checked via `decode.rs`'s periodic frame-
count log); and the drops are logged precisely at `input_surface.rs`'s `present_frame` — the
compositor isn't releasing `wl_shm` buffers fast enough, even after widening the buffer pool from
2 to 4 slots (searching for *any* free slot, not strict round-robin) made no difference to the
sustained-stall case. This points squarely at composited presentation itself being unable to keep
up when Mutter's compositor thread is also busy with other GPU work — exactly the tradeoff flagged
as a risk in the "Phase 1 findings — decode-to-scanout spike" section above (VAAPI dma-buf export
is broken on this NVIDIA + `nvidia-vaapi-driver` combination, forcing a composited-copy fallback
instead of true direct scanout). The real fix is still the same one identified back then: either
the upstream `nvidia-vaapi-driver` export bug gets fixed, or a different scanout path gets found —
not something fixable by further tuning the client's own copy/buffer-management code.

## Stutter investigation, continued (2026-08-12): root-caused to wl_shm presentation, not decode/network/GPU

Added real instrumentation rather than continuing to guess: per-`wl_buffer` commit-to-release
latency tracking in `input_surface.rs` (a `BufferTiming` struct alongside each buffer slot,
logging on `wl_buffer::Event::Release`), plus a `ConnListenerConnectionStatusUpdate` callback in
`stream.rs` (previously unwired) to catch `CONN_STATUS_POOR` if the network were degrading, plus
a parallel `nvidia-smi -l 1` GPU utilization log for correlation.

**Real data from a reproduced stutter**: three severe stalls in one session — 170.5s, 19.9s, and
a 4.7-6.5s cluster — each ending with multiple buffers releasing within *microseconds* of each
other (not gradually), the signature of a surface that stopped being actively repainted at all for
that stretch, then caught up in a burst. `connectionStatusUpdate` never once reported
`CONN_STATUS_POOR` during any of this. GStreamer's `appsink` kept decoding at a steady ~50-60fps
throughout (checked via `decode.rs`'s periodic frame-count log timestamps). GPU utilization stayed
busy (67-81%) through the entire 170s stall — not idle. This rules out network, host encoder, and
GPU/decode starvation as causes; the stall is specifically in `present_frame`'s wait for a free
`wl_shm` buffer slot.

Asked the user directly whether they were switching away from/minimizing the client window during
the stalls (the obvious alternative explanation for "stopped being repainted") — confirmed no, the
window stayed focused and visible throughout. Ruled out.

**Exhaustively tested every combination of xdg_toplevel state-request timing** against real
hardware: `set_fullscreen()` or `set_maximized()` requested *after* the first `configure` (the
order this client uses) makes the window visible but reproducibly trips a Mutter-internal
assertion (`meta_window_set_stack_position_no_sync: assertion 'window->stack_position >= 0'
failed`, visible in `journalctl --user`); either state requested *before* the first commit
(xdg-shell's documented/recommended pattern) leaves the window invisible; no state request at all
also leaves it invisible; and a 300ms settle delay before the deferred request (in case it was a
just-mapped-window race) made no difference — the assertion isn't timing-sensitive. Checked for
`zwlr_layer_shell_v1` (would sidestep `xdg_toplevel`'s window-stack machinery entirely, where this
assertion lives) via a registry dump — not advertised by this compositor, so not an available
escape hatch.

**Confirmed this is a genuine, external Mutter bug, not our code**: the identical assertion is a
long-standing, cross-distro bug tracked upstream as
[GNOME/mutter#1647](https://gitlab.gnome.org/GNOME/mutter/-/work_items/1647), reproducible with
literally `mpv -fullscreen some-video.mp4` — i.e. triggered by *any* client requesting fullscreen
shortly after mapping, unrelated to anything specific to this project.

**But then isolated the actual severity to our specific rendering approach, not the assertion
itself**: ran a controlled A/B test on this exact machine — `mpv --fullscreen --fs-screen-name=DP-2
--vo=gpu` playing a synthetic 20-second 1080p60 test video. The assertion fired (confirmed via
`journalctl`, same moment as our client's), but mpv finished playback in a clean ~21.3s
wall-clock — **no stalls at all**. This decisively separates two things this investigation had
been conflating: the Mutter assertion is real but apparently harmless on its own (mpv hits it and
keeps playing fine); our client's severe stalls come from something specific to *our* presentation
method, not the assertion. The concrete difference: mpv renders via GL/EGL (`eglSwapBuffers`),
while this client renders via `wl_shm` buffers gated on `wl_buffer::Release` events — a
fundamentally different Mutter code path for scheduling repaints. It's very plausible the same
underlying `stack_position` corruption disrupts Mutter's SHM-repaint scheduling specifically,
without affecting EGL swap-chain scheduling at all — SHM repaints and EGL buffer swaps are
serviced by different parts of the compositor.

**Next step (not yet implemented, sized as new work, not a small tweak)**: switch
`input_surface.rs`'s presentation from `wl_shm` to EGL/GLES rendering on the *same* existing
surface (so the single merged video+input surface — the whole reason `wl_shm` was chosen originally
over separately re-adopting `waylandsink`, per the "two windows" bug this project already fixed
once — stays intact): create an EGL context bound to our `wl_surface` via `wl_egl_window`, upload
each decoded BGRx frame as a GL texture, draw a full-screen textured quad, `eglSwapBuffers` instead
of `wl_shm`'s attach/damage/commit. Needs new dependencies (EGL/GLES bindings — `khronos-egl` or
similar) and a real GLES2 shader/texture-upload path; a genuinely new subsystem, not a quick patch.

**Implemented and confirmed fixed (2026-08-12)**: new `client/src/gl_present.rs` module —
`khronos-egl` (static-linked) for EGL context/surface/display management, `wayland-egl` for the
`wl_egl_window` native-window handle bound to the *same* `wl_surface` used for input (kept the
merged coordinate space), `glow` for GLES2 calls. Renders each decoded BGRx frame as a GL texture
onto a full-screen quad (fragment shader swaps R/B to avoid needing the
`EXT_texture_format_BGRA8888` extension — `decode.rs`'s frames are uploaded as if `RGBA` and
corrected in the shader) and presents via `eglSwapBuffers`. `wp_viewporter` is no longer needed at
all — GL's own bilinear texture sampling handles scaling the frame's native decode resolution up
to the output's physical size, so `input_surface.rs` dropped that dependency along with the entire
`wl_shm` buffer-pool/timing machinery (`BufferSlot`, `BufferTiming`, `FrameBuffers`,
`create_frame_buffers`, the `wl_buffer::Event::Release` `Dispatch` impl — all deleted). All API
signatures were verified directly against the vendored crate sources (not assumed from memory)
before writing the code, and it compiled and worked correctly on the first real test.

**Real-hardware result**: consistent ~60fps swap cadence sustained through the same
video-playback/window-animation stress conditions that previously produced 170-second stalls —
zero drop/stall warnings logged (there's no longer a mechanism that *could* drop a frame the way
`wl_shm`'s "no free buffer" path could). User-confirmed: "this looks much better." The Mutter
`stack_position` assertion still fires once in `journalctl` (expected — it's an external bug in
window-state transition handling, unrelated to the presentation backend), but no longer produces
any observable effect, exactly as the mpv control test predicted.

This closes out the stutter investigation. Phase 1 MVP (now on the Sunshine/Moonlight
architecture) is functionally complete: pairing, session launch, hardware-encoded streaming,
GLES-presented video, and focus-scoped real input capture all confirmed working end-to-end on real
hardware.
3. Implement the GameStream/Sunshine HTTPS PIN-pairing handshake.
4. Wire `client/src/decode.rs` and `client/src/input_surface.rs` into the FFI callback structs,
   replacing the QUIC-based `main.rs` transport.

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
- **Phase 1 — Single-monitor MVP — DONE (2026-08-12, on the Sunshine/Moonlight architecture)**: the
  description below is superseded by the architecture pivot (see that section above) — Sunshine
  (host capture/encode) + a custom Rust client speaking GameStream/Moonlight via FFI bindings to
  `moonlight-common-c` replaced the from-scratch QUIC protocol this paragraph originally described.
  Confirmed working end-to-end on real hardware: pairing, session launch, real AMD hardware
  encoding (via Sunshine, not our own since-retired Media Foundation code), GLES-presented video
  (`client/src/gl_present.rs`, replacing an earlier `wl_shm` path — see the stutter investigation
  above), and focus-scoped real Wayland input capture forwarding to `LiSend*` calls. Latency
  benchmark against `xfreerdp-aad` still not run — no longer a hard go/no-go gate now that this
  much has already shipped, but still worth doing at some point for a real before/after number.
- **Phase 2 — Multi-monitor — DONE (2026-08-12)**: extended to the 3-monitor topology this plan
  originally specified (portrait + ultrawide + 2560×1440), matching client output topology.
  Confirmed working end-to-end: `rdhost.exe` configures all 3 virtual monitors at their correct
  distinct resolutions, positions them to exactly match the client's real `xrandr` layout, and
  launches all 3 Sunshine instances.

  **Architecture**: GameStream (what Sunshine/Moonlight implement) is fundamentally
  single-display-per-session — there's no "stream one combined desktop spanning multiple
  monitors" mode. The approach: run **one Sunshine instance per monitor**, each bound to its own
  virtual display via `output_name` and a distinct base `port` (Sunshine's `port` config offsets
  its whole port family — web UI/HTTP/HTTPS/RTSP/etc — together as one unit, confirmed against
  real instances). Since `moonlight-common-c` is a single-global-connection-per-process library,
  the client runs as **3 separate `rdclient` processes**, one per monitor/port/output — each
  naturally focus-scoped by Wayland already (input only routes to whichever process's surface
  currently has pointer/keyboard focus), so no extra input-routing code needed. `client/src/
  pairing.rs`/`gamestream.rs`/`stream.rs` already generalized to take a `--port` argument for
  this (see `pairing::Ports::from_base`).

  **`host/` crate role**: `host/` was fully rewritten (old QUIC-era capture/encode/input code
  deleted) into a one-shot interactive setup tool (`host/src/{main,topology,vdd,mmt,sunshine}.rs`)
  that the user runs via the KVM (not SSH — see below): configures the virtual monitors, positions
  them via NirSoft's MultiMonitorTool, discovers each display's Sunshine `device_id` (parsed from
  a JSON block Sunshine logs at startup — `"Currently available display devices:"` — not
  documented anywhere, found by reading a real `sunshine.log`), and configures/launches the 3
  Sunshine instances. Also supports `rdhost.exe --teardown` to remove the virtual monitors and
  stop the extra Sunshine instances once a session's done, rather than leaving them sitting in
  Display Settings indefinitely.

  **Driver: switched from stock VDD to [MolotovCherry/virtual-display-rs](
  https://github.com/MolotovCherry/virtual-display-rs) mid-Phase-2, after confirming a hard,
  unfixed upstream bug in stock VirtualDrivers/Virtual-Display-Driver**: running 3 VDD virtual
  monitors at 3 *different* resolutions is fundamentally broken there — the driver periodically
  reasserts whatever resolution is listed *first* in `vdd_settings.xml`'s shared resolution list
  across **all** its monitors (confirmed by reordering the list and watching the "reverts to"
  target change to match), regardless of how aggressively the layout is reapplied. Matches
  [VirtualDrivers/Virtual-Display-Driver#178](https://github.com/VirtualDrivers/Virtual-Display-Driver/issues/178),
  no maintainer fix. (A from-source build of SudoMaker/SudoVDA — the driver Apollo's ecosystem
  uses instead, for the same reason — was attempted first as a replacement, but the EWDK/WDK
  driver-toolchain saga that required hit an unresolved MSBuild property-resolution bug specific
  to that Insiders-preview EWDK build; abandoned in favor of virtual-display-rs, which needed no
  from-source driver build at all.)

  virtual-display-rs configures monitors live over a named-pipe IPC (via its
  `virtual-display-driver-cli.exe`: `remove-all` + one `add <W>x<H>@<Hz> --name <label>` per
  monitor) rather than a config file + device restart — no shared-resolution-list bug, confirmed
  by holding 3 distinct resolutions indefinitely in testing. Two bugs of its own along the way,
  both worked around: (1) the last **tagged release** (v0.3.1)'s IPC pipe denies every caller —
  "Access is denied", even elevated Administrator from the interactive console — while the **dev
  branch** build's pipe is deliberately open to anyone (its own source comment: "these security
  attributes will allow anyone access, so local account does not need admin privileges to use
  it"); fixed by installing the prebuilt dev-branch driver from
  [issue #115](https://github.com/MolotovCherry/virtual-display-rs/issues/115) instead of the
  tagged release. (2) explicit `--id N` on `add` unreliably reports "already exists" even
  immediately after `remove-all`; worked around by leaving IDs auto-assigned and correlating
  monitors to targets by resolution instead (see below) — not needed for `--name`, which works
  fine and is what `mmt.rs`'s driver-detection filter (`"VirtuDisplay+"` in the monitor's PnP
  friendly name) and `sunshine.rs`'s existing resolution-based `output_name` matching both key
  off instead.

  Fully **scriptable, no interactive session needed**, unlike stock VDD's install: certificate via
  `certutil -addstore` (Root + TrustedPublisher — plain CLI, no UI to render), driver install via
  the `nefconc` CLI's 3-command sequence (`--remove-device-node`, `--create-device-node`,
  `--install-driver --inf-path`) instead of an MSI/GUI installer. (An attempt to switch to
  **Apollo**, ClassicOldSong's Sunshine fork with virtual-display-rs's SudoVDA-lineage driver
  bundled in, was tried first per a Reddit tip — its installer stalled/failed non-interactively
  over SSH the same way stock VDD's did, so it was dropped in favor of driving virtual-display-rs
  directly, which turned out to have the scriptable `nefconc` path this whole time.)

  **Positioning (`mmt.rs`) needed real debugging even after the driver swap fixed resolution**:
  - Real, hard-earned finding: `/SetMonitors` and `/EnableAtPosition` **must be issued as a single
    combined `/SetMonitors` call**, not split across two separate MultiMonitorTool invocations
    (one for the 3 virtual monitors, one for pushing the real monitor away). Splitting them
    produced a completely different, 100%-deterministic scrambled layout for *every* monitor
    (not just the one in the second call) — unaffected by call ordering, a `Primary=1` flag, or
    added settle delays, which is the signature of `/SetMonitors` operating on the complete
    monitor topology atomically rather than accepting incremental partial updates.
  - The real monitor being pushed **far** away (a 5000px margin) rather than just below the
    virtual monitors' bounding box was tried and also produced a different but still-broken
    scrambled layout — consistent with Windows' display-config validation rejecting/re-flowing a
    topology where a monitor is a fully disconnected "island" touching no other monitor's edge.
    Fixed by pushing it exactly flush against the bounding box's bottom edge (margin = 0) instead.
  - The target sitting at `(0,0)` needs `Primary=1` explicitly — otherwise the *real* monitor
    (Primary from before any of this ran) stays Primary even after being relocated, and Windows
    renormalizes the whole coordinate space around it, silently shifting every other monitor.
  - Correlating a `\\.\DISPLAYn` name to a `TargetMonitor` **by raw MultiMonitorTool CSV
    enumeration order doesn't reliably match whatever order Windows itself used when applying
    `/SetMonitors`** — this silently swapped two same-shaped-topology targets' positions with
    each other in testing. Fixed by matching monitors to targets **by resolution** instead
    (`(width, height)`, derived from the CSV's `Left-Top`/`Right-Bottom` columns rather than
    parsing the `Resolution` column's string format) — both for the initial assignment *and* for
    a post-`apply()` verification re-list, since a `\\.\DISPLAYn` name isn't guaranteed to still
    refer to the same monitor after `apply()` either. Every target's resolution is guaranteed
    distinct by `topology.rs`, the same property `sunshine::find_device_id` already relies on.
  - `sunshine::discover_displays()` was hardened alongside all this: a single fixed 3s wait after
    `Restart-Service SunshineService` sometimes lost the race against Sunshine's own startup
    display-enumeration pass (stale or entirely-missing log marker); replaced with polling for a
    marker newer than the restart, up to 20s. A 3s settle delay was also added between finishing
    `mmt::apply()` and restarting Sunshine, since Sunshine's re-enumeration was sometimes fast
    enough to still see a monitor's pre-resize mode.

  **Also confirmed along the way (holds regardless of driver)**: this whole tool must run from an
  actual interactive session (KVM/console), not SSH — MultiMonitorTool's monitor enumeration
  silently returns empty results from an SSH-spawned Session 0 process, even with another
  interactive session active elsewhere on the machine (a stricter version of the same class of
  problem as DDA capture's session requirement from Phase 1; virtual-display-rs's own CLI doesn't
  have this restriction, only MultiMonitorTool does). `PowerShell Start-Process -ArgumentList`
  with a string array does *not* auto-quote elements containing spaces (a real, repeatedly-hit bug
  this session — always build a single pre-quoted argument string instead, or use a `.ps1` script
  file with a proper `$args` array passed to the native exe directly).

  **`launch_instance` needed real hardening too, all confirmed by the extra Sunshine instances
  going dark shortly after launch, one bug at a time**:
  - Spawned without any special process-creation flags, an extra instance stayed attached to
    `rdhost.exe`'s own console — closing that console window (the normal end of an interactive
    "Run as administrator" session) sent it a CTRL_CLOSE_EVENT and killed it. `DETACHED_PROCESS`
    was tried first and made it *worse*: the instance now survived console closure but immediately
    crashed with `ERROR_ACCESS_DENIED` querying display paths (`DETACHED_PROCESS` evidently strips
    window-station/desktop access too, which Sunshine's DirectX/DXGI display enumeration needs, not
    just the console). `CREATE_NO_WINDOW` gets "survives console closure" without that side effect.
  - With `CREATE_NO_WINDOW`, there's no console for Sunshine's own stdout/stderr to land on at
    all — a start-up crash was completely invisible until stdout/stderr were explicitly redirected
    to per-instance log files.
  - Running with `current_dir` set to the *instance's own* directory (to keep its config
    self-contained) broke Sunshine's shader compilation entirely (`Couldn't compile
    assets/shaders/directx/... [0x80070003]` → `Platform failed to initialize`) — Sunshine resolves
    its assets via paths relative to cwd, not relative to its own exe location. Fixed by running
    with cwd at Sunshine's *install* directory instead, and redirecting only `file_state`/
    `credentials_file`/`log_path` to the instance's own directory via conf keys (matching Sunshine's
    own documented multi-instance pattern) so instances don't clobber each other's state.
  - Sunshine's CSRF protection only auto-allows localhost-style origins by default; pairing against
    a non-default instance's web UI from cwtrow's LAN IP (rather than `localhost`) was rejected
    ("The request was blocked by CSRF protection") until `csrf_allowed_origins` was set explicitly
    per instance.
  - `rdhost.exe` didn't stop previous extra instances before launching new ones, so **re-running it
    (e.g. to pick up a config fix) silently left old and new processes running side by side on the
    same ports** — the *old* one, having gotten there first, kept answering requests with its stale
    config, making every fix above look like it hadn't worked until this was caught. Fixed by
    calling `sunshine::stop_extra_instances()` at the start of every run, not just `--teardown`.

  **Client-side: a single-command launcher (`client/src/bin/rdconnect.rs`)** replaces manually
  running 3 `rdclient` invocations with hand-typed `--port`/`--width`/`--height`/`--output` per
  monitor. `client/` was restructured into a `lib.rs` + multiple binaries (`rdclient`, `rdconnect`)
  so the launcher can reuse `pairing`/`gamestream` without duplicating the pairing handshake.
  `rdconnect <host>` detects this machine's connected outputs via `xrandr --query`
  (`client/src/topology.rs`, mirroring `host/src/topology.rs`'s target list by hand — no shared
  crate between the two separate Cargo projects), matches them to targets by resolution, checks
  each instance's pair status via a new `pairing::pair_status()` (`/serverinfo`'s `PairStatus`
  field, safe to call whether or not already paired), prompts for a PIN only for instances not yet
  paired, then spawns one `rdclient` child per monitor with the right flags already filled in and
  waits on all of them, forwarding Ctrl+C to each so `LiStopConnection()` runs cleanly instead of
  leaving the host's app session stuck "already running" for the next connect.

  **HEVC support added mid-testing, not originally planned for Phase 2**: the first real 3-monitor
  test showed the portrait and standard monitors streaming fine but the ultrawide (5120px wide)
  never producing a single frame, connection eventually timing out (`error_code=-100`, moonlight-
  common-c's "no video traffic" code). Root cause, found in that instance's now-captured
  `sunshine.log`: cwtrow's AMD AMF H.264 hardware encoder fails `encoder->Init()` outright above
  some width ceiling well below 5120 — a real hardware/driver limitation, not fixable from either
  side's software. Client previously only ever advertised `VIDEO_FORMAT_H264` in
  `supportedVideoFormats`; changed to offer H264 *and* H265 (`stream.rs`), letting Sunshine fall
  back to HEVC (whose encoder doesn't share that ceiling) for whichever stream needs it — in
  practice Sunshine picked HEVC for all 3 once offered, not just the ultrawide. Client-side, this
  needed real work too, not just flipping a flag: `decode.rs`'s pipeline was H.264-only
  (`h264parse ! vah264dec`); added an HEVC branch (`h265parse` + a decoder chosen by codec), and
  `stream.rs`/`input_surface.rs` needed a way to get the *negotiated* codec (known only once
  `on_decoder_setup` fires, synchronously inside `LiStartConnection`) to the decoder-construction
  call that happens just after. Also found this client's VA-API stack has no HEVC decode entry
  point at all (`gst-inspect-1.0 vah265dec` finds nothing) despite having one for H.264 — used
  `nvh265dec` (NVDEC, via the separate `nvcodec` GStreamer plugin) instead, which does exist here
  and accepts the same byte-stream/`au` caps.

  **A real, serious bug found only by running all 3 streams together long enough**: the Linux
  desktop itself (running the client) crashed from memory exhaustion mid-test — GNOME's low-memory
  monitor force-killed an application after the whole system's available memory ran out. Root
  cause: every channel in the video/audio pipeline (`decode.rs`'s decoded-frame channel,
  `stream.rs`'s encoded-decode-unit and audio-sample channels) was an unbounded `mpsc::channel()`.
  The decoded-frame one is the dangerous one — full uncompressed BGRx frames, ~29MB each for the
  ultrawide at 60fps — and if presentation ever falls even briefly behind decode (very plausible
  with 3 windows competing for the same GPU/compositor), an unbounded channel queues them with no
  limit, `GStreamer`'s own `appsink` `drop(true)`/`max_buffers(1)` config notwithstanding (that
  only bounds GStreamer's *internal* backlog, not what the Rust code downstream of the callback
  does with each frame it *does* get). Fixed by switching every channel in the pipeline to
  `sync_channel` with a small bound (1 for raw video frames, 32 for the smaller compressed/audio
  ones) — a full channel now applies real backpressure (blocks the producer) instead of growing
  without limit.

  **Also found while testing multi-window focus behavior**: a stuck-modifier-key bug — each of the
  3 monitors is a fully independent process/window with its own local keyboard-modifier bitmask
  (`input_surface.rs`'s `Inner.modifiers`), and a real key-up event only ever arrives at whichever
  window happens to have Wayland keyboard focus *at release time*, not whichever had focus when
  the key went down. Pressing a modifier in one window then switching focus elsewhere before
  releasing it left that bit stuck set forever, corrupting every subsequent key event sent from
  that window. Fixed by handling `wl_keyboard::Event::Leave` (previously ignored entirely): on
  losing focus, synthesize a key-up for every modifier currently marked held — so the *host's* own
  OS-level modifier state also clears, not just local tracking — then reset to 0.

  **Known, accepted limitation, not planned for a fix within this architecture**: dragging a
  window across the boundary between two of the 3 monitors doesn't work like real RDP — each
  monitor is captured/encoded/streamed as a fully independent Sunshine session with no shared
  frame timing, so a window crossing that boundary would visually cut from one stream to the other
  rather than gliding continuously, even though the 3 virtual monitors form one continuous desktop
  on the host side. Fixing this for real means the from-scratch single-combined-desktop capture +
  custom protocol this project originally started as (see the architecture-pivot section below) —
  **decided (2026-08-12) to revisit that path in a future session** rather than mid-way through
  this one. Re-reading the Session 0 finding above: it was never a *dead end*, just deferred — the
  standard fix (a Windows service using `WTSQueryUserToken` + `CreateProcessAsUser` to launch the
  capture agent into the interactive console session, the same technique Sunshine/Parsec use
  themselves) was already known and explicitly scoped to Phase 5 packaging, not abandoned as
  infeasible. For live testing (not production packaging) it doesn't even need that — running the
  old capture/encode host binary interactively, the same way `rdhost.exe` runs today, would be
  enough to validate the approach. The old capture/encode/input code (`host/src/capture.rs`,
  `encode.rs`, `input.rs`) was deleted during the Sunshine pivot but is recoverable from git
  history (`4e55dd1` and earlier). Real scope if taken on: DDA capture + Media Foundation encode
  for all 3 virtual monitors *simultaneously* (Phase 1 only ever validated one), a combined
  multi-monitor wire protocol, and a client-side single window/compositor instead of 3 independent
  processes — most of Phase 1-2's original scope, redone for multi-monitor, not a quick follow-up.
- **Phase 3 — Audio — DONE (2026-08-12)**: `client/src/audio.rs`, a GStreamer pipeline
  (`appsrc ! opusdec ! audioconvert ! audioresample ! autoaudiosink`) wired into
  `AUDIO_RENDERER_CALLBACKS` in `stream.rs`. `moonlight-common-c`'s `AudioRendererDecodeAndPlaySample`
  hands over raw Opus packets (despite the name, it does not decode them itself) — decoded and
  played via the same GStreamer dependency already used for video, rather than adding a separate
  libopus binding. Current scope only handles simple mono/stereo Opus
  (`channel-mapping-family=0`, matching `stream.rs` negotiating `AUDIO_CONFIGURATION_STEREO`);
  surround would need libopus's multistream channel-mapping table, not implemented. Confirmed
  working end-to-end on real hardware, audio and video in sync. Hit and fixed one real bug along
  the way: `GST_VA_ALL_DRIVERS` (needed by `decode.rs`'s `vah264dec`, see the Phase 1 VAAPI
  findings) was being set inside `VideoDecoder::new()`, but GStreamer scans its plugin registry on
  the *first* `gst::init()` call from any subsystem and caches it — `LiStartConnection`'s internal
  stage order runs audio setup before `input_surface.rs` gets around to creating the video
  decoder, so audio's `gst::init()` (with the env var still unset) was winning the race and
  silently breaking VAAPI decode. Fixed by moving the `env::set_var` call to the very top of
  `main()`, before anything GStreamer-related can run.
- **Phase 4 — Clipboard & file transfer**: bidirectional text/image/file sync. Not started.
- **Phase 5 — Hardening**: reconnect/resilience, adaptive bitrate, auth/security finalization, packaging
  (host as a Windows service/startup app, client as a normal desktop app), replace `xfreerdp-aad` wrapper usage
  once at parity. Also planned (not started, noted 2026-08-12 during multi-monitor setup): an option to
  **disable the host's real/physical monitor(s) while a remote session is connected** — cwtrow's real KVM-
  connected display currently shows up as an extra, oddly-positioned 4th monitor alongside the 3 virtual ones
  in Windows' display arrangement, which is harmless functionally (the 3 virtual monitors' *relative* layout
  to each other is what actually matters for streaming) but cluttered. Disabling it outright would leave no
  way to see the host locally via the KVM, so this needs to be reversible from the **client** side — a toggle
  to re-enable the physical monitor(s) remotely, not just a host-side switch — before it's safe to actually
  use.

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
