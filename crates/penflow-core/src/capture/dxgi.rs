//! DXGI Output Duplication wrapper (Windows-only).
//!
//! References:
//!   - design.md §6.1 ("Capture Layer")
//!   - HANDOFF.md §1.2 (Sunshine MUST-adopt tricks)
//!   - HANDOFF.md §4.4b (DPI-awareness, multi-format DDA list)
//!
//! Sunshine `display_base.cpp` figured out most of the operational pitfalls
//! ten years ago; the comments below cite the specific tricks rather than
//! re-deriving them.

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, E_INVALIDARG, S_OK, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::{
    Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R8G8B8A8_UNORM,
    },
    IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED};

use super::cursor_shape::{decode_shape, CursorShape};
use crate::d3d11::D3d11Context;
use crate::error::{EngineError, EngineResult};
use crate::monitors::MonitorInfo;

// SAFETY: All COM objects inside live on a single thread (the pipeline
// capture thread). DDA + D3D11 device with SetMultithreadProtected serialise
// access. Send is the move from the main thread that constructed the
// capturer to the pipeline thread; never &-shared across threads.
unsafe impl Send for DxgiCapturer {}

/// Holds an `IDXGIOutputDuplication` against a specific output, with
/// transparent recovery from `DXGI_ERROR_ACCESS_LOST` /
/// `DXGI_ERROR_ACCESS_DENIED`.
pub struct DxgiCapturer {
    /// Re-opened on every reinit (the duplication is bound to a specific
    /// IDXGIOutput instance; if the desktop session changes, we re-EnumOutputs).
    monitor: MonitorInfo,
    /// The D3D11 context whose device backs this duplication. The output
    /// MUST belong to the same adapter as `ctx.adapter` — checked in `new`.
    ctx: D3d11Context,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    /// True iff a frame is currently held (between `acquire` and `release`).
    /// IDXGIOutputDuplication forbids re-acquiring while one is held.
    frame_held: bool,
    /// True iff we successfully called `SetThreadExecutionState` with
    /// `ES_DISPLAY_REQUIRED` and need to clear it on drop.
    display_required: bool,
}

/// One acquired DDA frame. Drops automatically release the duplication so the
/// next `acquire_frame` call works.
pub struct AcquiredFrame<'a> {
    pub texture: ID3D11Texture2D,
    pub captured_at: Instant,
    pub frame_info: DXGI_OUTDUPL_FRAME_INFO,
    capturer: &'a mut DxgiCapturer,
}

/// Cursor screen position reported alongside one DDA frame.
///
/// `visible == false` means the OS thinks the cursor is on a different
/// monitor, or hidden — the compositor should skip the blit.
/// Coordinates are in the duplicated output's local pixel space, with
/// origin (0,0) at the top-left of THIS monitor (not the virtual screen).
#[derive(Clone, Copy, Debug)]
pub struct PointerPosition {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

impl<'a> AcquiredFrame<'a> {
    /// True iff this frame's `LastPresentTime == 0`, meaning DDA had no new
    /// content but woke us up because the cursor moved. Encoder pipelines
    /// generally treat this as "no new frame, reuse keepalive".
    pub fn is_cursor_only(&self) -> bool {
        self.frame_info.LastPresentTime == 0
    }

    /// Cursor position iff DDA reports a non-zero `LastMouseUpdateTime`
    /// (i.e. the cursor moved since the previous frame). When `None`, the
    /// caller should reuse whatever position it last cached. The `visible`
    /// flag distinguishes "cursor is on this monitor" from "cursor is
    /// elsewhere or hidden" — the compositor blits only when visible.
    pub fn pointer_position(&self) -> Option<PointerPosition> {
        if self.frame_info.LastMouseUpdateTime == 0 {
            return None;
        }
        Some(PointerPosition {
            x: self.frame_info.PointerPosition.Position.x,
            y: self.frame_info.PointerPosition.Position.y,
            visible: self.frame_info.PointerPosition.Visible.as_bool(),
        })
    }

