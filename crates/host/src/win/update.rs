//! Installing a released version on Windows, by the host the service runs (as SYSTEM).
//!
//! 1. Check the server's offer: `release.json` signed by an rdesk release key (built into
//!    this program), the Windows file's size and SHA-256.
//! 2. Download it into `%ProgramData%\rdesk\updates\<version>` (only SYSTEM and
//!    Administrators may write there) and unpack `rdesk.exe`.
//! 3. Wait until nobody is connected.
//! 4. Start the new program as `rdesk service apply-update`, which stops the service, swaps
//!    the program, starts it again and puts the old one back if the new one does not come
//!    up (`service::apply_update`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use rdesk_common::config;
use rdesk_common::release::{check_file, download, file_for, verify_manifest};
use rdesk_common::tf;
use rdesk_proto::api::UpdateOffer;
use rdesk_proto::release::FileKind;

use crate::control::Reporter;
use crate::server::Server;

/// `%ProgramData%\rdesk\updates\<version>`.
pub fn staging_dir(version: &str) -> Result<PathBuf> {
    Ok(config::service_dir().context("%ProgramData% is not set")?.join("updates").join(version))
}

/// Marks left by the applier: this version was installed, or failed (not tried again).
pub const FAILED: &str = "failed";
pub const INSTALLED: &str = "installed";

pub(crate) async fn install(offer: UpdateOffer, server: Arc<Server>, reporter: Reporter) {
    let version = offer.version.clone();
    match prepare(&offer).await {
        Ok(None) => {}
        Ok(Some(exe)) => {
            reporter.event(
                tf!(
                    "rdesk {version} downloaded; it installs when nobody is connected",
                    "rdesk {version} baixado; ele é instalado quando ninguém estiver conectado"
                ),
                false,
            );
            while server.in_session().await {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            reporter.event(
                tf!("Installing rdesk {version}; the host restarts", "Instalando o rdesk {version}; o host reinicia"),
                false,
            );
            if let Err(e) = launch_applier(&exe, &version) {
                reporter.event(
                    tf!(
                        "Could not install rdesk {version}: {e:#}",
                        "Não foi possível instalar o rdesk {version}: {e:#}"
                    ),
                    true,
                );
            }
        }
        Err(e) => reporter.event(
            tf!("Could not download rdesk {version}: {e:#}", "Não foi possível baixar o rdesk {version}: {e:#}"),
            true,
        ),
    }
}

/// Download and check the new program; `None` if this version already failed here.
async fn prepare(offer: &UpdateOffer) -> Result<Option<PathBuf>> {
    let dir = staging_dir(&offer.version)?;
    if dir.join(FAILED).exists() {
        tracing::info!("rdesk {} failed to start here before; not trying again", offer.version);
        return Ok(None);
    }
    let m = verify_manifest(offer.manifest.as_bytes(), &offer.signature)?;
    ensure!(m.version == offer.version, "the signed manifest is for {}, not {}", m.version, offer.version);
    // rdesk-host updates to rdesk-host, the app to the app.
    let platform = rdesk_common::release::update_platform();
    let file = file_for(&m, FileKind::App, &platform).ok_or_else(|| anyhow!("no {platform} file in {}", m.version))?;
    let data = download(&format!("{}{}", offer.download_base, file.name), file.size).await?;
    check_file(file, &data)?;

    std::fs::create_dir_all(&dir)?;
    let zip = dir.join(&file.name);
    std::fs::write(&zip, &data)?;
    // Windows' own tar unpacks zip files.
    let tar = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
        .join("System32")
        .join("tar.exe");
    let out = std::process::Command::new(tar)
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&dir)
        .output()
        .context("unpacking the update")?;
    ensure!(out.status.success(), "unpacking the update: {}", String::from_utf8_lossy(&out.stderr));
    let exe = dir.join("rdesk.exe");
    // It must say it is the version the manifest promises.
    let out = std::process::Command::new(&exe).arg("--version").output().context("running the new program")?;
    let says = String::from_utf8_lossy(&out.stdout);
    ensure!(says.trim() == format!("rdesk {}", m.version), "the new program says {:?}", says.trim());
    Ok(Some(exe))
}

/// The applier runs on its own: stopping the service stops this process.
fn launch_applier(exe: &std::path::Path, version: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(exe)
        .args(["service", "apply-update", "--version", version])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .context("starting the installer")?;
    Ok(())
}
