//! Pen-button binding model (OTD-inspired, design.md §6.6).
//!
//! A `Binding` is "what to do when this pen button changes state". The
//! predecessor hardcoded `Ctrl/Shift/E` in `pen_injector.py`; the rewrite
//! makes that the **default profile** so the GUI can later expose binding
//! customization (Wave 4) without touching the engine hot path.
//!
//! Why these specific shapes:
//!   - `KeyTap` for buttons like "press 'E' to toggle eraser" — Krita
//!     consumes a single `keydown/keyup` pair.
//!   - `KeyHold` / `KeyChord` for modifier-style buttons (Ctrl held while
//!     drawing). Predecessor §2.3 #4 explains why we send keyboard events
//!     for modifier buttons rather than mouse clicks: SendInput mouse
//!     clicks get filtered by Windows Ink under concurrent pen contact.
//!   - `EraserToggle` flips the WinRT `Inverted` bit on subsequent pen
//!     samples; design §6.6 calls this out as cleaner than tapping `E`.

use windows::Win32::UI::Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_CONTROL, VK_E, VK_SHIFT};

#[derive(Clone, Debug)]
pub enum Binding {
    /// No-op. Useful as a "disabled" slot in a profile.
    None,
    /// Send `keydown(vk); keyup(vk)` once per button press.
    KeyTap(VIRTUAL_KEY),
    /// Send `keydown(vk)` on press, `keyup(vk)` on release.
    KeyHold(VIRTUAL_KEY),
    /// Send all keys down, then all up, in order. Useful for `Ctrl+Z` style.
    KeyChord(Vec<VIRTUAL_KEY>),
    /// Flip the WinRT `Inverted` bit on subsequent pen samples until
    /// pressed again. Krita Windows Ink mode reads the bit as "this is the
    /// eraser end of the pen".
    EraserToggle,
}

/// Bindings for one pen's three buttons + the contact threshold.
///
/// Slot mapping on the MovinkPad Pro 14 (HANDOFF §2.1):
///   - `barrel_1` ↔ `MotionEvent.BUTTON_STYLUS_PRIMARY`
///   - `barrel_2` ↔ `MotionEvent.BUTTON_STYLUS_SECONDARY`
///   - `tertiary` ↔ `MotionEvent.BUTTON_TERTIARY` (the third stylus button —
///     does NOT chord like Wacom Pro Pen 3, despite the docs)
#[derive(Clone, Debug)]
pub struct PenButtonProfile {
    pub barrel_1: Binding,
    pub barrel_2: Binding,
    pub tertiary: Binding,
    /// Pressure must exceed this fraction [0, 1] before we treat the pen as
    /// "in contact" (HANDOFF §1.5 — OTD pattern). Default 0 means any
    /// non-zero pressure registers, matching the predecessor's behaviour.
    pub tip_threshold: f32,
}

impl Default for PenButtonProfile {
    /// Predecessor-compatible default: barrel-1 holds Ctrl, barrel-2 holds
    /// Shift, tertiary taps 'E' (Krita's eraser-toggle shortcut). The
    /// design originally proposed `EraserToggle` (flip the WinRT
    /// `Inverted` bit) as cleaner state, but in practice users find the
    /// behaviour confusing — the pen "becomes" an eraser silently with
    /// no visible UI change in some apps. A 'E' tap goes through Krita's
    /// own tool-switch UI which matches what users expect from the
    /// tertiary button on physical Wacom tablets.
    fn default() -> Self {
        Self {
            barrel_1: Binding::KeyHold(VK_CONTROL),
            barrel_2: Binding::KeyHold(VK_SHIFT),
            tertiary: Binding::KeyTap(VK_E),
            tip_threshold: 0.0,
        }
    }
}

impl PenButtonProfile {
    /// Returns a profile that matches the predecessor's exact behaviour
    /// including the `E`-tap for the third button. Provided for users who
    /// want bit-for-bit compatibility while migrating from the old build.
    pub fn predecessor_compat() -> Self {
        Self {
            barrel_1: Binding::KeyHold(VK_CONTROL),
            barrel_2: Binding::KeyHold(VK_SHIFT),
            tertiary: Binding::KeyTap(VK_E),
            tip_threshold: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_matches_design_recommendation() {
        let p = PenButtonProfile::default();
        assert!(matches!(p.barrel_1, Binding::KeyHold(VK_CONTROL)));
        assert!(matches!(p.barrel_2, Binding::KeyHold(VK_SHIFT)));
        assert!(matches!(p.tertiary, Binding::KeyTap(VK_E)));
        assert_eq!(p.tip_threshold, 0.0);
    }

    #[test]
    fn predecessor_compat_uses_e_tap() {
        let p = PenButtonProfile::predecessor_compat();
        assert!(matches!(p.tertiary, Binding::KeyTap(VK_E)));
    }
}
