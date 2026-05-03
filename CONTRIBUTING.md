# Contributing

## Prerequisites (Windows)

- Rust stable (≥ 1.75) — install via [rustup.rs](https://rustup.rs/).
- Microsoft Edge WebView2 — preinstalled on Windows 10 ≥ 1809 and Windows 11.
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`.
- (Runtime only) GPU with hardware HEVC encoder support — NVIDIA, AMD, or Intel iGPU. The crate **builds** without one; only running the engine end-to-end needs it.

For the Android client (Kotlin): Android Studio Hedgehog (2023.1.1) or newer, with platform 34 + NDK r26b.

## Build the workspace

```powershell
git clone <repo-url> zpenflow
cd zpenflow
cargo build --workspace
```

## Run the GUI

```powershell
cd apps\penflow-gui\src-tauri
cargo tauri dev
```

A window titled "Penflow" opens. Engine wiring is in progress — see [`docs/design.md`](docs/design.md) for the plan.

## Tests + lints (must pass before pushing)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

GitHub Actions runs the same four commands on `windows-latest` for every push and PR.

## Hardware-dependent integration tests

Tests that require a live GPU + desktop session are marked `#[ignore]`. Run them locally before opening a PR that touches capture/encode:

```powershell
cargo test -p penflow-core -- --ignored
```

## Project conventions

- `cargo fmt --all` is the formatter. No exceptions.
- `cargo clippy -- -D warnings` is the lint bar. No new warnings, ever.
- Each commit is a working tree on its own (CI passes).
- Public APIs documented; `#![deny(missing_docs)]` enforced on crate roots where appropriate.
- See [`docs/design.md`](docs/design.md) §15 for known risks and §17 for open questions.
