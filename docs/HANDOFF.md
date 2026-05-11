# Handoff — read this before touching anything

You are picking up the **zpenflow** project mid-stream from a previous agent. This document captures the tribal knowledge accumulated through ~6 weeks of prototyping in the predecessor project (`C:\repo\krita\penflow\`, kept on disk as an archive) plus a focused research dive into reference open-source projects. The architectural design lives in [`design.md`](design.md) (574 lines, authoritative) — **read that first**. This document covers everything that *isn't* in the design: experience, baselines, pitfalls, what to do first.

The goal: **don't re-make mistakes that were already made**. There are several non-obvious traps below; each was discovered the painful way.

---

## 0. Orientation

- **Product**: PC → Android pen-display bridge. Drives a Wacom MovinkPad Pro 14 (or any Android tablet) as a Wintab/Windows-Ink pen display for Krita-style drawing on Windows.
- **Predecessor project**: `C:\repo\krita\penflow\` — Python server + C++ NVENC engine + Kotlin Android client. **Working build, ~20 ms e2e latency**. Kept on disk as reference; do not delete.
- **Current project (this one)**: `C:\repo\krita\zpenflow\` — clean Rust + Tauri rewrite, fresh git repo on `main` (initial commit `f2c9d63`).
- **What's already done**: empty Cargo workspace skeleton (4 crates), Tauri 2.x window scaffold, Android client copied from predecessor (mature; needs targeted polish), CI workflow, [`design.md`](design.md) with full architecture.
- **What's not done**: literally everything in `penflow-core` (capture/encode/inject) and `penflow-server` (session orchestrator). Engine implementation is the next milestone.

The hardware target the user develops on:
- **PC**: Windows 11 Pro 26200, RTX 5070 (NVENC HEVC P1 ULL ~2-3 ms/frame on RTX 5070)
- **Tablet**: Wacom MovinkPad Pro 14 (DTHA140) running Android 15 on Snapdragon 8s Gen 3 (Adreno 720), 2880×1800 panel
- **Connection**: USB-C with `adb reverse localabstract:penflow`

Tested on this rig only. AMD/Intel paths are designed-for but not yet exercised.

---

## 1. Research findings

Four reference open-source projects were cloned and read in depth. Clones are **not in this repo** (gitignored under `research/`); re-clone if you want to read them:

```powershell
mkdir research; cd research
git clone --depth 1 https://github.com/LizardByte/Sunshine.git sunshine
git clone --depth 1 https://github.com/moonlight-stream/moonlight-android.git
git clone --depth 1 https://github.com/Genymobile/scrcpy.git
git clone --depth 1 https://github.com/OpenTabletDriver/OpenTabletDriver.git opentabletdriver
```

### 1.1 Why per-platform native APIs (Media Foundation + VideoToolbox) won

We considered and rejected several alternatives before landing on the design in [`design.md`](design.md):

| Option | Verdict | Why |
|---|---|---|
| Direct NVENC SDK only | **Rejected** | NVIDIA-only. Dev work was actually completed (~700 LOC of bindgen FFI) and then thrown away because it didn't cover AMD/Intel. The user wants a publishable binary. |
| FFmpeg (`ffmpeg-next` / `ffmpeg-the-third`) | **Rejected** | Covers all GPUs but adds **~30 MB of DLLs** to the installer (or a custom stripped build pipeline that we'd own forever). We use < 5 % of FFmpeg's surface. The user pushed back hard on installer size. |
| GStreamer (`gstreamer-rs`) | **Rejected** | Covers all GPUs via plugins. **+20-30 MB** of GLib/GObject runtime. Same size objection. |
| Hybrid: direct NVENC fast path + MF for AMD/Intel | **Rejected** | 2× the encoder maintenance for marginal latency win. Sunshine does this; we explicitly chose not to. |
| Three vendor SDKs (NVENC + AMF + QSV via `dlopen`) | **Rejected** | 3× code, ongoing per-vendor quirks. |
| D3D12 Video Encode | **Deferred to future** | Vendor-agnostic and lower-overhead than MF, but Win11+ only. Revisit when we drop Win10. |
| **Windows Media Foundation (HEVC encoder MFT)** | **Chosen** | Auto-dispatches to NVENC/AMF/QSV/software. **Zero installer-size cost** (ships with Windows). Already in the `windows` crate via `Win32_Media_MediaFoundation` feature. ~+0.2-1 ms latency overhead vs direct NVENC (acceptable). |
| **macOS VideoToolbox** | **Chosen for post-v1.0 macOS port** | The native low-level API on macOS. WWDC21 added explicit low-latency mode. `objc2-video-toolbox` is the production-used Rust binding. |

The decision is final. **Do not** revisit this without strong cause.

### 1.2 Sunshine — what to copy, what to skip

Sunshine (`research/sunshine/`) is the gold standard for low-latency desktop streaming on Windows. Multiple Windows-specific tricks in their code are **necessary**, not optional. From `src/platform/windows/display_base.cpp`:

**MUST adopt** (without these you will hit the listed pathology):

| Trick | What breaks without it |
|---|---|
| `SetThreadExecutionState(ES_CONTINUOUS \| ES_DISPLAY_REQUIRED)` per session | Idle desktop puts the monitor to sleep → `AcquireNextFrame` returns `DXGI_ERROR_ACCESS_LOST` → re-init wakes the monitor → infinite cycle. |
| `IDXGIDevice1::SetMaximumFrameLatency(1)` | Swap chain queues frames; you eat queueing latency you can't see. |
| `IDXGIDevice::SetGPUThreadPriority(7)` + `D3DKMTSetProcessSchedulingPriorityClass(REALTIME)` | Frame timing jitters under contention. (HIGH on NVIDIA + HAGS to dodge a documented driver crash.) |
| 200 ms `AcquireNextFrame` timeout → 10 ms `sleep` *without* holding the D3D11 device lock | Encoder thread starves when capture is idle. |
| `IDXGIOutput5::DuplicateOutput1` with a scan-out format preference list such as `[B8G8R8A8_UNORM, R8G8B8A8_UNORM, R10G10B10A2_UNORM, R16G16B16A16_FLOAT]` (fall back to `IDXGIOutput1::DuplicateOutput`) | Avoids unnecessary conversion when fullscreen content is not BGRA. Do **not** request `NV12` here; DDA format lists must be display scan-out formats. Convert to NV12 in `color.rs`. |

**Skip** (out of scope for our setup):
- Their MinHook on `NtGdiDdDDIGetCachedHybridQueryValue` — only matters on hybrid-GPU laptops; user's rig is desktop + dGPU.
- Their WGC (Windows.Graphics.Capture) fallback — DDA is sufficient and lower-latency.
- Their full RFI (reference-frame invalidation) machinery — requires encoder cooperation we can't easily get from MF; we use aggressive intra-refresh + on-demand IDR instead.

**Architectural pattern to copy**: their two-level encoder abstraction (`encoder_t` data struct + `encode_session_t` virtual class, in `src/platform/common.h:405–458` and `src/video.h:210–220`). Already reflected in `design.md` §6.3.

**Specific fact about Sunshine that affected our design**: their MediaFoundation backend uses FFmpeg's `hevc_mf` codec, which has `FIXED_GOP_SIZE` because **FFmpeg's wrapper** can't do on-demand IDR. That is not necessarily an MF limitation — see §3.1 below.

### 1.3 moonlight-android — Android decode lessons

`research/moonlight-android/` has the most battle-tested MediaCodec setup on the planet. Two findings affect us directly:

**Latent crash bug in our existing Android code**: `android/app/src/main/java/dev/penflow/VideoDecoder.kt` sets both `KEY_OPERATING_RATE = 240` and `KEY_PRIORITY = 0` unconditionally. Moonlight (`MediaCodecHelper.java:482`) explicitly notes this combination **crashes Adreno 620** (Snapdragon 765G — Xiaomi Mi 10 lite 5G, Redmi K30i 5G). The MovinkPad Pro 14 is Adreno 720, so we don't hit it on the dev rig, but **anyone with a 765G-class device will SIGSEGV the decoder on connect**. Fix in §10.2 of [`design.md`](design.md):

```
on Qualcomm chips:
    KEY_OPERATING_RATE = Short.MAX_VALUE.toInt()  // moonlight's value
    do NOT set KEY_PRIORITY=0
otherwise:
    KEY_PRIORITY = 0
    do NOT set KEY_OPERATING_RATE
```

**Vendor key we are missing**: `vendor.qti-ext-dec-picture-order.enable = 1` (Qualcomm only). Disables HEVC reorder buffering. Saves 5-10 ms on Qualcomm chips. Add to the same MediaFormat setup.

**Codec recovery ladder we don't have**: Moonlight has 4-level escalation `Flush → Restart → Reset → Reinit` (`MediaCodecDecoderRenderer.java:714`). Our `VideoDecoder.kt`'s `onError` only logs. Production-quality gap.

**Decoder-hung watchdog**: 5-second timeout on `dequeueInputBuffer`. We have nothing.

**Surface-destroyed mid-stream**: Moonlight handles it; we don't. If the user backgrounds the app while streaming, our decoder will misbehave.

**Frame pacing — adopt MIN_LATENCY**: Today we do `releaseOutputBuffer(idx, true)` which always renders. Moonlight's MIN_LATENCY mode passes `System.nanoTime()` as the render PTS so SurfaceFlinger drops late frames automatically. Drain to newest, drop the rest.

**Dead zone after stylus lift**: Moonlight (`Game.java:2147`) enforces a spatial dead zone (~5 px) after `ACTION_UP` to prevent double-click artefacts. We have none.

**Don't copy**: their RFI machinery, their game-specific frame pacing modes (`MAX_SMOOTHNESS` / `CAP_FPS`), their HUD (ours is genuinely better — true e2e via NTP-style sync).

### 1.4 scrcpy — protocol patterns

`research/scrcpy/` (also already at `C:\repo\krita\penflow\scrcpy-ref/` in the predecessor project) is the canonical low-latency Android↔PC protocol implementation. We're its mirror image (PC→Android) but the patterns transfer.

**Adopt the 1-byte readiness probe** (`scrcpy/app/src/server.c:467`). The ADB tunnel can accept a TCP connect *while the Android app is still initializing* — `connect()` succeeds, but the Android side isn't listening yet. Both sides exchange a `0xA5` byte before any framed message. This is a constant in our `penflow-protocol` already (`READY_BYTE`).

**Our framing is better than scrcpy's**. Our `[u8 type][u32 BE len][payload]` envelope means an unknown message is a single skip; scrcpy's no-length-prefix raw stream means parser desync = unrecoverable. **Keep our framing.**

**Single socket is correct**. scrcpy uses 3 sockets (video/audio/control) only because their video is raw codec bytes without length framing. We don't need this.

**Verbose protocol trace**: `PENFLOW_PROTO_TRACE=1` env var → hex-dump every framed message with direction arrow + timestamp. ~20 lines, invaluable when debugging. Add this in `penflow-protocol` or `penflow-transport`.

### 1.5 OpenTabletDriver — cautionary tale + a few patterns

**Surprising finding**: OTD's Windows backend is **`SendInput` mouse-only**. Zero pen semantics. No pressure, no tilt, no hover, no eraser bit — just left-click. They cannot drive Krita's Windows Ink mode at all. Our existing WinRT `InputInjector` approach (in old project's `server/pen_injector.py`) is **strictly more capable** for creative apps.

**Patterns worth porting from OTD anyway**:
- `SetProcessDpiAwareness(2)` (`PROCESS_PER_MONITOR_DPI_AWARE`) at startup. Without this, virtual-screen geometry is in DIPs not physical pixels and Windows Ink injection lands in the wrong place on multi-monitor 4K + 1080p setups.
- `Matrix3x2`-style input-area→output-area transform (`AbsoluteOutputMode.cs:90-113`). Replaces our naive `left + norm * width`. Future-proofs "rotate the tablet 90°".
- `IStateBinding` enum-style binding model. Replaces our hardcoded Ctrl/Shift/E. Shape:
  ```rust
  enum Binding { None, KeyTap(VirtualKey), KeyHold(VirtualKey), KeyChord(Vec<VirtualKey>),
                 MouseButton(MouseButton), EraserToggle }
  ```
- Tip pressure threshold (`tip_threshold: f32`). Avoids accidental marks from near-hover noise.
- Eraser flip-then-flush: when switching the WinRT `INVERTED` bit, emit one `POINTER_UP` / out-of-range frame first to avoid drivers seeing both bits set simultaneously.

### 1.6 Closest analog: WeyLus

[Weylus](https://github.com/H-M-H/Weylus) is the only same-spirit project. Rust server + browser-based input forwarding. **It does not deliver Windows pen pressure** (only Linux uinput). This is precisely the gap zpenflow fills. We are first-mover for "PC → Android pen display with Windows Ink pressure" specifically.

---

## 2. Demo / prototype experience

The predecessor project (`C:\repo\krita\penflow\`) went through six phases over ~6 weeks. Each phase produced lessons that are baked into [`design.md`](design.md), but the *experience* of getting there is below.

### 2.1 Hardware specifics

- **MovinkPad Pro 14** quirks:
  - The third stylus button **does NOT generate a chord** of `BUTTON_STYLUS_PRIMARY + BUTTON_STYLUS_SECONDARY` like Wacom Pro Pen 3. It sends `MotionEvent.BUTTON_TERTIARY` (`0x04`) directly. The current `PenInputCapture.kt` decodes this correctly. (We previously had chord-detection code based on Pro Pen 3 docs; the MovinkPad firmware doesn't match the docs.)
  - Pen tilt is reported as `AXIS_ORIENTATION` + `AXIS_TILT` and we decompose into (tiltX, tiltY) in degrees. Range is roughly -60..+60 deg.
  - Pressure resolution looks like 14-bit (we normalize to float [0..1]).
  - Multi-touch: separate from pen events; `toolType=FINGER` filter applies.
  - Internal panel reports as `\\.\DISPLAY1` of size `2880x1800` natively (some sessions show `3840x2160` if HiDPI scaling kicks in — see DPI awareness pitfall in §4.1).

- **RTX 5070 + NVENC**: HEVC P1 ULTRA_LOW_LATENCY hits ~2-3 ms encode for 2560×1440. Tested on driver 555.x+. Older drivers (<540) lacked some intra-refresh slice modes we used.

- **VDD (Virtual Display Driver)**: The proven predecessor-main path uses [VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) release `25.7.23` to create a virtual extended monitor matching the MovinkPad's exact 2880×1800 native resolution, eliminating letterbox. **Critical pitfall**: the upstream `vdd_settings.xml` schema has HDR/auto_resolutions sections that **crash mttvdd.dll with `WUDFUnhandledException c0000005`** on our rig. The working schema is in `tools/vdd/vdd_settings.xml` — minimal, just `monitors/gpu/global/resolutions`. Do not regenerate from upstream defaults without testing.

### 2.2 Phase-by-phase journey (what was tried, what stuck)

The numbered phases below were the six iterations the old project went through. Each ended at a measured baseline. Read them as "we tried X, got Y, learned Z".

**Phase 1 — Software baseline** (libx264 + dxcam + SendInput cursor):
- e2e ~50 ms. Encoder ate 25 ms.
- Mouse-only injection (no pressure forwarded into Windows). Krita saw flat clicks.
- Established the protocol shape (still in use today: `[u8 type][u32 BE len]`).

**Phase 2 — VDD + WinRT pen injection**:
- Switched Krita from Wintab to Windows Ink in settings. Pressure suddenly worked.
- WinRT `InputInjector` via Python `winsdk` — `inject_pen_input` takes a single `PenInfo`, returns void.
- VDD bring-up was painful (see §2.1 schema crash). Once stable, no more letterbox.

**Phase 3 — C++ hot path with NVENC + DXGI Output Duplication**:
- Encoder dropped from 25 ms (libx264) to ~3 ms (NVENC HEVC). Capture from ~5 ms (dxcam Python) to <1 ms (DXGI native).
- pybind11 brought the C++ engine into Python. CMake via scikit-build-core. Visual Studio 17 2022 generator (Ninja didn't find cl.exe on PATH).
- NVENC headers are MIT (vendored from FFmpeg/nv-codec-headers). DLL is dynamically loaded — no link-time NVIDIA dependency.
- pybind11 LTCG anonymous-LTO-objects link error solved with `pybind11_add_module(... NO_EXTRAS ...)`.
- Windows.h `#define EnumMonitors EnumMonitorsA` macro collided with our `EnumMonitors` function. Renamed to `ListOutputs`. Lesson: avoid Win32-namespace-resembling identifiers.

**Phase 4 — Android decoder tuning**:
- Decoder went 19 ms → 7-8 ms via `KEY_OPERATING_RATE = 240`, `KEY_PRIORITY = 0`, `vendor.qti-ext-dec-low-latency.enable=1`, async MediaCodec callbacks, render directly to SurfaceView (no CPU copy).
- Parked-index pattern: when `onOutputBufferAvailable` arrives, hold the index, only release on the next callback. Avoids returning empty buffer back to codec.

**Phase 5 — IDR-spike elimination**:
- e2e was good *average* but had p99 spikes when periodic IDR fired (every 2 s by default).
- NVENC intra-refresh wave: `enableIntraRefresh=1`, `intraRefreshPeriod = fps*2 = 120 frames`, `intraRefreshCnt = 8`, slice mode 3, 4 equal slices per picture. Smoothed p99 dramatically.
- **Important context for the rewrite**: this aggressive intra-refresh is likely **not fully expressible via Media Foundation** on AMD/Intel MFTs (NVIDIA MFT supports it, others vary). Worst-case for AMD/Intel users: p99 returns to "periodic IDR" pattern. NVIDIA users see no regression. See `design.md` §6.4.

**Phase 6 — True e2e measurement (NTP-style sync)**:
- Before this, the HUD measured `displayedNs - recvNs` on Android only. That's not e2e. We didn't realize until comparing numbers to expectation.
- Built `TimeSync.kt` (NTP-style ping/pong with min-RTT filter). PC sends `pcMinusAndroidNs` offset; Android reconciles. True e2e = `displayedNs (Android local) - ptsNs (PC local) + offset`.
- Result: e2e ~20 ms (matches SuperDisplay). Specific breakdown: capture ≤1 ms, encode ~3 ms, net ~5 ms (USB ADB), decode 7-8 ms, display ~8 ms (1 vsync at 120 Hz Android panel).

### 2.3 Critical bugs found during demo

These are not in `design.md`. **Do not let them recur.**

1. **Pen "flying lines"** under fast strokes. Per-event `scope.launch { sendPenEvent }` on the UI thread caused samples to arrive out of order at the socket (different IO dispatcher cores). Fixed with single `Channel<PenSample>` + dedicated consumer coroutine. The current `PenflowClient.kt` has this; preserve the pattern. (And do the same for touch — `Channel<TouchSnapshot>` separately.)

2. **Decoder cold-start spikes** of 80 ms after idle. DXGI returns timeout when desktop is static, the encoder thread stops emitting frames, and Android's MediaCodec governor drops the decoder clock. First frame after activity resumes pays the cold-start cost. **Fix**: keepalive copy texture on the PC. When DXGI times out, re-encode the *last* captured frame at the configured fps. NVENC takes <1 ms on unchanged content (mostly "no change" motion vectors). Tiny bandwidth hit, huge latency win. Already in `design.md` §6.1.

3. **Touch injection `E_NOINTERFACE`**: Python `winsdk.Windows.UI.Input.Preview.Injection.InputInjector.inject_touch_input(...)` takes a **list / iterable**, not a single `InjectedInputTouchInfo`. Different from `inject_pen_input` (which is single). **Also**: `initialize_touch_injection(mode)` takes 1 arg in winsdk, not 2 like the C# WinRT signature suggests. Both errors return generic-looking COM HRESULTs that don't help debug. The Rust port uses Win32 `InjectTouchInput` directly via `windows-rs`; the C# WinRT wrapper documentation is misleading.

4. **Krita button mapping under Windows Ink**: SendInput mouse clicks for btn1/btn2 were *filtered out by Windows Ink* when concurrent with pen contact. Switched to keyboard modifiers held down (Ctrl for btn1, Shift for btn2). Btn3 sends an 'E' key tap (Krita's eraser shortcut). **Pattern**: when Krita is in Windows Ink mode, send keyboard events for "modifier" buttons, not synthesized mouse clicks. Tool=eraser sets the WinRT `INVERTED` bit on the pen event (better than the 'E' tap; cleaner state).

5. **DXGI ACCESS_LOST** on RDP, screen lock, full-screen game launch, GPU reset. Transparent re-init in the Capturer. `design.md` §6.1 has it.

6. **NVENC `nvEncRegisterResource` cache miss per frame**: if you pass a different `ID3D11Texture2D*` each call, NVENC re-registers (~0.5 ms). Pipeline copies fresh DXGI frame into a **stable** keepalive texture (same pointer for the engine's lifetime), so register fires once. This is *also* what enables the keepalive optimization (§2.3 #2). Two birds, one texture.

7. **Pen-stroke jitter via `InjectSyntheticPointerInput`** (issue #23): synthetic pointer injection takes `ptPixelLocation` as `POINT { x: i32, y: i32 }` — integer-pixel only. Visible as stair-step jitter when zooming in on strokes drawn at zoomed-out canvas. Investigation:
   - The `ptHimetricLocation` field is a documented sub-pixel channel on the receive side, but its scale for synthetic devices is undocumented; an empirical kernel-pRect probe on a 144-DPI rig showed ~17.6 himetric/pixel, which improves jitter ~17× but doesn't eliminate it.
   - SuperDisplay's drivers (`C:\Program Files\SuperDisplay\drivers\`) revealed the actual production answer: they ship a **VMulti** HID virtual digitizer (`superdisplay_hid.dll` contains the `VMulti*` symbol set verbatim). The kernel sees a real HID digitizer with declared `logical_max=32767` per axis → genuine sub-pixel coords flow into `POINTER_INFO` automatically, no scale guessing.
   - **Fix in `feat/vmulti-hid-injection`**: ship the [X9VoiD/vmulti-bin](https://github.com/X9VoiD/vmulti-bin) community fork (signed, MIT, 16384-pressure extended pen) as an optional driver. `InputInjector::new()` probes for it (VID `0x00FF` / PID `0xBACC` + 65-byte output report) and uses it when present; falls back to `InjectSyntheticPointerInput` otherwise. Report wire format: 12-byte extended digitizer report (REPORTID 0x06), little-endian, ported verbatim from `research/VoiDPlugins/src/VoiDPlugins.Library/VMulti/Device/` (the OpenTabletDriver WindowsInk plugin — the canonical open-source VMulti consumer). User-side install: download `VMulti.Driver.zip` from the vmulti-bin release page, run `install_hiddriver.bat` as admin. Bundled installer UX is a separate task.

### 2.4 Things that *should* work but were never validated end-to-end

- AMD GPU. We have no AMD machine. The Media Foundation path *should* dispatch to AMD's HEVC encoder MFT; needs verification.
- Intel iGPU. Same. Sunshine reports Intel iGPU has known-buggy MFTs on certain UHD generations; live-probe before depending.
- Android tablets other than the MovinkPad. Should work — our code reads `MediaCodecCapabilities` to pick HEVC vs H.264 and applies vendor extensions defensively. Untested on Samsung, Pixel, OnePlus, etc.
- macOS. Designed-for, not yet built.

---

## 3. Technical baselines

### 3.1 Latency budget (on RTX 5070 + MovinkPad Pro 14, USB-C ADB)

| Segment | Measured (ms) | Source |
|---|---|---|
| Capture (DXGI Output Duplication) | ≤ 1 | C++ build, instrumented |
| Encode (NVENC HEVC P1 ULL) | ~3 | C++ build, instrumented |
| Network (ADB localabstract over USB) | ~5 | ping/pong + frame size measurements |
| Decode (Snapdragon 8s Gen 3 HEVC) | 7-8 | MediaCodec callback timestamps |
| Display (1 vsync @ 120 Hz Android panel) | ~8 | computed from refresh rate |
| **Total e2e** | **~20** | TimeSync ping/pong measurement |

**Comparison points**:
| Product | e2e latency | License |
|---|---|---|
| Wacom Lab Instant Pen Display | ~70 ms | proprietary (free with hardware) |
| SuperDisplay | ~20 ms | paid (€10) |
| **Penflow predecessor (current build on `main` of old project)** | **~20 ms** | open-source |
| Penflow (this rewrite, target) | ≤ 25 ms p50, ≤ 35 ms p99 | open-source |

The rewrite target is slightly more conservative than the predecessor's measured number to absorb MF's overhead vs direct NVENC. If we hit 20 ms on RTX 5070 with MF, that's a stretch goal.

### 3.2 Bandwidth

- **Default bitrate**: 50 Mbps CBR. (Started at 20 Mbps, was visibly blurry, bumped to 50.)
- USB 2.0 ADB tunnel sustains ~280 Mbps headroom. 50 Mbps fits comfortably.
- VBV buffer = `bitrate / fps * 2` (4-frame buffer). Tight enough to limit IDR overshoot, loose enough to absorb intra-refresh waves.

### 3.3 The §6.4.1 IDR gate — VERIFIED on NVIDIA (2026-05-03)

Result: **PASS on NVIDIA HEVC Encoder MFT (RTX 5070).** `crates/penflow-core/examples/mf_idr_probe.rs` directly drives the MFT, sets `CODECAPI_AVEncVideoForceKeyFrame` with `VARIANT.vt = VT_UI4 / ulVal = 1` before submitting frame N, and frame N comes out as an IDR (HEVC NAL type 19, `IDR_W_RADL`) — stable across runs. The design (§6.4.1) holds; the engine uses on-demand IDR.

Still to verify on AMD and Intel MFTs once we have those adapters. If either fails, the §6.4.1 fallback (periodic IDR + on-connect reset) takes over; not a design-altering risk.

The probe also surfaced four other gotchas worth knowing before you touch `encoder/mf.rs` — captured in §4.6 below.

Run it yourself: `cargo run -p penflow-core --example mf_idr_probe`. Exit 0 = PASS, 1 = no IDR after force, 2 = setup error.

### 3.4 Specific NVENC tuning the predecessor used

Captured here for forward-looking comparison. The MF rewrite cannot necessarily express all of these; document the gap as you go.

| Knob | Value | Why |
|---|---|---|
| Codec | HEVC | better compression at same quality vs H.264; modern decoders have it |
| Preset | P1 (`NV_ENC_PRESET_P1_GUID`) | fastest, lowest quality knob — quality is fine at 50 Mbps |
| Tuning | `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY` | disables look-ahead, enables zero-reorder |
| Rate control | CBR | predictable bandwidth on tight USB tunnel |
| `frameIntervalP` | 1 | no B-frames |
| `gopLength` | `NVENC_INFINITE_GOPLENGTH` | intra-refresh handles GOP recovery |
| `enableAQ` | 0 | adaptive quant adds variance, hurts CBR |
| `zeroReorderDelay` | 1 | output frame N immediately, no reorder buffering |
| `enableIntraRefresh` | 1 | spread IDR cost across N frames as a wave |
| `intraRefreshPeriod` | `fps * 2` | 2-second wave at 60 fps = 120 frame period |
| `intraRefreshCnt` | 8 | refresh over 8 frames per wave |
| `sliceMode` | 3 | equal-sized slices |
| `sliceModeData` | 4 | 4 slices per picture |
| `repeatSPSPPS` | 1 | every IDR also includes SPS/PPS for resilience |
| `outputBufferingPeriodSEI` | 0 | save bytes |
| `outputPictureTimingSEI` | 0 | save bytes |
| `maxNumRefFrames` | 1 | minimum legal value, smallest DPB |
| VUI `videoFullRangeFlag` | 1 | full range |
| VUI `colourPrimaries` | BT709 (1) | sRGB |
| VUI `transferCharacteristics` | BT709 (1) | sRGB |
| VUI `colourMatrix` | BT709 (1) | sRGB |

Without the VUI flags, Android's MediaCodec defaults to limited-range Y'CbCr and crushes blacks (16↦0) / clips whites (235↦255) — a ~6% systematic offset visible as a flat washed-out look, hard to spot in a thumbnail but wrong for color-critical work. **This must work in MF too**; the equivalents are:
- `MF_MT_VIDEO_NOMINAL_RANGE = MFNominalRange_0_255`
- `MF_MT_VIDEO_PRIMARIES = MFVideoPrimaries_BT709`
- `MF_MT_TRANSFER_FUNCTION = MFVideoTransFunc_709`
- `MF_MT_YUV_MATRIX = MFVideoTransferMatrix_BT709`

---

## 4. Known landmines (a checklist of "don't trip on these")

### 4.1 Windows-rs 0.61 API quirks (discovered the hard way)

- `D3D11CreateDevice` takes **`HMODULE`** for the `software` parameter, **not `Option<HMODULE>`**. Pass `HMODULE::default()` for "no software rasterizer".
- `IDXGIOutput::GetDesc` returns the desc **by value via `Result<DXGI_OUTPUT_DESC>`**, not via an out-pointer. Don't pass `&mut desc`.
- `MONITORINFOF_PRIMARY` is **not exposed** as a constant in `Win32_Graphics_Gdi`. It's `0x0000_0001`; define it locally.
- `D3D11_TEXTURE2D_DESC::BindFlags` is `u32`, **not the typed `D3D11_BIND_FLAG` newtype**. Extract via `.0`: `(D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32`.
- `IDXGIFactory6::EnumAdapterByGpuPreference` is generic over the IID type. Annotate the return: `let adapter: IDXGIAdapter1 = factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)?;`.
- COM objects implementing `windows::core::Interface` need `.cast::<TargetInterface>()?` to QueryInterface.

### 4.2 Tauri 2.x scaffolding

- **`apps/penflow-gui/src-tauri/icons/icon.ico` is required at build time** even when `bundle.active = false`. Tauri's build script generates a Windows Resource for the .exe icon. There's already a 32×32 placeholder in the repo; replace with branding before MSI.
- For pure-static frontend (no JS bundler), set `frontendDist = "../ui"` and **omit `devUrl`**. `cargo tauri dev` will load the static HTML directly.
- `apps/penflow-gui/src-tauri/capabilities/default.json` references a schema at `../gen/schemas/desktop-schema.json` that only exists after first `cargo tauri dev` run. IDE warning, not a build failure.
- Tauri 2.x lib+bin pattern (separate `src/lib.rs` + `src/main.rs`) is for mobile targets. Desktop-only Penflow uses single `main.rs` — simpler.

### 4.3 NOT used in zpenflow (intentionally)

If you find yourself wanting any of these, **stop and re-read [`design.md`](design.md) §6**:

- ❌ `bindgen` — we explicitly avoid it. The MF design has no C-header FFI step. (Predecessor had ~700 LOC of bindgen-generated NVENC FFI; we threw it away.)
- ❌ `libloading` — same. MF is COM via `windows-rs`.
- ❌ LLVM / `libclang.dll` on dev machine — not needed. (Predecessor needed it for bindgen.)
- ❌ FFmpeg / `ffmpeg-next` / `ffmpeg-the-third` — see §1.1 for why we rejected.
- ❌ GStreamer / `gstreamer-rs` — see §1.1.
- ❌ NVENC SDK directly — see §1.1.

### 4.4 ADB tunnel pitfalls

- `adb reverse localabstract:penflow tcp:<server_port>` requires `adb start-server` first; otherwise reverse silently fails on first connect.
- The TCP `connect()` succeeds even if the Android app is still starting — that's why the 1-byte `READY_BYTE` probe in §1.4 matters.
- ADB on Windows holds onto a USB endpoint; if `adb kill-server` is called mid-session, the next session needs `adb usb` + `adb devices` to re-enumerate.
- Wireless ADB (TCP/IP mode) was tested briefly in predecessor; latency suffered (+30 ms variance). Stick to USB for v1.0.

### 4.4b DXGI adapter / topology gotchas (from the gate-2 probe)

`crates/penflow-core/examples/adapter_topology.rs` mapped the DXGI surface on the dev rig and turned up three things worth pinning down before `capture/dxgi.rs` starts:

1. **`IDXGIOutput5::DuplicateOutput1` with a single-format list (just `B8G8R8A8_UNORM`) failed silently and made the probe fall back to `IDXGIOutput1::DuplicateOutput`.** Using the design's full 4-format list (`B8G8R8A8_UNORM, R8G8B8A8_UNORM, R10G10B10A2_UNORM, R16G16B16A16_FLOAT`) made it succeed first try. The instruction in `design.md` §6.1 to use this exact list is load-bearing — don't trim it.
2. **Modern NVIDIA drivers expose one logical DXGI adapter per engine.** On RTX 5070, `EnumAdapters1` returned three identical "NVIDIA GeForce RTX 5070" entries with different LUIDs (only the first owns the desktop outputs); the others are compute/encode/decode-only. This means `EnumAdapterByGpuPreference(_, HIGH_PERFORMANCE)` could return an output-less adapter on a different system. **Use LUID for adapter equality** (description + vendor + device ID are not unique), and have the engine verify the picked adapter owns at least one output before treating it as the capture device.
3. **DXGI output dimensions depend on whether the process is DPI-aware.** The probe reported 2560×1440 in the first run (no manifest) and 3840×2160 in the second run on the same physical 4K monitor with 150 % Windows scaling. The OTD-derived rule in `design.md` §6.6 (`SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` at startup) is mandatory, not just nice-to-have — without it the capturer reads scaled DIPs and the injector will land coordinates in the wrong place.

VDD wasn't installed on the rig at probe time, so the probe could only report "no virtual-display indicators". The VDD-on-high-perf-adapter check is in the probe and will trigger automatically once the VDD is present.

### 4.5 Media Foundation HEVC encoder gotchas (from the §3.3 probe)

These are non-obvious failure modes the probe hit on the way to a PASS. Each costs hours if you discover it on the encoder hot path.

1. **Vendor MFTs are registered system-wide, regardless of which GPU is physically present.** On the dev rig (NVIDIA-only desktop), `MFTEnumEx` with `MFT_ENUM_FLAG_HARDWARE | SORTANDFILTER` returned `AMDh265Encoder` first and `NVIDIA HEVC Encoder MFT` second. Activating the wrong-vendor MFT against the NVIDIA D3D11 device fails with `E_OUTOFMEMORY` (0x8007000E) at `MFT_MESSAGE_SET_D3D_MANAGER`. **Filter MFTs by `MFT_ENUM_HARDWARE_VENDOR_ID_Attribute`** (returns a `VEN_XXXX` string — `VEN_10DE` for NVIDIA, `VEN_1002` for AMD, `VEN_8086` for Intel) and match against the adapter's `DXGI_ADAPTER_DESC1::VendorId`. Then live-probe in order, since the vendor-ID attribute may be missing on some MFTs.

2. **`MF_TRANSFORM_ASYNC_UNLOCK = 1` must be set BEFORE any other call on the MFT.** Otherwise `SET_D3D_MANAGER` and `SetOutputType` return `MF_E_TRANSFORM_ASYNC_LOCKED` (0xC00D6D77). The order is: GetAttributes → SetUINT32(`MF_TRANSFORM_ASYNC_UNLOCK`, 1) → SetUINT32(`MF_LOW_LATENCY`, 1) → SET_D3D_MANAGER → SetOutputType → SetInputType.

3. **The HEVC output media type requires `MF_MT_MPEG2_PROFILE`.** Use `eAVEncH265VProfile_Main_420_8` (= 1). Without it `SetOutputType` returns `MF_E_INVALIDMEDIATYPE` (0xC00D36B4) — generic enough to send you on a long debugging tour.

4. **Color-space attributes (`MF_MT_VIDEO_NOMINAL_RANGE`, `_PRIMARIES`, `_TRANSFER_FUNCTION`, `_YUV_MATRIX`) belong on the INPUT type, not the OUTPUT type.** The encoder reads them and writes the corresponding VUI bytes into the SPS. Setting them on output type is a no-op at best, an error at worst on some MFTs.

5. **NVIDIA's MFT emits AUD NALs (type 35) at the start of every access unit, and re-emits VPS+SPS+PPS on every IDR.** Both are harmless — Android's MediaCodec ignores AUDs, and repeated parameter sets give us free `repeatSPSPPS = 1` resilience. AMD and Intel MFTs may behave differently here; verify when those probes run.

### 4.6 VDD — virtual display lifecycle

The user explicitly wants VDD to **only create a virtual monitor while a client is connected**, not always-on. When the Android client disconnects, the virtual monitor should disappear. Idle state = no virtual monitor (the cursor can't wander into dead pixel space).

The predecessor `main` branch proves manual install + stable capture through Virtual Driver Control, not runtime plug/unplug. Technically this should be possible because IDD supports `IddCxMonitorArrival` / `IddCxMonitorDeparture`, but Wave 5 must first identify and pin the exact VDD control path that exposes it. If the proven VirtualDrivers release has no supported runtime control mechanism, evaluate a fork or companion service. Do not pull an arbitrary VDD `main` blindly; the XML schema and control surface have changed historically.

This is Wave 5 work, not Wave 2. Don't engineer it now; just don't accidentally lock yourself out.

---

## 5. Forward plan

### 5.1 Wave-2 gates: status

All three Wave-2 gates PASS on the dev rig (RTX 5070, NVIDIA-only desktop, 2026-05-03):

| Gate | Probe | Result | Carried-forward residue |
|---|---|---|---|
| 1. MF on-demand IDR | `cargo run -p penflow-core --example mf_idr_probe` | PASS on NVIDIA HEVC MFT | Re-run on AMD + Intel MFTs when those adapters are available. Five MF gotchas captured in §4.5 — they're load-bearing for `encoder/mf.rs`. |
| 2. Adapter/VDD topology | `cargo run -p penflow-core --example adapter_topology` | PASS — single D3D11 device for capture + encode | Three findings in §4.4b — multi-format DDA list, NVIDIA per-engine adapter quirk, DPI awareness. Re-run when VDD is installed (Wave 5) to confirm the VDD output lands on the high-perf adapter. |
| 3. WinRT `InputInjector` (unpackaged) | `cargo run -p penflow-core --example inject_probe` | PASS — no MSIX capability needed for unpackaged binary | Re-run inside the WiX/MSI release shape during Wave 5. If it fails there, switch to MSIX-with-restricted-capability, a small broker process, or virtual HID (see `design.md` §6.6). |

The encoder design (§6.4) is now locked. Engine implementation per §5.2 can proceed.

The probe binaries stay in `crates/penflow-core/examples/` — they're regression-protected by the workspace `cargo build` and exist as the canonical reference for "did the underlying API path actually work last time we touched this".

### 5.2 Implementation order (after the Wave-2 gates pass)

Roughly map to [`design.md`](design.md) §6, with the highest-confidence pieces first so milestones land early:

1. **`crates/penflow-core/src/d3d11.rs`** — D3D11 device on high-perf adapter. Already prototyped; just port. ~50 LOC.
2. **`crates/penflow-core/src/monitors.rs`** — DXGI output enumeration. Already prototyped. ~80 LOC.
3. **`crates/penflow-core/src/capture/dxgi.rs`** — DXGI Output Duplication + ACCESS_LOST recovery. Already prototyped (predecessor's `Capturer`). ~120 LOC. **Add the Sunshine §1.2 "MUST adopt" tricks here.**
4. **`crates/penflow-core/src/color.rs`** — D3D11 VideoProcessor BGRA→NV12. ~150 LOC. (Skip Sunshine's HLSL shader path; VideoProcessor is simpler and good enough.)
5. **`crates/penflow-core/src/encoder/mod.rs`** + **`encoder/mf.rs`** — the big one. ~400 LOC. Driven by IDR-gate findings.
6. **`crates/penflow-core/src/packet_queue.rs`** — Mutex<VecDeque> + Condvar SPSC, drop-oldest, depth 8. Already prototyped. ~80 LOC + 5 unit tests.
7. **`crates/penflow-core/src/pipeline.rs`** — capture+encode thread + keepalive. ~150 LOC.
8. **`crates/penflow-core/src/inject/win_ink.rs`** — port of `pen_injector.py`'s `PenInjector`. WinRT `InputInjector`. ~150 LOC.
9. **`crates/penflow-core/src/inject/win_touch.rs`** — Win32 `InjectTouchInput`. ~100 LOC.
10. **`crates/penflow-core/src/inject/coords.rs`** + **`binding.rs`** — Matrix3x2 transform + binding model from OTD. ~80 LOC.
11. **`crates/penflow-core/src/lib.rs`** — public `Engine` API. Replace placeholder `build_id()`.
12. **`crates/penflow-core/examples/capture_to_file.rs`** — verify with telemetry. Manual gate: `.h265` plays in VLC.
13. **Apply the Android-side fixes from §1.3** to `android/app/src/main/java/dev/penflow/VideoDecoder.kt` (Adreno 620 fix is critical; codec recovery ladder is production-quality work). For MIN_LATENCY frame pacing, keep async `MediaCodec.Callback`; do not call `dequeueOutputBuffer()` from inside callbacks. Use an async-safe latest-output-index strategy.

After steps 1-13 land, `penflow-core` is functionally complete. Wave 3 (`penflow-server`) is then an afternoon: tokio session loop wiring the engine to the transport.

### 5.3 Open questions that don't block immediate work

- **Pressure curve** (configurable Bezier vs identity-with-threshold). Default is identity + threshold; revisit post-v1.0 based on user feedback.
- **macOS pen injection target API**. WWDC offers `CGEvent` for mouse/pointing; pressure on macOS is historically thinner than Windows Ink. Investigate during the macOS wave.
- **VDD runtime-control pin**. Identify a specific VirtualDrivers release/fork/control mechanism that exposes plug/unplug. May need to fork into `tools/vdd/` or add a companion service if upstream is unstable.
- **Crash reporting**. v1.0 has none. Local logs only. Sentry-style remote reporting is a Wave 6+ choice.

### 5.4 Acceptance gate for v1.0

Reproduced from [`design.md`](design.md) §18 for convenience:

- Tauri GUI launches, picks monitor, picks codec, shows status.
- Android APK installs, opens, connects via ADB reverse.
- 1-byte `READY_BYTE` probe + handshake completes within 1 s of connect.
- Live capture+encode+decode pipeline runs at 60 fps, e2e ≤ 25 ms (p50), ≤ 35 ms (p99) on the RTX 5070 + MovinkPad Pro 14 reference rig.
- Pen pressure / tilt / 3 buttons / hover / eraser all visible in Krita Windows Ink mode.
- Multi-touch pan/zoom works in Krita.
- VDD virtual monitor appears at session start, disappears at session end.
- MSI installer ≤ 30 MB.
- Works on at least one machine with each of: NVIDIA GPU, AMD GPU, Intel iGPU.
- Tests on `windows-latest` CI green.
- Clean disconnect + reconnect cycle works without a server restart.

---

## 6. Where to look for what

When in doubt, the predecessor project at `C:\repo\krita\penflow\` is your reference oracle.

| If you need… | Look in… |
|---|---|
| Working NVENC HEVC encode setup (the gold reference for tuning knobs) | `C:\repo\krita\penflow\src\penflow_core\src\encoder.cpp` |
| Working DXGI Output Duplication with ACCESS_LOST recovery | `C:\repo\krita\penflow\src\penflow_core\src\capturer.cpp` |
| WinRT pen injector reference (Python, but the WinRT calls are 1:1 in Rust) | `C:\repo\krita\penflow\server\pen_injector.py` |
| Working VDD `vdd_settings.xml` (the schema that doesn't crash) | `C:\repo\krita\zpenflow\tools\vdd\vdd_settings.xml` (already copied) |
| Working keepalive copy texture pattern | `C:\repo\krita\penflow\src\penflow_core\src\pipeline.cpp` |
| All historical wave specs / plans (architecture archeology) | `C:\repo\krita\penflow\docs\superpowers\specs\`, `C:\repo\krita\penflow\docs\superpowers\plans\` |
| Original e2e measurement journey | `C:\repo\krita\penflow\docs\HANDOFF.md` (the predecessor's own handoff doc) |
| Sunshine's encoder abstraction | `research/sunshine/src/platform/common.h` and `src/video.h` (re-clone if not present) |
| Sunshine's DXGI tricks | `research/sunshine/src/platform/windows/display_base.cpp` |
| Moonlight's MediaCodec patterns | `research/moonlight-android/app/src/main/java/com/limelight/binding/video/MediaCodecHelper.java` and `MediaCodecDecoderRenderer.java` |
| scrcpy's protocol layout | `research/scrcpy/app/src/control_msg.{c,h}` and `device_msg.{c,h}` |
| OTD's binding model | `research/opentabletdriver/OpenTabletDriver.Desktop/Binding/` |
| Android client reference (it's already in this repo) | `android/app/src/main/java/dev/penflow/` |

---

## 7. Style and process notes

The user prefers:
- **Decisive defaults over endless options**. When research clearly favors one path, propose it as recommendation, not as a multiple-choice question.
- **Action over planning**. "Auto mode" is the default; minimize gates. Stop and ask only when there's a real ambiguity (e.g., a Wave-2 gate changes the encoder, adapter/VDD, or input-injection branch).
- **Honest cost accounting**. The user notices if you wave away tradeoffs. Don't say "small" — say "+30 MB DLL bundle" or "~+1 ms latency".
- **Chinese-language feedback** is fine; technical terms stay English.
- **Don't over-engineer for hypothetical needs**. Cross-platform abstractions exist *because the user explicitly asked for macOS support*, not for theoretical purity. Hybrid NVENC+MF was rejected for the same reason.

`AGENTS.md` / `CLAUDE.md` conventions are not currently in the repo; this document is the closest thing to a style guide. When you make a non-trivial decision, document it inline in the affected file (a short comment explaining *why*, not *what* — see Sunshine's code for the genre).

When committing: meaningful subject line, body explaining *why*, `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>` trailer if AI-assisted.

---

## 8. The TL;DR

1. **Read [`design.md`](design.md)** end-to-end before touching code.
2. **Wave-2 gates are PASS on NVIDIA** (see §5.1 table). Re-run the three probes whenever you change the relevant code path; they're fast and self-checking.
3. **Implement `penflow-core` in §5.2's order**. Each core hot-path step has a working reference in `C:\repo\krita\penflow\src\penflow_core\src\`, but remember that main used direct NVENC and Python `winsdk`, not MF and Rust packaging.
4. **Apply §1.3 fixes to `android/`** at any point — the Adreno 620 crash is a real bug today.
5. Don't reach for FFmpeg, GStreamer, bindgen, libclang, or NVENC SDK directly. The design rejected all five for documented reasons.
6. Latency target: e2e ≤ 25 ms p50 on the RTX 5070 + MovinkPad rig. Predecessor hit 20 ms; this rewrite has slightly more conservative target to absorb MF overhead.

Welcome aboard. The hard architectural decisions are in `design.md`; the painful learning is here. Good luck.
