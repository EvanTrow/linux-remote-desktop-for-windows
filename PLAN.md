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

## Known unknowns — resolve before committing to an architecture

These weren't established in the planning conversation and materially change the design. Phase 0 exists to answer
them:

1. **What GPU (if any) does the Windows host have?** Determines whether hardware encode (NVENC/QuickSync/AMF) is
   available or whether we're stuck with CPU (x264) encode, which changes the achievable resolution/framerate/latency
   budget.
2. **Windows version on the host** — affects which capture API is available (Desktop Duplication API vs. the newer
   Windows.Graphics.Capture) and virtual-display-driver signing requirements.
3. **Network path** — is this pure LAN, or does it ever need to work over VPN/WAN? Affects transport/congestion
   control choices and whether NAT traversal matters at all.
4. **Auth/security model** — the current setup rides on Azure AD auth built into RDP (`/sec:aad`). A custom
   protocol has no equivalent for free; needs an explicit replacement (see Security section).
5. **Windows driver signing** — same class of problem we just hit on the Linux side with secure boot / MOK
   enrollment: an unsigned custom Indirect Display Driver (IDD) on the host will need either test-signing mode
   enabled or reuse of an already-signed community IDD.

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
│  HW encode (NVENC/QSV/   │── video (unreliable, │  HW decode (VAAPI/NVDEC) │
│   AMF, else x264)        │   per-monitor stream)│         │                │
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

Recommend **QUIC** (via `quinn` if we go Rust, or `msquic` which has first-class Windows + cross-platform support)
as the single transport:

- One QUIC connection, multiple streams: a reliable control stream (handshake, topology negotiation, clipboard,
  file transfer) and either QUIC datagrams or dedicated unreliable-ish streams per monitor for video, plus one for
  audio.
- Gets us encryption (TLS 1.3 is mandatory in QUIC) "for free" — solves part of the auth/security gap left by
  dropping AAD.
- If QUIC proves to add too much complexity for the MVP, fall back to plain UDP (video/audio, custom lightweight
  framing) + TCP (control/clipboard) — simpler to debug, worse head-of-line-blocking characteristics.

### Video pipeline

- **Per-monitor stream**, not one giant composited canvas — keeps encode/decode resolution sane (avoids ever
  needing to encode a single 5120+2560+1920-wide frame) and lets each monitor's stream independently adapt
  bitrate/framerate to its content.
- Host: capture via Desktop Duplication API (per synthetic monitor) → hardware encoder (NVENC preferred; confirm
  availability in Phase 0) → H.264 or AV1 depending on encoder support and client decode capability.
- Client: hardware decode (VAAPI on the RTX 4070 Ti works fine for decode regardless of the encode source) → present
  directly to the relevant monitor's surface with minimal buffering (target: decode-to-scanout, not decode-to-
  compositor-to-scanout, if achievable under Wayland/GNOME — needs a Phase 0/1 spike, may require DRM lease or a
  dedicated fullscreen-unredirected path).

### Display topology negotiation

On connect, client enumerates its outputs (resolution, position, refresh rate, DPI) and sends them to the host. Host
driver creates/reconfigures 3 virtual monitors to match. This is the same problem `/multimon` already solves in RDP
— we're reimplementing it, not inventing new UX behavior. Windows IDD (Indirect Display Driver) frameworks that
already support dynamic resolution/topology changes should be evaluated before writing one from scratch — reuse
over reinvention here.

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
- Alternative if WAN access is ever needed (see open unknown #3): tunnel over WireGuard and trust the tunnel,
  skip app-level auth entirely.

## Suggested tech stack

**Rust** for both sides is the recommendation: one language across host (Windows) and client (Linux), strong crates
for everything needed (`windows-rs` for Win32/DDA/SendInput, `quinn` for QUIC, `ffmpeg-next` or `gstreamer-rs` for
encode/decode pipelines, `pipewire-rs` for Linux audio), and memory safety for code that's parsing network input.
C++ is the fallback if a specific Windows capture/IDD API turns out to need it and doesn't have usable Rust bindings.

## Phased plan

- **Phase 0 — Research spike (no shippable code)**: answer the "known unknowns" above. Confirm host GPU/encoder
  capability, pick a Windows capture API, evaluate existing IDD projects for the virtual multi-monitor requirement,
  decide QUIC vs TCP+UDP, benchmark decode-to-scanout latency achievability on GNOME/Wayland.
- **Phase 1 — Single-monitor MVP**: host streams one virtual/real display, client shows it fullscreen on one
  monitor, keyboard/mouse input works. Benchmark latency against current `xfreerdp` setup as the go/no-go gate for
  continuing.
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
