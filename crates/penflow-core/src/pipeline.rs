//! Capture + encode loop.
//!
//! Owns the D3D11 device, capturer, color converter, encoder session, and a
//! stable BGRA "keepalive" texture (HANDOFF §2.3 #2 + #6). Runs the hot path
//! on a dedicated thread; the server consumes encoded packets from the queue.
//!
//! Loop shape:
//!   1. Read the IDR-request flag (cleared atomically).
//!   2. `acquire_frame(timeout)` from DDA.
//!   3. If a real frame arrived, `CopyResource` it into the keepalive.
//!      If it timed out, the keepalive holds the last frame — reusing it
//!      keeps the encoder warm and avoids the 80 ms cold-start spike.
//!   4. `converter.convert(&keepalive)` writes the cached NV12 output.
//!   5. `encoder.submit_frame(converter.output_texture(), pts_ns, force_idr)`.
//!   6. Drain `encoder.try_packet()` into the packet queue.
//!
//! A single force-IDR request is consumed atomically per loop iteration —
//! repeated requests within one iteration coalesce to one IDR.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread,
    SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};

use crate::capture::dxgi::{DxgiCapturer, PointerPosition};
use crate::color::{clear_bgra_texture_to_black, create_bgra_keepalive_texture, ColorConverter};
use crate::cursor_blit::CursorBlitter;
use crate::d3d11::D3d11Context;
use crate::encoder::{EncodeSession, EncodedPacket};
use crate::error::{EngineError, EngineResult};
use crate::packet_queue::PacketQueue;
use crate::tonemap_blit::TonemapBlitter;

/// RAII guard for an MMCSS task subscription. Holds the handle returned by
/// `AvSetMmThreadCharacteristicsW` and reverts on drop. While alive, the
/// owning thread runs in MMCSS's "Capture" task scheduling category — for
/// our pipeline thread that means a Medium category boost (priority 16-22)
/// and protection against starvation by lower-priority work.
struct MmcssGuard {
    handle: HANDLE,
}

impl MmcssGuard {
    /// Subscribe the calling thread to the "Capture" MMCSS task. Returns
    /// `None` if the call fails (e.g. MMCSS service disabled or the process
    /// lacks the right). Best-effort — failure leaves the thread at default
    /// priority.
    fn acquire_capture() -> Option<Self> {
        let mut task_index: u32 = 0;
        // SAFETY: `AvSetMmThreadCharacteristicsW` writes the task index out
        // through the pointer and returns INVALID_HANDLE_VALUE on failure
        // (we treat any error result as failure and skip).
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &mut task_index).ok()? };
        Some(Self { handle })
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful `AvSetMmThreadCharacteristicsW`.
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}

/// Tunables for the encoder loop. Defaults match the v1.0 reference rig.
#[derive(Clone, Copy, Debug)]
pub struct PipelineConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `acquire_frame` timeout. Sunshine recommends 200 ms (HANDOFF §1.2);
    /// shorter values mean faster cold-recovery from DDA hiccups but also
    /// more CPU spinning when the desktop is static. 200 ms is fine because
    /// we resubmit the keepalive on timeout.
    pub acquire_timeout: Duration,
    /// Packet queue capacity. Drop-oldest-on-overflow (see `PacketQueue`),
    /// so depth tunes how many frames of consumer stall can stack up before
    /// the producer starts shedding stale frames. Default 2 keeps p99
    /// recovery tight: a transient send stall of ~16 ms (one frame at 60 fps)
    /// sheds rather than queues, so when the wire un-stalls the consumer
    /// flushes only the freshest frame instead of catching up through 100+ ms
    /// of stale backlog.
    pub packet_queue_capacity: usize,
    /// Anchor `Instant` for the per-frame `pts_ns` stamp (`pts = (now -
    /// pts_epoch).as_nanos()`). The server's TimeSync replies stamp t2/t3
    /// against `session_start`; if the pipeline used its own
    /// `Instant::now()` here, the Android HUD's translated PTS would be
    /// off by however long VDD enable + engine init took (~2 s on a cold
    /// start), making `displayedNs - ptsInAndroidNs` read several seconds
    /// instead of the actual ~20 ms e2e. The session passes its own
    /// `session_start` so both clocks share an epoch.
    pub pts_epoch: Instant,
    /// "SDR content brightness" slider value, expressed as the scRGB
    /// multiplier Windows applies to SDR content on a Windows-HDR-on
    /// desktop. 1.0 = default / HDR off / unknown. Values > 1 are
    /// common (boosted SDR for OLED HDR setups). Used by the tonemap
    /// shader to renormalise scRGB before clamping. Queried from
    /// `DisplayConfigGetDeviceInfo(GET_SDR_WHITE_LEVEL)` in
    /// `Engine::start`.
    pub scrgb_sdr_scale: f32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            width: 2560,
            height: 1440,
            fps: 60,
            // 16 ms = one frame at 60 fps. See `EngineBuilder`'s default
            // for the full rationale — this is also the **decoder feed
            // rate** when DDA times out, and it must stay within the
            // tablet's HEVC decoder spec (Adreno 720: ~4K @ 60 fps).
            acquire_timeout: Duration::from_millis(16),
            packet_queue_capacity: 2,
            pts_epoch: Instant::now(),
            scrgb_sdr_scale: 1.0,
        }
    }
}

