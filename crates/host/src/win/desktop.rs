//! Following the input desktop. Windows shows the sign-in screen, the lock screen and UAC
//! prompts on a separate desktop ("Winlogon"); a thread only sees and controls the desktop
//! it is attached to. Started by the service (as SYSTEM), the host can attach its capture
//! and input threads to whichever desktop has the keyboard right now. For a normal user
//! that desktop cannot be opened and nothing changes.

use std::cell::Cell;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

thread_local! {
    /// The desktop this thread switched to (ours to close when switching again).
    static OPENED: Cell<HDESK> = const { Cell::new(null_mut()) };
}

fn name(desk: HDESK) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(desk, UOI_NAME, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut needed)
    };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Attach the calling thread to the input desktop if it changed. Returns its name when
/// it switched. The thread must not own windows (capture and input threads do not).
pub fn follow_input_desktop() -> Option<String> {
    unsafe {
        let input = OpenInputDesktop(0, 0, GENERIC_ALL);
        if input.is_null() {
            return None;
        }
        let current = GetThreadDesktop(GetCurrentThreadId());
        let (want, have) = (name(input), name(current));
        if want.is_some() && want == have {
            CloseDesktop(input);
            return None;
        }
        if SetThreadDesktop(input) == 0 {
            CloseDesktop(input);
            return None;
        }
        let previous = OPENED.with(|o| o.replace(input));
        if !previous.is_null() {
            CloseDesktop(previous);
        }
        want
    }
}
