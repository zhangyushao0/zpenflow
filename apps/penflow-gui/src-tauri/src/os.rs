//! Windows-only OS integration: autostart registry entry + UAC
//! re-launch. Stubs for non-Windows platforms keep the call sites
//! cross-platform-clean.

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HWND};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, ShellExecuteW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

    /// Subkey under HKCU that Windows scans on logon. Adding a value here
    /// makes the executable launch on every user sign-in.
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Penflow";

    /// Name of the Task Scheduler task we create when "run as admin" is
    /// on. The task is configured with `/RL HIGHEST` so launching it
    /// runs Penflow elevated WITHOUT a UAC prompt — the prompt is
    /// charged once at task-creation time and never again.
    const ADMIN_TASK_NAME: &str = "Penflow";

    /// `CREATE_NO_WINDOW` — same trick the adb transport uses. Without
    /// this every `schtasks` invocation pops a console flicker for a
    /// fraction of a second since the GUI exe has no parent console.
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    fn silent(cmd: &mut Command) -> &mut Command {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW)
    }

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
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, std::mem::size_of_val(buf)) }
    }

    /// Best-effort check: are we running with elevated privileges?
    pub fn is_elevated() -> bool {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
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

    /// Does a Task Scheduler task named `Penflow` exist? Used both by
    /// the launcher (decides whether `schtasks /Run` is available as a
    /// no-UAC fast path) and by save_settings (decides whether to
    /// create vs. update). Query is unprivileged.
    pub fn has_admin_task() -> bool {
        let mut cmd = Command::new("schtasks.exe");
        cmd.args(["/Query", "/TN", ADMIN_TASK_NAME])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        silent(&mut cmd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Create or replace the `Penflow` task. Run level `HIGHEST` is the
    /// magic that makes subsequent `/Run` invocations skip UAC. When
    /// `with_logon_trigger` is true, the task fires on every user
    /// logon (autostart-as-admin); otherwise it only runs on demand.
    /// Creating a Highest-level task requires admin → first call burns
    /// one UAC prompt; future calls (e.g. flipping autostart later)
    /// reuse that elevation if we're already running as admin.
    pub fn create_admin_task(with_logon_trigger: bool) -> std::io::Result<()> {
        let exe = current_exe()?;
        // schtasks /TR wants the program path quoted if it has spaces,
        // and the surrounding quotes must be PART OF the value — so we
        // wrap the exe path in escaped quotes inside the arg string.
        let tr = format!("\"{}\"", exe.display());

        // Build args. ONLOGON gets a real trigger; the no-trigger
        // variant uses "ONCE" with a far-future date that never fires
        // (schtasks /Create requires SOME schedule).
        let args: Vec<&str> = if with_logon_trigger {
            vec![
                "/Create",
                "/TN",
                ADMIN_TASK_NAME,
                "/TR",
                &tr,
                "/SC",
                "ONLOGON",
                "/RL",
                "HIGHEST",
                "/F",
            ]
        } else {
            vec![
                "/Create",
                "/TN",
                ADMIN_TASK_NAME,
                "/TR",
                &tr,
                "/SC",
                "ONCE",
                "/ST",
                "00:00",
                "/SD",
                "01/01/2099",
                "/RL",
                "HIGHEST",
                "/F",
            ]
        };
        run_schtasks(&args)
    }

    /// Delete the `Penflow` task. No-op if it doesn't exist. Needs admin
    /// only because the task we created is `RL HIGHEST` (Microsoft
    /// gates manipulation of elevated tasks).
    pub fn delete_admin_task() -> std::io::Result<()> {
        if !has_admin_task() {
            return Ok(());
        }
        run_schtasks(&["/Delete", "/TN", ADMIN_TASK_NAME, "/F"])
    }

    /// Trigger an immediate run of the task. Returns once schtasks
    /// reports the start (the task itself runs async). The newly-
    /// spawned penflow-gui.exe inherits Highest run level → elevated,
    /// no UAC. Caller is expected to exit the current (unelevated)
    /// process.
    pub fn run_admin_task() -> std::io::Result<()> {
        let mut cmd = Command::new("schtasks.exe");
        cmd.args(["/Run", "/TN", ADMIN_TASK_NAME])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = silent(&mut cmd).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "schtasks /Run /TN {ADMIN_TASK_NAME} exit={status:?}"
            )))
        }
    }

    /// Invoke schtasks.exe with the given args. If we're already
    /// elevated, spawn directly; otherwise re-run schtasks via
    /// `ShellExecuteEx("runas")` which costs one UAC prompt. We block
    /// on the elevated child so the caller knows whether the task
    /// operation actually completed.
    fn run_schtasks(args: &[&str]) -> std::io::Result<()> {
        if is_elevated() {
            let mut cmd = Command::new("schtasks.exe");
            cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
            let out = silent(&mut cmd).output()?;
            return if out.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "schtasks {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                )))
            };
        }

        // Unelevated path: runas via ShellExecuteEx, wait for exit code.
        let params = serialize_args(args);
        let exe_w = to_wide("schtasks.exe");
        let params_w = to_wide(&params);
        let verb_w = to_wide("runas");

        let mut sei = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            hwnd: HWND::default(),
            lpVerb: PCWSTR(verb_w.as_ptr()),
            lpFile: PCWSTR(exe_w.as_ptr()),
            lpParameters: PCWSTR(params_w.as_ptr()),
            lpDirectory: PCWSTR::null(),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        unsafe { ShellExecuteExW(&mut sei) }
            .map_err(|e| std::io::Error::other(format!("ShellExecuteExW(runas): {e}")))?;
        let process = sei.hProcess;
        if process.is_invalid() {
            return Err(std::io::Error::other(
                "ShellExecuteEx returned no process handle (UAC declined?)",
            ));
        }
        let _ = unsafe { WaitForSingleObject(process, INFINITE) };
        let mut code: u32 = 0;
        let _ = unsafe { GetExitCodeProcess(process, &mut code) };
        let _ = unsafe { CloseHandle(process) };
        if code == 0 {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("schtasks exit={code}")))
        }
    }

    /// Run an arbitrary executable elevated, wait for completion,
    /// return its exit code. UAC prompt unless we're already elevated.
    /// Used by the VMulti fallback-install command (issue #23 follow-up).
    pub fn run_elevated_wait(exe: &std::path::Path, params: &str) -> std::io::Result<i32> {
        let exe_w = to_wide(&exe.to_string_lossy());
        let params_w = to_wide(params);
        let verb_w = to_wide("runas");

        let mut sei = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            hwnd: HWND::default(),
            lpVerb: PCWSTR(verb_w.as_ptr()),
            lpFile: PCWSTR(exe_w.as_ptr()),
            lpParameters: PCWSTR(params_w.as_ptr()),
            lpDirectory: PCWSTR::null(),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        unsafe { ShellExecuteExW(&mut sei) }
            .map_err(|e| std::io::Error::other(format!("ShellExecuteExW(runas): {e}")))?;
        let process = sei.hProcess;
        if process.is_invalid() {
            return Err(std::io::Error::other(
                "ShellExecuteEx returned no process handle (UAC declined?)",
            ));
        }
        let _ = unsafe { WaitForSingleObject(process, INFINITE) };
        let mut code: u32 = 0;
        let _ = unsafe { GetExitCodeProcess(process, &mut code) };
        let _ = unsafe { CloseHandle(process) };
        Ok(code as i32)
    }

    /// Quote each argv element for the Windows command line. The
    /// rules ShellExecute uses are CommandLineToArgvW-compatible:
    /// wrap in `"` if it has whitespace or quotes; double interior
    /// quotes by escaping with backslashes.
    fn serialize_args(args: &[&str]) -> String {
        args.iter()
            .map(|a| {
                let needs_quote =
                    a.is_empty() || a.contains(|c: char| c.is_whitespace() || c == '"');
                if needs_quote {
                    let escaped = a.replace('\\', "\\\\").replace('"', "\\\"");
                    format!("\"{escaped}\"")
                } else {
                    (*a).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_autostart_enabled() -> bool {
        false
    }
    pub fn set_autostart(_enabled: bool) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "autostart only implemented on Windows",
        ))
    }
    pub fn is_elevated() -> bool {
        false
    }
    pub fn relaunch_elevated() -> std::io::Result<()> {
        Err(std::io::Error::other(
            "elevated re-launch only implemented on Windows",
        ))
    }
    pub fn has_admin_task() -> bool {
        false
    }
    pub fn create_admin_task(_with_logon_trigger: bool) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "admin task only implemented on Windows",
        ))
    }
    pub fn delete_admin_task() -> std::io::Result<()> {
        Ok(())
    }
    pub fn run_admin_task() -> std::io::Result<()> {
        Err(std::io::Error::other(
            "admin task only implemented on Windows",
        ))
    }
    pub fn run_elevated_wait(_exe: &std::path::Path, _params: &str) -> std::io::Result<i32> {
        Err(std::io::Error::other(
            "elevated run only implemented on Windows",
        ))
    }
}

pub use imp::*;
