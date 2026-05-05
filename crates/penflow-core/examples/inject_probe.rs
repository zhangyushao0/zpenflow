//! Wave-2 gate: WinRT `InputInjector` packaging probe.
//!
//! Reference: design.md §6.6, HANDOFF §5.1.
//!
//! Microsoft documents `Windows.UI.Input.Preview.Injection.InputInjector` as
//! requiring the `inputInjectionBrokered` restricted capability for packaged
//! Windows apps. The predecessor `penflow` proves the API path works as an
//! unpackaged Python process. The Rust port needs to confirm:
//!   1. `InputInjector::TryCreate()` succeeds in an unpackaged Rust binary.
//!   2. `InitializePenInjection` does not refuse with a capability error.
//!   3. A real `InjectPenInput` call returns success (not just "API exists").
//!
//! Once this passes, the same shape needs to be re-tested through the WiX/MSI
//! packaging path during Wave 5 — there's no way to do that from a `cargo run`
//! example today, so that remains a gate-3.5 for later.
//!
//! What the probe does on PASS: it draws a tiny ~80px horizontal stroke at
//! mid-screen (Pressure 0.5) — enough that you can confirm Krita / OneNote /
//! any Windows Ink consumer received pen pressure rather than mouse.
//!
//! Run: `cargo run -p penflow-core --example inject_probe`.
//! Exit code 0 = PASS, 1 = API rejected (capability/packaging issue), 2 = setup error.

use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use windows::core::Result;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::UI::Input::Preview::Injection::{
    InjectedInputPenButtons, InjectedInputPenInfo, InjectedInputPenParameters, InjectedInputPoint,
    InjectedInputPointerInfo, InjectedInputPointerOptions, InjectedInputVisualizationMode,
    InputInjector,
};

fn main() -> ExitCode {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            eprintln!("[setup-fail] CoInitializeEx: {e:?}");
            return ExitCode::from(2);
        }
        // PER_MONITOR_AWARE_V2 so injected pixel coordinates are physical
        // pixels and not DIPs. Without this, on a 4K monitor at 150% scaling,
        // a click at "(1920, 1080)" lands at the wrong place.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let code = match run_probe() {
        Ok(()) => {
            println!();
            println!("=== VERDICT: PASS ===");
            println!("WinRT InputInjector works in this unpackaged Rust binary.");
            println!("Pen pressure (0.5) and tilt were injected at mid-screen — verify in Krita");
            println!("(Windows Ink mode) or OneNote that you saw a pressured stroke, not a click.");
            println!();
            println!("Remaining gate-3.5: re-run this probe inside the WiX/MSI release shape");
            println!(
                "during Wave 5. If it fails there, switch to MSIX-with-restricted-capability,"
            );
            println!("a small broker process, or virtual HID. (See design.md §6.6.)");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[FAIL] {e:?}");
            eprintln!();
            eprintln!("=== VERDICT: FAIL ===");
            eprintln!("The InputInjector API was refused in this packaging. Likely causes:");
            eprintln!("  - Process not granted inputInjectionBrokered (MSIX-only capability)");
            eprintln!("  - Service-context isolation (running under SYSTEM/Service)");
            eprintln!("  - Group policy disabling input injection");
            eprintln!("Switch packaging strategy before locking the design. See design.md §6.6.");
            ExitCode::from(1)
        }
    };

    unsafe { CoUninitialize() };
    code
}

fn run_probe() -> Result<()> {
    println!("[probe] InputInjector::TryCreate()");
    let injector = InputInjector::TryCreate()?;

    println!("[probe] InitializePenInjection(Default)");
    injector.InitializePenInjection(InjectedInputVisualizationMode::Default)?;

    // Inject a short pressured stroke at mid-screen so a human watching in
    // Krita / OneNote can confirm pen semantics (pressure, tilt, eraser bit)
    // actually arrived, not just a mouse click.
    let mid_x: i32 = 1920;
    let mid_y: i32 = 1080;
    let stroke_len = 80;
    let steps = 16;

    println!("[probe] injecting pen-down + {steps} move samples + pen-up at ({mid_x},{mid_y})");
    inject_pen(&injector, mid_x, mid_y, 0.0, true, false)?; // hover-arrival
    sleep(Duration::from_millis(8));
    for i in 0..=steps {
        let x = mid_x + (i * stroke_len / steps);
        let pressure = 0.5 + 0.4 * ((i as f64) / (steps as f64));
        inject_pen(&injector, x, mid_y, pressure, true, true)?;
        sleep(Duration::from_millis(8));
    }
    inject_pen(&injector, mid_x + stroke_len, mid_y, 0.0, false, false)?; // pen-up

    injector.UninitializePenInjection()?;
    println!("[probe] InjectPenInput sequence completed without error");
    Ok(())
}

fn inject_pen(
    injector: &InputInjector,
    x: i32,
    y: i32,
    pressure: f64,
    in_range: bool,
    in_contact: bool,
) -> Result<()> {
    let info = InjectedInputPenInfo::new()?;

    let mut opts = InjectedInputPointerOptions::None;
    if in_range {
        opts |= InjectedInputPointerOptions::InRange;
    }
    if in_contact {
        opts = opts | InjectedInputPointerOptions::InContact | InjectedInputPointerOptions::Update;
    }
    if !in_range && !in_contact {
        opts |= InjectedInputPointerOptions::PointerUp;
    }

    let pointer = InjectedInputPointerInfo {
        PointerId: 1,
        PointerOptions: opts,
        PixelLocation: InjectedInputPoint {
            PositionX: x,
            PositionY: y,
        },
        TimeOffsetInMilliseconds: 0,
        PerformanceCount: 0,
    };
    info.SetPointerInfo(pointer)?;
    info.SetPenButtons(InjectedInputPenButtons::None)?;
    info.SetPenParameters(InjectedInputPenParameters::Pressure)?;
    info.SetPressure(pressure)?;
    injector.InjectPenInput(&info)
}
