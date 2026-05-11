//! Unified pen + touch injection via Win32 synthetic pointer devices.
//!
//! Replaces the earlier `Windows.UI.Input.Preview.Injection.InputInjector`
//! WinRT wrapper. The downstream effect is identical — Windows kernel
//! routes synthetic pointer events to Windows Ink-aware apps the same way
//! whether they originated from the WinRT wrapper or from a direct
//! `InjectSyntheticPointerInput` call. We moved off WinRT because that
//! layer was opaque about how it interpreted `PixelLocation` in 3+ monitor
//! topologies (issue #3 reproducer: VDD's pen offset proportional to a
//! third monitor's position in the virtual desktop).
//!
//! Coordinate-space note (issue #16): MSDN documents
//! `POINTER_INFO.ptPixelLocation` as virtual-desktop pixels — primary
//! monitor's top-left = (0, 0), with negative coords allowed for monitors
//! above/left of primary. The kernel synthetic-pointer router does NOT
//! follow this convention: it treats `ptPixelLocation` as offset from the
//! **virtual-screen bounding box** (i.e., from
//! `(SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN)`). When the topology has any
//! monitor extending above or left of primary, the two spaces differ by
//! exactly that origin, and pen strokes land the same delta away from the
//! pen tip (issue #16: 4K monitor taller than primary → primary's `top` is
//! negative in DXGI coords → injected coords land ~3 cm above the physical
//! touch on the VDD). We translate primary-relative → bbox-relative inside
//! `virtual_screen_origin()`.
//!
//! Wave-2 packaging note carried forward: `CreateSyntheticPointerDevice` is
//! a documented user32 export with no `inputInjectionBrokered` capability
//! requirement (that was a WinRT-side restriction). It works in unpackaged
//! and packaged Win32 processes alike, as long as the process is not under
//! a Service-context isolation that strips access to the active console
//! session's input subsystem.

use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_NEW, POINTER_FLAG_PRIMARY,
    POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_FLAG_INVERTED, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y,
    PT_PEN, PT_TOUCH, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, TOUCH_MASK_NONE,
};

use crate::error::{EngineError, EngineResult};

use super::binding::{Binding, MouseButtonKind, PenButtonProfile};
use super::vmulti::{VMultiPen, VMultiPenSample};
use super::{PenSample, TouchPoint};

/// Multi-touch device capacity. Win32 docs allow up to 256, but the kernel
/// caps practical injection at MAX_TOUCH_COUNT=10. Matches the predecessor
/// (HANDOFF §1.5) and is plenty for the MovinkPad's 10-finger panel.
const MAX_TOUCH_CONTACTS: u32 = 10;

/// Unified pen + touch injector backed by Win32 synthetic pointer devices.
pub struct InputInjector {
    /// `CreateSyntheticPointerDevice(PT_PEN, 1, …)` handle. Must outlive
    /// every `InjectSyntheticPointerInput` we issue.
    pen_device: HSYNTHETICPOINTERDEVICE,
    /// `CreateSyntheticPointerDevice(PT_TOUCH, MAX_TOUCH_CONTACTS, …)`.
    touch_device: HSYNTHETICPOINTERDEVICE,

    // --- pen flip-then-flush state (HANDOFF §1.5) ---
    last_pen_eraser: bool,
    last_pen_in_range: bool,
    /// Tracks the previous `in_contact` separately from `in_range` so we can
    /// emit the correct DOWN/UP/UPDATE transition bits — the WinRT layer
    /// computed these internally, the Win32 contract makes us explicit.
    last_pen_in_contact: bool,

    // --- pen-button state machine ---
    pen_profile: PenButtonProfile,
    last_pen_buttons: u8,
    pen_eraser_sticky: bool,

    // --- touch state machine ---
    last_touch_pos: HashMap<u32, (i32, i32)>,

