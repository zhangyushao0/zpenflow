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

/// Ensure any process we spawn dies when we do. Done by putting
/// ourselves in a Windows job object with `KILL_ON_JOB_CLOSE` —
/// children inherit job membership, so when our last handle to the
/// job is released on process exit, the kernel tears them down with
/// us. Targets the detached `adb fork-server` daemon, which otherwise
/// outlives Penflow and pins the USB endpoint. A pre-existing daemon
/// from Android Studio / scrcpy sits in a different job and is
/// untouched.
#[cfg(windows)]
fn mark_kill_children_on_exit() {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    struct JobHandle(#[allow(dead_code)] HANDLE);
    // HANDLE is an opaque pointer we never dereference or mutate after
    // init; safe to share across threads.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB: OnceLock<Option<JobHandle>> = OnceLock::new();
    JOB.get_or_init(|| unsafe {
        let job = match CreateJobObjectW(None, None) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[adb] CreateJobObjectW failed: {e} — adb daemon will outlive Penflow");
                return None;
            }
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size,
        ) {
            eprintln!("[adb] SetInformationJobObject failed: {e}");
            return None;
        }
        if let Err(e) = AssignProcessToJobObject(job, GetCurrentProcess()) {
            // Most likely cause: parent process is already in a job that
            // doesn't allow nesting / breakaway. Windows 8+ allows nested
            // jobs, but a debugger / installer harness can still block
            // assignment. Non-fatal — we just fall back to the old
            // behavior (daemon outlives us).
            eprintln!(
                "[adb] AssignProcessToJobObject failed: {e} — adb daemon may outlive Penflow"
            );
            return None;
        }
        Some(JobHandle(job))
    });
}

#[cfg(not(windows))]
fn mark_kill_children_on_exit() {}

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
        // Resolve scoop-style shims to their underlying executable. See
        // [`resolve_through_shim`] for the why; tl;dr: scoop's shimexe
        // wrapper falls back to console-mode handle inheritance when it
        // can't determine the target's subsystem, and a windows_subsystem
        // = "windows" parent (Penflow's release build) has no console
        // for it to inherit, which makes CreateProcess fail with
        // "Could not create process". Going straight to the underlying
        // adb.exe sidesteps the entire shim.
        let adb_path: String = resolve_through_shim(&adb_path.into());

        mark_kill_children_on_exit();

        // 1. Start the adb daemon (idempotent — `start-server` is a no-op
        //    if one is already running). On a fresh install the very
        //    first start-server invocation can take 10–30s while Windows
        //    Defender scans adb.exe and the daemon initializes its
        //    keyring; wrap in spawn_blocking so the tokio executor
        //    thread isn't pinned. Without this wrap, every other async
        //    task on that worker (Tauri command handlers, the event
        //    pump, etc.) stalls until daemon startup completes — and
        //    that's the most plausible explanation for the "first
        //    launch can't connect to ADB" symptom users hit immediately
        //    after MSI install.
        {
            let adb_path = adb_path.clone();
            tokio::task::spawn_blocking(move || run_adb(&adb_path, &["start-server"]))
                .await
                .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))??;
        }

        // 2. Bind a TCP listener on a kernel-assigned port.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let bound_port = listener.local_addr()?.port();

        // 3. Set up the reverse tunnel. Also defensively remove any
        //    stale rule from a prior crashed Penflow that didn't run
        //    its Drop impl — that rule would point at a defunct TCP
        //    port, and overwriting via plain `adb reverse` should be
        //    fine in practice but let's be explicit.
        {
            let adb_path = adb_path.clone();
            let abstract_name = abstract_name.clone();
            tokio::task::spawn_blocking(move || {
                // Best-effort: ignore errors (no rule present is fine).
                let _ = run_adb(
                    &adb_path,
                    &[
                        "reverse",
                        "--remove",
                        &format!("localabstract:{abstract_name}"),
                    ],
                );
                run_adb(
                    &adb_path,
                    &[
                        "reverse",
                        &format!("localabstract:{abstract_name}"),
                        &format!("tcp:{bound_port}"),
                    ],
                )
            })
            .await
            .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))??;
        }

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

