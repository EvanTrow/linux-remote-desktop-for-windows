//! Owns the one client-visible window: a fullscreen Wayland surface that both presents decoded
//! video (via EGL/GLES, fed from `decode.rs`'s `appsink` — see `gl_present.rs`) and captures real
//! `wl_pointer`/`wl_keyboard` input. Previously these were two separate surfaces — GStreamer's
//! `waylandsink` for video plus a second transparent surface just for input — but two
//! independently-managed surfaces with different logical sizes covering the same physical
//! screen caused a real bug: click position didn't match what was on screen. One surface, one
//! coordinate space.
//!
//! Presentation used to go through `wl_shm` (see PLAN.md's stutter investigation for why that
//! was replaced with `gl_present.rs`'s EGL/GLES renderer): a Mutter bug tied to
//! fullscreen/maximized transitions disrupted `wl_shm` buffer-release scheduling specifically,
//! causing stalls up to 170 seconds, but left GL/EGL swap-chain scheduling unaffected.
//!
//! `wl_pointer` motion events give real surface-local coordinates in the surface's *logical*
//! size (the output's actual physical resolution, so it visually fills the screen). These are
//! sent via `LiSendMousePositionEvent` with `local_width`/`local_height` as the reference plane —
//! moonlight-common-c/the host handle scaling to the actual stream resolution, so no manual
//! scaling happens here. `wl_keyboard` key codes are Linux evdev keycodes per protocol, so the
//! VK-code table in `input_capture.rs` applies unchanged.
//!
//! Wayland events only actually arrive via a *blocking* read on the connection's socket
//! (`EventQueue::blocking_dispatch`) — `dispatch_pending` alone only replays whatever was
//! already buffered from an earlier read and never pulls in anything new, so a loop built
//! around it just freezes after the first burst (a real bug hit while building this: input and
//! video both went silent immediately after the first frame). So dispatch gets its own thread,
//! blocking forever; presentation runs on this function's thread instead, woken by new decoded
//! frames arriving on a channel. The two threads share `Inner` behind a `Mutex` for the handful
//! of fields presentation setup and input dispatch both need (geometry, the surface handle).
//! `GlPresenter` itself is *not* shared — EGL contexts are thread-bound, so it lives only on the
//! presentation-loop thread that creates and drives it.

use crate::decode::{DecodedFrame, VideoDecoder};
use crate::gl_present::GlPresenter;
use anyhow::{anyhow, Context, Result};
use moonlight_sys as ml;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

