//! Pen injection via WinRT `Windows.UI.Input.Preview.Injection.InputInjector`.
//!
//! Production version of the gate-3 probe (`crates/penflow-core/examples/inject_probe.rs`).
//! Same API surface (TryCreate → InitializePenInjection → InjectPenInput →
//! UninitializePenInjection), but reused across many samples and with the
//! eraser flip-then-flush protocol (HANDOFF §1.5) wired in.

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::UI::Input::Preview::Injection::{
    InjectedInputPenButtons, InjectedInputPenInfo, InjectedInputPenParameters,
    InjectedInputPoint, InjectedInputPointerInfo, InjectedInputPointerOptions,
    InjectedInputVisualizationMode, InputInjector,
};

use crate::error::{EngineError, EngineResult};

use super::PenSample;

pub struct PenInjector {
    injector: InputInjector,
    /// Whether `InitializePenInjection` succeeded; gates calls to
    /// `Uninitialize` in `Drop`.
    initialized: bool,
    /// Last frame's eraser state — drives the flip-then-flush protocol.
    last_eraser: bool,
    /// Last frame's `in_range` state — needed to fabricate a one-frame
    /// `out-of-range` event when eraser toggles mid-stroke.
    last_in_range: bool,
}

// SAFETY: WinRT InputInjector is callable from any thread (it's a multi-
// threaded apartment-friendly object). The struct is owned by one thread at a
// time (the server's input-dispatch task) and never &-shared.
unsafe impl Send for PenInjector {}

impl PenInjector {
    /// Create the injector. Sets process-wide DPI awareness to PER_MONITOR_V2
    /// so injected pixel coordinates are physical pixels (gate-2 finding —
    /// without this, on a 4K monitor at 150 % scaling, a click at "(1920,
    /// 1080)" lands at the wrong place).
    pub fn new() -> EngineResult<Self> {
        // Process-wide setting; idempotent. Best-effort — running inside an
        // already-DPI-aware host (some Tauri shells set this for the whole
        // process at startup) returns an error which we ignore.
        let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

        let injector = InputInjector::TryCreate().map_err(EngineError::from)?;
        injector
            .InitializePenInjection(InjectedInputVisualizationMode::Default)
            .map_err(EngineError::from)?;

        Ok(Self {
            injector,
            initialized: true,
            last_eraser: false,
            last_in_range: false,
        })
    }

    /// Inject one pen sample. Handles the eraser flip-then-flush automatically:
    /// if `sample.eraser` differs from the previous call AND the previous
    /// sample was in-range, we emit one synthetic `out-of-range` frame first
    /// at the previous coordinates so the driver sees the `Inverted` bit
    /// transition cleanly (HANDOFF §1.5).
    ///
    /// Takes `&mut self` because we track the previous sample's eraser /
    /// in-range state for the flip-then-flush logic. The server's input
    /// dispatch task owns the injector exclusively, so this is natural.
    pub fn inject(&mut self, sample: &PenSample) -> EngineResult<()> {
        // Eraser flip-then-flush: if the eraser bit changes mid-stroke,
        // emit a synthetic out-of-range sample first so the driver sees a
        // clean `Inverted` transition.
        if sample.eraser != self.last_eraser && self.last_in_range {
            let mut flush = *sample;
            flush.in_range = false;
            flush.in_contact = false;
            flush.pressure = 0.0;
            // Use the OLD eraser state for the flush so the driver sees a
            // transition from "old state, out of range" → "new state, in
            // range" rather than a same-frame both-bits switcheroo.
            self.write_sample(&flush, self.last_eraser)?;
        }

        self.write_sample(sample, sample.eraser)?;
        self.last_eraser = sample.eraser;
        self.last_in_range = sample.in_range;
        Ok(())
    }

    fn write_sample(&self, sample: &PenSample, eraser: bool) -> EngineResult<()> {
        let info = InjectedInputPenInfo::new().map_err(EngineError::from)?;

        // Pointer flags: New / InRange / InContact / Update / PointerUp etc.
        let mut opts = InjectedInputPointerOptions::None;
        if sample.in_range {
            opts = opts | InjectedInputPointerOptions::InRange;
        }
        if sample.in_contact {
            opts = opts
                | InjectedInputPointerOptions::InContact
                | InjectedInputPointerOptions::Update;
        }
        if !sample.in_range && !sample.in_contact {
            opts = opts | InjectedInputPointerOptions::PointerUp;
        }

        let pointer = InjectedInputPointerInfo {
            PointerId: 1,
            PointerOptions: opts,
            PixelLocation: InjectedInputPoint {
                PositionX: sample.x,
                PositionY: sample.y,
            },
            TimeOffsetInMilliseconds: 0,
            PerformanceCount: 0,
        };
        info.SetPointerInfo(pointer).map_err(EngineError::from)?;

        let buttons = if eraser {
            InjectedInputPenButtons::Inverted
        } else {
            InjectedInputPenButtons::None
        };
        info.SetPenButtons(buttons).map_err(EngineError::from)?;

        let params = InjectedInputPenParameters::Pressure
            | InjectedInputPenParameters::TiltX
            | InjectedInputPenParameters::TiltY;
        info.SetPenParameters(params).map_err(EngineError::from)?;
        info.SetPressure(sample.pressure as f64)
            .map_err(EngineError::from)?;
        info.SetTiltX(sample.tilt_x_deg).map_err(EngineError::from)?;
        info.SetTiltY(sample.tilt_y_deg).map_err(EngineError::from)?;

        self.injector
            .InjectPenInput(&info)
            .map_err(EngineError::from)?;
        Ok(())
    }
}

impl Drop for PenInjector {
    fn drop(&mut self) {
        if self.initialized {
            let _ = self.injector.UninitializePenInjection();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: create the injector, inject a single in-range hover sample at
    /// the corner of the desktop (0, 0). This actually moves the cursor
    /// briefly — same caveat as the gate-3 probe.
    #[test]
    fn create_and_hover() {
        // Initialize COM apartment for WinRT. CoInitializeEx is reentrant /
        // refcounted so this is harmless even if the harness already called
        // it (which `#[test]` infrastructure may or may not, depending on
        // who runs it).
        let _ = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
        };

        let mut inj = match PenInjector::new() {
            Ok(i) => i,
            // On CI without an interactive desktop session, TryCreate /
            // InitializePenInjection may legitimately refuse. Treat that as
            // the gate-3.5 (MSI re-test) follow-up, not a unit-test failure.
            Err(EngineError::Win32(e)) => {
                eprintln!("[skip] InputInjector unavailable in this context: {e:?}");
                return;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        };

        let sample = PenSample {
            x: 1,
            y: 1,
            pressure: 0.0,
            tilt_x_deg: 0,
            tilt_y_deg: 0,
            in_range: true,
            in_contact: false,
            eraser: false,
            captured_at: None,
        };
        inj.inject(&sample).expect("inject hover");
    }
}
