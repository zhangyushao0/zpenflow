//! Penflow capture + encode + inject engine.
//!
//! See `docs/design.md` §6 for the architecture and `docs/HANDOFF.md` §3.3 /
//! §4.4b / §4.5 for the gate-2 findings that shape the Windows hot-path.
//!
//! Top-level surface: [`Engine`] / [`EngineBuilder`] for capture+encode,
//! [`inject`] for pen/touch injection. The penflow-server crate owns the
//! tokio session loop that wires these together to the transport.

pub mod error;

#[cfg(windows)]
pub mod d3d11;
#[cfg(windows)]
pub mod monitors;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod color;
#[cfg(windows)]
pub mod encoder;

pub mod packet_queue;
#[cfg(windows)]
pub mod pipeline;
pub mod inject;

pub use error::{EngineError, EngineResult};

#[doc(hidden)]
#[cfg(test)]
pub mod test_lock {
    //! DXGI Output Duplication is a process-wide singleton (per output).
    //! Tests that hold DDA (capture/dxgi, pipeline, lib::engine_round_trip)
    //! must serialise on this mutex or they race and the second one to start
    //! gets `E_INVALIDARG` from `DuplicateOutput1`.
    use std::sync::Mutex;
    pub static DDA_LOCK: Mutex<()> = Mutex::new(());
}

#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use crate::capture::dxgi::DxgiCapturer;
#[cfg(windows)]
use crate::color::ColorConverter;
#[cfg(windows)]
use crate::d3d11::{create_dxgi_factory, D3d11Context};
#[cfg(windows)]
use crate::encoder::{
    mf::MfBackend, Codec, EncodedPacket, EncoderBackend, PixelFormat, SessionConfig,
};
#[cfg(windows)]
use crate::monitors::MonitorInfo;
#[cfg(windows)]
use crate::packet_queue::PacketQueue;
#[cfg(windows)]
use crate::pipeline::{Pipeline, PipelineConfig};

/// Public engine handle. Owns the pipeline thread and exposes the encoded-
/// packet queue, IDR request signal, and telemetry counters. Construct via
/// [`Engine::list_monitors`] + [`Engine::builder`].
#[cfg(windows)]
pub struct Engine {
    monitor: MonitorInfo,
    pipeline: Option<Pipeline>,
}

#[cfg(windows)]
impl Engine {
    /// Enumerate every output on every adapter — what the GUI shows in its
    /// monitor picker. Cheap (one DXGI factory walk); call once per GUI
    /// refresh.
    pub fn list_monitors() -> EngineResult<Vec<MonitorInfo>> {
        let factory = create_dxgi_factory()?;
        monitors::enumerate(&factory)
    }

    /// Start an [`EngineBuilder`] for the chosen monitor. Build with
    /// `.codec()` / `.bitrate_bps()` / `.fps()` / `.acquire_timeout()` /
    /// `.start()`.
    pub fn builder(monitor: MonitorInfo) -> EngineBuilder {
        EngineBuilder {
            monitor,
            codec: Codec::Hevc,
            bitrate_bps: 50_000_000,
            fps: 60,
            acquire_timeout: Duration::from_millis(200),
            packet_queue_capacity: 8,
        }
    }

    /// The encoded-packet queue — server pops, transport sends. The queue
    /// drops oldest packets on overflow (capacity defaults to 8; freshness
    /// wins for live video).
    pub fn packet_queue(&self) -> Arc<PacketQueue<EncodedPacket>> {
        self.pipeline
            .as_ref()
            .expect("pipeline live for engine lifetime")
            .packet_queue()
    }

    /// Request an IDR on the next encoded frame. Used on initial connect or
    /// when the Android client signals decoder recovery (`MSG_REQUEST_IDR`).
    pub fn request_idr(&self) {
        if let Some(p) = self.pipeline.as_ref() {
            p.request_idr();
        }
    }

    /// How many times the loop fell back to encoding the keepalive texture
    /// because DDA timed out. A handful is normal (idle desktop); spikes
    /// past ~fps×3 indicate a capture-side problem.
    pub fn keepalive_uses(&self) -> u64 {
        self.pipeline
            .as_ref()
            .map(|p| p.keepalive_uses())
            .unwrap_or(0)
    }

    /// The monitor that was selected for capture.
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Stop the pipeline and join its thread. Ignores already-stopped state.
    pub fn stop(mut self) -> EngineResult<()> {
        if let Some(p) = self.pipeline.take() {
            p.stop()?;
        }
        Ok(())
    }
}

#[cfg(windows)]
pub struct EngineBuilder {
    monitor: MonitorInfo,
    codec: Codec,
    bitrate_bps: u32,
    fps: u32,
    acquire_timeout: Duration,
    packet_queue_capacity: usize,
}

