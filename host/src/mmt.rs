//! Wraps NirSoft's MultiMonitorTool.exe for positioning/resizing/rotating monitors — the virtual
//! display driver (see `vdd.rs`) only controls monitor *count* and *available* resolutions, not
//! position/rotation, which Windows doesn't expose via any driver-level config at all; this is the
//! same tool a human would otherwise drive through Display Settings by hand.
//!
//! Must run from an interactive session — confirmed the hard way that MultiMonitorTool silently
//! returns empty results when run from an SSH-spawned (Session 0) process, even with another
//! interactive session active elsewhere on the same machine (a stricter version of the same
//! class of problem as DDA capture's session requirement, see PLAN.md's Phase 1 findings). This
//! is exactly why this whole tool exists as something the user launches interactively rather
//! than something driven over SSH.

use crate::topology::TargetMonitor;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

pub struct Monitor {
    /// `\\.\DISPLAYn` — MultiMonitorTool's own device identifier, used both to select and to
    /// build `/SetMonitors` commands. NOT stable enough to correlate a monitor across two
    /// separate `list()` calls (e.g. before vs. after `apply()`) — see `width`/`height` below.
    pub name: String,
    pub is_vdd: bool,
    /// Raw "Left-Top" column from MultiMonitorTool's CSV (e.g. `"0, 0"`) — Windows' own record of
    /// this monitor's current position, for verifying what actually took effect after `apply()`
    /// rather than trusting the command we sent.
    pub left_top: String,
    /// Derived from "Left-Top"/"Right-Bottom", not parsed from the "Resolution" column (whose
    /// exact string format wasn't worth depending on). Used to correlate a monitor to a
    /// `TargetMonitor` by its distinct resolution rather than by `name` — confirmed necessary in
    /// practice: assigning by raw CSV enumeration order produced a *different* monitor<->target
    /// pairing than whatever Windows actually used when applying `/SetMonitors`, silently
    /// swapping two targets' positions with each other.
    pub width: i32,
    pub height: i32,
}

pub fn list(mmt_path: &Path) -> Result<Vec<Monitor>> {
    let csv_path = std::env::temp_dir().join("rdhost_mmt_monitors.csv");
    let status = std::process::Command::new(mmt_path)
        .arg("/scomma")
        .arg(&csv_path)
        .status()
        .context("running MultiMonitorTool.exe /scomma")?;
    if !status.success() {
        anyhow::bail!("MultiMonitorTool /scomma exited with {:?}", status.code());
    }
    // The tool writes the file asynchronously relative to process exit in practice — give it a
    // moment rather than racing a read against an empty/partial file.
    std::thread::sleep(std::time::Duration::from_secs(1));

    let mut reader = csv::Reader::from_path(&csv_path).context("reading MultiMonitorTool CSV output")?;
    let headers = reader.headers()?.clone();
    let name_idx = header_index(&headers, "Name")?;
    let monitor_name_idx = header_index(&headers, "Monitor Name")?;
    let monitor_string_idx = header_index(&headers, "Monitor String")?;
    let left_top_idx = header_index(&headers, "Left-Top")?;
    let right_bottom_idx = header_index(&headers, "Right-Bottom")?;

    let mut monitors = Vec::new();
    for record in reader.records() {
        let record = record?;
        let name = record.get(name_idx).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let monitor_name = record.get(monitor_name_idx).unwrap_or("");
        let monitor_string = record.get(monitor_string_idx).unwrap_or("");
        let left_top = record.get(left_top_idx).unwrap_or("").to_string();
        let right_bottom = record.get(right_bottom_idx).unwrap_or("");
        let (left, top) = parse_point(&left_top).unwrap_or((0, 0));
        let (right, bottom) = parse_point(right_bottom).unwrap_or((0, 0));
        // MolotovCherry/virtual-display-rs's PnP FriendlyName is "Generic Monitor (VirtuDisplay+)"
        // (confirmed via Get-PnpDevice -Class Monitor) — distinctive enough not to collide with any
        // real monitor's name/string. Deliberately NOT matching the old stock-VDD marker ("MTT")
        // anymore: Windows keeps stale PnP entries for uninstalled drivers around (status
        // "Unknown" rather than "OK"), so after switching drivers both markers can be present
        // simultaneously and only this one reflects a live device.
        let is_vdd = monitor_name.contains("VirtuDisplay+") || monitor_string.contains("VirtuDisplay+");
        monitors.push(Monitor { name, is_vdd, left_top, width: right - left, height: bottom - top });
    }
    tracing::info!(count = monitors.len(), vdd_count = monitors.iter().filter(|m| m.is_vdd).count(), "enumerated monitors");
    Ok(monitors)
}