pub fn run(encoded_rx: Receiver<Vec<u8>>, output_name: Option<String>, capture_input: bool) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland display")?;
    let mut queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());

    let inner = Arc::new(Mutex::new(Inner {
        modifiers: 0,
        local_width: 1920,
        local_height: 1080,
        output_name,
        compositor: None,
        wm_base: None,
        seat: None,
        outputs: Vec::new(),
        surface: None,
        configured: false,
    }));
    let mut state = AppState { inner: inner.clone() };

    // Two roundtrips: the first delivers registry globals (and binds wl_output objects), the
    // second delivers those wl_output objects' `name`/`mode` events, which arrive after binding.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    let (compositor, wm_base, seat, target_output, local_width, local_height) = {
        let mut inner = inner.lock().unwrap();
        let compositor = inner.compositor.clone().context("compositor global not advertised")?;
        let wm_base = inner.wm_base.clone().context("xdg_wm_base global not advertised")?;
        let seat = inner.seat.clone().context("wl_seat global not advertised")?;

        let target_name = inner.output_name.clone();
        let target = inner
            .outputs
            .iter()
            .find(|o| target_name.as_deref().is_some_and(|n| o.name.as_deref() == Some(n)))
            .or_else(|| inner.outputs.first());
        let target_output = target.map(|o| o.output.clone());
        let target_mode = target.and_then(|o| o.mode_width.zip(o.mode_height).map(|(w, h)| (w, h, o.transform)));
        if let Some((w, h, transform)) = target_mode {
            let rotated = matches!(transform, wl_output::Transform::_90 | wl_output::Transform::_270 | wl_output::Transform::Flipped90 | wl_output::Transform::Flipped270);
            (inner.local_width, inner.local_height) = if rotated { (h, w) } else { (w, h) };
        }
        (compositor, wm_base, seat, target_output, inner.local_width, inner.local_height)
    };
    tracing::info!(local_width, local_height, "surface geometry");

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("rdclient".to_string());

    if capture_input {
        seat.get_pointer(&qh, ());
        seat.get_keyboard(&qh, ());
    }

    inner.lock().unwrap().surface = Some(surface.clone());
    // Initial commit, no state request yet — see the fullscreen request below for why.
    surface.commit();

    loop {
        queue.blocking_dispatch(&mut state)?;
        if inner.lock().unwrap().configured {
            break;
        }
    }

    // Exhaustively tested all 4 combinations of {fullscreen, maximized} x {before first commit,
    // after first configure} against real hardware this session:
    //   - fullscreen or maximized, requested AFTER the first configure: window visible, but
    //     reproducibly trips a Mutter-internal assertion (meta_window_set_stack_position_no_sync:
    //     'window->stack_position >= 0') correlated with severe (multi-second to 170-second)
    //     wl_buffer::Release stalls later in the session — this is what motivated switching to
    //     EGL/GLES presentation (see gl_present.rs), since GL swap-chain scheduling turned out to
    //     be unaffected by the same bug.
    //   - fullscreen requested BEFORE the first commit (xdg-shell's documented/recommended
    //     pattern): window invisible.
    //   - no state request at all: window invisible.
    // `zwlr_layer_shell_v1` isn't advertised by this compositor either (confirmed via a registry
    // dump), so that's not an available escape hatch from xdg_toplevel's stacking machinery. And
    // it isn't specific to this client at all: the identical assertion is a known, multi-year,
    // cross-distro Mutter bug (GNOME/mutter#1647), reproducible with plain
    // `mpv -fullscreen some-video.mp4` — i.e. any client requesting fullscreen shortly after
    // mapping. See PLAN.md's stutter investigation for the full writeup.
    toplevel.set_fullscreen(target_output.as_ref());
    surface.commit();
    queue.roundtrip(&mut state)?;

    let mut presenter = GlPresenter::new(&conn, &surface, local_width, local_height).context("creating EGL/GLES presenter")?;

    // Decode pipeline: encoded H.264/H.265 (from the network task, via `encoded_rx`) goes in,
    // decoded BGRx frames come out via `decoded_tx`/`decoded_rx`. GStreamer's push_buffer() is
    // a blocking C call, so feeding it happens on its own thread — never on the dispatch
    // thread below, which has to keep blocking-reading Wayland events.
    // Bounded, not `channel()`'s unbounded default — see `decode.rs`'s `VideoDecoder::new` doc
    // comment for why an unbounded queue of full uncompressed frames here caused a real
    // system-wide OOM running 3 concurrent monitor streams.
    let (decoded_tx, decoded_rx): (SyncSender<DecodedFrame>, Receiver<DecodedFrame>) = std::sync::mpsc::sync_channel(1);
    // stream::video_format() is safe to read here: stream::start() (which negotiates it, via
    // on_decoder_setup) always runs before this function is called — see main.rs's call order.
    let decoder = VideoDecoder::new(decoded_tx, crate::stream::video_format()).context("creating video decoder")?;
    std::thread::spawn(move || {
        while let Ok(frame) = encoded_rx.recv() {
            if let Err(e) = decoder.push_frame(&frame) {
                tracing::warn!(error = ?e, "decoder push_frame failed");
            }
        }
    });

    // All further Wayland events (pointer, keyboard) arrive here, forever — this is the only
    // thread allowed to touch `queue`/`state` from here on.
    std::thread::spawn(move || loop {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            tracing::error!(error = ?e, "Wayland dispatch thread exiting");
            break;
        }
    });

    tracing::info!("surface ready, presenting video and capturing input");
    let mut n: u64 = 0;
    for frame in decoded_rx {
        match presenter.present_frame(&frame) {
            Ok(()) => {
                if n % 60 == 0 {
                    tracing::info!(n, "present_frame: swapped");
                }
            }
            Err(e) => tracing::warn!(error = ?e, "failed to present decoded frame"),
        }
        n += 1;
    }
    Err(anyhow!("decoder thread exited"))
}

