//! Pen + touch input injection.
//!
//! `win_ink` is the unified backend: two Win32 synthetic pointer devices
//! (one PT_PEN, one PT_TOUCH) registered via `CreateSyntheticPointerDevice`,
//! driven by `InjectSyntheticPointerInput`. Pen path supplies pressure /
//! tilt / hover / eraser; touch path handles up to 10 simultaneous contacts.
//!
//! Why not the WinRT `InputInjector` wrapper anymore: it shared the same
//! kernel synthetic-pointer pipeline as the Win32 path but added an opaque
//! C++/WinRT marshalling layer on top. Moving off it didn't fix issue #3
//! on its own — the actual offset bug is documented in `win_ink.rs`'s
//! module-level comment (kernel treats `ptPixelLocation` as offset from
//! the virtual-screen bounding box, not primary-relative — issue #16).
//! The Win32 path keeps us closer to the real coordinate contract and
//! makes that translation explicit in code.
//!
//! `InjectSyntheticPointerInput` does not have the hard thread-affinity
//! rule that the older Win32 `InjectTouchInput` did (HANDOFF §2.3 #3), so
//! we can drive it from any tokio worker through the session's
//! `Mutex<InputInjector>` like before.
//!
//! Cross-cutting helpers:
//!   - `coords::AffineTransform` — input area → output area mapping.
//!   - `binding::PenButtonProfile` — OTD-inspired button bindings.

pub mod binding;
pub mod coords;

#[cfg(windows)]
pub mod win_ink;

#[cfg(windows)]
pub mod vmulti;

use std::time::Instant;

/// One pen sample after coordinate transform — i.e., already in virtual-screen
/// pixels and ready for `InjectSyntheticPointerInput`.
///
/// Carries two coordinate flavors: `x`/`y` are i32 virtual-screen-bbox-
/// relative pixels (consumed by the Win32 synthetic-pointer fallback path
/// and by cursor / overlay code); `x_logical`/`y_logical` are VMulti's
/// `[0, 32767]` HID logical units (consumed by the VMulti path when a
/// VMulti driver is installed). The caller computes both from the same
/// `(x_norm, y_norm)` so the injector can route to whichever backend is
/// active without re-doing the transform.
#[derive(Clone, Copy, Debug)]
pub struct PenSample {
    pub x: i32,
    pub y: i32,
    /// VMulti logical-axis position, `[0, 32767]`. Same point as `x`/`y`,
    /// just scaled into VMulti's HID descriptor range.
    pub x_logical: u16,
    /// VMulti logical-axis position, `[0, 32767]`.
    pub y_logical: u16,
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
    /// active `PenButtonProfile`; both contribute to the `PEN_FLAG_INVERTED`
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