fn header_index(headers: &csv::StringRecord, name: &str) -> Result<usize> {
    headers.iter().position(|h| h == name).ok_or_else(|| anyhow!("MultiMonitorTool CSV missing expected column {name:?}"))
}

/// Parses MultiMonitorTool's `"X, Y"`-style coordinate columns (e.g. `"1080, 303"`).
fn parse_point(s: &str) -> Option<(i32, i32)> {
    let (x, y) = s.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// How far below the target topology's bounding box (max `y + height` across all targets) to
/// push real monitors that aren't part of it. Deliberately **0** — flush against the bounding
/// box's bottom edge, not floating below it. A large margin was tried first and consistently
/// produced a completely different, deterministic scrambled layout for every monitor (not just
/// the pushed-away one) regardless of ordering, `Primary`, or added settle delays — the signature
/// of Windows' display-config validation rejecting a topology where a monitor is a fully
/// disconnected "island" (no edge touching any other monitor) and silently re-flowing everything
/// into some other valid (but unintended) connected arrangement instead. Sitting flush against
/// the bounding box keeps the whole desktop one connected region while still being far enough
/// out of the horizontal band the 3 target monitors occupy not to reintroduce the original
/// gap/overlap problem this push-away exists to solve.
const OUT_OF_THE_WAY_Y_MARGIN: i32 = 0;

/// Positions/resizes/rotates `assignments` (VDD monitor -> its target layout) *and* repositions
/// `others` (real, non-VDD monitors) far below the target bounding box, all in a **single**
/// `/SetMonitors` call. Confirmed necessary in practice: splitting this into a `/SetMonitors` call
/// for the VDD monitors followed by a separate `/EnableAtPosition` call for the real monitor
/// produced a completely different, deterministically-scrambled layout every single time,
/// unaffected by ordering, `Primary`, settle delays, or how far the real monitor was pushed —
/// `/SetMonitors` is documented (and its own generated-from-current-config workflow implies) as
/// operating on the *complete* monitor topology atomically; feeding it a subset while another
/// command repositions the rest isn't a coherent partial update, and Windows silently re-flows
/// everything into some other valid arrangement instead of honoring either command. Real monitors
/// only get `Name`/`PositionX`/`PositionY` in their spec (no `Width`/`Height`/etc.) since their
/// own resolution shouldn't be touched — the tool's own docs say unset fields are left alone.
pub fn apply(mmt_path: &Path, assignments: &[(&Monitor, &TargetMonitor)], others: &[&Monitor]) -> Result<()> {
    let bounding_bottom = assignments.iter().map(|(_, t)| t.y + t.height).max().unwrap_or(0);
    let out_of_the_way_y = bounding_bottom + OUT_OF_THE_WAY_Y_MARGIN;

    let mut args = vec!["/SetMonitors".to_string()];
    for (monitor, target) in assignments {
        // DisplayOrientation always 0 — target.width/height are already the final on-screen
        // dimensions (declared as a native mode, see topology.rs's doc comment for why this
        // replaced requesting a landscape mode + DisplayOrientation=1).
        //
        // The target sitting at (0,0) is explicitly marked Primary — confirmed necessary in
        // practice: Windows anchors its whole virtual-desktop coordinate space to whichever
        // monitor is Primary, and without this the *real* physical monitor (Primary from before
        // any of this ran) stayed Primary even after being pushed far away below the layout,
        // causing Windows to renormalize coordinates around it and silently shift every other
        // monitor's effective position away from what was actually requested.
        let primary = if target.x == 0 && target.y == 0 { " Primary=1" } else { "" };
        let spec = format!(
            "Name={} BitsPerPixel=32 Width={} Height={} DisplayFlags=0 DisplayFrequency=60 DisplayOrientation=0 PositionX={} PositionY={}{primary}",
            monitor.name, target.width, target.height, target.x, target.y
        );
        tracing::info!(label = target.label, spec, "positioning monitor");
        args.push(spec);
    }
    for monitor in others {
        let spec = format!("Name={} PositionX=0 PositionY={out_of_the_way_y}", monitor.name);
        tracing::info!(display = monitor.name, spec, "pushing non-VDD monitor out of the way (not disabling it)");
        args.push(spec);
    }

    let status = std::process::Command::new(mmt_path).args(&args).status().context("running MultiMonitorTool /SetMonitors")?;
    if !status.success() {
        anyhow::bail!("MultiMonitorTool /SetMonitors exited with {:?}", status.code());
    }
    // Per MultiMonitorTool's own readme: complex multi-monitor layouts sometimes need the
    // configuration applied more than once to fully take effect.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let status2 = std::process::Command::new(mmt_path).args(&args).status().context("re-applying MultiMonitorTool /SetMonitors")?;
    if !status2.success() {
        anyhow::bail!("MultiMonitorTool /SetMonitors (2nd pass) exited with {:?}", status2.code());
    }

    Ok(())
}
