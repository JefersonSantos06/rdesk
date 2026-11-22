//! Restarting the computer from the session ([`rdesk_proto::ExtClient::Power`]), normally or in
//! safe mode with networking (Windows), as TeamViewer and AnyDesk do for unattended computers.
//! The client keeps trying to reconnect and finds it again once the service is back.
//!
//! Safe mode: the service is registered to start there too, and the computer is told to boot
//! into it once; the host undoes that as soon as it starts in safe mode, so the next restart is
//! a normal one.

use anyhow::Result;

/// Whether this host can restart its computer: Windows, with administrator rights (the service).
pub fn supported() -> bool {
    cfg!(windows) && crate::elevation::is_elevated()
}

#[cfg(windows)]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(cmd).args(args).creation_flags(CREATE_NO_WINDOW).output()?;
    if !out.status.success() {
        anyhow::bail!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Restart in a few seconds (time for the session to hear about it).
#[cfg(windows)]
pub fn restart(safe_mode: bool, who: &str) -> Result<()> {
    anyhow::ensure!(supported(), "restarting needs rdesk installed as a service");
    if safe_mode {
        run(
            "reg",
            &[
                "add",
                r"HKLM\SYSTEM\CurrentControlSet\Control\SafeBoot\Network\rdesk",
                "/ve",
                "/t",
                "REG_SZ",
                "/d",
                "Service",
                "/f",
            ],
        )?;
        run("bcdedit", &["/set", "{current}", "safeboot", "network"])?;
    }
    let why = format!("rdesk: {who}");
    run("shutdown", &["/r", "/t", "5", "/d", "p:0:0", "/c", &why])
}

#[cfg(not(windows))]
pub fn restart(_: bool, _: &str) -> Result<()> {
    anyhow::bail!("restarting from a session works on Windows computers only")
}

/// Started in safe mode (asked for by a session): make the next boot a normal one.
#[cfg(windows)]
pub fn leave_safe_mode() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CLEANBOOT};
    if unsafe { GetSystemMetrics(SM_CLEANBOOT) } != 0 && supported() {
        match run("bcdedit", &["/deletevalue", "{current}", "safeboot"]) {
            Ok(()) => tracing::info!("in safe mode: the next restart is a normal one"),
            Err(e) => tracing::debug!("leaving safe mode: {e:#}"),
        }
    }
}

#[cfg(not(windows))]
pub fn leave_safe_mode() {}