pub struct Pipeline {
    queue: Arc<PacketQueue<EncodedPacket>>,
    stop: Arc<AtomicBool>,
    idr_request: Arc<AtomicBool>,
    keepalive_uses: Arc<AtomicU64>,
    handle: Option<JoinHandle<EngineResult<()>>>,
}

impl Pipeline {
    /// Spawn the capture+encode loop. Takes ownership of every COM resource
    /// (single-threaded ownership model — see Send impls in d3d11/capture/color/encoder).
    pub fn start(
        ctx: D3d11Context,
        capturer: DxgiCapturer,
        converter: ColorConverter,
        encoder: Box<dyn EncodeSession>,
        cfg: PipelineConfig,
    ) -> EngineResult<Self> {
        let queue = PacketQueue::new(cfg.packet_queue_capacity);
        let stop = Arc::new(AtomicBool::new(false));
        let idr_request = Arc::new(AtomicBool::new(false));
        let keepalive_uses = Arc::new(AtomicU64::new(0));

        let keepalive = create_bgra_keepalive_texture(&ctx.device, cfg.width, cfg.height)?;
        // D3D11 does NOT guarantee zero-init for USAGE_DEFAULT textures. If
        // we encode the keepalive before any DDA frame has overwritten it
        // (which happens on a freshly-attached VDD extend monitor — its
        // desktop is blank, no content changes, DDA returns
        // WAIT_TIMEOUT forever), the encoder receives an undefined-state
        // texture and rejects with MF_E_UNSUPPORTED_D3D_TYPE
        // (0xC00D6D76, "the content is not supported for the current
        // Direct3D device"). Explicit black clear gives the encoder a
        // valid input no matter what.
        clear_bgra_texture_to_black(&ctx, &keepalive)?;

        // Cursor compositor: bound once to the keepalive's RTV. Failing to
        // build it shouldn't kill the session — fall back to a no-cursor
        // stream rather than refusing to start. The most likely failure is
        // D3DCompile / D3DCompiler_47.dll missing, which is rare on Win10+
        // but possible on hardened images.
        let cursor_blitter = match CursorBlitter::new(&ctx, &keepalive, cfg.width, cfg.height) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!(
                    "[pipeline] CursorBlitter::new failed ({:?}); cursor will not be visible \
                     in the stream. Set HardwareCursor=false in vdd_settings.xml as a fallback.",
                    e
                );
                None
            }
        };

        // Tonemap blitter: handles the case where DDA returns a non-BGRA
        // format (HDR10 PQ from a fullscreen-exclusive HDR app, scRGB
        // float from a Windows-HDR-on desktop). When the DDA frame is
        // already BGRA8 we take the `CopyResource` fast path and the
        // blitter goes unused for the session. Build it eagerly so we
        // surface shader-compile failures at session start rather than
        // mid-stream when HDR happens to switch on.
        let tonemap_blitter = match TonemapBlitter::new(&ctx, &keepalive, cfg.width, cfg.height) {
            Ok(b) => {
                // Push the user's "SDR content brightness" slider value
                // into the shader cbuffer. Without this, scRGB inputs
                // get clamp-clipped at 1.0 and any SDR content the
                // user has boosted above 80 nits looks "blown out" on
                // the tablet — the visible "Windows UI is overexposed"
                // symptom. See `sdr_white_level.rs`.
                b.set_scrgb_sdr_scale(cfg.scrgb_sdr_scale);
                eprintln!(
                    "[pipeline] tonemap blitter ready; scRGB SDR scale = {:.3}",
                    cfg.scrgb_sdr_scale,
                );
                Some(b)
            }
            Err(e) => {
                eprintln!(
                    "[pipeline] TonemapBlitter::new failed ({:?}); HDR-display capture will \
                     show as black. SDR (BGRA) capture is unaffected.",
                    e
                );
                None
            }
        };

        let q = Arc::clone(&queue);
        let s = Arc::clone(&stop);
        let idr = Arc::clone(&idr_request);
        let ka = Arc::clone(&keepalive_uses);
        let handle = thread::Builder::new()
            .name("penflow-encode".into())
            .spawn(move || {
                // Subscribe to the MMCSS "Capture" scheduling category and
                // bump the thread to TIME_CRITICAL within it. The MMCSS
                // handle is RAII-dropped at thread exit; SetThreadPriority
                // is not reverted explicitly because the thread terminates
                // immediately afterwards.
                let _mmcss = MmcssGuard::acquire_capture();
                // SAFETY: GetCurrentThread returns a pseudo-handle valid
                // for the calling thread; SetThreadPriority on it is safe.
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
                }

                let pts_epoch = cfg.pts_epoch;
                let mut state = LoopState {
                    ctx,
                    capturer,
                    converter,
                    encoder,
                    keepalive,
                    cursor_blitter,
                    tonemap_blitter,
                    last_pointer: None,
                    queue: q,
                    stop: s,
                    idr_request: idr,
                    keepalive_uses: ka,
                    cfg,
                    has_real_frame: false,
                    start_instant: pts_epoch,
                    last_dda_format: None,
                };
                state.run()
            })
            .map_err(|_| EngineError::NotInitialized)?;

        Ok(Self {
            queue,
            stop,
            idr_request,
            keepalive_uses,
            handle: Some(handle),
        })
    }

    pub fn packet_queue(&self) -> Arc<PacketQueue<EncodedPacket>> {
        Arc::clone(&self.queue)
    }

    /// Request an IDR on the next encoded frame. Idempotent within a single
    /// iteration of the capture loop.
    pub fn request_idr(&self) {
        self.idr_request.store(true, Ordering::Release);
    }

    /// Total number of times the loop fell back to encoding the keepalive
    /// because DDA timed out (HANDOFF §2.3 #2 prevention metric).
    pub fn keepalive_uses(&self) -> u64 {
        self.keepalive_uses.load(Ordering::Acquire)
    }

    /// Signal the loop to stop, drain in-flight work, and join the thread.
    pub fn stop(mut self) -> EngineResult<()> {
        self.stop.store(true, Ordering::Release);
        self.queue.close();
        if let Some(h) = self.handle.take() {
            match h.join() {
                Ok(r) => r,
                Err(_) => Err(EngineError::NotInitialized),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.queue.close();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct LoopState {
    ctx: D3d11Context,
    capturer: DxgiCapturer,
    converter: ColorConverter,
    encoder: Box<dyn EncodeSession>,
    keepalive: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    /// `None` if compositor failed to build (see Pipeline::start). When
    /// `None`, cursor blits are silently skipped.
    cursor_blitter: Option<CursorBlitter>,
    /// Format-conversion + tonemap shader, used when DDA returns
    /// non-BGRA frames (HDR10 PQ, scRGB float). `None` only if shader
    /// compile failed at startup — in which case HDR captures will be
    /// black. SDR captures fall through to the `CopyResource` fast
    /// path and don't depend on this.
    tonemap_blitter: Option<TonemapBlitter>,
    /// Last pointer position seen from DDA. Persisted across frames because
    /// `AcquiredFrame::pointer_position()` returns `None` on frames where
    /// the cursor didn't move; we still need to draw it at the previous
    /// location since `CopyResource` overwrites the keepalive each tick.
    last_pointer: Option<PointerPosition>,
    queue: Arc<PacketQueue<EncodedPacket>>,
    stop: Arc<AtomicBool>,
    idr_request: Arc<AtomicBool>,
    keepalive_uses: Arc<AtomicU64>,
    cfg: PipelineConfig,
    has_real_frame: bool,
    start_instant: Instant,
    /// Last DDA format we logged the routing decision for. Lets us emit
    /// one log line per format transition (HDR toggled, monitor swap)
    /// instead of every tick. `None` until the first frame.
    last_dda_format: Option<windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT>,
}

impl LoopState {
    fn run(&mut self) -> EngineResult<()> {
        while !self.stop.load(Ordering::Acquire) {
            self.tick()?;
        }
        Ok(())
    }

    fn tick(&mut self) -> EngineResult<()> {
        let force_idr = self.idr_request.swap(false, Ordering::AcqRel);
        let trace = std::env::var_os("PENFLOW_PIPELINE_TRACE").is_some();
        if trace {
            eprintln!(
                "[pipeline] tick: acquiring (timeout={:?})",
                self.cfg.acquire_timeout
            );
        }
        let acquired = match self.capturer.acquire_frame(self.cfg.acquire_timeout) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[pipeline] acquire_frame ERR: {e:?}");
                return Err(e);
            }
        };
        let now = Instant::now();

        match acquired {
            Some(mut frame) => {
                if trace {
                    eprintln!("[pipeline] got DDA frame, copying to keepalive");
                    eprintln!(
                        "[pipeline] {}",
                        describe_texture("dda_frame", &frame.texture)
                    );
                    eprintln!(
                        "[pipeline] {}",
                        describe_texture("keepalive_before_copy", &self.keepalive)
                    );
                }

                // Move DDA pixels into the BGRA keepalive. Two paths:
                //
                //   - **Fast path**: when DDA returns BGRA8 (the common
                //     SDR case, also always true for the bundled VDD),
                //     `CopyResource` is a pure GPU memcpy — ~150 µs at
                //     4K on RTX 5070.
                //
                //   - **Tonemap path**: when DDA returns RGBA8 / HDR10
                //     PQ / scRGB float, run a pixel shader that does
                //     transfer-function decode + tonemap + sRGB encode.
                //     Sits at ~300-500 µs at 4K. Required because
                //     `CopyResource` between mismatched formats silently
                //     no-ops, leaving the keepalive black (the bug
                //     behind "tablet shows black + only the cursor").
                //
                // The tonemap blitter is `None` only if shader
                // compilation failed at session start; in that case we
                // fall through to `CopyResource` for everything, which
                // will be visibly broken on HDR sources but at least
                // SDR captures still work.
                let dda_format = texture_format(&frame.texture);
                let needs_tonemap = !is_bgra8(dda_format);
                if needs_tonemap {
                    if let Some(blitter) = self.tonemap_blitter.as_ref() {
                        log_format_change_once(&mut self.last_dda_format, dda_format);
                        if let Err(e) = blitter.convert(&self.ctx, &frame.texture, dda_format) {
                            eprintln!(
                                "[pipeline] tonemap convert ERR (fmt={}): {e:?} — \
                                 falling back to CopyResource (will be black)",
                                dda_format.0
                            );
                            unsafe {
                                self.ctx
                                    .immediate_context
                                    .CopyResource(&self.keepalive, &frame.texture);
                            }
                        }
                    } else {
                        // Tonemap blitter unavailable (shader compile
                        // failed at startup). Logging once is enough —
                        // every subsequent tick would otherwise spam.
                        log_format_change_once(&mut self.last_dda_format, dda_format);
                        unsafe {
                            self.ctx
                                .immediate_context
                                .CopyResource(&self.keepalive, &frame.texture);
                        }
                    }
                } else {
                    // BGRA8 fast path. Reset the "last format we logged
                    // about" tracker so a future HDR-on toggle gets
                    // logged again.
                    log_format_change_once(&mut self.last_dda_format, dda_format);
                    unsafe {
                        self.ctx
                            .immediate_context
                            .CopyResource(&self.keepalive, &frame.texture);
                    }
                }
                self.has_real_frame = true;

                // Pull cursor state. Shape only ships when it changed — if
                // GetFramePointerShape errors we keep the cached one rather
                // than failing the whole tick (a missing cursor for one
                // frame is far cheaper than aborting the session).
                if let Some(blitter) = self.cursor_blitter.as_mut() {
                    match frame.take_shape_update() {
                        Ok(Some(shape)) => {
                            if let Err(e) = blitter.update_shape(&self.ctx, &shape) {
                                eprintln!("[pipeline] cursor update_shape err: {e:?}");
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("[pipeline] take_shape_update err: {e:?}");
                        }
                    }
                }
                if let Some(pos) = frame.pointer_position() {
                    self.last_pointer = Some(pos);
                }
                drop(frame);

                // Composite the cursor onto the freshly-copied keepalive. The
                // CopyResource above wipes any cursor we drew last tick, so we
                // always re-blit. When `Visible=false` (cursor on a different
                // monitor) we skip — the keepalive then carries no cursor
                // until DDA tells us otherwise.
                if let (Some(blitter), Some(pos)) =
                    (self.cursor_blitter.as_ref(), self.last_pointer)
                {
                    if pos.visible {
                        if let Err(e) = blitter.composite(&self.ctx, pos.x, pos.y) {
                            eprintln!("[pipeline] cursor composite err: {e:?}");
                        }
                    }
                }
            }
            None => {
                // DDA timed out. Re-encode the keepalive texture. Two
                // sub-cases:
                //   - has_real_frame=true → keepalive holds the last
                //     captured DDA frame (HANDOFF §2.3 #2 cold-start
                //     protection).
                //   - has_real_frame=false → keepalive is the zero-
                //     initialised black BGRA texture from
                //     create_bgra_keepalive_texture. We still encode it
                //     so wait_for_keyframe sees an IDR; otherwise on a
                //     freshly-extended VDD output (where the desktop can be
                //     blank with no content changes, so DDA may not fire)
                //     wait_for_keyframe times out forever. Pipeline::start
                //     explicitly cleared the keepalive texture to black, so
                //     the first frame is valid even before anything paints
                //     onto the new desktop.
                self.keepalive_uses.fetch_add(1, Ordering::AcqRel);
            }
        }

        // BGRA → NV12 (writes to the converter's stable output texture).
        if let Err(e) = self.converter.convert(&self.keepalive) {
            eprintln!("[pipeline] convert ERR: {e:?}");
            return Err(e);
        }
        if trace {
            eprintln!("[pipeline] convert ok, submitting frame to encoder");
            eprintln!(
                "[pipeline] {}",
                describe_texture("encoder_input", self.converter.output_texture())
            );
        }

        // Encode.
        let pts_ns = now.duration_since(self.start_instant).as_nanos() as i64;
        if let Err(e) =
            self.encoder
                .submit_frame(self.converter.output_texture(), pts_ns, force_idr)
        {
            eprintln!("[pipeline] submit_frame ERR: {e:?}");
            return Err(e);
        }
        if trace {
            eprintln!("[pipeline] submit_frame ok, polling for output packets");
        }
        loop {
            match self.encoder.try_packet() {
                Ok(Some(pkt)) => {
                    if trace {
                        eprintln!(
                            "[pipeline] pushing pkt: {} bytes, keyframe={}",
                            pkt.bytes.len(),
                            pkt.is_keyframe
                        );
                    }
                    self.queue.push(pkt);
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[pipeline] try_packet ERR: {e:?}");
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

fn describe_texture(
    label: &str,
    tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
) -> String {
    let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    unsafe {
        tex.GetDesc(&mut desc);
    }
    format!(
        "{label}: {}x{} fmt={} bind=0x{:x} misc=0x{:x} usage={} cpu=0x{:x}",
        desc.Width,
        desc.Height,
        desc.Format.0,
        desc.BindFlags,
        desc.MiscFlags,
        desc.Usage.0,
        desc.CPUAccessFlags
    )
}

/// Read just the texture's DXGI format. Used by the pipeline tick to
/// route between the BGRA `CopyResource` fast path and the HDR-aware
/// `TonemapBlitter`.
fn texture_format(
    tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
    let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    desc.Format
}

/// Whether the DDA format can take the `CopyResource` fast path.
/// D3D11 `CopyResource` requires identical source/destination formats,
/// so this is strictly `B8G8R8A8_UNORM`. RGBA8, R10G10B10A2, and
/// RGBA16F all go through the tonemap shader.
fn is_bgra8(fmt: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT) -> bool {
    fmt == windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM
}

/// Log a single line when the DDA format changes from one tick to the
/// next. Routine ticks within the same format are silent — adapter
/// changes are infrequent (HDR toggle, monitor swap, fullscreen-HDR
/// game launch) and worth surfacing without `PENFLOW_PIPELINE_TRACE`.
fn log_format_change_once(
    last: &mut Option<windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT>,
    current: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) {
    if *last == Some(current) {
        return;
    }
    let prev_str = last
        .map(|f| format!("{}", f.0))
        .unwrap_or_else(|| "<initial>".into());
    let path = if is_bgra8(current) {
        "CopyResource fast path"
    } else {
        "TonemapBlitter shader path"
    };
    eprintln!(
        "[pipeline] DDA format {} → {} ({})",
        prev_str, current.0, path
    );
    *last = Some(current);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::dxgi::DxgiCapturer;
    use crate::color::ColorConverter;
    use crate::d3d11::D3d11Context;
    use crate::encoder::{mf::MfBackend, Codec, EncoderBackend, PixelFormat, SessionConfig};
    use crate::monitors;

    /// End-to-end: spin the pipeline against the desktop for a few hundred ms,
    /// expect at least a couple of packets and at least one keyframe.
    #[test]
    #[ignore = "requires real D3D11 + NVENC pipeline; GitHub windows-latest VM has no GPU"]
    fn pipeline_emits_packets_and_keyframe() {
        let _g = crate::test_lock::DDA_LOCK.lock().unwrap();
        let factory = crate::d3d11::create_dxgi_factory().expect("factory");
        let mons = monitors::enumerate(&factory).expect("monitors");
        let mon = mons
            .iter()
            .find(|m| m.attached_to_desktop && !m.adapter_is_software)
            .expect("attached output")
            .clone();
        let adapter = mon.open_adapter(&factory).expect("adapter");
        let ctx = D3d11Context::create_on_adapter(adapter).expect("d3d11");
        let cfg = PipelineConfig {
            width: mon.width.min(1920),
            height: mon.height.min(1080),
            fps: 60,
            acquire_timeout: Duration::from_millis(50),
            packet_queue_capacity: 16,
            pts_epoch: Instant::now(),
            scrgb_sdr_scale: 1.0,
        };
        let conv = ColorConverter::new(&ctx, cfg.width, cfg.height, cfg.fps).expect("conv");
        // Build the encoder session BEFORE moving ctx into the capturer
        // (capturer takes ownership; we need ctx for both, so clone the
        // adapter and rebuild a second context for the encoder).
        let backend = MfBackend::new().expect("MfBackend");
        let session_cfg = SessionConfig {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_bps: 5_000_000,
            codec: Codec::Hevc,
            input_format: PixelFormat::Nv12,
        };
        let encoder = backend.make_session(&ctx, session_cfg).expect("session");
        let capturer = DxgiCapturer::new(
            // Need a second ctx for capturer since it takes ownership;
            // both must be on the same adapter (LUID-checked at construction).
            D3d11Context::create_on_adapter(mon.open_adapter(&factory).expect("adapter2"))
                .expect("d3d11 #2"),
            mon,
        )
        .expect("capturer");

        let pipeline = Pipeline::start(ctx, capturer, conv, encoder, cfg).expect("start");
        // Force an IDR very early so we don't depend on the encoder's natural
        // GOP cadence.
        pipeline.request_idr();
        let q = pipeline.packet_queue();
        let mut got_keyframe = false;
        let mut packets = 0usize;
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            if let Some(pkt) = q.pop_timeout(Duration::from_millis(100)) {
                packets += 1;
                if pkt.is_keyframe {
                    got_keyframe = true;
                }
                if packets >= 5 && got_keyframe {
                    break;
                }
            }
        }
        let _ = pipeline.stop();
        assert!(packets >= 1, "pipeline produced zero packets in 800ms");
        assert!(got_keyframe, "no keyframe seen even after request_idr");
    }
}
