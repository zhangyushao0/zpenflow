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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bitrate_bps: default_bitrate(),
            fps: default_fps(),
            codec: default_codec(),
            bindings: PenBindings::default(),
            autostart: false,
            run_as_admin: false,
        }
    }
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

/// Process-wide settings cell. The Tauri app holds this in its managed
/// state so commands can read+write under a single lock; the running
/// `Service` clones a fresh snapshot at the start of each session.
pub type SharedSettings = Arc<RwLock<Settings>>;
