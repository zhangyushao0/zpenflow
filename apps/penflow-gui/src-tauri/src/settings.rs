//! Persistent user settings for the Penflow GUI.
//!
//! Stored at `%APPDATA%/Penflow/settings.json`. Read once at app startup,
//! mutated via the `save_settings` Tauri command, and re-read by the
//! `Service` whenever it accepts a new connection so changes take effect
//! on the next session without requiring a restart.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Top-level user-facing configuration. Defaults match the values that
/// the predecessor `run_session` example used as hardcoded constants.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Virtual display resolution to publish through the bundled VDD.
    /// The engine captures whatever mode Windows enumerates after the
    /// VDD is enabled, so changing this affects the next tablet session.
    #[serde(default)]
    pub vdd_resolution: DisplayResolution,
    /// Encoder bitrate in bits per second.
    #[serde(default = "default_bitrate")]
    pub bitrate_bps: u32,
    /// Capture / encode frame rate. 120 matches the MovinkPad's panel.
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Codec to ask NVENC for.
    #[serde(default = "default_codec")]
    pub codec: SettingsCodec,
    /// Pen-button bindings (slot 0 = barrel button 1, slot 1 = barrel
    /// button 2, slot 2 = tertiary). Mirrors `Binding` in design.md §6.6.
    #[serde(default)]
    pub bindings: PenBindings,
    /// Run on Windows logon. Implemented by writing the executable path
    /// to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
    #[serde(default)]
    pub autostart: bool,
    /// Re-launch elevated when the GUI starts unelevated. The pen
    /// `InputInjector` works without admin on the dev rig, but some
    /// applications (e.g. UAC-elevated Krita) only accept input from
    /// elevated injectors.
    #[serde(default)]
    pub run_as_admin: bool,
    /// Show the latency HUD overlay on the Android client. Default on.
    /// Toggling this requires the Android client to reconnect — the
    /// flag is sent during the session's `MSG_CLIENT_CONFIG` handshake.
    #[serde(default = "default_hud_enabled")]
    pub hud_enabled: bool,
    /// `Extend` (default) uses the VDD as a separate desktop. `Duplicate`
    /// skips the VDD and captures the primary monitor directly.
    #[serde(default)]
    pub topology: TopologyMode,
    /// Pen-tablet screen-off mode (Duplicate topology only). Panel dark,
    /// capture+encode skipped; pen + touch still flow.
    #[serde(default)]
    pub screen_off: bool,
    /// Drop incoming touch/hand-gesture events server-side (Duplicate
    /// topology only). Lets users palm-rest on the tablet without the OS
    /// interpreting fingers as taps. Pen samples are unaffected.
    #[serde(default)]
    pub disable_touch: bool,
}

fn default_hud_enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            vdd_resolution: DisplayResolution::default(),
            bitrate_bps: default_bitrate(),
            fps: default_fps(),
            codec: default_codec(),
            bindings: PenBindings::default(),
            autostart: false,
            run_as_admin: false,
            hud_enabled: default_hud_enabled(),
            topology: TopologyMode::default(),
            screen_off: false,
            disable_touch: false,
        }
    }
}

/// VDD-on (Extend) vs. VDD-off (Duplicate). See `Settings::topology`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TopologyMode {
    #[default]
    Extend,
    Duplicate,
}

fn default_bitrate() -> u32 {
    50_000_000
}
fn default_fps() -> u32 {
    120
}
fn default_codec() -> SettingsCodec {
    SettingsCodec::Hevc
}

pub const DEFAULT_VDD_WIDTH: u32 = 2880;
pub const DEFAULT_VDD_HEIGHT: u32 = 1800;
pub const MIN_VDD_WIDTH: u32 = 640;
pub const MIN_VDD_HEIGHT: u32 = 480;
pub const MAX_VDD_WIDTH: u32 = 7680;
pub const MAX_VDD_HEIGHT: u32 = 4320;

