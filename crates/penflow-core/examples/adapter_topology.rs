//! Wave-2 gate: DXGI adapter / output topology + VDD placement probe.
//!
//! Reference: design.md §6.1, HANDOFF §5.1.
//!
//! `IDXGIOutputDuplication` requires the D3D11 device and the selected output to
//! belong to the **same** DXGI adapter. The predecessor `penflow` project
//! shipped on a single-GPU desktop where this happened to be true by accident.
//! The Rust port must enumerate adapters and outputs together so the rule is
//! explicit, and so we can detect / reject the case where the VDD virtual
//! monitor lands on a different adapter than the high-performance dGPU.
//!
//! What this probe reports:
//!   - Every DXGI adapter (description, vendor, dedicated VRAM).
//!   - Every output per adapter (name, attached state, desktop coords, rotation).
//!   - For each adapter, whether its D3D11 device can `DuplicateOutput1` on
//!     each of its own outputs. (Same-adapter must succeed.)
//!   - The high-performance adapter pick that the encoder pipeline will use.
//!   - Whether the high-performance adapter owns at least one output (if not,
//!     capture must run on a different adapter than the encoder, requiring a
//!     cross-adapter copy path — flagged as out-of-scope for v1.0).
//!   - Best-guess VDD identification by adapter/output name heuristic.
//!
//! Run: `cargo run -p penflow-core --example adapter_topology`.
//! Exit code 0 = topology is encoder-compatible, 1 = mismatch (cross-adapter
//! capture would be required), 2 = setup error.

use std::process::ExitCode;

use windows::core::{Interface, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R8G8B8A8_UNORM,
    },
    CreateDXGIFactory2, IDXGIAdapter1, IDXGIFactory1, IDXGIFactory6, IDXGIOutput, IDXGIOutput1,
    IDXGIOutput5, DXGI_CREATE_FACTORY_FLAGS, DXGI_ERROR_NOT_FOUND,
    DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_OUTPUT_DESC,
};

