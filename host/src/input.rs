//! Injects keyboard/mouse events received from the client's control stream via `SendInput`.

use anyhow::{Context, Result};
use rdproto::{ControlMessage, InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    MOUSE_EVENT_FLAGS,
};

pub async fn run(mut recv: quinn::RecvStream) -> Result<()> {
    loop {
        let msg = recv_control(&mut recv).await?;
        if let ControlMessage::Input(event) = msg {
            if let Err(e) = inject(event) {
                tracing::warn!(error = %e, "failed to inject input event");
            }
        } else {
            tracing::warn!(?msg, "unexpected message on input stream");
        }
    }
}

fn inject(event: InputEvent) -> Result<()> {
    let input = match event {
        InputEvent::MouseMove { dx, dy } => mouse_input(dx, dy, MOUSEEVENTF_MOVE, 0),
        InputEvent::MouseButton { button, pressed } => {
            let flags = match (button, pressed) {
                (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            };
            mouse_input(0, 0, flags, 0)
        }
        InputEvent::MouseWheel { delta_y, .. } => mouse_input(0, 0, MOUSEEVENTF_WHEEL, delta_y),
        InputEvent::Key { scancode, pressed } => {
            // Client encodes extended keys (arrows, ins/del/home/end, right ctrl/alt, win) as
            // 0xE000 | base_code — see client/src/input_capture.rs.
            let extended = scancode & 0xE000 == 0xE000;
            let base_code = scancode & 0xFF;
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            keybd_input(base_code, flags)
        }
    };

    let inputs = [input];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != 1 {
        return Err(anyhow::anyhow!("SendInput failed to inject event"));
    }
    Ok(())
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, mouse_data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keybd_input(scancode: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

async fn recv_control(recv: &mut quinn::RecvStream) -> Result<ControlMessage> {
    let mut len_bytes = [0u8; 4];
    recv.read_exact(&mut len_bytes)
        .await
        .context("reading input message length")?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .context("reading input message body")?;
    rdproto::decode_control_message(&body)
}
