//! `rdesk install` / `rdesk uninstall`: rdesk as a normal Windows program. Installing is the
//! service (program in Program Files, host files in ProgramData, firewall, Start menu) plus
//! an entry in Settings → Apps, and optionally a computer that is only accessed: it asks the
//! server to add it (the owner approves it, or an enrollment key adds it at once).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use rdesk_common::config::{self, EnrollConfig};
use rdesk_common::{t, tf};

use super::service;

const UNINSTALL_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\rdesk";

#[derive(Debug, Clone, Default)]
pub struct InstallOpts {
    /// Only accessed, never used to reach others: join `server` without anyone signing in.
    pub only_share: bool,
    /// The rdesk server (default: from the program's name, `rdesk@server.exe`).
    pub server: Option<String>,
    /// Who will own it (they approve it in their app).
    pub owner: Option<String>,
    /// An enrollment key (added at once).
    pub key: Option<String>,
    /// Its name on the server (default: the computer's name).
    pub name: Option<String>,
}

fn reg(args: &[&str]) -> Result<()> {
    let out = Command::new("reg").args(args).output().context("running reg")?;
    if !out.status.success() {
        bail!("reg {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn set(name: &str, kind: &str, value: &str) -> Result<()> {
    reg(&["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", value, "/f"])
}

/// The entry in Settings → Apps (and Control Panel → Programs).
fn register_uninstall() -> Result<()> {
    let exe = service::program_exe();
    let exe_s = exe.display().to_string();
    set("DisplayName", "REG_SZ", "rdesk")?;
    set("DisplayVersion", "REG_SZ", env!("CARGO_PKG_VERSION"))?;
    set("Publisher", "REG_SZ", "rdesk")?;
    set("InstallLocation", "REG_SZ", &service::program_dir().display().to_string())?;
    set("DisplayIcon", "REG_SZ", &exe_s)?;
    set("UninstallString", "REG_SZ", &format!("\"{exe_s}\" uninstall"))?;
    set("QuietUninstallString", "REG_SZ", &format!("\"{exe_s}\" uninstall"))?;
    set("NoModify", "REG_DWORD", "1")?;
    set("NoRepair", "REG_DWORD", "1")?;
    let kb = std::fs::metadata(&exe).map(|m| m.len() / 1024).unwrap_or(0);
    set("EstimatedSize", "REG_DWORD", &kb.to_string())?;
    Ok(())
}

pub fn install(o: &InstallOpts) -> Result<String> {
    let mut msg = service::install()?;
    if o.only_share {
        let server = o
            .server
            .clone()
            .or_else(config::server_from_exe_name)
            .context("which rdesk server? (--server, or a program named rdesk@your-server.exe)")?;
        let enroll = EnrollConfig {
            server: Some(server.clone()),
            owner: o.owner.clone(),
            key: o.key.clone(),
            name: o.name.clone(),
        };
        crate::enroll::save(enroll)?;
        service::restart()?;
        msg.push_str(&tf!(
            " It asks {server} to add it{}.",
            " Ele pede a {server} para adicioná-lo{}.",
            if o.key.is_some() {
                t!(" with the enrollment key", " com a chave de cadastro")
            } else {
                t!(
                    "; its owner approves it in their app, comparing the code that rdesk (Start menu) shows here",
                    "; o dono o aprova no app dele, conferindo o código que o rdesk (menu Iniciar) mostra aqui"
                )
            }
        ));
    }
    register_uninstall()?;
    msg.push_str(t!(" Listed in Settings > Apps.", " Listado em Configurações > Aplicativos."));
    Ok(msg)
}

pub fn uninstall() -> Result<String> {
    let msg = service::uninstall()?;
    let _ = reg(&["delete", UNINSTALL_KEY, "/f"]);
    if let Some(data) = std::env::var_os("ProgramData") {
        let _ = std::fs::remove_file(PathBuf::from(data).join(r"Microsoft\Windows\Start Menu\Programs\rdesk.lnk"));
    }
    // This program may be the one in Program Files: remove the folder once it has exited.
    let dir = service::program_dir();
    if dir.exists() {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("cmd")
            .raw_arg(format!("/c timeout /t 3 /nobreak >nul & rmdir /s /q \"{}\"", dir.display()))
            .creation_flags(0x0800_0008) // CREATE_NO_WINDOW | DETACHED_PROCESS
            .spawn();
    }
    Ok(tf!("{msg} rdesk is uninstalled.", "{msg} O rdesk foi desinstalado."))
}
