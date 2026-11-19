//! Joining a server without anyone signing in on this computer: the host asks the server
//! to add it (owner's e-mail → approved in the app; enrollment key → added at once) and
//! keeps asking until it is a registered machine.

use std::time::{Duration, Instant};

use anyhow::Result;
use rdesk_common::account::{Api, Registration, normalize_server};
use rdesk_common::config::{self, EnrollConfig, HostConfig};
use rdesk_common::transport::HostIdentity;
use rdesk_proto::api::{ENROLL_MAX_WAIT_SECS, EnrollReply};
use tokio::sync::mpsc;

use crate::control::{Ctl, Reporter};

/// What to ask for, after the environment overrides host.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub server: String,
    pub owner: Option<String>,
    pub key: Option<String>,
    pub name: String,
}

impl Enrollment {
    /// Who decides, for messages.
    pub fn approver(&self) -> String {
        match &self.owner {
            Some(o) => o.clone(),
            None => "an admin of the server".into(),
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn clean(v: &Option<String>) -> Option<String> {
    v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// The enrollment to run, if a server is configured (environment first, then host.toml).
pub fn settings(cfg: &EnrollConfig) -> Option<Enrollment> {
    let server = env("RDESK_SERVER").or_else(|| clean(&cfg.server))?;
    Some(Enrollment {
        server,
        owner: env("RDESK_OWNER").or_else(|| clean(&cfg.owner)),
        key: env("RDESK_ENROLL_KEY").or_else(|| clean(&cfg.key)),
        name: env("RDESK_NAME").or_else(|| clean(&cfg.name)).unwrap_or_else(rdesk_common::local_hostname),
    })
}

/// Save `[enroll]` in host.toml (the host uses it the next time it starts unregistered).
pub fn save(e: EnrollConfig) -> Result<()> {
    if let Some(s) = &e.server {
        normalize_server(s)?;
    }
    crate::admin::ensure_initialized()?;
    let mut cfg: HostConfig = config::load_toml(config::HOST_CONFIG)?;
    cfg.enroll = e;
    crate::admin::save_config(&cfg)
}

/// Wait, unless told to stop. `false`: stop.
async fn pause(d: Duration, ctl: &mut mpsc::UnboundedReceiver<Ctl>) -> bool {
    let until = tokio::time::Instant::now() + d;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => return true,
            c = ctl.recv() => match c {
                Some(Ctl::Disconnect) => continue,
                _ => return false,
            },
        }
    }
}

/// Ask until the server registers this computer. `None` when told to stop first.
pub(crate) async fn until_registered(
    identity: &HostIdentity,
    e: &Enrollment,
    reporter: &Reporter,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
) -> Result<Option<Registration>> {
    let server = normalize_server(&e.server)?;
    let api = Api::new(&server, None)?;
    let key = identity.secret_key();
    let mut backoff = Duration::from_secs(5);
    let mut last_note = String::new();
    // Each new situation once (the host asks every ~25 s).
    let mut note = |text: String, warning: bool, print: bool| {
        if text != last_note {
            if print {
                println!("{text}");
            }
            reporter.enrollment(Some(text.clone()));
            reporter.event(text.clone(), warning);
            last_note = text;
        }
    };
    loop {
        let started = Instant::now();
        let call = api.enroll(&key, &e.name, e.owner.as_deref(), e.key.as_deref(), ENROLL_MAX_WAIT_SECS);
        let reply = tokio::select! {
            r = call => r,
            c = ctl.recv() => match c {
                Some(Ctl::Disconnect) => continue,
                _ => {
                    reporter.enrollment(None);
                    return Ok(None);
                }
            },
        };
        let wait = match reply {
            Ok(EnrollReply::Registered(r)) => {
                let r = *r;
                let reg = Registration {
                    server: server.clone(),
                    server_name: r.server_name,
                    machine_id: r.machine.id,
                    name: r.machine.name,
                    token: r.token,
                    server_key: r.server_key,
                    relay_url: r.relay_url,
                    endpoint_id: config::format_host_id(&identity.fingerprint()),
                };
                reg.save()?;
                forget_used_key();
                reporter.enrollment(None);
                reporter.event(format!("Added to {} as {}", reg.server_name, reg.name), false);
                return Ok(Some(reg));
            }
            Ok(EnrollReply::Pending { code }) => {
                backoff = Duration::from_secs(5);
                let text = format!("Waiting for {} to approve this computer (code {code}) on {server}", e.approver());
                note(text, false, true);
                // The server held the request open; ask again right away unless it answered early.
                Duration::from_secs(3).saturating_sub(started.elapsed())
            }
            Ok(EnrollReply::Rejected) => {
                note(format!("{} refused this computer on {server}", e.approver()), true, true);
                Duration::from_secs(30 * 60)
            }
            Err(err) => {
                note(format!("Cannot join {server}: {err:#}"), true, false);
                let w = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(300));
                w
            }
        };
        if !pause(wait, ctl).await {
            reporter.enrollment(None);
            return Ok(None);
        }
    }
}

/// A key in host.toml has done its job: do not keep the secret around.
fn forget_used_key() {
    if let Ok(mut cfg) = config::load_toml::<HostConfig>(config::HOST_CONFIG)
        && cfg.enroll.key.is_some()
    {
        cfg.enroll.key = None;
        if let Err(e) = crate::admin::save_config(&cfg) {
            tracing::warn!("could not remove the used enrollment key from host.toml: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_a_server_and_defaults_the_name() {
        // Only meaningful when the environment does not set these (CI, developers' shells).
        if ["RDESK_SERVER", "RDESK_OWNER", "RDESK_ENROLL_KEY", "RDESK_NAME"].iter().any(|v| env(v).is_some()) {
            return;
        }
        assert!(settings(&EnrollConfig::default()).is_none());
        let e = settings(&EnrollConfig {
            server: Some(" rdesk.example.com ".into()),
            owner: Some("ana@example.com".into()),
            key: Some("  ".into()),
            name: None,
        })
        .unwrap();
        assert_eq!(e.server, "rdesk.example.com");
        assert_eq!(e.key, None);
        assert_eq!(e.name, rdesk_common::local_hostname());
        assert_eq!(e.approver(), "ana@example.com");
    }
}
