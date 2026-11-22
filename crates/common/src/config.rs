//! Config files and their locations.
//!
//! Everything lives in one directory:
//! * Linux:   `~/.config/rdesk/`
//! * macOS:   `~/Library/Application Support/dev.rdesk.rdesk/`
//! * Windows: `%APPDATA%\rdesk\rdesk\config\`
//!
//! `RDESK_CONFIG_DIR` overrides it (tests, running several instances).
//!
//! The host's own files (identity, registration, `host.toml`, password, keys) belong to
//! the computer, not to whoever uses the app. They live in the same directory, except
//! when the Windows service runs the host: then in `%ProgramData%\rdesk`, readable only
//! by SYSTEM and Administrators. `RDESK_HOST_DIR` overrides it.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const HOST_CONFIG: &str = "host.toml";
pub const CLIENT_CONFIG: &str = "client.toml";
pub const AUTHORIZED_KEYS: &str = "authorized_keys";
/// The host's secret key (32 bytes); its public half is the host ID.
pub const HOST_KEY: &str = "host_id.key";
pub const OPAQUE_SETUP: &str = "opaque_server_setup.bin";
pub const OPAQUE_PASSWORD: &str = "password.opaque";

static CONFIG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Use `dir` for the config from now on (an app on a phone: its own data directory, which
/// the platform hands to it). Set once, before anything reads the config.
pub fn set_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR.set(dir);
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = CONFIG_DIR.get() {
        return Ok(dir.clone());
    }
    if let Some(dir) = std::env::var_os("RDESK_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("dev", "rdesk", "rdesk")
        .context("could not determine the user's config directory")?;
    Ok(dirs.config_dir().to_path_buf())
}

/// The host's files (see the module docs); `machine.json` is `account::MACHINE_FILE`.
pub const HOST_FILES: [&str; 6] =
    [HOST_CONFIG, AUTHORIZED_KEYS, HOST_KEY, OPAQUE_SETUP, OPAQUE_PASSWORD, "machine.json"];

/// The server in the program's own name, as the server's download page names it:
/// `rdesk@rdesk.example.com.exe` (also `… (1).exe`, a second download) → `rdesk.example.com`.
pub fn server_from_exe_name() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    server_in_name(exe.file_name()?.to_str()?)
}

fn server_in_name(name: &str) -> Option<String> {
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let (_, server) = name.split_once('@')?;
    let server = server.split(' ').next()?.trim();
    (server.contains('.') && !server.contains(['/', '\\'])).then(|| server.to_string())
}

/// Where the Windows service keeps the host's files: `%ProgramData%\rdesk`.
pub fn service_dir() -> Option<PathBuf> {
    if cfg!(windows) { std::env::var_os("ProgramData").map(|d| PathBuf::from(d).join("rdesk")) } else { None }
}

/// The host is run by the Windows service (`rdesk service install`), not by this app.
pub fn service_mode() -> bool {
    std::env::var_os("RDESK_CONFIG_DIR").is_none() && service_dir().is_some_and(|d| d.is_dir())
}

/// Where the host's files are.
pub fn host_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("RDESK_HOST_DIR") {
        return Ok(PathBuf::from(dir));
    }
    match service_dir() {
        Some(dir) if service_mode() => Ok(dir),
        _ => config_dir(),
    }
}

pub fn config_path(name: &str) -> Result<PathBuf> {
    if HOST_FILES.contains(&name) {
        return Ok(host_dir()?.join(name));
    }
    Ok(config_dir()?.join(name))
}

