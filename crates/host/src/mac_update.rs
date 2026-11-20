//! Installing a released version on a Mac, by the host inside rdesk.app.
//!
//! 1. Check the server's offer: `release.json` signed by an rdesk release key (built into
//!    this program), the Mac file's size and SHA-256.
//! 2. Download and unpack it next to the running rdesk.app (same disk, so the swap is a
//!    rename), and check that the new program says it is that version.
//! 3. Wait until nobody is connected, then swap the two bundles. The app notices that its
//!    program changed and starts again from the new one once no session of its own is open.
//!
//! Built elsewhere only for its tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use rdesk_common::release::{check_file, download, file_for, verify_manifest};
use rdesk_proto::api::UpdateOffer;
use rdesk_proto::release::FileKind;

use crate::control::Reporter;
use crate::server::Server;

pub const PLATFORM: &str = "macos-aarch64";

/// The `.app` bundle this program runs from, if it does.
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    // rdesk.app/Contents/MacOS/rdesk
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

/// Versions tried by this process: each once (a failure is not retried at every sync).
static TRIED: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub(crate) async fn install(offer: UpdateOffer, server: Arc<Server>, reporter: Reporter) {
    let version = offer.version.clone();
    let Some(app) = bundle() else {
        reporter.event(format!("rdesk {version} is available: download it from the release page"), false);
        return;
    };
    {
        let mut tried = TRIED.lock().unwrap();
        if tried.contains(&version) {
            return;
        }
        tried.push(version.clone());
    }
    let staged = match prepare(&offer, &app).await {
        Ok(p) => p,
        Err(e) => {
            reporter.event(format!("Could not download rdesk {version}: {e:#}"), true);
            return;
        }
    };
    reporter.event(format!("rdesk {version} downloaded; it installs when nobody is connected"), false);
    while server.in_session().await {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    match swap(&app, &staged) {
        Ok(()) => reporter.event(format!("Installed rdesk {version}; the app starts again"), false),
        Err(e) => reporter.event(format!("Could not install rdesk {version}: {e:#}"), true),
    }
}

/// Download, check and unpack the new rdesk.app; where it is.
async fn prepare(offer: &UpdateOffer, app: &Path) -> Result<PathBuf> {
    let m = verify_manifest(offer.manifest.as_bytes(), &offer.signature)?;
    ensure!(m.version == offer.version, "the signed manifest is for {}, not {}", m.version, offer.version);
    let file = file_for(&m, FileKind::App, PLATFORM).ok_or_else(|| anyhow!("no Mac file in {}", m.version))?;
    let data = download(&format!("{}{}", offer.download_base, file.name), file.size).await?;
    check_file(file, &data)?;

    let parent = app.parent().context("rdesk.app has no folder")?;
    let dir = parent.join(format!(".rdesk-{}.update", m.version));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| {
        format!("{} cannot be changed: move rdesk.app to a folder you can write to", parent.display())
    })?;
    let tgz = dir.join(&file.name);
    std::fs::write(&tgz, &data)?;
    let out = std::process::Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&dir)
        .output()
        .context("unpacking the update")?;
    ensure!(out.status.success(), "unpacking the update: {}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_file(&tgz);
    let new_app = dir.join("rdesk.app");
    // It must say it is the version the manifest promises.
    let exe = new_app.join("Contents").join("MacOS").join("rdesk");
    let out = std::process::Command::new(&exe).arg("--version").output().context("running the new program")?;
    let says = String::from_utf8_lossy(&out.stdout);
    ensure!(says.trim() == format!("rdesk {}", m.version), "the new program says {:?}", says.trim());
    // Downloaded by this program, it carries no quarantine mark; make sure of it.
    let _ = std::process::Command::new("/usr/bin/xattr").args(["-dr", "com.apple.quarantine"]).arg(&new_app).output();
    Ok(new_app)
}

/// Put `new_app` where `app` is; the old one goes away (back in place if that fails).
fn swap(app: &Path, new_app: &Path) -> Result<()> {
    let old = app.with_file_name(".rdesk-old.app");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(app, &old).with_context(|| format!("moving {} aside", app.display()))?;
    if let Err(e) = std::fs::rename(new_app, app) {
        let _ = std::fs::rename(&old, app);
        return Err(e).context("putting the new rdesk.app in place");
    }
    // The running program keeps its (now deleted) files open until it restarts.
    let _ = std::fs::remove_dir_all(&old);
    if let Some(dir) = new_app.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_replaces_the_bundle() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("rdesk.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(app.join("Contents").join("v"), "old").unwrap();
        let staged = d.path().join(".rdesk-9.9.9.update").join("rdesk.app");
        std::fs::create_dir_all(staged.join("Contents")).unwrap();
        std::fs::write(staged.join("Contents").join("v"), "new").unwrap();
        swap(&app, &staged).unwrap();
        assert_eq!(std::fs::read_to_string(app.join("Contents").join("v")).unwrap(), "new");
        let left: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("rdesk.app")]);
    }
}
