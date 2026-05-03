//! Headless Penflow session runner.
//!
//! Lists available monitors, picks one (`--monitor <index>` or first
//! attached non-software output by default), starts the engine + ADB
//! reverse tunnel, and waits for the Android client to connect.
//!
//! Usage:
//!   adb devices    # confirm the phone/tablet is attached
//!   cargo run -p penflow-server --example run_session [-- --monitor 0 --bitrate 50000000 --fps 60]
//!
//! On the Android side, launch the Penflow app — it opens
//! `localabstract:penflow` which the reverse tunnel forwards to this process.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use penflow_core::encoder::Codec;
use penflow_core::Engine;
use penflow_server::{Session, SessionConfig, SessionEvent, VddController};
use penflow_transport::adb::AdbLocalAbstractTransport;
use penflow_transport::Transport;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();

    // Probe for a Virtual Display Driver. If it's installed (regardless of
    // current enabled/disabled state), we'll enable it on connect and
    // disable it on disconnect — see design.md §16 / HANDOFF §4.6.
    let vdd: Option<VddController> = if args.no_vdd {
        println!("[run_session] --no-vdd: skipping VDD detection");
        None
    } else {
        match VddController::detect() {
            Ok(Some(v)) => {
                println!(
                    "[run_session] VDD detected: '{}' ({})",
                    v.friendly_name(),
                    v.instance_id()
                );
                println!(
                    "[run_session]   will enable on Android connect, disable on disconnect"
                );
                Some(v)
            }
            Ok(None) => {
                println!(
                    "[run_session] no VDD installed (PowerShell ran fine, no match)"
                );
                println!(
                    "[run_session]   capturing the physical monitor — see tools/vdd/README.md"
                );
                println!(
                    "[run_session]   (Qualcomm decoders may reject 4K streams; install VDD if so)"
                );
                None
            }
            Err(e) => {
                eprintln!("[run_session] VDD detection failed: {e}");
                eprintln!("[run_session]   continuing without VDD");
                None
            }
        }
    };

    println!("[run_session] enumerating monitors...");
    let monitors = Engine::list_monitors()?;
    let attached: Vec<_> = monitors
        .iter()
        .filter(|m| m.attached_to_desktop && !m.adapter_is_software)
        .collect();
    if attached.is_empty() && vdd.is_none() {
        eprintln!("no attached non-software outputs found and no VDD available");
        std::process::exit(2);
    }
    for (i, m) in attached.iter().enumerate() {
        println!(
            "  [{i}] {} on {} ({}x{}){}",
            m.device_name,
            m.adapter_name,
            m.width,
            m.height,
            if m.looks_virtual { "  [virtual]" } else { "" }
        );
    }
    // Fallback monitor for when --no-vdd is in effect (or VDD detection
    // failed). When `vdd` is Some, the session ignores this and uses the
    // virtual monitor that pops up after enable.
    let fallback_monitor = if attached.is_empty() {
        // We have a VDD that'll bring up its own monitor; just use a stub
        // here. (The session won't read this field when vdd is Some.)
        monitors[0].clone()
    } else {
        attached
            .get(args.monitor_index)
            .copied()
            .cloned()
            .unwrap_or_else(|| {
                eprintln!(
                    "monitor index {} out of range; using 0",
                    args.monitor_index
                );
                attached[0].clone()
            })
    };
    if vdd.is_none() {
        println!(
            "[run_session] selected: {} {}x{}",
            fallback_monitor.device_name, fallback_monitor.width, fallback_monitor.height
        );
    }

    println!("[run_session] starting ADB reverse tunnel...");
    let transport: Arc<dyn Transport> = Arc::new(
        AdbLocalAbstractTransport::bind("penflow")
            .await
            .map_err(|e| format!("ADB transport bind failed: {e}. Is `adb` on PATH and a device attached?"))?,
    );

    println!("[run_session] tunnel ready. Launch the Penflow app on the device now.");

    let cfg = SessionConfig {
        monitor: fallback_monitor.clone(),
        codec: Codec::Hevc,
        bitrate_bps: args.bitrate_bps,
        fps: args.fps,
        vdd,
    };

    // Subscribe to lifecycle events so the operator sees them.
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                SessionEvent::Connecting { peer } => {
                    println!("[run_session] connecting from {peer}");
                }
                SessionEvent::Connected {
                    peer,
                    device_width,
                    device_height,
                } => {
                    println!(
                        "[run_session] connected: {peer} → {device_width}x{device_height}"
                    );
                }
                SessionEvent::Disconnected => {
                    println!("[run_session] disconnected (clean)");
                }
                SessionEvent::Errored(e) => {
                    eprintln!("[run_session] session error: {e}");
                }
            }
        }
    });

    // Ctrl-C handler. tokio::signal::ctrl_c is async; race against the
    // session.
    let session = Session::new(cfg);
    let session_run = session.run(transport.clone(), Some(tx));
    tokio::select! {
        r = session_run => match r {
            Ok(()) => println!("[run_session] session ended cleanly"),
            Err(e) => eprintln!("[run_session] session ended with error: {e}"),
        },
        _ = tokio::signal::ctrl_c() => {
            println!("[run_session] Ctrl-C received");
        }
    }

    println!("[run_session] shutting down transport...");
    let _ = tokio::time::timeout(Duration::from_secs(3), transport.shutdown()).await;
    println!("[run_session] bye");
    Ok(())
}

struct Args {
    monitor_index: usize,
    bitrate_bps: u32,
    fps: u32,
    /// `--no-vdd`: skip VDD detection and capture the physical monitor.
    /// Useful when running in a non-elevated PowerShell (Enable-PnpDevice
    /// requires admin) or when the operator deliberately wants to mirror
    /// the existing desktop.
    no_vdd: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        monitor_index: 0,
        bitrate_bps: 50_000_000,
        fps: 60,
        no_vdd: false,
    };
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--monitor" => {
                i += 1;
                if let Some(v) = argv.get(i).and_then(|s| s.parse().ok()) {
                    a.monitor_index = v;
                }
            }
            "--bitrate" => {
                i += 1;
                if let Some(v) = argv.get(i).and_then(|s| s.parse().ok()) {
                    a.bitrate_bps = v;
                }
            }
            "--fps" => {
                i += 1;
                if let Some(v) = argv.get(i).and_then(|s| s.parse().ok()) {
                    a.fps = v;
                }
            }
            "--no-vdd" => {
                a.no_vdd = true;
            }
            other => {
                eprintln!("[run_session] ignoring unknown arg: {other}");
            }
        }
        i += 1;
    }
    a
}
