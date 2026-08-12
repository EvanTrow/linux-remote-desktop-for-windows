//! GameStream/Sunshine session setup: the HTTPS calls a client makes *after* pairing
//! (`pairing.rs`) and *before* `LiStartConnection()` — enumerating apps and launching one to get
//! back an RTSP session URL. Like pairing, `moonlight-common-c` doesn't implement this (it starts
//! at the RTSP handshake); every Moonlight client does this HTTP dance itself, ported here from
//! moonlight-qt's `nvhttp.cpp`.
//!
//! **Not yet live-tested against `/applist` or `/launch`** — `pairing.rs`'s handshake and
//! `/serverinfo` are confirmed working against cwtrow's real Sunshine instance, but this part
//! hasn't been exercised yet.

use crate::pairing::{hex_encode, https_client_with_cert, xml_ok, xml_tag, ClientIdentity, Ports};
use anyhow::{anyhow, bail, Result};

pub struct App {
    pub id: String,
    pub title: String,
}

pub struct SessionInfo {
    pub rtsp_session_url: String,
    pub server_codec_mode_support: i32,
    pub app_version: String,
    pub gfe_version: String,
}

/// Same channel mapping as `AUDIO_CONFIGURATION_STEREO` in `Limelight.h`
/// (`MAKE_AUDIO_CONFIGURATION(2, 0x3)`) — bindgen doesn't evaluate function-like macros, so this
/// is computed by hand rather than pulled from `moonlight_sys`.
const AUDIO_CONFIGURATION_STEREO: i32 = (0x3 << 16) | (2 << 8) | 0xCA;

/// Extracts every `<App>...</App>` block from `/applist`'s response. Reuses `pairing::xml_tag`
/// per block since each block is itself flat (no nesting below `<App>`).
fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut blocks = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(&close) else { break };
        blocks.push(&after_open[..end]);
        rest = &after_open[end + close.len()..];
    }
    blocks
}

/// Queries the host's app list and picks one to launch — prefers an app named "desktop"
/// (Sunshine ships this by default for exactly this no-specific-game use case), falling back to
/// the first app if none matches.
pub async fn pick_app(identity: &ClientIdentity, host: &str, base_port: u16) -> Result<App> {
    let https = Ports::from_base(base_port).https;
    let client = https_client_with_cert(identity)?;
    let url = format!("https://{host}:{https}/applist?uniqueid={}", identity.unique_id);
    let resp = client.get(&url).send().await?.text().await?;
    if !xml_ok(&resp) {
        bail!("applist request failed: {resp}");
    }

    let apps: Vec<App> = xml_blocks(&resp, "App")
        .into_iter()
        .filter_map(|block| {
            let id = xml_tag(block, "ID")?.to_string();
            let title = xml_tag(block, "AppTitle").unwrap_or("").to_string();
            Some(App { id, title })
        })
        .collect();

    if apps.is_empty() {
        bail!("host reported no apps at all — nothing to launch");
    }
    let chosen = apps
        .iter()
        .position(|a| a.title.eq_ignore_ascii_case("desktop"))
        .unwrap_or(0);
    let app = apps.into_iter().nth(chosen).unwrap();
    tracing::info!(app_id = %app.id, app_title = %app.title, "selected app to launch");
    Ok(app)
}

/// Starts (or resumes) a streaming session for `app`, returning the RTSP session URL
/// `LiStartConnection()` needs. `ri_key`/`ri_key_id` must be the exact same bytes later placed in
/// `STREAM_CONFIGURATION.remoteInputAesKey`/`Iv` — the host binds the input-encryption key to
/// this specific launch request.
#[allow(clippy::too_many_arguments)]
pub async fn launch(
    identity: &ClientIdentity,
    host: &str,
    base_port: u16,
    app: &App,
    width: i32,
    height: i32,
    fps: i32,
    ri_key: &[u8; 16],
    ri_key_id: i32,
) -> Result<SessionInfo> {
    let https = Ports::from_base(base_port).https;
    let client = https_client_with_cert(identity)?;

    // Extra query params Sunshine hosts look for (HDR toggles, etc.) — moonlight-common-c owns
    // this string so it stays in sync with whatever the library actually negotiates.
    let extra = unsafe {
        let ptr = moonlight_sys::LiGetLaunchUrlQueryParameters();
        if ptr.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };

    let rikey_hex = hex_encode(ri_key);
    let url = format!(
        "https://{host}:{https}/launch?uniqueid={uid}&appid={appid}&mode={width}x{height}x{fps}\
         &additionalStates=1&sops=0&rikey={rikey_hex}&rikeyid={ri_key_id}&localAudioPlayMode=0\
         &surroundAudioInfo={audio}&remoteControllersBitmap=0&gcmap=0&gcpersist=0{extra}",
        uid = identity.unique_id,
        appid = app.id,
        audio = AUDIO_CONFIGURATION_STEREO,
    );
    let mut resp = client.get(&url).send().await?.text().await?;
    if !xml_ok(&resp) {
        if resp.contains("already running") {
            // A prior session (e.g. this same client killed uncleanly mid-stream, without
            // reaching LiStopConnection) left the host's app session active. GameStream's
            // /resume reattaches to it instead of starting a new one — same idea as /launch but
            // without appid/mode/sops, since it resumes whatever's already running as-is.
            tracing::warn!("an app was already running on the host; resuming that session instead of launching a new one");
            let resume_url = format!(
                "https://{host}:{https}/resume?uniqueid={uid}&rikey={rikey_hex}&rikeyid={ri_key_id}\
                 &surroundAudioInfo={audio}{extra}",
                uid = identity.unique_id,
                audio = AUDIO_CONFIGURATION_STEREO,
            );
            resp = client.get(&resume_url).send().await?.text().await?;
            if !xml_ok(&resp) {
                bail!("resume request failed after launch reported an app already running: {resp}");
            }
        } else {
            bail!("launch request failed: {resp}");
        }
    }
    let rtsp_session_url = xml_tag(&resp, "sessionUrl0")
        .ok_or_else(|| anyhow!("no sessionUrl0 in launch response: {resp}"))?
        .to_string();

    let info_url = format!("https://{host}:{https}/serverinfo?uniqueid={}", identity.unique_id);
    let info_resp = client.get(&info_url).send().await?.text().await?;
    let server_codec_mode_support = xml_tag(&info_resp, "ServerCodecModeSupport")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let app_version = xml_tag(&info_resp, "appversion").unwrap_or("").to_string();
    let gfe_version = xml_tag(&info_resp, "GfeVersion").unwrap_or("").to_string();

    tracing::info!(rtsp_session_url, server_codec_mode_support, "session launched");
    Ok(SessionInfo {
        rtsp_session_url,
        server_codec_mode_support,
        app_version,
        gfe_version,
    })
}
