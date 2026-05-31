//! On-demand Virtual Display Driver lifecycle.
//!
//! ## Design
//!
//! - **The user keeps the VDD device installed but disabled** in Device
//!   Manager. The server enables it after the Android handshake and
//!   disables it on disconnect, so the idle PC desktop has no extra
//!   monitor.
//! - **No PowerShell.** Device enumeration + status uses native Win32
//!   (`CM_Locate_DevNodeW`, `CM_Get_DevNode_Status`, SetupAPI's
//!   `SetupDiGetClassDevs`). The actual enable/disable mutation goes
//!   through the bundled `devcon.exe` because devcon writes a
//!   registry-persistent `CONFIGFLAG_DISABLED` flag whereas
//!   `devcon disable` is runtime-only — the latter let VDD
//!   silently re-enable on every reboot (issue #22).
//! - **Just-in-time UAC.** devcon requires Administrator. The server
//!   runs as a regular user; when it needs to flip the device it
//!   invokes itself via `ShellExecuteW` with the `runas` verb in
//!   `--vdd-helper resident <instance> <event-base> <parent-pid>`
//!   mode. Windows shows the UAC prompt; the user clicks Yes once per
//!   session. The helper does the devcon call and exits with status 0
//!   / non-zero, no IPC complexity.
//!
//! ## Why a sub-process for elevation
//!
//! Windows can't elevate an already-running unelevated process — UAC
//! always spawns a fresh process. So even if we wanted "elevate the
//! current PID", we couldn't. The simplest correct shape is: keep the
//! main server unelevated, spawn a tiny elevated helper (same exe,
//! different argv) when the device flip is actually needed.
//!
//! ## Diagnostics
//!
//! - On enable, after devcon completes, we re-read
//!   `CM_Get_DevNode_Status` so we can tell the difference between "Enable
//!   succeeded → driver is starting up" and "Enable returned OK but the
//!   driver immediately failed" (HANDOFF §2.1 `mttvdd.dll
//!   WUDFUnhandledException` symptom).
//! - On `EnumerationTimeout`, the error includes the list of monitor names
//!   that DID appear in DXGI during the wait, so the operator can see
//!   whether the new monitor came up under a name our `looks_virtual`
//!   heuristic doesn't match.

use std::env;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use thiserror::Error;
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_Locate_DevNodeW, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW,
    CM_DEVNODE_STATUS_FLAGS, CM_LOCATE_DEVNODE_NORMAL, CM_PROB, CM_PROB_DISABLED, CONFIGRET,
    CR_NO_SUCH_DEVNODE, CR_SUCCESS, DIGCF_PRESENT, DN_HAS_PROBLEM, GUID_DEVCLASS_DISPLAY, HDEVINFO,
    SETUP_DI_REGISTRY_PROPERTY, SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Display::{
    SetDisplayConfig, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_TOPOLOGY_EXTEND, SDC_VIRTUAL_MODE_AWARE,
    SDC_VIRTUAL_REFRESH_RATE_AWARE,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, CDS_UPDATEREGISTRY, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessId, OpenEventW, OpenProcessToken, SetEvent,
    SYNCHRONIZATION_ACCESS_RIGHTS,
};
/// Standard SYNCHRONIZE access right (winnt.h `STANDARD_RIGHTS_REQUIRED`
/// adjacent), required for OpenEventW so the helper can WaitForSingleObject.
const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
/// `EVENT_MODIFY_STATE = 0x0002`. Required for `SetEvent` / `ResetEvent`
/// on a handle obtained via `OpenEventW`.
const EVENT_MODIFY_STATE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0002);
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use penflow_core::d3d11::create_dxgi_factory;
use penflow_core::monitors::{self, MonitorInfo};

/// Errors from the VDD lifecycle path.
#[derive(Debug, Error)]
pub enum VddError {
    /// A Windows API call failed unexpectedly.
    #[error("Windows API: {0}")]
    Win32(#[from] windows::core::Error),

    /// Windows accepted the PnP enable, but applying the desktop topology
    /// failed. PnP enable only makes the IddCx target available; the target
    /// still has to be attached to the desktop before DXGI can capture it.
    #[error("DisplayConfig: {0}")]
    DisplayConfig(String),

    /// `CM_Locate_DevNodeW` couldn't find the device. Either the
    /// instance id is wrong or the device was uninstalled.
    #[error("device '{0}' not found by Configuration Manager")]
    DevNodeNotFound(String),

    /// A Configuration Manager call returned `CR_ACCESS_DENIED` (we're
    /// not running elevated and this code path bypassed the elevated
    /// helper — shouldn't happen in normal operation).
    #[error("Configuration Manager refused: access denied (need Administrator)")]
    AccessDenied,

    /// A Configuration Manager call returned an unexpected error code.
    #[error("Configuration Manager error: CONFIGRET=0x{0:08x}")]
    ConfigManager(u32),

    /// We enabled the device but `CM_Get_DevNode_Status` reports the
    /// node is in trouble (typically because the user-mode driver host
    /// failed to start — `mttvdd.dll WUDFUnhandledException`).
    #[error(
        "Enable succeeded at the PnP layer but the driver reports a problem afterwards. \
         status=0x{status:08x} problem=0x{problem:08x}.\n\
         {hint}"
    )]
    DriverProblem {
        /// Raw `ulStatus` from CM_Get_DevNode_Status.
        status: u32,
        /// Raw `ulProblemNumber` from CM_Get_DevNode_Status.
        problem: u32,
        /// Best-effort guess at what the problem code means + remediation.
        hint: String,
    },

    /// Re-launched ourselves with `runas` to do the privileged operation,
    /// but the helper exited non-zero. The user probably clicked No on
    /// the UAC prompt, or the helper's CM call failed. The full helper
    /// log is in `%TEMP%\penflow-vdd-helper.log`.
    #[error("elevated helper exited non-zero ({code:?}); see %TEMP%\\penflow-vdd-helper.log for the full helper trace")]
    HelperExitedNonZero {
        /// Exit code from the helper, if available.
        code: Option<i32>,
    },

    /// Enable returned success but the device is still in the
    /// `CM_PROB_DISABLED` state. Typically means `devcon enable`
    /// silently no-op'd because the calling process wasn't elevated, or
    /// the helper sub-process didn't actually run elevated.
    #[error(
        "Enable reported success but the device is still disabled. \
         Likely cause: the elevated helper didn't actually run with \
         Administrator privileges. Check %TEMP%\\penflow-vdd-helper.log \
         for the helper trace."
    )]
    EnableHadNoEffect,

    /// Disable returned success but Configuration Manager still reports
    /// the device as started/healthy.
    #[error(
        "Disable reported success but the device is still enabled. \
         status=0x{status:08x} problem=0x{problem:08x}. \
         Check %TEMP%\\penflow-vdd-helper.log for the helper trace."
    )]
    DisableHadNoEffect {
        /// Raw `ulStatus` from CM_Get_DevNode_Status.
        status: u32,
        /// Raw `ulProblemNumber` from CM_Get_DevNode_Status.
        problem: u32,
    },

    /// `ShellExecuteExW` itself failed (code path that runs in the
    /// non-elevated parent). Usually means the user clicked No on the
    /// UAC prompt — Windows reports `ERROR_CANCELLED` (1223).
    #[error("could not launch elevated helper: {0}")]
    ShellExecute(String),

    /// We enabled the VDD but DXGI didn't enumerate a virtual monitor
    /// within the wait window. Includes everything we saw so the
    /// operator can diagnose whether (a) the monitor came up under an
    /// unexpected name (heuristic miss), (b) the driver flipped back to
    /// disabled mid-wait, or (c) the driver stayed healthy but never
    /// created a monitor (vdd_settings.xml not applied / 0-monitor
    /// fallback).
    #[error(
        "VDD enabled but DXGI didn't enumerate a virtual monitor within {timeout:?}.\n\
         Monitors seen during the wait: {monitors_seen:?}\n\
         Device status timeline: {status_timeline:?}\n\
         If the timeline shows the device stayed healthy (no DN_HAS_PROBLEM bit, problem=0)\n\
         but no virtual monitor appeared, check whether DisplayConfig topology extension\n\
         succeeded. This VDD publishes a generic DXGI output name such as \\\\.\\DISPLAY27,\n\
         so Penflow detects it as the new attached output that was not present before\n\
         PnP enable. If no new output appears, check recent Application/System events for\n\
         WUDFUnhandledException or UMDF crash events involving mttvdd.dll, then enable VDD\n\
         logging and inspect C:\\VirtualDisplayDriver\\Logs for DeviceD0Entry/InitAdapter/\n\
         MonitorArrival events.\n\
         Do not use the VDD 25.7.23 RELOAD_DRIVER pipe command as automatic recovery; that\n\
         driver path has been observed to crash mttvdd.dll on this setup."
    )]
    EnumerationTimeout {
        /// How long we waited before giving up.
        timeout: Duration,
        /// All distinct adapter+output labels enumerated during the wait.
        monitors_seen: Vec<String>,
        /// Per-second snapshots of `CM_Get_DevNode_Status`.
        status_timeline: Vec<String>,
    },

    /// Walking the DXGI factory raised an error.
    #[error("DXGI enumeration error: {0}")]
    Dxgi(String),
}

