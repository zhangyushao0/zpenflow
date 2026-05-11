//! Himetric-scale probe for `InjectSyntheticPointerInput` (issue #23).
//!
//! Goal: discover the **kernel-assigned `pRect`/`dRect`** for a synthetic pen
//! device created via `CreateSyntheticPointerDevice(PT_PEN, …)` so we can
//! compute the correct himetric-per-pixel scale to feed into
//! `POINTER_INFO.ptHimetricLocation`.
//!
//! Background: Qt's `QWindowsPointerHandler::translatePenEvent` reads
//! sub-pixel pen position from `ptHimetricLocation` via the formula
//!
//! ```text
//!   hiResX = dRect.left + (himetric_x - pRect.left)
//!                       / (pRect.right - pRect.left)
//!                       * (dRect.right - dRect.left)
//! ```
//!
//! where `pRect`/`dRect` are obtained by **the receiving app** via
//! `GetPointerDeviceRects(devHandle, &pRect, &dRect)`. The handle that goes
//! into `GetPointerDeviceRects` comes from `GetPointerDevices` enumeration;
//! `HSYNTHETICPOINTERDEVICE` itself is opaque and not accepted by
//! `GetPointerDeviceRects` directly.
//!
//! So the probe enumerates real-side `GetPointerDevices` BEFORE and AFTER
//! creating our synthetic device. The diff (if any) is the kernel's
//! externally-visible handle for our synthetic device, whose rects we can
//! then query — that gives us the EXACT scale Qt and any other WinInk-aware
//! reader will use.
//!
//! Run: `cargo run -p penflow-core --example himetric_probe`.

use std::process::ExitCode;

use windows::core::Result;
use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, POINTER_DEVICE_INFO,
    POINTER_FEEDBACK_DEFAULT,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::Pointer::{GetPointerDeviceRects, GetPointerDevices};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PT_PEN, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

fn main() -> ExitCode {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let (vx, vy) = (
        unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
    );
    let (vw, vh) = (
        unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
        unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
    );
    println!(
        "Virtual screen: origin=({vx}, {vy}) size={vw}x{vh}px (bottom-right=({},{}))",
        vx + vw,
        vy + vh
    );
    println!();

    println!("=== BEFORE creating synthetic pen device ===");
    let before = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("GetPointerDevices failed: {e:?}");
            return ExitCode::from(2);
        }
    };
    print_devices(&before);

    println!();
    println!("=== Creating synthetic pen device ===");
    let synth = match unsafe {
        CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)
    } {
        Ok(h) => h,
        Err(e) => {
            eprintln!("CreateSyntheticPointerDevice failed: {e:?}");
            return ExitCode::from(2);
        }
    };
    println!("HSYNTHETICPOINTERDEVICE handle: {synth:?}");

    println!();
    println!("=== AFTER creating synthetic pen device ===");
    let after = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("GetPointerDevices failed: {e:?}");
            unsafe { DestroySyntheticPointerDevice(synth) };
            return ExitCode::from(2);
        }
    };
    print_devices(&after);

    let new_devices: Vec<&POINTER_DEVICE_INFO> = after
        .iter()
        .filter(|a| !before.iter().any(|b| b.device == a.device))
        .collect();

    println!();
    println!("=== DIFF ===");
    if new_devices.is_empty() {
        println!("No new pointer device appeared in GetPointerDevices after");
        println!("CreateSyntheticPointerDevice. Synthetic devices are NOT");
        println!("enumerable via this API on this Windows build.");
        println!();
        println!("Implication: we cannot directly query our synthetic device's");
        println!("pRect/dRect. Receiving apps that read POINTER_INFO will use");
        println!("the rects of WHATEVER pointer device the kernel attaches to");
        println!("the synthetic event — typically the primary integrated digitizer");
        println!("or a kernel-synthesized default. Recommended next step:");
        println!("inspect each real pointer device's rects and try those as scale");
        println!("candidates.");
    } else {
        for d in &new_devices {
            println!("New device handle: {:?}", d.device);
            try_get_rects(d.device, vw, vh);
        }
    }

    println!();
    println!("=== ALL DEVICES — rects + himetric/pixel ratio ===");
    for (i, d) in after.iter().enumerate() {
        println!("[{i}] {:?}", d.device);
        try_get_rects(d.device, vw, vh);
    }

    unsafe { DestroySyntheticPointerDevice(synth) };
    ExitCode::SUCCESS
}

fn enumerate_devices() -> Result<Vec<POINTER_DEVICE_INFO>> {
    let mut count: u32 = 0;
    unsafe { GetPointerDevices(&mut count, None)? };
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![POINTER_DEVICE_INFO::default(); count as usize];
    unsafe { GetPointerDevices(&mut count, Some(buf.as_mut_ptr()))? };
    buf.truncate(count as usize);
    Ok(buf)
}

fn print_devices(devices: &[POINTER_DEVICE_INFO]) {
    if devices.is_empty() {
        println!("(no pointer devices enumerated)");
        return;
    }
    for (i, d) in devices.iter().enumerate() {
        let product = wide_to_string(&d.productString);
        println!(
            "[{i}] handle={:?} type={:?} cursorId={} product={:?}",
            d.device, d.pointerDeviceType, d.startingCursorId, product
        );
    }
}

fn try_get_rects(device: HANDLE, vw: i32, vh: i32) {
    let mut p_rect = RECT::default();
    let mut d_rect = RECT::default();
    match unsafe { GetPointerDeviceRects(device, &mut p_rect, &mut d_rect) } {
        Ok(()) => {
            let p_w = p_rect.right - p_rect.left;
            let p_h = p_rect.bottom - p_rect.top;
            let d_w = d_rect.right - d_rect.left;
            let d_h = d_rect.bottom - d_rect.top;
            println!(
                "    pRect (himetric): left={} top={} right={} bottom={} -> {p_w}x{p_h}",
                p_rect.left, p_rect.top, p_rect.right, p_rect.bottom
            );
            println!(
                "    dRect (pixel):    left={} top={} right={} bottom={} -> {d_w}x{d_h}",
                d_rect.left, d_rect.top, d_rect.right, d_rect.bottom
            );
            if d_w > 0 && d_h > 0 {
                println!(
                    "    himetric/pixel ratio: x={:.4}  y={:.4}",
                    p_w as f64 / d_w as f64,
                    p_h as f64 / d_h as f64
                );
            }
            // Hint: if dRect != virtual-screen size, this device targets a
            // specific monitor or sub-rectangle; useful context but not
            // necessarily our injection target.
            if d_w != vw || d_h != vh {
                println!(
                    "    NOTE: dRect ({d_w}x{d_h}) != virtual screen ({vw}x{vh}); device is monitor-specific"
                );
            }
        }
        Err(e) => println!("    GetPointerDeviceRects failed: {e:?}"),
    }
}

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
