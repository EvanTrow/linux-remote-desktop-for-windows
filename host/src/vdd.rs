//! Configures MolotovCherry/virtual-display-rs's virtual monitors via its `virtual-display-driver-cli.exe`,
//! which talks to the driver over a local named pipe (no config file + device restart dance, unlike
//! stock VirtualDrivers/Virtual-Display-Driver — see PLAN.md for why that driver was abandoned:
//! it has a confirmed, unfixed upstream bug where all monitors revert to a single shared resolution
//! a few seconds after being set). Position/rotation still aren't exposed here — that's `mmt.rs`'s
//! job, using MultiMonitorTool, which needs an *already-existing* monitor to reposition, hence this
//! runs first.
//!
//! Must use the driver's **dev-branch** build, not the last tagged release (v0.3.1) — confirmed the
//! hard way that the release build's IPC pipe denies access to every caller (even elevated
//! Administrator, even from the interactive console session), while the dev build's pipe grants
//! access to anyone by design (its own source comment: "these security attributes will allow
//! anyone access, so local account does not need admin privileges to use it"). The release's pipe
//! ACL bug is exactly what "no admin access required for state updates to driver" in the dev
//! branch's changelog (GitHub issue #115) was fixing.

use crate::topology::TargetMonitor;
use anyhow::{Context, Result};
use std::path::Path;

/// Wipes any existing virtual monitors and adds one per target, in topology order. IDs are
/// intentionally left unspecified (auto-assigned) — explicit `--id` was found to unreliably
/// report "already exists" even immediately after `remove-all` on this driver build; monitors are
/// instead correlated back to targets later by resolution (`sunshine::find_device_id`,
/// `mmt.rs`'s resolution-based split), which only needs each target's resolution to be unique —
/// already guaranteed by `topology.rs`.
pub fn configure(cli_path: &Path, monitors: &[TargetMonitor]) -> Result<()> {
    run(cli_path, &["remove-all"]).context("clearing existing virtual monitors")?;

    for m in monitors {
        let mode = format!("{}x{}@60", m.width, m.height);
        run(cli_path, &["add", &mode, "--name", m.label]).with_context(|| format!("adding virtual monitor for {}", m.label))?;
        tracing::info!(label = m.label, mode, "added virtual monitor");
    }

    Ok(())
}

/// Removes all virtual monitors — used by `rdhost --teardown` to clean up after a streaming
/// session ends, rather than leaving them sitting in Display Settings indefinitely.
pub fn remove_all(cli_path: &Path) -> Result<()> {
    run(cli_path, &["remove-all"])
}

fn run(cli_path: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new(cli_path)
        .args(args)
        .output()
        .with_context(|| format!("running {} {:?}", cli_path.display(), args))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} {:?} exited with {:?}: {}",
            cli_path.display(),
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
