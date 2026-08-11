//! Runtime encoder auto-detection and driving, per the Phase 0 decision: enumerate
//! available hardware encoder MFTs via `MFTEnumEx`, try NVENC -> Quick Sync -> AMF ->
//! software x264 in that priority order, and log whichever got selected. Once selected,
//! every backend is driven identically through the generic `IMFTransform` interface.

use anyhow::{anyhow, Context, Result};
use windows::core::GUID;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateSample,
    MFCreateMemoryBuffer, MFMediaType_Video, MFStartup, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::CoTaskMemFree;

const FPS: u32 = 60;
const BITRATE_BPS: u32 = 20_000_000;

pub struct EncoderChoice {
    pub name: String,
    activate: IMFActivate,
}

/// Enumerates H.264 encoder MFTs via `MFTEnumEx` and returns them sorted NVENC -> Quick Sync
/// -> AMF -> software, per the Phase 0 decision. Doesn't pick one yet — `open_best_available`
/// actually tries to activate each in order, since `MFTEnumEx` listing an MFT doesn't
/// guarantee it can successfully activate in the current session (e.g. a hardware encoder
/// MFT can fail `ActivateObject` if it can't get a context to its GPU from this session).
fn enumerate_encoders() -> Result<Vec<EncoderChoice>> {
    unsafe {
        MFStartup(windows::Win32::Media::MediaFoundation::MF_VERSION, MFSTARTUP_FULL)
            .context("MFStartup")?;
    }

    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let activates = unsafe {
        let mut list_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output_info),
            &mut list_ptr,
            &mut count,
        )
        .context("MFTEnumEx")?;
        let slice = std::slice::from_raw_parts(list_ptr, count as usize);
        let activates: Vec<IMFActivate> = slice.iter().filter_map(|a| a.clone()).collect();
        CoTaskMemFree(Some(list_ptr as *mut _));
        activates
    };

    if activates.is_empty() {
        return Err(anyhow!(
            "MFTEnumEx returned no H.264 encoder MFTs (not even the software fallback — is Media Foundation installed?)"
        ));
    }

    let mut named: Vec<EncoderChoice> = activates
        .into_iter()
        .filter_map(|a| friendly_name(&a).ok().map(|name| EncoderChoice { name, activate: a }))
        .collect();

    let priority = |name: &str| -> u8 {
        let n = name.to_lowercase();
        if n.contains("nvidia") || n.contains("nvenc") {
            0
        } else if n.contains("intel") || n.contains("quick sync") {
            1
        } else if n.contains("amd") || n.contains("amf") {
            2
        } else {
            3 // software fallback (e.g. Microsoft's built-in H.264 Encoder MFT)
        }
    };
    named.sort_by_key(|c| priority(&c.name));
    tracing::info!(candidates = ?named.iter().map(|c| &c.name).collect::<Vec<_>>(), "encoder MFTs found");

    Ok(named)
}

/// Tries each enumerated encoder in priority order and opens the first one that actually
/// activates and negotiates NV12->H264 media types successfully.
pub fn open_best_available(device: &ID3D11Device, width: u32, height: u32) -> Result<Encoder> {
    let candidates = enumerate_encoders()?;
    let mut last_err = None;
    for choice in candidates {
        let name = choice.name.clone();
        match choice.open(device, width, height) {
            Ok(encoder) => return Ok(encoder),
            Err(e) => {
                tracing::warn!(encoder = name, error = ?e, "encoder failed to open, trying next candidate");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no encoder candidates available")))
}

fn friendly_name(activate: &IMFActivate) -> Result<String> {
    unsafe {
        let mut pwstr = windows::core::PWSTR::null();
        let mut len = 0u32;
        activate.GetAllocatedString(
            &windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute,
            &mut pwstr,
            &mut len,
        )?;
        let name = pwstr.to_string().context("decoding MFT friendly name")?;
        CoTaskMemFree(Some(pwstr.0 as *mut _));
        Ok(name)
    }
}

impl EncoderChoice {
    pub fn open(self, _device: &ID3D11Device, width: u32, height: u32) -> Result<Encoder> {
        let transform: IMFTransform = unsafe { self.activate.ActivateObject().context("IMFActivate::ActivateObject")? };

        let input_type = make_media_type(MFVideoFormat_NV12, width, height)?;
        let output_type = make_media_type(MFVideoFormat_H264, width, height)?;
        unsafe {
            transform.SetOutputType(0, &output_type, 0).context("SetOutputType")?;
            transform.SetInputType(0, &input_type, 0).context("SetInputType")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("start of stream")?;
        }

        tracing::info!(encoder = self.name, width, height, fps = FPS, "encoder opened");
        Ok(Encoder {
            transform,
            frame_index: 0,
        })
    }
}

fn make_media_type(subtype: GUID, width: u32, height: u32) -> Result<IMFMediaType> {
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(FPS, 1))?;
        if subtype == MFVideoFormat_H264 {
            media_type.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_MT_AVG_BITRATE,
                BITRATE_BPS,
            )?;
            // Force a keyframe (+ SPS/PPS) at least every 2 seconds. Without this, the
            // encoder only emits its first IDR at startup — any client connecting later (or
            // any datagram loss) has no reference frame to decode against, which is exactly
            // the "video pipeline runs, decoder just shows black" symptom this fixes.
            media_type.SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_MT_MAX_KEYFRAME_SPACING,
                FPS * 2,
            )?;
        }
        Ok(media_type)
    }
}

fn pack_u64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

pub struct Encoder {
    transform: IMFTransform,
    frame_index: u64,
}

