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
//!   5. `encoder.submit_frame(converter.output_texture(), pts_ns, captured_at, force_idr)`.
//!   6. Drain `encoder.try_packet()` into the packet queue.
//!
//! A single force-IDR request is consumed atomically per loop iteration —
//! repeated requests within one iteration coalesce to one IDR.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::capture::dxgi::DxgiCapturer;
use crate::color::{
    clear_bgra_texture_to_black, create_bgra_keepalive_texture, ColorConverter,
};
use crate::d3d11::D3d11Context;
use crate::encoder::{EncodeSession, EncodedPacket};
use crate::error::{EngineError, EngineResult};
use crate::packet_queue::PacketQueue;

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
    /// Packet queue capacity. 8 ≈ 130 ms at 60 fps; deeper than the e2e
    /// budget so any backlog is unambiguously a consumer problem.
    pub packet_queue_capacity: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            width: 2560,
            height: 1440,
            fps: 60,
            acquire_timeout: Duration::from_millis(200),
            packet_queue_capacity: 8,
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

        let q = Arc::clone(&queue);
        let s = Arc::clone(&stop);
        let idr = Arc::clone(&idr_request);
        let ka = Arc::clone(&keepalive_uses);
        let handle = thread::Builder::new()
            .name("penflow-encode".into())
            .spawn(move || {
                let mut state = LoopState {
                    ctx,
                    capturer,
                    converter,
                    encoder,
                    keepalive,
                    queue: q,
                    stop: s,
                    idr_request: idr,
                    keepalive_uses: ka,
                    cfg,
                    has_real_frame: false,
                    start_instant: Instant::now(),
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
    queue: Arc<PacketQueue<EncodedPacket>>,
    stop: Arc<AtomicBool>,
    idr_request: Arc<AtomicBool>,
    keepalive_uses: Arc<AtomicU64>,
    cfg: PipelineConfig,
    has_real_frame: bool,
    start_instant: Instant,
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
            eprintln!("[pipeline] tick: acquiring (timeout={:?})", self.cfg.acquire_timeout);
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
            Some(frame) => {
                if trace {
                    eprintln!("[pipeline] got DDA frame, copying to keepalive");
                }
                // Copy DXGI texture → stable BGRA keepalive (GPU-only copy,
                // ~150 us on RTX 5070 for 4K). The frame's RAII guard
                // releases the duplication when it goes out of scope.
                unsafe {
                    self.ctx
                        .immediate_context
                        .CopyResource(&self.keepalive, &frame.texture);
                }
                self.has_real_frame = true;
                drop(frame);
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
                //     freshly-extended VDD output (where the desktop is
                //     blank with no content changes, so DDA never fires)
                //     wait_for_keyframe times out forever. D3D11
                //     USAGE_DEFAULT textures are zero-cleared by the
                //     driver at allocation, so the first frame the
                //     tablet sees is plain black until something paints
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
        }

        // Encode.
        let pts_ns = now.duration_since(self.start_instant).as_nanos() as i64;
        if let Err(e) = self.encoder.submit_frame(
            self.converter.output_texture(),
            pts_ns,
            Some(now),
            force_idr,
        ) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::dxgi::DxgiCapturer;
    use crate::color::ColorConverter;
    use crate::d3d11::D3d11Context;
    use crate::encoder::{
        mf::MfBackend, Codec, EncoderBackend, PixelFormat, SessionConfig,
    };
    use crate::monitors;

    /// End-to-end: spin the pipeline against the desktop for a few hundred ms,
    /// expect at least a couple of packets and at least one keyframe.
    #[test]
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
