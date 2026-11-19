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

pub fn config_dir() -> Result<PathBuf> {
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
        }
    }
}

impl HostConfig {
    pub fn inbox_dir(&self) -> PathBuf {
        if let Some(d) = &self.inbox_dir {
            return d.clone();
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
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { host_enabled: false, close_to_tray: true, start_minimized: false }
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

/// For connecting to others: the signed-in user's session.
pub fn load_network() -> NetworkConfig {
    match crate::account::Session::load() {
        Some(s) => NetworkConfig { relay: Some(s.relay_url), token: Some(s.token) },
        None => NetworkConfig::default(),
    }
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
