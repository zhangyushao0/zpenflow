//! Windows Media Foundation HEVC encoder backend.
//!
//! Production port of `crates/penflow-core/examples/mf_idr_probe.rs` with two
//! key changes:
//!   1. Zero-copy D3D11 input via `MFCreateDXGISurfaceBuffer` (probe used
//!      `MFCreateMemoryBuffer` because it generated NV12 in CPU memory).
//!   2. Async event loop driven from `submit_frame` / `try_packet` rather
//!      than a single inline drain loop.
//!
//! All five gate-1 findings (HANDOFF §4.5) are baked in:
//!   - vendor-ID MFT match against the device's adapter,
//!   - `MF_TRANSFORM_ASYNC_UNLOCK = 1` BEFORE any other call,
//!   - `MF_MT_MPEG2_PROFILE = Main_420_8` on the OUTPUT type,
//!   - colour-space attributes on the INPUT type (encoder writes VUI),
//!   - on-demand IDR via `CODECAPI_AVEncVideoForceKeyFrame` (`VT_UI4` / 1).

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::d3d11::D3d11Context;
use crate::error::{EngineError, EngineResult};

use super::{Codec, EncodeSession, EncodedPacket, EncoderBackend, PixelFormat, SessionConfig};

/// One global initialiser — `MFStartup` is refcounted internally but we
/// still want to call it once and `MFShutdown` once on process exit.
static MF_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn ensure_mf_started() -> EngineResult<()> {
    if MF_INITIALIZED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
    Ok(())
}

/// Media Foundation HEVC encoder backend. Stateless — sessions hold their own
/// MFT and codec API.
pub struct MfBackend;

impl MfBackend {
    pub fn new() -> EngineResult<Self> {
        ensure_mf_started()?;
        Ok(Self)
    }
}

impl EncoderBackend for MfBackend {
    fn name(&self) -> &'static str {
        "mf"
    }
    fn supported_codecs(&self) -> &[Codec] {
        &[Codec::Hevc]
    }
    fn supported_input_formats(&self) -> &[PixelFormat] {
        &[PixelFormat::Nv12]
    }

    fn make_session(
        &self,
        ctx: &D3d11Context,
        cfg: SessionConfig,
    ) -> EngineResult<Box<dyn EncodeSession>> {
        if cfg.codec != Codec::Hevc {
            return Err(EngineError::NoCompatibleEncoder);
        }
        if cfg.input_format != PixelFormat::Nv12 {
            return Err(EngineError::NoCompatibleEncoder);
        }
        let session = MfSession::new(ctx, cfg)?;
        Ok(Box::new(session))
    }
}

pub struct MfSession {
    cfg: SessionConfig,
    transform: IMFTransform,
    codec_api: ICodecAPI,
    event_gen: IMFMediaEventGenerator,
    /// Held so the device manager outlives the MFT.
    _dev_mgr: IMFDXGIDeviceManager,
    /// Filled with VPS+SPS+PPS bytes when we observe the first IDR.
    sequence_header: Vec<u8>,
    /// Cached `METransformNeedInput` credit. We pre-fetch the next credit
    /// at the END of `submit_frame` (post-`ProcessInput`) so the same
    /// blocking `GetEvent` call also drains *this* frame's HaveOutput
    /// promptly. Without this, `wait_for_need_input` only ran at the
    /// START of the next submit — meaning a frame's HaveOutput sat in
    /// the MFT event queue for a full pipeline tick (~8 ms), inflating
    /// `encode_us` and delaying the wire send by the same amount.
    /// True iff a credit is available without another GetEvent call.
    have_need_input_credit: bool,
    /// Internal output queue — packets we drained from `METransformHaveOutput`
    /// events but the caller hasn't claimed yet.
    output_queue: VecDeque<EncodedPacket>,
    /// FIFO of `(pts_ns, submit_instant)` per submitted input frame, used to
    /// stamp emitted packets in order. MF preserves order (no B-frames;
    /// zero-reorder MFT) so the i-th submit corresponds to the i-th
    /// output.
    ///
    /// Why a FIFO and not a single-slot `Option`: `submit_frame` blocks on
    /// `wait_for_need_input`, which drains *previous* frames'
    /// `HaveOutput` events into `output_queue` BEFORE we touch the meta
    /// slot. With a single slot, the sequence is:
    ///   1. submit(N).wait_for_need_input → drains N-1's output
    ///   2. submit(N).ProcessInput
    ///   3. submit(N) sets meta = (pts_N, instant_N)
    ///   4. try_packet pops N-1's packet, takes N's meta → encode_us
    ///      ≈ 0 µs (the meta was set ~1 µs ago) and the PTS is wrong.
    /// A FIFO eliminates this skew: each submit appends, each pop
    /// consumes the head, so packet ↔ meta line up correctly.
    pending_input_meta: VecDeque<(i64, Instant)>,
}

