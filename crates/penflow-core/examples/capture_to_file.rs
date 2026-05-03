//! End-to-end engine smoke: capture the desktop, encode HEVC, write Annex-B
//! bytes to a `.h265` file. Manual gate per HANDOFF §5.2 step 12 — the file
//! plays in VLC if the whole stack works.
//!
//! Usage:
//!   cargo run -p penflow-core --example capture_to_file --release -- \
//!       [monitor_index] [duration_seconds] [output_path]
//!
//! Defaults: monitor 0 (first attached non-software output), 5 seconds,
//! `capture.h265` next to the working directory.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use penflow_core::encoder::Codec;
use penflow_core::Engine;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mon_idx: usize = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let duration_s: u64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let out_path: PathBuf = args
        .get(3)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("capture.h265"));

    let monitors = Engine::list_monitors().expect("list_monitors");
    let attached: Vec<_> = monitors
        .iter()
        .filter(|m| m.attached_to_desktop && !m.adapter_is_software)
        .collect();
    if attached.is_empty() {
        eprintln!("no attached non-software outputs found");
        std::process::exit(2);
    }
    println!("[monitors]");
    for (i, m) in attached.iter().enumerate() {
        println!(
            "  [{}] {} on {} ({}x{})",
            i, m.device_name, m.adapter_name, m.width, m.height
        );
    }
    let monitor = attached
        .get(mon_idx)
        .copied()
        .cloned()
        .unwrap_or_else(|| {
            eprintln!("monitor index {mon_idx} out of range; using 0");
            attached[0].clone()
        });
    println!(
        "[selected] {} {}x{} on {}",
        monitor.device_name, monitor.width, monitor.height, monitor.adapter_name
    );

    let engine = Engine::builder(monitor.clone())
        .codec(Codec::Hevc)
        .bitrate_bps(50_000_000)
        .fps(60)
        .start()
        .expect("engine start");

    // Force an IDR up front so the file is decodable from byte zero.
    engine.request_idr();

    let mut file = File::create(&out_path).expect("create output file");
    let queue = engine.packet_queue();

    let start = Instant::now();
    let mut total_bytes: u64 = 0;
    let mut packets: u64 = 0;
    let mut keyframes: u64 = 0;
    let deadline = start + Duration::from_secs(duration_s);
    while Instant::now() < deadline {
        if let Some(pkt) = queue.pop_timeout(Duration::from_millis(50)) {
            file.write_all(&pkt.bytes).expect("write packet");
            total_bytes += pkt.bytes.len() as u64;
            packets += 1;
            if pkt.is_keyframe {
                keyframes += 1;
            }
        }
    }
    let stats_keepalive = engine.keepalive_uses();
    engine.stop().expect("engine stop");

    let elapsed = start.elapsed().as_secs_f64();
    let mbps = (total_bytes as f64 * 8.0) / 1_000_000.0 / elapsed;
    println!();
    println!("=== capture summary ===");
    println!("  output:         {}", out_path.display());
    println!("  duration:       {:.2} s", elapsed);
    println!("  packets:        {packets}  ({keyframes} keyframes)");
    println!("  bytes:          {total_bytes}");
    println!("  effective rate: {:.2} Mbps", mbps);
    println!("  keepalive uses: {stats_keepalive}");
    println!();
    println!("Play the file in VLC to verify: vlc {}", out_path.display());
}
