//! Phase 3: audio playback. `moonlight-common-c`'s `AudioRendererDecodeAndPlaySample` callback
//! (wired in `stream.rs`) hands us raw Opus packets — despite the name, it does *not* decode
//! them itself; the callback is responsible for decoding and playing. Reuses GStreamer (already
//! a dependency for video decode, see `decode.rs`) rather than adding a separate libopus
//! binding: `appsrc ! opusdec ! audioconvert ! audioresample ! autoaudiosink`.
//!
//! `autoaudiosink` rather than hardcoding `pipewiresink` — PLAN.md's Phase 3 design specifically
//! called out PipeWire playback, but `autoaudiosink` resolves to PipeWire in practice on this
//! target system (PipeWire is the default audio server on modern Fedora/Nobara) while being
//! more robust to any pipewiresink-specific session/permission quirks than hardcoding it.
//!
//! Current scope only handles simple mono/stereo Opus (`channel-mapping-family=0`) — matches
//! `stream.rs` negotiating `AUDIO_CONFIGURATION_STEREO`. Surround configurations use libopus's
//! multistream API with a channel mapping table (see `OPUS_MULTISTREAM_CONFIGURATION` in
//! `Limelight.h`) that this doesn't handle yet; revisit if surround support is ever needed.
//!
//! **Not yet live-tested** — written from GStreamer's documented `audio/x-opus` raw-caps
//! decoding support (the same mechanism WebRTC/RTP Opus pipelines use, e.g. after
//! `rtpopusdepay`), but this hasn't been run against real Sunshine audio yet.

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

pub struct AudioPlayer {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
}

impl AudioPlayer {
    pub fn new(channels: i32, sample_rate: i32) -> Result<Self> {
        // gst::init() is idempotent — safe to call whether this or decode.rs's VideoDecoder
        // happens to run first per process. Real bug hit here: this used to be the *first*
        // gst::init() call (LiStartConnection's internal stage order sets up audio before
        // input_surface.rs gets around to creating the video decoder), which meant
        // GST_VA_ALL_DRIVERS — needed by decode.rs's vah264dec, but set too late by the time
        // video initialized — never took effect. It's now set once in main.rs before anything
        // GStreamer-related runs, so this doesn't matter anymore, but don't reintroduce a
        // per-subsystem env::set_var here.
        gst::init().context("gst::init")?;

        let pipeline = gst::Pipeline::new();

        let caps = gst::Caps::builder("audio/x-opus")
            .field("channels", channels)
            .field("rate", sample_rate)
            .field("channel-mapping-family", 0)
            .build();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .is_live(true)
            .do_timestamp(true)
            .build();

        let queue = gst::ElementFactory::make("queue").build().context("creating queue")?;
        let opusdec = gst::ElementFactory::make("opusdec").build().context("creating opusdec")?;
        let audioconvert = gst::ElementFactory::make("audioconvert").build().context("creating audioconvert")?;
        let audioresample = gst::ElementFactory::make("audioresample").build().context("creating audioresample")?;
        let sink = gst::ElementFactory::make("autoaudiosink").build().context("creating autoaudiosink")?;

        pipeline
            .add_many([appsrc.upcast_ref(), &queue, &opusdec, &audioconvert, &audioresample, &sink])
            .context("adding elements to audio pipeline")?;
        gst::Element::link_many([appsrc.upcast_ref(), &queue, &opusdec, &audioconvert, &audioresample, &sink])
            .context("linking appsrc -> queue -> opusdec -> audioconvert -> audioresample -> autoaudiosink")?;

        let bus = pipeline.bus().ok_or_else(|| anyhow!("audio pipeline has no bus"))?;
        std::thread::spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(e) => {
                        tracing::error!(src = ?e.src().map(|s| s.path_string()), error = %e.error(), debug = ?e.debug(), "gstreamer audio pipeline error");
                    }
                    MessageView::Warning(w) => tracing::warn!(warning = %w.error(), "gstreamer audio pipeline warning"),
                    MessageView::Eos(_) => {
                        tracing::info!("gstreamer audio pipeline reached EOS");
                        break;
                    }
                    _ => {}
                }
            }
        });

        pipeline.set_state(gst::State::Playing).context("setting audio pipeline to Playing")?;

        tracing::info!(channels, sample_rate, "audio player ready");
        Ok(Self { pipeline, appsrc })
    }

    pub fn push_sample(&self, data: &[u8]) -> Result<()> {
        let buffer = gst::Buffer::from_slice(data.to_vec());
        self.appsrc.push_buffer(buffer).map_err(|e| anyhow!("appsrc push_buffer failed: {e:?}"))?;
        Ok(())
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