    /// VMulti virtual digitizer (issue #23 follow-up). When `Some`, every
    /// `inject_pen` writes a VMulti HID report instead of an
    /// `InjectSyntheticPointerInput` synthetic pen frame — this gives the
    /// receiver a full HID descriptor with declared logical resolution
    /// (32767 per axis) so sub-pixel coords survive end-to-end with no
    /// `ptHimetricLocation` scale guessing. `None` if the user hasn't
    /// installed VMulti, in which case we keep using the legacy synthetic
    /// pointer path. Barrel-button bindings (`PenButtonProfile`) keep
    /// working regardless via the existing `SendInput` path; the choice
    /// here only affects the position/pressure/tilt sample stream.
    vmulti: Option<VMultiPen>,
}

// SAFETY: Win32 synthetic pointer device handles are documented as usable
// from any thread once created (unlike Win32 InjectTouchInput, which has
// hard thread-affinity to whichever thread called InitializeTouchInjection).
// We serialise calls through the session's `Mutex<InputInjector>` anyway.
unsafe impl Send for InputInjector {}

impl InputInjector {
    /// Build the injector and register synthetic pointer devices for both
    /// pen (max 1 simultaneous) and touch (max 10). Sets
    /// `PER_MONITOR_AWARE_V2` process-wide so injected pixel coordinates are
    /// physical pixels, not DIPs.
    pub fn new() -> EngineResult<Self> {
        // Process-wide; idempotent. Returns an error if already set, which
        // we ignore — that case means the host (Tauri) already configured
        // a DPI awareness mode. Note: if the host set it to something other
        // than per-monitor-aware-v2, we silently inherit that mode and
        // ptPixelLocation will be interpreted as DIPs. The matching call
        // in `Engine::start` enumerates monitors under the same context, so
        // the input/output spaces stay consistent either way.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

        let pen_device = unsafe {
            CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)
                .map_err(EngineError::from)?
        };
        let touch_device = match unsafe {
            CreateSyntheticPointerDevice(PT_TOUCH, MAX_TOUCH_CONTACTS, POINTER_FEEDBACK_DEFAULT)
        } {
            Ok(h) => h,
            Err(e) => {
                // Tear down the pen device before bailing — we own it now.
                unsafe { DestroySyntheticPointerDevice(pen_device) };
                return Err(EngineError::from(e));
            }
        };

        // Probe for VMulti at startup. Not finding it is the common case
        // for users who haven't installed the driver yet — log clearly
        // (so HUD / bug reports tell us which path is active) and fall
        // through to the synthetic-pointer path.
        let vmulti = match VMultiPen::open() {
            Ok(v) => {
                eprintln!("[inject] VMulti HID digitizer found — using VMulti path for pen");
                Some(v)
            }
            Err(e) => {
                eprintln!(
                    "[inject] VMulti probe: {e}; falling back to InjectSyntheticPointerInput \
                     (install X9VoiD/vmulti-bin for higher-fidelity pen)"
                );
                None
            }
        };

