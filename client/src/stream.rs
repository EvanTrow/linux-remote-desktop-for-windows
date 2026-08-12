//! Bridges `moonlight-sys`'s raw FFI to the rest of this client: builds the
//! `STREAM_CONFIGURATION`/`SERVER_INFORMATION`/callback structs `LiStartConnection()` needs, and
//! forwards decoded-unit video data to `decode.rs` via `input_surface.rs`'s existing
//! `encoded_rx` channel — so `decode.rs` and `input_surface.rs` need no changes on the video
//! path at all, only on the input-sending path (see `input_surface.rs`'s `Dispatch` impls, which
//! now call `moonlight_sys::LiSend*` directly instead of forwarding over a channel).
//!
//! `moonlight-common-c` is a C library with a single global active connection (there's no
//! `void* context` threaded through `submitDecodeUnit`, unlike `setup`) — so the video-frame
//! sink has to be a global static, `VIDEO_FRAME_TX` below. This mirrors the library's own
//! threading model: internally it runs its own receive threads and calls these callbacks from
//! them directly, not from anything we control.
//!
//! **Not yet live-tested** — builds and type-checks against the real FFI signatures (verified by
//! reading the generated bindings directly), but this hasn't been run against a live stream yet.

use anyhow::{anyhow, Context, Result};
use moonlight_sys as ml;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::mpsc::SyncSender;
use std::sync::OnceLock;

static VIDEO_FRAME_TX: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();
/// Set inside `on_decoder_setup`, which fires synchronously during `LiStartConnection()` (before
/// `start()` returns) — so this is always populated by the time `input_surface.rs` reads it via
/// `video_format()` to build the matching decode pipeline.
static VIDEO_FORMAT: OnceLock<i32> = OnceLock::new();
/// Set inside `on_audio_init` (the only point the negotiated channel count/sample rate are
/// known), not in `start()` like `VIDEO_FRAME_TX` — audio setup can't happen until
/// moonlight-common-c tells us what it negotiated.
static AUDIO_SAMPLE_TX: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();

pub struct StreamParams {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate_kbps: i32,
}