struct OutputInfo {
    output: wl_output::WlOutput,
    name: Option<String>,
    mode_width: Option<i32>,
    mode_height: Option<i32>,
    /// From `wl_output::Event::Geometry` — compositors report `Mode`'s width/height in the
    /// output's *native* (pre-rotation) orientation, not its logical/visible one. For a portrait
    /// monitor achieved by rotating a landscape panel 90°/270° (this client's `HDMI-1`, see
    /// `topology.rs`), that means `Mode` reports e.g. 1920x1080 even though the visible area is
    /// actually 1080x1920 — confirmed the hard way: without swapping width/height for a 90/270
    /// transform, the EGL surface was created at the wrong aspect ratio, rendering hung off the
    /// window's actual (compositor-allocated, correctly-rotated) bounds.
    transform: wl_output::Transform,
}

/// Shared between the Wayland dispatch thread (which owns all `Dispatch` impls below) and the
/// presentation-loop thread in `run()`. Only setup (global binding, output geometry, the surface
/// handle) and input-derived state (`modifiers`) live here — presentation itself
/// (`GlPresenter`) is owned entirely by the presentation-loop thread, not shared.
struct Inner {
    /// Current `MODIFIER_*` bitmask (see `input_capture::modifier_bit`) — moonlight expects this
    /// on every keyboard event, not just non-modifier key presses.
    modifiers: u8,
    local_width: i32,
    local_height: i32,
    output_name: Option<String>,

    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    outputs: Vec<OutputInfo>,

    surface: Option<wl_surface::WlSurface>,
    configured: bool,
}

#[derive(Clone)]
struct AppState {
    inner: Arc<Mutex<Inner>>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            let mut inner = state.inner.lock().unwrap();
            match interface.as_str() {
                "wl_compositor" => inner.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                "xdg_wm_base" => inner.wm_base = Some(registry.bind(name, version.min(3), qh, ())),
                "wl_seat" => inner.seat = Some(registry.bind(name, version.min(7), qh, ())),
                "wl_output" => {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                    inner.outputs.push(OutputInfo {
                        output,
                        name: None,
                        mode_width: None,
                        mode_height: None,
                        transform: wl_output::Transform::Normal,
                    });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut inner = state.inner.lock().unwrap();
        let Some(o) = inner.outputs.iter_mut().find(|o| &o.output == proxy) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Mode { width, height, .. } => {
                o.mode_width = Some(width);
                o.mode_height = Some(height);
            }
            wl_output::Event::Geometry { transform, .. } => {
                o.transform = transform.into_result().unwrap_or(wl_output::Transform::Normal);
            }
            _ => {}
        }
    }
}