/// Width/height for the on-demand virtual display. Keep dimensions even:
/// D3D11 texture conversion and hardware encoders expect 4:2:0-friendly
/// frame sizes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayResolution {
    #[serde(default = "default_vdd_width")]
    pub width: u32,
    #[serde(default = "default_vdd_height")]
    pub height: u32,
}

impl Default for DisplayResolution {
    fn default() -> Self {
        Self {
            width: DEFAULT_VDD_WIDTH,
            height: DEFAULT_VDD_HEIGHT,
        }
    }
}

fn default_vdd_width() -> u32 {
    DEFAULT_VDD_WIDTH
}

fn default_vdd_height() -> u32 {
    DEFAULT_VDD_HEIGHT
}

impl DisplayResolution {
    pub fn validate(self) -> Result<(), String> {
        if !(MIN_VDD_WIDTH..=MAX_VDD_WIDTH).contains(&self.width) {
            return Err(format!(
                "VDD width must be between {MIN_VDD_WIDTH} and {MAX_VDD_WIDTH}"
            ));
        }
        if !(MIN_VDD_HEIGHT..=MAX_VDD_HEIGHT).contains(&self.height) {
            return Err(format!(
                "VDD height must be between {MIN_VDD_HEIGHT} and {MAX_VDD_HEIGHT}"
            ));
        }
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err("VDD width and height must be even numbers".into());
        }
        Ok(())
    }
}

pub fn validate(s: &Settings) -> Result<(), String> {
    s.vdd_resolution.validate()
}

/// Local serializable shadow of `penflow_core::encoder::Codec`. Wrapper
/// avoids forcing a serde dependency on the engine crate.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettingsCodec {
    H264,
    Hevc,
}

impl From<SettingsCodec> for penflow_core::encoder::Codec {
    fn from(c: SettingsCodec) -> Self {
        match c {
            SettingsCodec::H264 => penflow_core::encoder::Codec::H264,
            SettingsCodec::Hevc => penflow_core::encoder::Codec::Hevc,
        }
    }
}

/// Per-button bindings for the pen's barrel + tertiary switches.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PenBindings {
    pub button_0: Binding,
    pub button_1: Binding,
    pub button_2: Binding,
}

impl Default for PenBindings {
    fn default() -> Self {
        // Krita-friendly defaults: hold Ctrl on barrel-1 (color picker),
        // hold Shift on barrel-2 (line straighten), tap E on tertiary
        // (eraser toggle).
        Self {
            button_0: Binding::KeyHold { key: "Ctrl".into() },
            button_1: Binding::KeyHold {
                key: "Shift".into(),
            },
            button_2: Binding::KeyTap { key: "E".into() },
        }
    }
}

/// One pen-button → input-action mapping. Mirrors the OTD-inspired
/// `Binding` enum in `docs/design.md` §6.6. Concrete VK lookup happens
/// at injection time (see `penflow-core::inject::binding`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Binding {
    /// Do nothing on press / release.
    None,
    /// Tap the key once (down + up) on press.
    KeyTap { key: String },
    /// Hold the key for the lifetime of the press; release on release.
    KeyHold { key: String },
    /// Send the keys in order with the last held until release.
    KeyChord { keys: Vec<String> },
    /// Synthesize a mouse button press while pressed.
    MouseButton { button: MouseButton },
    /// Toggle the pen's eraser tool flag for the lifetime of the press.
    EraserToggle,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Returns the path to `settings.json` under `%APPDATA%/Penflow/`. The
/// directory is created on demand by `save`.
pub fn settings_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Penflow").join("settings.json")
}

/// Load settings from disk. Missing file or parse failure → defaults
/// (logged, never fatal — we don't want a corrupt settings file to keep
/// the user from launching the GUI).
pub fn load() -> Settings {
    let path = settings_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("[settings] no file at {}; using defaults", path.display());
            return Settings::default();
        }
        Err(e) => {
            eprintln!(
                "[settings] read {} failed: {e}; using defaults",
                path.display()
            );
            return Settings::default();
        }
    };
    match serde_json::from_slice::<Settings>(&bytes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[settings] parse {} failed: {e}; using defaults",
                path.display()
            );
            Settings::default()
        }
    }
}

