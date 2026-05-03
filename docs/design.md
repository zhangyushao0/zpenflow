# Penflow v1.0 — Master Design Spec

**Status:** Draft (awaiting user review)
**Date:** 2026-05-03
**Branch:** `v1-rust`
**Supersedes:**
- `2026-05-02-wave-1-foundation-design.md` (Wave 1 architecture — still authoritative for the Cargo workspace skeleton, but its encoder/transport details are restated here)
- `2026-05-02-wave-2-rust-core-port-design.md` (revised 2026-05-03 to MF, but this document is the new source of truth)

This spec synthesizes the original v1.0 plan with findings from a deep-read of four reference projects:
- [Sunshine](https://github.com/LizardByte/Sunshine) — PC capture + encode (the standard for Windows desktop streaming)
- [moonlight-android](https://github.com/moonlight-stream/moonlight-android) — Android MediaCodec decode + frame pacing
- [scrcpy](https://github.com/Genymobile/scrcpy) — ADB tunnel + control protocol
- [OpenTabletDriver](https://github.com/OpenTabletDriver/OpenTabletDriver) — Windows pen-input abstractions (Penflow already exceeds OTD on Windows pen, but its data model is clean)

Source clones live in `research/` (gitignored). Specific files referenced inline below.

---

## 1. Goal

Produce v1.0 of Penflow: a cross-platform PC → Android pen-display bridge with:

- **Latency**: ≤ 20 ms e2e on the user's reference setup (RTX 5070 + Wacom MovinkPad Pro 14, currently ~20 ms with the C++ build).
- **GPU vendor coverage on Windows**: NVIDIA + AMD + Intel + software fallback. Today: NVIDIA-only.
- **OS coverage**: Windows now, **macOS post-v1.0** (architecture must accommodate; no shipping macOS in v1.0).
- **Installer**: WiX MSI, ≤ 30 MB total. No bundled FFmpeg / GStreamer.
- **First-class Krita support**: Windows Ink with pressure / tilt / 3 buttons / hover / eraser, plus multi-touch pan/zoom.
- **Robustness**: handle codec hangs, surface destruction, ADB tunnel drops, monitor sleep cycle, GPU reset, and clean shutdowns gracefully.

## 2. Non-goals (v1.0)

- macOS shipping binary. (Architecture supports it; Wave for it lands post-v1.0.)
- Linux support.
- Predictive ink overlay (deferred indefinitely; current path matches SuperDisplay without it).
- Audio.
- Wireless ADB / TCP ADB (deferred; USB only).
- Multi-client (one Android device per server instance).
- Reference-frame invalidation (RFI) — Sunshine uses it but it requires encoder cooperation we don't get from MF. We rely on aggressive intra-refresh + IDR-on-demand instead. See §6.4.

## 3. Prior Art and Positioning

Three reference projects directly inform the design; one is a cautionary tale.

| Project | What we borrow | What we don't |
|---|---|---|
| **Sunshine** | Encoder abstraction shape, DXGI low-latency tricks (`SetMaximumFrameLatency(1)`, GPU thread priority, monitor-sleep keepalive, frame-pacing groups). | Their NVENC direct path (we use MF instead), their ~1000 LOC HLSL shaders for BGRA→NV12 (we use D3D11 VideoProcessor), their RFI machinery, their FFmpeg dependency for AMF/QSV/MF. |
| **moonlight-android** | Vendor-key probe ladder, codec recovery escalation ladder, decoder-hung watchdog, surface-destroyed mid-stream handling, MIN_LATENCY frame pacing. | Synchronous MediaCodec dequeue loop (we keep async), RFI integration, game-specific frame pacing modes, their HUD (ours is better). |
| **scrcpy** | 1-byte readiness probe before declaring tunnel connected, single-socket duplex with separate read/write threads, verbose protocol-trace logging pattern. | Three-socket multiplex, `app_process` JAR bootstrap, no-length-prefix raw stream framing, TCP/IP fallback. |
| **OpenTabletDriver** (cautionary) | `SetProcessDpiAwareness(2)`, `Matrix3x2` input-area transform, `IStateBinding`-style enum binding model, eraser flip-then-flush protocol, tip-pressure threshold. | Everything else — OTD's Windows backend is `SendInput` mouse-only with no pen semantics. We already exceed it via WinRT `InputInjector`. |

**Penflow's novelty** is the specific combination: PC → Android low-latency screen + Wacom-grade Windows Ink injection + ADB tunnel + multi-touch back-channel + clean cross-vendor encode. No project ships this composition. The closest analog ([Weylus](https://github.com/H-M-H/Weylus)) does not deliver Windows pen pressure.

## 4. Top-Level Architecture

```
┌─────────────────────────── PC (Windows) ───────────────────────────┐
│                                                                    │
│  ┌─────────────────┐    ┌─────────────────┐                        │
│  │  Tauri GUI      │    │  Engine (lib)   │                        │
│  │  (penflow-gui)  │◄──►│  penflow-core   │                        │
│  └─────────────────┘    │                 │                        │
│                         │  Capturer ──┐   │                        │
│                         │             ▼   │                        │
│                         │  Encoder (trait)│  ◄─── platform backend │
│                         │             │   │     mf | (videotoolbox)│
│                         │             ▼   │                        │
│                         │  PacketQueue    │                        │
│                         │             │   │                        │
│                         │  Pen Inject ◄───┼──┐                     │
│                         │  Touch Inject◄──┼──┤                     │
│                         └─────────────────┘  │                     │
│                                  │           │                     │
│                                  ▼           │                     │
│                         ┌──────────────────┐ │                     │
│                         │ penflow-server   │ │                     │
│                         │ (orchestrator)   │ │                     │
│                         └──────────┬───────┘ │                     │
│                                    │         │                     │
│                                    ▼         │                     │
│                         ┌──────────────────┐ │                     │
│                         │ Transport (trait)│ │                     │
│                         │  adb_localabstr. │ │                     │
│                         │  (raw_usb later) │ │                     │
│                         └──────────┬───────┘ │                     │
└────────────────────────────────────┼─────────┼─────────────────────┘
                                     │         │
                            ADB reverse tunnel │
                                     │         │
┌────────────────────────────────────┼─────────┼─────────────────────┐
│  Android client (Kotlin)           │         │                     │
│                                    ▼         │                     │
│  ┌─────────────────┐    ┌─────────────────┐  │                     │
│  │ Pen Capture     ├───►│ PenflowClient   │──┘ (pen+touch back)    │
│  │ Touch Capture   ├───►│ (sender thread) │                        │
│  └─────────────────┘    │                 │                        │
│                         │ VideoDecoder   │ ──► SurfaceView         │
│                         │ (MediaCodec)    │                        │
│                         │                 │                        │
│                         │ TimeSync (NTP)  │                        │
│                         │ Recovery ladder │                        │
│                         └─────────────────┘                        │
└────────────────────────────────────────────────────────────────────┘
```

## 5. Cargo Workspace Layout (revised from Wave 1)

```
penflow/
├── Cargo.toml                        # workspace
├── crates/
│   ├── penflow-protocol/             # wire-format types and codecs
│   ├── penflow-transport/            # Transport trait + adb_local_abstract
│   ├── penflow-core/                 # capture + encode + inject engine
│   │   └── src/
│   │       ├── lib.rs                # public API: Engine, Config
│   │       ├── error.rs
│   │       ├── d3d11.rs              # Windows: D3D11 device + DXGI factory
│   │       ├── monitors.rs           # Windows: DXGI output enumeration
│   │       ├── capture/              # Capture trait + platform impls
│   │       │   ├── mod.rs            # trait Capturer
│   │       │   └── dxgi.rs           # Windows: DXGI Output Duplication
│   │       │   # screencapturekit.rs # macOS: post-v1.0
│   │       ├── encoder/              # Encoder trait + platform/vendor impls
│   │       │   ├── mod.rs            # trait Encoder, EncodeSession; registry/probe
│   │       │   └── mf.rs             # Windows: Media Foundation HEVC MFT
│   │       │   # videotoolbox.rs     # macOS: post-v1.0
│   │       ├── color.rs              # BGRA → NV12 via D3D11 VideoProcessor
│   │       ├── inject/               # Pen + touch injection
│   │       │   ├── mod.rs            # trait PenInjector, TouchInjector
│   │       │   ├── win_ink.rs        # WinRT InputInjector wrapper (pen)
│   │       │   ├── win_touch.rs      # Win32 InjectTouchInput wrapper (touch)
│   │       │   └── coords.rs         # Matrix3x2-style input→output transform
│   │       ├── binding.rs            # Pen-button binding model (OTD-inspired)
│   │       ├── packet_queue.rs       # Mutex<VecDeque> + Condvar SPSC
│   │       └── pipeline.rs           # capture+encode thread + keepalive
│   ├── penflow-server/               # tokio session orchestrator
│   └── penflow-gui/                  # Tauri 2.x app (apps/penflow-gui/src-tauri)
└── apps/penflow-gui/                 # Tauri shell (already in place from Wave 1)
```

Differences from the original Wave 2 layout:
- New `capture/` subdir to make per-platform plug-in obvious.
- New `encoder/` subdir hosts `trait Encoder` + `mf.rs` (and future `videotoolbox.rs`).
- New `color.rs` for the BGRA→NV12 step (separated from encoder).
- New `inject/` subdir splits pen vs touch injection cleanly; `coords.rs` factors out the transform.
- New `binding.rs` for the OTD-inspired button-binding data model.
- `nvenc/` directory **does not exist** (deleted from Wave 2 work).

## 6. PC Engine (`penflow-core`)

### 6.1 Capture Layer

`trait Capturer` exposes:

```rust
pub trait Capturer {
    fn output_size(&self) -> (u32, u32);
    fn acquire_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, EngineError>;
}

pub struct CapturedFrame {
    pub texture: PlatformTexture,   // ID3D11Texture2D on Windows; CVPixelBuffer on macOS later
    pub captured_at: Instant,       // local high-precision clock
    pub size: (u32, u32),
}
```

Windows impl: `dxgi::DxgiCapturer` wraps `IDXGIOutputDuplication` (`IDXGIOutput5::DuplicateOutput1` preferred, with format preference `[NV12, BGRA]`; falls back to `IDXGIOutput1::DuplicateOutput`). Adopts these tricks from Sunshine `display_base.cpp`:

- `SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED)` per session — without this, idle desktop sleeps, AcquireNextFrame returns `DXGI_ERROR_ACCESS_LOST`, reinit wakes the monitor → infinite cycle. (Sunshine `display_base.cpp:239`.)
- `IDXGIDevice1::SetMaximumFrameLatency(1)` to keep the swap chain queue at depth 1.
- `IDXGIDevice::SetGPUThreadPriority(7)` for capture-side priority.
- `D3DKMTSetProcessSchedulingPriorityClass(REALTIME)` (HIGH on NVIDIA + HAGS to avoid documented driver crashes).
- 200 ms `AcquireNextFrame` timeout → 10 ms sleep without holding the D3D11 device lock so the encoder thread can grab it (Sunshine `display_base.cpp:317`).
- Transparent reinit on `DXGI_ERROR_ACCESS_LOST` and `DXGI_ERROR_ACCESS_DENIED` (already in Wave 2).

We **do not** copy:
- The MinHook on `NtGdiDdDDIGetCachedHybridQueryValue` (Sunshine `display_base.cpp:428`). It exists only for hybrid-GPU laptops; Penflow's deployment is a desktop with a discrete GPU.
- The WGC fallback (lower latency loss for Penflow's case is not worth the complexity).

### 6.2 Color Conversion (`color.rs`)

DXGI gives BGRA. MF HEVC encoders prefer NV12. We **do not** roll HLSL shaders (Sunshine's path is ~1000 LOC of HLSL+C++); we use **D3D11 VideoProcessor** (`ID3D11VideoProcessor`, `IDXGIVideoProcessorEnumerator`, `ID3D11VideoContext::VideoProcessorBlt`). This is a Microsoft-supplied, driver-accelerated path that does BGRA→NV12 in 0.1–0.3 ms with full colour-matrix control (BT.709, full range).

Output side configuration:
- `D3D11_VIDEO_PROCESSOR_OUTPUT_RATE` = `NORMAL`
- Color space = `DXGI_COLOR_SPACE_YCBCR_FULL_G22_NONE_P709` (full-range BT.709)

If a backend turns out to accept BGRA directly (some MF encoders do, e.g. ARGB32 input format), we skip the converter for that path. Probe at session init.

### 6.3 Encoder Abstraction (Sunshine-inspired)

Two-level trait, mirroring Sunshine's `encoder_t` / `encode_session_t` separation (`src/video.h:125-220`):

```rust
pub trait EncoderBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_codecs(&self) -> &[Codec];
    fn supported_input_formats(&self) -> &[PixelFormat];
    /// Live-probe: try to instantiate a session. Return error if unavailable.
    fn make_session(
        &self,
        device: &D3d11Device,
        cfg: SessionConfig,
    ) -> Result<Box<dyn EncodeSession>, EngineError>;
}

pub trait EncodeSession: Send {
    fn input_format(&self) -> PixelFormat;
    fn submit_frame(&mut self, tex: &PlatformTexture, pts_ns: i64, force_idr: bool) -> Result<(), EngineError>;
    fn poll_packet(&mut self) -> Result<Option<EncodedPacket>, EngineError>;
    fn sequence_header(&self) -> Vec<u8>;
    fn request_idr(&mut self);
}
```

Backend probe order (Windows): `mf` first; future hooks reserve room for `videotoolbox` on macOS. **No FFmpeg fallback in v1.0** — if MF fails to find any hardware encoder, we report a hard error rather than ship a 30 MB fallback for an unlikely case.

Probe is a **live test**, not just enumeration: we call `make_session` with a tiny config, encode one black frame, and only then mark the backend `passed`. Sunshine does this (`probe_encoders`, `src/video.cpp:2793`) for the same reason — driver capability strings lie.

### 6.4 Windows Encoder: Media Foundation HEVC MFT

`encoder/mf.rs`. Key API surfaces (all via `windows = { features = ["Win32_Media_MediaFoundation", ...] }`):

- `MFStartup(MF_VERSION, MFSTARTUP_FULL)` once at engine init; `MFShutdown` at teardown.
- `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, ...)` filtered by `MFVideoFormat_HEVC` output type. Returns activation pointers ranked by Windows; first one is typically NVIDIA on RTX systems, AMD on Radeon, QSV on Intel iGPU.
- `IMFActivate::ActivateObject` to instantiate.
- `IMFTransform::SetOutputType` first (HEVC, frame size, frame rate, bitrate), then `SetInputType` (NV12 or ARGB32 depending on `supported_input_formats` probe).
- Attributes: `MF_LOW_LATENCY = TRUE`, `MF_TRANSFORM_ASYNC_UNLOCK = TRUE` for async hardware MFTs.
- Codec API attributes via `ICodecAPI`:
  - `CODECAPI_AVEncCommonRateControlMode = eAVEncCommonRateControlMode_CBR`
  - `CODECAPI_AVEncCommonMeanBitRate` (50 Mbps default)
  - `CODECAPI_AVEncCommonLowLatency = VARIANT_TRUE`
  - `CODECAPI_AVEncMPVDefaultBPictureCount = 0`
  - `CODECAPI_AVEncH264CABACEnable / CABAC` → not applicable to HEVC
  - `CODECAPI_AVEncVideoForceKeyFrame` for on-demand IDR (see §6.4.1)
  - `CODECAPI_AVEncVideoIntraRefreshMode`, `CODECAPI_AVEncVideoIntraRefreshPeriod`, `CODECAPI_AVEncVideoIntraRefreshFrameCount` — set best-effort; if the driver rejects them we accept periodic IDR fallback.
- VUI / colour metadata via media-type attributes:
  - `MF_MT_VIDEO_NOMINAL_RANGE = MFNominalRange_0_255` (full range)
  - `MF_MT_VIDEO_PRIMARIES = MFVideoPrimaries_BT709`
  - `MF_MT_TRANSFER_FUNCTION = MFVideoTransFunc_709`
  - `MF_MT_YUV_MATRIX = MFVideoTransferMatrix_BT709`
- D3D11 binding via `MFCreateDXGIDeviceManager` + `IMFTransform::ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, ...)`. Captured textures are wrapped in `IMFSample` via `MFCreateDXGISurfaceBuffer + MFCreateSample`. Zero-copy GPU path.
- Encode loop is sync-style for the caller (`submit_frame` then `poll_packet`) but uses MF's async-MFT event model internally: `IMFMediaEventGenerator::GetEvent` for `METransformNeedInput` / `METransformHaveOutput` events, drained between submit/poll calls.

#### 6.4.1 On-demand IDR — the open question

Sunshine reports (`src/video.cpp:886`) that **FFmpeg's `hevc_mf` codec wrapper** sets `FIXED_GOP_SIZE` because it cannot do on-demand IDR. This may or may not be a limitation of MF itself or only of FFmpeg's wrapping. Microsoft's documentation lists `CODECAPI_AVEncVideoForceKeyFrame` as supported on the Windows H.264/HEVC encoder MFTs.

**Test-before-depend gate**: before committing to MF as the sole Windows path, verify `CODECAPI_AVEncVideoForceKeyFrame` actually produces an IDR within the next frame on at least NVIDIA + AMD + Intel MFTs. If it works, we keep the design as written. If it doesn't, we accept periodic IDR (every N seconds) and add an "on connect" forced reset; the worst case is the cold-start cost a connecting client pays for one IDR period.

This gate runs **once during Wave 2 implementation**, before locking the design.

### 6.5 macOS Encoder Hook (post-v1.0)

`encoder/videotoolbox.rs` will use `objc2-video-toolbox` (madsmtm/objc2 ecosystem; production-used by shiguredo/video-toolbox-rs). Key configuration:

- `kVTVideoEncoderSpecification_EnableLowLatencyRateControl = true` (WWDC21 low-latency mode; enforces no frame reordering, sync-style API, periodic IDR controllable).
- `kVTCompressionPropertyKey_RealTime = true`.
- `kVTCompressionPropertyKey_ProfileLevel = HEVC_Main_AutoLevel`.
- `kVTCompressionPropertyKey_AverageBitRate`, `kVTCompressionPropertyKey_MaxKeyFrameInterval = i32::MAX` with periodic forced IDR via `kVTEncodeFrameOptionKey_ForceKeyFrame`.

Capture side (post-v1.0): `ScreenCaptureKit` via `objc2-screen-capture-kit`. Output is `CMSampleBuffer` containing a `CVPixelBuffer` — VT consumes it directly, zero-copy.

This shape matches the Windows side intentionally so the engine API stays uniform. Wave for this lands after the Windows v1.0 ships.

### 6.6 Pen + Touch Injection

Penflow's WinRT `InputInjector`-based pen injection already exceeds OpenTabletDriver's Windows backend. From OTD we adopt only:

- `SetProcessDpiAwareness(2)` (`PROCESS_PER_MONITOR_DPI_AWARE`) at startup so virtual-screen geometry is in physical pixels.
- A `Matrix3x2`-style input-area → output-area transform (replace the current naive `left + norm * width`):
  ```
  raw_normalized → input-area centered → rotate (if needed) → scale → translate → virtual-screen pixel
  ```
  Implemented as a single `nalgebra::Matrix3` so future "rotate the tablet 90°" is one parameter.
- `IStateBinding`-style enum binding model:
  ```rust
  pub enum Binding {
      None,
      KeyTap(VirtualKey),
      KeyHold(VirtualKey),
      KeyChord(Vec<VirtualKey>),
      MouseButton(MouseButton),
      EraserToggle,
  }
  ```
  Configured per pen-button slot (slot 0 = barrel button 1, slot 1 = barrel 2, slot 2 = third / `BUTTON_TERTIARY`). Today's hardcoded `Ctrl/Shift/E` mapping becomes a default profile, not a hardcoded behaviour.
- Tip pressure threshold (`tip_threshold: f32`, default `0.0`) so accidental near-hover noise doesn't draw.
- Eraser tool flip-then-flush: when switching `INVERTED` bit, emit one `POINTER_UP` / out-of-range frame first to avoid drivers seeing both bits.

Touch injection (Win32 `InitializeTouchInjection` + `InjectTouchInput`) keeps its current snapshot-diff approach (Penflow already has this; OTD has nothing comparable on Windows).

## 7. Wire Protocol (`penflow-protocol`)

### 7.1 Framing (unchanged)

`[u8 msg_id][u32 BE length][payload of `length` bytes]`. Already better than scrcpy's no-length-prefix raw scheme: an unknown message is a single `read_exact` skip, not a parser-desync recovery problem.

### 7.2 Message catalogue (unchanged from current `protocol.py`)

| ID | Name | Direction | Purpose |
|---|---|---|---|
| 0x01 | `HELLO_PC` | server → client | Stream params (w, h, fps, codec) |
| 0x02 | `VIDEO_CONFIG` | server → client | csd-0 (VPS+SPS+PPS for HEVC) |
| 0x03 | `VIDEO_FRAME` | server → client | Encoded NAL units + timing |
| 0x04 | `BRUSH_HINT` | server → client | (reserved, post-v1.0) |
| 0x05 | `TELEMETRY` | server → client | Server-side timing samples |
| 0x06 | `TIME_SYNC_RESP` | server → client | NTP-style reply |
| 0x7F | `PC_GOODBYE` | server → client | Clean shutdown notice |
| 0x81 | `HELLO_ANDROID` | client → server | Device caps |
| 0x82 | `PEN_EVENT` | client → server | Pen sample |
| 0x83 | `TOUCH_EVENT` | client → server | Multi-finger snapshot |
| 0x84 | `TIME_SYNC_REQ` | client → server | NTP-style request |
| 0xFF | `ANDROID_GOODBYE` | client → server | Clean shutdown notice |

Direction encoding by high bit (already in place): server→client < 0x80; client→server ≥ 0x80.

### 7.3 Adopted improvements from scrcpy

- **1-byte readiness probe**: after the Android client connects to the local-abstract socket and before the server sends `HELLO_PC`, both sides exchange a 1-byte `0xA5` "ready" marker. Fixes the false-positive where the ADB tunnel accepts a TCP connection while the Android app is still initializing (`scrcpy/app/src/server.c:467`).
- **Verbose protocol trace**: a single function `log_msg(direction, type, len, &payload)` gated by `PENFLOW_PROTO_TRACE=1` env var. Hex-dumps every framed message with wall-clock timestamp and direction arrow. ~20 lines, invaluable when debugging timing issues.

### 7.4 Considered but deferred

- **u16 fixed-point pressure** (scrcpy `INJECT_TOUCH_EVENT`). Would save 2 bytes/sample. Penflow's float stays for v1.0 — the ~250 Hz pen rate × 2 bytes is negligible (500 B/s saved); not worth the mental tax.
- **Three-socket multiplex** (scrcpy video+audio+control). Overkill for our framed single-stream design.
- **TCP/IP ADB fallback**. USB-only for v1.0.

## 8. Transport Layer (`penflow-transport`)

`trait Transport` from Wave 1 stands. Implementations:

- v1.0: `AdbLocalAbstractTransport` — listen on a TCP port the ADB daemon `reverse`-tunnels to `localabstract:penflow` on the device. Single tokio `TcpListener` accepts; gives separate `AsyncRead` + `AsyncWrite` halves to the orchestrator.
- post-v1.0: `RawUsbTransport` (Android USB Accessory mode). New file in `penflow-transport`; orchestrator unchanged.

### 8.1 Single-socket duplex model (scrcpy-confirmed)

scrcpy's three-socket design exists only because their video+audio streams are raw codec bytes without length framing — they **need** separate sockets. Our framed messages on one socket are correct. We keep:
- One dedicated tokio `read_loop` task that decodes inbound frames and dispatches by `msg_id`.
- One dedicated `write_loop` task with a `tokio::sync::mpsc::Receiver` of outbound `Frame { msg_id, payload }` items. Single writer prevents interleaving.
- No mutex on the socket fd. Each direction owns its half.

### 8.2 Bootstrap sequence (scrcpy-inspired)

1. `adb start-server`.
2. `adb reverse localabstract:penflow tcp:<server_port>`.
3. PC server starts listening on `<server_port>`.
4. Android app launched (manually for now; later we may pass intent via `adb shell am start`).
5. Android opens `LocalSocket(LocalSocketAddress("penflow", ABSTRACT))`.
6. **1-byte ready exchange**: Android writes `0xA5`, then waits for `0xA5` back from PC. PC writes `0xA5` after accept().
7. Standard `HELLO_ANDROID` / `HELLO_PC` handshake.
8. `VIDEO_CONFIG`, then steady-state.

Connect retry: PC server retries `accept()` for 10 seconds (200 ms × 50) before reporting failure.

## 9. Server (`penflow-server`)

Tokio-based session orchestrator. Single `Session` struct owns:
- `Box<dyn Transport>` — accepted from configured transport.
- `Engine` (from `penflow-core`) — owns capture+encode thread.
- `PenInjector`, `TouchInjector`.
- `TimeSync` state.

Session loop pattern:
```
spawn read_loop(reader) ──► dispatch by msg_id
spawn write_loop(writer) ◄── mpsc<Frame>
spawn frame_pump:
    loop:
        pkt = engine.next_packet(timeout)
        send Frame::VideoFrame(pkt) on mpsc
spawn telemetry_pump:
    every 1 s: send Frame::Telemetry(...)

select! on:
    HELLO_ANDROID  → respond HELLO_PC + VIDEO_CONFIG, store device caps
    PEN_EVENT      → pen_injector.inject(...)
    TOUCH_EVENT    → touch_injector.inject(...)
    TIME_SYNC_REQ  → respond TIME_SYNC_RESP
    ANDROID_GOODBYE → break, clean shutdown
```

Errors propagate as `SessionError`; on any irrecoverable error the orchestrator tears down both the engine and the transport, and reports state up to the GUI.

## 10. Android Client

Existing Kotlin code in `android/` is mostly correct; below are the changes informed by moonlight-android's deep read.

### 10.1 MediaCodec async vs sync

**Keep async** (`MediaCodec.Callback`). Moonlight uses sync because they need fine control over frame-pacing modes for game streaming; Penflow's MIN_LATENCY behaviour is naturally expressible in async. Async also avoids the dedicated dequeue thread.

### 10.2 Vendor key changes (must-fix)

`VideoDecoder.kt` today sets `KEY_OPERATING_RATE = 240` and `KEY_PRIORITY = 0` together unconditionally. moonlight-android (`MediaCodecHelper.java:482-491`) explicitly notes **this combination crashes Adreno 620** (Snapdragon 765G). Fix:

```
on Qualcomm chips:
    KEY_OPERATING_RATE = Short.MAX_VALUE.toInt()  // 32767, moonlight's value
    do NOT set KEY_PRIORITY=0
otherwise:
    KEY_PRIORITY = 0
    do NOT set KEY_OPERATING_RATE
```

Also add (Qualcomm only, API ≥ 26):
- `vendor.qti-ext-dec-picture-order.enable = 1` — disables HEVC reorder buffering (moonlight finding; saves 5–10 ms).

Other vendor keys to add unconditionally (silently ignored on wrong vendor):
- `vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req = 1` (Kirin)
- `vendor.rtc-ext-dec-low-latency.enable = 1` (Samsung Exynos)
- `vendor.low-latency.enable = 1` (Amlogic)

API ≥ 31: probe `getSupportedVendorParameters()` and only set the ones the codec advertises.

### 10.3 Codec recovery ladder

Today's `VideoDecoder.kt` `onError` only logs. Adopt moonlight's 4-level escalation (`MediaCodecDecoderRenderer.java:714`):

```kotlin
enum class RecoveryLevel { Flush, Restart, Reset, Reinit }

private fun recover() {
    when (recoveryLevel) {
        Flush   -> codec.flush(); codec.start()
        Restart -> codec.stop(); codec.configure(...); codec.start()
        Reset   -> codec.reset(); codec.configure(...); codec.start()
        Reinit  -> codec.release(); codec = createCodec(); codec.configure(...); codec.start()
    }
    recoveryLevel = recoveryLevel.next()
    requestIdrFromServer()
}
```

After `Reinit` fails, escalate to disconnect + bubble error to UI.

### 10.4 Decoder-hung watchdog

5-second timeout on `dequeueInputBuffer` (or in async-mode equivalent: time-since-last-`onInputBufferAvailable`). On expiry, treat as decoder hung → trigger recovery ladder.

### 10.5 Surface-destroyed during streaming

Currently `waitForSurface()` only spin-waits at startup. Add `SurfaceHolder.Callback.surfaceDestroyed` handler that:
- sets `stopping = true`
- interrupts any blocking codec calls
- triggers a clean disconnect

### 10.6 Frame pacing — MIN_LATENCY mode

Replace `releaseOutputBuffer(index, true)` with moonlight's MIN_LATENCY pattern:

```kotlin
override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: BufferInfo) {
    // drain any further output buffers; keep only newest, drop the rest
    while (true) {
        val nextIdx = codec.dequeueOutputBuffer(scratchInfo, 0L)
        if (nextIdx < 0) break
        codec.releaseOutputBuffer(index, false)  // drop previous late frame
        index = nextIdx
        info = scratchInfo
    }
    codec.releaseOutputBuffer(index, System.nanoTime())  // wall-clock PTS = SurfaceFlinger drop policy
}
```

Passing `System.nanoTime()` as the render PTS lets SurfaceFlinger drop the frame if a newer one arrives within the same vsync — exactly what we want for a pen display under load.

### 10.7 Pen capture polish

- Add a **spatial dead zone** (~5 px? configurable) after `ACTION_UP` before accepting new `HOVER`/`DOWN` to prevent the well-known double-click artefact (moonlight `Game.java:2147`).
- Iterate **all pointers** in `MOVE` events rather than only pointer 0. (Penflow targets only the MovinkPad which has a single pen, but the cost is trivial and future-proofs against dual-pen devices.)

### 10.8 HUD

Penflow's `HudView.kt` is more sophisticated than moonlight's perf overlay (true e2e via TimeSync, p99, Choreographer-driven refresh). **Keep ours.** Optionally add a "host processing latency" segment (separate `cap_us` and `enc_us` from `MSG_VIDEO_FRAME`'s extended fields, already wired) to break down the PC-side delay further.

### 10.9 Per-vsync display estimate

Today: `displayedNs = decodedNs + 8_333_333L` (fixed 1 vsync at 120 Hz). Moonlight subtracts the actual `getAppVsyncOffsetNanos()`. **Replace the constant with a measured value** at startup; falls back to `8_333_333` if the API returns zero.

## 11. GUI (`penflow-gui`)

Tauri 2.x scaffold from Wave 1 stands. Wave 4 fills in:

- Monitor picker (uses `Engine::list_monitors()`).
- Codec / bitrate sliders.
- Pen-button binding UI (renders the new `Binding` enum from §6.6 as dropdowns).
- Side-button binding UI maps to `binding.rs`'s data model.
- Dev HUD toggle.
- Status / connection state.
- Per-vendor encoder picker (advanced; default = first probe-validated backend).

In-process: GUI calls `Engine` and `Session` directly via Rust; no IPC. Settings persisted via Tauri's plugin-store or a JSON file under `%APPDATA%/penflow/`.

## 12. Installer (`installer/`, Wave 5)

WiX MSI:
- Bundles `penflow-gui.exe` (~10–15 MB Tauri).
- Bundles signed VDD driver files into `tools/vdd/`.
- First-run installs the VDD driver via `pnputil` if not already present.
- Total target: **≤ 30 MB**.
- Code-sign with the user's certificate (manual ceremony for v1.0 release).

VDD on-demand monitor lifecycle (from Wave 1 spec, retained):
- PC starts → no virtual monitor.
- Android client connects → server commands VDD over named pipe to plug in a monitor with the negotiated EDID.
- Android disconnects / GUI quits → unplug.

Pinned VDD fork: lock to a specific commit of [itsmikethetech/Virtual-Display-Driver](https://github.com/itsmikethetech/Virtual-Display-Driver) whose IPC supports runtime plug/unplug (verify before locking; that fork's IPC has changed historically).

## 13. CI

`.github/workflows/ci.yml` (already in place from Wave 1):
- `windows-latest`
- `cargo build --workspace` + `clippy --all-targets -D warnings` + `fmt --check` + `cargo test --workspace`
- **No LLVM install** (we have no `bindgen` build step in the MF design).
- Hardware-dependent integration tests stay `#[ignore]`d.

Wave 6 adds release pipeline: tag → MSI artifact, tag → Android APK artifact, attach to GitHub Release.

## 14. Branching

- `main`: Python + C++ working build. Untouched until Wave 6.
- `v1-rust`: all v1.0 work. Wave 6 final step merges back, tag `v1.0.0`.

## 15. Risks and Mitigations

| Risk | Mitigation |
|---|---|
| MF's `CODECAPI_AVEncVideoForceKeyFrame` doesn't actually deliver IDR-on-demand on AMD/Intel MFTs | Explicit Wave-2 gate test before locking design (§6.4.1). Fallback: periodic IDR + on-connect reset. |
| Per-vendor MFT bugs (Intel iGPU known-buggy on certain Intel UHD generations) | `EncoderBackend::make_session` does live black-frame probe; Tauri GUI lets the user pick a non-default backend. Software fallback (`hevc_amf` is technically AMD-only but Microsoft ships a software MFT — try it last). |
| Adreno 620 crash from `KEY_OPERATING_RATE + KEY_PRIORITY=0` (latent today) | §10.2 fix lands in Android client first thing in Wave 3. |
| VDD upstream IPC changes break our plug/unplug | Vendored at a pinned commit in Wave 5. Decision to fork or contribute deferred. |
| WiX MSI total size exceeds 30 MB target | Strip Tauri to minimum features (no devtools in release builds). VDD bundle only the signed binaries needed (no source). |
| ADB `reverse` denied on locked-down corporate machines | Document the requirement; out of scope to work around for v1.0. |

## 16. Roadmap (revised)

| Wave | Scope | Status |
|---|---|---|
| 1 | Cargo workspace + Tauri skeleton + Transport trait + CI | ✅ done |
| 2 | `penflow-core`: capture + encoder (MF) + inject. Includes the §6.4.1 IDR gate test. | revised, in progress |
| 3 | `penflow-server`: tokio session orchestrator + protocol round-trip. Android-side fixes from §10.2–10.6. | next |
| 4 | Tauri GUI features (monitor picker, codec/bitrate, button binding UI, HUD toggle). | future |
| 5 | WiX MSI + bundled VDD + on-demand monitor lifecycle. | future |
| 6 | Release pipeline + English docs polish + merge to `main`, tag `v1.0.0`. | future |
| post-v1.0 | macOS port: VideoToolbox + ScreenCaptureKit + CGEvent inject. | future |
| post-v1.0 | Predictive ink overlay (if measurements show it's needed; SuperDisplay-class latency may be enough). | future |

## 17. Open Questions

1. **§6.4.1 IDR gate**: confirmed at Wave 2 implementation time, not now.
2. **macOS pen injection target**: WWDC has `CGEvent` for mouse / pointing; pressure injection on macOS is historically thinner than Windows Ink. Defer concrete API choice to the macOS wave.
3. **Multi-monitor source on PC**: today the user picks one monitor index. A future wave could let the GUI live-switch source monitor mid-session — orthogonal to v1.0.
4. **Configurable pressure curve**: out for v1.0; identity mapping + tip threshold is sufficient for the current setup. Can be added in `binding.rs` post-ship.
5. **Crash reporting / telemetry**: no opt-in crash reporter in v1.0. Local logs only.

## 18. Acceptance Gate (v1.0)

Cumulative across all waves:

- Tauri GUI launches, picks monitor, picks codec, shows status.
- Android APK installs, opens, connects via ADB reverse.
- 1-byte readiness probe + handshake completes within 1 s of connect.
- Live capture+encode+decode pipeline runs at 60 fps, e2e ≤ 25 ms (p50), ≤ 35 ms (p99) on the user's reference rig (RTX 5070 + MovinkPad Pro 14).
- Pen pressure / tilt / 3 buttons / hover / eraser all visible in Krita Windows Ink mode.
- Multi-touch pan/zoom works in Krita.
- VDD virtual monitor appears at session start, disappears at session end.
- MSI installer ≤ 30 MB.
- Works on at least one machine with each of: NVIDIA GPU, AMD GPU, Intel iGPU.
- Tests on `windows-latest` CI green.
- Clean disconnect + reconnect cycle works without a server restart.
