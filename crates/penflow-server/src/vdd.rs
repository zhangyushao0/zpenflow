//! On-demand Virtual Display Driver lifecycle.
//!
//! ## Design
//!
//! - **The user keeps the VDD device installed but disabled** in Device
//!   Manager. The server enables it after the Android handshake and
//!   disables it on disconnect, so the idle PC desktop has no extra
//!   monitor.
//! - **No PowerShell.** All device-control calls go through the native
//!   Win32 Configuration Manager API (`CM_Enable_DevNode` /
//!   `CM_Disable_DevNode` / `CM_Locate_DevNodeW` / `CM_Get_DevNode_Status`)
//!   and SetupAPI (`SetupDiGetClassDevs` for enumeration). No external
//!   process spawn for the actual enable/disable.
//! - **Just-in-time UAC.** `CM_Enable_DevNode` requires Administrator. The
//!   server runs as a regular user; when it actually needs to flip the
//!   device, it invokes itself via `ShellExecuteW` with the `runas` verb
//!   in `--vdd-helper enable <instance>` / `--vdd-helper disable
//!   <instance>` mode. Windows shows the UAC prompt; the user clicks Yes
//!   once per enable and once per disable. The helper does the CM call
//!   and exits with status 0 / non-zero, no IPC complexity.
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
//! - On enable, after we call `CM_Enable_DevNode`, we re-read
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
    CM_Disable_DevNode, CM_Enable_DevNode, CM_Get_DevNode_Status, CM_Locate_DevNodeW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, CM_DEVNODE_STATUS_FLAGS,
    CM_LOCATE_DEVNODE_NORMAL, CM_PROB, CM_PROB_DISABLED, CONFIGRET, CR_ACCESS_DENIED,
    CR_NO_SUCH_DEVNODE, CR_SUCCESS, DIGCF_PRESENT, DN_HAS_PROBLEM, GUID_DEVCLASS_DISPLAY,
    HDEVINFO, SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, SPDRP_DEVICEDESC,
    SPDRP_FRIENDLYNAME,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use penflow_core::d3d11::create_dxgi_factory;
use penflow_core::monitors::{self, MonitorInfo};

