//! One Penflow session: handshake + frame pump + input dispatch.
//!
//! Lifecycle (design.md §9):
//!
//! 1. `transport.accept().await` — block until the Android client connects.
//! 2. Read `MSG_HELLO_ANDROID` — record device caps, log them.
//! 3. Start the engine; wait for the first keyframe.
//! 4. Extract VPS+SPS+PPS from the keyframe → send `MSG_HELLO_PC` +
//!    `MSG_VIDEO_CONFIG` (csd-0).
//! 5. Spawn three tasks:
//!    - `frame_pump` — pulls `EncodedPacket`s off `engine.packet_queue()`
//!      and writes them as `MSG_VIDEO_FRAME`.
//!    - `read_loop` — decodes inbound frames and dispatches them to the
//!      pen / touch injectors / time-sync responder / IDR-request hook.
//!    - `telemetry_pump` — every second, sends a `MSG_TELEMETRY` so the
//!      Android HUD has a heartbeat.
//! 6. `select!` until any task ends or `Session::stop()` is called; tear
//!    down cleanly.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use penflow_core::encoder::{Codec, EncodedPacket};
use penflow_core::inject::coords::AffineTransform;
use penflow_core::inject::win_ink::InputInjector;
use penflow_core::inject::{PenSample, TouchPoint, TouchState};
use penflow_core::monitors::MonitorInfo;
use penflow_core::Engine;
use penflow_protocol::{
    encode_frame, extract_hevc_nals, read_frame, write_frame, HelloAndroid, HelloPc, PenEvent,
    Telemetry, TimeSyncReq, TimeSyncResp, TouchEvent, VideoFrame, CODEC_HEVC,
    FRAME_FLAG_KEYFRAME, MSG_ANDROID_GOODBYE, MSG_HELLO_ANDROID, MSG_HELLO_PC, MSG_PC_GOODBYE,
    MSG_PEN_EVENT, MSG_REQUEST_IDR, MSG_TELEMETRY, MSG_TIME_SYNC_REQ, MSG_TIME_SYNC_RESP,
    MSG_TOUCH_EVENT, MSG_VIDEO_CONFIG, MSG_VIDEO_FRAME,
};
use penflow_transport::{Transport, TransportStream};

use crate::vdd::{
    snapshot_attached_monitor_keys, wait_for_virtual_monitor, VddController, VddError,
};

/// Session-level errors. Most fan-in from the engine, transport, or protocol;
/// the variants below capture the few cases where the orchestrator wants to
/// surface a more specific message.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Engine startup or runtime failed.
    #[error("engine error: {0}")]
    Engine(#[from] penflow_core::EngineError),

    /// Underlying transport / protocol I/O failed.
    #[error("protocol error: {0}")]
    Protocol(#[from] penflow_protocol::ProtocolError),

    /// Catch-all for transport-level I/O.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The Android client sent a message we weren't expecting at handshake.
    #[error("expected MSG_HELLO_ANDROID, got 0x{0:02x}")]
    UnexpectedHandshakeMessage(u8),

    /// Waited for the engine's first keyframe but never received one.
    #[error("engine produced no keyframe within {0:?}")]
    NoKeyframe(Duration),

    /// VDD lifecycle (enable / disable / wait-for-DXGI) failed.
    #[error("VDD lifecycle: {0}")]
    Vdd(#[from] VddError),
}

/// What [`Session::run`] sends to its caller through the event channel.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// Transport connection was accepted; handshake about to start.
    Connecting {
        /// Human-readable peer identifier (e.g. `adb:127.0.0.1:1234`).
        peer: String,
    },
    /// Handshake completed; streaming is live.
    Connected {
        /// Human-readable peer identifier.
        peer: String,
        /// Android-side display width reported in `HELLO_ANDROID`.
        device_width: u16,
        /// Android-side display height reported in `HELLO_ANDROID`.
        device_height: u16,
    },
    /// Clean disconnect from the client (`MSG_ANDROID_GOODBYE` or EOF).
    Disconnected,
    /// Recoverable error — the read or write loop ended unexpectedly. The
    /// caller can re-run the session.
    Errored(String),
}

/// Configuration for one session.
#[derive(Debug)]
pub struct SessionConfig {
    /// Monitor to capture when not using VDD. When `vdd` is `Some`, the
    /// session enables the virtual driver after the Android handshake and
    /// captures the resulting virtual monitor instead — this field is the
    /// fallback for `vdd: None`.
    pub monitor: MonitorInfo,
    /// Encoder codec.
    pub codec: Codec,
    /// Encoder bitrate.
    pub bitrate_bps: u32,
    /// Encoder frame rate.
    pub fps: u32,
    /// Optional Virtual Display Driver controller. When set, the session
    /// calls `enable()` after the handshake completes and captures the
    /// virtual monitor that appears; on disconnect (or panic / Drop) the
    /// `disable()` is called to remove the virtual monitor from idle
    /// desktop. Discovered by `VddController::detect()` at process startup.
    pub vdd: Option<VddController>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            // Caller MUST overwrite `monitor` — there's no sensible default.
            monitor: MonitorInfo {
                adapter_index: 0,
                adapter_luid: 0,
                adapter_name: String::new(),
                adapter_vendor_id: 0,
                adapter_device_id: 0,
                adapter_is_software: false,
                output_index_within_adapter: 0,
                device_name: String::new(),
                width: 0,
                height: 0,
                desktop_coords: (0, 0, 0, 0),
                rotation: 1,
                attached_to_desktop: false,
                looks_virtual: false,
            },
            codec: Codec::Hevc,
            bitrate_bps: 50_000_000,
            fps: 60,
            vdd: None,
        }
    }
}

