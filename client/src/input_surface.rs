//! A dedicated, minimal Wayland client that owns one transparent, focused, fullscreen surface
//! purely to capture real pointer/keyboard input — GStreamer's `waylandsink` (see decode.rs)
//! is a black box with no input-event access, so this exists alongside it rather than
//! replacing it. The video surface stays fully visible underneath since this surface has no
//! visible content (a fully transparent 1x1 buffer, scaled to fill the output).
//!
//! `wl_pointer` motion events give real surface-local absolute coordinates — no drift, unlike
//! accumulating raw evdev deltas (the two earlier attempts this replaces). `wl_keyboard` key
//! codes are Linux evdev keycodes per protocol, so the same Windows-scancode table used for
//! evdev-based capture (see input_capture.rs) applies unchanged.

use anyhow::{Context, Result};
use rdproto::{InputEvent, MouseButton};
use std::io::Write;
use std::os::fd::AsFd;
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

pub fn run(
    tx: UnboundedSender<InputEvent>,
    output_name: Option<String>,
    remote_width: i32,
    remote_height: i32,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland display")?;
    let mut queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());

    let mut state = AppState {
        tx,
        remote_width,
        remote_height,
        output_name,
        compositor: None,
        shm: None,
        wm_base: None,
        seat: None,
        viewporter: None,
        outputs: Vec::new(),
        surface: None,
        xdg_surface: None,
        toplevel: None,
        viewport: None,
        configured: false,
    };

    // Two roundtrips: the first delivers registry globals (and binds wl_output objects), the
    // second delivers those wl_output objects' `name` events, which arrive after binding.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    let compositor = state.compositor.clone().context("compositor global not advertised")?;
    state.shm.as_ref().context("wl_shm global not advertised")?;
    let wm_base = state.wm_base.clone().context("xdg_wm_base global not advertised")?;
    let seat = state.seat.clone().context("wl_seat global not advertised")?;
    let viewporter = state.viewporter.clone().context("wp_viewporter global not advertised")?;

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("rdclient-input".to_string());

    let target_output = state
        .outputs
        .iter()
        .find(|o| state.output_name.as_deref().is_some_and(|n| o.name.as_deref() == Some(n)))
        .or_else(|| state.outputs.first())
        .map(|o| o.output.clone());
    toplevel.set_fullscreen(target_output.as_ref());

    let viewport = viewporter.get_viewport(&surface, &qh, ());

    seat.get_pointer(&qh, ());
    seat.get_keyboard(&qh, ());

    state.surface = Some(surface.clone());
    state.xdg_surface = Some(xdg_surface);
    state.toplevel = Some(toplevel);
    state.viewport = Some(viewport);
    surface.commit();

    // Drives the initial xdg_surface.configure -> ack -> attach buffer -> commit sequence.
    while !state.configured {
        queue.blocking_dispatch(&mut state)?;
    }

    tracing::info!("input surface ready, capturing pointer/keyboard");
    loop {
        queue.blocking_dispatch(&mut state)?;
    }
}

struct OutputInfo {
    output: wl_output::WlOutput,
    name: Option<String>,
}