        Ok(Self {
            pen_device,
            touch_device,
            last_pen_eraser: false,
            last_pen_in_range: false,
            last_pen_in_contact: false,
            pen_profile: PenButtonProfile::default(),
            last_pen_buttons: 0,
            pen_eraser_sticky: false,
            last_touch_pos: HashMap::new(),
            vmulti,
        })
    }

    /// Whether VMulti was found at startup and is the active pen backend.
    pub fn using_vmulti(&self) -> bool {
        self.vmulti.is_some()
    }

    /// Replace the active pen-button binding profile. Any keys currently
    /// held by the previous profile via `Binding::KeyHold` are released
    /// before the swap so they don't get stuck.
    pub fn set_pen_profile(&mut self, profile: PenButtonProfile) {
        let bits_to_release = self.last_pen_buttons;
        for slot in 0u8..3 {
            let mask = 1u8 << slot;
            if bits_to_release & mask == 0 {
                continue;
            }
            let binding = match slot {
                0 => &self.pen_profile.barrel_1,
                1 => &self.pen_profile.barrel_2,
                _ => &self.pen_profile.tertiary,
            };
            match binding {
                Binding::KeyHold(keys) => {
                    for vk in keys.iter().rev() {
                        let _ = send_key(*vk, false);
                    }
                }
                Binding::MouseButton(kind) => {
                    let _ = send_mouse_button(*kind, false);
                }
                _ => {}
            }
        }
        self.last_pen_buttons = 0;
        self.pen_profile = profile;
    }

    /// Inject one pen sample. Implements the eraser flip-then-flush protocol
    /// from HANDOFF §1.5: when the eraser bit changes mid-stroke, emit one
    /// synthetic out-of-range frame at the previous coordinates with the
    /// OLD eraser state so the driver sees a clean `Inverted` transition.
    ///
    /// Also dispatches barrel-button transitions through the active
    /// `PenButtonProfile` BEFORE the pen sample lands.
    ///
    /// Routes to VMulti when present (issue #23). The synthetic-pointer
    /// path is the fallback for users who haven't installed the driver.
    /// Barrel-button bindings keep going through `dispatch_pen_buttons` →
    /// `SendInput` either way.
    pub fn inject_pen(&mut self, sample: &PenSample) -> EngineResult<()> {
        self.dispatch_pen_buttons(sample.buttons)?;

        let effective_eraser = sample.eraser || self.pen_eraser_sticky;

        if self.vmulti.is_some() {
            // VMulti's HID descriptor encodes eraser as a button bit
            // (`Invert`) that the kernel translates into `PEN_FLAG_INVERTED`
            // on the receiver side. There's no analog of the synthetic-
            // pointer "out-of-range flush on eraser transition" hack
            // needed — the HID class driver handles transitions cleanly
            // because the report stream is a continuous truth stream
            // (every report says what the pen is doing now, not a delta).
            self.write_pen_vmulti(sample, effective_eraser)?;
        } else {
            if effective_eraser != self.last_pen_eraser && self.last_pen_in_range {
                let mut flush = *sample;
                flush.in_range = false;
                flush.in_contact = false;
                flush.pressure = 0.0;
                self.write_pen(&flush, self.last_pen_eraser)?;
            }
            self.write_pen(sample, effective_eraser)?;
        }

        self.last_pen_eraser = effective_eraser;
        self.last_pen_in_range = sample.in_range;
        self.last_pen_in_contact = sample.in_contact;
        Ok(())
    }

    fn write_pen_vmulti(&mut self, sample: &PenSample, eraser: bool) -> EngineResult<()> {
        let vmulti = self.vmulti.as_mut().expect("checked by caller");
        // Pressure: PenSample.pressure is f32 [0, 1]. VMulti extended
        // accepts [0, 16383]. Map and clamp.
        let pressure_u16 = (sample.pressure.clamp(0.0, 1.0) * 16383.0).round() as u16;
        // Tilts: PenSample carries i32 degrees. VMulti wants i8 in
        // [-127, 127]. Clamp to ±90 first (real digitizers report ±60
        // typically), then cast.
        let tilt_x = sample.tilt_x_deg.clamp(-90, 90) as i8;
        let tilt_y = sample.tilt_y_deg.clamp(-90, 90) as i8;

        let vsample = VMultiPenSample {
            x: sample.x_logical,
            y: sample.y_logical,
            pressure: pressure_u16,
            tilt_x_deg: tilt_x,
            tilt_y_deg: tilt_y,
            tip_down: sample.in_contact,
            barrel: sample.buttons & 0b001 != 0, // bit 0 = primary barrel
            eraser,
            inverted: eraser,
            in_range: sample.in_range,
        };
        vmulti.write_pen(&vsample).map_err(|e| match e {
            crate::inject::vmulti::VMultiError::Win32(w) => EngineError::Win32(w),
            crate::inject::vmulti::VMultiError::NotFound => {
                // Shouldn't happen — handle was opened at startup. Surface
                // as a Win32 error so callers see something.
                EngineError::Win32(windows::core::Error::from_thread())
            }
        })
    }

    /// Edge-triggered binding dispatch for the three barrel buttons.
    fn dispatch_pen_buttons(&mut self, now_bits: u8) -> EngineResult<()> {
        let prev = self.last_pen_buttons;
        let pressed_now = !prev & now_bits;
        let released_now = prev & !now_bits;

        for slot in 0u8..3 {
            let mask = 1u8 << slot;
            let binding = match slot {
                0 => &self.pen_profile.barrel_1,
                1 => &self.pen_profile.barrel_2,
                _ => &self.pen_profile.tertiary,
            };

            if pressed_now & mask != 0 {
                match binding {
                    Binding::None => {}
                    Binding::KeyTap(vk) => {
                        send_key(*vk, true)?;
                        send_key(*vk, false)?;
                    }
                    Binding::KeyHold(keys) => {
                        // Send each key down in declared order. Release
                        // happens below in the falling-edge branch — same
                        // keys, reverse order.
                        for vk in keys {
                            send_key(*vk, true)?;
                        }
                    }
                    Binding::KeyChord(keys) => {
                        for vk in keys {
                            send_key(*vk, true)?;
                        }
                        for vk in keys.iter().rev() {
                            send_key(*vk, false)?;
                        }
                    }
                    Binding::MouseButton(kind) => send_mouse_button(*kind, true)?,
                    Binding::EraserToggle => {
                        self.pen_eraser_sticky = !self.pen_eraser_sticky;
                    }
                }
            }

            if released_now & mask != 0 {
                match binding {
                    Binding::KeyHold(keys) => {
                        // Reverse-order release so that, for `Ctrl+Shift`,
                        // Shift releases first and Ctrl second — matches
                        // what users actually do on a keyboard and avoids
                        // a brief "just Ctrl" window between releases that
                        // some apps treat as a tool-mode change.
                        for vk in keys.iter().rev() {
                            send_key(*vk, false)?;
                        }
                    }
                    Binding::MouseButton(kind) => send_mouse_button(*kind, false)?,
                    _ => {}
                }
            }
        }

        self.last_pen_buttons = now_bits;
        Ok(())
    }

    fn write_pen(&self, sample: &PenSample, eraser: bool) -> EngineResult<()> {
        // Both prior states false AND both new states false → nothing to
        // synthesise. The Win32 API rejects a frame with neither INRANGE
        // nor UP/DOWN as ERROR_INVALID_PARAMETER, whereas the WinRT layer
        // tolerated it.
        if !self.last_pen_in_range && !sample.in_range {
            return Ok(());
        }

        let flags = pen_pointer_flags(
            self.last_pen_in_range,
            self.last_pen_in_contact,
            sample.in_range,
            sample.in_contact,
        );

        let pen_flags: u32 = if eraser { PEN_FLAG_INVERTED } else { 0 };
        let pressure_1024 = (sample.pressure.clamp(0.0, 1.0) * 1024.0).round() as u32;
        let tilt_x = sample.tilt_x_deg.clamp(-90, 90);
        let tilt_y = sample.tilt_y_deg.clamp(-90, 90);

        let (vx, vy) = virtual_screen_origin();
        let pen_info = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: 1,
                pointerFlags: flags,
                ptPixelLocation: POINT {
                    x: sample.x - vx,
                    y: sample.y - vy,
                },
                ..Default::default()
            },
            penFlags: pen_flags,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure: pressure_1024,
            rotation: 0,
            tiltX: tilt_x,
            tiltY: tilt_y,
        };

        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 { penInfo: pen_info },
        };

        let infos = [info];
        unsafe { InjectSyntheticPointerInput(self.pen_device, &infos).map_err(EngineError::from)? };
        Ok(())
    }

    /// Inject a multi-touch snapshot. Diff against the previous snapshot to
    /// compute per-contact transition bits:
    ///
    /// - **New id** (not in previous): `NEW | DOWN | INCONTACT | INRANGE | FIRSTBUTTON`
    /// - **Persistent id** (in both): `UPDATE | INCONTACT | INRANGE | FIRSTBUTTON`
    /// - **Lost id** (in previous, missing now): `UP` at the last known position
    ///
    /// The first contact in the snapshot also gets `PRIMARY`. `state` field
    /// on `TouchPoint` is **ignored** — the diff overrides whatever the caller
    /// passed.
    pub fn inject_touch(&mut self, snapshot: &[TouchPoint]) -> EngineResult<()> {
        if snapshot.is_empty() && self.last_touch_pos.is_empty() {
            return Ok(());
        }

        let mut infos: Vec<POINTER_TYPE_INFO> = Vec::new();
        let mut current_ids: HashSet<u32> = HashSet::with_capacity(snapshot.len());

        // 1. Down / Update for every contact in the new snapshot.
        for (i, tp) in snapshot.iter().enumerate() {
            current_ids.insert(tp.id);
            let was_down = self.last_touch_pos.contains_key(&tp.id);
            let mut flags =
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
            if was_down {
                flags |= POINTER_FLAG_UPDATE;
            } else {
                flags |= POINTER_FLAG_NEW | POINTER_FLAG_DOWN;
            }
            if i == 0 {
                flags |= POINTER_FLAG_PRIMARY;
            }
            infos.push(make_touch_info(tp.id, tp.x, tp.y, flags));
        }

        // 2. Up for ids that disappeared this frame, using the last known
        //    position (Win32 requires UP at a real coord, not (0,0)).
        let mut lost: Vec<(u32, i32, i32)> = Vec::new();
        for (id, pos) in self.last_touch_pos.iter() {
            if !current_ids.contains(id) {
                lost.push((*id, pos.0, pos.1));
            }
        }
        for (id, x, y) in lost {
            infos.push(make_touch_info(id, x, y, POINTER_FLAG_UP));
        }

        if infos.is_empty() {
            return Ok(());
        }
        // Hard ceiling per CreateSyntheticPointerDevice's maxCount.
        debug_assert!(infos.len() <= MAX_TOUCH_CONTACTS as usize);

        unsafe {
            InjectSyntheticPointerInput(self.touch_device, &infos).map_err(EngineError::from)?
        };

        self.last_touch_pos.clear();
        for tp in snapshot {
            self.last_touch_pos.insert(tp.id, (tp.x, tp.y));
        }
        Ok(())
    }
}

