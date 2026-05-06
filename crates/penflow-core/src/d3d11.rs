//! D3D11 device + DXGI factory bootstrap.
//!
//! All capture / color-conversion / encoder resources MUST be created from a
//! single `D3d11Context`'s device, on the same DXGI adapter that owns the
//! selected output. Cross-adapter capture is not supported in v1.0
//! (design.md §6.1).

use windows::core::{s, w, Interface};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter1, IDXGIDevice, IDXGIDevice1, IDXGIFactory6,
    DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::error::{EngineError, EngineResult};

/// Create a fresh `IDXGIFactory6`. The `6` interface gives us
/// `EnumAdapterByGpuPreference` for the high-performance pick.
pub fn create_dxgi_factory() -> EngineResult<IDXGIFactory6> {
    let factory: IDXGIFactory6 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))? };
    Ok(factory)
}

// SAFETY: D3D11 device with SetMultithreadProtected(true) is callable from any
// thread as long as access is serialised; we move the context to the pipeline
// thread and never share &D3d11Context across threads concurrently.
unsafe impl Send for D3d11Context {}

/// Owns the D3D11 device and the DXGI adapter it was created on. Holding the
/// adapter alongside the device lets the rest of the engine verify "this
/// output belongs to my device's adapter" by LUID equality.
pub struct D3d11Context {
    pub adapter: IDXGIAdapter1,
    pub adapter_luid: i64,
    pub adapter_vendor_id: u32,
    pub adapter_name: String,
    pub device: ID3D11Device,
    pub immediate_context: ID3D11DeviceContext,
    pub feature_level: D3D_FEATURE_LEVEL,
}

impl Clone for D3d11Context {
    fn clone(&self) -> Self {
        Self {
            adapter: self.adapter.clone(),
            adapter_luid: self.adapter_luid,
            adapter_vendor_id: self.adapter_vendor_id,
            adapter_name: self.adapter_name.clone(),
            device: self.device.clone(),
            immediate_context: self.immediate_context.clone(),
            feature_level: self.feature_level,
        }
    }
}

impl D3d11Context {
    /// Build a context bound to the given adapter. This is the engine's
    /// primary constructor — the adapter must be the one that owns the
    /// output you intend to capture.
    pub fn create_on_adapter(adapter: IDXGIAdapter1) -> EngineResult<Self> {
        let desc = unsafe { adapter.GetDesc1()? };
        let adapter_name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        let adapter_luid =
            ((desc.AdapterLuid.HighPart as i64) << 32) | (desc.AdapterLuid.LowPart as i64);

        let mut device: Option<ID3D11Device> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        let mut immediate_context: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut immediate_context),
            )?;
        }
        let device = device.ok_or(EngineError::NotInitialized)?;
        let immediate_context = immediate_context.ok_or(EngineError::NotInitialized)?;

        // SetMultithreadProtected is required when MF or any other component
        // shares this device across threads (gate-1 finding: MF's MFT will
        // call into the device from its own worker threads).
        let mt: ID3D11Multithread = device.cast()?;
        let _ = unsafe { mt.SetMultithreadProtected(true) };

        // Sunshine display_base.cpp: cap swap chain queue at 1 frame so we
        // don't eat queueing latency we can't observe. Best-effort — some
        // adapters reject the call (logged-then-ignored).
        if let Ok(dxgi_dev1) = device.cast::<IDXGIDevice1>() {
            let _ = unsafe { dxgi_dev1.SetMaximumFrameLatency(1) };
        }

        // Sunshine display_base.cpp: bump GPU thread priority for the capture
        // device. Range is -7..7; 7 is the highest userland-allowed priority.
        // Best-effort.
        if let Ok(dxgi_dev) = device.cast::<IDXGIDevice>() {
            let _ = unsafe { dxgi_dev.SetGPUThreadPriority(7) };
        }

        // Process-wide GPU scheduling priority. Sunshine sets REALTIME by
        // default and downgrades to HIGH on NVIDIA + HAGS to dodge a
        // documented driver freeze (NVIDIA driver bug: REALTIME + HAGS +
        // VRAM-near-full hangs the encoder). We default to HIGH for all
        // vendors — REALTIME's upside vs HIGH is small (a few percent of
        // p99 jitter under contention) and not worth carrying HAGS
        // detection plus a fallback path.
        set_d3dkmt_process_priority(D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH);

        Ok(Self {
            adapter,
            adapter_luid,
            adapter_vendor_id: desc.VendorId,
            adapter_name,
            device,
            immediate_context,
            feature_level,
        })
    }

    /// Convenience: build a context on the system's high-performance adapter.
    /// Useful for setup paths that don't depend on a specific output (the MF
    /// IDR probe, encoder feature detection). For capture, prefer
    /// `create_on_adapter` with the adapter that owns the chosen output.
    pub fn create_high_perf() -> EngineResult<Self> {
        let factory = create_dxgi_factory()?;
        let adapter: IDXGIAdapter1 = unsafe {
            factory
                .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
                .map_err(|_| EngineError::NoAdapter)?
        };
        Self::create_on_adapter(adapter)
    }
}

/// `D3DKMT_SCHEDULINGPRIORITYCLASS` enum values per `d3dkmthk.h`.
/// HIGH (4) is the priority Sunshine downgrades to on NVIDIA+HAGS.
const D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH: i32 = 4;

/// Best-effort wrapper around `D3DKMTSetProcessSchedulingPriorityClass` from
/// `gdi32.dll`. Resolved dynamically because the symbol isn't exposed in
/// `windows-rs`'s public surface (it's a kernel-mode thunk). Failure is
/// silently ignored — without elevated privileges or on unusual SKUs the
/// call may fail with `STATUS_ACCESS_DENIED`, and the engine still works
/// at default priority.
fn set_d3dkmt_process_priority(priority: i32) {
    type Fn = unsafe extern "system" fn(
        h_process: windows::Win32::Foundation::HANDLE,
        priority: i32,
    ) -> i32;
    unsafe {
        let Ok(module) = GetModuleHandleW(w!("gdi32.dll")) else {
            return;
        };
        let Some(proc) = GetProcAddress(module, s!("D3DKMTSetProcessSchedulingPriorityClass"))
        else {
            return;
        };
        let f: Fn = std::mem::transmute(proc);
        let _ = f(GetCurrentProcess(), priority);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity test: on a normal Windows machine with at least one GPU, the
    /// high-perf factory call returns a usable D3D11 device.
    #[test]
    #[ignore = "requires real D3D11 hardware; GitHub windows-latest VM has no GPU"]
    fn high_perf_context_creates() {
        let ctx = D3d11Context::create_high_perf().expect("D3D11 high-perf context");
        assert!(!ctx.adapter_name.is_empty(), "adapter name was empty");
        assert!(ctx.adapter_luid != 0, "adapter LUID was zero");
        assert!(ctx.adapter_vendor_id != 0, "adapter vendor was zero");
    }
}
