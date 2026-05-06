//! Always-running background service.
//!
//! Sits in a loop:
//!
//! 1. Wait for the Android client to connect via the ADB reverse tunnel.
//! 2. Run one [`penflow_server::Session`] until the client disconnects
//!    or the session errors.
//! 3. Go back to step 1.
//!
//! User can trigger `stop` via the GUI to pause the loop (e.g. before
//! reconfiguring); `start` resumes it. Settings are re-read at the
//! start of every accept cycle so changes from the GUI take effect on
//! the next reconnect without needing a service restart.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use penflow_core::Engine;
use penflow_server::{Session, SessionConfig, SessionEvent, VddController};
use penflow_transport::adb::AdbLocalAbstractTransport;
use penflow_transport::Transport;

use crate::settings::{write_installed_vdd_settings, SharedSettings};

/// Lifecycle events emitted by the running [`Service`]. Forwarded to
/// the Tauri frontend as window events.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceState {
    /// Service is paused; no transport is open.
    Stopped,
    /// Bringing up the ADB daemon and the reverse tunnel. On a fresh
    /// install this can take 10–30s the first time — Windows Defender
    /// scans adb.exe and the daemon initializes its keyring. We
    /// surface this as its own state instead of jumping straight to
    /// `Listening` so the user can tell the difference between
    /// "ready to accept a tablet" and "still warming up".
    Preparing,
    /// Service is running and waiting for an Android client to connect.
    Listening,
    /// Transport accepted a connection; handshake in progress.
    Connecting { peer: String },
    /// Session live.
    Connected {
        peer: String,
        device_width: u16,
        device_height: u16,
    },
    /// Session ended cleanly; the loop is about to go back to listening.
    Disconnected,
    /// Recoverable error. The loop will retry after a short backoff.
    Error { message: String },
}

/// Public interface to the background service. Owned by Tauri's managed
/// state.
pub struct Service {
    inner: Mutex<Inner>,
    /// Broadcasts every state transition. Frontend subscribes; commands
    /// call `current()` for the latest snapshot.
    events: broadcast::Sender<ServiceState>,
    settings: SharedSettings,
}

struct Inner {
    /// `Some` when running; `None` when stopped.
    task: Option<JoinHandle<()>>,
    /// Sender used to ask the running task to exit.
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// Latest emitted state. Cached so newly-subscribed clients can be
    /// caught up immediately.
    last_state: ServiceState,
}

impl Service {
    pub fn new(settings: SharedSettings) -> Self {
        let (tx, _) = broadcast::channel(16);
        Self {
            inner: Mutex::new(Inner {
                task: None,
                cancel: None,
                last_state: ServiceState::Stopped,
            }),
            events: tx,
            settings,
        }
    }

    /// Subscribe to state-transition events. Each subscriber gets every
    /// new state; lagging subscribers see the channel reset.
    pub fn subscribe(&self) -> broadcast::Receiver<ServiceState> {
        self.events.subscribe()
    }

    pub async fn current_state(&self) -> ServiceState {
        self.inner.lock().await.last_state.clone()
    }

    /// Start the accept-loop if not already running.
    pub async fn start(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        if inner.task.is_some() {
            return;
        }
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let me = Arc::clone(self);
        let task = tokio::spawn(async move {
            me.run_accept_loop(cancel_rx).await;
        });
        inner.task = Some(task);
        inner.cancel = Some(cancel_tx);
    }

