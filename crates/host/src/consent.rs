//! Asking the person at the computer before a session starts (`[consent]` in host.toml,
//! off by default): "Maria wants to see and control this computer. Allow?". Like other
//! remote desktops' attended mode. Nobody is asked when nobody can answer: the computer is
//! locked or at the sign-in screen. Without an answer in time, `allow_on_timeout` decides.

use std::time::Duration;

use rdesk_common::config::ConsentConfig;
use rdesk_common::{t, tf};

/// What happened when asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    Denied,
    /// Nobody answered (or could be asked): the configured default applies.
    NoAnswer,
}

/// Ask whether `who` may start a session that can do `what`; `true` to go on.
pub async fn ask(cfg: &ConsentConfig, who: &str, what: &str) -> bool {
    if !cfg.ask {
        return true;
    }
    let text = tf!(
        "{who} wants to connect to this computer ({what}).\n\nAllow it?",
        "{who} quer se conectar a este computador ({what}).\n\nPermitir?"
    );
    let timeout = Duration::from_secs(cfg.timeout_s.clamp(5, 300) as u64);
    let answer = tokio::task::spawn_blocking(move || platform_ask(&text, timeout)).await.unwrap_or(Answer::NoAnswer);
    tracing::info!("asked the person at the computer about {who}: {answer:?}");
    match answer {
        Answer::Allowed => true,
        Answer::Denied => false,
        Answer::NoAnswer => cfg.allow_on_timeout,
    }
}

const TITLE: fn() -> &'static str = || t!("rdesk: allow access?", "rdesk: permitir acesso?");

#[cfg(windows)]
fn platform_ask(text: &str, timeout: Duration) -> Answer {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDNO, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_SETFOREGROUND, MB_SYSTEMMODAL, MB_TOPMOST, MB_YESNO,
    };
    // Locked, signed out or a UAC prompt: nobody to ask.
    if crate::win::desktop::input_desktop_name().is_none_or(|d| !d.eq_ignore_ascii_case("Default")) {
        return Answer::NoAnswer;
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, title) = (wide(text), wide(TITLE()));
    let ms = timeout.as_millis() as u32;
    // Its own thread, on the desktop with the keyboard (the host may run as SYSTEM).
    let r = std::thread::spawn(move || {
        let _ = crate::win::desktop::follow_input_desktop();
        // MessageBoxTimeoutW: exported by user32 since Windows XP, not in the SDK headers.
        type Mbt = unsafe extern "system" fn(*mut core::ffi::c_void, *const u16, *const u16, u32, u16, u32) -> i32;
        let f: Option<Mbt> = unsafe {
            use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
            let lib = LoadLibraryA(c"user32.dll".as_ptr() as _);
            GetProcAddress(lib, c"MessageBoxTimeoutW".as_ptr() as _).map(|p| std::mem::transmute(p))
        };
        let Some(f) = f else { return 0 };
        let style = MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND | MB_SYSTEMMODAL;
        unsafe { f(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), style, 0, ms) }
    })
    .join()
    .unwrap_or(0);
    match r {
        x if x == IDYES => Answer::Allowed,
        x if x == IDNO => Answer::Denied,
        _ => Answer::NoAnswer,
    }
}

#[cfg(target_os = "linux")]
fn platform_ask(text: &str, timeout: Duration) -> Answer {
    // zenity: 0 yes, 1 no, 5 timed out.
    let out = std::process::Command::new("zenity")
        .args(["--question", "--title", TITLE(), "--text", text, "--timeout", &timeout.as_secs().to_string()])
        .status();
    match out.map(|s| s.code()) {
        Ok(Some(0)) => Answer::Allowed,
        Ok(Some(1)) => Answer::Denied,
        _ => Answer::NoAnswer,
    }
}

#[cfg(target_os = "macos")]
fn platform_ask(text: &str, timeout: Duration) -> Answer {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display dialog \"{}\" with title \"{}\" buttons {{\"{}\", \"{}\"}} default button 1 giving up after {}",
        esc(text),
        esc(TITLE()),
        esc(t!("Deny", "Recusar")),
        esc(t!("Allow", "Permitir")),
        timeout.as_secs()
    );
    let out = std::process::Command::new("osascript").args(["-e", &script]).output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("gave up:true") {
                Answer::NoAnswer
            } else if s.contains(t!("Allow", "Permitir")) {
                Answer::Allowed
            } else {
                Answer::Denied
            }
        }
        // Cancelled (Escape) counts as no.
        Ok(_) => Answer::Denied,
        Err(_) => Answer::NoAnswer,
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_ask(_: &str, _: Duration) -> Answer {
    Answer::NoAnswer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn off_means_no_question() {
        let cfg = ConsentConfig { ask: false, ..Default::default() };
        assert!(ask(&cfg, "someone", "full control").await);
    }
}