/// Handle to one PnP-managed Virtual Display Driver device. `enable()` and
/// `disable()` are idempotent at the OS level (Windows is fine with
/// enabling an enabled device). `Drop` calls `disable()` if we believe
/// the device is currently enabled.
#[derive(Debug)]
pub struct VddController {
    instance_id: String,
    friendly_name: String,
    enabled: bool,
    /// In unelevated mode: a long-lived elevated helper that did the
    /// initial `devcon enable` and now waits on a named event OR the
    /// parent's process handle. We signal the event on
    /// `disable()` / `Drop` so the helper does the matching
    /// `devcon disable` and exits — costing only ONE UAC prompt (at
    /// first enable) instead of one each for enable+disable. Parent-
    /// handle watch is the crash-recovery path: parent dies → helper
    /// still runs the persistent disable.
    resident: Option<ResidentHelper>,
}

/// Live resident-mode helper. The fields aren't `Debug`-able cleanly
/// (raw HANDLEs), so we hand-implement a stub.
struct ResidentHelper {
    /// Elevated child process handle. Used to wait-for-exit on shutdown.
    process: HANDLE,
    /// Helper signals this after `devcon enable` completes. We hold
    /// it so we can re-wait on it if needed (and to keep its name reserved).
    done_event: HANDLE,
    /// We signal this to ask the helper to disable + exit.
    stop_event: HANDLE,
}

impl std::fmt::Debug for ResidentHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResidentHelper")
            .field("process", &(self.process.0 as usize))
            .field("done_event", &(self.done_event.0 as usize))
            .field("stop_event", &(self.stop_event.0 as usize))
            .finish()
    }
}

unsafe impl Send for ResidentHelper {}
unsafe impl Sync for ResidentHelper {}

/// What happened when we asked the resident helper to wind down.
/// Drives the fallback decision in `VddController::disable`. The exit
/// code in `NonZero` is surfaced through `Debug` only (eprintln in
/// `disable`) — `#[allow(dead_code)]` silences the dead-field warning
/// rustc raises because `Debug` reads aren't counted as use.
#[derive(Debug)]
enum ShutdownOutcome {
    /// Helper exited 0 — devcon disable inside the helper succeeded.
    Clean,
    /// Helper didn't exit within the 5 s window. Probably hung in
    /// devcon or its own cleanup; treat the disable as not done.
    Timeout,
    /// Helper exited but with non-zero status (or we couldn't read it).
    NonZero(#[allow(dead_code)] u32),
}

impl ResidentHelper {
    /// Signal the stop event, wait briefly for the helper to exit
    /// (so `devcon disable` actually completes before we tear down),
    /// then close handles. Caller looks at the returned outcome to
    /// decide whether to run a direct fallback disable.
    fn shutdown(&mut self) -> ShutdownOutcome {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        unsafe {
            let _ = SetEvent(self.stop_event);
        }
        let wait = unsafe { WaitForSingleObject(self.process, 5000) };
        if wait != WAIT_OBJECT_0 {
            return ShutdownOutcome::Timeout;
        }
        let mut code: u32 = 0;
        if unsafe { GetExitCodeProcess(self.process, &mut code) }.is_err() {
            return ShutdownOutcome::NonZero(code);
        }
        if code == 0 {
            ShutdownOutcome::Clean
        } else {
            ShutdownOutcome::NonZero(code)
        }
    }
}

impl Drop for ResidentHelper {
    fn drop(&mut self) {
        // If we get here without `disable()` having taken us out of the
        // option, run shutdown for cleanliness. The outcome is the
        // caller's worry — Drop just makes sure the handle isn't leaked.
        let _ = self.shutdown();
        unsafe {
            let _ = CloseHandle(self.process);
            let _ = CloseHandle(self.done_event);
            let _ = CloseHandle(self.stop_event);
        }
    }
}

impl VddController {
    /// PnP instance id (`ROOT\DISPLAY\0001` etc.). Identifies the device
    /// uniquely across the lifetime of the install.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Human-readable name (`Virtual Display Driver`, `MTT VDD`, etc.).
    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    /// Build a controller for an explicit instance id. Bypasses
    /// `detect()`'s heuristic; useful from the helper sub-mode where we
    /// receive the id verbatim from argv.
    pub fn for_instance(instance_id: impl Into<String>) -> Self {
        let id = instance_id.into();
        Self {
            instance_id: id.clone(),
            friendly_name: id,
            enabled: false,
            resident: None,
        }
    }

