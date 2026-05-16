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

use std::process::ExitCode;

use penflow_core::encoder::Codec;
use penflow_core::Engine;
use penflow_server::vdd;
use penflow_server::{Session, SessionConfig, SessionEvent, VddController};
use penflow_transport::adb::AdbLocalAbstractTransport;
use penflow_transport::Transport;

fn main() -> ExitCode {
    // VDD helper sub-mode — spawned by the unelevated parent via
    // `ShellExecuteW("runas", ...)`. We do the requested CM_Enable_DevNode /
    // CM_Disable_DevNode and exit. NO tokio, no engine, no transport.
    let argv: Vec<String> = env::args().collect();
    if argv.get(1).map(|s| s.as_str()) == Some("--vdd-helper") {
        return vdd::helper_main(&argv[1..]);
    }

    // Normal session path runs on a tokio runtime.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[run_session] tokio runtime build failed: {e}");
            return ExitCode::from(2);
        }
    };
    match rt.block_on(run_session_main()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[run_session] {e}");
            ExitCode::from(1)
        }
    }
}

async fn run_session_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();
    if args.vdd_probe {
        return vdd_probe_main().await;
    }

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
                println!("[run_session]   will enable on Android connect, disable on disconnect");
                let log_path = std::env::temp_dir().join("penflow-vdd-helper.log");
                println!(
                    "[run_session]   if enable fails, the elevated helper trace is at: {}",
                    log_path.display()
                );
                Some(v)
            }
            Ok(None) => {
                println!(
                    "[run_session] no VDD installed (Display-class enumeration found no match)"
                );
                println!(
                    "[run_session]   capturing the physical monitor — see tools/vdd/README.md"
                );
                println!(
                    "[run_session]   (Qualcomm decoders may reject 4K streams; install VDD if so)"
                );
                println!(
                    "[run_session]   tip: set PENFLOW_VDD_TRACE=1 to see all enumerated devices"
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
                eprintln!("monitor index {} out of range; using 0", args.monitor_index);
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
            .map_err(|e| {
                format!("ADB transport bind failed: {e}. Is `adb` on PATH and a device attached?")
            })?,
    );

    println!("[run_session] transport ready. Launch the Penflow app on the device now.");

    let cfg = SessionConfig {
        monitor: fallback_monitor.clone(),
        // HEVC + the `.low_latency` decoder variant on Adreno is the
        // measured-best combo on the dev rig (~7-9 ms decode steady).
        // The H.264 path costs more on the decoder side because NVENC's
        // H.264 SPS inflates max_num_ref_frames — see SessionConfig
        // default in session.rs for the full story.
        codec: if args.h264 { Codec::H264 } else { Codec::Hevc },
        bitrate_bps: args.bitrate_bps,
        fps: args.fps,
        idr_interval: None,
        motion_idr_threshold_bytes: None,
        motion_idr_min_interval: Duration::from_millis(250),
        vdd,
        // The example doesn't read the GUI's settings.json — leave the
        // saved topology alone. The GUI's `service.rs` populates this
        // from `Settings::vdd_resolution` for the production path.
        vdd_target_resolution: None,
        hud_enabled: true,
        screen_off: false,
        pen_profile: penflow_core::inject::binding::PenButtonProfile::default(),
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
                    println!("[run_session] connected: {peer} → {device_width}x{device_height}");
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
    // 3rd arg = `finish`. None → session runs until Android
    // disconnects.
    let session_run = session.run(transport.clone(), Some(tx), None);
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

async fn vdd_probe_main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(mut vdd) = VddController::detect()? else {
        return Err("no Virtual Display Driver detected".into());
    };
    println!(
        "[vdd-probe] detected: '{}' ({})",
        vdd.friendly_name(),
        vdd.instance_id()
    );
    let baseline = vdd::snapshot_attached_monitor_keys()?;
    println!("[vdd-probe] baseline attached outputs: {}", baseline.len());

    let instance_id = vdd.instance_id().to_string();
    println!("[vdd-probe] enabling VDD; approve the UAC prompt");
    vdd.enable()?;

    println!("[vdd-probe] waiting for a new attached DXGI output");
    let result =
        vdd::wait_for_virtual_monitor(Duration::from_secs(15), Some(&instance_id), Some(&baseline))
            .await;

    println!("[vdd-probe] disabling VDD cleanup; approve the UAC prompt");
    if let Err(e) = vdd.disable() {
        eprintln!("[vdd-probe] cleanup disable failed: {e}");
    }

    let virt = result?;
    println!(
        "[vdd-probe] virtual monitor appeared: {} {}x{} on {}",
        virt.device_name, virt.width, virt.height, virt.adapter_name
    );
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
    /// `--vdd-probe`: enable VDD, attach display topology, wait for DXGI,
    /// then disable it again. Does not start ADB or wait for Android.
    vdd_probe: bool,
    /// `--h264`: use H.264 instead of HEVC. Default HEVC is faster on
    /// Adreno's `.low_latency` decoder; H.264 fallback for older devices
    /// without HEVC decode hardware.
    h264: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        monitor_index: 0,
        bitrate_bps: 50_000_000,
        fps: 120,
        no_vdd: false,
        vdd_probe: false,
        h264: false,
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
            "--vdd-probe" => {
                a.vdd_probe = true;
            }
            "--h264" => {
                a.h264 = true;
            }
            other => {
                eprintln!("[run_session] ignoring unknown arg: {other}");
            }
        }
        i += 1;
    }
    a
}
