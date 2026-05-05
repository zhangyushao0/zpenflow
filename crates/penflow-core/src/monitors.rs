//! DXGI adapter + output enumeration.
//!
//! `IDXGIOutputDuplication` requires the D3D11 device and the captured output
//! to live on the same DXGI adapter. The engine therefore enumerates adapters
//! AND outputs together (design.md §6.1, HANDOFF §4.4b) and surfaces both in
//! one flat list of `MonitorInfo`. The GUI shows that list to the user; the
//! engine re-opens the adapter + output when the user picks one.

use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter1, IDXGIFactory6, IDXGIOutput, DXGI_ERROR_NOT_FOUND,
};

use crate::error::{EngineError, EngineResult};

/// One physical display output, with the adapter that owns it.
///
/// The COM objects (`IDXGIAdapter1`, `IDXGIOutput`) are NOT held here so this
/// struct is `Send + Clone` and can be passed to the GUI thread freely. The
/// engine re-opens them via `open_adapter` / `open_output` when it wants to
/// actually create a D3D11 device or DDA duplicator.
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    /// Adapter index from `IDXGIFactory1::EnumAdapters1` (stable across calls
    /// in the same process, but new factories may renumber).
    pub adapter_index: u32,
    pub adapter_luid: i64,
    pub adapter_name: String,
    pub adapter_vendor_id: u32,
    pub adapter_device_id: u32,
    /// True for `Microsoft Basic Render Driver` / WARP / similar — these
    /// typically have no outputs and shouldn't be picked for capture.
    pub adapter_is_software: bool,

    /// Output index within the adapter (`IDXGIAdapter::EnumOutputs`).
    pub output_index_within_adapter: u32,
    /// `\\.\DISPLAY1` / etc. Stable identifier for GUI persistence.
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    /// `(left, top, right, bottom)` in the virtual desktop. NOTE: these
    /// values reflect the process's DPI-awareness state — call
    /// `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` at engine init
    /// (design.md §6.6) so they're physical pixels, not DIPs.
    pub desktop_coords: (i32, i32, i32, i32),
    /// `DXGI_MODE_ROTATION` value. 1 = identity (no rotation).
    pub rotation: u32,
    pub attached_to_desktop: bool,

    /// Heuristic: name matches known virtual-display-driver patterns.
    /// True does NOT guarantee it's the Penflow VDD; false does NOT guarantee
    /// it's a real panel. Use for diagnostics, not access control.
    pub looks_virtual: bool,
}

impl MonitorInfo {
    /// Re-open this monitor's adapter from a fresh factory. Use the same
    /// factory you'll use for everything else in the engine session — DXGI
    /// objects from different factories don't compose.
    pub fn open_adapter(&self, factory: &IDXGIFactory6) -> EngineResult<IDXGIAdapter1> {
        let adapter = unsafe { factory.EnumAdapters1(self.adapter_index)? };
        // Confirm we got the same physical adapter (LUIDs survive even if
        // adapter indices renumber).
        let desc = unsafe { adapter.GetDesc1()? };
        let luid = ((desc.AdapterLuid.HighPart as i64) << 32) | (desc.AdapterLuid.LowPart as i64);
        if luid != self.adapter_luid {
            return Err(EngineError::AdapterMismatch {
                output_luid: self.adapter_luid,
                device_luid: luid,
            });
        }
        Ok(adapter)
    }

    /// Re-open this monitor's IDXGIOutput from its (already-opened) adapter.
    pub fn open_output(&self, adapter: &IDXGIAdapter1) -> EngineResult<IDXGIOutput> {
        let output = unsafe { adapter.EnumOutputs(self.output_index_within_adapter)? };
        Ok(output)
    }
}

/// Enumerate every output on every adapter.
pub fn enumerate(factory: &IDXGIFactory6) -> EngineResult<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    let mut ai: u32 = 0;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        let desc = unsafe { adapter.GetDesc1()? };
        let adapter_name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        let adapter_luid =
            ((desc.AdapterLuid.HighPart as i64) << 32) | (desc.AdapterLuid.LowPart as i64);
        // DXGI_ADAPTER_FLAG_SOFTWARE = 2.
        let adapter_is_software = (desc.Flags & 2) != 0;

        let mut oi: u32 = 0;
        loop {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let odesc = unsafe { output.GetDesc()? };
            let device_name = String::from_utf16_lossy(&odesc.DeviceName)
                .trim_end_matches('\0')
                .to_string();
            let coords = (
                odesc.DesktopCoordinates.left,
                odesc.DesktopCoordinates.top,
                odesc.DesktopCoordinates.right,
                odesc.DesktopCoordinates.bottom,
            );
            out.push(MonitorInfo {
                adapter_index: ai,
                adapter_luid,
                adapter_name: adapter_name.clone(),
                adapter_vendor_id: desc.VendorId,
                adapter_device_id: desc.DeviceId,
                adapter_is_software,
                output_index_within_adapter: oi,
                device_name: device_name.clone(),
                width: (coords.2 - coords.0).max(0) as u32,
                height: (coords.3 - coords.1).max(0) as u32,
                desktop_coords: coords,
                rotation: odesc.Rotation.0 as u32,
                attached_to_desktop: odesc.AttachedToDesktop.as_bool(),
                looks_virtual: looks_virtual(&adapter_name) || looks_virtual(&device_name),
            });
            oi += 1;
        }
        ai += 1;
    }
    Ok(out)
}

/// Heuristic name match for virtual-display-driver products. Used only for
/// the diagnostic `MonitorInfo::looks_virtual` flag.
fn looks_virtual(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "virtual display",
        "virtual monitor",
        "iddcx",
        "iddsample",
        "iddsampledriver",
        "vdd",
        "mttvdd",
        "amyuni",
        "spacedesk",
        "superdisplay",
    ]
    .iter()
    .any(|needle| n.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::create_dxgi_factory;

    #[test]
    fn enumerate_returns_at_least_one_attached_output() {
        let factory = create_dxgi_factory().expect("factory");
        let mons = enumerate(&factory).expect("enumerate");
        let attached = mons.iter().filter(|m| m.attached_to_desktop).count();
        assert!(
            attached >= 1,
            "expected at least one attached output, found {} of {}",
            attached,
            mons.len()
        );
    }

    #[test]
    fn open_adapter_round_trips() {
        let factory = create_dxgi_factory().expect("factory");
        let mons = enumerate(&factory).expect("enumerate");
        for m in &mons {
            let adapter = m.open_adapter(&factory).expect("open adapter");
            let output = m.open_output(&adapter).expect("open output");
            // Output must report the same desktop coords we recorded.
            let odesc = unsafe { output.GetDesc().expect("output desc") };
            assert_eq!(
                odesc.DesktopCoordinates.left, m.desktop_coords.0,
                "desktop coords drifted between enumerate and open_output for {}",
                m.device_name
            );
        }
    }
}
