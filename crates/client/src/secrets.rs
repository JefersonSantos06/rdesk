//! Remembered host passwords in the operating system's credential store: Windows
//! Credential Manager, the macOS Keychain, or the Secret Service on Linux. Nothing is
//! written to rdesk's own files.
//!
//! These calls block (the Keychain may even show a dialog); call them off async workers.

use anyhow::{Result, anyhow};
use zeroize::Zeroizing;

const SERVICE: &str = "rdesk";

fn entry(target: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, target).map_err(|e| anyhow!("{e}"))
}

/// Human name of the store, for labels like "Remember in …".
pub fn store_name() -> &'static str {
    if cfg!(windows) {
        "Windows Credential Manager"
    } else if cfg!(target_os = "macos") {
        "the macOS Keychain"
    } else {
        "the system keyring"
    }
}

/// Is a credential store usable on this machine (e.g. no Secret Service on a bare Linux)?
pub fn available() -> bool {
    keyring::Entry::store_status().is_ok()
}

pub fn get(target: &str) -> Option<Zeroizing<String>> {
    match entry(target).and_then(|e| e.get_password().map_err(|e| anyhow!("{e}"))) {
        Ok(p) if !p.is_empty() => Some(Zeroizing::new(p)),
        _ => None,
    }
}

pub fn has(target: &str) -> bool {
    get(target).is_some()
}

pub fn set(target: &str, password: &str) -> Result<()> {
    entry(target)?.set_password(password).map_err(|e| anyhow!("saving to {}: {e}", store_name()))
}

pub fn forget(target: &str) -> Result<()> {
    match entry(target)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!("removing from {}: {e}", store_name())),
    }
}

/// Follow a host rename.
pub fn rename(old: &str, new: &str) -> Result<()> {
    if let Some(p) = get(old) {
        set(new, &p)?;
        forget(old)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Touches the real credential store: run by hand with `--ignored`.
    #[test]
    #[ignore]
    fn round_trip_in_the_real_store() {
        assert!(super::available(), "no credential store");
        let t = format!("rdesk-selftest-{}", std::process::id());
        assert!(super::get(&t).is_none());
        super::set(&t, "s3cret-é").unwrap();
        assert_eq!(super::get(&t).as_deref().map(String::as_str), Some("s3cret-é"));
        super::rename(&t, &format!("{t}-renamed")).unwrap();
        assert!(super::get(&t).is_none());
        super::forget(&format!("{t}-renamed")).unwrap();
        assert!(super::get(&format!("{t}-renamed")).is_none());
        // Forgetting twice is fine.
        super::forget(&t).unwrap();
    }
}