    /// Stop the accept-loop. Cancels any in-flight session as well.
    pub async fn stop(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(c) = inner.cancel.take() {
            let _ = c.send(());
        }
        if let Some(t) = inner.task.take() {
            // Give the loop a moment to finish naturally; abort if it
            // ignores the cancel signal.
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
        inner.last_state = ServiceState::Stopped;
        let _ = self.events.send(ServiceState::Stopped);
    }

    async fn emit(&self, s: ServiceState) {
        self.inner.lock().await.last_state = s.clone();
        let _ = self.events.send(s);
    }

    async fn run_accept_loop(self: Arc<Self>, mut cancel: tokio::sync::oneshot::Receiver<()>) {
        eprintln!("[service] accept loop started");
        loop {
            if cancel.try_recv().is_ok() {
                eprintln!("[service] cancel received, exiting");
                return;
            }

            // Preparing → bind() does the slow work (start-server +
            // reverse). We emit Listening only AFTER bind succeeds, so
            // the UI label genuinely reflects "ready for a tablet to
            // connect" rather than "we *would* be ready, but we're
            // still waiting on adb.exe".
            self.emit(ServiceState::Preparing).await;
            let adb_path = bundled_or_path_adb();
            eprintln!("[service] preparing — adb at '{adb_path}'");

            let transport: Arc<dyn Transport> =
                match AdbLocalAbstractTransport::bind_with_adb("penflow", adb_path).await {
                    Ok(t) => {
                        eprintln!("[service] adb reverse OK; bound port={}", t.bound_port());
                        self.emit(ServiceState::Listening).await;
                        Arc::new(t)
                    }
                    Err(e) => {
                        let full = format!("adb reverse failed: {e}");
                        eprintln!("[service] {full}");
                        // Also append to a debug log under %APPDATA% so the
                        // FULL stderr survives release builds (where the
                        // GUI has no console attached) and survives the
                        // UI's truncation of the error badge — which only
                        // shows the first ~80 chars and clips real
                        // diagnostic detail like "Shim: Could not start
                        // the executable" with an ellipsis.
                        log_diagnostic(&full);
                        // "adb not on PATH" can never be fixed by retrying —
                        // back off hard so we're not respawning adb 30× a
                        // minute and (pre-CREATE_NO_WINDOW fix) flashing
                        // a console window each time. Other errors
                        // (transient daemon hiccup, USB unplug) get a
                        // shorter retry.
                        let missing_binary = e.kind() == std::io::ErrorKind::NotFound
                            || e.to_string().contains("Is adb on PATH?");
                        let backoff = if missing_binary {
                            Duration::from_secs(30)
                        } else {
                            Duration::from_secs(5)
                        };
                        self.emit(ServiceState::Error { message: full }).await;
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => continue,
                            _ = &mut cancel => return,
                        }
                    }
                };

            eprintln!("[service] building session config (VDD detect…)");
            let cfg = build_session_config(&self.settings);
            eprintln!(
                "[service] session config: fallback={}x{}@{} codec={:?} vdd={}",
                cfg.monitor.width,
                cfg.monitor.height,
                cfg.fps,
                cfg.codec,
                cfg.vdd.is_some(),
            );

            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let me = Arc::clone(&self);
            let event_pump = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    eprintln!("[service] session event: {ev:?}");
                    me.emit(translate_event(ev)).await;
                }
            });

            eprintln!("[service] running session (waiting for android client)");
            let session = Session::new(cfg);
            let session_run = session.run(Arc::clone(&transport), Some(tx));
            tokio::select! {
                r = session_run => match r {
                    Ok(()) => {
                        eprintln!("[service] session ended cleanly (Disconnected)");
                        self.emit(ServiceState::Disconnected).await;
                    }
                    Err(e) => {
                        let msg = format!("session: {e}");
                        eprintln!("[service] {msg}");
                        log_diagnostic(&msg);
                        self.emit(ServiceState::Error { message: msg }).await;
                    }
                },
                _ = &mut cancel => {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2),
                        transport.shutdown(),
                    )
                    .await;
                    event_pump.abort();
                    return;
                }
            }

            // Drain the event pump and tear down the transport before
            // the next listen iteration so adb-reverse is re-bound clean.
            event_pump.abort();
            let _ = tokio::time::timeout(Duration::from_secs(2), transport.shutdown()).await;

            // Brief cool-off to avoid a tight retry loop when adb is in a
            // weird state.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                _ = &mut cancel => return,
            }
        }
    }
}