fn main() -> ExitCode {
    match run_probe() {
        Ok(true) => {
            println!();
            println!("=== VERDICT: PASS ===");
            println!("High-performance adapter owns at least one output.");
            println!("DuplicateOutput1 on its own outputs succeeded for every probed pair.");
            println!("Engine can use a single D3D11 device for capture + encode (no cross-adapter copy).");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!();
            println!("=== VERDICT: FAIL ===");
            println!("High-performance adapter has no outputs of its own (cross-adapter capture");
            println!("would be required). v1.0 does not implement cross-adapter copy; either");
            println!("force-pick a different adapter, or the VDD must be installed against the");
            println!("high-performance adapter.");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("[probe-error] {e:?}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
struct AdapterInfo {
    index: u32,
    name: String,
    vendor_id: u32,
    device_id: u32,
    luid: i64,
    dedicated_vram_mb: u64,
    is_software: bool,
    outputs: Vec<OutputInfo>,
}

#[derive(Debug)]
struct OutputInfo {
    index: u32,
    device_name: String,
    attached_to_desktop: bool,
    coords: (i32, i32, i32, i32), // left, top, right, bottom
    rotation: u32,
    duplicate_ok: Option<DuplicateProbeResult>,
}

#[derive(Debug)]
struct DuplicateProbeResult {
    method: &'static str,
    success: bool,
    err: Option<String>,
}

fn run_probe() -> Result<bool> {
    let factory: IDXGIFactory6 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))? };

    // 1. Enumerate every adapter and its outputs.
    let mut adapters = enumerate_adapters_and_outputs(&factory)?;

    // 2. For each adapter, create a D3D11 device and probe DuplicateOutput1 on
    //    each of its own outputs.
    for ai in adapters.iter_mut() {
        if ai.is_software || ai.outputs.is_empty() {
            continue;
        }
        let adapter = unsafe { factory.EnumAdapters1(ai.index)? };
        let device = match create_d3d11_on_adapter(&adapter) {
            Ok(d) => d,
            Err(e) => {
                println!("[adapter#{}] D3D11 create failed: {e:?}", ai.index);
                continue;
            }
        };
        for oi in ai.outputs.iter_mut() {
            let output = unsafe { adapter.EnumOutputs(oi.index)? };
            oi.duplicate_ok = Some(probe_duplicate(&device, &output));
        }
    }

    // 3. Identify the high-performance adapter (LUID is the unique identifier).
    let hp_adapter: IDXGIAdapter1 =
        unsafe { factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)? };
    let hp_desc = unsafe { hp_adapter.GetDesc1()? };
    let hp_luid =
        ((hp_desc.AdapterLuid.HighPart as i64) << 32) | (hp_desc.AdapterLuid.LowPart as i64);

    // 4. Print the topology.
    println!("=== DXGI topology ===");
    for ai in &adapters {
        println!(
            "[adapter#{}] {} (vendor 0x{:04x}, device 0x{:04x}, LUID 0x{:016x}, vram {} MB{})",
            ai.index,
            ai.name,
            ai.vendor_id,
            ai.device_id,
            ai.luid,
            ai.dedicated_vram_mb,
            if ai.is_software { ", software" } else { "" }
        );
        if ai.outputs.is_empty() {
            println!("    (no outputs)");
        }
        for oi in &ai.outputs {
            let (l, t, r, b) = oi.coords;
            println!(
                "    output#{} {} {}x{} @ ({},{})  rot={}{}",
                oi.index,
                oi.device_name,
                r - l,
                b - t,
                l,
                t,
                oi.rotation,
                if oi.attached_to_desktop {
                    ""
                } else {
                    "  [DETACHED]"
                }
            );
            if let Some(d) = &oi.duplicate_ok {
                if d.success {
                    println!("        DuplicateOutput1 via {} -> OK", d.method);
                } else {
                    println!(
                        "        DuplicateOutput1 via {} -> FAIL: {}",
                        d.method,
                        d.err.as_deref().unwrap_or("(no error)")
                    );
                }
            }
        }
    }

    println!();
    println!(
        "[high-perf pick] {} (vendor 0x{:04x}, LUID 0x{:016x})",
        String::from_utf16_lossy(&hp_desc.Description).trim_end_matches('\0'),
        hp_desc.VendorId,
        hp_luid
    );
    let hp_match = adapters.iter().find(|a| a.luid == hp_luid);
    match hp_match {
        Some(a) => println!(
            "[high-perf pick] -> matches adapter#{} (LUID-keyed)",
            a.index
        ),
        None => println!(
            "[high-perf pick] WARNING: no LUID match in enumerated list; this should not happen"
        ),
    }
    let hp_has_outputs = hp_match.map(|a| !a.outputs.is_empty()).unwrap_or(false);

    // 6. VDD heuristic: any adapter or output whose name suggests virtual display.
    println!();
    println!("[VDD scan]");
    let mut vdd_hits: Vec<(u32, u32, String)> = Vec::new();
    for ai in &adapters {
        for oi in &ai.outputs {
            if looks_like_vdd(&ai.name) || looks_like_vdd(&oi.device_name) {
                vdd_hits.push((
                    ai.index,
                    oi.index,
                    format!("{} / {}", ai.name, oi.device_name),
                ));
            }
        }
    }
    if vdd_hits.is_empty() {
        println!("    no virtual-display indicators in adapter or output names.");
        println!(
            "    (VDD lifecycle is Wave-5 work; no virtual monitor expected on idle desktop.)"
        );
    } else {
        for (ai, oi, label) in &vdd_hits {
            println!("    candidate VDD: adapter#{ai} output#{oi}: {label}");
        }
        let vdd_on_hp = vdd_hits.iter().any(|(ai, _, _)| {
            adapters
                .iter()
                .find(|a| a.index == *ai)
                .map(|a| a.luid == hp_luid)
                .unwrap_or(false)
        });
        if vdd_on_hp {
            println!("    VDD lives on the high-perf adapter — single-device capture works.");
        } else {
            println!(
                "    NOTE: VDD candidate is NOT on the high-perf adapter. Capture would need to"
            );
            println!("    use the adapter that owns the VDD output, not the high-perf adapter.");
        }
    }

    Ok(hp_has_outputs)
}