const STREAM_ID: u32 = 0;

impl MfSession {
    fn new(ctx: &D3d11Context, cfg: SessionConfig) -> EngineResult<Self> {
        ensure_mf_started()?;

        // 1. Pick the right MFT for our adapter (gate-1 vendor-ID match).
        let activate = pick_mft_for_adapter(ctx.adapter_vendor_id)?;
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };

        // 2. Async-unlock BEFORE anything else (gate-1 finding).
        let attrs = unsafe { transform.GetAttributes()? };
        let _ = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
        let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };

        // 3. Bind the D3D11 device manager so MF can take DXGI samples.
        let dev_mgr = create_dev_mgr(ctx)?;
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, dev_mgr.as_raw() as usize)?;
        }

        // 4. Output type (HEVC) — must include MF_MT_MPEG2_PROFILE.
        let out_type = unsafe { MFCreateMediaType()? };
        configure_hevc_output_type(&out_type, &cfg)?;
        unsafe { transform.SetOutputType(0, &out_type, 0)? };

        // 5. Input type (NV12) — colour-space attrs on this side.
        let in_type = unsafe { MFCreateMediaType()? };
        configure_nv12_input_type(&in_type, &cfg)?;
        unsafe { transform.SetInputType(0, &in_type, 0)? };

        // 6. Codec API: rate control + bitrate + low-latency mode.
        let codec_api: ICodecAPI = transform.cast()?;
        set_codec_ui4(
            &codec_api,
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
        )?;
        set_codec_ui4(&codec_api, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate_bps)?;
        let _ = set_codec_bool(&codec_api, &CODECAPI_AVLowLatencyMode, true);
        // `AVEncCommonRealTime` is independent of `AVLowLatencyMode` — it
        // tells the MFT "this is a real-time stream, prefer latency over
        // quality on every internal trade-off". On NVIDIA's HEVC MFT (and
        // confirmed by multiple NVIDIA dev-forum reports) it shaves 1-3 ms
        // off the encode tail by disabling rate-control look-ahead and any
        // remaining buffer-headroom scheduling. Best-effort: failure on
        // backends that don't expose the property is fine.
        let _ = set_codec_bool(&codec_api, &CODECAPI_AVEncCommonRealTime, true);
        let _ = set_codec_ui4(&codec_api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        // Long GOP — we drive IDR on demand (gate-1 PASS). If a backend
        // ignores ForceKeyFrame, the design §6.4.1 fallback is to set this to
        // a small value (fps × 2) for periodic IDR; that lives outside the
        // session for now since the dev rig confirmed on-demand works.
        let _ = set_codec_ui4(&codec_api, &CODECAPI_AVEncMPVGOPSize, 600);

        // 7. Begin streaming.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        let event_gen: IMFMediaEventGenerator = transform.cast()?;
        Ok(Self {
            cfg,
            transform,
            codec_api,
            event_gen,
            _dev_mgr: dev_mgr,
            sequence_header: Vec::new(),
            output_queue: VecDeque::new(),
            pending_input_meta: VecDeque::new(),
            have_need_input_credit: false,
        })
    }

    /// Block on `GetEvent` until we receive a `METransformNeedInput`
    /// (returning `Ok(())`), queueing any `METransformHaveOutput` events we
    /// encounter along the way. Used by `submit_frame` to obtain permission
    /// to call `ProcessInput`.
    fn wait_for_need_input(&mut self) -> EngineResult<()> {
        loop {
            let event = unsafe { self.event_gen.GetEvent(MF_EVENT_FLAG_NONE)? };
            let etype = unsafe { event.GetType()? };
            if etype == METransformNeedInput.0 as u32 {
                return Ok(());
            }
            if etype == METransformHaveOutput.0 as u32 {
                if let Some(pkt) = self.collect_output_packet()? {
                    self.output_queue.push_back(pkt);
                }
            }
            // Other events (stream-state etc.) ignored.
        }
    }

    /// Drain whatever events are immediately available (no waiting). Used by
    /// `try_packet` to pick up any `HaveOutput` events that arrived between
    /// pipeline ticks.
    fn drain_events_nowait(&mut self) -> EngineResult<()> {
        loop {
            let event = match unsafe { self.event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(EngineError::Win32(e)),
            };
            let etype = unsafe { event.GetType()? };
            if etype == METransformHaveOutput.0 as u32 {
                if let Some(pkt) = self.collect_output_packet()? {
                    self.output_queue.push_back(pkt);
                }
            }
            // METransformNeedInput credits arriving here would just be wasted
            // (we'd need a sample to submit). Ignored — submit_frame will
            // block-wait for the next one.
        }
    }

    fn collect_output_packet(&mut self) -> EngineResult<Option<EncodedPacket>> {
        let info = unsafe { self.transform.GetOutputStreamInfo(STREAM_ID)? };
        let provides = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

        let mut out_buf = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: STREAM_ID,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        if !provides {
            let sample = unsafe { MFCreateSample()? };
            let needed = info.cbSize.max(self.cfg.width * self.cfg.height * 2);
            let mb = unsafe { MFCreateMemoryBuffer(needed)? };
            unsafe { sample.AddBuffer(&mb)? };
            out_buf.pSample = ManuallyDrop::new(Some(sample));
        }

        let mut status: u32 = 0;
        let r = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut out_buf), &mut status)
        };
        let opt_sample = unsafe { ManuallyDrop::take(&mut out_buf.pSample) };
        let _ = unsafe { ManuallyDrop::take(&mut out_buf.pEvents) };
        r?;
        let sample = match opt_sample {
            Some(s) => s,
            None => return Ok(None),
        };
        // Stamp the encoder-finished moment HERE — `ProcessOutput` returned
        // a sample so the MFT just emitted it. If we deferred until the
        // pipeline's next `try_packet` tick, encode_us would also include
        // however long the pipeline slept between ticks (e.g. 200 ms when
        // DDA timed out on a static desktop), turning the metric into
        // "submit → we noticed" instead of "submit → encoder done".
        let finished_at = Instant::now();

        let bytes = read_sample_bytes(&sample)?;
        // Match MF's preserved 1:1 input/output ordering: the head of the
        // meta FIFO corresponds to this output. Use the meta's pts_ns
        // (caller-supplied, exact ns) over `sample.GetSampleTime()`'s
        // 100-ns rounded value.
        let (pts_ns, encode_us) = match self.pending_input_meta.pop_front() {
            Some((pts, submit_instant)) => {
                let us = finished_at
                    .saturating_duration_since(submit_instant)
                    .as_micros();
                (pts, Some(us.min(u32::MAX as u128) as u32))
            }
            None => {
                // Fall back to the MFT's own PTS (e.g. after a hot reset
                // that drained outputs without paired meta).
                let pts = unsafe { sample.GetSampleTime() }.map(|t| t * 100).unwrap_or(0);
                (pts, None)
            }
        };

        let is_keyframe = first_nal_is_idr(&bytes);
        // First IDR carries VPS+SPS+PPS; cache them for the protocol layer.
        if is_keyframe && self.sequence_header.is_empty() {
            self.sequence_header = bytes.clone();
        }
        Ok(Some(EncodedPacket {
            bytes,
            pts_ns,
            is_keyframe,
            encode_us,
        }))
    }
}

