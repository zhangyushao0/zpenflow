//! Pen + touch input injection.
//!
//! Two backends today, both Windows-only:
//!   - `win_ink` — WinRT `InputInjector` for pen pressure / tilt / hover /
//!     buttons / eraser. Gate-3-proven (HANDOFF §3.3 / §5.1).
//!   - `win_touch` — Win32 `InitializeTouchInjection` / `InjectTouchInput`
//!     for multi-touch (snapshot diff). Penflow already does this in the
//!     predecessor; OTD has nothing comparable on Windows.
//!
//! Cross-cutting helpers:
//!   - `coords::AffineTransform` — input area → output area mapping.
//!   - `binding::PenButtonProfile` — OTD-inspired button bindings.

pub mod binding;
pub mod coords;

#[cfg(windows)]
pub mod win_ink;
#[cfg(windows)]
pub mod win_touch;

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
    /// True iff the third stylus button has toggled the eraser end on. The
    /// pen injector flips the WinRT `Inverted` bit only when this changes,
    /// emitting a one-frame `out-of-range` first (HANDOFF §1.5
    /// "flip-then-flush").
    pub eraser: bool,
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
