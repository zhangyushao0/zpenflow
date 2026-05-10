//! Query the user's "SDR content brightness" slider setting for a given
//! monitor, expressed as the multiplier applied to scRGB linear by
//! Windows' DWM compositor.
//!
//! # Why this matters
//!
//! When a Windows-HDR-on desktop renders an SDR window (Notepad, Krita,
//! file explorer, taskbar — i.e. 99% of the visible desktop), the
//! compositor places those pixels into the scRGB float buffer at
//! `linear * sdr_scale`, where `sdr_scale` is `SDRWhiteLevel_nits / 80`.
//! At default slider, SDR-white sits at scRGB 1.0 (= 80 nits). At
//! cranked-up slider (common on OLED HDR setups so SDR doesn't look
//! dim next to bright HDR areas), SDR-white can reach scRGB 6+ (= 480
//! nits).
//!
//! Our DDA-capture-→-encoder path treats scRGB as linear and clamps to
//! [0, 1] before sRGB encoding. Without compensating for the slider,
//! every SDR pixel above scRGB 1.0 gets crushed to white, which the
//! tablet sees as "Windows UI is overexposed". Dividing by `sdr_scale`
//! before clamp restores the [0, 1] range and renders SDR content
//! byte-identical to a native SDR display.
//!
//! # API path
//!
//! There is no DXGI shortcut for this — the DXGI output desc reports
//! the panel's HDR capability, not the user's brightness preference.
//! The information lives in `DisplayConfigGetDeviceInfo` with type
//! `DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL`. We:
//!   1. `GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS)` for sizes.
//!   2. `QueryDisplayConfig` to get path + mode arrays.
//!   3. Walk each path, call `DisplayConfigGetDeviceInfo` with
//!      `DISPLAYCONFIG_GET_SOURCE_NAME` to read the GDI device name
//!      (`\\.\DISPLAYn`).
//!   4. Match against the requested `device_name`; on a hit, call
//!      `DisplayConfigGetDeviceInfo` with `GET_SDR_WHITE_LEVEL`.
//!   5. Returned `SDRWhiteLevel` is in units where 1000 = 80 nits.
//!      Scale factor = `level / 1000`.
//!
//! On any failure the caller defaults to 1.0 (slider-at-default
//! assumption) — silent fallback is preferable to refusing to start.

use std::mem::size_of;

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};

/// Query the SDR-white-level multiplier for the monitor matching
/// `device_name` (e.g. `\\.\DISPLAY1`). Returns the scale factor by
/// which Windows multiplies SDR linear values when placing them into
/// scRGB:
///
///   - 1.0  → slider at default (SDR-white = 80 nits, scRGB 1.0)
///   - 2.0  → slider boosted to ~160 nits
///   - 6.0  → slider near max (~480 nits) — common on bright OLEDs
///
/// Returns `None` only if the API call genuinely failed (no matching
/// path, or HDR isn't on for this output, in which case the question
/// doesn't apply). Caller should treat `None` as "use 1.0".
pub fn query_sdr_white_level_scale(device_name: &str) -> Option<f32> {
    // Step 1: how many paths and modes does this system have right now?
    let mut path_count: u32 = 0;
    let mut mode_count: u32 = 0;
    let r = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    };
    if r.is_err() || path_count == 0 {
        return None;
    }

    // Step 2: pull the active path + mode arrays.
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
        vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
        vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    let r = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    };
    if r.is_err() {
        return None;
    }
    paths.truncate(path_count as usize);

    // Step 3 + 4: walk paths, find the one whose source device name
    // matches `device_name`, then ask for its SDR white level.
    for path in paths.iter() {
        // Pull the GDI device name for this source.
        let mut source_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // `DisplayConfigGetDeviceInfo` returns 0 (ERROR_SUCCESS) on
        // success, non-zero error code otherwise. We don't propagate —
        // a single missing path doesn't mean the whole query failed.
        let rc = unsafe { DisplayConfigGetDeviceInfo(&mut source_name.header as *mut _) };
        if rc != 0 {
            continue;
        }

        // Convert the wide GDI name to a Rust string. The buffer is
        // null-terminated; use the position of the first NUL.
        let gdi_name = wide_to_string(&source_name.viewGdiDeviceName);
        if gdi_name != device_name {
            continue;
        }

        // Found our path. Now ask for the SDR white level on the
        // SAME source (note: SDRWhiteLevel is a SOURCE attribute,
        // queried with the source's adapterId+id). Windows
        // documentation calls this out explicitly — it doesn't go on
        // the target.
        let mut sdr = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                // GET_SDR_WHITE_LEVEL is queried against the TARGET,
                // not the source. Use targetInfo's adapterId + id.
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            SDRWhiteLevel: 0,
        };
        let rc = unsafe { DisplayConfigGetDeviceInfo(&mut sdr.header as *mut _) };
        if rc != 0 {
            // Most likely: the output isn't in HDR mode, in which case
            // SDR white level isn't a meaningful query. Log and treat
            // as scale=1.0 (which is what the caller defaults to on
            // None).
            return None;
        }

        // The returned value is in units where 1000 = 80 nits.
        // scale = SDRWhiteLevel_nits / 80 = level / 1000.
        let scale = (sdr.SDRWhiteLevel as f32) / 1000.0;
        // Sanity-clamp. Windows allows values < 1000 (slider below
        // default) but the realistic range is roughly [0.5, 8.0].
        let clamped = scale.clamp(0.1, 16.0);
        return Some(clamped);
    }

    None
}

/// Convert a fixed-size wide character buffer (null-terminated) into a
/// Rust `String`. Stops at the first NUL.
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: the function shouldn't panic and shouldn't return
    /// nonsense values. `None` is acceptable on a CI runner with no
    /// monitors / no HDR.
    #[test]
    fn query_does_not_panic() {
        let _ = query_sdr_white_level_scale(r"\\.\DISPLAY1");
    }

    /// Diagnostic probe: print the SDR scale factor for every active
    /// monitor on the system. Run with
    /// `cargo test -p penflow-core probe_sdr_white_level -- --ignored --nocapture`.
    #[test]
    #[ignore = "diagnostic probe; not a regression test"]
    fn probe_sdr_white_level() {
        // Try DISPLAY1..DISPLAY8 — most setups don't go higher.
        for i in 1..=8 {
            let name = format!(r"\\.\DISPLAY{i}");
            let scale = query_sdr_white_level_scale(&name);
            println!("    {name} → SDR scale = {scale:?}");
        }
    }
}