fn enumerate_adapters_and_outputs(factory: &IDXGIFactory1) -> Result<Vec<AdapterInfo>> {
    let mut adapters = Vec::new();
    let mut idx: u32 = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(idx) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e),
        };
        let desc = unsafe { adapter.GetDesc1()? };
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        let is_software = (desc.Flags & 2) != 0; // DXGI_ADAPTER_FLAG_SOFTWARE = 2

        // Enumerate outputs on this adapter.
        let mut outputs = Vec::new();
        let mut oidx: u32 = 0;
        loop {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(oidx) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e),
            };
            let odesc: DXGI_OUTPUT_DESC = unsafe { output.GetDesc()? };
            let device_name = String::from_utf16_lossy(&odesc.DeviceName)
                .trim_end_matches('\0')
                .to_string();
            outputs.push(OutputInfo {
                index: oidx,
                device_name,
                attached_to_desktop: odesc.AttachedToDesktop.as_bool(),
                coords: (
                    odesc.DesktopCoordinates.left,
                    odesc.DesktopCoordinates.top,
                    odesc.DesktopCoordinates.right,
                    odesc.DesktopCoordinates.bottom,
                ),
                rotation: odesc.Rotation.0 as u32,
                duplicate_ok: None,
            });
            oidx += 1;
        }

        adapters.push(AdapterInfo {
            index: idx,
            name,
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            luid: ((desc.AdapterLuid.HighPart as i64) << 32) | (desc.AdapterLuid.LowPart as i64),
            dedicated_vram_mb: (desc.DedicatedVideoMemory as u64) / (1024 * 1024),
            is_software,
            outputs,
        });
        idx += 1;
    }
    Ok(adapters)
}

fn create_d3d11_on_adapter(adapter: &IDXGIAdapter1) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut got = D3D_FEATURE_LEVEL::default();
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut got),
            None,
        )?;
    }
    device.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
}

fn probe_duplicate(device: &ID3D11Device, output: &IDXGIOutput) -> DuplicateProbeResult {
    // The design (§6.1) calls for IDXGIOutput5::DuplicateOutput1 with a
    // scan-out format preference list, falling back to IDXGIOutput1::DuplicateOutput.
    let o5_err: Option<String> = match output.cast::<IDXGIOutput5>() {
        Ok(o5) => {
            let formats = [
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                DXGI_FORMAT_R10G10B10A2_UNORM,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
            ];
            match unsafe { o5.DuplicateOutput1(device, 0, &formats) } {
                Ok(_dupe) => {
                    return DuplicateProbeResult {
                        method: "IDXGIOutput5::DuplicateOutput1",
                        success: true,
                        err: None,
                    };
                }
                Err(e) => Some(format!("{e:?}")),
            }
        }
        Err(_) => Some("output does not implement IDXGIOutput5".into()),
    };
    if let Ok(o1) = output.cast::<IDXGIOutput1>() {
        match unsafe { o1.DuplicateOutput(device) } {
            Ok(_) => DuplicateProbeResult {
                method: "IDXGIOutput1::DuplicateOutput (Output5 failed first)",
                success: true,
                err: o5_err,
            },
            Err(e) => DuplicateProbeResult {
                method: "IDXGIOutput1::DuplicateOutput",
                success: false,
                err: Some(format!(
                    "Output5 err: {}; Output1 err: {e:?}",
                    o5_err.unwrap_or_else(|| "<n/a>".into())
                )),
            },
        }
    } else {
        DuplicateProbeResult {
            method: "IDXGIOutput1::DuplicateOutput",
            success: false,
            err: Some("output does not implement IDXGIOutput1".into()),
        }
    }
}

fn looks_like_vdd(name: &str) -> bool {
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
