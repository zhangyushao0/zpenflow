//! Wave-2 gate: Media Foundation HEVC on-demand IDR probe.
//!
//! Reference: design.md §6.4.1, HANDOFF §3.3.
//!
//! Sunshine notes that FFmpeg's `hevc_mf` wrapper sets `FIXED_GOP_SIZE` because
//! FFmpeg cannot do on-demand IDR through MF. Microsoft documents
//! `CODECAPI_AVEncVideoForceKeyFrame` (`ULONG`/`VT_UI4`, set `ulVal = 1`) as
//! supported on the H.264/HEVC encoder MFTs. This probe confirms or denies that
//! claim on the active hardware MFT (likely NVIDIA on the dev rig).
//!
//! Run: `cargo run -p penflow-core --example mf_idr_probe`
//! Exit code 0 = PASS, 1 = FAIL (no IDR after force), 2 = setup error.

use std::mem::ManuallyDrop;
use std::process::ExitCode;
use std::ptr;

use windows::core::{Interface, Result, GUID, PWSTR};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HMODULE, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter1, IDXGIFactory6, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 60;
const BITRATE: u32 = 5_000_000;
const TOTAL_FRAMES: usize = 30;
const FORCE_IDR_FRAME: usize = 10;

fn main() -> ExitCode {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            eprintln!("[setup-fail] CoInitializeEx: {e:?}");
            return ExitCode::from(2);
        }
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            eprintln!("[setup-fail] MFStartup: {e:?}");
            CoUninitialize();
            return ExitCode::from(2);
        }
    }

    let code = match run_probe() {
        Ok(true) => {
            println!();
            println!("=== VERDICT: PASS ===");
            println!("MF on-demand IDR via CODECAPI_AVEncVideoForceKeyFrame works on this MFT.");
            println!("Design §6.4.1 holds; build the engine on the as-written shape.");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!();
            println!("=== VERDICT: FAIL ===");
            println!("Force-key-frame request was IGNORED. Fall back to periodic IDR + on-connect");
            println!("encoder reset (see design.md §6.4.1 fallback paragraph).");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("[probe-error] {e:?}");
            ExitCode::from(2)
        }
    };

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    code
}

fn run_probe() -> Result<bool> {
    let (device, adapter_vendor_id) = create_d3d11_device_on_high_perf_adapter()?;
    let dev_mgr = create_dxgi_device_manager(&device)?;
    let transform = pick_compatible_hevc_hardware_mft(&dev_mgr, adapter_vendor_id)?;

    let codec_api: ICodecAPI = transform.cast()?;
    apply_codec_settings(&codec_api)?;

    unsafe {
        transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }

    let event_gen: IMFMediaEventGenerator = transform.cast()?;
    let nv12 = make_black_nv12(WIDTH, HEIGHT);

    let mut frames_in: usize = 0;
    let mut frames_out: usize = 0;
    let mut nal_per_frame: Vec<Vec<u8>> = Vec::new();
    let stream_id: u32 = 0;
    let mut drained_eos = false;
    let mut total_bytes: usize = 0;

    while frames_out < TOTAL_FRAMES {
        let event = unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NONE)? };
        let etype = unsafe { event.GetType()? };

        let me_need_input = METransformNeedInput.0 as u32;
        let me_have_output = METransformHaveOutput.0 as u32;
        let me_drain = METransformDrainComplete.0 as u32;

        if etype == me_need_input {
            if frames_in < TOTAL_FRAMES {
                if frames_in == FORCE_IDR_FRAME {
                    set_codec_ui4(&codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1)?;
                    println!("[probe] before submitting frame {frames_in}: requested force-IDR");
                }
                let sample = make_nv12_sample(&nv12, frames_in as i64)?;
                unsafe { transform.ProcessInput(stream_id, &sample, 0)? };
                frames_in += 1;
                if frames_in == TOTAL_FRAMES && !drained_eos {
                    unsafe {
                        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                        transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                    }
                    drained_eos = true;
                }
            }
        } else if etype == me_have_output {
            let info = unsafe { transform.GetOutputStreamInfo(stream_id)? };
            let provides = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

            let mut out_buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: stream_id,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            if !provides {
                let sample = unsafe { MFCreateSample()? };
                let needed = info.cbSize.max(WIDTH * HEIGHT * 2);
                let mb = unsafe { MFCreateMemoryBuffer(needed)? };
                unsafe { sample.AddBuffer(&mb)? };
                out_buf.pSample = ManuallyDrop::new(Some(sample));
            }

            let mut status: u32 = 0;
            let r = unsafe {
                transform.ProcessOutput(0, std::slice::from_mut(&mut out_buf), &mut status)
            };

            // Always reclaim ownership so refcounts don't leak.
            let opt_sample = unsafe { ManuallyDrop::take(&mut out_buf.pSample) };
            let _ = unsafe { ManuallyDrop::take(&mut out_buf.pEvents) };

            r?;
            let sample = opt_sample.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let bytes = read_sample_bytes(&sample)?;
            total_bytes += bytes.len();
            let nal_types = parse_hevc_nal_types(&bytes);
            println!(
                "[frame {frames_out:>2}] {:>5} B  NALs = {:?} {}",
                bytes.len(),
                nal_types,
                describe_nal_types(&nal_types)
            );
            nal_per_frame.push(nal_types);
            frames_out += 1;
        } else if etype == me_drain {
            // Drain finished; remaining HaveOutput events should still arrive before this.
        } else {
            // Ignore other events (METransformInputStreamStateChanged etc.)
        }
    }

    unsafe {
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)?;
    }
    println!();
    println!("[summary] {frames_out} frames decoded, {total_bytes} bytes total");

    let force_nals = nal_per_frame
        .get(FORCE_IDR_FRAME)
        .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    Ok(force_nals.iter().any(|&t| t == 19 || t == 20))
}