pub fn ensure_host_dir() -> Result<PathBuf> {
    let dir = host_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Write a file readable only by the current user (0600 on Unix; on Windows the
/// per-user profile directory's inherited ACL already restricts access).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        use std::io::Write;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Congestion {
    #[default]
    Cubic,
    Bbr,
    NewReno,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransportTuning {
    /// QUIC PING interval. Keeps NAT mappings warm and detects dead links.
    pub keep_alive_ms: u64,
    /// Connection is declared dead after this long without hearing from the peer.
    pub idle_timeout_ms: u64,
    /// Upper bound for path MTU discovery (UDP payload bytes). 1452 fits a 1500-byte link.
    pub max_udp_payload: u16,
    pub congestion: Congestion,
}

impl Default for TransportTuning {
    fn default() -> Self {
        Self { keep_alive_ms: 1000, idle_timeout_ms: 6000, max_udp_payload: 1452, congestion: Congestion::Cubic }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    /// UDP port for direct connections (LAN, port forwarding). Clients that know only the
    /// host's address use it; through a relay any port works.
    pub port: u16,
    /// Where pushed files land. Defaults to `~/Downloads/rdesk`.
    pub inbox_dir: Option<PathBuf>,
    pub max_fps: u8,
    pub jpeg_quality: u8,
    /// Share the clipboard (text) in both directions.
    pub clipboard: bool,
    /// Registered with a server: besides the server's ticket, also ask for this computer's
    /// own password or SSH key (an extra lock).
    pub require_local: bool,
    pub transport: TransportTuning,
    /// Join a server without anyone signing in on this computer.
    #[serde(skip_serializing_if = "EnrollConfig::is_empty")]
    pub enroll: EnrollConfig,
    /// Logging in with this computer's own password or key, without the server.
    pub direct: DirectConfig,
    /// Windows: full-control sessions may open a terminal as administrator (the signed-in
    /// person's elevated rights, if they are an administrator).
    pub admin_terminal: bool,
    /// Ask the person at the computer before a session starts.
    pub consent: ConsentConfig,
}

/// `[consent]` in host.toml: ask the person at the computer before each session (not for a
/// reconnect, nor when the computer is locked or nobody is signed in).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsentConfig {
    pub ask: bool,
    /// Seconds to wait for an answer.
    pub timeout_s: u32,
    /// Without an answer in time (nobody there): let the session in.
    pub allow_on_timeout: bool,
}

impl Default for ConsentConfig {
    fn default() -> Self {
        Self { ask: false, timeout_s: 30, allow_on_timeout: true }
    }
}

/// `[direct]` in host.toml: this computer's own password or SSH key open it without the
/// server's permission, only for connections from the local network or Tailscale, never
/// through the relay. Other addresses are turned away before any login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectConfig {
    /// `None`: on while the computer is not registered with a server, off once it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Where such connections may come from: `"lan"` (private, link-local and loopback
    /// addresses) and `"tailscale"` (100.64.0.0/10, fd7a:115c:a1e0::/48).
    pub from: Vec<String>,
    /// Announce this computer on the local network (mDNS), so others connect with its
    /// host ID alone. Anyone on the network then sees that an rdesk host is here.
    pub mdns: bool,
}

impl Default for DirectConfig {
    fn default() -> Self {
        Self { enabled: None, from: vec!["lan".into(), "tailscale".into()], mdns: false }
    }
}

impl DirectConfig {
    pub fn is_on(&self, registered: bool) -> bool {
        self.enabled.unwrap_or(!registered)
    }

    /// May a direct connection from `ip` log in this way?
    pub fn allows(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.from.iter().any(|class| match class.as_str() {
            "lan" => is_lan(ip),
            "tailscale" => is_tailscale(ip),
            _ => false,
        })
    }
}

/// Private, link-local or loopback: reachable only from the local network.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        // fc00::/7 (unique local), fe80::/10 (link-local).
        IpAddr::V6(v6) => {
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Tailscale's address ranges.
pub fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// `[enroll]` in host.toml: how an unregistered host asks a server to add it. The
/// `RDESK_SERVER`, `RDESK_OWNER`, `RDESK_ENROLL_KEY` and `RDESK_NAME` environment variables
/// take precedence (handy for ready-made images and services).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrollConfig {
    pub server: Option<String>,
    /// E-mail of who should own it: they approve it in the app.
    pub owner: Option<String>,
    /// An enrollment key: added at once (removed from here once used).
    pub key: Option<String>,
    /// Defaults to the computer's name.
    pub name: Option<String>,
}

impl EnrollConfig {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            port: rdesk_proto::DEFAULT_PORT,
            inbox_dir: None,
            max_fps: 30,
            jpeg_quality: 70,
            clipboard: true,
            require_local: false,
            transport: TransportTuning::default(),
            enroll: EnrollConfig::default(),
            direct: DirectConfig::default(),
            admin_terminal: true,
            consent: ConsentConfig::default(),
        }
    }
}

