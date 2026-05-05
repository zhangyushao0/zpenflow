<div align="center">

# Penflow

**Turn your Android tablet into a real Windows pen display — with full pressure, tilt, and Windows Ink — over a USB cable.**

[![CI](https://github.com/zhangyushaow/zpenflow/actions/workflows/ci.yml/badge.svg)](https://github.com/zhangyushaow/zpenflow/actions/workflows/ci.yml)
[![Release](https://github.com/zhangyushaow/zpenflow/actions/workflows/release.yml/badge.svg)](https://github.com/zhangyushaow/zpenflow/actions/workflows/release.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2B-0078d4.svg)](#platform-support)

[English](README.md) · [简体中文](README.zh-CN.md)

</div>

---

## What is Penflow?

Penflow streams your Windows desktop to an Android tablet over USB and feeds the tablet's pen events back to the PC as **first-class Windows Ink** input — pressure, tilt, hover, three barrel buttons, and palm rejection all preserved. The result: a USD-499 Wacom Movink Pad becomes a fully-fledged 14″ pen display for Krita / Photoshop / Clip Studio / Blender, **with end-to-end latency around 26 ms.**

Think of it as the OSS, vendor-neutral answer to Wacom's *EasyCanvas*, Astropad, or Duet Display — but for Android, with the source code on your side and configurable down to the pen-button level.

> **Status**: pre-v1.0, actively developed. Currently Windows-only; macOS host support is on the [roadmap](#roadmap).

## Performance

| Metric | Value | Conditions |
|---|---|---|
| **End-to-end latency** (pen-tip to pixel) | **~26 ms** | RTX 3060+ • USB 2.0 OTG • HEVC 50 Mbps • 120 Hz |
| Capture → encode | ~6 ms | DXGI Desktop Duplication, NVENC HEVC |
| Wire (USB ADB tunnel) | ~3 ms | Reverse-tunnel local-abstract socket |
| Decode → display | ~10 ms | MediaCodec (Android) async, surface-bound |
| Pen event → injection | ~7 ms | One-frame budget on a 120 Hz capture |

Latency was measured with a high-speed camera comparing tip-touch to pixel-update on the host, then independently validated with the in-app HUD.

## Features

- 🎯 **Real Windows Ink** — pressure, tilt, hover, eraser, three barrel buttons. Apps see a Wintab/HID-compatible digitizer, not a synthesised mouse.
- 🚀 **HEVC over GPU** — DXGI Desktop Duplication captures the desktop directly to a D3D11 texture, NVENC / AMF / QSV encodes without ever touching system RAM.
- 🔌 **USB-only path** — runs on top of `adb reverse`, so no Wi-Fi setup, no NAT, no per-network config. Plug in, launch, draw.
- 🖥️ **Optional Virtual Display Driver** — extend rather than mirror your desktop, so your physical monitor isn't stuck at the tablet's resolution. Bundled `MttVDD` is auto-installed by the MSI.
- 🎨 **Per-button bindings** — barrel-1 / barrel-2 / tertiary each map to *Tap key* / *Hold key* / *Mouse button* / *Eraser toggle*. Krita-friendly defaults out of the box.
- 🔐 **One-time UAC** — when you flip "Run as administrator" in settings, Penflow registers a Highest-run-level scheduled task; every subsequent launch (and the boot autostart) is silent — no UAC popups.
- 🪟 **Native Win11 look** — Mica backdrop, Fluent UI v9 controls, system-tray background service. Closing the window keeps the streaming session alive.

## Why Penflow?

| | Penflow | scrcpy | Spacedesk | EasyCanvas (Wacom) | Astropad / Duet |
|---|:---:|:---:|:---:|:---:|:---:|
| **Direction** | PC → tablet (PC drives, tablet draws) | Tablet → PC | PC → tablet | PC → tablet | PC/Mac → iPad |
| **Pen pressure / tilt** | ✅ Windows Ink | ❌ touch only | ❌ no pen | ✅ | ✅ |
| **Three barrel buttons** | ✅ all three, configurable | n/a | n/a | partial | ✅ |
| **Latency** | ~26 ms | ~50–80 ms | ~80–120 ms | ~30 ms | ~25 ms |
| **Transport** | USB (ADB) | USB / Wi-Fi | Wi-Fi | USB-C DP-Alt + USB | USB / Wi-Fi |
| **Android target** | ✅ | ✅ | ✅ | ✅ (Wacom Movink Pad only) | ❌ iPad-only |
| **Open source** | ✅ MIT/Apache | ✅ | ❌ | ❌ | ❌ |
| **Per-app key bindings** | ✅ | n/a | n/a | partial | partial |
| **Cost** | Free | Free | Free / paid | Bundled with hardware | $80+ subscription |

The closest analogue is Wacom's first-party *EasyCanvas* — but it's closed-source, hardware-locked to the Movink Pad family, and offers no scripting / binding customisation. Penflow runs the same workload on the same hardware (and on any Android tablet with a digitiser) with the source on your side.

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
| Other Android tablets (Android 11+) | 🟡 Should work if the digitiser exposes pressure via Android InputDevice; YMMV |
| iPad | ❌ Use Astropad / Duet — Apple's sandbox blocks the USB transport Penflow needs |

### Roadmap

- **v0.x** (now): Windows host stabilisation. VDD auto-install, pen-button binding UI, system tray + scheduled-task elevation.
- **v1.0**: Full Wintab packet support (legacy app compatibility), per-app preset profiles, frame-pacing tuner.
- **v1.x**: macOS (Apple Silicon) host port. The encoder abstraction has a stub `videotoolbox.rs` slot already; the capture side moves to `ScreenCaptureKit` and pen injection to `IOHIDManager`. Same Android client, same protocol.
- **v2.x**: Raw USB transport (no ADB dependency) — see `crates/penflow-transport/` archived AOA path for prior art.

## Quick install (Windows)

1. Download the latest **Penflow_*.msi** from the [Releases page](https://github.com/zhangyushaow/zpenflow/releases).
2. Run it. The installer registers Penflow under Programs and installs the Virtual Display Driver in one go (single UAC prompt).
3. On your Android tablet:
   - Enable **Developer options** → **USB debugging**.
   - Install the [Penflow Android client](https://github.com/zhangyushaow/zpenflow/releases) APK (also attached to each release).
4. Connect the tablet via USB. Approve the *Allow USB debugging from this computer* prompt on the tablet.
5. Launch **Penflow** from the Start Menu. The status badge flips to *connected* once the Android app handshakes.

## Build from source

### Prerequisites

- Windows 10 22H2+ or Windows 11 (x64)
- [Rust stable](https://rustup.rs) ≥ 1.75
- [Node.js](https://nodejs.org) 20.x
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`
- WebView2 Runtime (preinstalled on Win11; auto-installed by the MSI on Win10)
- Android Studio Hedgehog+ (only for the Android client)

### Build everything

```powershell
git clone https://github.com/zhangyushaow/zpenflow.git
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

Penflow is genuinely first-mover in the *PC → Android pen display with Windows Ink* niche, but the engineering stands on shoulders:

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

The bundled `MttVDD` driver is © its respective authors under MPL-2.0; `devcon.exe` is © Microsoft, redistributed under the WDK redist terms.
