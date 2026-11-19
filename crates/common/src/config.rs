//! Config files and their locations.
//!
//! Everything lives in one directory:
//! * Linux:   `~/.config/rdesk/`
//! * macOS:   `~/Library/Application Support/dev.rdesk.rdesk/`
//! * Windows: `%APPDATA%\rdesk\rdesk\config\`
//!
//! `RDESK_CONFIG_DIR` overrides it (tests, running several instances).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const HOST_CONFIG: &str = "host.toml";
pub const CLIENT_CONFIG: &str = "client.toml";
pub const AUTHORIZED_KEYS: &str = "authorized_keys";
pub const KNOWN_HOSTS: &str = "known_hosts";
pub const HOST_KEY: &str = "host_key.der";
pub const HOST_CERT: &str = "host_cert.der";
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

pub fn config_path(name: &str) -> Result<PathBuf> {
    Ok(config_dir()?.join(name))
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
    /// QUIC PING interval. Keeps NAT/Tailscale paths warm and detects dead links.
    pub keep_alive_ms: u64,
    /// Connection is declared dead after this long without hearing from the peer.
    pub idle_timeout_ms: u64,
    /// Largest UDP payload we will send. 1232 fits Tailscale's 1280-byte MTU.
    pub max_udp_payload: u16,
    pub congestion: Congestion,
}

impl Default for TransportTuning {
    fn default() -> Self {
        Self { keep_alive_ms: 1000, idle_timeout_ms: 6000, max_udp_payload: 1232, congestion: Congestion::Cubic }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    /// Addresses to listen on. `"tailscale"` means "every address `tailscale ip` reports".
    pub listen: Vec<String>,
    pub port: u16,
    /// Accept peers outside the Tailscale CGNAT/ULA ranges (e.g. LAN or loopback testing).
    pub allow_non_tailnet: bool,
    /// Where pushed files land. Defaults to `~/Downloads/rdesk`.
    pub inbox_dir: Option<PathBuf>,
    pub max_fps: u8,
    pub jpeg_quality: u8,
    /// Share the clipboard (text) in both directions.
    pub clipboard: bool,
    pub transport: TransportTuning,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            listen: vec!["tailscale".into()],
            port: rdesk_proto::DEFAULT_PORT,
            allow_non_tailnet: false,
            inbox_dir: None,
            max_fps: 30,
            jpeg_quality: 70,
            clipboard: true,
            transport: TransportTuning::default(),
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
    /// `host:port` or `host` (default port). MagicDNS names work.
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

/// `known_hosts`: one `<name> <sha256-hex>` per line.
#[derive(Debug, Default, Clone)]
pub struct KnownHosts {
    pub entries: BTreeMap<String, [u8; 32]>,
}

impl KnownHosts {
    pub fn load() -> Result<Self> {
        let path = config_path(KNOWN_HOSTS)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let mut entries = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(name), Some(fp)) = (parts.next(), parts.next()) else {
                bail!("{}:{}: expected `<name> <fingerprint>`", path.display(), i + 1);
            };
            entries.insert(name.to_string(), parse_fingerprint(fp)?);
        }
        Ok(Self { entries })
    }

    pub fn save(&self) -> Result<()> {
        let mut out = String::from("# rdesk known hosts: <name> <host certificate SHA-256>\n");
        for (name, fp) in &self.entries {
            out.push_str(&format!("{name} {}\n", format_fingerprint(fp)));
        }
        write_private(&config_path(KNOWN_HOSTS)?, out.as_bytes())
    }
}

/// Fingerprints are printed as `SHA256:` + lowercase hex.
pub fn format_fingerprint(fp: &[u8; 32]) -> String {
    format!("SHA256:{}", hex::encode(fp))
}

pub fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    let hexpart = s.strip_prefix("SHA256:").unwrap_or(s).replace(':', "");
    let bytes = hex::decode(hexpart.trim()).context("fingerprint is not hex")?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("fingerprint must be 32 bytes (SHA-256)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_roundtrip() {
        let fp = [0xab; 32];
        let s = format_fingerprint(&fp);
        assert_eq!(parse_fingerprint(&s).unwrap(), fp);
        assert_eq!(parse_fingerprint(&hex::encode(fp)).unwrap(), fp);
        assert!(parse_fingerprint("SHA256:abcd").is_err());
    }

    #[test]
    fn host_config_defaults_parse() {
        let c: HostConfig = toml::from_str("port = 9000\n[transport]\ncongestion = \"bbr\"\n").unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.transport.congestion, Congestion::Bbr);
        assert_eq!(c.transport.idle_timeout_ms, 6000);
        assert_eq!(c.listen, vec!["tailscale".to_string()]);
    }
}
