//! Discovers Sunshine's own view of available displays (it logs a JSON array with each
//! display's `device_id` — the UUID `output_name` actually needs, confirmed against real
//! `sunshine.log` output; this isn't documented anywhere, `output_name`'s docs only say
//! "device ID in UUID format" without saying where to find it for a specific display) and
//! generates/launches per-monitor Sunshine instances.

use crate::topology::TargetMonitor;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

/// Windows process creation flag: no console window is created/inherited for the child, but
/// unlike `DETACHED_PROCESS` (tried first) it does *not* touch window-station/desktop
/// inheritance. That distinction mattered here: `DETACHED_PROCESS` made the spawned instances
/// crash immediately with `ERROR_ACCESS_DENIED` querying display paths (confirmed by reproducing
/// the identical error running Sunshine manually — DirectX/DXGI display enumeration needs a real
/// desktop handle, which `DETACHED_PROCESS` was evidently taking away too, not just the console).
/// `CREATE_NO_WINDOW` gets the same "survive `rdhost.exe`'s console closing" outcome without that
/// side effect.
const CREATE_NO_WINDOW: u32 = 0x08000000;

const DEFAULT_SUNSHINE_EXE: &str = r"C:\Program Files\Sunshine\sunshine.exe";
const DEFAULT_LOG_PATH: &str = r"C:\Program Files\Sunshine\config\sunshine.log";
const DEFAULT_CONF_PATH: &str = r"C:\Program Files\Sunshine\config\sunshine.conf";

/// cwtrow's LAN IP — matches the hardcoded value `main.rs`'s summary printout already uses for
/// web UI URLs. Sunshine's own CSRF protection only auto-allows localhost-style origins by
/// default; browsing to a non-default instance's web UI from this LAN IP (as opposed to
/// localhost) gets rejected with "The request was blocked by CSRF protection" unless that exact
/// origin is added via `csrf_allowed_origins` — confirmed the hard way trying to PIN-pair a
/// non-default instance from another machine on the LAN.
const HOST_LAN_IP: &str = "192.168.1.55";

#[derive(Deserialize, Debug)]
struct DisplayEntry {
    device_id: String,
    info: DisplayEntryInfo,
}

#[derive(Deserialize, Debug)]
struct DisplayEntryInfo {
    resolution: Resolution,
}

#[derive(Deserialize, Debug)]
struct Resolution {
    width: i32,
    height: i32,
}

/// Restarts the already-installed Sunshine Windows service (which owns port 47989 — the same
/// port `target_topology()`'s first entry intentionally uses, so that monitor reuses this
/// existing service rather than us launching a redundant duplicate process on the same port)
/// and reads back the freshest "Currently available display devices" block from its log, which
/// now reflects the just-repositioned VDD monitors.
pub fn discover_displays() -> Result<Vec<(String, i32, i32)>> {
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "Restart-Service -Name SunshineService -Force"])
        .status()
        .context("restarting SunshineService to refresh its display enumeration")?;
    if !status.success() {
        anyhow::bail!("Restart-Service SunshineService failed (exit code {:?})", status.code());
    }

    // Sunshine's startup — including its own display-enumeration pass, which writes the block
    // we're about to look for — doesn't finish instantly after the service reports "running";
    // a single fixed 3s wait was found to sometimes lose the race (only a marker from *before*
    // this restart is present yet, or the file hasn't been reopened for writing at all yet).
    // Poll for a marker that's newer than this restart instead of trusting one fixed delay.
    let restart_time = std::time::Instant::now();
    let marker = "Currently available display devices:";
    let log = loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Ok(log) = std::fs::read_to_string(DEFAULT_LOG_PATH) {
            if log.rfind(marker).is_some() {
                break log;
            }
        }
        if restart_time.elapsed() > std::time::Duration::from_secs(20) {
            anyhow::bail!("no display enumeration found in sunshine.log after waiting 20s for SunshineService to restart");
        }
    };

    let start_of_marker = log.rfind(marker).ok_or_else(|| anyhow!("no display enumeration found in sunshine.log"))?;
    let json_start = log[start_of_marker..].find('[').ok_or_else(|| anyhow!("no JSON array after display enumeration marker"))? + start_of_marker;

    // The JSON is pretty-printed across many lines; find the matching closing bracket by
    // tracking nesting depth rather than assuming a fixed line count.
    let mut depth = 0i32;
    let mut json_end = None;
    for (i, c) in log[json_start..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    json_end = Some(json_start + i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let json_end = json_end.ok_or_else(|| anyhow!("unterminated JSON array in sunshine.log"))?;
    let json_str = &log[json_start..json_end];

    let entries: Vec<DisplayEntry> = serde_json::from_str(json_str).context("parsing display enumeration JSON")?;
    let displays = entries.into_iter().map(|e| (e.device_id, e.info.resolution.width, e.info.resolution.height)).collect::<Vec<_>>();
    tracing::info!(count = displays.len(), "discovered displays from Sunshine's own enumeration");
    Ok(displays)
}

/// Matches `target` to a discovered display by resolution — safe as long as every target
/// resolution in this run's topology is unique, which `target_topology()` guarantees today.
pub fn find_device_id<'a>(displays: &'a [(String, i32, i32)], target: &TargetMonitor) -> Result<&'a str> {
    displays
        .iter()
        .find(|(_, w, h)| *w == target.width && *h == target.height)
        .map(|(id, _, _)| id.as_str())
        .ok_or_else(|| anyhow!("no discovered display matches {}x{} for {}", target.width, target.height, target.label))
}

