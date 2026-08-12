//! VAAPI decode for the Phase 1 MVP. Presentation lives in `input_surface.rs`, not here —
//! this used to also own a `waylandsink` for display, but that made input capture need its
//! own *second* window (GStreamer sinks are a black box with no input-event access), which
//! caused a real bug: two independently-managed surfaces with different coordinate spaces
//! covering the same screen, so clicks didn't line up with what was on screen. Now this only
//! decodes and hands raw frames to whoever wants to present them.
//!
//! Uses GStreamer (`vah264dec`) rather than hand-rolled VAAPI code, since the Phase 1
//! decode-to-scanout spike already validated this element against this machine's NVIDIA +
//! nvidia-vaapi-driver setup. Per that finding (see PLAN.md), dma-buf export from VAAPI
//! decode surfaces doesn't work on this driver, so frames come out via a CPU-readable `appsink`
//! (composited presentation) rather than true zero-copy scanout — accepted tradeoff for the
//! MVP, revisit only if the latency benchmark demands it.

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::sync::mpsc::SyncSender;

/// One decoded, presentation-ready frame — tightly packed BGRx8888, which is byte-for-byte
/// identical to Wayland's `wl_shm::Format::Xrgb8888` (little-endian B,G,R,X per pixel), so the
/// receiver can wl_shm-present it directly with no further conversion.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

pub struct VideoDecoder {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
}

/// `VIDEO_FORMAT_MASK_H265` from `Limelight.h` (`0x0F00`) — any bit set there means the
/// negotiated codec is some HEVC profile rather than H.264. Duplicated here rather than pulled
/// from `moonlight_sys` to keep this module decoupled from the FFI crate; it's a stable ABI
/// constant, not something that needs to track upstream.
const VIDEO_FORMAT_MASK_H265: i32 = 0x0F00;