    /// Walk Device Manager's `Display` class via SetupAPI, pick the most
    /// likely Virtual Display Driver. Selection priority when several
    /// devices match the heuristic name keywords:
    ///   1. Currently disabled (Status & DN_HAS_PROBLEM with
    ///      ProblemCode == CM_PROB_DISABLED) — the operator's intent is
    ///      on-demand enable; an already-enabled device belongs to
    ///      something else (e.g. an emulator's virtual adapter).
    ///   2. FriendlyName contains exactly `Virtual Display Driver` —
    ///      the canonical name from the VirtualDrivers project.
    ///   3. Whatever SetupAPI returns first.
    ///
    /// Returns `Ok(None)` if no candidate was found. Doesn't require
    /// admin (read-only enumeration).
    pub fn detect() -> Result<Option<Self>, VddError> {
        let candidates = enumerate_display_devices()?;
        // Verbose listing if PENFLOW_VDD_TRACE=1 — useful when detection
        // missed an obviously-installed device.
        if std::env::var_os("PENFLOW_VDD_TRACE").is_some() {
            eprintln!(
                "[vdd-trace] enumerated {} Display-class devices:",
                candidates.len()
            );
            for c in &candidates {
                eprintln!(
                    "[vdd-trace]   id={} disabled={} name={:?}",
                    c.instance_id, c.is_disabled, c.friendly_name
                );
            }
        }
        let chosen = candidates
            .into_iter()
            .filter(|c| matches_vdd_heuristic(&c.friendly_name))
            .min_by_key(|c| {
                let status_ok = !c.is_disabled as u8; // disabled sorts first
                let canonical_no = !c
                    .friendly_name
                    .to_lowercase()
                    .contains("virtual display driver") as u8;
                (status_ok, canonical_no)
            });
        Ok(chosen.map(|c| Self {
            instance_id: c.instance_id,
            friendly_name: c.friendly_name,
            enabled: false,
            resident: None,
        }))
    }

    /// Enable the device. If the current process is elevated, just calls
    /// `devcon enable` directly. Otherwise, spawns a long-lived
    /// elevated helper (`--vdd-helper-resident <event-name> <instance>`)
    /// that does the enable AND will do the matching disable when we
    /// later signal it on `Drop`/`disable()` — keeping us at one UAC
    /// prompt total instead of one for enable + one for disable.
    pub fn enable(&mut self) -> Result<(), VddError> {
        if is_process_elevated() {
            devcon_action("enable", &self.instance_id)?;
            verify_devnode_started(&self.instance_id)?;
        } else if self.resident.is_none() {
            self.resident = Some(spawn_resident_helper(&self.instance_id)?);
            // The helper does devcon enable in its own elevated context.
            // Caller (session.rs) follows up with
            // `wait_for_virtual_monitor()` which DXGI-polls until the
            // virtual monitor actually appears, so we don't need a
            // separate "enable done" signal.
        } else {
            // Already running. Re-enable in resident mode is not yet
            // supported (would need a second signal); fall back to the
            // old per-call helper which costs another UAC prompt.
            run_helper_elevated("enable", &self.instance_id)?;
        }
        self.enabled = true;
        Ok(())
    }

