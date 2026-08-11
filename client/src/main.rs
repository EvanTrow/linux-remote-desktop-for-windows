use anyhow::{anyhow, Context, Result};
use clap::Parser;
use rdproto::ControlMessage;
use std::net::SocketAddr;
use tracing::{info, warn};

mod decode;
mod input_capture;

/// Linux client for the custom remote desktop protocol. Phase 1 MVP: single monitor,
/// no clipboard/audio. Connects to the host agent over QUIC, negotiates topology, then
/// streams video (received as datagrams) while forwarding input on the control stream.
#[derive(Parser, Debug)]
struct Args {
    /// Host agent address, e.g. 192.168.1.55:5900
    #[arg(long)]
    host: SocketAddr,

    /// SHA-256 fingerprint of the host's self-signed cert, printed by the host agent on
    /// first run. Pinned instead of doing full CA-chain validation (see netcommon).
    #[arg(long)]
    fingerprint: String,

    /// Send a canned sequence of mouse/keyboard events after connecting — a smoke test for
    /// SendInput injection independent of local input devices/permissions.
    #[arg(long)]
    test_input: bool,

    /// Capture real local keyboard/mouse via evdev and forward it to the host. Not exclusive
    /// (input still reaches the local desktop too — see input_capture.rs). Requires the
    /// running user to be in the `input` group.
    #[arg(long)]
    capture_input: bool,

    /// Wayland output to present fullscreen on (e.g. "DP-2"). Defaults to the compositor's
    /// choice if unset. Phase 1 MVP targets a single monitor.
    #[arg(long)]
    output: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let client_config = rdnet::build_client_endpoint_config(args.fingerprint)?;
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    info!(host = %args.host, "connecting");
    let connection = endpoint
        .connect(args.host, "cwtrow")?
        .await
        .context("QUIC handshake with host failed")?;
    info!("connected");

    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("opening control stream")?;

    send_control(&mut send, &ControlMessage::ClientHello {
        protocol_version: rdproto::PROTOCOL_VERSION,
    })
    .await?;

    match recv_control(&mut recv).await? {
        ControlMessage::ServerHello {
            accepted: true,
            protocol_version,
        } => {
            info!(protocol_version, "host accepted connection");
        }
        ControlMessage::ServerHello {
            accepted: false, ..
        } => return Err(anyhow!("host rejected connection (protocol version mismatch)")),
        other => return Err(anyhow!("unexpected message during handshake: {other:?}")),
    }

    // Phase 1 MVP: single hardcoded monitor topology. Real topology probing (xrandr /
    // wlr-output-management) lands alongside multi-monitor support in Phase 2.
    let topology = ControlMessage::Topology(rdproto::Topology {
        monitors: vec![rdproto::MonitorInfo {
            id: 0,
            width: 1920,
            height: 1080,
            refresh_rate_mhz: 60_000,
            pos_x: 0,
            pos_y: 0,
        }],
    });
    send_control(&mut send, &topology).await?;
    match recv_control(&mut recv).await? {
        ControlMessage::TopologyAck => info!("topology acknowledged by host"),
        other => warn!(?other, "expected TopologyAck"),
    }

    let output = args.output.clone();
    let video_task = tokio::spawn(receive_video(connection.clone(), output));

    if args.test_input {
        send_test_input(&mut send).await?;
    }

    if args.capture_input {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
        let count = input_capture::spawn_all(input_tx)?;
        info!(devices = count, "input capture started");
        tokio::spawn(async move {
            while let Some(event) = input_rx.recv().await {
                if let Err(e) = send_control(&mut send, &ControlMessage::Input(event)).await {
                    warn!(error = %e, "failed to forward input event, stopping capture");
                    break;
                }
            }
        });
    }

    video_task.await??;
    Ok(())
}

/// Temporary smoke test for SendInput injection (see `Args::test_input`): moves the mouse in
/// a small square, clicks, then types "hi".
async fn send_test_input(send: &mut quinn::SendStream) -> Result<()> {
    use rdproto::{InputEvent, MouseButton};
    use tokio::time::{sleep, Duration};

    info!("sending test input sequence");
    let moves = [(200, 0), (0, 200), (-200, 0), (0, -200)];
    for (dx, dy) in moves {
        send_control(send, &ControlMessage::Input(InputEvent::MouseMove { dx, dy })).await?;
        sleep(Duration::from_millis(300)).await;
    }
    send_control(
        send,
        &ControlMessage::Input(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }),
    )
    .await?;
    sleep(Duration::from_millis(100)).await;
    send_control(
        send,
        &ControlMessage::Input(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: false,
        }),
    )
    .await?;

    // US QWERTY scancodes for 'h' (0x23) and 'i' (0x17).
    for scancode in [0x23u16, 0x17] {
        send_control(send, &ControlMessage::Input(InputEvent::Key { scancode, pressed: true })).await?;
        sleep(Duration::from_millis(60)).await;
        send_control(send, &ControlMessage::Input(InputEvent::Key { scancode, pressed: false })).await?;
        sleep(Duration::from_millis(150)).await;
    }
    info!("test input sequence sent");
    Ok(())
}

async fn receive_video(connection: quinn::Connection, output: Option<String>) -> Result<()> {
    let decoder = decode::VideoDecoder::new(output.as_deref())?;
    let mut reassembler = decode::FrameReassembler::new();
    let mut frames_decoded: u64 = 0;

    loop {
        let datagram = connection.read_datagram().await?;
        let (header, header_len) = rdproto::decode_video_header(&datagram)?;
        let payload = &datagram[header_len..];

        if let Some(frame) = reassembler.push(&header, payload) {
            decoder.push_frame(&frame)?;
            frames_decoded += 1;
            if frames_decoded % 60 == 0 {
                info!(frame_id = header.frame_id, frames_decoded, "decoded frame pushed");
            }
        }
    }
}

async fn send_control(send: &mut quinn::SendStream, msg: &ControlMessage) -> Result<()> {
    let framed = rdproto::encode_control_message(msg)?;
    send.write_all(&framed).await?;
    Ok(())
}

async fn recv_control(recv: &mut quinn::RecvStream) -> Result<ControlMessage> {
    let mut len_bytes = [0u8; 4];
    recv.read_exact(&mut len_bytes)
        .await
        .context("reading control message length")?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .context("reading control message body")?;
    rdproto::decode_control_message(&body)
}
