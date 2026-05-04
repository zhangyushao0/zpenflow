//! Transport abstraction over the PC↔Android byte stream.
//!
//! See `docs/design.md` §8 for the full transport design. Today: one
//! implementation, [`adb::AdbLocalAbstractTransport`], which exposes a TCP
//! port over an ADB reverse tunnel to a `localabstract:` socket on the
//! device. Future raw-USB transport adds a single new module without
//! touching the consumers.

#![deny(missing_docs)]

use std::io;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(any(unix, windows))]
pub mod adb;
// `pub mod usb_aoa` was removed — the AOA path produced persistent block
// artifacts on Snapdragon 8s Gen 3 / Adreno 720 that we couldn't fix at
// the software layer (see `docs/usb-aoa-tearing.md` for the full diagnostic
// log). Recover via `git log --diff-filter=D -- crates/penflow-transport/src/usb_aoa.rs`
// if you want to re-attempt.

/// One bidirectional byte stream to the connected Android client.
///
/// `reader` and `writer` are split so the protocol layer can read on one task
/// and write on another without holding a single lock across `.await` points.
pub struct TransportStream {
    /// Inbound half (frames produced by the Android client).
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    /// Outbound half (frames bound for the Android client).
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// Human-readable peer identifier for logs / telemetry.
    /// Examples: `"adb:127.0.0.1:1234"`, `"usb:VID_054C&PID_xxxx"`.
    pub peer_label: String,
}

/// Listener for one Android client at a time.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Block until exactly one Android client connects and any
    /// transport-level handshake completes. Returns the framed stream.
    async fn accept(&self) -> io::Result<TransportStream>;

    /// Release transport-level resources (close listening sockets, release
    /// USB interface, etc.). Called on shutdown.
    async fn shutdown(&self) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{empty, sink, AsyncReadExt, AsyncWriteExt};

    /// Trivial `Transport` impl whose stream reads EOF and discards writes.
    /// Exists only to prove the trait is implementable from outside the crate
    /// using stable types.
    struct NullTransport;

    #[async_trait]
    impl Transport for NullTransport {
        async fn accept(&self) -> io::Result<TransportStream> {
            Ok(TransportStream {
                reader: Box::new(empty()),
                writer: Box::new(sink()),
                peer_label: "null".into(),
            })
        }

        async fn shutdown(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn trait_is_dyn_dispatchable_and_stream_round_trips() {
        let t: Box<dyn Transport> = Box::new(NullTransport);
        let mut stream = t.accept().await.expect("accept");
        assert_eq!(stream.peer_label, "null");

        stream.writer.write_all(b"hello").await.expect("write");

        let mut buf = [0u8; 8];
        let n = stream.reader.read(&mut buf).await.expect("read");
        assert_eq!(n, 0, "empty() reader should report EOF immediately");

        t.shutdown().await.expect("shutdown");
    }
}
