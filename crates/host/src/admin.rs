//! Host administration shared by the command line and the desktop app: identity,
//! authorized keys, the password, settings and a diagnostic report.

use anyhow::{Context, Result, anyhow, bail};
use rdesk_common::account::{Registration, Session};
use rdesk_common::agent;
use rdesk_common::auth;
use rdesk_common::config::{self, HostConfig};
use rdesk_common::transport::{self, HostIdentity};
use ssh_key::PublicKey;

/// Create whatever is missing (identity, host.toml, OPAQUE setup) and return the fingerprint.
/// Safe to call every time the host starts.
pub fn ensure_initialized() -> Result<[u8; 32]> {
    config::ensure_host_dir()?;
    remove_legacy_files();
    let id = match HostIdentity::load() {
        Ok(id) => id,
        Err(_) => {
            let id = HostIdentity::generate();
            id.save()?;
            id
        }
    };
    if !config::config_path(config::HOST_CONFIG)?.exists() {
        config::save_toml(config::HOST_CONFIG, &HostConfig::default())?;
    }
    let setup = config::config_path(config::OPAQUE_SETUP)?;
    if !setup.exists() {
        config::write_private(&setup, &auth::server_setup_to_bytes(&auth::new_server_setup()))?;
    }
    Ok(id.fingerprint())
}

/// Files of the Tailscale-era versions (TLS certificate, pinned fingerprints): unused now.
fn remove_legacy_files() {
    for name in ["host_key.der", "host_cert.der", "known_hosts", "network.toml"] {
        if let Ok(p) = config::config_path(name)
            && p.exists()
        {
            match std::fs::remove_file(&p) {
                Ok(()) => tracing::info!("removed the old {}", p.display()),
                Err(e) => tracing::debug!("{}: {e}", p.display()),
            }
        }
    }
}

/// New identity, so a new host ID: every client must be given the new ID.
pub fn regenerate_identity() -> Result<[u8; 32]> {
    config::ensure_host_dir()?;
    let id = HostIdentity::generate();
    id.save()?;
    Ok(id.fingerprint())
}

pub fn fingerprint() -> Result<[u8; 32]> {
    Ok(HostIdentity::load()?.fingerprint())
}

// ---- registration with the server ---------------------------------------------

/// Register this computer with the signed-in user's server (or refresh its token).
/// The host must be restarted to use it.
pub async fn register(session: &Session, name: &str, visibility: rdesk_proto::api::Visibility) -> Result<Registration> {
    ensure_initialized()?;
    let id = HostIdentity::load()?;
    let api = session.api()?;
    let r = api.register_machine(session.user.id, &id.secret_key(), name, visibility).await?;
    let reg = Registration {
        server: session.server.clone(),
        server_name: r.server_name,
        machine_id: r.machine.id,
        name: r.machine.name,
        token: r.token,
        server_key: r.server_key,
        relay_url: r.relay_url,
        endpoint_id: config::format_host_id(&id.fingerprint()),
    };
    reg.save()?;
    forget_enrollment();
    Ok(reg)
}

/// Registered (or unregistered) by hand: stop asking a server to add this computer.
fn forget_enrollment() {
    if let Ok(mut cfg) = load_config()
        && !cfg.enroll.is_empty()
    {
        cfg.enroll = Default::default();
        if let Err(e) = save_config(&cfg) {
            tracing::warn!("host.toml: {e:#}");
        }
    }
}

/// Remove this computer from the server (only who registered it can) and forget it here.
pub async fn unregister(session: &Session) -> Result<()> {
    let Some(reg) = Registration::load() else { bail!("this computer is not registered") };
    if reg.server != session.server {
        bail!("this computer is registered with {}, not with the server you are signed in to", reg.server);
    }
    match session.api()?.remove_machine(reg.machine_id).await {
        Ok(()) => {}
        // Already gone on the server: just forget it here.
        Err(e) if format!("{e:#}").contains("no such computer") => {}
        Err(e) => return Err(e),
    }
    forget_enrollment();
    Registration::clear()
}

// ---- keys -------------------------------------------------------------------