impl Drop for InputInjector {
    fn drop(&mut self) {
        unsafe {
            DestroySyntheticPointerDevice(self.touch_device);
            DestroySyntheticPointerDevice(self.pen_device);
        }
    }
}

/// `(SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN)` — the top-left corner of the
/// virtual desktop's bounding rectangle. Equal to (0, 0) only when no
/// monitor extends above or left of the primary; otherwise negative in the
/// extended dimension.
///
/// We need this because of an undocumented coordinate-space mismatch on
/// `InjectSyntheticPointerInput`. The MSDN docs describe `ptPixelLocation`
/// as virtual-desktop pixels (primary-relative, with negatives allowed for
/// monitors that hang above or left of the primary) — same convention as
/// `IDXGIOutput::GetDesc().DesktopCoordinates`. In practice the kernel
/// pointer router treats `ptPixelLocation` as **bbox-relative** (offset
/// from `(SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN)`), so feeding raw DXGI
/// coords lands the synthetic pen exactly `(SM_XVIRTUALSCREEN,
/// SM_YVIRTUALSCREEN)` away from where it should be on any topology with a
/// non-zero virtual-screen origin (issue #16: VDD on the right of a 4K
/// taller than primary → 4K's `top` is negative → pen strokes appear ~3 cm
/// above the physical touch). Subtracting the origin here converts our
/// primary-relative coords to bbox-relative so the kernel routes events
/// onto the right monitor.
///
/// Re-queried per call rather than cached: cheap (~tens of ns), and
/// monitor topology changes (hot-plug, display-arrangement edit) move
/// `SM_*VIRTUALSCREEN` mid-session — caching would freeze the offset at
/// session start.
fn virtual_screen_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

