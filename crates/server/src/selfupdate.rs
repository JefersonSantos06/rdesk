//! Updating the server itself, by an admin's click. The server runs as an unprivileged user
//! and may not replace its own program, so it only prepares: it downloads the release's
//! server file into `<data>/update/` and asks for it with `<data>/update/request`. A root
//! systemd unit (`deploy/rdesk-server-update.path`) notices the request and runs the
//! program already installed — trusted, root-owned — as `rdesk-server apply-update`, which
//! checks the signature and the hash again itself, backs up the database, swaps the program,
//! restarts the service, and puts both back if the new version does not answer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rdesk_proto::release::{FileKind, ReleaseFile, ReleaseManifest, parse_version};
use sha2::{Digest, Sha256};

use crate::config::{self, ServerConfig};
use crate::db::Db;
use crate::state::Shared;

pub const UPDATE_DIR: &str = "update";
const REQUEST: &str = "request";
const MANIFEST: &str = "release.json";
const SIGNATURE: &str = "release.json.sig";
/// What the last update did, for the admin page (written by the applier).
pub const RESULT: &str = "result";

/// `linux-x86_64`, `linux-aarch64`…
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn server_file(m: &ReleaseManifest) -> Result<&ReleaseFile> {
    let p = platform();
    m.files
        .iter()
        .find(|f| f.kind == FileKind::Server && f.platform == p)
        .ok_or_else(|| anyhow!("{} has no server for {p}", m.version))
}

fn check_hash(f: &ReleaseFile, data: &[u8]) -> Result<()> {
    ensure!(data.len() as u64 == f.size, "{}: size differs from the signed manifest", f.name);
    ensure!(hex::encode(Sha256::digest(data)) == f.sha256, "{}: SHA-256 differs from the signed manifest", f.name);
    Ok(())
}

/// Can this server update itself (runs under systemd, not in a container)?
pub fn supported() -> bool {
    cfg!(target_os = "linux") && !Path::new("/.dockerenv").exists() && std::env::var_os("INVOCATION_ID").is_some()
}

pub fn last_result(data: &Path) -> Option<String> {
    std::fs::read_to_string(data.join(UPDATE_DIR).join(RESULT)).ok().map(|s| s.trim().to_string())
}

/// The server's part: download `version`'s server file and ask the root unit to install it.
pub async fn prepare(st: &Shared, data: &Path, version: &str) -> Result<()> {
    ensure!(supported(), "this server can not update itself (only under systemd, outside containers)");
    ensure!(
        parse_version(version) > parse_version(env!("CARGO_PKG_VERSION")),
        "the server already runs {}",
        env!("CARGO_PKG_VERSION")
    );
    let row = st.db.release(version)?.ok_or_else(|| anyhow!("no such version"))?;
    let m = crate::updates::verify(&row.manifest, &row.signature)?;
    let f = server_file(&m)?;
    let url = format!("https://github.com/{}/releases/download/v{version}/{}", st.cfg.update_repo, f.name);
    let resp =
        st.http.get(&url).timeout(Duration::from_secs(600)).send().await.with_context(|| format!("GET {url}"))?;
    ensure!(resp.status().is_success(), "GET {url}: HTTP {}", resp.status());
    let data_bytes = resp.bytes().await?;
    check_hash(f, &data_bytes)?;

    let dir = data.join(UPDATE_DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(&f.name), &data_bytes)?;
    std::fs::write(dir.join(MANIFEST), &row.manifest)?;
    std::fs::write(dir.join(SIGNATURE), &row.signature)?;
    std::fs::write(dir.join(RESULT), format!("installing {version}…"))?;
    // Last: this file is what the root unit waits for.
    std::fs::write(dir.join(REQUEST), version)?;
    Ok(())
}

// ---- the root side ---------------------------------------------------------------------

