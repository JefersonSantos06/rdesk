//! The app around the sessions: start-up, signing in to the rdesk server, the computer
//! list, computers saved on the phone (no server), and updates (Android).

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use flutter_rust_bridge::frb;
use rdesk_common::account::{self, PendingLogin, Session};
use rdesk_common::config::{self, ClientConfig, HostEntry};

use crate::runtime::run;

/// Called once by the generated Dart before anything else.
#[frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

/// Where the app keeps its files and how it is named; call once at start.
/// `data_dir`: the app's private directory; `device_name`: the phone's name (shown to the
/// people at the computer); `portuguese`: the phone speaks Portuguese.
pub fn start(data_dir: String, device_name: String, portuguese: bool) -> Result<()> {
    let dir = PathBuf::from(data_dir);
    config::set_config_dir(dir.clone());
    config::ensure_config_dir()?;
    rdesk_common::set_device_name(device_name);
    rdesk_common::i18n::set(if portuguese { rdesk_common::i18n::Lang::Pt } else { rdesk_common::i18n::Lang::En });
    account::install_crypto();
    start_log(&dir);
    tracing::info!("rdesk {} started", env!("CARGO_PKG_VERSION"));
    Ok(())
}

/// Log to `logs/app.log` in the app's directory (kept small: rewritten at each start).
fn start_log(dir: &std::path::Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let logs = dir.join("logs");
        let _ = std::fs::create_dir_all(&logs);
        let Ok(file) = std::fs::File::create(logs.join("app.log")) else { return };
        let filter = tracing_subscriber::EnvFilter::try_new("info,iroh=warn,noq=warn,portmapper=warn")
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let _ =
            tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(Mutex::new(file)).try_init();
    });
}

/// The app's version (`MAJOR.MINOR.PATCH`).
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ---- account ---------------------------------------------------------------------------

/// Who is signed in, and to which server.
#[derive(Debug, Clone)]
pub struct Account {
    pub server: String,
    pub server_name: String,
    pub email: String,
    pub name: String,
    pub is_admin: bool,
    pub days_left: i64,
}

fn account_of(s: &Session) -> Account {
    Account {
        server: s.server.clone(),
        server_name: s.server_name.clone(),
        email: s.user.email.clone(),
        name: s.user.name.clone(),
        is_admin: s.user.is_admin,
        days_left: s.days_left(),
    }
}

/// The saved sign-in, if it has not expired.
pub fn account() -> Option<Account> {
    Session::load().map(|s| account_of(&s))
}

/// The server last signed in to (to pre-fill the sign-in page).
pub fn last_server() -> Option<String> {
    config::load_toml::<ClientConfig>(config::CLIENT_CONFIG).ok()?.server
}

/// Used without a server (chosen on the sign-in page): only computers saved on the phone.
pub fn local_only() -> bool {
    config::load_toml::<ClientConfig>(config::CLIENT_CONFIG).map(|c| c.app.local_only).unwrap_or(false)
}

pub fn set_local_only(enabled: bool) -> Result<()> {
    let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    cfg.app.local_only = enabled;
    config::save_toml(config::CLIENT_CONFIG, &cfg)
}

static PENDING: Mutex<Option<PendingLogin>> = Mutex::new(None);

/// Start signing in to `server`: returns the address to open in the browser. The browser
/// comes back to `rdesk://callback?…`; hand that to [`finish_sign_in`].
pub async fn begin_sign_in(server: String) -> Result<String> {
    let device = rdesk_common::local_hostname();
    let pending = run(async move { account::begin_login(&server, &device, account::APP_REDIRECT).await }).await?;
    let url = pending.url().to_string();
    *PENDING.lock().unwrap() = Some(pending);
    Ok(url)
}

/// Finish the sign-in with the address the browser came back with.
pub async fn finish_sign_in(callback_url: String) -> Result<Account> {
    let pending = PENDING.lock().unwrap().take().context("no sign-in is waiting: start again")?;
    let session = run(async move { pending.complete_from_callback(&callback_url).await }).await?;
    let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
    cfg.server = Some(session.server.clone());
    cfg.app.local_only = false;
    let _ = config::save_toml(config::CLIENT_CONFIG, &cfg);
    Ok(account_of(&session))
}

/// Sign out here and on the server (the server part may fail when offline; the phone
/// forgets the sign-in anyway).
pub async fn sign_out() -> Result<()> {
    if let Some(s) = Session::load() {
        let _ = run(async move { s.api()?.logout().await }).await;
    }
    Session::clear()
}

// ---- computers -----------------------------------------------------------------------

/// A computer on the server that this account may see.
#[derive(Debug, Clone)]
pub struct Computer {
    pub machine_id: i64,
    /// Host ID (hex).
    pub host_id: String,
    pub name: String,
    pub os: String,
    pub owner: String,
    pub online: bool,
    pub in_session: bool,
    /// Unix seconds, when it was last online.
    pub last_seen: Option<i64>,
    pub can_access: bool,
    pub can_see: bool,
    pub can_control: bool,
    pub can_files: bool,
    pub can_terminal: bool,
    pub is_mine: bool,
    /// Addresses it reported (tried before the relay).
    pub addrs: Vec<String>,
    /// It also takes its own password or key on the local network.
    pub direct: bool,
}

