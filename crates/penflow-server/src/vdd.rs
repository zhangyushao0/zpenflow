//! On-demand Virtual Display Driver lifecycle.
//!
//! Design: the user installs the Virtual Display Driver once (manually,
//! signed binary; see `tools/vdd/README.md`) and leaves it **disabled** in
//! Device Manager. The Penflow session enables it via `Enable-PnpDevice`
//! after the Android client completes the handshake, then disables it via
//! `Disable-PnpDevice` when the session ends. Idle desktop has no virtual
//! monitor → cursor can't wander into dead pixel space and Windows's
//! display-arrangement UI stays uncluttered.
//!
//! Requires Administrator privileges (the Enable/Disable-PnpDevice cmdlets
//! are gated). If the server isn't elevated, `enable()` surfaces a clear
//! error and the session aborts — no silent fall-back to capturing the
//! physical monitor (which would hit the 4K decoder mismatch we already
//! diagnosed on Qualcomm c2.qti).
//!
//! `Drop` disables the device so an aborted session (panic, Ctrl-C, kill)
//! still cleans up the virtual monitor.

use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

use penflow_core::d3d11::create_dxgi_factory;
use penflow_core::monitors::{self, MonitorInfo};

/// Errors from the VDD lifecycle path.
#[derive(Debug, Error)]
pub enum VddError {
    /// Couldn't even spawn `powershell.exe`. Usually means PowerShell isn't
    /// on PATH (very unusual on Windows).
    #[error("failed to spawn PowerShell: {0}")]
    Spawn(#[from] io::Error),

    /// PowerShell ran but reported a non-zero exit. The most common case
    /// is `Enable-PnpDevice` without admin: stderr will contain
    /// `Access is denied` or `要求提升`.
    #[error("PowerShell command failed: {0}")]
    PowerShell(String),

    /// We enabled the VDD device, but Windows didn't enumerate the new
    /// monitor through DXGI within the wait window. Either the driver is
    /// faulty, or the wait time was too short for this rig.
    #[error("VDD device enabled but DXGI didn't enumerate a virtual monitor within {0:?}")]
    EnumerationTimeout(Duration),

    /// Something went wrong walking the DXGI factory while waiting for
    /// the virtual monitor.
    #[error("DXGI enumeration error: {0}")]
    Dxgi(String),
}

/// Handle to one PnP-managed Virtual Display Driver device.
///
/// `enable()` / `disable()` are both idempotent; Drop disables for safety.
#[derive(Debug)]
pub struct VddController {
    instance_id: String,
    friendly_name: String,
    /// True iff we currently believe the device is enabled. Drop uses this
    /// to decide whether to fire Disable-PnpDevice.
    enabled: bool,
}

impl VddController {
    /// The Windows PnP instance id (`ROOT\DISPLAY\0000` or similar). Used
    /// for diagnostics; the rest of the API drives by it implicitly.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Human-readable name of the device (`Virtual Display Driver`,
    /// `MTT VDD`, etc.).
    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    /// Probe Device Manager for an installed Virtual Display Driver.
    /// Matches by friendly-name keywords (`virtual`, `vdd`, `iddsample`,
    /// `MTT`). Returns `Ok(None)` if PowerShell ran fine but no VDD was
    /// installed (the operator should follow `tools/vdd/README.md` to
    /// install one).
    ///
    /// Selection priority when multiple VDD-style devices are present
    /// (e.g. an emulator's virtual adapter alongside the user's installed
    /// VirtualDrivers/Virtual-Display-Driver release):
    ///   1. Currently disabled (Status != "OK") — the operator's intent
    ///      is on-demand enable; an already-enabled device is something
    ///      else's.
    ///   2. FriendlyName matching `Virtual Display Driver` exactly — the
    ///      canonical name from the VirtualDrivers project.
    ///   3. Whatever PowerShell returns first.
    pub fn detect() -> Result<Option<Self>, VddError> {
        // Sort by composite key (status_ok asc, name_not_canonical asc).
        // PowerShell `Sort-Object` with hashtable expressions evaluates
        // the boolean to 1/0 — disabled (Status != OK) sorts first, then
        // canonical-name first within each status bucket.
        let cmd = "Get-PnpDevice -Class Display -ErrorAction SilentlyContinue | \
                   Where-Object { $_.FriendlyName -match 'virtual|vdd|iddsample|MTT' } | \
                   Sort-Object \
                     @{Expression={if ($_.Status -eq 'OK') {1} else {0}}}, \
                     @{Expression={if ($_.FriendlyName -match 'Virtual Display Driver') {0} else {1}}} | \
                   Select-Object -First 1 InstanceId, FriendlyName | \
                   ConvertTo-Json -Compress";
        let stdout = run_ps(cmd)?;
        if stdout.is_empty() || stdout == "null" {
            return Ok(None);
        }
        let (instance_id, friendly_name) = parse_pnp_json(&stdout).ok_or_else(|| {
            VddError::PowerShell(format!("unexpected Get-PnpDevice output: {stdout}"))
        })?;
        Ok(Some(Self {
            instance_id,
            friendly_name,
            enabled: false,
        }))
    }

