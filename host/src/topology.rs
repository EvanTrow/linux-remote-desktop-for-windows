//! The target virtual-monitor layout this host should present, matching the Linux client's real
//! physical arrangement (captured via `xrandr` on the client, 2026-08-12):
//!
//! ```text
//! HDMI-1 (portrait, 1080x1920 @ 0,0) | DP-3 (ultrawide, 5120x1440 @ 1080,303) | DP-2 (2560x1440 @ 6200,303)
//! ```
//!
//! Hardcoded for now — PLAN.md's original design called for the client sending this over the
//! wire at connect time (dynamic topology negotiation), which never got built before the
//! Sunshine/Moonlight pivot retired the protocol that would have carried it. Revisit if the
//! client's physical layout ever changes, or when dynamic negotiation becomes worth building.

pub struct TargetMonitor {
    /// Human-readable label, used in logs and as this instance's config directory name.
    pub label: &'static str,
    /// Final, on-screen resolution — declared to VDD as a native mode (see `vdd.rs`) and
    /// requested from MultiMonitorTool with `DisplayOrientation=0` always, deliberately *not*
    /// declaring a landscape mode and rotating it at the Windows/MultiMonitorTool layer.
    ///
    /// That was the original design (1920x1080 + `DisplayOrientation=1` for the portrait
    /// monitor) and it visually worked, but every layout reverted to all-1920x1080 a few seconds
    /// after being applied, repeatedly, regardless of how aggressively it was re-applied — the
    /// symptom (reverting specifically to the *first* resolution in `vdd_settings.xml`'s shared
    /// resolution list, not to whatever was there before) pointed at VDD's own driver
    /// periodically reasserting a default mode from that list, not a Windows-level "keep these
    /// display settings?" timeout. Declaring the already-rotated 1080x1920 as its own native mode
    /// sidesteps the rotation path entirely, so there's nothing for that default-reassertion to
    /// fight.
    pub width: i32,
    pub height: i32,
    /// Position within the combined virtual desktop, matching the client's real layout.
    pub x: i32,
    pub y: i32,
    /// Sunshine's `port` config value for this instance — offsets its whole port family
    /// (web UI, HTTP, HTTPS, RTSP, video/control/audio) together. Spaced 1000 apart to safely
    /// clear Sunshine's actual port-family span (roughly 47984-48010) with margin.
    pub sunshine_port: u16,
}

pub fn target_topology() -> Vec<TargetMonitor> {
    vec![
        TargetMonitor {
            label: "hdmi1-portrait",
            width: 1080,
            height: 1920,
            x: 0,
            y: 0,
            sunshine_port: 47989, // Sunshine's own default — matches the already-installed instance
        },
        TargetMonitor {
            label: "dp3-ultrawide",
            width: 5120,
            height: 1440,
            x: 1080,
            y: 303,
            sunshine_port: 48989,
        },
        TargetMonitor {
            label: "dp2",
            width: 2560,
            height: 1440,
            x: 6200,
            y: 303,
            sunshine_port: 49989,
        },
    ]
}
