//! Pen-injection smoke probe for the Win32 synthetic-pointer path.
//!
//! Reference: design.md §6.6, HANDOFF §5.1.
//!
//! Earlier waves used `Windows.UI.Input.Preview.Injection.InputInjector`
//! (the WinRT wrapper). Issue #3 — pen offset proportional to monitor
//! arrangement on 3-monitor setups — traced back to that wrapper's opaque
//! coordinate-space handling, so the engine moved to the underlying Win32
//! API directly: `CreateSyntheticPointerDevice` + `InjectSyntheticPointerInput`.
//!
//! What the probe checks:
//!   1. `CreateSyntheticPointerDevice(PT_PEN, 1, …)` succeeds (no special
//!      capability needed for Win32; the WinRT-only `inputInjectionBrokered`
//!      restriction does not apply here).
//!   2. A real `InjectSyntheticPointerInput` call returns success on the
//!      virtual-screen coordinate `ptPixelLocation` we set.
//!
//! On PASS: a tiny ~80px horizontal stroke is drawn at mid-screen with
//! pressure 0.5–0.9. Open Krita / OneNote / any Windows Ink consumer to
//! confirm the events arrive as **pen** (with pressure) and not mouse.
//!
//! Run: `cargo run -p penflow-core --example inject_probe`.
//! Exit code 0 = PASS, 1 = injection rejected, 2 = setup error.

use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use windows::core::Result;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_NEW, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{PEN_MASK_PRESSURE, PT_PEN};

fn main() -> ExitCode {
    unsafe {
        // PER_MONITOR_AWARE_V2 so injected pixel coordinates are physical
        // pixels and not DIPs. Without this, on a 4K monitor at 150% scaling,
        // a click at "(1920, 1080)" lands at the wrong place.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    match run_probe() {
        Ok(()) => {
            println!();
            println!("=== VERDICT: PASS ===");
            println!("Win32 synthetic pointer injection works in this binary.");
            println!("A pen stroke (pressure ramp 0.5→0.9) was drawn at mid-screen — verify in");
            println!("Krita (Windows Ink mode) or OneNote that you saw a pressured stroke.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[FAIL] {e:?}");
            eprintln!();
            eprintln!("=== VERDICT: FAIL ===");
            eprintln!("Win32 synthetic pointer injection was refused. Likely causes:");
            eprintln!("  - Service-context isolation (running under SYSTEM/Service)");
            eprintln!("  - Group policy disabling input injection");
            eprintln!("  - Session 0 isolation (no interactive desktop available)");
            ExitCode::from(1)
        }
    }
}

fn run_probe() -> Result<()> {
    println!("[probe] CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)");
    let device = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)? };

    let mid_x: i32 = 1920;
    let mid_y: i32 = 1080;
    let stroke_len = 80;
    let steps = 16;

    println!("[probe] injecting pen-down + {steps} move samples + pen-up at ({mid_x},{mid_y})");

    // Hover-arrival.
    inject(
        device,
        mid_x,
        mid_y,
        0.0,
        POINTER_FLAG_INRANGE | POINTER_FLAG_NEW | POINTER_FLAG_UPDATE,
    )?;
    sleep(Duration::from_millis(8));

    // Pen-down on the first step, then update on subsequent.
    for i in 0..=steps {
        let x = mid_x + (i * stroke_len / steps);
        let pressure = 0.5 + 0.4 * ((i as f32) / (steps as f32));
        let flags = if i == 0 {
            POINTER_FLAG_INRANGE
                | POINTER_FLAG_INCONTACT
                | POINTER_FLAG_FIRSTBUTTON
                | POINTER_FLAG_DOWN
        } else {
            POINTER_FLAG_INRANGE
                | POINTER_FLAG_INCONTACT
                | POINTER_FLAG_FIRSTBUTTON
                | POINTER_FLAG_UPDATE
        };
        inject(device, x, mid_y, pressure, flags)?;
        sleep(Duration::from_millis(8));
    }

    // Pen-up.
    inject(device, mid_x + stroke_len, mid_y, 0.0, POINTER_FLAG_UP)?;

    unsafe { DestroySyntheticPointerDevice(device) };
    println!("[probe] InjectSyntheticPointerInput sequence completed without error");
    Ok(())
}

fn inject(
    device: HSYNTHETICPOINTERDEVICE,
    x: i32,
    y: i32,
    pressure: f32,
    flags: POINTER_FLAGS,
) -> Result<()> {
    let pen_info = POINTER_PEN_INFO {
        pointerInfo: POINTER_INFO {
            pointerType: PT_PEN,
            pointerId: 1,
            pointerFlags: flags,
            ptPixelLocation: POINT { x, y },
            ..Default::default()
        },
        penFlags: 0,
        penMask: PEN_MASK_PRESSURE,
        pressure: (pressure.clamp(0.0, 1.0) * 1024.0).round() as u32,
        rotation: 0,
        tiltX: 0,
        tiltY: 0,
    };

    let info = POINTER_TYPE_INFO {
        r#type: PT_PEN,
        Anonymous: POINTER_TYPE_INFO_0 { penInfo: pen_info },
    };

    unsafe { InjectSyntheticPointerInput(device, &[info]) }
}