/// Errors from the VDD lifecycle path.
#[derive(Debug, Error)]
pub enum VddError {
    /// A Windows API call failed unexpectedly.
    #[error("Windows API: {0}")]
    Win32(#[from] windows::core::Error),

    /// `CM_Locate_DevNodeW` couldn't find the device. Either the
    /// instance id is wrong or the device was uninstalled.
    #[error("device '{0}' not found by Configuration Manager")]
    DevNodeNotFound(String),

    /// `CM_Enable_DevNode` / `CM_Disable_DevNode` returned `CR_ACCESS_DENIED`
    /// (we're not running elevated and this code path bypassed the helper —
    /// shouldn't happen in normal operation).
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
    /// `CM_PROB_DISABLED` state. Typically means `CM_Enable_DevNode`
    /// silently no-op'd because the calling process wasn't elevated, or
    /// the helper sub-process didn't actually run elevated.
    #[error(
        "Enable reported success but the device is still disabled. \
         Likely cause: the elevated helper didn't actually run with \
         Administrator privileges. Check %TEMP%\\penflow-vdd-helper.log \
         for the helper trace."
    )]
    EnableHadNoEffect,

    /// `ShellExecuteExW` itself failed (code path that runs in the
    /// non-elevated parent). Usually means the user clicked No on the
    /// UAC prompt — Windows reports `ERROR_CANCELLED` (1223).
    #[error("could not launch elevated helper: {0}")]
    ShellExecute(String),

    /// We enabled the VDD but DXGI didn't enumerate a virtual monitor
    /// within the wait window. The error includes every monitor name we
    /// saw during the wait so the operator can spot the case where the
    /// monitor came up under an unexpected name.
    #[error(
        "VDD enabled but DXGI didn't enumerate a virtual monitor within {timeout:?}.\n\
         Monitors seen during the wait: {monitors_seen:?}"
    )]
    EnumerationTimeout {
        /// How long we waited before giving up.
        timeout: Duration,
        /// All distinct adapter+output labels enumerated during the wait.
        monitors_seen: Vec<String>,
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
            eprintln!("[vdd-trace] enumerated {} Display-class devices:", candidates.len());
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
                    .contains("virtual display driver")
                    as u8;
                (status_ok, canonical_no)
            });
        Ok(chosen.map(|c| Self {
            instance_id: c.instance_id,
            friendly_name: c.friendly_name,
            enabled: false,
        }))
    }

    /// Enable the device. If the current process isn't elevated, spawns
    /// an elevated helper (`<exe> --vdd-helper enable <instance>`) via
    /// `ShellExecuteW("runas", ...)` — Windows shows the UAC prompt and
    /// the user clicks Yes once. The helper does the actual CM call and
    /// exits.
    pub fn enable(&mut self) -> Result<(), VddError> {
        if is_process_elevated() {
            cm_enable(&self.instance_id)?;
            verify_devnode_started(&self.instance_id)?;
        } else {
            run_helper_elevated("enable", &self.instance_id)?;
        }
        self.enabled = true;
        Ok(())
    }

    /// Disable the device. Same elevation handling as `enable`.
    pub fn disable(&mut self) -> Result<(), VddError> {
        if is_process_elevated() {
            cm_disable(&self.instance_id)?;
        } else {
            run_helper_elevated("disable", &self.instance_id)?;
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
/// attached output appears. On timeout, report which monitors WERE
/// visible during the wait (so the operator can spot a heuristic miss).
pub async fn wait_for_virtual_monitor(timeout: Duration) -> Result<MonitorInfo, VddError> {
    let start = Instant::now();
    let mut all_seen: Vec<String> = Vec::new();
    let mut last_dxgi_err: Option<String> = None;
    while Instant::now().duration_since(start) < timeout {
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
                        m.looks_virtual && m.attached_to_desktop && !m.adapter_is_software
                    }) {
                        return Ok(m);
                    }
                }
                Err(e) => last_dxgi_err = Some(format!("enumerate: {e:?}")),
            },
            Err(e) => last_dxgi_err = Some(format!("create_dxgi_factory: {e:?}")),
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if let Some(e) = last_dxgi_err {
        Err(VddError::Dxgi(e))
    } else {
        Err(VddError::EnumerationTimeout {
            timeout,
            monitors_seen: all_seen,
        })
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
        log.append("usage error: expected `--vdd-helper <enable|disable> <instance_id>`");
        return ExitCode::from(2);
    }
    let action = args[1].as_str();
    let instance_id = args[2].as_str();

    // Snapshot device status BEFORE the action so we can compare.
    match snapshot_devnode_status(instance_id) {
        Ok((s, p)) => log.append(&format!(
            "before {action}: status=0x{:08x} problem=0x{:08x}",
            s.0, p.0
        )),
        Err(e) => log.append(&format!("before {action}: status query failed: {e}")),
    }

    let result = match action {
        "enable" => cm_enable(instance_id).and_then(|_| {
            log.append("CM_Enable_DevNode returned CR_SUCCESS");
            verify_devnode_started(instance_id).inspect(|_| {
                log.append("verify_devnode_started: device is healthy after enable");
            })
        }),
        "disable" => cm_disable(instance_id).inspect(|_| {
            log.append("CM_Disable_DevNode returned CR_SUCCESS");
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
    [
        "virtual display driver",
        "virtual display",
        "iddcx",
        "iddsample",
        "iddsampledriver",
        "vdd",
        "mttvdd",
        "amyuni",
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

fn cm_enable(instance_id: &str) -> Result<(), VddError> {
    let devinst = locate_devnode(instance_id)?;
    let r = unsafe { CM_Enable_DevNode(devinst, 0) };
    match r {
        CR_SUCCESS => Ok(()),
        CR_ACCESS_DENIED => Err(VddError::AccessDenied),
        other => Err(VddError::ConfigManager(other.0)),
    }
}

fn cm_disable(instance_id: &str) -> Result<(), VddError> {
    let devinst = locate_devnode(instance_id)?;
    let r = unsafe { CM_Disable_DevNode(devinst, 0) };
    match r {
        CR_SUCCESS => Ok(()),
        CR_ACCESS_DENIED => Err(VddError::AccessDenied),
        other => Err(VddError::ConfigManager(other.0)),
    }
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

/// After Enable, re-read CM_Get_DevNode_Status. If the driver came up
/// fine the node has no problem flag. If the user-mode driver host
/// crashed (HANDOFF §2.1 mttvdd.dll case), `DN_HAS_PROBLEM` is set with a
/// problem code other than `CM_PROB_DISABLED`.
fn verify_devnode_started(instance_id: &str) -> Result<(), VddError> {
    // Wait briefly — Windows reports the result asynchronously after
    // CM_Enable_DevNode returns.
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
        31 => "CM_PROB_FAILED_POST_START — the user-mode driver host (mttvdd.dll) likely crashed during init. HANDOFF §2.1 documents this for VirtualDrivers/Virtual-Display-Driver: replace `C:\\VirtualDisplayDriver\\vdd_settings.xml` with the minimal `tools/vdd/vdd_settings.xml` we ship and Disable→Enable the device once.".into(),
        43 => "CM_PROB_FAILED_INSTALL — the user-mode driver host failed to start. Same fix as code 31: replace vdd_settings.xml with the minimal one in tools/vdd/.".into(),
        _ => format!("(no hint for problem code {problem}; check Device Manager → device → Properties → General → Device status)"),
    }
}

fn is_process_elevated() -> bool {
    let mut token: HANDLE = HANDLE::default();
    let opened = unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_ok()
    };
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

/// Re-launch the current executable elevated, with `--vdd-helper <action>
/// <instance_id>` arguments. Waits for the elevated child to exit and
/// translates exit code into `Result`.
fn run_helper_elevated(action: &str, instance_id: &str) -> Result<(), VddError> {
    let exe = env::current_exe().map_err(|e| {
        VddError::ShellExecute(format!("can't resolve current exe path: {e}"))
    })?;

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
        assert!(matches_vdd_heuristic("MTT VDD"));
        assert!(matches_vdd_heuristic("IddSampleDriver"));
    }

    #[test]
    fn heuristic_rejects_real_gpus() {
        assert!(!matches_vdd_heuristic("NVIDIA GeForce RTX 5070"));
        assert!(!matches_vdd_heuristic("AMD Radeon Graphics"));
        // MuMu — emulator's display adapter, contains "virtual"; we
        // accept it here, but `detect()` deprioritises it via
        // status (it's enabled, ours is disabled) and via canonical
        // name (it doesn't contain "virtual display driver" exactly).
        assert!(matches_vdd_heuristic("MuMu Virtual Display Adapter"));
    }

    #[test]
    fn elevation_check_runs_without_crashing() {
        // Don't assert the result — depends on test environment.
        let _ = is_process_elevated();
    }
}