// SAFETY: MfSession owns its MF objects exclusively (one thread at a time —
// the capture-encode pipeline). The D3D11 device behind the dev-manager has
// SetMultithreadProtected(true), so MF's worker threads serialise device
// access. Send (transfer of ownership across threads) is safe; we deliberately
// don't impl Sync because we never share &MfSession across threads.
unsafe impl Send for MfSession {}

impl EncodeSession for MfSession {
    fn input_format(&self) -> PixelFormat {
        PixelFormat::Nv12
    }

    fn submit_frame(
        &mut self,
        tex: &ID3D11Texture2D,
        pts_ns: i64,
        force_idr: bool,
    ) -> EngineResult<()> {
        // 1. Acquire a NeedInput credit. Usually pre-fetched by the
        //    previous submit's post-drain step, in which case we're free
        //    to ProcessInput immediately. On the very first call (or
        //    after a hot restart) we have to block.
        if !self.have_need_input_credit {
            self.wait_for_need_input()?;
        }
        self.have_need_input_credit = false;

        // 2. Force IDR before ProcessInput if requested. Failure is
        //    non-fatal — design §6.4.1 fallback handles backends that
        //    ignore the property.
        if force_idr {
            let _ = set_codec_ui4(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1);
        }

        // 3. Wrap the D3D11 texture as an IMFSample (zero-copy via
        //    MFCreateDXGISurfaceBuffer). The IID is ID3D11Texture2D.
        let buffer: IMFMediaBuffer = unsafe {
            MFCreateDXGISurfaceBuffer(
                &<ID3D11Texture2D as Interface>::IID,
                tex,
                0,
                false,
            )?
        };
        let sample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_ns / 100)?; // ns → 100-ns units
            let dur_100ns = if self.cfg.fps > 0 {
                10_000_000 / self.cfg.fps as i64
            } else {
                166_667
            };
            sample.SetSampleDuration(dur_100ns)?;
        }

        unsafe { self.transform.ProcessInput(STREAM_ID, &sample, 0)? };
        // Anchor *after* ProcessInput returns so encode_us measures only
        // the encoder's wall-clock work, not the caller's pre-submit
        // texture wrap / SetSampleTime overhead.
        self.pending_input_meta
            .push_back((pts_ns, Instant::now()));

        // 4. Pre-fetch the next NeedInput credit. NVENC P1 ULL with
        //    `MF_LOW_LATENCY=1` has a shallow pipeline that emits
        //    HaveOutput for THIS frame BEFORE signalling readiness for
        //    the next input — so this blocking GetEvent loop drains the
        //    just-submitted frame's output along the way and stamps
        //    `encode_us` accurately at the actual MFT-emit moment. If
        //    we deferred this drain to the next pipeline tick (which
        //    might be 8 ms away when DDA is timing out on a static
        //    desktop), encode_us and the wire-send latency would both
        //    inflate by that gap.
        self.wait_for_need_input()?;
        self.have_need_input_credit = true;

        Ok(())
    }

    fn try_packet(&mut self) -> EngineResult<Option<EncodedPacket>> {
        // pts_ns and encode_us were stamped in `collect_output_packet` at
        // the moment `ProcessOutput` returned a sample, so no metadata
        // work to do here — just hand the next packet to the caller.
        self.drain_events_nowait()?;
        Ok(self.output_queue.pop_front())
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

// ----------------- helpers (mirror the gate-1 probe) -----------------

fn pick_mft_for_adapter(adapter_vendor_id: u32) -> EngineResult<IMFActivate> {
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
        return Err(EngineError::NoCompatibleEncoder);
    }
    let raw = unsafe { std::slice::from_raw_parts_mut(activate_arr, count as usize) };
    let mut activates: Vec<IMFActivate> = Vec::with_capacity(count as usize);
    for slot in raw.iter_mut() {
        if let Some(a) = slot.take() {
            activates.push(a);
        }
    }
    unsafe { CoTaskMemFree(Some(activate_arr as *const _)) };

    let target_tag = format!("VEN_{:04X}", adapter_vendor_id);
    for a in &activates {
        if let Some(v) = read_mft_vendor_id(a) {
            if v.eq_ignore_ascii_case(&target_tag) {
                return Ok(a.clone());
            }
        }
    }
    // Fallback: take the first MFT and let the live-probe at SetOutputType
    // surface the failure if it's wrong-vendor.
    activates.into_iter().next().ok_or(EngineError::NoCompatibleEncoder)
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

fn create_dev_mgr(ctx: &D3d11Context) -> EngineResult<IMFDXGIDeviceManager> {
    let mut token: u32 = 0;
    let mut mgr: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut mgr)? };
    let mgr = mgr.ok_or(EngineError::NotInitialized)?;
    unsafe { mgr.ResetDevice(&ctx.device, token)? };
    Ok(mgr)
}

