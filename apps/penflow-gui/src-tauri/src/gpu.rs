//! GPU selection: keep the VDD's host adapter and this process's DXGI
//! adapter preference on the SAME GPU.
//!
//! On hybrid-GPU laptops the engine picks its D3D11 device with
//! `EnumAdapterByGpuPreference(HIGH_PERFORMANCE)` while the bundled VDD
//! attaches wherever `vdd_settings.xml`'s `<gpu><friendlyname>` points
//! (default: wherever Windows feels like). When those disagree, Desktop
//! Duplication of the virtual monitor fails cross-adapter and Extend mode
//! dies with an opaque `0x8000FFFF`. Users previously had to hand-edit the
//! XML *and* flip Settings → Display → Graphics for Penflow.exe — two
//! hidden knobs that must agree.
//!
//! This module powers a single GUI dropdown that drives both:
//!   - the friendly name written into `vdd_settings.xml` (see
//!     `settings::render_vdd_settings_xml`), and
//!   - the per-app GPU preference Windows stores under
//!     `HKCU\Software\Microsoft\DirectX\UserGpuPreferences` — the exact
//!     registry value the Settings → Display → Graphics page writes. That
//!     preference remaps what `EnumAdapterByGpuPreference` returns for
//!     this process, steering the engine onto the chosen GPU.
//!
//! Both take effect lazily: the registry value at next process start, the
//! XML at the next VDD enable cycle. The GUI surfaces that as a "restart
//! Penflow" hint.

/// `GpuPreference` values as written by Settings → Display → Graphics.
pub const GPU_PREF_AUTO: u32 = 0;
pub const GPU_PREF_POWER_SAVING: u32 = 1;
pub const GPU_PREF_HIGH_PERFORMANCE: u32 = 2;

/// Registry value payload for a given preference, byte-for-byte what the
/// Windows Settings page writes (trailing semicolon included).
pub fn gpu_pref_registry_value(pref: u32) -> String {
    format!("GpuPreference={pref};")
}

/// Map a user-chosen adapter name onto a `GpuPreference` value using the
/// system's own notion of which adapter is "power saving" vs "high
/// performance". Empty choice = auto. An unrecognized name falls back to
/// auto rather than guessing — wrong-but-confident is how the 0x8000FFFF
/// class of bug happens.
pub fn preference_for_choice(
    choice: &str,
    power_saving: Option<&str>,
    high_performance: Option<&str>,
) -> u32 {
    let c = choice.trim();
    if c.is_empty() {
        return GPU_PREF_AUTO;
    }
    if Some(c) == high_performance {
        return GPU_PREF_HIGH_PERFORMANCE;
    }
    if Some(c) == power_saving {
        return GPU_PREF_POWER_SAVING;
    }
    GPU_PREF_AUTO
}

