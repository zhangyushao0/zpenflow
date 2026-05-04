//! Windows-only OS integration: autostart registry entry + UAC
//! re-launch. Stubs for non-Windows platforms keep the call sites
//! cross-platform-clean.

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    /// Subkey under HKCU that Windows scans on logon. Adding a value here
    /// makes the executable launch on every user sign-in.
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Penflow";

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn current_exe() -> std::io::Result<PathBuf> {
        std::env::current_exe()
    }

    pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
        unsafe {
            let subkey = to_wide(RUN_KEY);
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

            let value = to_wide(VALUE_NAME);
            let result: std::io::Result<()> = if enabled {
                let exe = current_exe()?;
                // Quote the path so spaces don't split it on logon.
                let cmd = format!("\"{}\"", exe.display());
                let cmd_w = to_wide(&cmd);
                let r = RegSetValueExW(
                    hkey,
                    PCWSTR(value.as_ptr()),
                    Some(0),
                    REG_SZ,
                    Some(bytes_of_u16(&cmd_w)),
                );
                if r.is_ok() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "RegSetValueExW: 0x{:08X}",
                        r.0
                    )))
                }
            } else {
                let r = RegDeleteValueW(hkey, PCWSTR(value.as_ptr()));
                if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "RegDeleteValueW: 0x{:08X}",
                        r.0
                    )))
                }
            };
            let _ = RegCloseKey(hkey);
            result
        }
    }

    fn bytes_of_u16(buf: &[u16]) -> &[u8] {
        // SAFETY: `[u16]` is `[u8; 2]`-aligned and we expose the same
        // total length in bytes; `RegSetValueExW` reads the byte buffer
        // and treats it as wide-char data via REG_SZ.
        unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const u8, std::mem::size_of_val(buf))
        }
    }

    /// Best-effort check: are we running with elevated privileges?
    pub fn is_elevated() -> bool {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        unsafe {
            let mut token: HANDLE = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut elev = TOKEN_ELEVATION::default();
            let mut ret_len: u32 = 0;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elev as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
            .is_ok();
            let _ = CloseHandle(token);
            ok && elev.TokenIsElevated != 0
        }
    }

    /// Spawn a fresh elevated copy of the running executable and return.
    /// Caller is expected to exit the current (unelevated) process.
    /// Triggers a UAC prompt; if the user declines, returns `Err`.
    pub fn relaunch_elevated() -> std::io::Result<()> {
        let exe = current_exe()?;
        let exe_w = to_wide(&exe.to_string_lossy());
        let verb_w = to_wide("runas");
        unsafe {
            let h = ShellExecuteW(
                None,
                PCWSTR(verb_w.as_ptr()),
                PCWSTR(exe_w.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
            // ShellExecuteW returns an HINSTANCE; values <= 32 indicate
            // failure (per docs).
            if (h.0 as isize) <= 32 {
                return Err(std::io::Error::other(format!(
                    "ShellExecuteW(runas) returned {}",
                    h.0 as isize
                )));
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_autostart_enabled() -> bool {
        false
    }
    pub fn set_autostart(_enabled: bool) -> std::io::Result<()> {
        Err(std::io::Error::other("autostart only implemented on Windows"))
    }
    pub fn is_elevated() -> bool {
        false
    }
    pub fn relaunch_elevated() -> std::io::Result<()> {
        Err(std::io::Error::other(
            "elevated re-launch only implemented on Windows",
        ))
    }
}

pub use imp::*;
