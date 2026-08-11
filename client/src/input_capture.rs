//! Captures local keyboard/mouse via raw evdev and translates to `rdproto::InputEvent`.
//!
//! Phase 1 MVP: listens on every device with relevant capabilities (not exclusive-grabbed —
//! input still reaches the local desktop too. Exclusive capture, so input goes to the remote
//! session only, is a UX/safety decision worth its own review, deferred rather than risking
//! locking someone out of their own keyboard). Requires the running user to be in the `input`
//! group (`/dev/input/event*` is `root:input 0660`).

use anyhow::{Context, Result};
use evdev::{Device, EventSummary, EventType, KeyCode, RelativeAxisCode};
use rdproto::{InputEvent, MouseButton};
use tokio::sync::mpsc::UnboundedSender;

pub fn spawn_all(tx: UnboundedSender<InputEvent>) -> Result<usize> {
    let mut spawned = 0;
    for (path, device) in evdev::enumerate() {
        let has_keys = device.supported_events().contains(EventType::KEY);
        let has_rel = device.supported_events().contains(EventType::RELATIVE);
        if !has_keys && !has_rel {
            continue;
        }
        let name = device.name().unwrap_or("<unknown>").to_string();
        tracing::info!(?path, name, "capturing input device");
        let tx = tx.clone();
        std::thread::spawn(move || {
            if let Err(e) = read_device(device, tx) {
                tracing::warn!(?path, error = ?e, "input device read loop exited");
            }
        });
        spawned += 1;
    }
    if spawned == 0 {
        tracing::warn!(
            "no readable input devices found — is this user in the `input` group? (/dev/input/event* is root:input 0660)"
        );
    }
    Ok(spawned)
}

fn read_device(mut device: Device, tx: UnboundedSender<InputEvent>) -> Result<()> {
    let (mut pending_dx, mut pending_dy) = (0i32, 0i32);
    loop {
        for ev in device.fetch_events().context("fetch_events")? {
            match ev.destructure() {
                EventSummary::RelativeAxis(_, code, value) => match code {
                    RelativeAxisCode::REL_X => pending_dx += value,
                    RelativeAxisCode::REL_Y => pending_dy += value,
                    RelativeAxisCode::REL_WHEEL => {
                        let _ = tx.send(InputEvent::MouseWheel {
                            delta_x: 0,
                            delta_y: value * 120, // WHEEL_DELTA
                        });
                    }
                    RelativeAxisCode::REL_HWHEEL => {
                        let _ = tx.send(InputEvent::MouseWheel {
                            delta_x: value * 120,
                            delta_y: 0,
                        });
                    }
                    _ => {}
                },
                EventSummary::Synchronization(..) => {
                    if pending_dx != 0 || pending_dy != 0 {
                        let _ = tx.send(InputEvent::MouseMove {
                            dx: pending_dx,
                            dy: pending_dy,
                        });
                        pending_dx = 0;
                        pending_dy = 0;
                    }
                }
                EventSummary::Key(_, code, value) => {
                    let pressed = value != 0; // 1 = down, 0 = up, 2 = autorepeat
                    if value == 2 {
                        continue; // let the host's own autorepeat handle held keys
                    }
                    if let Some(button) = mouse_button(code) {
                        let _ = tx.send(InputEvent::MouseButton { button, pressed });
                    } else if let Some(scancode) = windows_scancode(code) {
                        let _ = tx.send(InputEvent::Key { scancode, pressed });
                    }
                }
                _ => {}
            }
        }
    }
}

fn mouse_button(key: KeyCode) -> Option<MouseButton> {
    match key {
        KeyCode::BTN_LEFT => Some(MouseButton::Left),
        KeyCode::BTN_RIGHT => Some(MouseButton::Right),
        KeyCode::BTN_MIDDLE => Some(MouseButton::Middle),
        _ => None,
    }
}