#[cfg(windows)]
mod imp {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIFactory6,
        DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
        DXGI_GPU_PREFERENCE_MINIMUM_POWER,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    const USER_GPU_PREFS_KEY: &str = r"Software\Microsoft\DirectX\UserGpuPreferences";

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn adapter_name(a: &IDXGIAdapter1) -> Option<(String, bool)> {
        // windows 0.62: GetDesc1 returns the struct (no out-param).
        let desc = unsafe { a.GetDesc1() }.ok()?;
        let len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..len])
            .trim()
            .to_string();
        let software = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        Some((name, software))
    }

    /// All hardware adapters, deduplicated, enumeration order preserved.
    /// (Modern NVIDIA drivers expose multiple logical adapters per GPU.)
    pub fn list_gpus() -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
            Ok(f) => f,
            Err(_) => return out,
        };
        let mut i = 0u32;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            if let Some((name, software)) = adapter_name(&adapter) {
                if !software && !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }

    fn first_by_preference(
        pref: windows::Win32::Graphics::Dxgi::DXGI_GPU_PREFERENCE,
    ) -> Option<String> {
        let factory: IDXGIFactory6 = unsafe { CreateDXGIFactory1() }.ok()?;
        let adapter: IDXGIAdapter1 =
            unsafe { factory.EnumAdapterByGpuPreference(0, pref) }.ok()?;
        adapter_name(&adapter).map(|(n, _)| n)
    }

    pub fn power_saving_gpu() -> Option<String> {
        first_by_preference(DXGI_GPU_PREFERENCE_MINIMUM_POWER)
    }

    pub fn high_performance_gpu() -> Option<String> {
        first_by_preference(DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
    }

    /// Write (or clear, for `GPU_PREF_AUTO`) this process's per-app GPU
    /// preference — same key + format as Settings → Display → Graphics.
    /// The value name is the executable's full path.
    pub fn set_process_gpu_preference(pref: u32) -> std::io::Result<()> {
        let exe = std::env::current_exe()?;
        let exe_w = to_wide(&exe.to_string_lossy());

        unsafe {
            let subkey = to_wide(USER_GPU_PREFS_KEY);
            let mut hkey: HKEY = HKEY::default();
            let create = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                Some(0),
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut hkey,
                None,
            );
            if !create.is_ok() {
                return Err(std::io::Error::other(format!(
                    "RegCreateKeyExW: 0x{:08X}",
                    create.0
                )));
            }

            let result: std::io::Result<()> = if pref == super::GPU_PREF_AUTO {
                let r = RegDeleteValueW(hkey, PCWSTR(exe_w.as_ptr()));
                if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "RegDeleteValueW: 0x{:08X}",
                        r.0
                    )))
                }
            } else {
                let data = to_wide(&super::gpu_pref_registry_value(pref));
                let bytes = std::slice::from_raw_parts(
                    data.as_ptr() as *const u8,
                    data.len() * 2,
                );
                let r = RegSetValueExW(
                    hkey,
                    PCWSTR(exe_w.as_ptr()),
                    Some(0),
                    REG_SZ,
                    Some(bytes),
                );
                if r.is_ok() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "RegSetValueExW: 0x{:08X}",
                        r.0
                    )))
                }
            };
            let _ = RegCloseKey(hkey);
            result
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn list_gpus() -> Vec<String> {
        Vec::new()
    }
    pub fn power_saving_gpu() -> Option<String> {
        None
    }
    pub fn high_performance_gpu() -> Option<String> {
        None
    }
    pub fn set_process_gpu_preference(_pref: u32) -> std::io::Result<()> {
        Ok(())
    }
}

pub use imp::{high_performance_gpu, list_gpus, power_saving_gpu, set_process_gpu_preference};

/// Resolve the user's chosen adapter name and apply the per-app GPU
/// preference accordingly. Called from `save_settings`.
pub fn apply_process_gpu_preference(choice: &str) -> std::io::Result<()> {
    let pref = preference_for_choice(
        choice,
        power_saving_gpu().as_deref(),
        high_performance_gpu().as_deref(),
    );
    set_process_gpu_preference(pref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_value_matches_windows_settings_format() {
        assert_eq!(gpu_pref_registry_value(1), "GpuPreference=1;");
        assert_eq!(gpu_pref_registry_value(2), "GpuPreference=2;");
    }

    #[test]
    fn choice_maps_to_system_preference() {
        let ps = Some("Intel(R) Graphics");
        let hp = Some("NVIDIA GeForce RTX 5080 Laptop GPU");
        assert_eq!(
            preference_for_choice("NVIDIA GeForce RTX 5080 Laptop GPU", ps, hp),
            GPU_PREF_HIGH_PERFORMANCE
        );
        assert_eq!(
            preference_for_choice("Intel(R) Graphics", ps, hp),
            GPU_PREF_POWER_SAVING
        );
        assert_eq!(preference_for_choice("", ps, hp), GPU_PREF_AUTO);
        assert_eq!(preference_for_choice("  ", ps, hp), GPU_PREF_AUTO);
        // Unknown name: auto, never a confident wrong guess.
        assert_eq!(preference_for_choice("Some eGPU", ps, hp), GPU_PREF_AUTO);
        // Single-GPU machine: both probes return the same adapter — the
        // high-performance match must win so the pref is deterministic.
        let only = Some("Only GPU");
        assert_eq!(
            preference_for_choice("Only GPU", only, only),
            GPU_PREF_HIGH_PERFORMANCE
        );
    }
}