/// Directory this host app keeps per-instance Sunshine state in, for instances beyond the
/// already-installed default one (which keeps using its own install-directory config).
pub fn instances_dir() -> PathBuf {
    PathBuf::from(r"C:\Users\evan\dev\rdw\sunshine-instances")
}

/// For the target that reuses the default installed service (port == 47989): patches its
/// existing `sunshine.conf` in place with the correct `output_name`, leaving everything else
/// (credentials, pairing state, all other settings) untouched.
pub fn update_default_instance_output_name(device_id: &str) -> Result<()> {
    let existing = std::fs::read_to_string(DEFAULT_CONF_PATH).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().filter(|l| !l.starts_with("output_name")).map(str::to_string).collect();
    lines.push(format!("output_name = {device_id}"));
    std::fs::write(DEFAULT_CONF_PATH, lines.join("\n") + "\n").context("updating default sunshine.conf's output_name")?;

    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "Restart-Service -Name SunshineService -Force"])
        .status()
        .context("restarting SunshineService to apply output_name")?;
    if !status.success() {
        anyhow::bail!("Restart-Service SunshineService failed (exit code {:?})", status.code());
    }
    tracing::info!(device_id, "updated default Sunshine instance's output_name");
    Ok(())
}

/// Writes a fresh config + launches a new Sunshine instance for one non-default target monitor.
/// Not registered as a Windows service — it lives only as long as this process's session, same
/// as this whole tool needing an interactive session to run at all today. Proper auto-start
/// service registration is Phase 5 ("host as a Windows service/startup app") work, not done yet.
pub fn launch_instance(target: &TargetMonitor, device_id: &str, sunshine_exe: &Path) -> Result<std::process::Child> {
    let dir = instances_dir().join(target.label);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating instance directory for {}", target.label))?;
    let conf_path = dir.join("sunshine.conf");
    // file_state/credentials_file/log_path all redirected to this instance's own directory —
    // otherwise every instance defaults to the same paths under Sunshine's install directory,
    // colliding with both the default instance and each other (pairing state in particular:
    // each instance is paired with independently, via its own PIN, so sharing credentials_file
    // across instances would mean one instance's pairing state clobbers another's). Everything
    // *not* set here (assets, shaders, etc.) intentionally falls back to Sunshine's own defaults,
    // which only resolve correctly with cwd left at Sunshine's install directory — see below.
    let web_ui_port = target.sunshine_port + 1;
    let conf = format!(
        "port = {}\noutput_name = {}\nfile_state = {state}\ncredentials_file = {creds}\nlog_path = {log}\ncsrf_allowed_origins = https://{HOST_LAN_IP}:{web_ui_port}\n",
        target.sunshine_port,
        device_id,
        state = dir.join("sunshine_state.json").display(),
        creds = dir.join("credentials.json").display(),
        log = dir.join("sunshine.log").display(),
    );
    std::fs::write(&conf_path, conf).context("writing instance sunshine.conf")?;

    // With CREATE_NO_WINDOW there's no console for Sunshine's own stdout/stderr writes to land
    // on — redirect both to a file explicitly, otherwise a startup crash (which has happened —
    // see PLAN.md) is completely invisible: no console output, and (before file_state/log_path
    // above were instance-specific) Sunshine didn't even get far enough to open its own
    // configured log file.
    let stdout_log = std::fs::File::create(dir.join("stdout.log")).context("creating instance stdout.log")?;
    let stderr_log = std::fs::File::create(dir.join("stderr.log")).context("creating instance stderr.log")?;

    // cwd is Sunshine's *install* directory, not this instance's directory — confirmed necessary
    // in practice: Sunshine resolves its assets (shaders, app icons, ...) via paths relative to
    // cwd, not relative to its own exe location, so running with cwd = an arbitrary instance
    // directory broke shader compilation entirely ("Couldn't compile
    // assets/shaders/directx/... [0x80070003]" → "Platform failed to initialize"). The
    // conf-driven redirects above are what keep this instance's *state* from colliding with the
    // default instance's, now that cwd is shared between them.
    let sunshine_dir = sunshine_exe.parent().context("sunshine_exe has no parent directory")?;

    tracing::info!(label = target.label, port = target.sunshine_port, device_id, path = %conf_path.display(), "launching Sunshine instance");
    let child = std::process::Command::new(sunshine_exe)
        .arg(&conf_path)
        .current_dir(sunshine_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::from(stdout_log))
        .stderr(std::process::Stdio::from(stderr_log))
        .spawn()
        .with_context(|| format!("launching Sunshine instance for {}", target.label))?;
    Ok(child)
}

pub fn default_sunshine_exe() -> PathBuf {
    PathBuf::from(DEFAULT_SUNSHINE_EXE)
}

/// Stops the extra (non-default) Sunshine instances `launch_instance` started — identified by
/// their command line referencing `instances_dir()`, so this only ever touches processes this
/// tool itself launched, never the SunshineService-managed default instance (which has its own
/// install-directory config and isn't a plain `sunshine.exe <conf_path>` invocation at all).
pub fn stop_extra_instances() -> Result<()> {
    let script = format!(
        r#"
Get-CimInstance Win32_Process -Filter "Name = 'sunshine.exe'" |
    Where-Object {{ $_.CommandLine -like '*{}*' }} |
    ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}
"#,
        instances_dir().display()
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .context("running powershell to stop extra Sunshine instances")?;
    if !status.success() {
        anyhow::bail!("stopping extra Sunshine instances failed (exit code {:?})", status.code());
    }
    tracing::info!("stopped extra Sunshine instances");
    Ok(())
}
