//! ADB reverse-tunnel transport.
//!
//! The Android client opens `LocalSocket("penflow", ABSTRACT)`; ADB's reverse
//! tunnel forwards that to a TCP port on the host, which we listen on with
//! a tokio `TcpListener`. Bootstrap matches design.md §8.2:
//!
//!   1. `adb start-server` (idempotent — no-op if a daemon is already running).
//!   2. Bind a TCP listener on `127.0.0.1:0` (let the OS pick a free port).
//!   3. `adb reverse localabstract:penflow tcp:<assigned_port>`.
//!   4. `accept()` returns the first connection.
//!
//! Shutdown removes the reverse rule so subsequent runs / other tools can
//! re-bind the local-abstract name.
//!
//! HANDOFF §4.4 calls out the trap that ADB happily accepts a TCP `connect()`
//! while the Android app is still launching — the server then sits waiting
//! for `HELLO_ANDROID` that never comes. Today we punt on that (the
//! `HELLO_ANDROID` read times out and the operator restarts); design §7.3's
//! `READY_BYTE = 0xA5` probe is still TODO and lands when the Android client
//! adds support.

use std::io;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// `CREATE_NO_WINDOW` from `wincon.h`. We attach this flag to every
/// `adb.exe` spawn because the Tauri GUI ships with
/// `#![windows_subsystem = "windows"]` (no parent console), so any
/// console child without this flag pops a black `cmd`-like window for
/// a few hundred milliseconds. With reverse-tunnel rebinding running
/// 2× per accept-loop iteration, that produced a continuous flicker
/// — especially on machines without adb installed where every retry
/// failed and the error path looped fast.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Wrap `Command::new(...)` callers so every adb spawn is silent on
/// Windows; on other platforms this is a no-op pass-through.
#[cfg(windows)]
fn silent(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
fn silent(cmd: &mut Command) -> &mut Command {
    cmd
}

use async_trait::async_trait;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::{Transport, TransportStream};

/// Default `localabstract:` name. Must match
/// `PenflowClient.kt`'s `abstractName = "penflow"`.
pub const DEFAULT_ABSTRACT_NAME: &str = "penflow";

/// ADB-reverse-tunnel transport. Build with [`AdbLocalAbstractTransport::bind`].
pub struct AdbLocalAbstractTransport {
    abstract_name: String,
    /// `Mutex<Option<TcpListener>>` because `Transport::accept` takes `&self`,
    /// but `TcpListener::accept` is `&self`-callable too — so really the
    /// mutex only protects the "listener has been taken / shut down" state.
    listener: Mutex<Option<TcpListener>>,
    bound_port: u16,
    /// `adb` executable path. Lets tests / packaged installers override.
    adb_path: String,
    reverse_active: Mutex<bool>,
}

impl AdbLocalAbstractTransport {
    /// Bind a TCP listener on `127.0.0.1:0`, then run
    /// `adb reverse localabstract:<name> tcp:<port>`.
    pub async fn bind(abstract_name: impl Into<String>) -> io::Result<Self> {
        Self::bind_with_adb(abstract_name, "adb").await
    }

    /// Like [`bind`] but with a custom `adb` executable path. Used by the
    /// MSI installer to point at a bundled adb, and by tests to point at a
    /// shim.
    pub async fn bind_with_adb(
        abstract_name: impl Into<String>,
        adb_path: impl Into<String>,
    ) -> io::Result<Self> {
        let abstract_name = abstract_name.into();
        let adb_path: String = adb_path.into();

        // 1. Start the adb daemon (idempotent — `start-server` is a no-op
        //    if one is already running).
        run_adb(&adb_path, &["start-server"])?;

        // 2. Bind a TCP listener on a kernel-assigned port.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let bound_port = listener.local_addr()?.port();

        // 3. Set up the reverse tunnel.
        run_adb(
            &adb_path,
            &[
                "reverse",
                &format!("localabstract:{abstract_name}"),
                &format!("tcp:{bound_port}"),
            ],
        )?;

        Ok(Self {
            abstract_name,
            listener: Mutex::new(Some(listener)),
            bound_port,
            adb_path,
            reverse_active: Mutex::new(true),
        })
    }

    /// The PC-side TCP port that ADB is forwarding to.
    pub fn bound_port(&self) -> u16 {
        self.bound_port
    }

    /// The `localabstract:` name on the Android side.
    pub fn abstract_name(&self) -> &str {
        &self.abstract_name
    }
}

#[async_trait]
impl Transport for AdbLocalAbstractTransport {
    async fn accept(&self) -> io::Result<TransportStream> {
        let mut g = self.listener.lock().await;
        let listener = g.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "transport already shut down")
        })?;
        let (sock, peer) = listener.accept().await?;
        // Disable Nagle so small input messages (PEN_EVENT, TIME_SYNC_REQ)
        // ship immediately. ADB's USB tunnel is already low-latency; we
        // don't want the kernel to coalesce.
        sock.set_nodelay(true).ok();

        let (read_half, write_half) = sock.into_split();
        Ok(TransportStream {
            reader: Box::new(read_half),
            writer: Box::new(write_half),
            peer_label: format!("adb:{peer}"),
        })
    }

    async fn shutdown(&self) -> io::Result<()> {
        // Drop the listener first so accept() unblocks if anything is
        // currently waiting.
        {
            let mut g = self.listener.lock().await;
            *g = None;
        }
        // Best-effort remove the reverse rule. If adb is gone or the rule
        // isn't there any more, nothing useful to surface; log-and-ignore.
        let mut active = self.reverse_active.lock().await;
        if *active {
            // Use a short timeout so we don't hang shutdown on an unresponsive
            // adb daemon.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking({
                    let adb_path = self.adb_path.clone();
                    let abstract_name = self.abstract_name.clone();
                    move || {
                        let mut cmd = Command::new(&adb_path);
                        cmd.args([
                            "reverse",
                            "--remove",
                            &format!("localabstract:{abstract_name}"),
                        ])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                        let _ = silent(&mut cmd).status();
                    }
                }),
            )
            .await;
            *active = false;
        }
        Ok(())
    }
}

impl Drop for AdbLocalAbstractTransport {
    fn drop(&mut self) {
        // Best-effort cleanup. `try_lock` because we may be in a sync drop
        // context and shouldn't deadlock if shutdown() is also racing.
        if let Ok(mut active) = self.reverse_active.try_lock() {
            if *active {
                let mut cmd = Command::new(&self.adb_path);
                cmd.args([
                    "reverse",
                    "--remove",
                    &format!("localabstract:{}", self.abstract_name),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
                let _ = silent(&mut cmd).status();
                *active = false;
            }
        }
    }
}

fn run_adb(adb_path: &str, args: &[&str]) -> io::Result<Output> {
    let mut cmd = Command::new(adb_path);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = silent(&mut cmd).output().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "failed to invoke `{adb_path} {}`: {e}. Is adb on PATH?",
                args.join(" ")
            ),
        )
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Err(io::Error::other(format!(
            "adb {} failed (status {:?}): stderr={stderr}; stdout={stdout}",
            args.join(" "),
            out.status.code()
        )));
    }
    Ok(out)
}