/// Compose the per-frame pointer-flag set for a pen sample given the
/// previous and current `(in_range, in_contact)` states. Pure function so
/// the transition matrix is unit-testable without spinning up a real
/// pointer device.
fn pen_pointer_flags(
    was_in_range: bool,
    was_in_contact: bool,
    in_range: bool,
    in_contact: bool,
) -> POINTER_FLAGS {
    if !in_range {
        // Leaving proximity (or already gone). UP is the documented terminal
        // transition; INRANGE is intentionally cleared.
        return POINTER_FLAG_UP;
    }

    let mut flags = POINTER_FLAG_INRANGE;
    if !was_in_range {
        flags |= POINTER_FLAG_NEW;
    }
    if in_contact {
        flags |= POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
        if was_in_contact {
            flags |= POINTER_FLAG_UPDATE;
        } else {
            flags |= POINTER_FLAG_DOWN;
        }
    } else if was_in_contact {
        // Contact → hover-only: lift but stay in range.
        flags |= POINTER_FLAG_UP;
    } else {
        flags |= POINTER_FLAG_UPDATE;
    }
    flags
}

/// Build a single touch contact's POINTER_TYPE_INFO. Contact rect is the
/// pixel itself (1×1) — we don't have geometric contact-area data from the
/// Android side, and apps that don't query rcContact are unaffected.
///
/// `(x, y)` arrives in primary-monitor-relative virtual-desktop pixels (DXGI
/// `DesktopCoordinates` space). We translate to virtual-screen-bbox-relative
/// pixels here so the kernel pointer router lands events on the correct
/// monitor — see `virtual_screen_origin` for why.
fn make_touch_info(id: u32, x: i32, y: i32, flags: POINTER_FLAGS) -> POINTER_TYPE_INFO {
    let (vx, vy) = virtual_screen_origin();
    let kx = x - vx;
    let ky = y - vy;
    let touch_info = POINTER_TOUCH_INFO {
        pointerInfo: POINTER_INFO {
            pointerType: PT_TOUCH,
            pointerId: id,
            pointerFlags: flags,
            ptPixelLocation: POINT { x: kx, y: ky },
            ..Default::default()
        },
        touchFlags: 0,
        touchMask: TOUCH_MASK_NONE,
        rcContact: windows::Win32::Foundation::RECT {
            left: kx,
            top: ky,
            right: kx + 1,
            bottom: ky + 1,
        },
        rcContactRaw: windows::Win32::Foundation::RECT {
            left: kx,
            top: ky,
            right: kx + 1,
            bottom: ky + 1,
        },
        orientation: 0,
        pressure: 0,
    };

    POINTER_TYPE_INFO {
        r#type: PT_TOUCH,
        Anonymous: POINTER_TYPE_INFO_0 {
            touchInfo: touch_info,
        },
    }
}

