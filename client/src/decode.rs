//! VAAPI decode + Wayland presentation for the Phase 1 MVP.
//!
//! Uses GStreamer (`vah264dec` -> `waylandsink`) rather than hand-rolled VAAPI/Wayland code,
//! since the Phase 1 decode-to-scanout spike already validated this exact element chain
//! against this machine's NVIDIA + nvidia-vaapi-driver setup. Per the Phase 1 finding in
//! PLAN.md, dma-buf export from VAAPI decode surfaces doesn't work on this driver, so
//! `waylandsink` falls back to composited (SHM) presentation rather than true direct scanout
//! — accepted tradeoff for the MVP, revisit only if the latency benchmark demands it.

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::collections::BTreeMap;

/// Reassembles fragmented video datagrams (QUIC datagrams are capped at
/// `rdproto::MAX_DATAGRAM_PAYLOAD` and are unreliable/unordered) into complete encoded frames.
pub struct FrameReassembler {
    partial: BTreeMap<u64, PartialFrame>,
    highest_seen: u64,
}

struct PartialFrame {
    fragments: Vec<Option<Vec<u8>>>,
    received: u16,
}

/// Drop any partial frame this many frame-ids behind the newest one seen — it's never going
/// to complete (a fragment was lost) and would otherwise leak memory forever.
const STALE_FRAME_WINDOW: u64 = 30;

impl FrameReassembler {
    pub fn new() -> Self {
        Self {
            partial: BTreeMap::new(),
            highest_seen: 0,
        }
    }

    /// Feed one received datagram's header + payload. Returns the complete frame bytes once
    /// every fragment of that frame has arrived.
    pub fn push(&mut self, header: &rdproto::VideoDatagramHeader, payload: &[u8]) -> Option<Vec<u8>> {
        self.highest_seen = self.highest_seen.max(header.frame_id);
        self.partial
            .retain(|&id, _| id + STALE_FRAME_WINDOW >= self.highest_seen);

        if header.fragment_count == 1 {
            return Some(payload.to_vec());
        }

        let entry = self.partial.entry(header.frame_id).or_insert_with(|| PartialFrame {
            fragments: vec![None; header.fragment_count as usize],
            received: 0,
        });

        let slot = entry.fragments.get_mut(header.fragment_index as usize)?;
        if slot.is_none() {
            *slot = Some(payload.to_vec());
            entry.received += 1;
        }

        if entry.received == header.fragment_count {
            let complete = self.partial.remove(&header.frame_id).unwrap();
            let mut assembled = Vec::new();
            for fragment in complete.fragments.into_iter() {
                assembled.extend_from_slice(&fragment?);
            }
            Some(assembled)
        } else {
            None
        }
    }
}

pub struct VideoDecoder {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
}

impl VideoDecoder {
    /// `fullscreen_output`: Wayland output name (e.g. "DP-2") to present on, or `None` for
    /// whatever the compositor picks as default. Phase 1 MVP targets a single monitor.
    pub fn new(fullscreen_output: Option<&str>) -> Result<Self> {
        // Must be set before gst::init() scans plugins — libva-nvidia-driver isn't on the
        // `va` plugin's default vendor allow-list otherwise (see Phase 1 spike in PLAN.md).
        std::env::set_var("GST_VA_ALL_DRIVERS", "1");
        gst::init().context("gst::init")?;

        let pipeline = gst::Pipeline::new();

        let caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .do_timestamp(true)
            .build();

        // A `queue` decouples appsrc's push thread from the decode/present thread — without
        // it, GStreamer warns "add queues" and live buffers get dropped under any hiccup.
        let queue = gst::ElementFactory::make("queue").build().context("creating queue")?;
        let h264parse = gst::ElementFactory::make("h264parse")
            .build()
            .context("creating h264parse (is gstreamer1-plugins-bad installed?)")?;
        // Tried avdec_h264 (software decode) hoping to skip VAAPI's GPU round-trip, but it
        // produced visible frame corruption (stride/buffer-layout mismatch feeding waylandsink,
        // not a frame-loss artifact — reproduced consistently, not just occasionally). Back to
        // vah264dec, which decoded cleanly; the release-build + shorter-keyframe-interval wins
        // stay either way.
        let decoder = gst::ElementFactory::make("vah264dec")
            .build()
            .context("creating vah264dec (is gstreamer1-plugins-bad installed + GST_VA_ALL_DRIVERS set?)")?;
        let sink_builder = gst::ElementFactory::make("waylandsink").property("fullscreen", true);
        let sink = if let Some(output) = fullscreen_output {
            sink_builder.property("fullscreen-output", output)
        } else {
            sink_builder
        }
        .build()
        .context("creating waylandsink")?;

        pipeline
            .add_many([appsrc.upcast_ref(), &queue, &h264parse, &decoder, &sink])
            .context("adding elements to pipeline")?;
        gst::Element::link_many([appsrc.upcast_ref(), &queue, &h264parse, &decoder, &sink])
            .context("linking appsrc -> queue -> h264parse -> vah264dec -> waylandsink")?;

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
