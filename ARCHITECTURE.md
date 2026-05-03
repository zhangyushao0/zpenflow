# Architecture

This is the short tour. The authoritative design is [`docs/design.md`](docs/design.md).

## Workspace layout

```
zpenflow/
├── Cargo.toml                          # workspace manifest
├── crates/
│   ├── penflow-protocol/               # wire-format constants + types
│   ├── penflow-transport/              # Transport trait + impls (ADB now, raw USB later)
│   ├── penflow-core/                   # capture + encode + WinRT inject engine
│   └── penflow-server/                 # tokio session orchestrator
├── apps/
│   └── penflow-gui/                    # Tauri 2.x desktop app
│       ├── src-tauri/                  # Rust backend
│       └── ui/                         # static HTML / CSS frontend
├── android/                            # Android client (Kotlin)
├── tools/vdd/                          # Virtual Display Driver settings
├── installer/                          # WiX MSI sources (future)
└── docs/                               # design + research notes
```

## Crate boundaries

| Crate | Responsibility | Depends on (internal) |
|---|---|---|
| `penflow-protocol` | Wire-format encode/decode, message-id constants. Leaf. | — |
| `penflow-transport` | `Transport` trait + concrete transports. | — |
| `penflow-core` | DXGI capture, MF encode, WinRT pen + touch inject, time-sync. | `protocol` |
| `penflow-server` | Tokio session loop: handshake → dispatch → telemetry. | `core`, `protocol`, `transport` |
| `penflow-gui` | Tauri app — owns the engine in-process (no IPC). | `server`, `core` |

## The Transport trait

```rust
#[async_trait]
pub trait Transport: Send + Sync {
    async fn accept(&self) -> io::Result<TransportStream>;
    async fn shutdown(&self) -> io::Result<()>;
}
```

`TransportStream` carries split `AsyncRead` + `AsyncWrite` halves plus a peer label. Swapping ADB for raw USB is one new file in `penflow-transport` plus a matching Android-side change. No other crate moves.

## The Encoder trait (per-platform)

```rust
pub trait EncoderBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_codecs(&self) -> &[Codec];
    fn make_session(&self, device: &D3d11Device, cfg: SessionConfig) -> Result<Box<dyn EncodeSession>>;
}

pub trait EncodeSession: Send {
    fn submit_frame(&mut self, tex: &PlatformTexture, pts_ns: i64, force_idr: bool) -> Result<()>;
    fn poll_packet(&mut self) -> Result<Option<EncodedPacket>>;
    fn sequence_header(&self) -> Vec<u8>;
    fn request_idr(&mut self);
}
```

Two-level pattern (backend + session) inspired by Sunshine. Windows backend is Media Foundation (`encoder/mf.rs`); macOS backend (`encoder/videotoolbox.rs`) lands post-v1.0.