macro_rules! ignore_dispatch {
    ($iface:ty) => {
        impl Dispatch<$iface, ()> for AppState {
            fn event(
                _: &mut Self,
                _: &$iface,
                _: <$iface as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
ignore_dispatch!(wl_compositor::WlCompositor);
ignore_dispatch!(wl_seat::WlSeat);
ignore_dispatch!(wl_surface::WlSurface);
ignore_dispatch!(xdg_toplevel::XdgToplevel);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for AppState {
    fn event(
        _state: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            let mut inner = state.inner.lock().unwrap();
            if !inner.configured {
                inner.configured = true;
                // Nothing to present yet — the surface stays unmapped/invisible until the first
                // decoded frame arrives and GlPresenter::present_frame() swaps a real buffer in.
                if let Some(surface) = &inner.surface {
                    surface.commit();
                }
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let inner = state.inner.lock().unwrap();
        let (local_width, local_height) = (inner.local_width, inner.local_height);
        drop(inner); // don't hold the lock across an FFI call

        let send_position = |x: f64, y: f64| unsafe {
            let ret = ml::LiSendMousePositionEvent(x.round() as i16, y.round() as i16, local_width as i16, local_height as i16);
            if ret != 0 {
                tracing::debug!(ret, "LiSendMousePositionEvent failed");
            }
        };
        match event {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } => send_position(surface_x, surface_y),
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => send_position(surface_x, surface_y),
            wl_pointer::Event::Button { button, state: btn_state, .. } => {
                let Some(mouse_button) = linux_button_code(button) else {
                    return;
                };
                let action = if matches!(btn_state, WEnum::Value(wl_pointer::ButtonState::Pressed)) {
                    ml::BUTTON_ACTION_PRESS
                } else {
                    ml::BUTTON_ACTION_RELEASE
                };
                unsafe {
                    let ret = ml::LiSendMouseButtonEvent(action as i8, mouse_button as i32);
                    if ret != 0 {
                        tracing::debug!(ret, "LiSendMouseButtonEvent failed");
                    }
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                // WHEEL_DELTA (Windows' notion of "one notch") is 120; Wayland's axis value is
                // in surface-local pixels-ish units, so this scaling is approximate — matches
                // the rough scaling the old QUIC-protocol path used.
                let amount = (value * -12.0) as i16;
                unsafe {
                    match axis {
                        WEnum::Value(wl_pointer::Axis::VerticalScroll) => {
                            ml::LiSendHighResScrollEvent(amount);
                        }
                        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                            ml::LiSendHighResHScrollEvent(amount);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// Linux evdev BTN_* codes, as reported by wl_pointer's `button` event, mapped to
/// `moonlight_sys::BUTTON_*`.
fn linux_button_code(code: u32) -> Option<u32> {
    match code {
        0x110 => Some(ml::BUTTON_LEFT),
        0x111 => Some(ml::BUTTON_RIGHT),
        0x112 => Some(ml::BUTTON_MIDDLE),
        _ => None,
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for AppState {
    fn event(
        state: &mut Self,
        _keyboard: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Losing keyboard focus (e.g. dragging a modifier-chorded selection across to one of the
        // *other* 2 monitors' independent windows — each is its own process with its own local
        // `modifiers` bitmask) means this surface will never see the matching key-up for
        // whatever's currently held: real key-up events only ever arrive at whichever surface
        // has focus *when the physical key comes up*, not the one that was focused when it went
        // down. Left unhandled, that bit stays stuck set in `inner.modifiers` forever, corrupting
        // every subsequent key event this surface sends — confirmed the hard way. Fix: on losing
        // focus, synthesize a key-up for every modifier currently marked held (so the *host's*
        // OS-level modifier state also clears, not just our local tracking) and reset to 0.
        if let wl_keyboard::Event::Leave { .. } = event {
            let mut inner = state.inner.lock().unwrap();
            let mut modifiers = inner.modifiers;
            if modifiers != 0 {
                for (key_code, bit) in [
                    (evdev::KeyCode::KEY_LEFTSHIFT, ml::MODIFIER_SHIFT as u8),
                    (evdev::KeyCode::KEY_LEFTCTRL, ml::MODIFIER_CTRL as u8),
                    (evdev::KeyCode::KEY_LEFTALT, ml::MODIFIER_ALT as u8),
                    (evdev::KeyCode::KEY_LEFTMETA, ml::MODIFIER_META as u8),
                ] {
                    if modifiers & bit == 0 {
                        continue;
                    }
                    modifiers &= !bit;
                    if let Some(vk_code) = crate::input_capture::windows_vk_code(key_code) {
                        unsafe {
                            ml::LiSendKeyboardEvent2(vk_code as i16, ml::KEY_ACTION_UP as i8, modifiers as i8, 0);
                        }
                    }
                }
            }
            inner.modifiers = 0;
            return;
        }

        if let wl_keyboard::Event::Key { key, state: key_state, .. } = event {
            // wl_keyboard reports the raw evdev keycode directly (the "+8" offset is an
            // XKB/X11 keysym-table convention, not part of this event).
            let key_code = evdev::KeyCode::new(key as u16);
            let Some(vk_code) = crate::input_capture::windows_vk_code(key_code) else {
                return;
            };
            let pressed = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));

            let mut inner = state.inner.lock().unwrap();
            if let Some(bit) = crate::input_capture::modifier_bit(key_code) {
                if pressed {
                    inner.modifiers |= bit;
                } else {
                    inner.modifiers &= !bit;
                }
            }
            let modifiers = inner.modifiers;
            drop(inner); // don't hold the lock across an FFI call

            let action = if pressed { ml::KEY_ACTION_DOWN } else { ml::KEY_ACTION_UP };
            unsafe {
                let ret = ml::LiSendKeyboardEvent2(vk_code as i16, action as i8, modifiers as i8, 0);
                if ret != 0 {
                    tracing::debug!(ret, "LiSendKeyboardEvent2 failed");
                }
            }
        }
    }
}
