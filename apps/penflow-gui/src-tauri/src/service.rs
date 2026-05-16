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

use penflow_core::inject::binding::{Binding as CoreBinding, MouseButtonKind, PenButtonProfile};
use penflow_core::Engine;
use penflow_server::{Session, SessionConfig, SessionEvent, VddController};
use penflow_transport::adb::AdbLocalAbstractTransport;
use penflow_transport::Transport;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT,
    VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP,
};

use crate::settings::{
    self, write_installed_vdd_settings, MouseButton as SettingsMouseButton, PenBindings,
    SharedSettings,
};

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
            // Channel for forwarding this loop's `cancel` parameter into
            // the session as `finish`. Needed because `adb reverse`
            // doesn't propagate FIN — without it Android stays stuck on
            // a dead socket after stop().
            let (session_finish_tx, session_finish_rx) = tokio::sync::oneshot::channel::<()>();
            // Scope so dropping `session_run` cancels any in-flight
            // accept() — otherwise transport.shutdown() below would
            // deadlock waiting for the listener mutex.
            let cancelled = {
                let session = Session::new(cfg);
                let session_run = session.run(
                    Arc::clone(&transport),
                    Some(tx),
                    Some(session_finish_rx),
                );
                tokio::pin!(session_run);
                // Phase 1: either the session ends on its own (Android
                // disconnects → read loop EOF → cleanup) or the user
                // clicks Pause (cancel fires). Whichever happens first
                // wins; on Pause we signal the session to wrap up and
                // Phase 2 below awaits its goodbye + cleanup.
                let cancelled = tokio::select! {
                    r = &mut session_run => {
                        match r {
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
                        }
                        false
                    }
                    _ = &mut cancel => {
                        eprintln!("[service] cancel requested — signaling session to finish");
                        let _ = session_finish_tx.send(());
                        true
                    }
                };
                // Phase 2: bounded wait for the session to finish its
                // goodbye write + cleanup. If the session was pre-handshake
                // (stuck on accept), this just times out and the drop on
                // scope exit handles teardown.
                if cancelled {
                    match tokio::time::timeout(Duration::from_secs(3), &mut session_run).await {
                        Ok(_) => eprintln!("[service] session honored finish signal"),
                        Err(_) => {
                            eprintln!("[service] session finish timed out — proceeding to teardown");
                            log_diagnostic("[service] session finish timed out");
                        }
                    }
                }
                cancelled
            };
            if cancelled {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    transport.shutdown(),
                )
                .await;
                event_pump.abort();
                return;
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

    // Duplicate captures the user's primary monitor directly, so the
    // bundled VDD virtual display should not be attached. Disable it if
    // a previous Extend session (or the MSI installer, which enables it
    // by default — see installer/wxs/vdd-install.wxs) left it on.
    // Without this, the user sees a phantom second monitor AND the
    // `attached` selection below could pick the VDD as the capture
    // target instead of the real primary.
    if matches!(s.topology, settings::TopologyMode::Duplicate) {
        let leftover_vdd = Engine::list_monitors()
            .map(|ms| {
                ms.into_iter()
                    .any(|m| m.attached_to_desktop && m.looks_virtual)
            })
            .unwrap_or(false);
        if leftover_vdd {
            eprintln!(
                "[service] Duplicate mode — leftover VDD attached, disabling so pen targets a real monitor"
            );
            match VddController::detect() {
                Ok(Some(mut ctrl)) => {
                    if let Err(e) = ctrl.disable() {
                        eprintln!("[service] VDD disable failed: {e} (continuing)");
                    } else {
                        eprintln!("[service] VDD disabled");
                    }
                }
                Ok(None) => {
                    eprintln!("[service] no VDD controller found despite virtual monitor present — proceeding anyway");
                }
                Err(e) => eprintln!("[service] VDD detect failed: {e} (continuing)"),
            }
        }
    }

    // In Duplicate, exclude virtual monitors from selection. Normally
    // redundant after the disable above, but kept as a fallback for
    // when that failed (UAC denied, no controller found).
    let monitors = Engine::list_monitors().unwrap_or_default();
    let attached = if matches!(s.topology, settings::TopologyMode::Duplicate) {
        monitors
            .iter()
            .find(|m| m.attached_to_desktop && !m.adapter_is_software && !m.looks_virtual)
            .cloned()
            .unwrap_or_else(|| monitors.first().cloned().unwrap_or_else(stub_monitor))
    } else {
        monitors
            .iter()
            .find(|m| m.attached_to_desktop && !m.adapter_is_software)
            .cloned()
            .unwrap_or_else(|| monitors.first().cloned().unwrap_or_else(stub_monitor))
    };

    // Best-effort VDD detection. If it fails or isn't installed, we fall
    // back to capturing whatever physical monitor was selected. Duplicate
    // mode skips detection so the primary is captured directly (IDDCx
    // clone is too unreliable to drive the VDD as a mirror).
    let vdd = if matches!(s.topology, settings::TopologyMode::Duplicate) {
        eprintln!("[service] Duplicate mode — bypassing VDD");
        None
    } else {
        match VddController::detect() {
            Ok(opt) => opt,
            Err(e) => {
                eprintln!("[service] VDD detection failed: {e}");
                None
            }
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

    let vdd_target_resolution = vdd
        .as_ref()
        .map(|_| (s.vdd_resolution.width, s.vdd_resolution.height));

    SessionConfig {
        monitor: attached,
        codec: s.codec.into(),
        bitrate_bps: s.bitrate_bps,
        fps: s.fps,
        idr_interval: None,
        motion_idr_threshold_bytes: None,
        motion_idr_min_interval: Duration::from_millis(250),
        vdd,
        vdd_target_resolution,
        hud_enabled: s.hud_enabled,
        // Screen-off requires Duplicate — blanking the tablet in Extend
        // mode would leave the user with no view of the VDD desktop.
        screen_off: s.screen_off && matches!(s.topology, settings::TopologyMode::Duplicate),
        pen_profile: build_pen_profile(&s.bindings),
    }
}

/// Convert the GUI's user-edited `settings::PenBindings` into the engine's
/// runtime `PenButtonProfile` (issue #6). The settings layer stores key
/// names as strings (`"Ctrl"`, `"Ctrl+E"`) for editor portability; the
/// engine wants `VIRTUAL_KEY` constants.
fn build_pen_profile(b: &PenBindings) -> PenButtonProfile {
    PenButtonProfile {
        barrel_1: convert_binding(&b.button_0),
        barrel_2: convert_binding(&b.button_1),
        tertiary: convert_binding(&b.button_2),
        tip_threshold: 0.0,
    }
}

fn convert_binding(b: &settings::Binding) -> CoreBinding {
    match b {
        settings::Binding::None => CoreBinding::None,
        settings::Binding::EraserToggle => CoreBinding::EraserToggle,
        settings::Binding::MouseButton { button } => CoreBinding::MouseButton(match button {
            SettingsMouseButton::Left => MouseButtonKind::Left,
            SettingsMouseButton::Right => MouseButtonKind::Right,
            SettingsMouseButton::Middle => MouseButtonKind::Middle,
        }),
        settings::Binding::KeyTap { key } => match parse_key_combo(key) {
            Some(keys) if keys.len() == 1 => CoreBinding::KeyTap(keys[0]),
            Some(keys) if keys.len() > 1 => CoreBinding::KeyChord(keys),
            _ => {
                eprintln!("[bindings] unrecognised KeyTap spec '{key}'; mapping to None");
                CoreBinding::None
            }
        },
        settings::Binding::KeyHold { key } => match parse_key_combo(key) {
            Some(keys) if !keys.is_empty() => CoreBinding::KeyHold(keys),
            _ => {
                eprintln!("[bindings] unrecognised KeyHold spec '{key}'; mapping to None");
                CoreBinding::None
            }
        },
        settings::Binding::KeyChord { keys } => {
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                match parse_key_token(k) {
                    Some(vk) => out.push(vk),
                    None => {
                        eprintln!("[bindings] unrecognised KeyChord token '{k}'; skipping");
                    }
                }
            }
            if out.is_empty() {
                CoreBinding::None
            } else {
                CoreBinding::KeyChord(out)
            }
        }
    }
}