/// The computers on the server (signed in only).
pub async fn computers() -> Result<Vec<Computer>> {
    let s = Session::load().ok_or_else(|| anyhow!(rdesk_common::t!("Not signed in.", "Você não entrou.")))?;
    let list = run(async move { s.api()?.machines().await }).await?;
    let mut out: Vec<Computer> = list
        .into_iter()
        .map(|m| Computer {
            machine_id: m.id,
            host_id: m.endpoint_id,
            name: m.name,
            os: m.os,
            owner: if m.owner.name.is_empty() { m.owner.email } else { m.owner.name },
            online: m.online,
            in_session: m.in_session,
            last_seen: m.last_seen,
            can_access: m.can_access,
            can_see: m.access.screen,
            can_control: m.access.input,
            can_files: m.access.files,
            can_terminal: m.access.terminal,
            is_mine: m.is_owner,
            addrs: m.addrs,
            direct: m.direct,
        })
        .collect();
    out.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

/// A computer saved on this phone, reached without the server (local network, Tailscale).
#[derive(Debug, Clone)]
pub struct SavedComputer {
    pub host_id: String,
    pub name: String,
    /// IP or name; empty: found on the local network (mDNS).
    pub addr: String,
}

pub fn saved_computers() -> Result<Vec<SavedComputer>> {
    let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    let mut out: Vec<SavedComputer> = cfg
        .hosts
        .values()
        .filter(|e| !e.name.is_empty())
        .map(|e| SavedComputer { host_id: e.id.clone(), name: e.name.clone(), addr: e.addr.clone() })
        .collect();
    out.sort_by_key(|c| c.name.to_lowercase());
    Ok(out)
}

/// Save (or update) a computer on this phone. `host_id`: as `rdesk host id` shows it.
pub fn save_computer(name: String, host_id: String, addr: String) -> Result<SavedComputer> {
    let id = config::parse_host_id(&host_id)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        bail!(rdesk_common::t!("Give it a name.", "Dê um nome."));
    }
    let key = config::format_host_id(&id);
    let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    let e = cfg.hosts.entry(key.clone()).or_default();
    e.id = key.clone();
    e.name = name.clone();
    e.addr = addr.trim().to_string();
    config::save_toml(config::CLIENT_CONFIG, &cfg)?;
    Ok(SavedComputer { host_id: key, name, addr: addr.trim().to_string() })
}

pub fn forget_computer(host_id: String) -> Result<()> {
    let id = config::parse_host_id(&host_id)?;
    let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    if let Some(e) = cfg.hosts.get_mut(&config::format_host_id(&id)) {
        // Sessions remember a password under the name shown.
        let _ = rdesk_core::secrets::forget(&e.name);
        // Keep its login options only if any are set; otherwise drop the entry.
        e.name.clear();
        e.addr.clear();
        if *e == (HostEntry { id: e.id.clone(), ..Default::default() }) {
            cfg.hosts.remove(&config::format_host_id(&id));
        }
    }
    config::save_toml(config::CLIENT_CONFIG, &cfg)
}

/// Forget the password remembered for a computer (by its name, as sessions save it).
pub fn forget_password(name: String) -> Result<()> {
    rdesk_core::secrets::forget(&name)
}

pub fn has_password(name: String) -> bool {
    rdesk_core::secrets::has(&name)
}

// ---- updates (Android) ---------------------------------------------------------------

/// A newer version released on the server.
#[derive(Debug, Clone)]
pub struct Update {
    pub version: String,
    pub size: i64,
}

async fn offer() -> Result<Option<(rdesk_proto::release::ReleaseFile, String, String)>> {
    let Some(server) = Session::load().map(|s| s.server).or_else(last_server) else { return Ok(None) };
    let url = format!("{}{}", account::normalize_server(&server)?, rdesk_proto::api::PATH_UPDATE_LATEST);
    let offer: Option<rdesk_proto::api::UpdateOffer> = account::http_client()
        .timeout(std::time::Duration::from_secs(20))
        .build()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(offer) = offer else { return Ok(None) };
    let manifest = rdesk_common::release::verify_manifest(offer.manifest.as_bytes(), &offer.signature)?;
    let newer = rdesk_proto::release::parse_version(&manifest.version)
        > rdesk_proto::release::parse_version(env!("CARGO_PKG_VERSION"));
    let file =
        rdesk_common::release::file_for(&manifest, rdesk_proto::release::FileKind::App, rdesk_proto::release::ANDROID);
    Ok(match file {
        Some(f) if newer => Some((f.clone(), offer.download_base, manifest.version)),
        _ => None,
    })
}

/// The version released on the server, if newer than this app and it has an APK.
pub async fn check_update() -> Result<Option<Update>> {
    let o = run(offer()).await?;
    Ok(o.map(|(f, _, version)| Update { version, size: f.size as i64 }))
}

/// Download the released APK into `dir` and check it against the signed release manifest;
/// returns the file to hand to Android's installer.
pub async fn download_update(dir: String) -> Result<String> {
    run(async move {
        let (file, base, _) = offer().await?.context("no newer version")?;
        let data = rdesk_common::release::download(&format!("{base}{}", file.name), file.size).await?;
        rdesk_common::release::check_file(&file, &data)?;
        let path = PathBuf::from(dir).join(&file.name);
        std::fs::write(&path, data)?;
        Ok(path.display().to_string())
    })
    .await
}