fn create_d3d11_device_on_high_perf_adapter() -> Result<(ID3D11Device, u32)> {
    let factory: IDXGIFactory6 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))? };
    let adapter: IDXGIAdapter1 =
        unsafe { factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)? };
    let desc = unsafe { adapter.GetDesc1()? };
    let name: String = String::from_utf16_lossy(&desc.Description)
        .trim_end_matches('\0')
        .to_string();
    println!(
        "[adapter] {name} (vendor 0x{:04x}, device 0x{:04x})",
        desc.VendorId, desc.DeviceId
    );

    let mut device: Option<ID3D11Device> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut got_level = D3D_FEATURE_LEVEL::default();
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut got_level),
            None,
        )?;
    }
    let device = device.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    println!("[d3d11] feature level 0x{:04x}", got_level.0 as u32);

    let mt: ID3D11Multithread = device.cast()?;
    let _ = unsafe { mt.SetMultithreadProtected(true) };
    Ok((device, desc.VendorId))
}

fn create_dxgi_device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let mut token: u32 = 0;
    let mut mgr: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut mgr)? };
    let mgr = mgr.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    unsafe { mgr.ResetDevice(device, token)? };
    Ok(mgr)
}

/// Walk the MFT list and pick the first one that:
///   1. Reports a vendor ID matching the active D3D11 adapter (preferred), OR
///   2. Successfully binds the D3D11 device manager + accepts the HEVC output type
///      (fallback for MFTs that don't advertise a vendor ID).
///
/// MFTEnumEx returns vendor MFTs system-wide regardless of which GPUs are
/// physically present, so on an NVIDIA-only box we still see AMD's MFT first
/// in the merit-sorted list. Activating the wrong-vendor MFT against an
/// NVIDIA D3D11 device fails with E_OUTOFMEMORY at SET_D3D_MANAGER /
/// SetOutputType time. This is the design's live-probe pattern (Sunshine
/// `probe_encoders`, design.md §6.3) — driver capability strings lie.
fn pick_compatible_hevc_hardware_mft(
    dev_mgr: &IMFDXGIDeviceManager,
    adapter_vendor_id: u32,
) -> Result<IMFTransform> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    let mut activate_arr: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count: u32 = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output_info),
            &mut activate_arr,
            &mut count,
        )?;
    }
    if count == 0 {
        eprintln!("[mft] no hardware HEVC encoder MFTs found on this system");
        return Err(windows::core::Error::from(E_FAIL));
    }
    println!("[mft] {count} hardware HEVC encoder(s) enumerated by MFTEnumEx");

    let raw = unsafe { std::slice::from_raw_parts_mut(activate_arr, count as usize) };

    // Take ownership of all activates first so we can iterate freely.
    let mut activates: Vec<IMFActivate> = Vec::with_capacity(count as usize);
    for slot in raw.iter_mut() {
        if let Some(a) = slot.take() {
            activates.push(a);
        }
    }
    unsafe { CoTaskMemFree(Some(activate_arr as *const _)) };

    // Print every candidate for diagnostic value.
    for (i, a) in activates.iter().enumerate() {
        let name = read_friendly_name(a).unwrap_or_else(|_| "<unnamed>".into());
        let vendor = read_mft_vendor_id(a).unwrap_or_else(|| "<no vendor attr>".into());
        println!("  [mft#{i}] {name}  vendor={vendor}");
    }

    let target_vendor_tag = format!("VEN_{:04X}", adapter_vendor_id);

    // Pass 1: prefer vendor-ID match.
    for (i, a) in activates.iter().enumerate() {
        if let Some(v) = read_mft_vendor_id(a) {
            if v.eq_ignore_ascii_case(&target_vendor_tag) {
                println!(
                    "[mft] candidate #{i} matches adapter vendor ({target_vendor_tag}); trying it"
                );
                if let Some(t) = try_activate_and_configure(a, dev_mgr) {
                    return Ok(t);
                }
                println!("[mft] vendor-matched candidate #{i} failed live probe; continuing");
            }
        }
    }

    // Pass 2: try every candidate in enumerated order until one binds successfully.
    for (i, a) in activates.iter().enumerate() {
        println!("[mft] live-probing candidate #{i}");
        if let Some(t) = try_activate_and_configure(a, dev_mgr) {
            return Ok(t);
        }
    }

    eprintln!("[mft] no enumerated HEVC MFT could bind to this D3D11 device");
    Err(windows::core::Error::from(E_FAIL))
}

