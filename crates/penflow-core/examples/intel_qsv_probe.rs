//! Issue #37 repro: force the Intel adapter and run the PRODUCTION encoder
//! path (`MfBackend` → `MfSession`) at the configs the GUI actually uses.
//!
//! Background: README marks Intel Quick Sync (QSV) as "code path exists, not
//! validated on real hardware". Issue #37 is the first Intel-only user; the
//! virtual display flaps and never connects. The hypothesis is that the
//! engine fails to bring up the HEVC encode path on Intel right after the VDD
//! comes up, so the session errors and the service loop re-enables the VDD in
//! a tight loop (the "频繁切换显示器状态" symptom).
//!
//! This probe isolates JUST the encoder half: pick the Intel adapter by
//! vendor id 0x8086 (override with arg, e.g. `0x10DE` for NVIDIA to sanity-
//! check against the known-good path), build the real `MfSession` for a
//! matrix of (codec, resolution, fps), feed ~30 frames through the production
//! BGRA→NV12 converter, and report whether each combo produces a keyframe or
//! the exact HRESULT it dies on.
//!
//! Run: `cargo run -p penflow-core --example intel_qsv_probe [vendor_hex]`

use std::time::Instant;

use penflow_core::color::{create_bgra_keepalive_texture, ColorConverter};
use penflow_core::d3d11::{create_dxgi_factory, D3d11Context};
use penflow_core::encoder::{mf::MfBackend, Codec, EncoderBackend, PixelFormat, SessionConfig};

use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, DXGI_ERROR_NOT_FOUND};

struct Combo {
    label: &'static str,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
}

fn main() {
    let target_vendor = std::env::args()
        .nth(1)
        .and_then(|s| {
            let t = s.trim_start_matches("0x").trim_start_matches("0X");
            u32::from_str_radix(t, 16).ok()
        })
        .unwrap_or(0x8086); // Intel by default

    println!(
        "=== intel_qsv_probe: targeting adapter vendor 0x{:04X} ===",
        target_vendor
    );

    let factory = create_dxgi_factory().expect("dxgi factory");

    // Find the first adapter matching the requested vendor id.
    let mut chosen: Option<IDXGIAdapter1> = None;
    let mut idx = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(idx) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => {
                eprintln!("EnumAdapters1({idx}) failed: {e:?}");
                break;
            }
        };
        let desc = unsafe { adapter.GetDesc1().expect("GetDesc1") };
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        println!(
            "[adapter#{idx}] {name} (vendor 0x{:04X}, device 0x{:04X})",
            desc.VendorId, desc.DeviceId
        );
        if desc.VendorId == target_vendor && chosen.is_none() {
            chosen = Some(adapter);
        }
        idx += 1;
    }

    let adapter = match chosen {
        Some(a) => a,
        None => {
            eprintln!(
                "\nNo adapter with vendor 0x{:04X} found. Pass a vendor hex as arg1.",
                target_vendor
            );
            std::process::exit(2);
        }
    };

    let ctx = match D3d11Context::create_on_adapter(adapter) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("D3d11Context::create_on_adapter failed: {e:?}");
            std::process::exit(2);
        }
    };
    println!(
        "\n[d3d11] device on '{}' (vendor 0x{:04X}, LUID 0x{:016x})\n",
        ctx.adapter_name, ctx.adapter_vendor_id, ctx.adapter_luid
    );

    // The matrix: the real GUI default first, then degrade.
    let combos = [
        Combo {
            label: "HEVC 2880x1800@120 50Mbps (GUI default)",
            codec: Codec::Hevc,
            width: 2880,
            height: 1800,
            fps: 120,
            bitrate_bps: 50_000_000,
        },
        Combo {
            label: "HEVC 2880x1800@60 50Mbps",
            codec: Codec::Hevc,
            width: 2880,
            height: 1800,
            fps: 60,
            bitrate_bps: 50_000_000,
        },
        Combo {
            label: "HEVC 1920x1200@60 30Mbps",
            codec: Codec::Hevc,
            width: 1920,
            height: 1200,
            fps: 60,
            bitrate_bps: 30_000_000,
        },
        Combo {
            label: "H264 2880x1800@120 50Mbps",
            codec: Codec::H264,
            width: 2880,
            height: 1800,
            fps: 120,
            bitrate_bps: 50_000_000,
        },
        Combo {
            label: "H264 1920x1200@60 30Mbps",
            codec: Codec::H264,
            width: 1920,
            height: 1200,
            fps: 60,
            bitrate_bps: 30_000_000,
        },
    ];

    let backend = match MfBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("MfBackend::new failed: {e:?}");
            std::process::exit(2);
        }
    };

    let mut any_fail = false;
    for c in &combos {
        print!("[probe] {:<42} ... ", c.label);
        match run_one(&backend, &ctx, c) {
            Ok((packets, keyframes, first_kf_ms, avg_us)) => {
                if keyframes > 0 {
                    println!(
                        "PASS  ({packets} pkts, {keyframes} keyframes, first KF in {first_kf_ms} ms, avg encode {avg_us} us)"
                    );
                } else {
                    any_fail = true;
                    println!("FAIL  (produced {packets} packets but NO keyframe in 30 frames)");
                }
            }
            Err(e) => {
                any_fail = true;
                println!("FAIL  ({e})");
            }
        }
    }

    println!();
    if any_fail {
        println!("=== VERDICT: at least one combo FAILED on this adapter ===");
        std::process::exit(1);
    } else {
        println!("=== VERDICT: all combos produced keyframes on this adapter ===");
    }
}

