//! Pen + touch input injection.
//!
//! `win_ink` is the unified backend: a single WinRT
//! `Windows.UI.Input.Preview.Injection.InputInjector` instance handles both
//! pen pressure / tilt / hover / buttons / eraser AND multi-touch contacts.
//! Gate-3-proven (HANDOFF §3.3 / §5.1) for the pen path; touch is the same
//! WinRT API.
//!
//! Predecessor used Win32 `InjectTouchInput` for touch (HANDOFF §2.3 #3)
//! to dodge Python `winsdk` signature ambiguities. Those don't apply to
//! `windows-rs`, and Win32's hard thread-affinity rule (the calling thread
//! that ran `InitializeTouchInjection` must be the only thread that ever
//! calls `InjectTouchInput`) is incompatible with tokio's worker-thread
//! shuffling. WinRT's `InputInjector` is **agile** so we can drive it
//! freely from any tokio task.
//!
//! Cross-cutting helpers:
//!   - `coords::AffineTransform` — input area → output area mapping.
//!   - `binding::PenButtonProfile` — OTD-inspired button bindings.

pub mod binding;
pub mod coords;

#[cfg(windows)]
pub mod win_ink;

use std::time::Instant;

/// One pen sample after coordinate transform — i.e., already in virtual-screen
/// pixels and ready for WinRT `InputInjector`.
#[derive(Clone, Copy, Debug)]
pub struct PenSample {
    pub x: i32,
    pub y: i32,
    /// `[0, 1]`. Caller should already have applied the per-profile
    /// `tip_threshold`; the injector treats `pressure > 0` as the indicator
    /// for "in contact" unless `force_in_contact` is set.
    pub pressure: f32,
    pub tilt_x_deg: i32,
    pub tilt_y_deg: i32,
    pub in_range: bool,
    pub in_contact: bool,
    /// True iff the **physical eraser end** of the stylus is in use (Android
    /// reports `MotionEvent.TOOL_TYPE_ERASER`). The barrel-button-driven
    /// "eraser toggle" is handled separately inside the injector via the
    /// active `PenButtonProfile`; both contribute to the WinRT `Inverted`
    /// bit. HANDOFF §1.5 "flip-then-flush" still applies — a one-frame
    /// out-of-range sample is emitted whenever the *combined* eraser state
    /// changes.
    pub eraser: bool,
    /// Live barrel-button bitmask straight off the wire. Bit 0 = barrel-1
    /// (`BUTTON_STYLUS_PRIMARY`), bit 1 = barrel-2
    /// (`BUTTON_STYLUS_SECONDARY`), bit 2 = tertiary
    /// (`BUTTON_TERTIARY`). The Android side already decodes chord-style
    /// firmware into bit 2 so this is a clean per-button view; the
    /// injector reads transitions and applies the active
    /// `binding::PenButtonProfile`.
    pub buttons: u8,
    /// Captured-at instant for telemetry (latency measurement).
    pub captured_at: Option<Instant>,
}

/// One contact point in a touch snapshot — already in virtual-screen pixels.
#[derive(Clone, Copy, Debug)]
pub struct TouchPoint {
    /// Stable contact ID across the lifetime of the touch (down → moves → up).
    /// Client-side `TouchInputCapture.kt` already supplies these; we forward
    /// them directly so `POINTER_FLAG_NEW` lights up correctly.
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub state: TouchState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TouchState {
    Down,
    Update,
    Up,
}
