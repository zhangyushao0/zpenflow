//! Multi-touch injection via Win32 `InitializeTouchInjection` /
//! `InjectTouchInput`.
//!
//! HANDOFF §2.3 #3 noted that the C# `WinRT` wrapper for touch injection
//! had misleading documentation; the predecessor's Python build hit
//! `E_NOINTERFACE`-style HRESULTs because the API takes an iterable, not a
//! single info object. The Rust port goes straight to the Win32 functions
//! (`user32.dll`), which take a typed slice of `POINTER_TOUCH_INFO` — clear,
//! documented, and matches the predecessor's working C++ path.
//!
//! Snapshot-diff design: callers pass the FULL set of currently-down
//! contacts each frame (not deltas). The injector computes flags
//! (`POINTER_FLAG_DOWN` / `_UPDATE` / `_UP`) by comparing against the
//! previous snapshot. This matches the Win32 API's expectation: "every
//! contact you don't include this call is treated as having gone up".

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO,
    POINTER_TOUCH_INFO, TOUCH_FEEDBACK_INDIRECT,
};
use windows::Win32::UI::WindowsAndMessaging::PT_TOUCH;

use crate::error::EngineResult;
#[cfg(test)]
use crate::error::EngineError;

use super::{TouchPoint, TouchState};

pub struct TouchInjector {
    /// Last submitted snapshot, indexed by contact id. Used to compute UP
    /// flags for contacts that disappeared this frame.
    last_ids: Vec<u32>,
    /// Cached `max_contacts` from `InitializeTouchInjection`; pure
    /// diagnostic.
    max_contacts: u32,
}

// SAFETY: Win32 InjectTouchInput is callable from any thread. The struct
// owns its `last_ids` Vec, no COM objects. Single-thread use model.
unsafe impl Send for TouchInjector {}

const TOUCH_MASK_NONE: u32 = 0;
// const TOUCH_MASK_CONTACTAREA: u32 = 1;  // unused; we don't supply contact rect dims
// const TOUCH_MASK_ORIENTATION: u32 = 2;
// const TOUCH_MASK_PRESSURE: u32 = 4;

impl TouchInjector {
    /// Initialise the touch injection subsystem. `max_contacts` is the
    /// largest number of simultaneous contacts the caller will ever submit;
    /// Windows enforces this. Reasonable default for Krita pan/zoom: 10.
    pub fn new(max_contacts: u32) -> EngineResult<Self> {
        unsafe { InitializeTouchInjection(max_contacts, TOUCH_FEEDBACK_INDIRECT)? };
        Ok(Self {
            last_ids: Vec::new(),
            max_contacts,
        })
    }

    pub fn max_contacts(&self) -> u32 {
        self.max_contacts
    }

    /// Submit one snapshot of currently-down contacts. Caller passes ONLY the
    /// contacts that are down or moving this frame; any id that was in the
    /// previous snapshot but is missing from this one is automatically
    /// emitted as a `POINTER_FLAG_UP`.
    pub fn inject_snapshot(&mut self, snapshot: &[TouchPoint]) -> EngineResult<()> {
        let mut contacts: Vec<POINTER_TOUCH_INFO> =
            Vec::with_capacity(snapshot.len() + self.last_ids.len());

        // Down / Update / Up explicit contacts from caller.
        for tp in snapshot {
            contacts.push(make_touch_info(tp.id, tp.x, tp.y, tp.state));
        }

        // Synthetic UP for contacts that disappeared this frame. We emit
        // these in the SAME call so the kernel sees a coherent batch.
        for old_id in &self.last_ids {
            if !snapshot.iter().any(|tp| tp.id == *old_id) {
                // Last known position is unknown to us at this layer — we
                // didn't keep coordinates. Use (0, 0) but mark UP only;
                // Windows resolves the up against the existing contact.
                contacts.push(make_touch_info(*old_id, 0, 0, TouchState::Up));
            }
        }

        if contacts.is_empty() {
            return Ok(());
        }
        unsafe { InjectTouchInput(&contacts)? };

        self.last_ids.clear();
        self.last_ids
            .extend(snapshot.iter().filter(|tp| tp.state != TouchState::Up).map(|tp| tp.id));
        Ok(())
    }
}

fn make_touch_info(id: u32, x: i32, y: i32, state: TouchState) -> POINTER_TOUCH_INFO {
    let flags = match state {
        TouchState::Down => {
            POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
        }
        TouchState::Update => {
            POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
        }
        TouchState::Up => POINTER_FLAG_UP,
    };
    POINTER_TOUCH_INFO {
        pointerInfo: POINTER_INFO {
            pointerType: PT_TOUCH,
            pointerId: id,
            pointerFlags: flags,
            ptPixelLocation: POINT { x, y },
            ..POINTER_INFO::default()
        },
        touchFlags: 0,
        touchMask: TOUCH_MASK_NONE,
        rcContact: RECT::default(),
        rcContactRaw: RECT::default(),
        orientation: 0,
        pressure: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: initialise the touch injection subsystem. Don't actually
    /// submit a snapshot — that would require a real touch input area on
    /// the desktop, which CI may not have. The init call alone proves the
    /// API surface compiles and links correctly.
    #[test]
    fn init_does_not_explode() {
        match TouchInjector::new(10) {
            Ok(_) => {}
            // Headless / non-interactive sessions may legitimately fail;
            // treat like the inject_probe pattern.
            Err(EngineError::Win32(e)) => {
                eprintln!("[skip] InitializeTouchInjection unavailable: {e:?}");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
