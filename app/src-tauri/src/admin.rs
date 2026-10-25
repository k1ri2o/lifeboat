//! Running as administrator: check, restart elevated, and wait for the old copy to exit.

use std::time::Duration;

/// The pid after `--relaunched`, if the app was started by "Restart as administrator".
pub fn relaunched_from(args: &[String]) -> Option<u32> {
    let i = args.iter().position(|a| a == "--relaunched")?;
    args.get(i + 1)?.parse().ok()
}

#[cfg(windows)]
pub fn is_admin() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: plain Win32 calls on our own process token; the handle is closed.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_admin() -> bool {
    false
}

/// Ask Windows to start this program again as administrator. Err holds a friendly message
/// (for example when the user pressed No).
#[cfg(windows)]
pub fn relaunch_as_admin() -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe().map_err(|_| "Could not restart Lifeboat.".to_string())?;
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(&format!("--relaunched {}", std::process::id()));
    // SAFETY: all strings are NUL-terminated and outlive the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if r > 32 {
        Ok(())
    } else {
        Err("Lifeboat was not restarted. Windows needs you to press \"Yes\" to allow it.".into())
    }
}

#[cfg(not(windows))]
pub fn relaunch_as_admin() -> Result<(), String> {
    Err("Restarting as administrator only works on Windows.".into())
}

/// Wait until process `pid` has exited, at most `max`.
#[cfg(windows)]
pub fn wait_for_exit(pid: u32, max: Duration) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    // SAFETY: the handle is checked and closed.
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return; // already gone
        }
        WaitForSingleObject(h, max.as_millis() as u32);
        CloseHandle(h);
    }
}

#[cfg(not(windows))]
pub fn wait_for_exit(_pid: u32, _max: Duration) {}

#[cfg(test)]
mod tests {
    use super::relaunched_from;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_the_old_pid() {
        assert_eq!(
            relaunched_from(&args(&["lifeboat.exe", "--relaunched", "4242"])),
            Some(4242)
        );
    }

    #[test]
    fn ignores_missing_or_bad_pid() {
        assert_eq!(relaunched_from(&args(&["lifeboat.exe"])), None);
        assert_eq!(relaunched_from(&args(&["x", "--relaunched"])), None);
        assert_eq!(relaunched_from(&args(&["x", "--relaunched", "abc"])), None);
    }
}
