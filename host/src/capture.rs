//! Screen capture via the Desktop Duplication API (IDXGIOutputDuplication), per the Phase 0
//! decision (DDA over Windows.Graphics.Capture — see PLAN.md). Captures one display, feeds
//! frames into the selected encoder, and sends encoded output as QUIC datagrams.

use crate::encode;
use anyhow::{anyhow, Context, Result};
use rdproto::VideoDatagramHeader;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_OUTDUPL_FRAME_INFO,
};

/// One-time diagnostic: this system has multiple GPUs (Intel + AMD, per the encoder MFTs
/// found), and `D3D11CreateDevice`'s default adapter isn't necessarily the one actually
/// driving the KVM-connected display. Logs every adapter/output so we can see which one is
/// real before committing to "adapter 0, output 0".
fn log_all_adapters_and_outputs() {
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return;
        };
        let mut i = 0;
        loop {
            let Ok(adapter) = factory.EnumAdapters(i) else {
                break;
            };
            if let Ok(desc) = adapter.GetDesc() {
                let name = String::from_utf16_lossy(&desc.Description)
                    .trim_end_matches('\0')
                    .to_string();
                tracing::info!(adapter = i, name, "DXGI adapter");
            }
            let mut j = 0;
            loop {
                let Ok(output) = adapter.EnumOutputs(j) else {
                    break;
                };
                if let Ok(desc) = output.GetDesc() {
                    let name = String::from_utf16_lossy(&desc.DeviceName)
                        .trim_end_matches('\0')
                        .to_string();
                    let r = desc.DesktopCoordinates;
                    tracing::info!(
                        adapter = i,
                        output = j,
                        name,
                        attached = desc.AttachedToDesktop.as_bool(),
                        left = r.left,
                        top = r.top,
                        right = r.right,
                        bottom = r.bottom,
                        "DXGI output"
                    );
                }
                j += 1;
            }
            i += 1;
        }
    }
}

pub async fn run(connection: quinn::Connection) -> Result<()> {
    // Desktop Duplication + Media Foundation both use blocking COM calls, so the capture
    // loop runs on a dedicated OS thread rather than as an async task.
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let capture_thread = std::thread::spawn(move || capture_and_encode_loop(frame_tx));

    let mut frame_id: u64 = 0;
    while let Some(encoded) = frame_rx.recv().await {
        frame_id += 1;
        send_encoded_frame(&connection, frame_id, &encoded).await?;
    }

    capture_thread
        .join()
        .map_err(|_| anyhow!("capture thread panicked"))??;
    Ok(())
}

async fn send_encoded_frame(connection: &quinn::Connection, frame_id: u64, data: &[u8]) -> Result<()> {
    let max_payload = rdproto::MAX_DATAGRAM_PAYLOAD;
    let fragment_count = data.len().div_ceil(max_payload).max(1) as u16;
    for (i, chunk) in data.chunks(max_payload).enumerate() {
        let header = VideoDatagramHeader {
            monitor_id: 0,
            frame_id,
            fragment_index: i as u16,
            fragment_count,
            keyframe: false, // TODO: plumb real keyframe flag through from the encoder.
        };
        let mut datagram = rdproto::encode_video_header(&header)?;
        datagram.extend_from_slice(chunk);
        connection.send_datagram(datagram.into())?;
    }
    Ok(())
}

/// Runs on a dedicated thread: owns the D3D11 device, DDA duplication session, and the
/// selected Media Foundation encoder MFT. Sends encoded Annex-B/AVCC packets to `frame_tx`.
fn capture_and_encode_loop(frame_tx: tokio::sync::mpsc::Sender<Vec<u8>>) -> Result<()> {
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .context("CoInitializeEx")?;
    }

    log_all_adapters_and_outputs();

    let (device, context) = create_d3d11_device()?;
    let (duplication, width, height) = create_output_duplication(&device)?;
    tracing::info!(width, height, "capturing display");
    let mut encoder = encode::open_best_available(&device, width, height)?;

    let mut acquired: u64 = 0;
    let mut encoded_ok: u64 = 0;
    let mut encoded_none: u64 = 0;
    let mut last_heartbeat = std::time::Instant::now();

    loop {
        if last_heartbeat.elapsed() > Duration::from_secs(5) {
            tracing::info!(acquired, encoded_ok, encoded_none, "capture loop heartbeat");
            last_heartbeat = std::time::Instant::now();
        }

        let frame = match acquire_frame(&duplication, &device, &context) {
            Ok(Some(texture)) => {
                acquired += 1;
                texture
            }
            Ok(None) => continue, // timeout, no new frame yet (DDA: screen hasn't changed)
            Err(e) => {
                tracing::warn!("frame acquisition failed, resetting duplication: {e:?}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        match encoder.encode_frame(&frame) {
            Ok(Some(encoded)) => {
                encoded_ok += 1;
                if frame_tx.blocking_send(encoded).is_err() {
                    return Ok(()); // receiver dropped, connection closed
                }
            }
            Ok(None) => encoded_none += 1, // encoder buffering, no output yet
            Err(e) => tracing::warn!("encode failed for this frame: {e:?}"),
        }
    }
}

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
    }
    Ok((device.unwrap(), context.unwrap()))
}