    /// Disable the device. If a resident helper is alive, signal it (no
    /// UAC). Otherwise fall back to a direct devcon call (if elevated)
    /// or a one-shot elevated helper.
    ///
    /// If the resident helper is alive but doesn't acknowledge the stop
    /// signal cleanly (timed out, crashed, or exited non-zero), fall
    /// through to the direct/one-shot path so VDD doesn't get stranded
    /// in the enabled state.
    pub fn disable(&mut self) -> Result<(), VddError> {
        let needs_fallback = if let Some(mut helper) = self.resident.take() {
            match helper.shutdown() {
                ShutdownOutcome::Clean => false,
                other => {
                    eprintln!(
                        "[vdd] resident helper shutdown unclean ({other:?}); falling back to direct disable"
                    );
                    true
                }
            }
            // helper drops here, closing handles.
        } else {
            true
        };
        if needs_fallback {
            if is_process_elevated() {
                devcon_action("disable", &self.instance_id)?;
                verify_devnode_disabled(&self.instance_id)?;
            } else {
                // Best-effort: spawning another helper costs an extra
                // UAC prompt mid-disconnect, but leaving VDD attached
                // is worse — and the next-launch leftover detect in
                // main.rs is the safety net if the user dismisses UAC.
                run_helper_elevated("disable", &self.instance_id)?;
            }
        }
        self.enabled = false;
        Ok(())
    }
}

impl Drop for VddController {
    fn drop(&mut self) {
        if self.enabled {
            let _ = self.disable();
        }
    }
}

/// After enabling the VDD, Windows takes a moment to publish the new
/// monitor through DXGI. Poll the factory until a `looks_virtual`
/// attached output appears.
///
/// On timeout, report:
/// - all monitors that DID appear in DXGI during the wait (heuristic
///   miss diagnostic),
/// - the device's PnP status checkpoints (so we can tell if the driver
///   stayed healthy throughout the wait or flipped back to disabled
///   somewhere in the middle).
///
/// `instance_id` is optional: if provided we sample CM_Get_DevNode_Status
/// once per second during the wait so the timeout error includes the
/// driver's status timeline.
pub async fn wait_for_virtual_monitor(
    timeout: Duration,
    instance_id: Option<&str>,
    baseline_attached_keys: Option<&[String]>,
) -> Result<MonitorInfo, VddError> {
    let start = Instant::now();
    let mut all_seen: Vec<String> = Vec::new();
    let mut status_timeline: Vec<String> = Vec::new();
    let mut last_dxgi_err: Option<String> = None;
    let mut last_status_t = Instant::now();
    let mut last_topology_t: Option<Instant> = None;
    while Instant::now().duration_since(start) < timeout {
        // PnP enable starts the IddCx device and publishes monitor targets.
        // Windows still has to attach the newly-available target to the
        // desktop topology. DisplaySwitch.exe /extend proved this is the
        // missing step on the target machine; SetDisplayConfig is the native
        // equivalent and avoids spawning another helper process.
        if baseline_attached_keys.is_some()
            && last_topology_t
                .map(|t| t.elapsed() >= Duration::from_secs(1))
                .unwrap_or(true)
        {
            let elapsed_ms = start.elapsed().as_millis();
            match extend_desktop_to_available_displays() {
                Ok(()) => status_timeline.push(format!(
                    "+{elapsed_ms:>5}ms display topology: requested SDC_TOPOLOGY_EXTEND"
                )),
                Err(e) => status_timeline.push(format!(
                    "+{elapsed_ms:>5}ms display topology: extend failed: {e}"
                )),
            }
            last_topology_t = Some(Instant::now());
        }

        // Sample DXGI.
        match create_dxgi_factory() {
            Ok(factory) => match monitors::enumerate(&factory) {
                Ok(mons) => {
                    for m in &mons {
                        let label = format!(
                            "{} on {} ({}x{}, attached={})",
                            m.device_name, m.adapter_name, m.width, m.height, m.attached_to_desktop
                        );
                        if !all_seen.iter().any(|l| l == &label) {
                            all_seen.push(label);
                        }
                    }
                    if let Some(m) = mons.into_iter().find(|m| {
                        if !m.attached_to_desktop || m.adapter_is_software {
                            return false;
                        }
                        if m.looks_virtual {
                            return true;
                        }
                        baseline_attached_keys
                            .map(|keys| !keys.iter().any(|k| k == &monitor_key(m)))
                            .unwrap_or(false)
                    }) {
                        return Ok(m);
                    }
                }
                Err(e) => last_dxgi_err = Some(format!("enumerate: {e:?}")),
            },
            Err(e) => last_dxgi_err = Some(format!("create_dxgi_factory: {e:?}")),
        }

        // Sample device status once a second.
        if let Some(id) = instance_id {
            if last_status_t.elapsed() >= Duration::from_secs(1) {
                let elapsed_ms = start.elapsed().as_millis();
                match snapshot_devnode_status(id) {
                    Ok((s, p)) => status_timeline.push(format!(
                        "+{elapsed_ms:>5}ms status=0x{:08x} problem=0x{:08x}",
                        s.0, p.0
                    )),
                    Err(e) => status_timeline.push(format!("+{elapsed_ms:>5}ms err: {e}")),
                }
                last_status_t = Instant::now();
            }
        }

        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if let Some(e) = last_dxgi_err {
        Err(VddError::Dxgi(e))
    } else {
        Err(VddError::EnumerationTimeout {
            timeout,
            monitors_seen: all_seen,
            status_timeline,
        })
    }
}

/// Snapshot the outputs that are already attached before enabling VDD.
/// The MTT VDD presents as a generic DXGI output name (`\\.\DISPLAY27`)
/// on the physical GPU, so after topology extension the reliable signal is
/// "new attached output", not "name contains VDD".
pub fn snapshot_attached_monitor_keys() -> Result<Vec<String>, VddError> {
    let factory =
        create_dxgi_factory().map_err(|e| VddError::Dxgi(format!("create_dxgi_factory: {e:?}")))?;
    let monitors =
        monitors::enumerate(&factory).map_err(|e| VddError::Dxgi(format!("enumerate: {e:?}")))?;
    Ok(monitors
        .iter()
        .filter(|m| m.attached_to_desktop && !m.adapter_is_software)
        .map(monitor_key)
        .collect())
}

fn monitor_key(m: &MonitorInfo) -> String {
    format!("{}:{}", m.adapter_luid, m.device_name)
}

fn extend_desktop_to_available_displays() -> Result<(), VddError> {
    let flags = SDC_APPLY
        | SDC_TOPOLOGY_EXTEND
        | SDC_ALLOW_CHANGES
        | SDC_VIRTUAL_MODE_AWARE
        | SDC_VIRTUAL_REFRESH_RATE_AWARE;
    let code = unsafe { SetDisplayConfig(None, None, flags) };
    if code == 0 {
        Ok(())
    } else {
        Err(VddError::DisplayConfig(format!(
            "SetDisplayConfig(SDC_TOPOLOGY_EXTEND) returned {code}: {}",
            std::io::Error::from_raw_os_error(code)
        )))
    }
}

/// Force a specific GDI device (`\\.\DISPLAYn`) to a given mode and write
/// the change to the saved-topology database via `CDS_UPDATEREGISTRY`.
///
/// Why this exists: `SDC_TOPOLOGY_EXTEND` replays the most recently saved
/// extend topology. If the user changed the VDD's published resolution in
/// `vdd_settings.xml`, the saved topology still pins the OLD resolution
/// — Windows applies it via `SDC_VIRTUAL_MODE_AWARE` (virtual scaling) so
/// refresh rate updates correctly but resolution sticks. Calling
/// `ChangeDisplaySettingsExW` with `CDS_UPDATEREGISTRY` after the extend
/// both applies the new mode dynamically AND replaces the saved entry,
/// so subsequent enable cycles read the correct mode.
pub fn force_monitor_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<(), VddError> {
    let mut devmode: DEVMODEW = unsafe { std::mem::zeroed() };
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    devmode.dmPelsWidth = width;
    devmode.dmPelsHeight = height;
    devmode.dmDisplayFrequency = refresh_hz;
    devmode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;

    let wide: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let device_pcwstr = PCWSTR(wide.as_ptr());

    let result = unsafe {
        ChangeDisplaySettingsExW(
            device_pcwstr,
            Some(&devmode as *const DEVMODEW),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };

    if result == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(VddError::DisplayConfig(format!(
            "ChangeDisplaySettingsExW({device_name}, {width}x{height}@{refresh_hz}Hz) returned DISP_CHANGE={}",
            result.0
        )))
    }
}

/// Helper sub-mode entry point. Call from `main.rs` / `run_session.rs`
/// at the very top — before tokio, before anything else — when argv[1]
/// is `--vdd-helper`. The function does the requested CM operation and
/// returns an `ExitCode` for the helper process to surface.
///
/// Argv shape: `<exe> --vdd-helper <enable|disable> <instance_id>`.
///
/// **Logging:** the helper runs with `SW_HIDE` (no console window
/// attached by ShellExecute), so stderr/stdout are dropped. We instead
/// append a trace to `%TEMP%\penflow-vdd-helper.log` for every step.
/// When the parent reports a helper failure or `EnableHadNoEffect`, the
/// operator reads that file to see what actually happened.
pub fn helper_main(args: &[String]) -> ExitCode {
    let log = HelperLog::open();
    log.append("--- helper invoked ---");
    log.append(&format!("argv: {:?}", args));
    log.append(&format!("elevated: {}", is_process_elevated()));

    if args.len() < 3 {
        log.append("usage error: expected `--vdd-helper <enable|disable|resident> <instance_id> [event-name]`");
        return ExitCode::from(2);
    }
    let action = args[1].as_str();
    let instance_id = args[2].as_str();

    // Resident sub-mode: helper does enable now, then waits on the named
    // event OR the parent process handle for shutdown. Parent PID lets
    // us recover when the parent dies abnormally (kill, BSOD, panic
    // before Drop) — without it the helper would block forever and
    // leak the VDD enabled.
    if action == "resident" {
        if args.len() < 5 {
            log.append(
                "usage error: resident mode needs `<instance_id> <event_name> <parent_pid>`",
            );
            return ExitCode::from(2);
        }
        let event_name = args[3].as_str();
        let parent_pid: u32 = args[4].parse().unwrap_or(0);
        return resident_helper_main(&log, instance_id, event_name, parent_pid);
    }

    // Snapshot device status BEFORE the action so we can compare.
    match snapshot_devnode_status(instance_id) {
        Ok((s, p)) => log.append(&format!(
            "before {action}: status=0x{:08x} problem=0x{:08x}",
            s.0, p.0
        )),
        Err(e) => log.append(&format!("before {action}: status query failed: {e}")),
    }

    // `install` / `uninstall` go through the bundled devcon.exe rather
    // than pnputil. MttVDD's INF advertises `Root\MttVDD` (root-
    // enumerated virtual device); pnputil /add-driver /install only
    // updates drivers on EXISTING matching PnP nodes, but for a root
    // device there is no pre-existing node, so pnputil silently leaves
    // the driver in the store with nothing actually installed. devcon's
    // `install <inf> <hwid>` form does both: store-add + root-node
    // creation in one step. The 2nd arg is the full path to the .inf
    // when action is install/uninstall; we sit devcon next to the .inf
    // (Tauri MSI lays both at `[INSTALLDIR]vdd\`).
    if action == "install" || action == "uninstall" {
        let inf_path = std::path::Path::new(instance_id);
        let devcon = inf_path
            .parent()
            .map(|p| p.join("devcon.exe"))
            .unwrap_or_else(|| std::path::PathBuf::from("devcon.exe"));
        log.append(&format!(
            "{action} via devcon at {}: {}",
            devcon.display(),
            inf_path.display()
        ));
        // Hide devcon's console window. The elevated helper itself is
        // GUI-subsystem with no console attached, so any console child
        // it spawns gets a fresh popup unless we set CREATE_NO_WINDOW.
        #[cfg(windows)]
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let mut cmd = std::process::Command::new(&devcon);
        if action == "install" {
            cmd.args(["install", &inf_path.to_string_lossy(), "Root\\MttVDD"]);
        } else {
            // devcon remove takes the hardware ID, not the .inf path.
            cmd.args(["remove", "Root\\MttVDD"]);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let r = cmd.status();
        return match r {
            Ok(s) if s.success() => {
                log.append(&format!("devcon {action} OK"));
                ExitCode::SUCCESS
            }
            // devcon's exit code 1 means "reboot required" but the
            // device is registered + driver loaded — treat as success
            // for our purposes (IDDCx targets attach without reboot).
            Ok(s) if s.code() == Some(1) => {
                log.append(&format!(
                    "devcon {action} OK (reboot recommended; ignoring)"
                ));
                ExitCode::SUCCESS
            }
            Ok(s) => {
                log.append(&format!("devcon {action} exit={s:?}"));
                ExitCode::from(1)
            }
            Err(e) => {
                log.append(&format!("devcon {action} spawn failed: {e}"));
                ExitCode::from(1)
            }
        };
    }

    // devcon (vs CM_*_DevNode) writes registry-persistent CONFIGFLAG_
    // DISABLED on disable and clears it on enable, so the off-state
    // survives reboot — without that, root-enumerated devices come
    // back up on every boot and Penflow looks like it leaked VDD
    // (issue #22).
    let result = match action {
        "enable" => devcon_action("enable", instance_id).and_then(|()| {
            log.append("devcon enable OK");
            verify_devnode_started(instance_id).inspect(|_| {
                log.append("verify_devnode_started: device is healthy after enable");
            })
        }),
        "disable" => devcon_action("disable", instance_id).and_then(|()| {
            log.append("devcon disable returned OK; verifying disabled state");
            verify_devnode_disabled(instance_id).inspect(|()| {
                log.append("verify_devnode_disabled: device is disabled");
            })
        }),
        other => {
            log.append(&format!("unknown action: {other}"));
            return ExitCode::from(2);
        }
    };

    // Snapshot AFTER as well — comparing before/after is the cleanest
    // signal for "did anything actually change?".
    match snapshot_devnode_status(instance_id) {
        Ok((s, p)) => log.append(&format!(
            "after  {action}: status=0x{:08x} problem=0x{:08x}",
            s.0, p.0
        )),
        Err(e) => log.append(&format!("after  {action}: status query failed: {e}")),
    }

    match result {
        Ok(()) => {
            log.append(&format!("{action} OK; exiting 0"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            log.append(&format!("{action} failed: {e}"));
            ExitCode::from(1)
        }
    }
}

/// Resident-mode helper. Two named events bracket the lifetime:
///
/// - `<base>-done`: the helper signals this AFTER it finishes the
///   initial `devcon enable` + verify. The parent waits on it so
///   `enable()` is effectively synchronous (matches old `WaitForSingleObject`
///   semantics) and only returns once the device is actually started.
/// - `<base>-stop`: the parent signals this when it wants the helper to
///   tear down. The helper then runs `devcon disable` and exits.
///
/// Two events instead of one (or one with manual reset + race-prone
/// handshake) keeps the protocol stupid-obvious.
fn resident_helper_main(
    log: &HelperLog,
    instance_id: &str,
    event_base: &str,
    parent_pid: u32,
) -> ExitCode {
    let done_name = format!("{event_base}-done");
    let stop_name = format!("{event_base}-stop");
    log.append(&format!(
        "resident: enabling {instance_id}; events done={done_name} stop={stop_name} parent_pid={parent_pid}"
    ));

    let done_w = wide_z(&done_name);
    let done_evt =
        match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR::from_raw(done_w.as_ptr())) } {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                log.append("OpenEventW(done) failed; cannot proceed");
                return ExitCode::from(2);
            }
        };
    let stop_w = wide_z(&stop_name);
    let stop_evt =
        match unsafe { OpenEventW(SYNCHRONIZE, false, PCWSTR::from_raw(stop_w.as_ptr())) } {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                log.append("OpenEventW(stop) failed; cannot proceed");
                unsafe {
                    let _ = CloseHandle(done_evt);
                };
                return ExitCode::from(2);
            }
        };

    // Initial enable.
    if let Err(e) = devcon_action("enable", instance_id) {
        log.append(&format!("devcon enable failed: {e}"));
        unsafe {
            let _ = SetEvent(done_evt); // unblock parent so it can fail fast
            let _ = CloseHandle(done_evt);
            let _ = CloseHandle(stop_evt);
        };
        return ExitCode::from(1);
    }
    log.append("devcon enable OK");
    match verify_devnode_started(instance_id) {
        Ok(()) => log.append("verify_devnode_started: device is healthy after enable"),
        Err(e) => log.append(&format!("verify_devnode_started failed: {e}")),
    }

    // Tell the parent enable is done. The parent's `enable()` returns
    // here; subsequent `wait_for_virtual_monitor` polls DXGI until the
    // IddCx target attaches to the desktop.
    unsafe {
        let _ = SetEvent(done_evt);
    }

    // Wait for stop signal OR parent process exit. Parent-watch is the
    // crash-recovery path: if the parent gets killed (Task Manager,
    // BSOD, panic with `panic = "abort"`, or just exits before its
    // Drop chain runs to completion) we still get to the persistent
    // `devcon disable` below — otherwise the VDD stays attached
    // forever and re-enables on every reboot (issue #22).
    use windows::Win32::Foundation::{STILL_ACTIVE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, WaitForMultipleObjects, INFINITE,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };
    let parent_access = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
    let parent_handle = unsafe { OpenProcess(parent_access, false, parent_pid) };
    let trigger = match parent_handle {
        Ok(h) if !h.is_invalid() => {
            let mut last_exit_code: u32 = STILL_ACTIVE.0 as u32;
            let trigger = loop {
                let waitables = [stop_evt, h];
                let r = unsafe { WaitForMultipleObjects(&waitables, false, 1000) };
                if r == WAIT_OBJECT_0 {
                    break 0;
                }
                if r.0 == WAIT_OBJECT_0.0 + 1 {
                    break 1;
                }
                if r == WAIT_TIMEOUT {
                    match unsafe { GetExitCodeProcess(h, &mut last_exit_code) } {
                        Ok(()) if last_exit_code != STILL_ACTIVE.0 as u32 => {
                            log.append(&format!(
                                "parent exit code observed by watchdog: {last_exit_code}"
                            ));
                            break 1;
                        }
                        Ok(()) => continue,
                        Err(e) => {
                            log.append(&format!(
                                "GetExitCodeProcess({parent_pid}) failed: {e}; tearing down"
                            ));
                            break 2;
                        }
                    }
                }
                if r == WAIT_FAILED {
                    log.append("WaitForMultipleObjects failed; tearing down");
                    break 2;
                }
                break r.0.wrapping_sub(WAIT_OBJECT_0.0);
            };
            unsafe {
                let _ = CloseHandle(h);
            };
            trigger
        }
        _ => {
            log.append(&format!(
                "OpenProcess({parent_pid}) failed; waiting on stop event only"
            ));
            use windows::Win32::System::Threading::WaitForSingleObject;
            let _ = unsafe { WaitForSingleObject(stop_evt, INFINITE) };
            0
        }
    };
    match trigger {
        0 => log.append("stop event signaled; tearing down"),
        1 => log.append("parent died; tearing down (orphan recovery)"),
        other => log.append(&format!(
            "wait returned unexpected index {other}; tearing down anyway"
        )),
    }

    // Final disable. Persistent flag — survives reboot so the device
    // doesn't auto-re-enable on next boot enumeration.
    let exit_code = match devcon_action("disable", instance_id) {
        Ok(()) => match verify_devnode_disabled(instance_id) {
            Ok(()) => {
                log.append("devcon disable OK; verified disabled; exiting 0");
                ExitCode::SUCCESS
            }
            Err(e) => {
                log.append(&format!("devcon disable did not disable device: {e}"));
                ExitCode::from(1)
            }
        },
        Err(e) => {
            log.append(&format!("devcon disable failed: {e}"));
            ExitCode::from(1)
        }
    };

    unsafe {
        let _ = CloseHandle(done_evt);
        let _ = CloseHandle(stop_evt);
    };
    exit_code
}

/// Spawn the elevated resident helper that owns this VDD device's
/// enable/disable cycle for the lifetime of our process. Blocks until
/// the helper signals completion of the initial `devcon enable`,
/// matching the synchronous semantics of the legacy non-resident path.
fn spawn_resident_helper(instance_id: &str) -> Result<ResidentHelper, VddError> {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::System::Threading::WaitForSingleObject;

    let exe = env::current_exe()
        .map_err(|e| VddError::ShellExecute(format!("can't resolve current exe path: {e}")))?;

    // Unique-per-process event base name in the Local\\ namespace.
    // `<base>-done` and `<base>-stop` are the two actual events.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = unsafe { GetCurrentProcessId() };
    let ctr = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let event_base = format!("Local\\penflow-vdd-{pid}-{ctr}");

    let create = |suffix: &str| -> Result<HANDLE, VddError> {
        let name = format!("{event_base}-{suffix}");
        let name_w = wide_z(&name);
        unsafe {
            CreateEventW(
                Some(std::ptr::null::<SECURITY_ATTRIBUTES>()),
                true,  // manual reset
                false, // initial state: non-signaled
                PCWSTR::from_raw(name_w.as_ptr()),
            )
        }
        .map_err(|e| VddError::ShellExecute(format!("CreateEventW({suffix}): {e}")))
    };
    let done_evt = create("done")?;
    let stop_evt = match create("stop") {
        Ok(h) => h,
        Err(e) => {
            unsafe {
                let _ = CloseHandle(done_evt);
            };
            return Err(e);
        }
    };

    // Spawn helper elevated (UAC prompt). Pass our PID so the helper
    // can `OpenProcess(PROCESS_SYNCHRONIZE, ...)` and detect parent
    // death — without it, abnormal exit (kill, BSOD) leaks the VDD
    // enabled until next reboot.
    let exe_w = wide_z(exe.as_os_str());
    let parent_pid = pid;
    let params = format!("--vdd-helper resident \"{instance_id}\" \"{event_base}\" {parent_pid}");
    let params_w = wide_z(&params);
    let verb_w = wide_z("runas");
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: PCWSTR::from_raw(verb_w.as_ptr()),
        lpFile: PCWSTR::from_raw(exe_w.as_ptr()),
        lpParameters: PCWSTR::from_raw(params_w.as_ptr()),
        lpDirectory: PCWSTR::null(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut sei) } {
        unsafe {
            let _ = CloseHandle(done_evt);
            let _ = CloseHandle(stop_evt);
        };
        return Err(VddError::ShellExecute(format!("ShellExecuteExW: {e}")));
    }
    let process = sei.hProcess;
    if process.is_invalid() {
        unsafe {
            let _ = CloseHandle(done_evt);
            let _ = CloseHandle(stop_evt);
        };
        return Err(VddError::ShellExecute(
            "ShellExecuteExW returned no process handle (likely user clicked No on UAC)".into(),
        ));
    }

