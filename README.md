<div align="center">

# Penflow

**Turn your Wacom Movink Pad Pro 14 into a real Windows pen display — full pressure, tilt, and Windows Ink — over a USB cable.**

[![CI](https://github.com/zhangyushao0/zpenflow/actions/workflows/ci.yml/badge.svg)](https://github.com/zhangyushao0/zpenflow/actions/workflows/ci.yml)
[![Release](https://github.com/zhangyushao0/zpenflow/actions/workflows/release.yml/badge.svg)](https://github.com/zhangyushao0/zpenflow/actions/workflows/release.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2B-0078d4.svg)](#platform-support)

[English](README.md) · [简体中文](README.zh-CN.md)

</div>

---

## What is Penflow?

Penflow streams your Windows desktop to a **Wacom Movink Pad Pro 14** over USB and feeds the tablet's pen events back to the PC as **first-class Windows Ink** input — pressure (8192 levels), tilt, hover, and all three Pro Pen 3 side switches preserved. End-to-end latency is **~26 ms**.

Penflow is a free, open-source replacement for **[Wacom Instant Pen Display Mode](https://community.wacom.com/en-sg/how-to-use-instant-pen-display-mode-movinkpad-tablet/)** — Wacom's own (currently beta, MovinkPad-exclusive) PC connection app. Compared with Wacom's first-party path, Penflow:

- Drives a **120 Hz virtual display at the tablet's exact 2880×1800 panel resolution by default** — so what you see on the tablet can be your desktop at 1:1 native pixels with no resampling. The GUI can also publish a lower custom VDD resolution for lighter encode load. Wacom IPD mirrors-and-scales whatever your source monitor is to fit the panel; there is no native-resolution toggle in its settings UI ([Gigazine, Dec 2025](https://gigazine.net/gsc_news/en/20251206-instant-pen-display-mode/) noted curves blurring under upscaling). The pipeline also runs at 120 Hz vs Wacom's 60 Hz, end-to-end.
- Cuts pen-to-pixel latency from a measured **~60–70 ms** on Wacom's app to **~26 ms** on Penflow on the same rig.
- Exposes **all three Pro Pen 3 side switches** with fully customizable per-button bindings (Tap key / Hold key / Mouse button / Eraser toggle). Wacom's PC mode doesn't surface the side switches to Windows at all.
- Is **open source** and configurable down to the wire format.

> **Status**: pre-v1.0, actively developed. Currently Windows-only; macOS host support is on the [roadmap](#roadmap).

## Performance

| Metric | Value | Conditions |
|---|---|---|
| **End-to-end latency** (pen-tip to pixel) | **~26 ms** | RTX 5070 • USB 2.0 OTG • HEVC 50 Mbps • 120 Hz capture |
| Capture → encode | ~6 ms | DXGI Desktop Duplication, NVENC HEVC |
| Wire (USB ADB tunnel) | ~3 ms | Reverse-tunnel local-abstract socket |
| Decode → display | ~10 ms | MediaCodec (Android) async, surface-bound |
| Pen event → injection | ~7 ms | One-frame budget on a 120 Hz capture |

Latency was measured with a high-speed camera comparing tip-touch to pixel-update on the host, then independently validated with the in-app HUD.

### GPU support

The encoder uses Windows Media Foundation's hardware-MFT path and selects the matching encoder by adapter Vendor ID, so all three desktop GPU vendors are covered in code:

| Vendor | Encoder MFT | Status |
|---|---|---|
| **NVIDIA** | NVENC | ✅ Daily-driver-tested (RTX 5070) |
| Intel | Quick Sync (QSV) | 🟡 Code path exists; **not yet validated** on real hardware |
| AMD | AMF | ✅ Daily-driver-tested (RX 6600) |

If you run Penflow on Intel Arc / iGPU or Radeon and it works (or doesn't), please file an issue with `dxdiag` output — that's how we close out the matrix.

## Features

- 🎯 **Real Windows Ink** — pressure (8192 levels), tilt, hover, eraser. Apps see a Wintab/HID-compatible digitizer, not a synthesised mouse.
- 🖊️ **All three Pro Pen 3 buttons, configurable** — Switch 1 / Switch 2 / Switch 3 each map to *Tap key* / *Hold key* / *Mouse button* / *Eraser toggle*. Krita-friendly defaults out of the box.
- 🚀 **HEVC over GPU** — DXGI Desktop Duplication captures the desktop directly to a D3D11 texture; the encoder MFT runs on the same texture without a system-RAM round trip.
- 🔌 **USB-only path** — runs on top of `adb reverse`, so no Wi-Fi setup, no NAT, no per-network config. Plug in, launch, draw.
- 🖥️ **120 Hz virtual display** — bundled `MttVDD` exposes the tablet as a separate 120 Hz extended desktop, not a 60 Hz mirror of your primary monitor. The whole pipeline (capture → encode → decode → present) runs at 120 Hz; pen strokes feel 2× smoother than Wacom's 60 Hz path.

## Why Penflow?

The two PC-driving-the-Movink-Pad options today, side by side:

| | Penflow | Wacom Instant Pen Display |
|---|:---:|:---:|
| **Direction** | PC → tablet | PC → tablet |
| **Pen pressure / tilt** | ✅ Windows Ink | ✅ Windows Ink |
| **3 Pro Pen 3 side buttons** | ✅ all three, configurable | ❌ not exposed in IPD mode |
| **Native 1:1 panel resolution (no resampling)** | ✅ (VDD defaults to 2880×1800; GUI-adjustable) | ❌ scaled mirror; no native-resolution mode in IPD UI |
| **Refresh rate** | **120 Hz** | 60 Hz |
| **Latency (wired)** | **~26 ms** | ~60–70 ms |
| **Transport** | USB (ADB) | USB or Wi-Fi |
| **Open source** | ✅ MIT/Apache | ❌ |
| **Cost** | Free | Free (beta) |

> **About the latency and refresh-rate numbers**: both are measured by the project author on the same rig (Movink Pad Pro 14 + RTX 5070, USB cable). Wacom does not publish an official pen-to-pixel latency for Instant Pen Display Mode, so the 60–70 ms figure is **our own measurement, not a vendor spec**. If your numbers come out different, please [open an issue](https://github.com/zhangyushao0/zpenflow/issues) — we'll update the table.

## Platform support

| Host (PC) | Status |
|---|---|
| Windows 11 (x64) | ✅ Supported, primary target |
| Windows 10 22H2 (x64) | ✅ Supported (Mica falls back to opaque) |
| Windows on ARM | 🟡 Should build; not tested |
| macOS (Apple Silicon) | 🟡 Roadmap — see below |
| Linux | ❌ Not planned for v1.x |

| Tablet (client) | Status |
|---|---|
| **Wacom Movink Pad Pro 14** | ✅ Reference device, daily-driver-tested |
| Wacom Movink Pad 11 | 🟡 Same Pro Pen 3 + Android base; should work, untested |
| Other Android tablets (Android 11+) | 🟡 Should work if the digitiser exposes pressure via Android InputDevice; YMMV |
| iPad | ❌ Use Astropad / Duet — Apple's sandbox blocks the USB transport Penflow needs |

### Roadmap

- **v0.x** (now): Windows host stabilisation. VDD auto-install, pen-button binding UI, system tray + scheduled-task elevation.
- **v1.0**: Full Wintab packet support (legacy app compatibility), per-app preset profiles, frame-pacing tuner, validated Intel QSV / AMD AMF paths.
- **v1.x**: macOS (Apple Silicon) host port. The encoder abstraction has a stub `videotoolbox.rs` slot already; the capture side moves to `ScreenCaptureKit` and pen injection to `IOHIDManager`. Same Android client, same protocol.
- **v2.x**: Raw USB transport (no ADB dependency) — see `crates/penflow-transport/` archived AOA path for prior art.

## Quick install (Windows)

1. Download the latest **Penflow_*.msi** from the [Releases page](https://github.com/zhangyushao0/zpenflow/releases).
2. Run it. The installer registers Penflow under Programs and installs the Virtual Display Driver in one go (single UAC prompt).
3. On your Movink Pad Pro 14:
   - Enable **Developer options** → **USB debugging**.
   - Install the [Penflow Android client](https://github.com/zhangyushao0/zpenflow/releases) APK (also attached to each release).
4. Connect the tablet via USB. Approve the *Allow USB debugging from this computer* prompt on the tablet.
5. Launch **Penflow** from the Start Menu. The status badge flips to *connected* once the Android app handshakes.

## Build from source

### Prerequisites

- Windows 10 22H2+ or Windows 11 (x64)
- [Rust stable](https://rustup.rs) ≥ 1.76
- [Node.js](https://nodejs.org) 20.x
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`
- WebView2 Runtime (preinstalled on Win11; auto-installed by the MSI on Win10)
- Android Studio Hedgehog+ (only for the Android client)

### Build everything

```powershell
git clone https://github.com/zhangyushao0/zpenflow.git
cd zpenflow

# One-time: download MttVDD + devcon.exe from the upstream release.
# The MSI bundle references these files; they're not committed.
powershell -ExecutionPolicy Bypass -File installer/fetch-vdd.ps1

# Front-end deps (one-time)
cd apps/penflow-gui/ui
npm install
cd ../../..

# Workspace check
cargo build --workspace
cargo test --workspace

# Build the MSI
cd apps/penflow-gui
cargo tauri build --bundles msi
# → target/release/bundle/msi/Penflow_<version>_x64_en-US.msi
```

### Run the GUI in dev mode (no MSI)

```powershell
cd apps/penflow-gui
cargo tauri dev
```

## Architecture overview

```
zpenflow/
├── crates/
│   ├── penflow-protocol/        wire-format constants + types
│   ├── penflow-transport/       Transport trait + ADB impl
│   ├── penflow-core/            DXGI capture, MF/NVENC encode, WinRT pen inject
│   └── penflow-server/          tokio session orchestrator + VDD lifecycle
├── apps/penflow-gui/            Tauri 2 + React + Fluent UI desktop app
├── android/                     Android client (Kotlin)
├── installer/                   WiX MSI fragment + VDD fetcher
└── docs/                        design + research notes
```

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for crate boundaries and the `Transport` / `EncoderBackend` trait shapes, and [`docs/design.md`](docs/design.md) for the full architectural rationale.

## Documentation

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — workspace + crate map, public traits.
- [`docs/design.md`](docs/design.md) — authoritative design doc (capture pipeline, wire format, error taxonomy).
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — dev setup, lint / test commands, branching.

## Prior art & influences

Penflow is genuinely first-mover in the *PC → Wacom Movink Pad with Windows Ink* niche, but the engineering stands on shoulders:

- [**Sunshine**](https://github.com/LizardByte/Sunshine) — DXGI low-latency capture tricks, encoder abstraction.
- [**moonlight-android**](https://github.com/moonlight-stream/moonlight-android) — MediaCodec recovery and frame pacing.
- [**scrcpy**](https://github.com/Genymobile/scrcpy) — ADB tunnel and control protocol shape.
- [**OpenTabletDriver**](https://github.com/OpenTabletDriver/OpenTabletDriver) — pen-input data model, binding semantics.
- [**Virtual Display Driver**](https://github.com/VirtualDrivers/Virtual-Display-Driver) — bundled MttVDD for non-mirroring extended-desktop mode.

See [`docs/design.md`](docs/design.md) §3 for what we borrowed from each.

## Contributing

PRs welcome. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) first — short version: `cargo fmt && cargo clippy -- -D warnings && cargo test` must all pass, and follow the design doc unless your change argues a case to update it.

## License

Dual-licensed under [**MIT**](LICENSE-MIT) or [**Apache-2.0**](LICENSE-APACHE) at your option.

The bundled `MttVDD` driver is © its respective authors under MPL-2.0; `devcon.exe` is © Microsoft, redistributed under the WDK redist terms. *Wacom*, *Movink*, and *Pro Pen 3* are trademarks of Wacom Co., Ltd. Penflow is not affiliated with or endorsed by Wacom.