pub fn load_keys() -> Result<Vec<PublicKey>> {
    match std::fs::read_to_string(config::config_path(config::AUTHORIZED_KEYS)?) {
        Ok(t) => auth::parse_authorized_keys(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_keys(keys: &[PublicKey]) -> Result<()> {
    config::ensure_host_dir()?;
    let mut out = String::from("# rdesk authorized keys (OpenSSH format)\n");
    for k in keys {
        out.push_str(k.to_openssh()?.trim());
        out.push('\n');
    }
    config::write_private(&config::config_path(config::AUTHORIZED_KEYS)?, out.as_bytes())
}

/// An OpenSSH public key line (`ssh-ed25519 AAAA… comment`) or the path of a `.pub` file.
pub fn parse_public_key(text_or_path: &str) -> Result<PublicKey> {
    let t = text_or_path.trim();
    let text = if !t.starts_with("ssh-") && !t.starts_with("ecdsa-") && std::path::Path::new(t).is_file() {
        std::fs::read_to_string(t)?
    } else {
        t.to_string()
    };
    PublicKey::from_openssh(text.trim()).context("not an OpenSSH public key (expected `ssh-ed25519 AAAA… comment`)")
}

/// Returns `false` when the key was already authorized.
pub fn add_key(key: &PublicKey) -> Result<bool> {
    let mut keys = load_keys()?;
    if keys.iter().any(|k| k.key_data() == key.key_data()) {
        return Ok(false);
    }
    keys.push(key.clone());
    save_keys(&keys)?;
    Ok(true)
}

pub fn remove_key(fingerprint: &str) -> Result<()> {
    let mut keys = load_keys()?;
    let before = keys.len();
    keys.retain(|k| agent::key_fingerprint(k.key_data()) != fingerprint);
    if keys.len() == before {
        return Err(anyhow!("no key with fingerprint {fingerprint}"));
    }
    save_keys(&keys)
}

// ---- password ---------------------------------------------------------------

pub fn has_password() -> bool {
    config::config_path(config::OPAQUE_PASSWORD).map(|p| p.exists()).unwrap_or(false)
}

/// Store the OPAQUE record for `password` (Argon2id: takes about a second).
pub fn set_password(password: &str) -> Result<()> {
    ensure_initialized()?;
    let setup = auth::server_setup_from_bytes(&std::fs::read(config::config_path(config::OPAQUE_SETUP)?)?)?;
    let record = auth::register_password(&setup, password.as_bytes())?;
    config::write_private(&config::config_path(config::OPAQUE_PASSWORD)?, &record)
}

pub fn clear_password() -> Result<()> {
    let p = config::config_path(config::OPAQUE_PASSWORD)?;
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

/// A random password from an unambiguous alphabet, ~143 bits for 24 characters.
pub fn generate_password(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789-_";
    let mut rng = rand::rngs::OsRng;
    (0..len).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

// ---- settings -----------------------------------------------------------------

pub fn load_config() -> Result<HostConfig> {
    config::load_toml(config::HOST_CONFIG)
}

pub fn save_config(cfg: &HostConfig) -> Result<()> {
    config::ensure_host_dir()?;
    config::save_toml(config::HOST_CONFIG, cfg)
}

// ---- diagnostics --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CheckLine {
    pub label: String,
    pub value: String,
    /// `false` when this is a problem worth fixing.
    pub ok: bool,
}

fn line(label: &str, value: impl Into<String>, ok: bool) -> CheckLine {
    CheckLine { label: label.to_string(), value: value.into(), ok }
}

/// What `rdesk host check` prints, as data.
pub async fn diagnose() -> Vec<CheckLine> {
    let mut out = Vec::new();
    match config::host_dir() {
        Ok(d) => out.push(line("Host folder", d.display().to_string(), true)),
        Err(e) => out.push(line("Host folder", format!("{e:#}"), false)),
    }
    match HostIdentity::load() {
        Ok(id) => out.push(line("Host ID", config::format_host_id(&id.fingerprint()), true)),
        Err(_) => out.push(line("Identity", "not created yet (starting the host creates it)", false)),
    }
    let keys = load_keys().map(|k| k.len()).unwrap_or(0);
    let pw = has_password();
    out.push(line("SSH keys", format!("{keys} authorized"), keys > 0 || pw));
    out.push(line("Password", if pw { "enabled" } else { "not set" }, keys > 0 || pw));
    match Registration::load() {
        Some(r) => {
            out.push(line("Server", format!("registered as {} on {} ({})", r.name, r.server_name, r.server), true))
        }
        None => out.push(line("Server", "not registered: only direct connections on the LAN", false)),
    }
    let net = config::host_network();
    match transport::relay_url(&net) {
        Ok(None) => {}
        Ok(Some(url)) => {
            let reach = async {
                let ep = transport::bind(None, &net, &Default::default(), transport::Role::Client).await?;
                let ok = transport::wait_relay(&ep, &net, std::time::Duration::from_secs(5)).await;
                ep.close().await;
                ok
            };
            match reach.await {
                Ok(_) => out.push(line("Relay", format!("{url} (reachable)"), true)),
                Err(e) => out.push(line("Relay", format!("{url}: {e:#}"), false)),
            }
        }
        Err(e) => out.push(line("Relay", format!("{e:#}"), false)),
    }
    let cap = crate::capture::Capturer::spawn(false);
    match cap.monitors().await {
        Ok(ms) if ms.is_empty() => out.push(line("Screens", "none found", false)),
        Ok(ms) => {
            for m in ms {
                let t = std::time::Instant::now();
                let label = format!("Screen {}", m.info.name);
                match cap.grab(m.info.id).await {
                    Ok(f) => out.push(line(
                        &label,
                        format!("{}x{}, captured in {} ms", f.width, f.height, t.elapsed().as_millis()),
                        true,
                    )),
                    Err(e) => out.push(line(&label, format!("capture failed: {e:#}"), false)),
                }
            }
        }
        Err(e) => out.push(line("Screens", format!("{e:#}"), false)),
    }
    if cfg!(target_os = "macos") {
        out.push(line(
            "macOS",
            "grant rdesk Screen Recording and Accessibility in System Settings → Privacy & Security",
            true,
        ));
    }
    #[cfg(target_os = "linux")]
    {
        // Wayland goes through the desktop portal, which asks once before sharing the screen.
        let wayland = crate::wayland::active();
        out.push(line(
            "Session",
            if wayland { "Wayland: screen and input through the desktop portal (allow it when asked)" } else { "X11" },
            true,
        ));
    }
    if cfg!(windows) {
        let admin = crate::elevation::is_elevated();
        out.push(line(
            "Administrator",
            if admin {
                "yes: admin windows can be controlled"
            } else {
                "no: admin windows (Task Manager…) ignore input"
            },
            admin,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_are_long_and_varied() {
        let a = generate_password(24);
        let b = generate_password(24);
        assert_eq!(a.chars().count(), 24);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn parses_key_lines() {
        let k = parse_public_key(
            "  ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDvg93Fy/gLMmRQCxhC7CrYugOkEwJJjiCkP2JtvJ/14 test  ",
        )
        .unwrap();
        assert_eq!(k.comment(), "test");
        assert!(parse_public_key("not a key").is_err());
    }
}
