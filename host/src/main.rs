use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::prelude::*;

mod mmt;
mod sunshine;
mod topology;
mod vdd;

/// One-shot interactive setup tool for multi-monitor streaming on cwtrow: configures
/// MolotovCherry/virtual-display-rs's 3 virtual monitors to match the Linux client's real physical
/// layout, positions them via MultiMonitorTool, then configures and launches one Sunshine instance
/// per monitor (see PLAN.md's multi-monitor architecture notes for why "one Sunshine instance per
/// monitor" rather than one instance streaming a combined desktop — GameStream is fundamentally
/// single-display-per-session).
///
/// Must be run **interactively** (double-click, or from a terminal in the actual console/KVM
/// session) — not over SSH. Confirmed the hard way: MultiMonitorTool's monitor enumeration
/// silently returns empty results when run from an SSH-spawned Session 0 process, even with
/// another interactive session active elsewhere on the machine (the virtual display driver's own
/// CLI doesn't have this restriction — only positioning via MultiMonitorTool does). Must also run
/// **elevated** (Administrator) — the Sunshine service restarts require it.
#[derive(Parser, Debug)]
struct Args {
    /// Path to MultiMonitorTool.exe. Defaults to looking next to this executable.
    #[arg(long)]
    mmt_path: Option<std::path::PathBuf>,
    /// Path to virtual-display-driver-cli.exe. Defaults to looking next to this executable.
    #[arg(long)]
    vdd_cli_path: Option<std::path::PathBuf>,
    /// Tear down instead of setting up: removes all virtual monitors and stops the extra
    /// (non-default) Sunshine instances this tool launched. Run this once a streaming session is
    /// done, rather than leaving 3 phantom monitors sitting in Display Settings indefinitely.
    #[arg(long)]
    teardown: bool,
}