struct AppState {
    tx: UnboundedSender<InputEvent>,
    remote_width: i32,
    remote_height: i32,
    output_name: Option<String>,

    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    outputs: Vec<OutputInfo>,

    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    #[allow(dead_code)]
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    viewport: Option<wp_viewport::WpViewport>,
    configured: bool,
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
            match interface.as_str() {
                "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "xdg_wm_base" => state.wm_base = Some(registry.bind(name, version.min(3), qh, ())),
                "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, version.min(1), qh, ())),
                "wl_output" => {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                    state.outputs.push(OutputInfo { output, name: None });
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
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.iter_mut().find(|o| &o.output == proxy) {
                o.name = Some(name);
            }
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
ignore_dispatch!(wl_shm::WlShm);
ignore_dispatch!(wl_seat::WlSeat);
ignore_dispatch!(wp_viewporter::WpViewporter);
ignore_dispatch!(wl_shm_pool::WlShmPool);
ignore_dispatch!(wl_buffer::WlBuffer);
ignore_dispatch!(wl_surface::WlSurface);
ignore_dispatch!(wp_viewport::WpViewport);
ignore_dispatch!(wl_region::WlRegion);

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
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if !state.configured {
                state.configured = true;
                if let Err(e) = attach_transparent_buffer(state, qh) {
                    tracing::error!(error = ?e, "failed to attach transparent buffer to input surface");
                    return;
                }
                if let Some(viewport) = &state.viewport {
                    viewport.set_destination(state.remote_width, state.remote_height);
                }
                if let Some(surface) = &state.surface {
                    surface.commit();
                }
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            tracing::warn!("input surface toplevel closed by compositor");
        }
    }
}

/// A 1x1 fully-transparent ARGB8888 buffer, scaled to fill the output via wp_viewport — the
/// surface must have *some* buffer to become mapped/interactive, but it must stay invisible
/// so the video surface underneath (via waylandsink) remains what's actually seen.
fn attach_transparent_buffer(state: &mut AppState, qh: &QueueHandle<AppState>) -> Result<()> {
    let shm = state.shm.as_ref().context("no wl_shm bound")?;
    let compositor = state.compositor.as_ref().context("no compositor bound")?;
    let surface = state.surface.as_ref().context("no surface")?;

    let mut file = tempfile::tempfile().context("creating anonymous backing file for wl_shm")?;
    file.write_all(&[0u8; 4]).context("writing transparent pixel")?;

    let pool = shm.create_pool(file.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, 1, 1);

    // wp_viewport.set_destination scales the *rendered* surface to fill the output, but the
    // input region isn't implied by that — without setting it explicitly, some compositors
    // fall back to the backing buffer's actual size (1x1), so only a single pixel would ever
    // be clickable/hoverable. Set it to the same logical size as the viewport destination.
    let region = compositor.create_region(qh, ());
    region.add(0, 0, state.remote_width, state.remote_height);
    surface.set_input_region(Some(&region));
    region.destroy();

    Ok(())
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
        match event {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } => {
                tracing::info!(surface_x, surface_y, "pointer entered input surface");
                let x = surface_x.clamp(0.0, state.remote_width as f64 - 1.0) as i32;
                let y = surface_y.clamp(0.0, state.remote_height as f64 - 1.0) as i32;
                let _ = state.tx.send(InputEvent::MouseMove { x, y });
            }
            wl_pointer::Event::Leave { .. } => {
                tracing::info!("pointer left input surface");
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                let x = surface_x.clamp(0.0, state.remote_width as f64 - 1.0) as i32;
                let y = surface_y.clamp(0.0, state.remote_height as f64 - 1.0) as i32;
                let _ = state.tx.send(InputEvent::MouseMove { x, y });
            }
            wl_pointer::Event::Button { button, state: btn_state, .. } => {
                let Some(mouse_button) = linux_button_code(button) else {
                    return;
                };
                let pressed = matches!(btn_state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                let _ = state.tx.send(InputEvent::MouseButton { button: mouse_button, pressed });
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let delta = (value * -12.0) as i32; // roughly WHEEL_DELTA-scaled, natural direction
                let event = match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => {
                        InputEvent::MouseWheel { delta_x: 0, delta_y: delta }
                    }
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        InputEvent::MouseWheel { delta_x: delta, delta_y: 0 }
                    }
                    _ => return,
                };
                let _ = state.tx.send(event);
            }
            _ => {}
        }
    }
}

/// Linux evdev BTN_* codes, as reported by wl_pointer's `button` event.
fn linux_button_code(code: u32) -> Option<MouseButton> {
    match code {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
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
        match event {
            wl_keyboard::Event::Enter { .. } => tracing::info!("keyboard focus entered input surface"),
            wl_keyboard::Event::Leave { .. } => tracing::info!("keyboard focus left input surface"),
            wl_keyboard::Event::Key { key, state: key_state, .. } => {
                // wl_keyboard reports the raw evdev keycode directly (the "+8" offset is an
                // XKB/X11 keysym-table convention, not part of this event).
                let key_code = evdev::KeyCode::new(key as u16);
                let Some(scancode) = crate::input_capture::windows_scancode(key_code) else {
                    return;
                };
                let pressed = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                let _ = state.tx.send(InputEvent::Key { scancode, pressed });
            }
            _ => {}
        }
    }
}
