//! Remembered host passwords in the operating system's credential store: Windows
//! Credential Manager, the macOS Keychain, or the Secret Service on Linux. Nothing is
//! written to rdesk's own files.
//!
//! On a phone the app's own storage holds them instead (`secrets.json` in its private
//! data directory, which no other app can read and the phone encrypts at rest; the same
//! place as the server sign-in).
//!
//! These calls block (the Keychain may even show a dialog); call them off async workers.

use anyhow::{Result, anyhow};
use zeroize::Zeroizing;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub use phone::*;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use desktop::*;

/// Follow a host rename.
pub fn rename(old: &str, new: &str) -> Result<()> {
    if let Some(p) = get(old) {
        set(new, &p)?;
        forget(old)?;
    }
    Ok(())
}

pub fn has(target: &str) -> bool {
    get(target).is_some()
}

#[cfg(any(target_os = "android", target_os = "ios"))]
mod phone {
    use std::collections::BTreeMap;

    use super::*;

    const FILE: &str = "secrets.json";
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub fn store_name() -> &'static str {
        "this phone"
    }

    pub fn available() -> bool {
        rdesk_common::config::config_dir().is_ok()
    }

    fn load() -> BTreeMap<String, String> {
        let Ok(path) = rdesk_common::config::config_path(FILE) else { return BTreeMap::new() };
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(map: &BTreeMap<String, String>) -> Result<()> {
        let path = rdesk_common::config::config_path(FILE)?;
        let text = Zeroizing::new(serde_json::to_string(map)?);
        rdesk_common::config::write_private(&path, text.as_bytes())
    }

    pub fn get(target: &str) -> Option<Zeroizing<String>> {
        let _g = LOCK.lock().unwrap();
        load().remove(target).filter(|p| !p.is_empty()).map(Zeroizing::new)
    }

    pub fn set(target: &str, password: &str) -> Result<()> {
        let _g = LOCK.lock().unwrap();
        let mut m = load();
        m.insert(target.to_string(), password.to_string());
        save(&m).map_err(|e| anyhow!("saving the password: {e:#}"))
    }

    pub fn forget(target: &str) -> Result<()> {
        let _g = LOCK.lock().unwrap();
        let mut m = load();
        if m.remove(target).is_some() {
            save(&m)?;
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod desktop {
    use super::*;

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

    pub fn set(target: &str, password: &str) -> Result<()> {
        entry(target)?.set_password(password).map_err(|e| anyhow!("saving to {}: {e}", store_name()))
    }

    pub fn forget(target: &str) -> Result<()> {
        match entry(target)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!("removing from {}: {e}", store_name())),
        }
    }
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
