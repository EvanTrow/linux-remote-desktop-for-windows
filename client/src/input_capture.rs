//! Linux evdev keycode -> Windows Virtual-Key (VK) code translation, used by `input_surface.rs`'s
//! `wl_keyboard` handling (a Wayland surface's `wl_keyboard` reports these same evdev keycodes
//! directly per protocol, so this table applies unchanged).
//!
//! `LiSendKeyboardEvent`/`LiSendKeyboardEvent2` (see `stream.rs`/`input_surface.rs`) take Win32
//! VK codes, *not* PS/2 scancodes — this replaced an earlier scancode table that existed when
//! this client drove `SendInput` directly (see PLAN.md's architecture-pivot section: that host
//! code is retired now that Sunshine does input injection).

use evdev::KeyCode;

/// Linux evdev `KEY_*` -> Win32 `VK_*` code, per `Limelight.h`'s "keys on a US English layout"
/// convention. Covers standard US QWERTY; not exhaustive (no multimedia/international keys) —
/// good enough for now, worth widening later if real use turns up gaps.
pub(crate) fn windows_vk_code(key: KeyCode) -> Option<u16> {
    Some(match key {
        KeyCode::KEY_ESC => 0x1B,       // VK_ESCAPE
        KeyCode::KEY_1 => b'1' as u16,
        KeyCode::KEY_2 => b'2' as u16,
        KeyCode::KEY_3 => b'3' as u16,
        KeyCode::KEY_4 => b'4' as u16,
        KeyCode::KEY_5 => b'5' as u16,
        KeyCode::KEY_6 => b'6' as u16,
        KeyCode::KEY_7 => b'7' as u16,
        KeyCode::KEY_8 => b'8' as u16,
        KeyCode::KEY_9 => b'9' as u16,
        KeyCode::KEY_0 => b'0' as u16,
        KeyCode::KEY_MINUS => 0xBD,     // VK_OEM_MINUS
        KeyCode::KEY_EQUAL => 0xBB,     // VK_OEM_PLUS
        KeyCode::KEY_BACKSPACE => 0x08, // VK_BACK
        KeyCode::KEY_TAB => 0x09,       // VK_TAB
        KeyCode::KEY_Q => b'Q' as u16,
        KeyCode::KEY_W => b'W' as u16,
        KeyCode::KEY_E => b'E' as u16,
        KeyCode::KEY_R => b'R' as u16,
        KeyCode::KEY_T => b'T' as u16,
        KeyCode::KEY_Y => b'Y' as u16,
        KeyCode::KEY_U => b'U' as u16,
        KeyCode::KEY_I => b'I' as u16,
        KeyCode::KEY_O => b'O' as u16,
        KeyCode::KEY_P => b'P' as u16,
        KeyCode::KEY_LEFTBRACE => 0xDB,  // VK_OEM_4
        KeyCode::KEY_RIGHTBRACE => 0xDD, // VK_OEM_6
        KeyCode::KEY_ENTER => 0x0D,      // VK_RETURN
        KeyCode::KEY_LEFTCTRL => 0xA2,   // VK_LCONTROL
        KeyCode::KEY_A => b'A' as u16,
        KeyCode::KEY_S => b'S' as u16,
        KeyCode::KEY_D => b'D' as u16,
        KeyCode::KEY_F => b'F' as u16,
        KeyCode::KEY_G => b'G' as u16,
        KeyCode::KEY_H => b'H' as u16,
        KeyCode::KEY_J => b'J' as u16,
        KeyCode::KEY_K => b'K' as u16,
        KeyCode::KEY_L => b'L' as u16,
        KeyCode::KEY_SEMICOLON => 0xBA,  // VK_OEM_1
        KeyCode::KEY_APOSTROPHE => 0xDE, // VK_OEM_7
        KeyCode::KEY_GRAVE => 0xC0,      // VK_OEM_3
        KeyCode::KEY_LEFTSHIFT => 0xA0,  // VK_LSHIFT
        KeyCode::KEY_BACKSLASH => 0xDC,  // VK_OEM_5
        KeyCode::KEY_Z => b'Z' as u16,
        KeyCode::KEY_X => b'X' as u16,
        KeyCode::KEY_C => b'C' as u16,
        KeyCode::KEY_V => b'V' as u16,
        KeyCode::KEY_B => b'B' as u16,
        KeyCode::KEY_N => b'N' as u16,
        KeyCode::KEY_M => b'M' as u16,
        KeyCode::KEY_COMMA => 0xBC,  // VK_OEM_COMMA
        KeyCode::KEY_DOT => 0xBE,    // VK_OEM_PERIOD
        KeyCode::KEY_SLASH => 0xBF,  // VK_OEM_2
        KeyCode::KEY_RIGHTSHIFT => 0xA1, // VK_RSHIFT
        KeyCode::KEY_LEFTALT => 0xA4,    // VK_LMENU
        KeyCode::KEY_SPACE => 0x20,      // VK_SPACE
        KeyCode::KEY_CAPSLOCK => 0x14,   // VK_CAPITAL
        KeyCode::KEY_F1 => 0x70,
        KeyCode::KEY_F2 => 0x71,
        KeyCode::KEY_F3 => 0x72,
        KeyCode::KEY_F4 => 0x73,
        KeyCode::KEY_F5 => 0x74,
        KeyCode::KEY_F6 => 0x75,
        KeyCode::KEY_F7 => 0x76,
        KeyCode::KEY_F8 => 0x77,
        KeyCode::KEY_F9 => 0x78,
        KeyCode::KEY_F10 => 0x79,
        KeyCode::KEY_F11 => 0x7A,
        KeyCode::KEY_F12 => 0x7B,
        KeyCode::KEY_RIGHTCTRL => 0xA3, // VK_RCONTROL
        KeyCode::KEY_RIGHTALT => 0xA5,  // VK_RMENU
        KeyCode::KEY_HOME => 0x24,      // VK_HOME
        KeyCode::KEY_UP => 0x26,        // VK_UP
        KeyCode::KEY_PAGEUP => 0x21,    // VK_PRIOR
        KeyCode::KEY_LEFT => 0x25,      // VK_LEFT
        KeyCode::KEY_RIGHT => 0x27,     // VK_RIGHT
        KeyCode::KEY_END => 0x23,       // VK_END
        KeyCode::KEY_DOWN => 0x28,      // VK_DOWN
        KeyCode::KEY_PAGEDOWN => 0x22,  // VK_NEXT
        KeyCode::KEY_INSERT => 0x2D,    // VK_INSERT
        KeyCode::KEY_DELETE => 0x2E,    // VK_DELETE
        KeyCode::KEY_LEFTMETA => 0x5B,  // VK_LWIN
        KeyCode::KEY_RIGHTMETA => 0x5C, // VK_RWIN
        _ => return None,
    })
}

/// True for keys that should update the `modifiers` byte passed to `LiSendKeyboardEvent2`
/// (moonlight expects the *current* modifier state on every key event, not just non-modifier
/// keys). Returns the `MODIFIER_*` bit this key corresponds to, if any.
pub(crate) fn modifier_bit(key: KeyCode) -> Option<u8> {
    Some(match key {
        KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => moonlight_sys::MODIFIER_SHIFT as u8,
        KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL => moonlight_sys::MODIFIER_CTRL as u8,
        KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT => moonlight_sys::MODIFIER_ALT as u8,
        KeyCode::KEY_LEFTMETA | KeyCode::KEY_RIGHTMETA => moonlight_sys::MODIFIER_META as u8,
        _ => return None,
    })
}
