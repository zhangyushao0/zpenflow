//! Probe the production D3D texture -> MF HEVC encoder path at an arbitrary
//! size, without DXGI Output Duplication. Useful for separating "encoder does
//! not accept this D3D/NV12 input" from capture/VDD problems.
//!
//! Run:
//!   cargo run -p penflow-core --example encoder_texture_probe -- 2880 1800

use std::env;

use penflow_core::color::{
    clear_bgra_texture_to_black, create_bgra_keepalive_texture, ColorConverter,
};
use penflow_core::d3d11::D3d11Context;
use penflow_core::encoder::{mf::MfBackend, Codec, EncoderBackend, PixelFormat, SessionConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let width = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1280);
    let height = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(720);
    let fps = 60;

    println!("[probe] size: {width}x{height}");
    let ctx = D3d11Context::create_high_perf()?;
    println!(
        "[probe] adapter: {} LUID 0x{:016x}",
        ctx.adapter_name, ctx.adapter_luid
    );

    let backend = MfBackend::new()?;
    let cfg = SessionConfig {
        width,
        height,
        fps,
        bitrate_bps: 5_000_000,
        codec: Codec::Hevc,
        input_format: PixelFormat::Nv12,
    };
    let mut session = backend.make_session(&ctx, cfg)?;
    let conv = ColorConverter::new(&ctx, width, height, fps)?;
    let bgra = create_bgra_keepalive_texture(&ctx.device, width, height)?;
    clear_bgra_texture_to_black(&ctx, &bgra)?;

    let mut packets = 0usize;
    let mut keyframes = 0usize;
    for i in 0..30 {
        conv.convert(&bgra)?;
        session.submit_frame(conv.output_texture(), i as i64 * 16_666_667, None, i == 0)?;
        while let Some(pkt) = session.try_packet()? {
            packets += 1;
            if pkt.is_keyframe {
                keyframes += 1;
            }
        }
    }

    for _ in 0..30 {
        while let Some(pkt) = session.try_packet()? {
            packets += 1;
            if pkt.is_keyframe {
                keyframes += 1;
            }
        }
    }

    println!("[probe] packets={packets} keyframes={keyframes}");
    Ok(())
}
