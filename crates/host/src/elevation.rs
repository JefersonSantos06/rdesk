//! Windows integrity levels. A process that is not elevated cannot send clicks or keys
//! to windows of elevated processes (UIPI): Task Manager, installers, admin terminals.
//! On other systems there is no such split and everything reports "fine".

/// Is this process running elevated ("as administrator")?
pub fn is_elevated() -> bool {
    imp::is_elevated()
}

/// If the foreground window belongs to a process we cannot send input to, its executable
/// name (e.g. `Taskmgr.exe`).
pub fn foreground_blocks_input() -> Option<String> {
    imp::foreground_blocks_input()
}

/// What to tell the remote user when Windows is eating their clicks.
pub fn blocked_message(app: Option<&str>) -> String {
    let what = match app {
        Some(a) => format!("\"{a}\" runs as administrator and is in front"),
        None => "a window that runs as administrator is probably in front".to_string(),
    };
    format!(
        "Windows is blocking remote clicks and keys: {what}, and the rdesk host does not. \
         Close that window at the computer, or run the host as administrator."
    )
}

pub const NOT_ELEVATED_HINT: &str = "The host is not running as administrator: Windows will block remote clicks \
     and keys on windows that are (Task Manager, installers, admin terminals). To control those too, start \
     `rdesk host run` from a terminal opened with \"Run as administrator\", or use the scheduled task from the README.";

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    /// `None` when the token cannot be read at all.
    fn token_elevated(process: HANDLE) -> Option<bool> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: plain Win32 calls with valid out-pointers; the token handle is closed below.
        unsafe {
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut e as *mut TOKEN_ELEVATION as *mut c_void,
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            (ok != 0).then_some(e.TokenIsElevated != 0)
        }
    }

    pub fn is_elevated() -> bool {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)
    }

    pub fn foreground_blocks_input() -> Option<String> {
        // SAFETY: plain Win32 calls; the process handle is closed before returning.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 || pid == std::process::id() {
                return None;
            }
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let elevated = token_elevated(process);
            let mut buf = [0u16; 512];
            let mut size = buf.len() as u32;
            let name = if QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size) != 0 {
                let full = String::from_utf16_lossy(&buf[..size as usize]);
                full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
            } else {
                format!("process {pid}")
            };
            CloseHandle(process);
            // A non-elevated process may not even be allowed to read an elevated one's
            // token: that refusal is itself the sign.
            match elevated {
                Some(false) => None,
                Some(true) | None => Some(name),
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_elevated() -> bool {
        true
    }

    pub fn foreground_blocks_input() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_app() {
        assert!(blocked_message(Some("Taskmgr.exe")).contains("\"Taskmgr.exe\" runs as administrator"));
        assert!(blocked_message(None).contains("probably"));
    }

    #[test]
    fn queries_do_not_crash() {
        // Whatever the test runner's elevation and foreground window are, these must answer.
        let _ = is_elevated();
        let _ = foreground_blocks_input();
    }
}