fn configure_hevc_output_type(t: &IMFMediaType, cfg: &SessionConfig) -> EngineResult<()> {
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_bps)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_2u32(cfg.width, cfg.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_2u32(cfg.fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_2u32(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // Required by NVIDIA's HEVC MFT — gate-1 finding.
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
    }
    Ok(())
}

fn configure_nv12_input_type(t: &IMFMediaType, cfg: &SessionConfig) -> EngineResult<()> {
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_2u32(cfg.width, cfg.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_2u32(cfg.fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_2u32(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_0_255.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    }
    Ok(())
}

fn set_codec_ui4(codec: &ICodecAPI, key: &GUID, value: u32) -> EngineResult<()> {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
        codec.SetValue(key, &var)?;
    }
    Ok(())
}

fn set_codec_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> EngineResult<()> {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if value {
            VARIANT_TRUE
        } else {
            windows::Win32::Foundation::VARIANT_FALSE
        };
        codec.SetValue(key, &var)?;
    }
    Ok(())
}

fn pack_2u32(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

fn read_sample_bytes(sample: &IMFSample) -> EngineResult<Vec<u8>> {
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

fn first_nal_is_idr(bytes: &[u8]) -> bool {
    // Walk Annex-B start codes, classify the first VCL NAL we see. VCL types
    // for HEVC are 0..=31; IDR_W_RADL=19 / IDR_N_LP=20 / CRA=21.
    let mut i = 0;
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
        if nal_type < 32 {
            return matches!(nal_type, 19 | 20 | 21);
        }
        // Non-VCL; keep scanning.
        i = off + 2;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{create_bgra_keepalive_texture, ColorConverter};

    /// End-to-end smoke: build an MfBackend session and submit a few black
    /// frames, expect at least one IDR packet back. Mirrors the gate-1 probe
    /// but goes through the production trait so we catch trait-shape bugs.
    #[test]
    fn session_emits_keyframe() {
        let backend = MfBackend::new().expect("MfBackend");
        let ctx = D3d11Context::create_high_perf().expect("d3d11 ctx");
        let cfg = SessionConfig {
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_bps: 5_000_000,
            codec: Codec::Hevc,
            input_format: PixelFormat::Nv12,
        };
        let mut session = backend.make_session(&ctx, cfg).expect("session");

        let conv = ColorConverter::new(&ctx, cfg.width, cfg.height, cfg.fps).expect("conv");
        let bgra = create_bgra_keepalive_texture(&ctx.device, cfg.width, cfg.height).expect("bgra");

        let mut got_idr = false;
        let mut packets = 0usize;
        for i in 0..30 {
            conv.convert(&bgra).expect("convert");
            let force_idr = i == 5;
            session
                .submit_frame(conv.output_texture(), i as i64 * 16_666_667, force_idr)
                .expect("submit");
            // Drain whatever's ready so we don't backlog.
            while let Some(pkt) = session.try_packet().expect("try_packet") {
                packets += 1;
                if pkt.is_keyframe {
                    got_idr = true;
                }
            }
        }
        // Allow extra polling cycles so trailing frames flush.
        for _ in 0..30 {
            while let Some(pkt) = session.try_packet().expect("try_packet drain") {
                packets += 1;
                if pkt.is_keyframe {
                    got_idr = true;
                }
            }
        }
        assert!(packets >= 1, "encoder produced zero packets");
        assert!(got_idr, "no keyframe seen across 30 inputs");
    }
}