/// One `SendInput` keyboard event. `down=true` is keydown, `false` is keyup.
/// We use `SendInput` rather than the synthetic pointer path because
/// pen-button modifier keys must look like a real keyboard to apps like
/// Krita that only honour modifiers during ongoing pen contact (HANDOFF §2.3
/// #4).
fn send_key(vk: VIRTUAL_KEY, down: bool) -> EngineResult<()> {
    let flags = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [input];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(EngineError::Win32(windows::core::Error::from_thread()));
    }
    Ok(())
}

/// One `SendInput` mouse-button event for a `Binding::MouseButton`.
/// HANDOFF §2.3 #4 warned that mouse-button presses get filtered by Windows
/// Ink during in-stroke pen contact — it's still useful for off-stroke
/// barrel-button → right-click style mappings (Wacom Cintiq convention).
fn send_mouse_button(kind: MouseButtonKind, down: bool) -> EngineResult<()> {
    let flags: MOUSE_EVENT_FLAGS = match (kind, down) {
        (MouseButtonKind::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButtonKind::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButtonKind::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButtonKind::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButtonKind::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButtonKind::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    };
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [input];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(EngineError::Win32(windows::core::Error::from_thread()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::TouchState;
    use super::*;

    #[test]
    fn pen_flags_hover_arrival() {
        // Pen first appears in range, no contact — NEW | INRANGE | UPDATE.
        let f = pen_pointer_flags(false, false, true, false);
        assert!(f & POINTER_FLAG_NEW != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_INRANGE != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_UPDATE != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_INCONTACT == POINTER_FLAGS(0));
    }

    #[test]
    fn pen_flags_contact_arrival_without_hover() {
        // Pen jumps directly from out-of-range to contact (rare but possible
        // when the Android side coalesces the hover frame).
        let f = pen_pointer_flags(false, false, true, true);
        assert!(f & POINTER_FLAG_NEW != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_DOWN != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_INCONTACT != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_FIRSTBUTTON != POINTER_FLAGS(0));
    }

    #[test]
    fn pen_flags_hover_to_contact_emits_down() {
        let f = pen_pointer_flags(true, false, true, true);
        assert!(f & POINTER_FLAG_DOWN != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_NEW == POINTER_FLAGS(0));
    }

    #[test]
    fn pen_flags_contact_continuing_emits_update() {
        let f = pen_pointer_flags(true, true, true, true);
        assert!(f & POINTER_FLAG_UPDATE != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_DOWN == POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_INCONTACT != POINTER_FLAGS(0));
    }

    #[test]
    fn pen_flags_contact_to_hover_emits_up_inrange() {
        // Lift-but-still-detected: UP transition while INRANGE persists.
        let f = pen_pointer_flags(true, true, true, false);
        assert!(f & POINTER_FLAG_UP != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_INRANGE != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_INCONTACT == POINTER_FLAGS(0));
    }

    #[test]
    fn pen_flags_leave_proximity_drops_inrange() {
        let f = pen_pointer_flags(true, false, false, false);
        assert!(f & POINTER_FLAG_UP != POINTER_FLAGS(0));
        assert!(f & POINTER_FLAG_INRANGE == POINTER_FLAGS(0));
    }

    /// Smoke: build the unified injector. The only failure modes here are
    /// ones we can't fix from inside the test (Service-isolated context, or
    /// a stripped-down WinPE shell with no input subsystem).
    #[test]
    fn create_injector() {
        match InputInjector::new() {
            Ok(_) => {}
            Err(EngineError::Win32(e)) => {
                eprintln!("[skip] InputInjector unavailable in this context: {e:?}");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// Inject a single hover sample at (1, 1) and a one-finger touch at
    /// (1, 1). Cursor blip is the same as the gate-3 probe, harmless.
    #[test]
    fn pen_and_touch_smoke() {
        let mut inj = match InputInjector::new() {
            Ok(i) => i,
            Err(EngineError::Win32(e)) => {
                eprintln!("[skip] InputInjector unavailable: {e:?}");
                return;
            }
            Err(other) => panic!("unexpected: {other:?}"),
        };

        let pen = PenSample {
            x: 1,
            y: 1,
            x_logical: 1,
            y_logical: 1,
            pressure: 0.0,
            tilt_x_deg: 0,
            tilt_y_deg: 0,
            in_range: true,
            in_contact: false,
            eraser: false,
            buttons: 0,
            captured_at: None,
        };
        inj.inject_pen(&pen).expect("pen hover");

        let touch_down = vec![TouchPoint {
            id: 1,
            x: 1,
            y: 1,
            state: TouchState::Update,
        }];
        inj.inject_touch(&touch_down).expect("touch down");
        inj.inject_touch(&[]).expect("touch up");
    }
}