/// Activates the MFT, runs full `configure_transform` against it, and returns
/// the transform if all setup succeeds. Returns `None` (with a log line) on
/// failure so the caller can move to the next candidate.
fn try_activate_and_configure(
    activate: &IMFActivate,
    dev_mgr: &IMFDXGIDeviceManager,
) -> Option<IMFTransform> {
    let transform: IMFTransform = match unsafe { activate.ActivateObject() } {
        Ok(t) => t,
        Err(e) => {
            println!("    activate failed: {e:?}");
            return None;
        }
    };
    if let Err(e) = configure_transform(&transform, dev_mgr) {
        println!("    configure failed: {e:?}");
        return None;
    }
    let name = read_friendly_name(activate).unwrap_or_else(|_| "<unnamed>".into());
    println!("[mft] selected: {name}");
    Some(transform)
}

fn read_mft_vendor_id(activate: &IMFActivate) -> Option<String> {
    let mut p: PWSTR = PWSTR(ptr::null_mut());
    let mut len: u32 = 0;
    unsafe {
        activate
            .GetAllocatedString(&MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, &mut p, &mut len)
            .ok()?;
    }
    if p.0.is_null() {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(p.0, len as usize) };
    let s = String::from_utf16_lossy(slice);
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    Some(s)
}

fn read_friendly_name(activate: &IMFActivate) -> Result<String> {
    let mut p: PWSTR = PWSTR(ptr::null_mut());
    let mut len: u32 = 0;
    unsafe {
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len)?;
    }
    if p.0.is_null() {
        return Err(windows::core::Error::from(E_FAIL));
    }
    let slice = unsafe { std::slice::from_raw_parts(p.0, len as usize) };
    let s = String::from_utf16_lossy(slice);
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    Ok(s)
}

fn configure_transform(transform: &IMFTransform, dev_mgr: &IMFDXGIDeviceManager) -> Result<()> {
    // 1. UNLOCK the async MFT BEFORE calling anything else on it. Without this,
    //    SET_D3D_MANAGER and SetOutputType return MF_E_TRANSFORM_ASYNC_LOCKED
    //    (HRESULT 0xC00D6D77).
    let attrs = unsafe { transform.GetAttributes()? };
    let _ = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
    let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };

    // 2. Bind D3D11 device manager so the MFT can use GPU-side input.
    unsafe {
        transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, dev_mgr.as_raw() as usize)?;
    }

    // OUTPUT type first (HEVC). Required attrs per Microsoft docs:
    //   MAJOR_TYPE, SUBTYPE, AVG_BITRATE, FRAME_RATE, FRAME_SIZE,
    //   INTERLACE_MODE, MPEG2_PROFILE, PIXEL_ASPECT_RATIO.
    // Color-space attributes belong on the INPUT type (the encoder writes
    // VUI bytes into the bitstream from input metadata; they are rejected
    // on the OUTPUT type by some encoders).
    let out_type: IMFMediaType = unsafe { MFCreateMediaType()? };
    unsafe {
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
        out_type.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE)?;
        out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_2u32(WIDTH, HEIGHT))?;
        out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_2u32(FPS, 1))?;
        out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_2u32(1, 1))?;
        out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
        transform.SetOutputType(0, &out_type, 0)?;
    }

    // INPUT type (NV12). Color-space attrs go here so the encoder writes the
    // matching VUI metadata into the SPS.
    let in_type: IMFMediaType = unsafe { MFCreateMediaType()? };
    unsafe {
        in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_2u32(WIDTH, HEIGHT))?;
        in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_2u32(FPS, 1))?;
        in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_2u32(1, 1))?;
        in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        let _ = in_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_0_255.0 as u32);
        let _ = in_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = in_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        let _ = in_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        transform.SetInputType(0, &in_type, 0)?;
    }
    Ok(())
}

