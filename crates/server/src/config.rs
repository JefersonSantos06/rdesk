//! `server.toml` and the data directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "server.toml";
pub const DB_FILE: &str = "rdesk.db";
/// Ed25519 secret key the server signs access tickets with.
pub const KEY_FILE: &str = "server.key";
pub const ACME_DIR: &str = "acme";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tls {
    /// Certificates from Let's Encrypt for the host of `public_url` (TLS-ALPN-01 on port 443).
    #[default]
    Acme,
    /// Plain HTTP: only for tests on this machine, or behind a proxy that does TLS.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthApp {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Shown on the login page and in the app.
    pub name: String,
    /// How the app and the browser reach this server, e.g. `https://rdesk.example.com`.
    pub public_url: String,
    /// TCP address for HTTPS (or HTTP with `tls = "none"`).
    pub listen: String,
    pub tls: Tls,
    /// Contact for Let's Encrypt (`admin@example.com`).
    pub acme_contact: Option<String>,
    /// Use Let's Encrypt's staging CA (for trying the setup without rate limits).
    pub acme_staging: bool,
    /// UDP port for the relay's QUIC address discovery (helps hole punching); 0 = off.
    /// Needs TLS: Let's Encrypt (`tls = "acme"`), or `tls_cert` and `tls_key`.
    pub relay_quic_port: u16,
    /// Behind a reverse proxy (`tls = "none"`): the proxy's certificate for this host
    /// (PEM files, e.g. Caddy's Let's Encrypt ones), used only by the relay's QUIC.
    /// Read again after renewals.
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
    pub google: Option<OAuthApp>,
    pub github: Option<OAuthApp>,
    /// Where new rdesk versions are published (GitHub `owner/repo`); empty: do not look.
    pub update_repo: String,
    /// Log in by typing an allowed e-mail, without Google/GitHub (2FA still required).
    /// Only honoured with `tls = "none"` and a loopback `public_url`: for tests.
    pub dev_login: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "rdesk".into(),
            public_url: String::new(),
            listen: "0.0.0.0:443".into(),
            tls: Tls::Acme,
            acme_contact: None,
            acme_staging: false,
            relay_quic_port: 7842,
            tls_cert: None,
            tls_key: None,
            update_repo: "JefersonSantos06/rdesk".into(),
            google: None,
            github: None,
            dev_login: false,
        }
    }
}

impl ServerConfig {
    /// The relay answers QUIC address discovery (it has a certificate for it).
    pub fn relay_quic(&self) -> bool {
        self.relay_quic_port != 0 && (self.tls == Tls::Acme || (self.tls_cert.is_some() && self.tls_key.is_some()))
    }

    pub fn load(data: &Path) -> Result<Self> {
        let path = data.join(CONFIG_FILE);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} (run `rdesk-server init` first)", path.display()))?;
        let cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, data: &Path) -> Result<()> {
        write_private(&data.join(CONFIG_FILE), toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn public_url(&self) -> Result<url::Url> {
        let u: url::Url = self.public_url.parse().with_context(|| format!("public_url {:?}", self.public_url))?;
        if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() {
            bail!("public_url must be http(s)://host[:port]");
        }
        Ok(u)
    }

    /// The browser and the app talk HTTPS (cookies get `Secure`).
    pub fn is_https(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    pub fn is_loopback(&self) -> bool {
        self.public_url()
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .is_some_and(|h| matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1"))
    }

    /// `dev_login` is ignored unless this is a local test server.
    pub fn dev_login_enabled(&self) -> bool {
        self.dev_login && self.tls == Tls::None && self.is_loopback()
    }

    pub fn validate(&self) -> Result<()> {
        let u = self.public_url()?;
        if self.tls == Tls::Acme {
            if u.scheme() != "https" {
                bail!("with tls = \"acme\" the public_url must be https://");
            }
            if self.acme_contact.as_deref().unwrap_or_default().trim().is_empty() {
                bail!("set acme_contact (an e-mail for Let's Encrypt)");
            }
        }
        if self.dev_login && !self.dev_login_enabled() {
            tracing::warn!("dev_login is ignored: it needs tls = \"none\" and a localhost public_url");
        }
        Ok(())
    }

    pub fn providers(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.google.is_some() {
            v.push("google".to_string());
        }
        if self.github.is_some() {
            v.push("github".to_string());
        }
        if self.dev_login_enabled() {
            v.push("dev".to_string());
        }
        v
    }
}

pub fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RDESK_SERVER_DATA") {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        let base = std::env::var_os("PROGRAMDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("rdesk-server")
    } else {
        PathBuf::from("/var/lib/rdesk-server")
    }
}

/// Write a file only the current user can read (0600 on Unix).
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

/// The key before the last rotation (public half) and when it was replaced.
pub const PREV_KEY_FILE: &str = "server.key.prev";

/// Replace the ticket-signing key. The old public key stays trusted by hosts for 12 h
/// (the longest a ticket lasts), so nobody is cut off. Restart the server afterwards.
pub fn rotate_key(data: &Path) -> Result<iroh_base::PublicKey> {
    let old = load_or_create_key(data)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    write_private(&data.join(PREV_KEY_FILE), format!("{} {stamp}\n", hex::encode(old.public().as_bytes())).as_bytes())?;
    let new = iroh_base::SecretKey::generate();
    write_private(&data.join(KEY_FILE), &new.to_bytes())?;
    Ok(new.public())
}

/// The previous key, while its tickets may still be around.
pub fn load_prev_key(data: &Path) -> Result<Option<iroh_base::PublicKey>> {
    let Ok(text) = std::fs::read_to_string(data.join(PREV_KEY_FILE)) else { return Ok(None) };
    let mut it = text.split_whitespace();
    let (Some(k), Some(t)) = (it.next(), it.next()) else { return Ok(None) };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let at: i64 = t.parse().unwrap_or(0);
    if now - at > rdesk_proto::ticket::TICKET_MAX_SECS + rdesk_proto::ticket::CLOCK_SKEW_SECS {
        return Ok(None);
    }
    let b: [u8; 32] = hex::decode(k).ok().and_then(|b| b.try_into().ok()).context("bad server.key.prev")?;
    Ok(Some(iroh_base::PublicKey::from_bytes(&b).context("bad server.key.prev")?))
}

/// The ticket-signing key, created on first use.
pub fn load_or_create_key(data: &Path) -> Result<iroh_base::SecretKey> {
    let path = data.join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(b) => {
            let b: [u8; 32] = b.try_into().map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", path.display()))?;
            Ok(iroh_base::SecretKey::from_bytes(&b))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let k = iroh_base::SecretKey::generate();
            write_private(&path, &k.to_bytes())?;
            Ok(k)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_login_only_on_local_http() {
        let mut c = ServerConfig {
            public_url: "http://127.0.0.1:8080".into(),
            tls: Tls::None,
            dev_login: true,
            ..Default::default()
        };
        assert!(c.dev_login_enabled());
        assert_eq!(c.providers(), vec!["dev".to_string()]);
        c.public_url = "http://rdesk.example.com".into();
        assert!(!c.dev_login_enabled());
        c.public_url = "https://127.0.0.1".into();
        c.tls = Tls::Acme;
        assert!(!c.dev_login_enabled());
    }

    #[test]
    fn acme_needs_https_and_contact() {
        let mut c = ServerConfig { public_url: "http://x.example.com".into(), ..Default::default() };
        assert!(c.validate().is_err());
        c.public_url = "https://x.example.com".into();
        assert!(c.validate().is_err());
        c.acme_contact = Some("a@b.c".into());
        assert!(c.validate().is_ok());
    }
}