impl VideoDecoder {
    /// `frame_tx`: every decoded frame is sent here as soon as it's ready. Fed from a
    /// GStreamer-internal streaming thread (the `appsink` callback), not the caller's thread.
    /// Must be a bounded (`sync_channel`) sender with a small capacity, not `channel()`'s
    /// unbounded one — confirmed the hard way running 3 concurrent streams (one at 5120x1440,
    /// ~29MB/frame): if the presentation thread ever falls a little behind decode (entirely
    /// possible with 3 windows competing for the same GPU/compositor), an unbounded channel here
    /// queues full uncompressed frames without limit, and memory usage runs away fast enough to
    /// trigger the whole *system's* low-memory killer, not just this process. A bounded sender
    /// makes `send()` block instead, naturally rate-limiting decode to whatever presentation can
    /// keep up with.
    ///
    /// `video_format`: the `VIDEO_FORMAT_*` bitflag `stream.rs`'s `on_decoder_setup` negotiated
    /// (see `stream::video_format()`) — selects between an H.264 and an H.265 pipeline. Added
    /// alongside HEVC support after cwtrow's AMD AMF H.264 hardware encoder turned out unable to
    /// init at all above a certain width (see `stream.rs`'s `supportedVideoFormats` comment);
    /// H.264-only was fine for Phase 1's single 1920x1080 monitor but not the ultrawide.
    pub fn new(frame_tx: SyncSender<DecodedFrame>, video_format: i32) -> Result<Self> {
        // GST_VA_ALL_DRIVERS is set in main.rs, before this or any other GStreamer subsystem
        // (audio.rs included) gets a chance to call gst::init() first — see main.rs for why it
        // has to happen there and not here.
        gst::init().context("gst::init")?;

        let is_hevc = video_format & VIDEO_FORMAT_MASK_H265 != 0;
        tracing::info!(video_format, is_hevc, "building decode pipeline");

        let pipeline = gst::Pipeline::new();

        let caps = gst::Caps::builder(if is_hevc { "video/x-h265" } else { "video/x-h264" })
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .do_timestamp(true)
            .build();

        // A `queue` decouples appsrc's push thread from the decode thread — without it,
        // GStreamer warns "add queues" and live buffers get dropped under any hiccup.
        let queue = gst::ElementFactory::make("queue").build().context("creating queue")?;
        let parse_name = if is_hevc { "h265parse" } else { "h264parse" };
        let parser = gst::ElementFactory::make(parse_name)
            .build()
            .with_context(|| format!("creating {parse_name} (is gstreamer1-plugins-bad installed?)"))?;
        // Tried avdec_h264 (software decode) hoping to skip VAAPI's GPU round-trip, but it
        // produced visible frame corruption (stride/buffer-layout mismatch, not a frame-loss
        // artifact — reproduced consistently). vah264dec decodes cleanly; the release-build +
        // shorter-keyframe-interval wins stay either way.
        //
        // HEVC uses `nvh265dec` (NVDEC), not `vah265dec` — this machine's VA-API stack doesn't
        // register an HEVC decode entry point at all (`gst-inspect-1.0 vah265dec` finds nothing),
        // while `nvh265dec` (from the separate `nvcodec` plugin, direct NVDEC access) does and
        // accepts the same byte-stream/au caps as the H.264 path.
        let decoder_name = if is_hevc { "nvh265dec" } else { "vah264dec" };
        let decoder = gst::ElementFactory::make(decoder_name)
            .build()
            .with_context(|| format!("creating {decoder_name} (is gstreamer1-plugins-bad installed + GST_VA_ALL_DRIVERS set?)"))?;
        let videoconvert = gst::ElementFactory::make("videoconvert")
            .build()
            .context("creating videoconvert")?;
        // BGRx matches wl_shm's Xrgb8888 memory layout exactly — no conversion needed on the
        // presentation side, just copy the mapped buffer straight into a wl_shm buffer.
        let convert_caps = gst::Caps::builder("video/x-raw").field("format", "BGRx").build();
        let appsink = gst_app::AppSink::builder()
            .caps(&convert_caps)
            .sync(false) // present ASAP, don't throttle to the stream's own timestamps
            .max_buffers(1)
            .drop(true) // always the latest frame; never build a backlog under load
            .build();

        pipeline
            .add_many([appsrc.upcast_ref(), &queue, &parser, &decoder, &videoconvert, appsink.upcast_ref()])
            .context("adding elements to pipeline")?;
        gst::Element::link_many([appsrc.upcast_ref(), &queue, &parser, &decoder, &videoconvert, appsink.upcast_ref()])
            .with_context(|| format!("linking appsrc -> queue -> {parse_name} -> {decoder_name} -> videoconvert -> appsink"))?;

        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let s = caps.structure(0).ok_or(gst::FlowError::Error)?;
                    let width: i32 = s.get("width").map_err(|_| gst::FlowError::Error)?;
                    let height: i32 = s.get("height").map_err(|_| gst::FlowError::Error)?;
                    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n % 60 == 0 {
                        tracing::info!(n, width, height, bytes = map.size(), "appsink decoded frame");
                    }
                    let _ = frame_tx.send(DecodedFrame {
                        width: width as u32,
                        height: height as u32,
                        data: map.as_slice().to_vec(),
                    });
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
        std::thread::spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(e) => {
                        tracing::error!(
                            src = ?e.src().map(|s| s.path_string()),
                            error = %e.error(),
                            debug = ?e.debug(),
                            "gstreamer pipeline error"
                        );
                    }
                    MessageView::Warning(w) => {
                        tracing::warn!(warning = %w.error(), "gstreamer pipeline warning");
                    }
                    MessageView::Eos(_) => {
                        tracing::info!("gstreamer pipeline reached EOS");
                        break;
                    }
                    _ => {}
                }
            }
        });

        pipeline
            .set_state(gst::State::Playing)
            .context("setting pipeline to Playing")?;

        Ok(Self { pipeline, appsrc })
    }

    pub fn push_frame(&self, data: &[u8]) -> Result<()> {
        let buffer = gst::Buffer::from_slice(data.to_vec());
        self.appsrc
            .push_buffer(buffer)
            .map_err(|e| anyhow!("appsrc push_buffer failed: {e:?}"))?;
        Ok(())
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