impl Encoder {
    /// Feeds one captured (BGRA8) frame in, converting to NV12, and returns encoded H.264
    /// Annex-B data if the encoder produced output for it (encoders commonly buffer a frame
    /// or two before emitting anything, so `Ok(None)` on early calls is expected).
    pub fn encode_frame(&mut self, frame: &crate::capture::CapturedFrame) -> Result<Option<Vec<u8>>> {
        let nv12 = bgra_to_nv12(frame);
        let sample = nv12_to_sample(&nv12, self.frame_index)?;
        self.frame_index += 1;

        unsafe {
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput")?;
        }

        self.pull_output()
    }

    fn pull_output(&mut self) -> Result<Option<Vec<u8>>> {
        unsafe {
            // Many encoder MFTs (this software H.264 one included) don't allocate their own
            // output samples — the caller has to pre-allocate a sample/buffer sized per
            // GetOutputStreamInfo and hand it in, or ProcessOutput fails with E_INVALIDARG.
            let stream_info = self
                .transform
                .GetOutputStreamInfo(0)
                .context("GetOutputStreamInfo")?;
            let provides_samples = (stream_info.dwFlags
                & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32)
                != 0;

            let output_buffer = if provides_samples {
                MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(None),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }
            } else {
                let size = stream_info.cbSize.max(1);
                let buffer = MFCreateMemoryBuffer(size).context("MFCreateMemoryBuffer (output)")?;
                let sample = MFCreateSample().context("MFCreateSample (output)")?;
                sample.AddBuffer(&buffer).context("AddBuffer (output)")?;
                MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }
            };

            let mut status = 0u32;
            let mut buffers = [output_buffer];
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let output_buffer = std::mem::replace(
                &mut buffers[0],
                MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(None),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                },
            );

            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => return Err(e).context("ProcessOutput"),
            }

            let Some(sample) = std::mem::ManuallyDrop::into_inner(output_buffer.pSample) else {
                return Ok(None);
            };
            let media_buffer = sample
                .ConvertToContiguousBuffer()
                .context("IMFSample::ConvertToContiguousBuffer")?;
            let mut data_ptr = std::ptr::null_mut();
            let mut data_len = 0u32;
            media_buffer
                .Lock(&mut data_ptr, None, Some(&mut data_len))
                .context("IMFMediaBuffer::Lock (output)")?;
            let data = std::slice::from_raw_parts(data_ptr, data_len as usize).to_vec();
            media_buffer.Unlock().context("IMFMediaBuffer::Unlock (output)")?;
            Ok(Some(data))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
    }
}

/// CPU BGRA8 -> NV12 (BT.709, limited range) conversion. Simple and correct, not the fastest
/// possible path — worth revisiting (GPU color-convert via a compute shader, or a Media
/// Foundation Video Processor MFT) if this shows up in the Phase 1 latency benchmark.
struct Nv12Frame {
    width: u32,
    height: u32,
    y_plane: Vec<u8>,
    uv_plane: Vec<u8>,
}

fn bgra_to_nv12(frame: &crate::capture::CapturedFrame) -> Nv12Frame {
    let width = frame.width;
    let height = frame.height;
    let row_bytes = (width * 4) as usize;
    let mut y_plane = vec![0u8; (width * height) as usize];
    let mut uv_plane = vec![0u8; (width * height / 2) as usize];

    for y in 0..height as usize {
        let row = &frame.data[y * row_bytes..(y + 1) * row_bytes];
        for x in 0..width as usize {
            let b = row[x * 4] as f32;
            let g = row[x * 4 + 1] as f32;
            let r = row[x * 4 + 2] as f32;

            let yv = 16.0 + (0.183 * r + 0.614 * g + 0.062 * b);
            y_plane[y * width as usize + x] = yv.clamp(0.0, 255.0) as u8;

            if y % 2 == 0 && x % 2 == 0 {
                let u = 128.0 + (-0.101 * r - 0.339 * g + 0.439 * b);
                let v = 128.0 + (0.439 * r - 0.399 * g - 0.040 * b);
                let uv_index = (y / 2) * width as usize + x;
                uv_plane[uv_index] = u.clamp(0.0, 255.0) as u8;
                uv_plane[uv_index + 1] = v.clamp(0.0, 255.0) as u8;
            }
        }
    }

    Nv12Frame {
        width,
        height,
        y_plane,
        uv_plane,
    }
}

fn nv12_to_sample(frame: &Nv12Frame, frame_index: u64) -> Result<IMFSample> {
    unsafe {
        let len = frame.y_plane.len() + frame.uv_plane.len();
        let buffer = MFCreateMemoryBuffer(len as u32).context("MFCreateMemoryBuffer")?;
        let mut data_ptr = std::ptr::null_mut();
        buffer
            .Lock(&mut data_ptr, None, None)
            .context("IMFMediaBuffer::Lock")?;
        std::ptr::copy_nonoverlapping(frame.y_plane.as_ptr(), data_ptr, frame.y_plane.len());
        std::ptr::copy_nonoverlapping(
            frame.uv_plane.as_ptr(),
            data_ptr.add(frame.y_plane.len()),
            frame.uv_plane.len(),
        );
        buffer
            .SetCurrentLength(len as u32)
            .context("IMFMediaBuffer::SetCurrentLength")?;
        buffer.Unlock().context("IMFMediaBuffer::Unlock")?;

        let sample = MFCreateSample().context("MFCreateSample")?;
        sample.AddBuffer(&buffer).context("IMFSample::AddBuffer")?;
        let frame_duration = 10_000_000u64 / FPS as u64; // 100ns units
        sample
            .SetSampleTime((frame_index * frame_duration) as i64)
            .context("IMFSample::SetSampleTime")?;
        sample
            .SetSampleDuration(frame_duration as i64)
            .context("IMFSample::SetSampleDuration")?;
        let _ = frame.width;
        let _ = frame.height;
        Ok(sample)
    }
}