/// Parse a `+`-separated key combo like "Ctrl+Shift+E" into ordered VKs.
/// Returns `None` if any token fails to resolve. Empty input → `Some(vec![])`
/// — caller should treat that the same as an unrecognised binding.
fn parse_key_combo(spec: &str) -> Option<Vec<VIRTUAL_KEY>> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for tok in trimmed.split('+') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        out.push(parse_key_token(t)?);
    }
    Some(out)
}

/// Map one key name (modifier or single key) to a Win32 `VIRTUAL_KEY`.
/// Case-insensitive on letter / modifier names. Returns `None` for
/// unknown tokens — caller decides the fallback (skip / abort the
/// whole combo / log).
fn parse_key_token(name: &str) -> Option<VIRTUAL_KEY> {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        // Modifiers, including the JS KeyboardEvent.code aliases the UI
        // can produce ("Meta" → Win key on macOS-style hardware).
        "CTRL" | "CONTROL" => Some(VK_CONTROL),
        "SHIFT" => Some(VK_SHIFT),
        "ALT" | "MENU" | "OPTION" => Some(VK_MENU),
        "WIN" | "META" | "LWIN" | "CMD" | "COMMAND" | "SUPER" => Some(VK_LWIN),
        // Whitespace + edit keys.
        "SPACE" | " " => Some(VK_SPACE),
        "TAB" => Some(VK_TAB),
        "ENTER" | "RETURN" => Some(VK_RETURN),
        "ESC" | "ESCAPE" => Some(VK_ESCAPE),
        "BACKSPACE" | "BACK" => Some(VK_BACK),
        "DEL" | "DELETE" => Some(VK_DELETE),
        "INS" | "INSERT" => Some(VK_INSERT),
        "HOME" => Some(VK_HOME),
        "END" => Some(VK_END),
        "PAGEUP" | "PRIOR" => Some(VK_PRIOR),
        "PAGEDOWN" | "NEXT" => Some(VK_NEXT),
        // Arrow keys, including KeyboardEvent.key style ("ArrowUp").
        "UP" | "ARROWUP" => Some(VK_UP),
        "DOWN" | "ARROWDOWN" => Some(VK_DOWN),
        "LEFT" | "ARROWLEFT" => Some(VK_LEFT),
        "RIGHT" | "ARROWRIGHT" => Some(VK_RIGHT),
        // F1..F24.
        s if s.starts_with('F') && s.len() <= 3 => {
            let n: u16 = s[1..].parse().ok()?;
            if (1..=24).contains(&n) {
                // VK_F1 = 0x70, VK_F2 = 0x71, … VK_F24 = 0x87.
                Some(VIRTUAL_KEY(0x6F + n))
            } else {
                None
            }
        }
        // Single ASCII letter A..Z → VK code = the byte itself.
        s if s.len() == 1 && s.chars().all(|c| c.is_ascii_alphabetic()) => {
            Some(VIRTUAL_KEY(s.as_bytes()[0] as u16))
        }
        // Single digit 0..9 → VK code = the byte itself.
        s if s.len() == 1 && s.chars().all(|c| c.is_ascii_digit()) => {
            Some(VIRTUAL_KEY(s.as_bytes()[0] as u16))
        }
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_E, VK_F1};

    #[test]
    fn parses_letters_case_insensitively() {
        assert_eq!(parse_key_token("E"), Some(VK_E));
        assert_eq!(parse_key_token("e"), Some(VK_E));
    }

    #[test]
    fn parses_modifier_aliases() {
        assert_eq!(parse_key_token("Ctrl"), Some(VK_CONTROL));
        assert_eq!(parse_key_token("Control"), Some(VK_CONTROL));
        assert_eq!(parse_key_token("Meta"), Some(VK_LWIN));
        assert_eq!(parse_key_token("Win"), Some(VK_LWIN));
    }

    #[test]
    fn parses_function_keys_in_range() {
        assert_eq!(parse_key_token("F1"), Some(VK_F1));
        assert_eq!(parse_key_token("F24"), Some(VIRTUAL_KEY(0x87)));
        assert_eq!(parse_key_token("F25"), None);
        assert_eq!(parse_key_token("F0"), None);
    }

    #[test]
    fn parses_digit_keys() {
        assert_eq!(parse_key_token("5"), Some(VIRTUAL_KEY(b'5' as u16)));
    }

    #[test]
    fn rejects_unknown_token() {
        assert_eq!(parse_key_token("HyperUltra"), None);
    }

    #[test]
    fn key_combo_splits_on_plus() {
        let v = parse_key_combo("Ctrl+Shift+E").unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], VK_CONTROL);
        assert_eq!(v[1], VK_SHIFT);
        assert_eq!(v[2], VK_E);
    }

    #[test]
    fn key_combo_returns_none_when_any_token_invalid() {
        assert!(parse_key_combo("Ctrl+Nope").is_none());
    }

    #[test]
    fn convert_keyhold_single_key_keeps_keyhold() {
        let b = settings::Binding::KeyHold { key: "Ctrl".into() };
        match convert_binding(&b) {
            CoreBinding::KeyHold(keys) => assert_eq!(keys.as_slice(), &[VK_CONTROL]),
            other => panic!("expected KeyHold([VK_CONTROL]), got {other:?}"),
        }
    }

    #[test]
    fn convert_keyhold_multi_key_preserves_full_combo() {
        let b = settings::Binding::KeyHold {
            key: "Ctrl+Shift".into(),
        };
        match convert_binding(&b) {
            CoreBinding::KeyHold(keys) => {
                assert_eq!(keys.as_slice(), &[VK_CONTROL, VK_SHIFT]);
            }
            other => panic!("expected KeyHold([VK_CONTROL, VK_SHIFT]), got {other:?}"),
        }
    }

    #[test]
    fn convert_keytap_with_combo_falls_back_to_chord() {
        let b = settings::Binding::KeyTap {
            key: "Ctrl+S".into(),
        };
        match convert_binding(&b) {
            CoreBinding::KeyChord(keys) => {
                assert_eq!(keys.len(), 2);
                assert_eq!(keys[0], VK_CONTROL);
            }
            other => panic!("expected KeyChord, got {other:?}"),
        }
    }

    #[test]
    fn convert_mouse_button_passes_kind() {
        let b = settings::Binding::MouseButton {
            button: SettingsMouseButton::Right,
        };
        match convert_binding(&b) {
            CoreBinding::MouseButton(MouseButtonKind::Right) => {}
            other => panic!("expected MouseButton(Right), got {other:?}"),
        }
    }
}
