// `windows_subsystem = "windows"` hides the spawned console on release
// builds. Debug builds keep the console attached so server logs are
// visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod os;
mod service;
mod settings;

use std::sync::{Arc, RwLock};

use tauri::Emitter;

use crate::service::{Service, ServiceState};
use crate::settings::{SharedSettings, Settings};

struct AppState {
    settings: SharedSettings,
    service: Arc<Service>,
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings.read().expect("settings poisoned").clone()
}

#[tauri::command]
fn save_settings(
    state: tauri::State<'_, AppState>,
    new: Settings,
) -> Result<(), String> {
    settings::save(&new).map_err(|e| e.to_string())?;
    *state.settings.write().expect("settings poisoned") = new.clone();

    // Apply autostart side-effect immediately. Other settings take effect
    // on the next session reconnect.
    if let Err(e) = os::set_autostart(new.autostart) {
        return Err(format!("autostart toggle failed: {e}"));
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

#[tauri::command]
fn relaunch_as_admin(app: tauri::AppHandle) -> Result<(), String> {
    os::relaunch_elevated().map_err(|e| e.to_string())?;
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
    if initial_settings.run_as_admin && !os::is_elevated() {
        match os::relaunch_elevated() {
            Ok(()) => {
                eprintln!("[gui] handed off to elevated copy; exiting unelevated process");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[gui] elevated re-launch failed (continuing unelevated): {e}");
            }
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
                // Auto-start the service on launch — the design is
                // "always running, ready to accept the next plug-in".
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