    // Block until helper finishes initial devcon enable (or process
    // dies, whichever comes first). We use 30 s — generous; devcon enable +
    // verify_devnode_started typically completes in < 2 s.
    let waitables = [done_evt, process];
    use windows::Win32::System::Threading::WaitForMultipleObjects;
    let r = unsafe { WaitForMultipleObjects(&waitables, false, 30_000) };
    if r == WAIT_OBJECT_0 {
        // done_evt fired: helper finished devcon enable.
        Ok(ResidentHelper {
            process,
            done_event: done_evt,
            stop_event: stop_evt,
        })
    } else {
        // Either helper died or we timed out. Either way bail out and
        // close handles. Caller falls back to per-call helper or errors.
        let _ = r; // suppress unused-value warning
                   // Did the helper exit? If yes, surface its exit code if non-zero.
        let mut code: u32 = 0;
        let _ = unsafe { WaitForSingleObject(process, 0) };
        let _ =
            unsafe { windows::Win32::System::Threading::GetExitCodeProcess(process, &mut code) };
        unsafe {
            let _ = CloseHandle(done_evt);
            let _ = CloseHandle(stop_evt);
            let _ = CloseHandle(process);
        };
        Err(VddError::ShellExecute(format!(
            "resident helper did not signal enable-done within 30s (exit code: {code})"
        )))
    }
}

