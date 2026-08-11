//! Wire protocol shared between the host agent (Windows) and client (Linux).
//!
//! Control messages (handshake, topology, input) travel on a reliable QUIC stream,
//! bincode-framed with a u32 length prefix (see `write_framed`/`read_framed`).
//! Video/audio travel on unreliable QUIC datagrams using `VideoDatagramHeader` as a
//! fixed-size binary header followed by the encoded payload.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    /// First message sent by the client after the QUIC handshake completes.
    ClientHello { protocol_version: u32 },
    /// Host's reply, accepting or rejecting the client based on protocol version.
    ServerHello {
        protocol_version: u32,
        accepted: bool,
    },
    /// Client announces its display topology. Phase 1 MVP: exactly one monitor.
    Topology(Topology),
    /// Host confirms the topology has been applied (or a real display is being used as-is).
    TopologyAck,
    /// Codec/encoder the host selected, sent once before the first video datagram.
    StreamInfo(StreamInfo),
    Input(InputEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topology {
    pub monitors: Vec<MonitorInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_rate_mhz: u32,
    pub pos_x: i32,
    pub pos_y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Av1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    pub monitor_id: u32,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub encoder_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, pressed: bool },
    MouseWheel { delta_x: i32, delta_y: i32 },
    Key { scancode: u16, pressed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Fixed-size header prepended to every video QUIC datagram (unreliable, unordered).
/// Kept small and `bincode`-stable since it rides on a size-constrained datagram.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct VideoDatagramHeader {
    pub monitor_id: u32,
    pub frame_id: u64,
    /// Index of this fragment within the frame (frames larger than one datagram are split).
    pub fragment_index: u16,
    pub fragment_count: u16,
    pub keyframe: bool,
}

pub const MAX_DATAGRAM_PAYLOAD: usize = 1200;

/// Length-prefixed bincode framing for the reliable control stream.
pub fn encode_control_message(msg: &ControlMessage) -> anyhow::Result<Vec<u8>> {
    let body = bincode::serialize(msg)?;
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

pub fn decode_control_message(body: &[u8]) -> anyhow::Result<ControlMessage> {
    Ok(bincode::deserialize(body)?)
}

pub fn encode_video_header(header: &VideoDatagramHeader) -> anyhow::Result<Vec<u8>> {
    Ok(bincode::serialize(header)?)
}

pub fn decode_video_header(bytes: &[u8]) -> anyhow::Result<(VideoDatagramHeader, usize)> {
    let header: VideoDatagramHeader = bincode::deserialize(bytes)?;
    let size = bincode::serialized_size(&header)? as usize;
    Ok((header, size))
}