/// Returns the duplication session plus the actual desktop resolution — the encoder's media
/// types must match this exactly (a hardcoded resolution here previously caused corrupted/
/// black decoded output when it didn't match the real display, e.g. 1024x768 instead of the
/// assumed 1920x1080; see Phase 1 findings in PLAN.md).
fn create_output_duplication(device: &ID3D11Device) -> Result<(IDXGIOutputDuplication, u32, u32)> {
    unsafe {
        let dxgi_device: IDXGIDevice = device.cast().context("ID3D11Device -> IDXGIDevice cast")?;
        let adapter: IDXGIAdapter = dxgi_device.GetAdapter().context("IDXGIDevice::GetAdapter")?;
        // Phase 1 MVP: always output 0 (first display on the adapter). Multi-monitor
        // enumeration matching client topology is Phase 2.
        let output: IDXGIOutput = adapter
            .EnumOutputs(0)
            .context("IDXGIAdapter::EnumOutputs(0) — no display attached to this adapter from this session?")?;

        let output_desc = output.GetDesc().context("IDXGIOutput::GetDesc")?;
        let rect = output_desc.DesktopCoordinates;
        let width = (rect.right - rect.left) as u32;
        let height = (rect.bottom - rect.top) as u32;

        let output1: IDXGIOutput1 = output.cast().context("IDXGIOutput -> IDXGIOutput1 cast")?;
        let duplication = output1
            .DuplicateOutput(device)
            .context("DuplicateOutput (is another process already duplicating this output?)")?;
        Ok((duplication, width, height))
    }
}

/// Captured frame already read back to system memory (tightly packed BGRA8, `width * 4`
/// bytes per row) — read back and released before returning, so nothing here still
/// references the DDA-owned resource.
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

fn acquire_frame(
    duplication: &IDXGIOutputDuplication,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
) -> Result<Option<CapturedFrame>> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };

    unsafe {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let result = duplication.AcquireNextFrame(16, &mut frame_info, &mut resource);
        match result {
            Ok(()) => {}
            Err(e) if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT => {
                return Ok(None);
            }
            Err(e) => return Err(anyhow::Error::from(e).context("AcquireNextFrame")),
        }
        // Everything from here must happen before ReleaseFrame() — including the actual CPU
        // read via Map, not just enqueuing a GPU copy. ReleaseFrame() lets DDA reclaim the
        // source texture; a queued-but-not-yet-executed copy (even after Flush(), which only
        // submits work, it doesn't wait for completion) can lose the race and read back as an
        // all-zero frame every time, which is exactly what was happening before this fix.
        let result = (|| -> Result<CapturedFrame> {
            let resource = resource.context("AcquireNextFrame returned no resource")?;
            let source_texture: ID3D11Texture2D =
                resource.cast().context("IDXGIResource -> ID3D11Texture2D cast")?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            source_texture.GetDesc(&mut desc);
            if desc.Format != windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM {
                return Err(anyhow!(
                    "captured texture format is {:?}, not B8G8R8A8_UNORM (HDR/Advanced Color enabled?)",
                    desc.Format
                ));
            }

            let mut staging_desc = desc;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;

            let mut staging = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .context("CreateTexture2D (staging)")?;
            let staging = staging.context("CreateTexture2D returned no texture")?;
            context.CopyResource(&staging, &source_texture);

            let mut mapped = Default::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("ID3D11DeviceContext::Map (staging) — this is the real sync point")?;

            let width = desc.Width;
            let height = desc.Height;
            let row_bytes = (width * 4) as usize;
            let mut data = vec![0u8; row_bytes * height as usize];
            let src = mapped.pData as *const u8;
            let stride = mapped.RowPitch as usize;
            for y in 0..height as usize {
                let row = std::slice::from_raw_parts(src.add(y * stride), row_bytes);
                data[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row);
            }
            context.Unmap(&staging, 0);

            Ok(CapturedFrame { width, height, data })
        })();

        duplication.ReleaseFrame()?;
        result.map(Some)
    }
}