/// Atomically replace the settings file. Writes to a sibling tempfile
/// then renames into place — never leaves a half-written JSON if the
/// process is killed mid-write.
pub fn save(s: &Settings) -> std::io::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        let json = serde_json::to_vec_pretty(s)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn installed_vdd_settings_path() -> PathBuf {
    PathBuf::from(r"C:\VirtualDisplayDriver\vdd_settings.xml")
}

pub fn write_installed_vdd_settings(s: &Settings) -> std::io::Result<()> {
    write_vdd_settings_file(&installed_vdd_settings_path(), s, true)
}

pub fn write_vdd_settings_file(
    path: &std::path::Path,
    s: &Settings,
    create_parent: bool,
) -> std::io::Result<()> {
    validate(s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    if create_parent {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let xml = render_vdd_settings_xml(s.vdd_resolution);
    std::fs::write(path, xml)
}

fn render_vdd_settings_xml(resolution: DisplayResolution) -> String {
    format!(
        r#"<?xml version='1.0' encoding='utf-8'?>
<!--
  Penflow VDD configuration.

  This installed copy is generated from the GUI's virtual-display
  resolution setting. The checked-in template defaults to 2880x1800 for
  the MovinkPad Pro 14, but users can choose another even width/height
  and the next VDD enable cycle will publish that mode.
-->
<vdd_settings>
    <monitors>
        <count>1</count>
    </monitors>
    <gpu>
        <friendlyname>default</friendlyname>
    </gpu>
    <global>
        <g_refresh_rate>60</g_refresh_rate>
        <g_refresh_rate>120</g_refresh_rate>
    </global>
    <resolutions>
        <resolution>
            <width>{}</width>
            <height>{}</height>
            <refresh_rate>120</refresh_rate>
        </resolution>
    </resolutions>
    <options>
        <CustomEdid>false</CustomEdid>
        <PreventSpoof>false</PreventSpoof>
        <EdidCeaOverride>false</EdidCeaOverride>
        <!-- HardwareCursor=true: the OS does NOT paint the cursor into the
             VDD framebuffer (no DWM software-cursor compose step on this
             monitor). The capture pipeline composites the cursor itself
             via penflow-core::cursor_blit just before NV12 conversion,
             using the position+shape DDA reports through
             DXGI_OUTDUPL_FRAME_INFO. Saves the one-frame DWM-compose
             latency the false setting paid for; cost is ~10-30 µs/frame
             of GPU blit. -->
        <HardwareCursor>true</HardwareCursor>
        <SDR10bit>false</SDR10bit>
        <HDRPlus>false</HDRPlus>
        <logging>false</logging>
        <debuglogging>false</debuglogging>
    </options>
</vdd_settings>
"#,
        resolution.width, resolution.height
    )
}

/// Process-wide settings cell. The Tauri app holds this in its managed
/// state so commands can read+write under a single lock; the running
/// `Service` clones a fresh snapshot at the start of each session.
pub type SharedSettings = Arc<RwLock<Settings>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_vdd_resolution_is_valid() {
        validate(&Settings::default()).expect("default settings should be valid");
    }

    #[test]
    fn vdd_xml_uses_selected_resolution() {
        let xml = render_vdd_settings_xml(DisplayResolution {
            width: 1920,
            height: 1200,
        });
        assert!(xml.contains("<width>1920</width>"));
        assert!(xml.contains("<height>1200</height>"));
    }

    #[test]
    fn vdd_resolution_rejects_odd_dimensions() {
        let err = DisplayResolution {
            width: 1921,
            height: 1200,
        }
        .validate()
        .expect_err("odd width should be rejected");
        assert!(err.contains("even"));
    }
}