    /// Build a controller from an explicit `InstanceId` (skip auto-detect).
    /// Useful when the operator has multiple VDD-style devices and wants
    /// to be unambiguous via `--vdd-instance ROOT\DISPLAY\0001`.
    pub fn for_instance(instance_id: impl Into<String>) -> Self {
        let id = instance_id.into();
        Self {
            instance_id: id.clone(),
            friendly_name: id,
            enabled: false,
        }
    }

    /// Run `Enable-PnpDevice -Confirm:$false`. Idempotent — already-enabled
    /// is reported as success by Windows.
    pub fn enable(&mut self) -> Result<(), VddError> {
        let cmd = format!(
            "Enable-PnpDevice -InstanceId '{}' -Confirm:$false",
            self.instance_id.replace('\'', "''")
        );
        run_ps(&cmd)?;
        self.enabled = true;
        Ok(())
    }

    /// Run `Disable-PnpDevice -Confirm:$false`. Idempotent.
    pub fn disable(&mut self) -> Result<(), VddError> {
        let cmd = format!(
            "Disable-PnpDevice -InstanceId '{}' -Confirm:$false",
            self.instance_id.replace('\'', "''")
        );
        run_ps(&cmd)?;
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
/// attached output appears. Returns its `MonitorInfo` (the engine builder
/// uses that to pick the adapter and device).
pub async fn wait_for_virtual_monitor(timeout: Duration) -> Result<MonitorInfo, VddError> {
    let start = Instant::now();
    let mut last_err: Option<String> = None;
    while Instant::now().duration_since(start) < timeout {
        // Re-create the factory each tick — DXGI caches enumeration on the
        // factory instance, so a held one might not reflect the new device.
        match create_dxgi_factory() {
            Ok(factory) => match monitors::enumerate(&factory) {
                Ok(mons) => {
                    if let Some(m) = mons.into_iter().find(|m| {
                        m.looks_virtual && m.attached_to_desktop && !m.adapter_is_software
                    }) {
                        return Ok(m);
                    }
                }
                Err(e) => last_err = Some(format!("enumerate: {e:?}")),
            },
            Err(e) => last_err = Some(format!("create_dxgi_factory: {e:?}")),
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if let Some(e) = last_err {
        Err(VddError::Dxgi(e))
    } else {
        Err(VddError::EnumerationTimeout(timeout))
    }
}

fn run_ps(command: &str) -> Result<String, VddError> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let mut hint = String::new();
        let lower = stderr.to_lowercase();
        if lower.contains("access is denied")
            || lower.contains("要求提升")
            || lower.contains("requires elevation")
            || lower.contains("拒绝访问")
        {
            hint.push_str(
                "\nHint: Enable-PnpDevice / Disable-PnpDevice require Administrator. \
                 Re-launch the server from an elevated PowerShell.",
            );
        }
        return Err(VddError::PowerShell(format!(
            "exit {:?}; stderr: {}; stdout: {}{}",
            out.status.code(),
            stderr,
            stdout,
            hint
        )));
    }
    Ok(stdout)
}

/// Parse the one-line JSON `{"InstanceId":"...","FriendlyName":"..."}` that
/// `Select-Object | ConvertTo-Json -Compress` produces. We deliberately
/// don't pull `serde_json` for two fields.
fn parse_pnp_json(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    let id = extract_json_field(s, "InstanceId")?;
    let name = extract_json_field(s, "FriendlyName")?;
    Some((id, name))
}

fn extract_json_field(s: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = s.find(&needle)? + needle.len();
    let rest = &s[start..];
    // Find the closing quote, skipping `\"` escapes.
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => {
                if let Some(esc) = chars.next() {
                    out.push(esc);
                }
            }
            other => out.push(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_powershell_json() {
        let s = r#"{"InstanceId":"ROOT\\DISPLAY\\0000","FriendlyName":"Virtual Display Driver"}"#;
        let (id, name) = parse_pnp_json(s).expect("parse");
        assert_eq!(id, r"ROOT\DISPLAY\0000");
        assert_eq!(name, "Virtual Display Driver");
    }

    #[test]
    fn parse_handles_field_order_and_spacing() {
        let s = r#"{"FriendlyName":"MTT VDD","InstanceId":"ROOT\\MTTVDD\\0001"}"#;
        let (id, name) = parse_pnp_json(s).expect("parse");
        assert_eq!(id, r"ROOT\MTTVDD\0001");
        assert_eq!(name, "MTT VDD");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_pnp_json("").is_none());
        assert!(parse_pnp_json("null").is_none());
        assert!(parse_pnp_json("not json").is_none());
    }
}
