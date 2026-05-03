# Penflow

Low-latency PC → Android pen-display bridge. Drive an Android tablet (Wacom MovinkPad Pro 14, etc.) as a Wintab/Windows-Ink pen display from a Windows PC over ADB.

**Status: pre-v1.0, in active development.**

This repository is a clean reset of an earlier prototype. The original project explored the architecture in Python + C++ (and proved it works at ~20 ms end-to-end with NVENC HEVC). Penflow proper is a Rust + Tauri rewrite, designed from day one for cross-vendor GPUs (NVIDIA / AMD / Intel) and a future macOS port.

## Quick start (Windows)

```powershell
# Prereqs: Rust stable (https://rustup.rs/), Tauri CLI:
cargo install tauri-cli --version "^2.0" --locked

# Build the workspace (no GPU needed):
cargo build --workspace

# Run the GUI shell (opens an empty window for now — engine wiring in progress):
cd apps\penflow-gui\src-tauri
cargo tauri dev
```

## What's done / what's coming

| Area | Status |
|---|---|
| Cargo workspace + Tauri scaffold + CI | ✅ |
| Wire-format constants (`penflow-protocol`) | ✅ |
| `Transport` trait reservation (`penflow-transport`) | ✅ |
| Capture + encode engine (`penflow-core`) | 🚧 in progress |
| Session orchestrator (`penflow-server`) | 🚧 next |
| Tauri GUI features | future |
| WiX MSI installer | future |
| macOS port | post-v1.0 |

## Documentation

- [`docs/design.md`](docs/design.md) — the authoritative architecture document. Read this first.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — short tour of the workspace layout.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — dev environment + workflow.

## Prior art

Penflow is genuinely first-mover in its specific niche (PC → Android pen display with full Windows Ink pressure / tilt / 3 buttons + multi-touch over ADB). Much of the design is informed by deep-reads of:

- [Sunshine](https://github.com/LizardByte/Sunshine) — PC capture + encode patterns (DXGI low-latency tricks, encoder abstraction).
- [moonlight-android](https://github.com/moonlight-stream/moonlight-android) — Android MediaCodec recovery and frame pacing.
- [scrcpy](https://github.com/Genymobile/scrcpy) — ADB tunnel + control protocol.
- [OpenTabletDriver](https://github.com/OpenTabletDriver/OpenTabletDriver) — pen-input data model.

See [`docs/design.md`](docs/design.md) §3 for what we borrowed from each.

## Licence

Dual-licensed under MIT or Apache-2.0 at the user's option.