fn translate_event(ev: SessionEvent) -> ServiceState {
    match ev {
        SessionEvent::Connecting { peer } => ServiceState::Connecting { peer },
        SessionEvent::Connected {
            peer,
            device_width,
            device_height,
        } => ServiceState::Connected {
            peer,
            device_width,
            device_height,
        },
        SessionEvent::Disconnected => ServiceState::Disconnected,
        SessionEvent::Errored(e) => ServiceState::Error { message: e },
    }
}

/// Find the adb executable to use for this session.
///
/// Preference order:
///   1. **Bundled adb** at `<exe-dir>/adb/adb.exe`. The MSI installer
///      drops a private adb.exe + the two AdbWin*Api DLLs into the
///      Penflow install folder so the user doesn't need adb anywhere
///      on PATH. This is the reliable path that doesn't depend on
///      whatever the user's local Android tooling looks like.
///   2. **`adb` on PATH** as a fallback. Only hit when running against
///      `cargo tauri dev` from a checkout (where the bundled adb
///      isn't laid out next to the dev binary), or when the user
///      manually deleted Penflow's adb folder. The transport crate's
///      `resolve_through_shim` then handles scoop-style indirection
///      if applicable.
fn bundled_or_path_adb() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join("adb").join("adb.exe");
            if bundled.is_file() {
                return bundled.to_string_lossy().into_owned();
            }
        }
    }
    "adb".to_string()
}

/// Append a diagnostic line to %APPDATA%/Penflow/debug.log. Best-effort —
/// failures here are silently dropped (we don't want logging itself to be
/// the thing that hangs a service-startup error path).
fn log_diagnostic(msg: &str) {
    use std::io::Write;
    let Some(base) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
        return;
    };
    let dir = base.join("Penflow");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("debug.log");
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "[{now}] {msg}");
}

fn build_session_config(settings: &SharedSettings) -> SessionConfig {
    let s = settings.read().expect("settings poisoned").clone();

    // Pick monitor: first attached non-software output, or a stub when
    // VDD is taking over (`Session::run` ignores the field in that case).
    let monitors = Engine::list_monitors().unwrap_or_default();
    let attached = monitors
        .iter()
        .find(|m| m.attached_to_desktop && !m.adapter_is_software)
        .cloned()
        .unwrap_or_else(|| monitors.first().cloned().unwrap_or_else(stub_monitor));

    // Best-effort VDD detection. If it fails or isn't installed, we fall
    // back to capturing whatever physical monitor was selected.
    let vdd = match VddController::detect() {
        Ok(opt) => opt,
        Err(e) => {
            eprintln!("[service] VDD detection failed: {e}");
            None
        }
    };
    if vdd.is_some() {
        match write_installed_vdd_settings(&s) {
            Ok(()) => eprintln!(
                "[service] VDD settings updated: {}x{}",
                s.vdd_resolution.width, s.vdd_resolution.height
            ),
            Err(e) => eprintln!("[service] VDD settings update failed: {e}"),
        }
    }

    SessionConfig {
        monitor: attached,
        codec: s.codec.into(),
        bitrate_bps: s.bitrate_bps,
        fps: s.fps,
        idr_interval: None,
        motion_idr_threshold_bytes: None,
        motion_idr_min_interval: Duration::from_millis(250),
        vdd,
        hud_enabled: s.hud_enabled,
    }
}

fn stub_monitor() -> penflow_core::monitors::MonitorInfo {
    penflow_core::monitors::MonitorInfo {
        adapter_index: 0,
        adapter_luid: 0,
        adapter_name: String::new(),
        adapter_vendor_id: 0,
        adapter_device_id: 0,
        adapter_is_software: false,
        output_index_within_adapter: 0,
        device_name: String::new(),
        width: 0,
        height: 0,
        desktop_coords: (0, 0, 0, 0),
        rotation: 1,
        attached_to_desktop: false,
        looks_virtual: false,
    }
}