    /// Pull the latest cursor shape from DDA, if this frame includes one.
    ///
    /// `frame_info.PointerShapeBufferSize == 0` means "no shape change
    /// this frame, reuse the one you already have." For non-zero values we
    /// allocate (or reuse) a buffer of that size, call `GetFramePointerShape`,
    /// and decode into the engine-side BGRA representation.
    ///
    /// Returns `Ok(None)` when no shape was provided this frame.
    pub fn take_shape_update(&mut self) -> EngineResult<Option<CursorShape>> {
        let needed = self.frame_info.PointerShapeBufferSize;
        if needed == 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; needed as usize];
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut required: u32 = 0;
        unsafe {
            self.capturer.duplication.GetFramePointerShape(
                needed,
                buf.as_mut_ptr() as *mut _,
                &mut required,
                &mut info,
            )?;
        }
        // The buffer may not be entirely filled — `required` is the actual
        // payload length when smaller than `needed`. Truncate so decode
        // sees only valid bytes.
        if (required as usize) < buf.len() {
            buf.truncate(required as usize);
        }
        let shape = decode_shape(
            info.Type,
            info.Width,
            info.Height,
            info.Pitch,
            info.HotSpot.x,
            info.HotSpot.y,
            &buf,
        )?;
        Ok(Some(shape))
    }
}

impl Drop for AcquiredFrame<'_> {
    fn drop(&mut self) {
        if self.capturer.frame_held {
            let _ = unsafe { self.capturer.duplication.ReleaseFrame() };
            self.capturer.frame_held = false;
        }
    }
}

impl DxgiCapturer {
    /// Create a capturer for the given monitor. The provided `D3d11Context`
    /// MUST be on the same adapter that owns the monitor (LUID-equality);
    /// otherwise `DuplicateOutput1` fails with E_INVALIDARG.
    pub fn new(ctx: D3d11Context, monitor: MonitorInfo) -> EngineResult<Self> {
        if ctx.adapter_luid != monitor.adapter_luid {
            return Err(EngineError::AdapterMismatch {
                output_luid: monitor.adapter_luid,
                device_luid: ctx.adapter_luid,
            });
        }

        let output = monitor.open_output(&ctx.adapter)?;
        let duplication = create_duplication(&output, &ctx)?;

        // Sunshine display_base.cpp:239 — without ES_DISPLAY_REQUIRED, an idle
        // desktop sleeps the monitor → AcquireNextFrame returns ACCESS_LOST →
        // reinit wakes the monitor → infinite cycle. Set per-thread for the
        // capturer's lifetime; clear in Drop. SetThreadExecutionState returns
        // 0 on failure (NOT a HRESULT).
        let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED) };
        let display_required = prev.0 != 0;

        Ok(Self {
            width: monitor.width,
            height: monitor.height,
            monitor,
            ctx,
            duplication,
            frame_held: false,
            display_required,
        })
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    pub fn d3d11(&self) -> &D3d11Context {
        &self.ctx
    }

    /// Block for up to `timeout` waiting for the next frame.
    ///
    /// Returns:
    ///   - `Ok(Some(frame))` — frame ready; drop the `AcquiredFrame` to release.
    ///   - `Ok(None)` — DDA timeout (no new content within `timeout`). Caller
    ///     typically falls back to keepalive frame.
    ///   - `Err(EngineError::Win32)` — fatal HRESULT after recovery attempt.
    pub fn acquire_frame(&mut self, timeout: Duration) -> EngineResult<Option<AcquiredFrame<'_>>> {
        if self.frame_held {
            // Defensive: previous AcquiredFrame must have been dropped. If we
            // ever see this, fix the caller — DDA refuses re-acquire otherwise.
            let _ = unsafe { self.duplication.ReleaseFrame() };
            self.frame_held = false;
        }

        let timeout_ms = clamp_timeout_ms(timeout);
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let r = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };

        match r {
            Ok(()) => {
                self.frame_held = true;
                let resource = resource.ok_or_else(|| {
                    EngineError::Win32(windows::core::Error::from_hresult(E_INVALIDARG))
                })?;
                let texture: ID3D11Texture2D = resource.cast()?;
                Ok(Some(AcquiredFrame {
                    texture,
                    captured_at: Instant::now(),
                    frame_info,
                    capturer: self,
                }))
            }
            Err(e)
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT || e.code().0 == WAIT_TIMEOUT.0 as i32 =>
            {
                Ok(None)
            }
            Err(e)
                if e.code() == DXGI_ERROR_ACCESS_LOST
                    || e.code() == DXGI_ERROR_ACCESS_DENIED
                    || e.code() == DXGI_STATUS_OCCLUDED =>
            {
                // Transparent reinit: another fullscreen app stole the
                // duplication, or the desktop session was switched. Try once;
                // if reinit fails we surface it.
                self.reinit()?;
                Ok(None)
            }
            Err(e) => Err(EngineError::Win32(e)),
        }
    }

    /// Tear down the existing duplication and create a new one against the
    /// same output. Used after `DXGI_ERROR_ACCESS_LOST` / `_ACCESS_DENIED`.
    pub fn reinit(&mut self) -> EngineResult<()> {
        if self.frame_held {
            let _ = unsafe { self.duplication.ReleaseFrame() };
            self.frame_held = false;
        }
        let output = self.monitor.open_output(&self.ctx.adapter)?;
        self.duplication = create_duplication(&output, &self.ctx)?;
        Ok(())
    }
}

impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        if self.frame_held {
            let _ = unsafe { self.duplication.ReleaseFrame() };
        }
        if self.display_required {
            let _ = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}

/// Run `IDXGIOutput5::DuplicateOutput1` with the design's 4-format scan-out
/// preference list (gate-2 finding: a single-format list silently fails on
/// some configurations and falls back to `IDXGIOutput1::DuplicateOutput`).
/// Falls back to `IDXGIOutput1::DuplicateOutput` if the Output5 path errors
/// or the interface isn't available.
fn create_duplication(
    output: &IDXGIOutput,
    ctx: &D3d11Context,
) -> EngineResult<IDXGIOutputDuplication> {
    if let Ok(o5) = output.cast::<IDXGIOutput5>() {
        let formats = [
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            DXGI_FORMAT_R10G10B10A2_UNORM,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
        ];
        match unsafe { o5.DuplicateOutput1(&ctx.device, 0, &formats) } {
            Ok(d) => return Ok(d),
            Err(e) => {
                // Some adapters (older Intel, virtual displays) reject Output5;
                // fall through to the simpler API.
                let _ = e;
            }
        }
    }
    let o1: IDXGIOutput1 = output.cast()?;
    Ok(unsafe { o1.DuplicateOutput(&ctx.device)? })
}

fn clamp_timeout_ms(d: Duration) -> u32 {
    let ms = d.as_millis();
    if ms == 0 {
        // 0 means "poll, return immediately if no frame" — the kernel APIs
        // accept 0 explicitly.
        return 0;
    }
    if ms > u32::MAX as u128 {
        u32::MAX
    } else {
        ms as u32
    }
}

// Suppress "unused" for symbols only consumed via `match arms`.
#[allow(dead_code)]
fn _ok_alias() -> windows::core::HRESULT {
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::create_dxgi_factory;
    use crate::monitors;

    /// End-to-end: open the first attached output and grab one frame. The
    /// timeout is generous (500 ms); CI runs may legitimately have a static
    /// desktop and time out, so a None result is also acceptable.
    #[test]
    #[ignore = "requires real D3D11 hardware (DXGI Desktop Duplication); GitHub windows-latest VM has no GPU"]
    fn capture_one_frame() {
        let _g = crate::test_lock::DDA_LOCK.lock().unwrap();
        let factory = create_dxgi_factory().expect("factory");
        let mons = monitors::enumerate(&factory).expect("enumerate");
        let mon = mons
            .iter()
            .find(|m| m.attached_to_desktop && !m.adapter_is_software)
            .expect("at least one attached non-software output")
            .clone();
        let adapter = mon.open_adapter(&factory).expect("open adapter");
        let ctx = D3d11Context::create_on_adapter(adapter).expect("d3d11 ctx");
        let mut cap = DxgiCapturer::new(ctx, mon).expect("capturer");
        let (w, h) = cap.output_size();
        assert!(w > 0 && h > 0, "output size was zero");

        // First call may legitimately hit ACCESS_LOST during session setup
        // (DDA can race); accept either Ok(Some), Ok(None), or one retry.
        let mut got_frame_or_timeout = false;
        for _ in 0..3 {
            match cap.acquire_frame(Duration::from_millis(500)) {
                Ok(_) => {
                    got_frame_or_timeout = true;
                    break;
                }
                Err(EngineError::Win32(_)) => continue,
                Err(e) => panic!("non-Win32 error: {e:?}"),
            }
        }
        assert!(
            got_frame_or_timeout,
            "DDA never returned a frame or timeout"
        );
    }
}