/// Starts the GameStream connection. `video_tx` is the same channel `input_surface.rs` already
/// reads from (`encoded_rx`) to feed `decode.rs` — this function is the new "network task" that
/// channel expects, replacing the old QUIC datagram-reassembly loop.
///
/// Must be called at most once per process (mirrors `LiStartConnection`'s own "not thread-safe,
/// one connection" contract) — `VIDEO_FRAME_TX.set()` enforces that at runtime.
pub fn start(
    session: &crate::gamestream::SessionInfo,
    host: &str,
    params: &StreamParams,
    ri_key: &[u8; 16],
    ri_key_iv: &[u8; 16],
    video_tx: SyncSender<Vec<u8>>,
) -> Result<()> {
    VIDEO_FRAME_TX
        .set(video_tx)
        .map_err(|_| anyhow!("stream::start called more than once"))?;

    // These CStrings must outlive the call to LiStartConnection (moonlight-common-c reads
    // through the raw pointers during the RTSP handshake inside that call) — Box::leak keeps
    // them alive for the process lifetime, acceptable since this is a one-shot, one-connection
    // client.
    let host_cstr = Box::leak(Box::new(CString::new(host)?));
    let app_version_cstr = Box::leak(Box::new(CString::new(session.app_version.clone())?));
    let gfe_version_cstr = Box::leak(Box::new(CString::new(session.gfe_version.clone())?));
    let rtsp_url_cstr = Box::leak(Box::new(CString::new(session.rtsp_session_url.clone())?));

    let mut server_info: ml::SERVER_INFORMATION = unsafe { std::mem::zeroed() };
    unsafe { ml::LiInitializeServerInformation(&mut server_info) };
    server_info.address = host_cstr.as_ptr();
    server_info.serverInfoAppVersion = app_version_cstr.as_ptr();
    server_info.serverInfoGfeVersion = gfe_version_cstr.as_ptr();
    server_info.rtspSessionUrl = rtsp_url_cstr.as_ptr();
    server_info.serverCodecModeSupport = session.server_codec_mode_support;

    let mut stream_config: ml::STREAM_CONFIGURATION = unsafe { std::mem::zeroed() };
    unsafe { ml::LiInitializeStreamConfiguration(&mut stream_config) };
    stream_config.width = params.width;
    stream_config.height = params.height;
    stream_config.fps = params.fps;
    stream_config.bitrate = params.bitrate_kbps;
    stream_config.packetSize = 1024;
    stream_config.streamingRemotely = ml::STREAM_CFG_LOCAL as c_int;
    stream_config.audioConfiguration = (0x3i32 << 16) | (2 << 8) | 0xCA; // AUDIO_CONFIGURATION_STEREO, see gamestream.rs
    // H265 offered alongside H264 (not H264-only, the original Phase 1 scope) — confirmed
    // necessary in practice: cwtrow's AMD AMF H.264 hardware encoder fails to initialize at all
    // for the 5120px-wide ultrawide monitor ("encoder->Init() failed with error 5", looping
    // forever in Sunshine's own retry logic, the client's connection eventually timing out with
    // no video ever received) — a real H.264 hardware-encoder resolution ceiling, not something
    // fixable from the client side. HEVC's encoder doesn't share that limit. Offering both lets
    // Sunshine keep using H.264 for the two monitors under the limit (no reason to give that up)
    // while it picks HEVC for the one that needs it.
    stream_config.supportedVideoFormats = (ml::VIDEO_FORMAT_H264 | ml::VIDEO_FORMAT_H265) as c_int;
    stream_config.colorSpace = ml::COLORSPACE_REC_601 as c_int;
    stream_config.colorRange = ml::COLOR_RANGE_LIMITED as c_int;
    stream_config.encryptionFlags = ml::ENCFLG_NONE as c_int;
    for (dst, src) in stream_config.remoteInputAesKey.iter_mut().zip(ri_key.iter()) {
        *dst = *src as c_char;
    }
    for (dst, src) in stream_config.remoteInputAesIv.iter_mut().zip(ri_key_iv.iter()) {
        *dst = *src as c_char;
    }

    let mut connection_callbacks: ml::CONNECTION_LISTENER_CALLBACKS = unsafe { std::mem::zeroed() };
    unsafe { ml::LiInitializeConnectionCallbacks(&mut connection_callbacks) };
    connection_callbacks.stageStarting = Some(on_stage_starting);
    connection_callbacks.stageComplete = Some(on_stage_complete);
    connection_callbacks.stageFailed = Some(on_stage_failed);
    connection_callbacks.connectionStarted = Some(on_connection_started);
    connection_callbacks.connectionTerminated = Some(on_connection_terminated);
    connection_callbacks.connectionStatusUpdate = Some(on_connection_status_update);
    // logMessage is C-variadic (`const char*, ...`) — stable Rust can't implement a matching
    // extern "C" fn pointer for that, so this just loses moonlight-common-c's own internal debug
    // log lines. stageFailed/connectionTerminated below still surface real problems.

    let mut decoder_callbacks: ml::DECODER_RENDERER_CALLBACKS = unsafe { std::mem::zeroed() };
    unsafe { ml::LiInitializeVideoCallbacks(&mut decoder_callbacks) };
    decoder_callbacks.setup = Some(on_decoder_setup);
    decoder_callbacks.start = Some(on_decoder_start);
    decoder_callbacks.stop = Some(on_decoder_stop);
    decoder_callbacks.cleanup = Some(on_decoder_cleanup);
    decoder_callbacks.submitDecodeUnit = Some(on_submit_decode_unit);

    let mut audio_callbacks: ml::AUDIO_RENDERER_CALLBACKS = unsafe { std::mem::zeroed() };
    unsafe { ml::LiInitializeAudioCallbacks(&mut audio_callbacks) };
    audio_callbacks.init = Some(on_audio_init);
    audio_callbacks.start = Some(on_audio_start);
    audio_callbacks.stop = Some(on_audio_stop);
    audio_callbacks.cleanup = Some(on_audio_cleanup);
    audio_callbacks.decodeAndPlaySample = Some(on_audio_decode_and_play_sample);

    tracing::info!(host, width = params.width, height = params.height, fps = params.fps, "starting LiStartConnection");
    let ret = unsafe {
        ml::LiStartConnection(
            &mut server_info,
            &mut stream_config,
            &mut connection_callbacks,
            &mut decoder_callbacks,
            &mut audio_callbacks,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        return Err(anyhow!("LiStartConnection failed with error code {ret}")).context("starting GameStream connection");
    }
    Ok(())
}

unsafe extern "C" fn on_stage_starting(stage: c_int) {
    tracing::info!(stage = stage_name(stage), "stage starting");
}

unsafe extern "C" fn on_stage_complete(stage: c_int) {
    tracing::info!(stage = stage_name(stage), "stage complete");
}

unsafe extern "C" fn on_stage_failed(stage: c_int, error_code: c_int) {
    tracing::error!(stage = stage_name(stage), error_code, "stage failed");
}

unsafe extern "C" fn on_connection_started() {
    tracing::info!("connection started");
}

/// Fired when moonlight-common-c's internal RTT/loss tracking detects the connection degrading
/// (see `CONN_STATUS_POOR`) or recovering — added specifically to distinguish "host encoder
/// falling behind" from "network dropping packets" as causes of observed video stutter, since
/// both would otherwise look identical from the outside (frames arrive late/out of order).
unsafe extern "C" fn on_connection_status_update(status: c_int) {
    if status == ml::CONN_STATUS_POOR as c_int {
        tracing::warn!("connection status: POOR (packet loss/high latency detected)");
    } else {
        tracing::info!("connection status: OKAY");
    }
}

unsafe extern "C" fn on_connection_terminated(error_code: c_int) {
    if error_code == 0 {
        tracing::info!("connection terminated gracefully (host app likely exited)");
    } else {
        tracing::error!(error_code, "connection terminated unexpectedly");
    }
}

fn stage_name(stage: c_int) -> String {
    unsafe {
        let ptr = ml::LiGetStageName(stage);
        if ptr.is_null() {
            format!("stage_{stage}")
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

unsafe extern "C" fn on_decoder_setup(video_format: c_int, width: c_int, height: c_int, _redraw_rate: c_int, _context: *mut std::os::raw::c_void, _dr_flags: c_int) -> c_int {
    tracing::info!(video_format, width, height, "decoder setup");
    let _ = VIDEO_FORMAT.set(video_format);
    0 // DR_OK
}

unsafe extern "C" fn on_decoder_start() {
    tracing::info!("decoder start");
}

unsafe extern "C" fn on_decoder_stop() {
    tracing::info!("decoder stop");
}

unsafe extern "C" fn on_decoder_cleanup() {
    tracing::info!("decoder cleanup");
}

/// Reassembles the `DECODE_UNIT`'s buffer chain (a linked list of `LENTRY`s, each a fragment of
/// one Annex-B frame — SPS/PPS/PPS/picture-data buffers for IDR frames, per `Limelight.h`) into
/// one contiguous blob and forwards it to `decode.rs` via the same channel the old QUIC
/// datagram-reassembly loop used to feed.
unsafe extern "C" fn on_submit_decode_unit(decode_unit: ml::PDECODE_UNIT) -> c_int {
    let du = &*decode_unit;
    let mut data = Vec::with_capacity(du.fullLength.max(0) as usize);
    let mut entry = du.bufferList;
    while !entry.is_null() {
        let e = &*entry;
        if !e.data.is_null() && e.length > 0 {
            data.extend_from_slice(std::slice::from_raw_parts(e.data as *const u8, e.length as usize));
        }
        entry = e.next;
    }

    match VIDEO_FRAME_TX.get() {
        Some(tx) => {
            if tx.send(data).is_err() {
                tracing::warn!("video frame channel closed, dropping decode unit");
            }
        }
        None => tracing::warn!("submitDecodeUnit called before stream::start finished setting up"),
    }
    0 // DR_OK
}

/// Fires once, before any audio samples arrive, with the negotiated Opus configuration — this is
/// the only point `audio.rs`'s `AudioPlayer` (which needs channel count/sample rate up front to
/// build its GStreamer pipeline) can be created. Spawns the dedicated feeder thread that owns it,
/// same pattern as video's decoder-feeder thread in `input_surface.rs`, to keep GStreamer's
/// blocking `push_buffer()` off whatever thread moonlight-common-c calls this callback from.
unsafe extern "C" fn on_audio_init(
    audio_configuration: c_int,
    opus_config: ml::POPUS_MULTISTREAM_CONFIGURATION,
    _context: *mut std::os::raw::c_void,
    _ar_flags: c_int,
) -> c_int {
    if opus_config.is_null() {
        tracing::error!("audio init called with a null opus config");
        return -1;
    }
    let cfg = &*opus_config;
    tracing::info!(
        audio_configuration,
        channels = cfg.channelCount,
        sample_rate = cfg.sampleRate,
        streams = cfg.streams,
        coupled_streams = cfg.coupledStreams,
        "audio init"
    );

    match crate::audio::AudioPlayer::new(cfg.channelCount, cfg.sampleRate) {
        Ok(player) => {
            // Bounded like the video channels above — same unbounded-memory-growth risk if
            // playback ever falls behind, just with smaller (Opus, not raw PCM) buffers.
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
            std::thread::spawn(move || {
                while let Ok(sample) = rx.recv() {
                    if let Err(e) = player.push_sample(&sample) {
                        tracing::warn!(error = ?e, "audio push_sample failed");
                    }
                }
            });
            if AUDIO_SAMPLE_TX.set(tx).is_err() {
                tracing::error!("audio init called more than once");
                return -1;
            }
            0 // success
        }
        Err(e) => {
            tracing::error!(error = ?e, "failed to create audio player");
            -1
        }
    }
}

unsafe extern "C" fn on_audio_start() {
    tracing::info!("audio start");
}

unsafe extern "C" fn on_audio_stop() {
    tracing::info!("audio stop");
}

unsafe extern "C" fn on_audio_cleanup() {
    tracing::info!("audio cleanup");
}

unsafe extern "C" fn on_audio_decode_and_play_sample(sample_data: *mut c_char, sample_length: c_int) {
    if sample_data.is_null() || sample_length <= 0 {
        return;
    }
    let data = std::slice::from_raw_parts(sample_data as *const u8, sample_length as usize).to_vec();
    match AUDIO_SAMPLE_TX.get() {
        Some(tx) => {
            let _ = tx.send(data);
        }
        None => tracing::warn!("decodeAndPlaySample called before audio init finished setting up"),
    }
}

/// The `VIDEO_FORMAT_*` bitflag `on_decoder_setup` negotiated (see `Limelight.h`) — read by
/// `input_surface.rs` to build a decode pipeline matching whatever codec Sunshine actually picked
/// (H.264 or H.265; see `start()`'s `supportedVideoFormats` comment for why both are offered).
/// Defaults to H264 if read before `on_decoder_setup` has fired, which shouldn't happen in
/// practice (it fires synchronously inside `LiStartConnection`, before `start()` returns).
pub fn video_format() -> i32 {
    *VIDEO_FORMAT.get().unwrap_or(&(ml::VIDEO_FORMAT_H264 as i32))
}