/// Linux evdev `KEY_*` -> Windows PS/2 Set 1 scan code, for `SendInput(KEYEVENTF_SCANCODE)`.
/// Extended keys (arrows, ins/del/home/end/pgup/pgdn, right ctrl/alt, win keys) are encoded as
/// `0xE000 | base_code` — the host checks that high byte to add `KEYEVENTF_EXTENDEDKEY`.
/// Covers standard US QWERTY; not exhaustive (no multimedia/international keys) — good enough
/// for the Phase 1 MVP, worth widening later if real use turns up gaps.
fn windows_scancode(key: KeyCode) -> Option<u16> {
    const EXT: u16 = 0xE000;
    Some(match key {
        KeyCode::KEY_ESC => 0x01,
        KeyCode::KEY_1 => 0x02,
        KeyCode::KEY_2 => 0x03,
        KeyCode::KEY_3 => 0x04,
        KeyCode::KEY_4 => 0x05,
        KeyCode::KEY_5 => 0x06,
        KeyCode::KEY_6 => 0x07,
        KeyCode::KEY_7 => 0x08,
        KeyCode::KEY_8 => 0x09,
        KeyCode::KEY_9 => 0x0A,
        KeyCode::KEY_0 => 0x0B,
        KeyCode::KEY_MINUS => 0x0C,
        KeyCode::KEY_EQUAL => 0x0D,
        KeyCode::KEY_BACKSPACE => 0x0E,
        KeyCode::KEY_TAB => 0x0F,
        KeyCode::KEY_Q => 0x10,
        KeyCode::KEY_W => 0x11,
        KeyCode::KEY_E => 0x12,
        KeyCode::KEY_R => 0x13,
        KeyCode::KEY_T => 0x14,
        KeyCode::KEY_Y => 0x15,
        KeyCode::KEY_U => 0x16,
        KeyCode::KEY_I => 0x17,
        KeyCode::KEY_O => 0x18,
        KeyCode::KEY_P => 0x19,
        KeyCode::KEY_LEFTBRACE => 0x1A,
        KeyCode::KEY_RIGHTBRACE => 0x1B,
        KeyCode::KEY_ENTER => 0x1C,
        KeyCode::KEY_LEFTCTRL => 0x1D,
        KeyCode::KEY_A => 0x1E,
        KeyCode::KEY_S => 0x1F,
        KeyCode::KEY_D => 0x20,
        KeyCode::KEY_F => 0x21,
        KeyCode::KEY_G => 0x22,
        KeyCode::KEY_H => 0x23,
        KeyCode::KEY_J => 0x24,
        KeyCode::KEY_K => 0x25,
        KeyCode::KEY_L => 0x26,
        KeyCode::KEY_SEMICOLON => 0x27,
        KeyCode::KEY_APOSTROPHE => 0x28,
        KeyCode::KEY_GRAVE => 0x29,
        KeyCode::KEY_LEFTSHIFT => 0x2A,
        KeyCode::KEY_BACKSLASH => 0x2B,
        KeyCode::KEY_Z => 0x2C,
        KeyCode::KEY_X => 0x2D,
        KeyCode::KEY_C => 0x2E,
        KeyCode::KEY_V => 0x2F,
        KeyCode::KEY_B => 0x30,
        KeyCode::KEY_N => 0x31,
        KeyCode::KEY_M => 0x32,
        KeyCode::KEY_COMMA => 0x33,
        KeyCode::KEY_DOT => 0x34,
        KeyCode::KEY_SLASH => 0x35,
        KeyCode::KEY_RIGHTSHIFT => 0x36,
        KeyCode::KEY_LEFTALT => 0x38,
        KeyCode::KEY_SPACE => 0x39,
        KeyCode::KEY_CAPSLOCK => 0x3A,
        KeyCode::KEY_F1 => 0x3B,
        KeyCode::KEY_F2 => 0x3C,
        KeyCode::KEY_F3 => 0x3D,
        KeyCode::KEY_F4 => 0x3E,
        KeyCode::KEY_F5 => 0x3F,
        KeyCode::KEY_F6 => 0x40,
        KeyCode::KEY_F7 => 0x41,
        KeyCode::KEY_F8 => 0x42,
        KeyCode::KEY_F9 => 0x43,
        KeyCode::KEY_F10 => 0x44,
        KeyCode::KEY_F11 => 0x57,
        KeyCode::KEY_F12 => 0x58,
        KeyCode::KEY_RIGHTCTRL => EXT | 0x1D,
        KeyCode::KEY_RIGHTALT => EXT | 0x38,
        KeyCode::KEY_HOME => EXT | 0x47,
        KeyCode::KEY_UP => EXT | 0x48,
        KeyCode::KEY_PAGEUP => EXT | 0x49,
        KeyCode::KEY_LEFT => EXT | 0x4B,
        KeyCode::KEY_RIGHT => EXT | 0x4D,
        KeyCode::KEY_END => EXT | 0x4F,
        KeyCode::KEY_DOWN => EXT | 0x50,
        KeyCode::KEY_INSERT => EXT | 0x52,
        KeyCode::KEY_DELETE => EXT | 0x53,
        KeyCode::KEY_LEFTMETA => EXT | 0x5B,
        KeyCode::KEY_RIGHTMETA => EXT | 0x5C,
        _ => return None,
    })
}