fn snapshot_devnode_status(
    instance_id: &str,
) -> Result<(CM_DEVNODE_STATUS_FLAGS, CM_PROB), VddError> {
    let devinst = locate_devnode(instance_id)?;
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    let r = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if r != CR_SUCCESS {
        return Err(VddError::ConfigManager(r.0));
    }
    Ok((status, problem))
}

/// Tiny `%TEMP%\penflow-vdd-helper.log` appender. The helper has no
/// console; this file is the only diagnostic the operator can read.
struct HelperLog {
    path: std::path::PathBuf,
}

impl HelperLog {
    fn open() -> Self {
        let mut path = std::env::temp_dir();
        path.push("penflow-vdd-helper.log");
        Self { path }
    }

    fn append(&self, line: &str) {
        use std::io::Write;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "[{now}] {line}");
        }
    }
}

// =====================================================================
// Internals: native CM / SetupAPI calls.
// =====================================================================

struct DiscoveredDevice {
    instance_id: String,
    friendly_name: String,
    is_disabled: bool,
}

fn enumerate_display_devices() -> Result<Vec<DiscoveredDevice>, VddError> {
    // hwndparent = None (not Some(HWND::default()) — we don't have a UI
    // window). flags = DIGCF_PRESENT to include disabled-but-installed
    // devices as well as enabled ones (a "Disabled" device is still
    // considered "present" by SetupAPI).
    let info_set: HDEVINFO = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_DISPLAY),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        )?
    };
    if info_set.is_invalid() {
        return Err(VddError::Win32(windows::core::Error::from_thread()));
    }

    let mut out = Vec::new();
    let mut idx: u32 = 0;
    loop {
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let r = unsafe { SetupDiEnumDeviceInfo(info_set, idx, &mut data) };
        if r.is_err() {
            // ERROR_NO_MORE_ITEMS = 259; treat as end of iteration.
            break;
        }

        let instance_id = match get_instance_id(info_set, &data) {
            Ok(s) => s,
            Err(_) => {
                idx += 1;
                continue;
            }
        };
        // SPDRP_DEVICEDESC is the "Device description" string Device
        // Manager and PowerShell's `Get-PnpDevice -FriendlyName` actually
        // surface for most devices. SPDRP_FRIENDLYNAME is a different
        // (often empty) registry property. Try DEVICEDESC first, fall
        // back to FRIENDLYNAME, fall back to instance id.
        let friendly_name = get_string_property(info_set, &data, SPDRP_DEVICEDESC)
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                get_string_property(info_set, &data, SPDRP_FRIENDLYNAME)
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| instance_id.clone());
        let is_disabled = devnode_is_disabled(&instance_id).unwrap_or(false);

        out.push(DiscoveredDevice {
            instance_id,
            friendly_name,
            is_disabled,
        });
        idx += 1;
    }

    let _ = unsafe { SetupDiDestroyDeviceInfoList(info_set) };
    Ok(out)
}

