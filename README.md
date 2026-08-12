# Resuming this work

Phase 1 MVP is functionally working end-to-end (real screen capture → encode → QUIC → decode →
display, plus real keyboard/mouse input) but has two known rough edges being worked on when this
was paused. See PLAN.md for full architecture and the detailed history of findings/bugs fixed to
get here — this file is just the practical "how do I pick this back up" doc.

## Known issues to fix next

1. **Two separate windows on the client (video + input) — should be one.** `client/src/decode.rs`
   presents video via GStreamer's `waylandsink` (its own toplevel window); `client/src/
   input_surface.rs` creates a *second*, separate transparent toplevel purely to capture real
   `wl_pointer`/`wl_keyboard` events (waylandsink is a black box with no input-event access, which
   is why these ended up split). Having two independent fullscreen toplevels on the same output is
   janky and was the direct cause of issue #2. Real fix is probably to stop delegating
   presentation to `waylandsink` and have `input_surface.rs`'s surface *also* display the decoded
   video itself (attach decoded frames as `wl_shm` buffers directly, same technique already used
   for the transparent placeholder buffer) — i.e. merge the two into one owned surface/window.
2. **Input coordinates don't line up with the video.** Almost certainly downstream of #1: the
   input surface's `wp_viewport` destination is set to the *remote* resolution (1920x1080, see
   `remote_width`/`remote_height` params), but the video surface is whatever size Mutter renders
   the fullscreen `waylandsink` window at on the physical monitor (e.g. DP-2 is 2560x1440) — two
   independently-managed surfaces with different logical sizes covering the same physical area.
   Merging into one surface (fix for #1) should resolve this by construction, since there'd only
   be one coordinate space to reason about.

## How to build and run

This is two independent Cargo projects talking over the LAN — client here on Linux, host on
`cwtrow` (Windows, 192.168.1.55), synced manually (not via git — see below).

### Client (this machine)

```
cd client
cargo build --release
./target/release/rdclient --host 192.168.1.55:5900 --fingerprint <FINGERPRINT> --output DP-2 --capture-input
```

- `--fingerprint`: printed by the host on startup (`host certificate ready — pass this to the
  client with --fingerprint fingerprint="..."`). It's cached after the first run (same value
  every time unless the host's cert files are deleted), so you likely already have it from a
  previous session's logs.
- `--output DP-2`: target monitor name (`xrandr --listmonitors` to list yours). Omit for the
  compositor's default choice.
- `--capture-input`: enables real keyboard/mouse capture via `input_surface.rs`. Omit to just
  watch video.
- `--test-input`: sends a canned mouse-square + "hi" keystroke sequence after connecting — useful
  for a quick `SendInput` smoke test independent of the Wayland input surface.
- Logs to stdout only (`RUST_LOG=info` for the usual level). No file logging on the client side
  yet (the host has this — see below — client doesn't, since it's normally run interactively).

### Host (cwtrow, via SSH)

SSH key is at `.ssh/evan_ssh_key` (gitignored, already in this repo). The host's working copy is
**not** a git checkout — it's a manually `scp`'d copy, because Windows dev environment setup
happened via SSH command execution rather than git:

```
ssh -i .ssh/evan_ssh_key evan@192.168.1.55
```

Working directory on cwtrow: `C:\Users\evan\dev\rdw\host` (siblings: `..\proto`, `..\netcommon`).
**Whenever you edit `host/*`, `proto/*`, or `netcommon/*` here, you must re-`scp` the changed
files to the matching path under `C:\Users\evan\dev\rdw\` before rebuilding** — editing files in
this git checkout does *not* automatically update the Windows copy. Example:

```
scp -i .ssh/evan_ssh_key host/src/encode.rs "evan@192.168.1.55:C:/Users/evan/dev/rdw/host/src/encode.rs"
ssh -i .ssh/evan_ssh_key evan@192.168.1.55 "cd C:\Users\evan\dev\rdw\host; cargo build --release"
```

To run it, you need an interactive session on cwtrow — **not** SSH exec directly (Windows'
OpenSSH server runs as a service, so anything it spawns lands in Session 0, which has no display
access; DDA capture will fail — this is a real, previously-hit bug, not a theoretical concern).
Use the **network KVM** (not a separate Windows RDP session — DDA also specifically needs the
active console/physical session, confirmed the hard way; see PLAN.md's Phase 1 findings), open a
terminal there, and run:

```
cd C:\Users\evan\dev\rdw\host
.\target\release\rdhost.exe
```

- Building while `rdhost.exe` is still running fails with "Access is denied" (file lock) — stop
  the running process first (Ctrl+C in its terminal, or `Stop-Process` via a separate SSH command)
  before rebuilding.
- Logs go to both stdout and `C:\Users\evan\dev\rdw\host\target\release\rdhost.log` (absolute
  path next to the exe, so it's fetchable over SSH without needing someone to copy-paste from the
  terminal — handy since you can't easily watch the terminal directly).
- Default `RUST_LOG` level shows INFO; set the env var for more (e.g. `$env:RUST_LOG='debug'`
  before running, in the same PowerShell session).

### A full edit-test cycle looks like

1. Edit `host/*` (or `proto/*`, `netcommon/*`) here.
2. `scp` the changed file(s) to `C:\Users\evan\dev\rdw\...` on cwtrow.
3. Ask whoever's at the KVM to stop `rdhost.exe` if it's running.
4. `ssh ... "cd C:\Users\evan\dev\rdw\host; cargo build --release"`.
5. Ask them to restart `.\target\release\rdhost.exe`.
6. `cd client && cargo build --release && ./target/release/rdclient ...` here.
7. Check `rdhost.log` (via SSH) and client stdout for what happened.

Client-only changes (`client/*`) just need a local `cargo build --release` — no sync step, since
this machine *is* the client.

## Current state summary (2026-08-11)

Everything through Phase 1 task 10 is done and verified working on real hardware at least once:
QUIC handshake + cert pinning, Desktop Duplication capture, Media Foundation encoder
auto-detection with fallback (this host's hardware encoders don't activate — software H.264 is
what actually works here), VAAPI decode, composited Wayland presentation, real evdev-free
Wayland-native input capture with cursor-position accuracy (modulo the two issues above),
release-build performance (~30-60fps depending on conditions — debug builds were 5-10x slower and
masked this for a while).

Not started: task 11 (latency benchmark vs `xfreerdp-aad`, the Phase 2 go/no-go gate) — paused to
fix the two issues above first, since a benchmark isn't meaningful with a broken input/video
window setup.

Nothing in this session's work is pushed to the `origin` remote — only committed locally so far.