/// Build the production MfSession for one combo and pump 30 frames.
/// Returns (packets, keyframes, ms_to_first_keyframe, avg_encode_us) or a
/// human-readable error string (which carries the HRESULT for MF failures).
fn run_one(
    backend: &MfBackend,
    ctx: &D3d11Context,
    c: &Combo,
) -> Result<(usize, usize, u128, u32), String> {
    let cfg = SessionConfig {
        width: c.width,
        height: c.height,
        fps: c.fps,
        bitrate_bps: c.bitrate_bps,
        codec: c.codec,
        input_format: PixelFormat::Nv12,
    };

    let mut session = backend
        .make_session(ctx, cfg)
        .map_err(|e| format!("make_session: {e:?}"))?;
    let conv = ColorConverter::new(ctx, c.width, c.height, c.fps)
        .map_err(|e| format!("ColorConverter::new: {e:?}"))?;
    let bgra = create_bgra_keepalive_texture(&ctx.device, c.width, c.height)
        .map_err(|e| format!("keepalive tex: {e:?}"))?;

    let start = Instant::now();
    let mut packets = 0usize;
    let mut keyframes = 0usize;
    let mut first_kf_ms: u128 = 0;
    let mut encode_us_sum: u64 = 0;
    let mut encode_us_n: u64 = 0;

    for i in 0..30i64 {
        conv.convert(&bgra).map_err(|e| format!("convert: {e:?}"))?;
        let force_idr = i == 2;
        session
            .submit_frame(
                conv.output_texture(),
                i * (1_000_000_000 / c.fps as i64),
                force_idr,
            )
            .map_err(|e| format!("submit_frame[{i}]: {e:?}"))?;
        while let Some(pkt) = session
            .try_packet()
            .map_err(|e| format!("try_packet[{i}]: {e:?}"))?
        {
            packets += 1;
            if let Some(us) = pkt.encode_us {
                encode_us_sum += us as u64;
                encode_us_n += 1;
            }
            if pkt.is_keyframe {
                if keyframes == 0 {
                    first_kf_ms = start.elapsed().as_millis();
                }
                keyframes += 1;
            }
        }
    }
    // Drain.
    for _ in 0..30 {
        while let Some(pkt) = session
            .try_packet()
            .map_err(|e| format!("try_packet drain: {e:?}"))?
        {
            packets += 1;
            if let Some(us) = pkt.encode_us {
                encode_us_sum += us as u64;
                encode_us_n += 1;
            }
            if pkt.is_keyframe {
                if keyframes == 0 {
                    first_kf_ms = start.elapsed().as_millis();
                }
                keyframes += 1;
            }
        }
    }

    let avg_us = if encode_us_n > 0 {
        (encode_us_sum / encode_us_n) as u32
    } else {
        0
    };
    Ok((packets, keyframes, first_kf_ms, avg_us))
}