fn main() {
    let log_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())).unwrap_or_else(|| ".".into());
    eprintln!("logging to {}", log_dir.join("rdhost.log").display());
    let file_appender = tracing_appender::rolling::never(&log_dir, "rdhost.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let env_filter = || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter()))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file_writer).with_filter(env_filter()))
        .init();

    // Belt-and-suspenders for the exact failure mode this was written to fix: the console
    // window (opened via right-click "Run as administrator") closes before anyone can read a
    // top-level error, and by default neither an Err returned from main() nor a panic go through
    // tracing at all — they'd only ever have reached the console, never rdhost.log. Both now do.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panicked: {info}");
    }));

    let args = Args::parse();
    if let Err(e) = run(args) {
        tracing::error!("failed: {e:#}");
        eprintln!("failed: {e:#}");
        drop(guard); // flush the non-blocking log writer before exiting
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    require_elevated()?;

    let mmt_path = args.mmt_path.unwrap_or_else(|| {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.join("MultiMonitorTool.exe"))).unwrap_or_else(|| "MultiMonitorTool.exe".into())
    });
    if !mmt_path.exists() {
        bail!("MultiMonitorTool.exe not found at {} — pass --mmt-path or place it next to this executable", mmt_path.display());
    }
    let vdd_cli_path = args.vdd_cli_path.unwrap_or_else(|| {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.join("virtual-display-driver-cli.exe"))).unwrap_or_else(|| "virtual-display-driver-cli.exe".into())
    });
    if !vdd_cli_path.exists() {
        bail!("virtual-display-driver-cli.exe not found at {} — pass --vdd-cli-path or place it next to this executable", vdd_cli_path.display());
    }

    if args.teardown {
        sunshine::stop_extra_instances()?;
        vdd::remove_all(&vdd_cli_path)?;
        info!("teardown complete");
        println!("\nVirtual monitors removed and extra Sunshine instances stopped.");
        return Ok(());
    }

    // Stop any extra instances from a previous run *before* launching fresh ones — without this,
    // re-running rdhost.exe (e.g. to pick up a config change) leaves the old processes running
    // alongside the new ones, silently answering requests with stale config since they got there
    // first and are still holding the port. Confirmed the hard way: a `csrf_allowed_origins` fix
    // kept appearing to not work because a stale pre-fix process was still bound to the port.
    sunshine::stop_extra_instances()?;

    let targets = topology::target_topology();
    info!(count = targets.len(), "configuring virtual display topology");

    vdd::configure(&vdd_cli_path, &targets)?;

    // Give Windows a moment to settle each freshly-added virtual monitor's display identity and
    // any auto-arrangement it applies to a newly-detected display before we capture names and
    // reposition — unlike stock VDD (which created all 3 monitors atomically via one PnP device
    // restart), this driver's CLI adds them one at a time over a live IPC pipe, so there's more
    // room for MultiMonitorTool to grab a display name that's about to be reassigned, or to
    // reposition a monitor before Windows' own default "extend" placement for it has finished.
    std::thread::sleep(std::time::Duration::from_secs(3));

    let monitors = mmt::list(&mmt_path)?;
    let vdd_monitors: Vec<&mmt::Monitor> = monitors.iter().filter(|m| m.is_vdd).collect();
    let other_monitors: Vec<&mmt::Monitor> = monitors.iter().filter(|m| !m.is_vdd).collect();
    if vdd_monitors.len() != targets.len() {
        bail!(
            "expected {} VDD-owned monitors, found {} — check `virtual-display-driver-cli.exe list` and that the driver came up correctly",
            targets.len(),
            vdd_monitors.len()
        );
    }
    // Assignment is by resolution, not raw CSV/enumeration order — MultiMonitorTool's enumeration
    // order was found to disagree with whatever order Windows itself used when actually applying
    // `/SetMonitors`, silently swapping which of two same-shaped-topology targets a given monitor
    // ended up at. Every target's resolution is distinct (topology.rs guarantees this — same
    // property `sunshine::find_device_id` already relies on), so matching by (width, height)
    // sidesteps whatever's reordering things instead of trying to chase it further.
    let mut assignments: Vec<(&mmt::Monitor, &topology::TargetMonitor)> = Vec::new();
    for target in &targets {
        let monitor = vdd_monitors
            .iter()
            .find(|m| m.width == target.width && m.height == target.height)
            .copied()
            .with_context(|| format!("no VDD-owned monitor found matching {}x{} for {}", target.width, target.height, target.label))?;
        info!(display = monitor.name, label = target.label, "assigned");
        assignments.push((monitor, target));
    }
    // Real (non-VDD) monitors get pushed out of the way rather than left alone — confirmed the
    // hard way that leaving one in the middle of the intended layout makes Windows auto-space
    // the VDD monitors apart to avoid overlapping it, breaking the edge-to-edge arrangement.
    mmt::apply(&mmt_path, &assignments, &other_monitors)?;

    // Verify what Windows actually recorded, rather than trusting the command we just sent —
    // positioning has been unreliable enough (see PLAN.md) to be worth confirming directly. Match
    // by resolution again here too, for the same reason as the initial assignment above — the
    // `\\.\DISPLAYn` name a monitor had *before* `apply()` isn't guaranteed to still refer to the
    // same monitor *after* it (this is what caused the swap in the first place).
    let verify = mmt::list(&mmt_path)?;
    for (_, target) in &assignments {
        let actual = verify
            .iter()
            .find(|m| m.width == target.width && m.height == target.height)
            .map(|m| m.left_top.as_str())
            .unwrap_or("?");
        info!(label = target.label, requested_x = target.x, requested_y = target.y, actual_left_top = actual, "post-apply position check");
    }

    // Give Windows a moment to actually commit the just-applied modes to its own display config
    // before restarting Sunshine to re-enumerate — confirmed necessary in practice: without this,
    // Sunshine's enumeration sometimes ran fast enough to still see a stale (pre-resize) mode for
    // whichever monitor was resized last, failing `find_device_id`'s resolution match.
    std::thread::sleep(std::time::Duration::from_secs(3));

    let sunshine_exe = sunshine::default_sunshine_exe();
    let mut summary = Vec::new();
    let displays = sunshine::discover_displays()?;
    for target in &targets {
        let device_id = sunshine::find_device_id(&displays, target)?;
        if target.sunshine_port == 47989 {
            sunshine::update_default_instance_output_name(device_id)?;
        } else {
            // Dropping a std::process::Child handle doesn't kill the child (unlike some
            // other languages' process APIs) — it keeps running independently once spawned,
            // exactly what we want here.
            sunshine::launch_instance(target, device_id, &sunshine_exe)?;
        }
        summary.push((target.label, target.sunshine_port));
    }

    info!("setup complete");
    println!("\nSunshine instances ready — pair the client with each one (web UI PIN entry, then --host <ip> --port <port> once the client supports it):");
    for (label, port) in summary {
        println!("  {label}: port {port}  (web UI: https://192.168.1.55:{})", port + 1);
    }
    Ok(())
}

fn require_elevated() -> Result<()> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
        ])
        .output()
        .context("checking for Administrator privileges")?;
    let is_admin = String::from_utf8_lossy(&output.stdout).trim().eq_ignore_ascii_case("true");
    if !is_admin {
        bail!("this must be run as Administrator (right-click -> Run as administrator) — PnP device and service restarts need it");
    }
    Ok(())
}