/// Runtime counters; the telemetry pump samples this each tick. `dropped`
/// is read from `queue.stats()` directly (it's the authoritative source) so
/// we only track frames-out and current queue depth here.
#[derive(Default)]
struct Stats {
    frames: AtomicU32,
    queue_depth: AtomicU32,
}

/// One Penflow session. `run()` blocks for the lifetime of the connection.
pub struct Session {
    cfg: SessionConfig,
}

impl Session {
    /// Create the session orchestrator. Doesn't open the transport yet —
    /// that happens in [`run`].
    pub fn new(cfg: SessionConfig) -> Self {
        Self { cfg }
    }

    /// Run one session: open the engine, accept the transport stream, do the
    /// handshake, pump frames + dispatch input until the client disconnects.
    ///
    /// `events` (optional) receives lifecycle notifications. The function
    /// returns after the connection ends or `stop` flag is set.
    pub async fn run(
        mut self,
        transport: Arc<dyn Transport>,
        events: Option<tokio::sync::mpsc::Sender<SessionEvent>>,
    ) -> Result<(), SessionError> {
        let session_start = Instant::now();

        // 1. Accept the transport FIRST. Engine startup happens after the
        //    handshake — otherwise the pipeline runs while we wait for
        //    Android, the queue (capacity 8, drop-oldest) silently evicts
        //    the encoder's natural first-frame IDR, and `wait_for_keyframe`
        //    times out waiting for the next IDR (10s away at GOP=600).
        let stream = transport.accept().await?;
        let TransportStream {
            mut reader,
            mut writer,
            peer_label,
        } = stream;

        if let Some(tx) = &events {
            let _ = tx
                .send(SessionEvent::Connecting { peer: peer_label.clone() })
                .await;
        }

        // 2. Handshake: read HELLO_ANDROID.
        let (msg_id, payload) = read_frame(&mut reader).await?;
        if msg_id != MSG_HELLO_ANDROID {
            return Err(SessionError::UnexpectedHandshakeMessage(msg_id));
        }
        let android = HelloAndroid::decode(&payload)?;
        eprintln!(
            "[session] HELLO_ANDROID from {}: {}x{} pen_max_pressure={} caps=0b{:08b}",
            peer_label,
            android.display_width,
            android.display_height,
            android.pen_max_pressure,
            android.codec_caps
        );

        // 3. Enable the Virtual Display Driver if we have one. Otherwise
        //    fall through to capturing whatever monitor the operator
        //    configured.
        //
        //    Order matters: enable VDD AFTER HELLO_ANDROID (so the
        //    virtual monitor only exists while a client is actually
        //    connected — design.md §16 / HANDOFF §4.6) and BEFORE engine
        //    startup (the engine builder enumerates monitors and creates
        //    its D3D11 context; the new VDD output must be visible to
        //    DXGI by then).
        let capture_monitor = if let Some(vdd) = self.cfg.vdd.as_mut() {
            eprintln!(
                "[session] enabling VDD device '{}' ({})",
                vdd.friendly_name(),
                vdd.instance_id()
            );
            let baseline_attached = snapshot_attached_monitor_keys()?;
            vdd.enable()?;
            // Windows + the VDD driver itself can take a couple of seconds
            // to publish the new monitor through DXGI on a cold start (it
            // re-reads vdd_settings.xml, calls IddCxMonitorArrival, and
            // DisplayConfig attaches the new target to the desktop). 15 s is
            // generous; if we hit this we genuinely have a driver/topology
            // problem.
            let instance_id = vdd.instance_id().to_string();
            let virt = wait_for_virtual_monitor(
                Duration::from_secs(15),
                Some(&instance_id),
                Some(&baseline_attached),
            )
            .await?;
            eprintln!(
                "[session] virtual monitor up: {} {}x{} on {} (adapter LUID 0x{:016x})",
                virt.device_name,
                virt.width,
                virt.height,
                virt.adapter_name,
                virt.adapter_luid
            );

            // Cross-adapter check: NVIDIA exposes the RTX 5070 as multiple
            // logical DXGI adapters (one with desktop outputs + NVENC,
            // one or two more for compute/encode-only). When VDD enables
            // its IDDCx output can land on any of those — and if it lands
            // on a non-NVENC sibling, the NVIDIA HEVC Encoder MFT rejects
            // textures from that device with HRESULT 0xC00D6D76 ("D3D
            // device does not support this input type"). Surface this up
            // front rather than letting submit_frame fail mysteriously.
            let factory = penflow_core::d3d11::create_dxgi_factory()?;
            let high_perf: windows::Win32::Graphics::Dxgi::IDXGIAdapter1 = unsafe {
                factory
                    .EnumAdapterByGpuPreference(
                        0,
                        windows::Win32::Graphics::Dxgi::DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
                    )
                    .map_err(|e| std::io::Error::other(format!("{e:?}")))?
            };
            let hp_desc = unsafe {
                high_perf
                    .GetDesc1()
                    .map_err(|e| std::io::Error::other(format!("{e:?}")))?
            };
            let hp_luid = ((hp_desc.AdapterLuid.HighPart as i64) << 32)
                | (hp_desc.AdapterLuid.LowPart as i64);
            eprintln!(
                "[session] high-perf adapter LUID 0x{:016x} (where NVENC lives)",
                hp_luid
            );
            if hp_luid != virt.adapter_luid {
                eprintln!(
                    "[session] WARNING: VDD output is on a different DXGI adapter than the\n\
                     [session]          high-performance NVENC adapter. The NVIDIA HEVC encoder\n\
                     [session]          MFT will reject the input texture (HRESULT 0xC00D6D76).\n\
                     [session]          This is design.md §6.1's known cross-adapter case —\n\
                     [session]          v1.0 doesn't yet have a shared-texture path."
                );
            }

            // SetDisplayConfig(EXTEND) returns synchronously but the
            // NVIDIA user-mode driver keeps reshuffling internal device
            // handles for a few hundred ms after — bringing up new
            // monitor scanout queues, re-binding hardware engines, etc.
            // If we create the MF HEVC encoder MFT during that window,
            // the MFT binds to a device handle that the driver
            // invalidates moments later. The first `ProcessInput` then
            // fails with HRESULT 0xC00D6D76 (MF_E_DXGI_NEW_VIDEO_DEVICE
            // in mferror.h — "the underlying D3D device has changed
            // since the MFT was bound; reinitialise it"). Give the
            // driver time to fully settle before Engine.start() creates
            // the MFT. 500 ms is generous; if this turns out to be
            // flaky we can poll + retry instead.
            tokio::time::sleep(Duration::from_millis(500)).await;
            virt
        } else {
            self.cfg.monitor.clone()
        };

        // 4. NOW start the engine. HEVC's first encoded frame is necessarily
        //    an IDR (no reference frames available), so we don't need an
        //    explicit `request_idr()` — just take whatever comes off the
        //    queue first.
        let engine = Engine::builder(capture_monitor)
            .codec(self.cfg.codec)
            .bitrate_bps(self.cfg.bitrate_bps)
            .fps(self.cfg.fps)
            .start()?;

        // 5. Build the unified pen+touch injector and the input→output
        //    coordinate transform. InputInjector::new sets
        //    PER_MONITOR_AWARE_V2 process-wide so captured coords +
        //    injection coords are both physical pixels (gate-2 §4.4b).
        //    A single WinRT InputInjector instance handles both pen and
        //    touch — agile object, safe to call from any tokio worker.
        let injector = Arc::new(Mutex::new(InputInjector::new()?));
        // Map DXGI rotation enum (1=identity, 2=90°, 3=180°, 4=270°) to
        // degrees for the AffineTransform.
        let rotation_deg: u32 = match engine.monitor().rotation {
            2 => 90,
            3 => 180,
            4 => 270,
            _ => 0,
        };
        let coords = AffineTransform::from_normalized_to_rect(
            engine.monitor().desktop_coords.0,
            engine.monitor().desktop_coords.1,
            engine.monitor().width,
            engine.monitor().height,
            rotation_deg,
        );

        // 5. Wait for the first keyframe so we can derive csd-0. The engine
        //    just started; first packet is the IDR. 5 s timeout absorbs DDA
        //    cold-start (Sunshine reports first AcquireNextFrame can take
        //    400-800 ms on hot reconfig).
        let queue = engine.packet_queue();
        let first_pkt = wait_for_keyframe(&queue, Duration::from_secs(5)).await?;
        let csd0 = extract_hevc_nals(&first_pkt.bytes, &[32, 33, 34]);
        if csd0.is_empty() {
            // Defensive: a keyframe with no parameter sets is a violation
            // of the encoder contract. Fail loudly so the operator knows.
            return Err(SessionError::NoKeyframe(Duration::from_secs(3)));
        }

        // 6. Send HELLO_PC + VIDEO_CONFIG + the first VIDEO_FRAME.
        let hello_pc = HelloPc {
            protocol_version: 0,
            width: engine.monitor().width.min(u16::MAX as u32) as u16,
            height: engine.monitor().height.min(u16::MAX as u32) as u16,
            codec: CODEC_HEVC, // matches Codec::Hevc
            bitrate_bps: self.cfg.bitrate_bps,
            fps: self.cfg.fps.min(255) as u8,
        };
        write_frame(&mut writer, MSG_HELLO_PC, &hello_pc.encode()).await?;
        write_frame(&mut writer, MSG_VIDEO_CONFIG, &csd0).await?;
        let first_vf = VideoFrame {
            pts_ns: first_pkt.pts_ns,
            flags: FRAME_FLAG_KEYFRAME,
            capture_us: None,
            encode_us: None,
            coded: first_pkt.bytes.clone(),
        };
        write_frame(&mut writer, MSG_VIDEO_FRAME, &first_vf.encode()).await?;
        writer.flush().await?;

        if let Some(tx) = &events {
            let _ = tx
                .send(SessionEvent::Connected {
                    peer: peer_label.clone(),
                    device_width: android.display_width,
                    device_height: android.display_height,
                })
                .await;
        }

        // 7. Spawn the three pumps.
        let stats = Arc::new(Stats::default());
        let writer = Arc::new(Mutex::new(writer));
        let stop = Arc::new(tokio::sync::Notify::new());

        let frame_pump = tokio::spawn(frame_pump(
            queue.clone(),
            writer.clone(),
            stats.clone(),
            stop.clone(),
        ));

        let telemetry_pump = tokio::spawn(telemetry_pump(
            writer.clone(),
            stats.clone(),
            queue.clone(),
            stop.clone(),
        ));

        // IDR-request relay: read_loop signals here, run() owns the engine
        // and calls request_idr() in response. Avoids needing to share &Engine
        // across tasks.
        let (idr_tx, mut idr_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let dispatch = tokio::spawn(read_loop(
            reader,
            writer.clone(),
            injector.clone(),
            coords,
            android.display_width,
            android.display_height,
            idr_tx,
            session_start,
        ));

        // 8. Wait for the read loop to finish, while servicing IDR requests.
        let mut dispatch = dispatch;
        let read_result: Result<(), SessionError> = loop {
            tokio::select! {
                r = &mut dispatch => match r {
                    Ok(inner) => break inner,
                    Err(join_err) => {
                        break Err(SessionError::Io(std::io::Error::other(
                            format!("read_loop join: {join_err}")
                        )));
                    }
                },
                Some(()) = idr_rx.recv() => {
                    engine.request_idr();
                }
            }
        };

        stop.notify_waiters();
        let _ = tokio::time::timeout(Duration::from_millis(500), frame_pump).await;
        let _ = tokio::time::timeout(Duration::from_millis(500), telemetry_pump).await;

        // 9. Send PC_GOODBYE so the Android side logs a clean shutdown.
        {
            let mut w = writer.lock().await;
            let _ = write_frame(&mut *w, MSG_PC_GOODBYE, &[]).await;
            let _ = w.flush().await;
        }
        let _ = engine.stop();
        // VddController's Drop runs Disable-PnpDevice via the elevated
        // helper after this scope exits. Windows clears the extend
        // topology automatically when the VDD output disappears, so
        // we don't need to call SetDisplayConfig ourselves on the way
        // out.

        if let Some(tx) = &events {
            match &read_result {
                Ok(()) => {
                    let _ = tx.send(SessionEvent::Disconnected).await;
                }
                Err(e) => {
                    let _ = tx.send(SessionEvent::Errored(format!("{e}"))).await;
                }
            }
        }
        read_result
    }
}

async fn wait_for_keyframe(
    queue: &Arc<penflow_core::packet_queue::PacketQueue<EncodedPacket>>,
    timeout: Duration,
) -> Result<EncodedPacket, SessionError> {
    let trace = std::env::var_os("PENFLOW_PIPELINE_TRACE").is_some();
    let start = Instant::now();
    let deadline = start + timeout;
    let mut popped_total: u32 = 0;
    while Instant::now() < deadline {
        let q = queue.clone();
        let pkt = tokio::task::spawn_blocking(move || {
            q.pop_timeout(Duration::from_millis(100))
        })
        .await
        .map_err(|e| std::io::Error::other(format!("blocking pop join: {e}")))?;
        if let Some(p) = pkt {
            popped_total += 1;
            if p.is_keyframe {
                if trace {
                    eprintln!(
                        "[wait_for_keyframe] got keyframe after {:.2}s, total={}",
                        start.elapsed().as_secs_f64(),
                        popped_total
                    );
                }
                return Ok(p);
            }
            // Drop non-keyframes that arrived before the IDR.
        }
    }
    eprintln!(
        "[wait_for_keyframe] TIMEOUT after {:?}: popped {} non-keyframe packets, queue depth on exit: {}",
        timeout, popped_total, queue.stats().depth
    );
    Err(SessionError::NoKeyframe(timeout))
}

async fn frame_pump(
    queue: Arc<penflow_core::packet_queue::PacketQueue<EncodedPacket>>,
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    stats: Arc<Stats>,
    stop: Arc<tokio::sync::Notify>,
) {
    loop {
        let q = queue.clone();
        let pop = tokio::task::spawn_blocking(move || q.pop_timeout(Duration::from_millis(200)));
        tokio::select! {
            _ = stop.notified() => return,
            r = pop => {
                let pkt = match r {
                    Ok(Some(p)) => p,
                    Ok(None) => continue,
                    Err(_) => return,
                };
                let vf = VideoFrame {
                    pts_ns: pkt.pts_ns,
                    flags: if pkt.is_keyframe { FRAME_FLAG_KEYFRAME } else { 0 },
                    capture_us: None,
                    encode_us: None,
                    coded: pkt.bytes,
                };
                let bytes = encode_frame(MSG_VIDEO_FRAME, &vf.encode());
                let mut w = writer.lock().await;
                if let Err(e) = w.write_all(&bytes).await {
                    eprintln!("[frame_pump] write failed: {e}");
                    return;
                }
                if let Err(e) = w.flush().await {
                    eprintln!("[frame_pump] flush failed: {e}");
                    return;
                }
                stats.frames.fetch_add(1, Ordering::Relaxed);
                let depth = queue.stats().depth as u32;
                stats.queue_depth.store(depth, Ordering::Relaxed);
            }
        }
    }
}

async fn telemetry_pump(
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    stats: Arc<Stats>,
    queue: Arc<penflow_core::packet_queue::PacketQueue<EncodedPacket>>,
    stop: Arc<tokio::sync::Notify>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.tick().await; // skip the immediate first tick
    loop {
        tokio::select! {
            _ = stop.notified() => return,
            _ = interval.tick() => {
                let frames = stats.frames.swap(0, Ordering::Relaxed);
                let queue_stats = queue.stats();
                let dropped = queue_stats.dropped_overflow as u32;
                let depth = queue_stats.depth.min(u8::MAX as usize) as u8;
                let t = Telemetry {
                    frames,
                    dropped,
                    capture_us_avg: 0,
                    encode_us_avg: 0,
                    encode_us_p99: 0,
                    queue_depth: depth,
                };
                let bytes = encode_frame(MSG_TELEMETRY, &t.encode());
                let mut w = writer.lock().await;
                if w.write_all(&bytes).await.is_err() { return; }
                if w.flush().await.is_err() { return; }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    injector: Arc<Mutex<InputInjector>>,
    coords: AffineTransform,
    android_w: u16,
    android_h: u16,
    idr_tx: tokio::sync::mpsc::UnboundedSender<()>,
    session_start: Instant,
) -> Result<(), SessionError> {
    let _ = (android_w, android_h); // captured for future use
    loop {
        let (msg_id, payload) = match read_frame(&mut reader).await {
            Ok(v) => v,
            Err(penflow_protocol::ProtocolError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        match msg_id {
            MSG_PEN_EVENT => {
                let pe = PenEvent::decode(&payload)?;
                let (x, y) = coords.map_to_pixel(pe.x_norm, pe.y_norm);
                let sample = PenSample {
                    x,
                    y,
                    pressure: pe.pressure,
                    tilt_x_deg: pe.tilt_x as i32,
                    tilt_y_deg: pe.tilt_y as i32,
                    in_range: pe.phase != 4,                     // 4 = leave
                    in_contact: matches!(pe.phase, 1 | 2),       // down or move
                    eraser: pe.tool == 1,
                    captured_at: None,
                };
                let mut inj = injector.lock().await;
                if let Err(e) = inj.inject_pen(&sample) {
                    eprintln!("[read_loop] pen inject failed: {e:?}");
                }
            }
            MSG_TOUCH_EVENT => {
                let te = TouchEvent::decode(&payload)?;
                let snapshot: Vec<TouchPoint> = te
                    .contacts
                    .iter()
                    .map(|c| {
                        let (x, y) = coords.map_to_pixel(c.x_norm, c.y_norm);
                        TouchPoint {
                            id: c.pointer_id as u32,
                            x,
                            y,
                            // Android sends only currently-down contacts;
                            // InputInjector synthesises Down/Up transitions
                            // from the diff with the previous snapshot.
                            state: TouchState::Update,
                        }
                    })
                    .collect();
                let mut inj = injector.lock().await;
                if let Err(e) = inj.inject_touch(&snapshot) {
                    eprintln!("[read_loop] touch inject failed: {e:?}");
                }
            }
            MSG_TIME_SYNC_REQ => {
                let req = TimeSyncReq::decode(&payload)?;
                let pc_t2_ns = session_start.elapsed().as_nanos() as i64;
                // Measure t3 immediately before write so it's tighter.
                let pc_t3_ns = session_start.elapsed().as_nanos() as i64;
                let resp = TimeSyncResp {
                    android_t1_ns: req.android_t1_ns,
                    pc_t2_ns,
                    pc_t3_ns,
                };
                let mut w = writer.lock().await;
                if write_frame(&mut *w, MSG_TIME_SYNC_RESP, &resp.encode()).await.is_err() {
                    return Ok(());
                }
                let _ = w.flush().await;
            }
            MSG_REQUEST_IDR => {
                // Forward to run() which holds the engine.
                let _ = idr_tx.send(());
            }
            MSG_ANDROID_GOODBYE => {
                eprintln!("[read_loop] android sent goodbye");
                return Ok(());
            }
            other => {
                eprintln!(
                    "[read_loop] unhandled msg 0x{other:02x} len={}",
                    payload.len()
                );
            }
        }
    }
}