/// Resolve a `Command::new(name)` style target through scoop's shim layer.
///
/// scoop installs binaries under `~/scoop/apps/<name>/current/...` and
/// puts thin trampoline `name.exe` wrappers in `~/scoop/shims/`. Each
/// trampoline reads a sibling `name.shim` text file containing the real
/// target path, then `CreateProcessW`s it. The trampoline runs a PE
/// header peek to decide GUI-vs-console handle wiring; on failure it
/// logs `"Could not determine if target is a GUI app. Assuming console."`
/// and tries to inherit the parent's console handles. A
/// `windows_subsystem = "windows"` parent (Penflow's release build)
/// has no console, the inheritance call returns invalid, and the whole
/// CreateProcess fails with `"Could not create process"`.
///
/// Workaround: when we detect a `.shim` file next to the resolved
/// path, parse the `path = "..."` line out of it and use that
/// directly. The real adb.exe doesn't need a console.
///
/// On non-Windows platforms this is a no-op pass-through.
#[cfg(windows)]
fn resolve_through_shim(cmd: &str) -> String {
    // Step 1: turn `cmd` into an absolute path on disk, by walking PATH
    // if it's a bare name. Mirrors how `Command::new(cmd).spawn()`
    // would search.
    let candidate: std::path::PathBuf = if cmd.contains('\\') || cmd.contains('/') {
        std::path::PathBuf::from(cmd)
    } else {
        let Some(path_var) = std::env::var_os("PATH") else {
            return cmd.to_string();
        };
        let mut found: Option<std::path::PathBuf> = None;
        'outer: for dir in std::env::split_paths(&path_var) {
            for ext in ["", ".exe", ".bat", ".cmd"] {
                let probe = dir.join(format!("{cmd}{ext}"));
                if probe.is_file() {
                    found = Some(probe);
                    break 'outer;
                }
            }
        }
        match found {
            Some(p) => p,
            None => return cmd.to_string(), // let Command::new error naturally
        }
    };

    // Step 2: if a sibling `.shim` exists (scoop convention), parse
    // `path = "..."` out of it and prefer that over the trampoline.
    let shim = candidate.with_extension("shim");
    if shim.is_file() {
        if let Ok(content) = std::fs::read_to_string(&shim) {
            for line in content.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("path") {
                    let after_eq = rest.trim_start().strip_prefix('=').unwrap_or("").trim();
                    let unquoted = after_eq.trim_matches('"');
                    if !unquoted.is_empty() && std::path::Path::new(unquoted).is_file() {
                        return unquoted.to_string();
                    }
                }
            }
        }
    }

    candidate.to_string_lossy().into_owned()
}

#[cfg(not(windows))]
fn resolve_through_shim(cmd: &str) -> String {
    cmd.to_string()
}

fn run_adb(adb_path: &str, args: &[&str]) -> io::Result<Output> {
    let mut cmd = Command::new(adb_path);
    cmd.args(args)
        // Penflow's GUI binary has windows_subsystem="windows" in release,
        // so the parent process has NO console — and therefore NO valid
        // stdin handle. Some shim wrappers (notably scoop's shimexe) try
        // to inherit and validate the parent's stdin and fail with
        // "Shim: Could not start the executable" when it's invalid.
        // Explicitly null stdin to give the child a well-defined handle.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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

#[cfg(test)]
mod resolve_through_shim_tests {
    use super::resolve_through_shim;

    /// On non-Windows targets the function is a pass-through and the
    /// scoop-style shim parsing isn't compiled, so most of the body
    /// can't be exercised; smoke-test that the input is returned as-is.
    #[cfg(not(windows))]
    #[test]
    fn pass_through_on_unix() {
        assert_eq!(resolve_through_shim("adb"), "adb");
    }

    /// Windows: build a fake `~/scoop/shims/`-style layout in tempdir,
    /// drop a `foo.exe` trampoline + `foo.shim` config, point PATH at
    /// it, and verify resolve picks the path inside the .shim.
    #[cfg(windows)]
    #[test]
    fn parses_scoop_shim_config() {
        let tmp = std::env::temp_dir().join(format!("penflow-shim-test-{}", std::process::id()));
        let shims = tmp.join("shims");
        let real_dir = tmp.join("real");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();

        let trampoline = shims.join("foo.exe");
        let shim_cfg = shims.join("foo.shim");
        let real_target = real_dir.join("foo.exe");
        std::fs::write(&trampoline, b"trampoline-bytes").unwrap();
        std::fs::write(&real_target, b"real-bytes").unwrap();
        std::fs::write(&shim_cfg, format!("path = \"{}\"\n", real_target.display())).unwrap();

        // Save + override PATH for the duration of the test.
        let saved_path = std::env::var_os("PATH");
        std::env::set_var("PATH", shims.as_os_str());

        let resolved = resolve_through_shim("foo");
        // Restore PATH BEFORE asserting so a panic doesn't leak.
        match saved_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(resolved, real_target.to_string_lossy());
    }
}