impl HostConfig {
    pub fn inbox_dir(&self) -> PathBuf {
        if let Some(d) = &self.inbox_dir {
            return d.clone();
        }
        // A phone: its shared Download folder, where any app may put files.
        if let Some(home) = crate::files::phone_home() {
            return home.join("Download").join("rdesk");
        }
        directories::UserDirs::new()
            .and_then(|u| u.download_dir().map(Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rdesk")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthPref {
    /// Try the SSH agent first, then the password.
    #[default]
    Auto,
    Agent,
    Password,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HostEntry {
    /// The host's ID (`rdesk host id` on that machine). Connections are authenticated
    /// against it, so it is also what makes the host trusted.
    pub id: String,
    /// Saved on this device to reach without the server (local network or Tailscale): the
    /// name shown in the list. Empty for entries that only hold login options.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Optional direct address, `host:port` or `host` (default port): LAN or port forwarding.
    /// Not needed when a relay is configured.
    pub addr: String,
    pub auth: AuthPref,
    /// SHA256 fingerprint (as printed by `rdesk agent-keys`) of the agent key to use.
    pub key: Option<String>,
    /// 1Password secret reference for the password, e.g. `op://Private/rdesk/password`.
    pub password_ref: Option<String>,
    /// Override the SSH agent socket / pipe.
    pub agent_socket: Option<String>,
    /// Use an unencrypted OpenSSH private key file instead of the agent.
    pub identity: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ClientConfig {
    pub hosts: BTreeMap<String, HostEntry>,
    pub transport: TransportTuning,
    /// Swap Cmd and Ctrl when the client is a Mac (so Cmd+C sends Ctrl+C).
    pub swap_cmd_ctrl: bool,
    /// Desktop app behaviour.
    pub app: AppSettings,
    /// The rdesk server last signed in to (pre-filled on the sign-in page).
    pub server: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Let others connect to this computer while the app runs (the built-in host).
    pub host_enabled: bool,
    /// The window's close button hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Start hidden in the tray.
    pub start_minimized: bool,
    /// Used without a server: only computers on this network (chosen on the sign-in page).
    pub local_only: bool,
    /// The app's language (English or Brazilian Portuguese; the system's by default).
    pub language: crate::i18n::LanguagePref,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            host_enabled: false,
            close_to_tray: true,
            start_minimized: false,
            local_only: false,
            language: Default::default(),
        }
    }
}

/// How this computer reaches others: the relay of the rdesk server it is signed in to,
/// and the session token the relay asks for. Without a session there is no relay, and
/// computers are reached by address only.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NetworkConfig {
    pub relay: Option<String>,
    pub token: Option<String>,
}

/// For connecting to others: the account in use.
pub fn load_network() -> NetworkConfig {
    crate::account::Session::load().map(|s| s.network()).unwrap_or_default()
}

/// For the host: this computer's own registration (works with nobody signed in).
pub fn host_network() -> NetworkConfig {
    match crate::account::Registration::load() {
        Some(r) => NetworkConfig { relay: Some(r.relay_url), token: Some(r.token) },
        None => NetworkConfig::default(),
    }
}

pub fn load_toml<T: for<'de> Deserialize<'de> + Default>(name: &str) -> Result<T> {
    let path = config_path(name)?;
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save_toml<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let path = config_path(name)?;
    let s = toml::to_string_pretty(value)?;
    write_private(&path, s.as_bytes())
}

/// Host IDs are shown as 64 lowercase hex characters.
pub fn format_host_id(id: &[u8; 32]) -> String {
    hex::encode(id)
}

/// Accepts the hex form (spaces and colons ignored) or iroh's base32 form.
pub fn parse_host_id(s: &str) -> Result<[u8; 32]> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace() && *c != ':').collect();
    let clean = if clean.len() == 64 { clean.to_ascii_lowercase() } else { clean };
    let key: iroh::EndpointId =
        clean.parse().map_err(|_| anyhow::anyhow!("{s:?} is not a host ID (64 hex characters)"))?;
    Ok(*key.as_bytes())
}

#[cfg(test)]
mod tests {

    #[test]
    fn server_from_the_program_name() {
        assert_eq!(server_in_name("rdesk@rdesk.example.com.exe").as_deref(), Some("rdesk.example.com"));
        assert_eq!(server_in_name("rdesk@rdesk.example.com (1).exe").as_deref(), Some("rdesk.example.com"));
        assert_eq!(server_in_name("rdesk@rdesk.example.com").as_deref(), Some("rdesk.example.com"));
        assert_eq!(server_in_name("rdesk.exe"), None);
        assert_eq!(server_in_name("rdesk@local.exe"), None);
    }

    #[test]
    fn direct_sources() {
        let d = DirectConfig::default();
        let ok = |a: &str| d.allows(a.parse().unwrap());
        for a in ["192.168.0.10", "10.1.2.3", "172.20.0.1", "169.254.1.1", "127.0.0.1", "::1", "fe80::1", "fd00::5"] {
            assert!(ok(a), "{a} is local");
        }
        for a in ["100.97.96.81", "100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::1", "::ffff:192.168.1.2"] {
            assert!(ok(a), "{a} is allowed");
        }
        for a in ["8.8.8.8", "100.128.0.1", "100.63.255.255", "2001:db8::1", "2804:14c::1", "::ffff:8.8.8.8"] {
            assert!(!ok(a), "{a} is the internet");
        }
        let lan_only = DirectConfig { from: vec!["lan".into()], ..Default::default() };
        assert!(!lan_only.allows("100.97.96.81".parse().unwrap()));
        assert!(DirectConfig::default().is_on(false) && !DirectConfig::default().is_on(true));
        let forced = DirectConfig { enabled: Some(true), ..Default::default() };
        assert!(forced.is_on(true));
    }

    use super::*;

    #[test]
    fn host_id_forms() {
        let id = *iroh::SecretKey::generate().public().as_bytes();
        let s = format_host_id(&id);
        assert_eq!(s.len(), 64);
        assert_eq!(parse_host_id(&s).unwrap(), id);
        assert_eq!(parse_host_id(&format!("  {} ", s.to_uppercase())).unwrap(), id);
        assert!(parse_host_id("SHA256:abcd").is_err());
        assert!(parse_host_id("").is_err());
    }

    #[test]
    fn host_config_defaults_parse() {
        let c: HostConfig = toml::from_str("port = 9000\n[transport]\ncongestion = \"bbr\"\n").unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.transport.congestion, Congestion::Bbr);
        assert_eq!(c.transport.idle_timeout_ms, 6000);
        // Settings of older versions are ignored, not an error.
        let old: HostConfig = toml::from_str("listen = [\"tailscale\"]\nallow_non_tailnet = true\n").unwrap();
        assert_eq!(old.port, rdesk_proto::DEFAULT_PORT);
    }
}
