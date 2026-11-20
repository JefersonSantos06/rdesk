//! Ctrl+Alt+Del (the "secure attention sequence"). No injected key press makes it: Windows
//! sends it only when asked through `SendSAS` (sas.dll), by a service or a SYSTEM process
//! started by one, and only if the `SoftwareSASGeneration` policy lets services do it.
//! The service turns that policy on when it starts and the uninstall turns it back off.

use anyhow::{Context, Result, bail};
use rdesk_common::tf;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const POLICY_VALUE: &str = "SoftwareSASGeneration";
/// Bit 1: services may send it (bit 2: accessibility programs).
const SERVICES: u32 = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn policy() -> Option<u32> {
    let (key, value) = (wide(POLICY_KEY), wide(POLICY_VALUE));
    let mut data = 0u32;
    let mut len = size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut len,
        )
    };
    (r == ERROR_SUCCESS).then_some(data)
}

fn set_policy(v: Option<u32>) -> Result<()> {
    let (key, value) = (wide(POLICY_KEY), wide(POLICY_VALUE));
    let r = unsafe {
        match v {
            Some(v) => RegSetKeyValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                REG_DWORD,
                (&v as *const u32).cast(),
                size_of::<u32>() as u32,
            ),
            None => RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr()),
        }
    };
    if r != ERROR_SUCCESS {
        bail!("writing HKLM\\{POLICY_KEY}\\{POLICY_VALUE}: error {r}");
    }
    Ok(())
}

/// Marks that the policy was turned on by rdesk, so the uninstall undoes only its own change.
fn marker() -> PathBuf {
    super::service::program_dir().join("sas-policy")
}

/// Let services send Ctrl+Alt+Del (run as the service, or elevated).
pub fn allow() -> Result<()> {
    let now = policy();
    if now.unwrap_or(0) & SERVICES != 0 {
        return Ok(());
    }
    set_policy(Some(now.unwrap_or(0) | SERVICES))?;
    let _ = std::fs::write(marker(), now.map(|v| v.to_string()).unwrap_or_default());
    tracing::info!("allowed services to send Ctrl+Alt+Del ({POLICY_VALUE})");
    Ok(())
}

/// Undo [`allow`], if it changed anything.
pub fn revert() {
    let m = marker();
    let Ok(before) = std::fs::read_to_string(&m) else { return };
    let before = before.trim().parse::<u32>().ok();
    // Keep any other bit set since then; drop the value if it was not there before.
    let now = policy().unwrap_or(0) & !SERVICES;
    let restore = if before.is_none() && now == 0 { None } else { Some(now) };
    if let Err(e) = set_policy(restore) {
        tracing::warn!("{e:#}");
    }
    let _ = std::fs::remove_file(m);
}

/// Send Ctrl+Alt+Del to the session this process runs in.
pub fn send() -> Result<()> {
    if policy().unwrap_or(0) & SERVICES == 0 {
        bail!(tf!(
            "Windows does not let services send Ctrl+Alt+Del ({POLICY_VALUE}); restart the rdesk service",
            "O Windows não deixa serviços enviarem Ctrl+Alt+Del ({POLICY_VALUE}); reinicie o serviço do rdesk"
        ));
    }
    unsafe {
        let lib = LoadLibraryW(wide("sas.dll").as_ptr());
        if lib.is_null() {
            return Err(std::io::Error::last_os_error()).context("loading sas.dll");
        }
        // Freed with the process: loading it again is cheap.
        let f = GetProcAddress(lib, c"SendSAS".as_ptr().cast()).context("sas.dll has no SendSAS")?;
        let send_sas: unsafe extern "system" fn(i32) = std::mem::transmute(f);
        // FALSE: the caller is a service (or a SYSTEM process), not an accessibility program.
        send_sas(0);
    }
    Ok(())
}