fn run(cmd: &mut std::process::Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    ensure!(out.status.success(), "{cmd:?}: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

fn systemctl(action: &str) -> Result<()> {
    run(std::process::Command::new("systemctl").args([action, "rdesk-server"]))
}

async fn answers(cfg: &ServerConfig, version: &str) -> bool {
    // The server's own address when it is plain HTTP (behind a proxy), else its public URL.
    let url = if cfg.tls == config::Tls::None {
        format!("http://{}/api/info", cfg.listen)
    } else {
        format!("{}/api/info", cfg.public_url.trim_end_matches('/'))
    };
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    for _ in 0..30 {
        if let Ok(r) = client.get(&url).send().await
            && let Ok(info) = r.json::<rdesk_proto::api::ServerInfo>().await
            && info.version == version
        {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

/// `rdesk-server apply-update`, run as root by `rdesk-server-update.service` from the
/// installed program. Nothing it reads from the (server-writable) update folder is trusted
/// without the signature and the hash.
pub async fn apply(data: &Path) -> Result<String> {
    let dir = data.join(UPDATE_DIR);
    let Ok(version) = std::fs::read_to_string(dir.join(REQUEST)) else {
        return Ok("nothing to do".into());
    };
    let version = version.trim().to_string();
    let _ = std::fs::remove_file(dir.join(REQUEST));
    let result = apply_version(data, &dir, &version).await;
    let text = match &result {
        Ok(()) => format!("updated to {version}"),
        Err(e) => format!("update to {version} failed: {e:#}"),
    };
    let _ = std::fs::write(dir.join(RESULT), &text);
    result.map(|_| text)
}

async fn apply_version(data: &Path, dir: &Path, version: &str) -> Result<()> {
    let manifest = std::fs::read_to_string(dir.join(MANIFEST))?;
    let signature = std::fs::read_to_string(dir.join(SIGNATURE))?;
    let m = crate::updates::verify(&manifest, &signature)?;
    ensure!(m.version == version, "the signed manifest is for {}, not {version}", m.version);
    let f = server_file(&m)?;
    let tarball = std::fs::read(dir.join(&f.name))?;
    check_hash(f, &tarball)?;

    // Unpacked by root, into a folder only root can write.
    let work = PathBuf::from(format!("/root/rdesk-server-update-{version}"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    std::fs::write(work.join(&f.name), &tarball)?;
    run(std::process::Command::new("tar").arg("-xzf").arg(work.join(&f.name)).arg("-C").arg(&work))?;
    let new = work.join("rdesk-server");
    let out = std::process::Command::new(&new).arg("--version").output()?;
    ensure!(
        String::from_utf8_lossy(&out.stdout).trim() == format!("rdesk-server {version}"),
        "the new program does not say it is {version}"
    );

    let cfg = ServerConfig::load(data)?;
    let installed = std::env::current_exe()?;
    let old = installed.with_extension("old");
    let backup = data.join("backups").join(format!("rdesk-before-{version}-{}.db", unix_now()));
    std::fs::create_dir_all(backup.parent().unwrap())?;
    Db::open(&data.join(config::DB_FILE))?.backup(&backup)?;
    run(std::process::Command::new("chown").arg("-R").arg("rdesk:rdesk").arg(data.join("backups")))?;

    systemctl("stop")?;
    std::fs::copy(&installed, &old)?;
    // Replace by renaming a copy next to it (the running program file is not overwritten).
    let staged = installed.with_extension("new");
    std::fs::copy(&new, &staged)?;
    std::fs::rename(&staged, &installed)?;
    systemctl("start")?;
    if answers(&cfg, version).await {
        let _ = std::fs::remove_dir_all(&work);
        return Ok(());
    }

    // Back to the previous program and database.
    systemctl("stop")?;
    std::fs::rename(&old, &installed)?;
    let db = data.join(config::DB_FILE);
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{ext}", db.display())));
    }
    std::fs::copy(&backup, &db)?;
    run(std::process::Command::new("chown").arg("rdesk:rdesk").arg(&db))?;
    systemctl("start")?;
    bail!("version {version} did not answer; the previous program and database are back")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
