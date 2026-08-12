use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tracing::info;

use rdclient::{gamestream, input_surface, pairing, stream};

/// Linux client for streaming from a Sunshine host via the Moonlight/GameStream protocol (see
/// PLAN.md's architecture-pivot section — this replaced an earlier from-scratch QUIC protocol).
/// Phase 1 MVP scope carries over: single monitor, no clipboard/audio yet.
#[derive(Parser, Debug)]
struct Args {
    /// Sunshine host to stream from (bare hostname/IP, no port) — must already be paired (see
    /// --pair-host) before this will work.
    #[arg(long)]
    host: Option<String>,

    /// Sunshine's base port (its `port` config value — offsets its whole port family together,
    /// see pairing.rs's `Ports`). Multiple monitors mean multiple Sunshine instances on cwtrow,
    /// each on a different base port (see host/src/topology.rs) — this selects which one.
    #[arg(long, default_value_t = 47989)]
    port: u16,

    /// GameStream/Sunshine host to pair with (bare hostname/IP, no port) — see pairing.rs.
    /// Requires --pair-pin. Exits after pairing; does not stream. One-time setup per host. Uses
    /// --port same as normal streaming, since each Sunshine instance needs its own pairing.
    #[arg(long)]
    pair_host: Option<String>,

    /// PIN to pair with, as entered into the host's Sunshine web UI. Required with --pair-host.
    #[arg(long)]
    pair_pin: Option<String>,

    /// Capture real local keyboard/mouse and forward it to the host, via a dedicated Wayland
    /// input surface (see input_surface.rs) — real absolute cursor position, and naturally
    /// focus-scoped (only captures while that surface has focus).
    #[arg(long)]
    capture_input: bool,

    /// Wayland output to present fullscreen on (e.g. "DP-2"). Defaults to the compositor's
    /// choice if unset. Phase 1 MVP targets a single monitor.
    #[arg(long)]
    output: Option<String>,

    /// Requested stream resolution/framerate/bitrate. Defaults match Phase 1's single-monitor
    /// MVP target; Phase 2 will need to probe real client topology instead of hardcoding this.
    #[arg(long, default_value_t = 1920)]
    width: i32,
    #[arg(long, default_value_t = 1080)]
    height: i32,
    #[arg(long, default_value_t = 60)]
    fps: i32,
    #[arg(long, default_value_t = 20000)]
    bitrate_kbps: i32,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Must happen before *any* GStreamer gst::init() call, from any subsystem — GStreamer scans
    // its plugin registry on the first gst::init() and caches it, so setting this later (e.g.
    // inside decode.rs's VideoDecoder::new(), where it lived originally) only works if video
    // happens to initialize GStreamer before audio does. It doesn't: LiStartConnection's internal
    // stage order runs audio setup (audio.rs's AudioPlayer::new(), added for Phase 3) before
    // input_surface.rs gets around to creating the VideoDecoder, so audio was winning the race
    // and video's vah264dec silently never saw libva-nvidia-driver on the `va` plugin's vendor
    // allow-list (see PLAN.md's Phase 1 VAAPI findings) — confirmed the hard way: "Error: creating
    // video decoder" on the first real test after adding audio.
    std::env::set_var("GST_VA_ALL_DRIVERS", "1");

    tracing_subscriber::fmt::init();
    let args = Args::parse();

    if let Some(pair_host) = &args.pair_host {
        let pin = args.pair_pin.as_deref().ok_or_else(|| anyhow!("--pair-pin is required with --pair-host"))?;
        let identity = pairing::ClientIdentity::load_or_generate()?;
        pairing::pair(&identity, pair_host, args.port, pin).await?;
        return Ok(());
    }

    let host = args.host.ok_or_else(|| anyhow!("--host is required (unless using --pair-host)"))?;
    let identity = pairing::ClientIdentity::load_or_generate()?;

    let app = gamestream::pick_app(&identity, &host, args.port).await.context("selecting an app to launch")?;

    let mut ri_key = [0u8; 16];
    let mut ri_key_iv = [0u8; 16];
    openssl::rand::rand_bytes(&mut ri_key)?;
    openssl::rand::rand_bytes(&mut ri_key_iv)?;
    // GameStream derives the input-encryption key ID from the first 4 bytes of the IV,
    // big-endian — matches moonlight-qt's convention (see gamestream.rs's launch() doc comment).
    let ri_key_id = i32::from_be_bytes(ri_key_iv[..4].try_into().unwrap());

    let session = gamestream::launch(&identity, &host, args.port, &app, args.width, args.height, args.fps, &ri_key, ri_key_id)
        .await
        .context("launching stream session")?;

    // Bounded, not unbounded — see decode.rs's VideoDecoder::new doc comment for why an
    // unbounded queue anywhere in this pipeline is a real system-OOM risk under load. These are
    // compressed decode units (much smaller than the raw frames further downstream), so a modest
    // bound rather than 1 — no need to be as aggressive about forcing backpressure here.
    let (encoded_tx, encoded_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    stream::start(
        &session,
        &host,
        &stream::StreamParams {
            width: args.width,
            height: args.height,
            fps: args.fps,
            bitrate_kbps: args.bitrate_kbps,
        },
        &ri_key,
        &ri_key_iv,
        encoded_tx,
    )?;

    if args.capture_input {
        info!("input capture enabled (focus-scoped)");
    }

    // Without this, killing the process (Ctrl+C, `kill`/`pkill`'s default SIGTERM, or anything
    // short of a clean return from input_surface::run below) leaves the host's app session
    // marked active — confirmed the hard way, repeatedly, in testing: an interrupted run leaves
    // Sunshine rejecting the next /launch with "an app is already running" until either
    // gamestream.rs's /resume fallback works (not reliable yet — see its doc comment) or the
    // Sunshine service gets restarted by hand. LiStopConnection() tells the host the session
    // ended on purpose. Catching only SIGINT (ctrl_c) wasn't enough — SIGTERM is `pkill`'s
    // default signal and was hitting this exact bug during iteration.
    tokio::spawn(async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        info!("shutting down");
        unsafe { moonlight_sys::LiStopConnection() };
        std::process::exit(0);
    });

    // input_surface.rs owns the one client-visible window: presents decoded video and, if
    // capture_input is set, captures real keyboard/mouse input. Runs on this thread — it blocks
    // for the life of the stream.
    input_surface::run(encoded_rx, args.output, args.capture_input)
}