#[cfg(windows)]
impl EngineBuilder {
    pub fn codec(mut self, c: Codec) -> Self {
        self.codec = c;
        self
    }

    pub fn bitrate_bps(mut self, b: u32) -> Self {
        self.bitrate_bps = b;
        self
    }

    pub fn fps(mut self, f: u32) -> Self {
        self.fps = f;
        self
    }

    pub fn acquire_timeout(mut self, d: Duration) -> Self {
        self.acquire_timeout = d;
        self
    }

    pub fn packet_queue_capacity(mut self, n: usize) -> Self {
        self.packet_queue_capacity = n;
        self
    }

    /// Build and start the engine. Constructs the D3D11 context on the
    /// monitor's owning adapter, the DDA capturer, the BGRA→NV12 converter,
    /// the MF HEVC session, and finally the pipeline thread.
    pub fn start(self) -> EngineResult<Engine> {
        // Process-wide DPI awareness so DXGI output dimensions are physical
        // pixels, not DIPs (gate-2 finding §4.4b — without this, capture on
        // a 4K monitor at 150% scaling reports 2560×1440 instead of
        // 3840×2160 and the encoder loses half the pixels). Best-effort —
        // a host that already set this returns an error, which we ignore.
        let _ = unsafe {
            windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        };

        let factory = create_dxgi_factory()?;
        // Re-enumerate after DPI awareness so the monitor's reported
        // dimensions reflect physical pixels for the encoder config.
        let monitor = monitors::enumerate(&factory)?
            .into_iter()
            .find(|m| {
                m.adapter_luid == self.monitor.adapter_luid
                    && m.device_name == self.monitor.device_name
            })
            .unwrap_or(self.monitor.clone());
        let adapter = monitor.open_adapter(&factory)?;
        let ctx = D3d11Context::create_on_adapter(adapter)?;

        let width = monitor.width;
        let height = monitor.height;
        if width == 0 || height == 0 {
            return Err(EngineError::AdapterHasNoOutputs {
                name: monitor.adapter_name.clone(),
                luid: monitor.adapter_luid,
            });
        }

        // Capturer needs its own D3d11Context (takes ownership of one);
        // the encoder + converter share the second one. Both must be on the
        // same adapter — enforced by LUID check inside DxgiCapturer::new.
        let capturer_ctx =
            D3d11Context::create_on_adapter(monitor.open_adapter(&factory)?)?;
        let capturer = DxgiCapturer::new(capturer_ctx, monitor.clone())?;
        let converter = ColorConverter::new(&ctx, width, height, self.fps)?;

        let backend = MfBackend::new()?;
        let session = backend.make_session(
            &ctx,
            SessionConfig {
                width,
                height,
                fps: self.fps,
                bitrate_bps: self.bitrate_bps,
                codec: self.codec,
                input_format: PixelFormat::Nv12,
            },
        )?;

        let pipeline = Pipeline::start(
            ctx,
            capturer,
            converter,
            session,
            PipelineConfig {
                width,
                height,
                fps: self.fps,
                acquire_timeout: self.acquire_timeout,
                packet_queue_capacity: self.packet_queue_capacity,
            },
        )?;

        Ok(Engine {
            monitor,
            pipeline: Some(pipeline),
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn engine_round_trip() {
        use super::*;
        let _g = crate::test_lock::DDA_LOCK.lock().unwrap();
        let mons = Engine::list_monitors().expect("list_monitors");
        let mon = mons
            .into_iter()
            .find(|m| m.attached_to_desktop && !m.adapter_is_software)
            .expect("attached output");
        let engine = Engine::builder(mon)
            .codec(Codec::Hevc)
            .bitrate_bps(5_000_000)
            .fps(60)
            .acquire_timeout(Duration::from_millis(50))
            .packet_queue_capacity(16)
            .start()
            .expect("engine start");
        engine.request_idr();
        let q = engine.packet_queue();
        let mut packets = 0;
        let mut keyframes = 0;
        let deadline = std::time::Instant::now() + Duration::from_millis(700);
        while std::time::Instant::now() < deadline {
            if let Some(pkt) = q.pop_timeout(Duration::from_millis(100)) {
                packets += 1;
                if pkt.is_keyframe {
                    keyframes += 1;
                }
                if packets >= 3 && keyframes >= 1 {
                    break;
                }
            }
        }
        engine.stop().expect("engine stop");
        assert!(packets >= 1, "engine produced zero packets in 700ms");
        assert!(keyframes >= 1, "no keyframe even after request_idr");
    }
}
