// `windows_subsystem = "windows"` hides the spawned console on release
// builds. Debug builds keep the console attached so server logs are
// visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod os;
mod service;
mod settings;

use std::sync::{Arc, RwLock};

use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};

use crate::service::{Service, ServiceState};
use crate::settings::{Settings, SharedSettings};

struct AppState {
    settings: SharedSettings,
    service: Arc<Service>,
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings.read().expect("settings poisoned").clone()
}

#[tauri::command]
fn save_settings(state: tauri::State<'_, AppState>, new: Settings) -> Result<(), String> {
    settings::validate(&new)?;
    settings::save(&new).map_err(|e| e.to_string())?;
    *state.settings.write().expect("settings poisoned") = new.clone();

    // Apply OS-level side-effects of the (run_as_admin × autostart)
    // matrix immediately. The other settings (bitrate / fps / bindings)
    // take effect on the next session reconnect.
    //
    //   run_as_admin | autostart || HKCU Run | Scheduled task (RL=HIGHEST)
    //   -------------+-----------++---------+----------------------------
    //   false        | false     || none    | absent
    //   false        | true      || present | absent
    //   true         | false     || none    | present (no logon trigger)
    //   true         | true      || none    | present (ONLOGON trigger)
    //
    // The scheduled task variant is what makes "no UAC on subsequent
    // launches" possible: HIGHEST run-level tasks bypass the consent
    // dialog when triggered. Cost: one UAC at task-create time.
    if new.run_as_admin {
        // Drop any HKCU Run autostart — the task replaces it. Doing
        // this first means a failure to set up the task leaves the
        // user without a duplicate launcher.
        if let Err(e) = os::set_autostart(false) {
            return Err(format!("autostart cleanup failed: {e}"));
        }
        if let Err(e) = os::create_admin_task(new.autostart) {
            return Err(format!("admin task create failed: {e}"));
        }
    } else {
        // Tear down any leftover scheduled task (one UAC if currently
        // unelevated and the task exists; no-op if absent).
        if let Err(e) = os::delete_admin_task() {
            return Err(format!("admin task delete failed: {e}"));
        }
        if let Err(e) = os::set_autostart(new.autostart) {
            return Err(format!("autostart toggle failed: {e}"));
        }
    }

    // If the bundled VDD is installed, update its active settings file
    // immediately. The driver reads this on its next enable cycle, so a
    // saved resolution takes effect on the next tablet reconnect.
    if matches!(penflow_server::VddController::detect(), Ok(Some(_))) {
        settings::write_installed_vdd_settings(&new)
            .map_err(|e| format!("VDD settings write failed: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
async fn get_status(state: tauri::State<'_, AppState>) -> Result<ServiceState, String> {
    Ok(state.service.current_state().await)
}

#[tauri::command]
async fn start_service(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.service.start().await;
    Ok(())
}

#[tauri::command]
async fn stop_service(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.service.stop().await;
    Ok(())
}

#[tauri::command]
fn is_elevated() -> bool {
    os::is_elevated()
}

/// Returns true if a Virtual Display Driver is currently installed and
/// detectable by SetupAPI. The GUI uses this to decide whether to show
/// the "Install VDD" banner.
#[tauri::command]
fn is_vdd_installed() -> bool {
    matches!(penflow_server::VddController::detect(), Ok(Some(_)))
}

/// Whether the X9VoiD/vmulti-bin virtual HID digitizer is currently
/// installed and reachable. The MSI installer's
/// `PenflowInstallVmulti` custom action ships this driver at install
/// time; this probe lets the GUI surface a "driver missing" banner +
/// "Reinstall" button when that action failed (rare — typically only
/// if devcon errored or the user blocked UAC).
#[tauri::command]
fn is_vmulti_installed() -> bool {
    #[cfg(windows)]
    {
        penflow_core::inject::vmulti::VMultiPen::open().is_ok()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Manually (re-)install the bundled VMulti driver via the elevated
/// `devcon install vmulti.inf pentablet\hid`. Triggers one UAC prompt.
/// Returns Ok(()) iff devcon exits 0.
#[tauri::command]
async fn install_vmulti(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir: {e}"))?;
    let devcon = resource_dir.join("vmulti").join("devcon.exe");
    let inf = resource_dir.join("vmulti").join("vmulti.inf");
    if !devcon.exists() || !inf.exists() {
        return Err(format!(
            "bundled VMulti files not found at {} / {} — was installer/vmulti-driver populated before tauri build (fetch-vmulti.ps1)?",
            devcon.display(),
            inf.display()
        ));
    }
    let inf_arg = format!(r#""{}""#, inf.display());
    let params = format!(r"install {inf_arg} pentablet\hid");
    let devcon_clone = devcon.clone();
    let code = tokio::task::spawn_blocking(move || os::run_elevated_wait(&devcon_clone, &params))
        .await
        .map_err(|e| format!("join error: {e}"))?
        .map_err(|e| format!("run_elevated_wait: {e}"))?;
    if code != 0 {
        return Err(format!("devcon install exited {code}"));
    }
    Ok(())
}

/// Install the bundled VDD driver via the elevated pnputil helper.
/// Returns Ok(()) when pnputil completes successfully.
#[tauri::command]
async fn install_vdd(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    use tauri::Manager;
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir: {e}"))?;
    let inf = resource_dir.join("vdd").join("MttVDD.inf");
    if !inf.exists() {
        return Err(format!(
            "bundled VDD driver not found at {} — was the installer/vdd-driver folder populated before tauri build?",
            inf.display()
        ));
    }
    // Run the install on a blocking thread so the elevated helper can
    // block on UAC + pnputil without freezing the tokio runtime.
    let inf_clone = inf.clone();
    tokio::task::spawn_blocking(move || penflow_server::install_driver(&inf_clone))
        .await
        .map_err(|e| format!("join error: {e}"))?
        .map_err(|e| format!("install_driver: {e}"))?;

    // After install, write the GUI-selected VDD settings over the driver's
    // default install location so the next enable publishes the requested
    // resolution.
    let s = state.settings.read().expect("settings poisoned").clone();
    settings::write_installed_vdd_settings(&s)
        .map_err(|e| format!("VDD settings write failed: {e}"))?;
    Ok(())
}

/// Restore + focus the main window after a tray click or the "Show"
/// menu item. Handles the un-minimized + hidden combo (clicking X
/// hides; if the user had also minimized first the window is both
/// minimized AND hidden).
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn relaunch_as_admin(app: tauri::AppHandle) -> Result<(), String> {
    // Same hand-off ladder as startup: prefer the no-UAC scheduled
    // task if it exists, fall back to ShellExecuteW(runas) only when
    // the task isn't there. This way, after the first time the user
    // enables run_as_admin (which creates the task and burns one UAC
    // for that creation), every subsequent admin re-launch — settings
    // edits, fresh launches — costs zero prompts.
    if os::has_admin_task() {
        if let Err(e) = os::run_admin_task() {
            eprintln!("[gui] schtasks /Run failed: {e}; falling back to UAC");
            os::relaunch_elevated().map_err(|e| e.to_string())?;
        }
    } else {
        os::relaunch_elevated().map_err(|e| e.to_string())?;
    }
    // Schedule the current process to exit so the elevated copy takes
    // over. Give Tauri a tick to flush the command response first.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        app.exit(0);
    });
    Ok(())
}

fn main() -> std::process::ExitCode {
    // VDD helper sub-mode: when `VddController::enable()` re-launches us
    // elevated via `ShellExecuteW("runas", current_exe, "--vdd-helper
    // enable <instance>")`, we must do the device-toggle and exit — NOT
    // spawn a second Tauri window. Same dispatch trick that
    // `run_session.exe` uses.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s.as_str()) == Some("--vdd-helper") {
        return penflow_server::vdd::helper_main(&argv[1..]);
    }

    let initial_settings = settings::load();

    // Sync the autostart registry entry with the saved setting on every
    // launch so manual edits to the Run key don't drift out of band.
    if let Err(e) = os::set_autostart(initial_settings.autostart) {
        eprintln!("[gui] autostart sync failed: {e}");
    }

    // Hand off to elevated copy if the user asked for admin and we
    // aren't already there. Skip for the elevated copy itself.
    //
    // Prefer the scheduled-task path: if a Penflow task with RL=HIGHEST
    // exists (created the first time the user toggled run_as_admin),
    // `schtasks /Run` launches an elevated copy WITHOUT a UAC prompt.
    // Fall back to ShellExecuteW(runas) — which DOES prompt — only
    // when no task is registered yet (e.g. fresh install where the
    // user enabled run_as_admin but the task creation hasn't happened
    // yet, or task got deleted out-of-band).
    if initial_settings.run_as_admin && !os::is_elevated() {
        let mut handed_off = false;
        if os::has_admin_task() {
            match os::run_admin_task() {
                Ok(()) => {
                    eprintln!("[gui] handed off via scheduled task (no UAC)");
                    handed_off = true;
                }
                Err(e) => {
                    eprintln!("[gui] schtasks /Run failed: {e}; falling back to UAC path");
                }
            }
        }
        if !handed_off {
            match os::relaunch_elevated() {
                Ok(()) => {
                    eprintln!("[gui] handed off to elevated copy via UAC");
                    handed_off = true;
                }
                Err(e) => {
                    eprintln!("[gui] elevated re-launch failed (continuing unelevated): {e}");
                }
            }
        }
        if handed_off {
            std::process::exit(0);
        }
    }

    let settings: SharedSettings = Arc::new(RwLock::new(initial_settings));
    let service = Arc::new(Service::new(Arc::clone(&settings)));

    let app = tauri::Builder::default()
        .manage(AppState {
            settings: Arc::clone(&settings),
            service: Arc::clone(&service),
        })
        .setup({
            let service = Arc::clone(&service);
            move |app| {
                // Apply Win11 Mica to the main window. Falls back silently
                // on Win10 or older where Mica isn't supported (apply_mica
                // returns Err but it's not fatal).
                #[cfg(target_os = "windows")]
                if let Some(window) = app.get_webview_window("main") {
                    match window_vibrancy::apply_mica(&window, Some(true)) {
                        Ok(()) => eprintln!("[gui] Win11 Mica applied"),
                        Err(e) => eprintln!("[gui] Mica unavailable ({e}); window will be opaque"),
                    }
                }

                // Close-to-tray: clicking the window's X must hide the
                // window, not exit the process. Otherwise the background
                // service dies the moment the user closes the settings
                // panel — exactly the bug the user hit. Only the tray's
                // explicit Quit menu (or app.exit) actually shuts down.
                if let Some(window) = app.get_webview_window("main") {
                    let w = window.clone();
                    window.on_window_event(move |event| {
                        if let WindowEvent::CloseRequested { api, .. } = event {
                            api.prevent_close();
                            let _ = w.hide();
                        }
                    });
                }

                // System tray. Left-click toggles window visibility;
                // menu has explicit "Show" + "Quit". Quit exits the app
                // which fires RunEvent::ExitRequested → svc.stop().
                let tray_show = MenuItemBuilder::with_id("show", "Show Penflow").build(app)?;
                let tray_separator = PredefinedMenuItem::separator(app)?;
                let tray_quit = MenuItemBuilder::with_id("quit", "Quit Penflow").build(app)?;
                let tray_menu = MenuBuilder::new(app)
                    .items(&[&tray_show, &tray_separator, &tray_quit])
                    .build()?;

                let app_handle_menu = app.handle().clone();
                let app_handle_click = app.handle().clone();
                let _tray = TrayIconBuilder::with_id("penflow-tray")
                    .icon(app.default_window_icon().cloned().unwrap())
                    .tooltip("Penflow")
                    .menu(&tray_menu)
                    .show_menu_on_left_click(false)
                    .on_menu_event(move |_tray, event| match event.id().as_ref() {
                        "show" => show_main_window(&app_handle_menu),
                        "quit" => app_handle_menu.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(move |_tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            if let Some(w) = app_handle_click.get_webview_window("main") {
                                if w.is_visible().unwrap_or(false) {
                                    let _ = w.hide();
                                } else {
                                    show_main_window(&app_handle_click);
                                }
                            }
                        }
                    })
                    .build(app)?;

                // Auto-start the service on launch.
                let svc = Arc::clone(&service);
                tauri::async_runtime::spawn(async move {
                    svc.start().await;
                });

                // Forward every state transition as a window event.
                let svc_for_pump = Arc::clone(&service);
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let mut rx = svc_for_pump.subscribe();
                    while let Ok(state) = rx.recv().await {
                        let _ = app_handle.emit("service-state", state);
                    }
                });
                Ok(())
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_status,
            start_service,
            stop_service,
            is_elevated,
            relaunch_as_admin,
            is_vdd_installed,
            install_vdd,
            is_vmulti_installed,
            install_vmulti,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Penflow GUI");

    let cleanup_service = Arc::clone(&service);
    app.run(move |_app_handle, event| match event {
        tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => {
            // Stop the service synchronously before exit so the
            // VddController inside the running session gets dropped on
            // a live tokio runtime — its Drop impl spawns the elevated
            // helper that disables the virtual display device. Skipping
            // this leaves the virtual monitor attached to the desktop
            // until the next `enable + disable` cycle.
            let svc = Arc::clone(&cleanup_service);
            tauri::async_runtime::block_on(async move {
                svc.stop().await;
            });
        }
        _ => {}
    });

    std::process::ExitCode::SUCCESS
}