fn get_instance_id(info_set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Result<String, VddError> {
    let mut buf = vec![0u16; 512];
    let mut required: u32 = 0;
    unsafe {
        SetupDiGetDeviceInstanceIdW(info_set, data, Some(&mut buf), Some(&mut required))?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

fn get_string_property(
    info_set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    prop: SETUP_DI_REGISTRY_PROPERTY,
) -> Result<String, VddError> {
    let mut buf = vec![0u8; 1024];
    let mut required: u32 = 0;
    let mut prop_type: u32 = 0;
    unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            info_set,
            data,
            prop,
            Some(&mut prop_type),
            Some(&mut buf),
            Some(&mut required),
        )?;
    }
    let len_bytes = (required as usize).min(buf.len());
    let u16_len = len_bytes / 2;
    let u16_slice: Vec<u16> = (0..u16_len)
        .map(|i| u16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]))
        .collect();
    let trim = u16_slice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(u16_slice.len());
    Ok(String::from_utf16_lossy(&u16_slice[..trim]))
}

fn matches_vdd_heuristic(name: &str) -> bool {
    let n = name.to_lowercase();
    // Specific product names of IDDCx-based virtual display drivers.
    // We deliberately do NOT match a bare "virtual display" or "vdd"
    // substring — too many emulators (MuMu, BlueStacks, NoxPlayer)
    // expose adapters whose names contain "Virtual Display" but which
    // are not driveable like a real IDDCx device.
    [
        "virtual display driver", // VirtualDrivers project (our bundle)
        "mttvdd",                 // shipped INF / hardware id
        "iddsample",              // Microsoft IDDCx sample (incl. iddsampledriver)
        "amyuni usb mobile",      // commercial Amyuni VDD
    ]
    .iter()
    .any(|needle| n.contains(needle))
}