fn apply_codec_settings(codec: &ICodecAPI) -> Result<()> {
    set_codec_ui4(
        codec,
        &CODECAPI_AVEncCommonRateControlMode,
        eAVEncCommonRateControlMode_CBR.0 as u32,
    )?;
    set_codec_ui4(codec, &CODECAPI_AVEncCommonMeanBitRate, BITRATE)?;
    let _ = set_codec_bool(codec, &CODECAPI_AVLowLatencyMode, true);
    let _ = set_codec_ui4(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
    // Long GOP so periodic IDRs don't hide the on-demand request.
    let _ = set_codec_ui4(codec, &CODECAPI_AVEncMPVGOPSize, 600);
    Ok(())
}

fn set_codec_ui4(codec: &ICodecAPI, key: &GUID, value: u32) -> Result<()> {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
        codec.SetValue(key, &var)
    }
}

fn set_codec_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> Result<()> {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if value {
            VARIANT_TRUE
        } else {
            windows::Win32::Foundation::VARIANT_FALSE
        };
        codec.SetValue(key, &var)
    }
}

fn pack_2u32(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

fn make_black_nv12(w: u32, h: u32) -> Vec<u8> {
    let y_size = (w * h) as usize;
    let uv_size = y_size / 2;
    let mut buf = vec![0u8; y_size + uv_size];
    for b in buf[y_size..].iter_mut() {
        *b = 128; // chroma neutral
    }
    buf
}

fn make_nv12_sample(data: &[u8], frame_index: i64) -> Result<IMFSample> {
    let buf: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(data.len() as u32)? };
    unsafe {
        let mut p: *mut u8 = ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        buf.Lock(&mut p, Some(&mut max_len), Some(&mut cur_len))?;
        ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buf.SetCurrentLength(data.len() as u32)?;
        buf.Unlock()?;
    }
    let sample: IMFSample = unsafe { MFCreateSample()? };
    unsafe {
        sample.AddBuffer(&buf)?;
        let pts_100ns = frame_index * 10_000_000 / FPS as i64;
        let dur_100ns = 10_000_000 / FPS as i64;
        sample.SetSampleTime(pts_100ns)?;
        sample.SetSampleDuration(dur_100ns)?;
    }
    Ok(sample)
}

fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    let buf = unsafe { sample.ConvertToContiguousBuffer()? };
    let mut p: *mut u8 = ptr::null_mut();
    let mut max_len: u32 = 0;
    let mut cur_len: u32 = 0;
    let bytes = unsafe {
        buf.Lock(&mut p, Some(&mut max_len), Some(&mut cur_len))?;
        let v = std::slice::from_raw_parts(p, cur_len as usize).to_vec();
        buf.Unlock()?;
        v
    };
    Ok(bytes)
}

/// HEVC Annex-B NAL parser. Returns the `nal_unit_type` of every NAL we find.
fn parse_hevc_nal_types(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        let off = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            i + 4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            i + 3
        } else {
            i += 1;
            continue;
        };
        if off >= bytes.len() {
            break;
        }
        let nal_type = (bytes[off] >> 1) & 0x3F;
        out.push(nal_type);
        i = off + 2;
    }
    out
}

fn describe_nal_types(nals: &[u8]) -> String {
    let parts: Vec<&str> = nals
        .iter()
        .map(|t| match t {
            0 => "TRAIL_N",
            1 => "TRAIL_R",
            19 => "IDR_W_RADL",
            20 => "IDR_N_LP",
            21 => "CRA",
            32 => "VPS",
            33 => "SPS",
            34 => "PPS",
            35 => "AUD",
            36 => "EOS",
            37 => "EOB",
            38 => "FD",
            39 | 40 => "PREFIX_SEI",
            _ => "?",
        })
        .collect();
    format!("({})", parts.join(", "))
}

#[allow(dead_code)]
fn unused_close(h: windows::Win32::Foundation::HANDLE) {
    let _ = unsafe { CloseHandle(h) };
}
