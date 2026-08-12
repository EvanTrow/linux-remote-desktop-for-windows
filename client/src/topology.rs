//! Mirrors `host/src/topology.rs`'s target layout — see that file's doc comment for the
//! "hardcoded for now" rationale (dynamic topology negotiation over the wire never got built).
//! Duplicated here by hand rather than shared via a workspace crate, since `host/` and `client/`
//! are separate Cargo projects that build and run on different OSes/machines. Keep both in sync
//! if the client's physical monitor layout ever changes.

use anyhow::{Context, Result};

#[derive(Clone, Copy)]
pub struct TargetMonitor {
    pub label: &'static str,
    pub width: i32,
    pub height: i32,
    pub sunshine_port: u16,
}

pub fn target_topology() -> Vec<TargetMonitor> {
    vec![
        TargetMonitor { label: "hdmi1-portrait", width: 1080, height: 1920, sunshine_port: 47989 },
        TargetMonitor { label: "dp3-ultrawide", width: 5120, height: 1440, sunshine_port: 48989 },
        TargetMonitor { label: "dp2", width: 2560, height: 1440, sunshine_port: 49989 },
    ]
}

pub struct LocalOutput {
    /// e.g. "HDMI-1" — passed to `input_surface.rs`'s `--output` for fullscreen placement.
    pub name: String,
    pub width: i32,
    pub height: i32,
}

/// Runs `xrandr --query` and parses every connected output's name and effective (post-rotation)
/// resolution — the same command/values `topology.rs`'s targets were originally captured from by
/// hand, now done live so a cable reshuffle doesn't silently desync client and host.
pub fn detect_local_outputs() -> Result<Vec<LocalOutput>> {
    let output = std::process::Command::new("xrandr").arg("--query").output().context("running xrandr --query")?;
    if !output.status.success() {
        anyhow::bail!("xrandr --query exited with {:?}", output.status.code());
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let mut outputs = Vec::new();
    for line in text.lines() {
        if !line.contains(" connected") {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(name) = tokens.next() else { continue };
        // e.g. "1080x1920+0+0" — the first token containing both 'x' and '+', which is always
        // the geometry field regardless of whether "primary" appears before it.
        let Some(geometry) = tokens.find(|t| t.contains('x') && t.contains('+')) else { continue };
        let Some((size, _rest)) = geometry.split_once('+') else { continue };
        let Some((w, h)) = size.split_once('x') else { continue };
        let (Ok(width), Ok(height)) = (w.parse(), h.parse()) else { continue };
        outputs.push(LocalOutput { name: name.to_string(), width, height });
    }
    Ok(outputs)
}

/// Matches each target to a currently-connected local output by resolution — the same strategy
/// `host/src/mmt.rs` and `sunshine.rs` use on the host side, for the same reason: it's a more
/// stable correlator than any name/enumeration-order signal. Every target's resolution is
/// distinct (guaranteed by `target_topology()`), so this is unambiguous as long as the client's
/// physical layout actually matches the host's assumed topology.
pub fn match_targets_to_outputs(targets: &[TargetMonitor], outputs: &[LocalOutput]) -> Result<Vec<(TargetMonitor, String)>> {
    let mut matched = Vec::new();
    for target in targets {
        let output = outputs
            .iter()
            .find(|o| o.width == target.width && o.height == target.height)
            .with_context(|| format!("no connected output matches {}x{} (target {})", target.width, target.height, target.label))?;
        matched.push((*target, output.name.clone()));
    }
    Ok(matched)
}