fn locate_devnode(instance_id: &str) -> Result<u32, VddError> {
    let wide = wide_z(instance_id);
    let mut devinst: u32 = 0;
    let r: CONFIGRET = unsafe {
        CM_Locate_DevNodeW(
            &mut devinst as *mut u32,
            PCWSTR::from_raw(wide.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    match r {
        CR_SUCCESS => Ok(devinst),
        CR_NO_SUCH_DEVNODE => Err(VddError::DevNodeNotFound(instance_id.to_string())),
        other => Err(VddError::ConfigManager(other.0)),
    }
}

/// Run the bundled `devcon.exe <action> @<instance_id>` against the
/// current Penflow process's adjacent `vdd/` resource dir. `action` is
/// `"enable"` or `"disable"`. Caller must already be elevated — devcon
/// requires admin to mutate device state.
///
/// Returns Ok on devcon exit 0 (success) or 1 (success + "reboot
/// recommended"; IDDCx targets attach without reboot so this is fine
/// for our purposes). Anything else is an error.
fn devcon_action(action: &str, instance_id: &str) -> Result<(), VddError> {
    let devcon = bundled_devcon_path().ok_or_else(|| {
        VddError::ShellExecute(
            "devcon.exe not found at <exe-dir>/vdd/devcon.exe — broken install?".into(),
        )
    })?;
    // `@<instance_id>` targets the specific PnP node we detected, so
    // multiple matching VDDs (if a third-party VDD is also installed)
    // don't get caught in the crossfire.
    let target = format!("@{instance_id}");
    let mut cmd = std::process::Command::new(&devcon);
    cmd.args([action, &target]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let transcript = format!("{stdout}\n{stderr}");
            let no_match = transcript
                .to_ascii_lowercase()
                .contains("no matching devices found");
            if output.status.success() || (output.status.code() == Some(1) && !no_match) {
                Ok(())
            } else {
                Err(VddError::ShellExecute(format!(
                    "devcon {action} {target} exit={:?}; stdout={stdout:?}; stderr={stderr:?}",
                    output.status
                )))
            }
        }
        Err(e) => Err(VddError::ShellExecute(format!(
            "devcon {action} spawn failed: {e}"
        ))),
    }
}

fn bundled_devcon_path() -> Option<std::path::PathBuf> {
    let exe = env::current_exe().ok()?;
    let candidate = exe.parent()?.join("vdd").join("devcon.exe");
    candidate.is_file().then_some(candidate)
}

fn devnode_is_disabled(instance_id: &str) -> Result<bool, VddError> {
    let devinst = locate_devnode(instance_id)?;
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    let r = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if r != CR_SUCCESS {
        return Err(VddError::ConfigManager(r.0));
    }
    let has_problem = (status.0 & DN_HAS_PROBLEM.0) != 0;
    Ok(has_problem && problem.0 == CM_PROB_DISABLED.0)
}

/// After Disable, re-read CM_Get_DevNode_Status. `devcon` can return
/// exit code 1 for both "reboot recommended" and real misses, so the
/// status query is the source of truth.
fn verify_devnode_disabled(instance_id: &str) -> Result<(), VddError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if devnode_is_disabled(instance_id)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let (status, problem) = snapshot_devnode_status(instance_id)?;
            return Err(VddError::DisableHadNoEffect {
                status: status.0,
                problem: problem.0,
            });
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// After Enable, re-read CM_Get_DevNode_Status. If the driver came up
/// fine the node has no problem flag. If the user-mode driver host
/// crashed (HANDOFF §2.1 mttvdd.dll case), `DN_HAS_PROBLEM` is set with a
/// problem code other than `CM_PROB_DISABLED`.
fn verify_devnode_started(instance_id: &str) -> Result<(), VddError> {
    // Wait briefly — Windows reports the result asynchronously after
    // devcon enable returns.
    std::thread::sleep(Duration::from_millis(300));
    let devinst = locate_devnode(instance_id)?;
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    let r = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if r != CR_SUCCESS {
        return Err(VddError::ConfigManager(r.0));
    }
    let has_problem = (status.0 & DN_HAS_PROBLEM.0) != 0;
    if has_problem {
        if problem.0 == CM_PROB_DISABLED.0 {
            // Enable returned success but device is still disabled.
            // Means CM_Enable was a no-op (almost always: helper sub-
            // process didn't actually run elevated).
            return Err(VddError::EnableHadNoEffect);
        }
        return Err(VddError::DriverProblem {
            status: status.0,
            problem: problem.0,
            hint: problem_code_hint(problem.0),
        });
    }
    Ok(())
}

fn problem_code_hint(problem: u32) -> String {
    // Subset of CM_PROB_* codes that frequently surface for user-mode
    // display drivers. Numbers from cfg.h.
    match problem {
        18 => "CM_PROB_REINSTALL — Windows wants to reinstall the driver. Open Device Manager → uninstall the device → re-run Virtual Driver Control's Install.".into(),
        19 => "CM_PROB_REGISTRY — registry entries for the driver are corrupt.".into(),
        21 => "CM_PROB_WILL_BE_REMOVED — device is being removed.".into(),
        24 => "CM_PROB_DISABLED_SERVICE — the driver service is disabled.".into(),
        28 => "CM_PROB_NEEDS_FORCED_CONFIG — driver wants a manual configuration.".into(),
        31 => "CM_PROB_FAILED_POST_START — the user-mode driver host (mttvdd.dll) likely crashed during init. Replace `C:\\VirtualDisplayDriver\\vdd_settings.xml` with a known-good 25.7.x schema, then Disable→Enable the device. Do not use the 25.7.23 RELOAD_DRIVER pipe command as recovery; it has been observed to crash this driver build.".into(),
        43 => "CM_PROB_FAILED_INSTALL — the user-mode driver host failed to start. Same fix as code 31: replace vdd_settings.xml with a known-good 25.7.x schema, then Disable→Enable the device. Do not use RELOAD_DRIVER as recovery.".into(),
        _ => format!("(no hint for problem code {problem}; check Device Manager → device → Properties → General → Device status)"),
    }
}

fn is_process_elevated() -> bool {
    let mut token: HANDLE = HANDLE::default();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_ok() };
    if !opened {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size: u32 = 0;
    let got = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok()
    };
    let _ = unsafe { CloseHandle(token) };
    got && elevation.TokenIsElevated != 0
}

/// Install the bundled Virtual Display Driver. Spawns the elevated
/// helper with `--vdd-helper install <inf-path>` which calls
/// `pnputil /add-driver <inf> /install`. One UAC prompt; returns when
/// the install is complete (or errors out).
pub fn install_driver(inf_path: &std::path::Path) -> Result<(), VddError> {
    let s = inf_path.to_string_lossy();
    run_helper_elevated("install", &s)
}

/// Uninstall the VDD via pnputil. `inf_path` is the same .inf used
/// during install (or its OEM-renamed twin in `%WINDIR%\INF`).
pub fn uninstall_driver(inf_path: &std::path::Path) -> Result<(), VddError> {
    let s = inf_path.to_string_lossy();
    run_helper_elevated("uninstall", &s)
}

/// Re-launch the current executable elevated, with `--vdd-helper <action>
/// <instance_id>` arguments. Waits for the elevated child to exit and
/// translates exit code into `Result`.
fn run_helper_elevated(action: &str, instance_id: &str) -> Result<(), VddError> {
    let exe = env::current_exe()
        .map_err(|e| VddError::ShellExecute(format!("can't resolve current exe path: {e}")))?;

    // Helper invocation: `<exe> --vdd-helper <action> <instance_id>`.
    // Quote the instance id because it can contain backslashes (spaces
    // are unlikely but quoting is harmless).
    let exe_w = wide_z(exe.as_os_str());
    let params = format!("--vdd-helper {action} \"{instance_id}\"");
    let params_w = wide_z(&params);
    let verb_w = wide_z("runas");

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: PCWSTR::from_raw(verb_w.as_ptr()),
        lpFile: PCWSTR::from_raw(exe_w.as_ptr()),
        lpParameters: PCWSTR::from_raw(params_w.as_ptr()),
        lpDirectory: PCWSTR::null(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut sei) }
        .map_err(|e| VddError::ShellExecute(format!("ShellExecuteExW: {e}")))?;

    // sei.hProcess is set because we passed SEE_MASK_NOCLOSEPROCESS.
    let process = sei.hProcess;
    if process.is_invalid() {
        return Err(VddError::ShellExecute(
            "ShellExecuteExW returned no process handle (likely user clicked No on UAC)".into(),
        ));
    }
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    let _ = unsafe { WaitForSingleObject(process, INFINITE) };
    let mut code: u32 = 0;
    let _ = unsafe { GetExitCodeProcess(process, &mut code) };
    let _ = unsafe { CloseHandle(process) };
    if code == 0 {
        Ok(())
    } else {
        Err(VddError::HelperExitedNonZero {
            code: Some(code as i32),
        })
    }
}

fn wide_z(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_recognises_canonical_name() {
        assert!(matches_vdd_heuristic("Virtual Display Driver"));
        assert!(matches_vdd_heuristic("MttVDD"));
        assert!(matches_vdd_heuristic("IddSampleDriver"));
    }

    #[test]
    fn heuristic_rejects_real_gpus_and_emulators() {
        assert!(!matches_vdd_heuristic("NVIDIA GeForce RTX 5070"));
        assert!(!matches_vdd_heuristic("AMD Radeon Graphics"));
        // MuMu / BlueStacks / NoxPlayer expose adapters whose names
        // contain "Virtual Display" but don't behave like real IDDCx
        // devices. Excluding them prevents the GUI's first-run banner
        // from being suppressed by an unrelated emulator.
        assert!(!matches_vdd_heuristic("MuMu Virtual Display Adapter"));
        assert!(!matches_vdd_heuristic("BlueStacks Virtual Display"));
    }

    #[test]
    fn elevation_check_runs_without_crashing() {
        // Don't assert the result — depends on test environment.
        let _ = is_process_elevated();
    }

    #[test]
    fn monitor_key_uses_luid_and_device_name() {
        let m = MonitorInfo {
            adapter_index: 0,
            adapter_luid: 42,
            adapter_name: "adapter".into(),
            adapter_vendor_id: 0,
            adapter_device_id: 0,
            adapter_is_software: false,
            output_index_within_adapter: 0,
            device_name: r"\\.\DISPLAY27".into(),
            width: 2880,
            height: 1800,
            desktop_coords: (0, 0, 2880, 1800),
            rotation: 1,
            attached_to_desktop: true,
            looks_virtual: false,
        };
        assert_eq!(monitor_key(&m), r"42:\\.\DISPLAY27");
    }
}
