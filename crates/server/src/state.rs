//! What the HTTP handlers share: config, database, sign-in flows in progress.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use webauthn_rs::prelude::{PasskeyAuthentication, PasskeyRegistration, Webauthn, WebauthnBuilder};

use crate::config::ServerConfig;
use crate::db::Db;
use crate::oauth::Provider;

/// A browser sign-in may take this long.
pub const FLOW_TTL: Duration = Duration::from_secs(15 * 60);
/// The one-time code the app trades for a session.
pub const CODE_TTL: Duration = Duration::from_secs(120);
/// Wrong TOTP codes allowed in one sign-in.
pub const MAX_TOTP_FAILURES: u32 = 5;

/// What the app asked for when it opened the browser.
#[derive(Debug, Clone)]
pub struct AppRequest {
    pub redirect_uri: String,
    pub state: String,
    pub code_challenge: String,
    pub device: String,
}

/// A sign-in in progress (keyed by the `rdesk_flow` cookie).
pub struct Flow {
    pub created: Instant,
    pub app: AppRequest,
    /// `(provider, OAuth state, PKCE verifier)` while at the provider.
    pub oauth: Option<(Provider, String, String)>,
    /// Set once Google/GitHub (or dev login) said who this is.
    pub user_id: Option<i64>,
    /// TOTP secret being set up (base32), shown as a QR code.
    pub totp_setup: Option<String>,
    pub totp_failures: u32,
    pub passkey_reg: Option<PasskeyRegistration>,
    pub passkey_auth: Option<PasskeyAuthentication>,
}

pub struct PendingCode {
    pub created: Instant,
    pub user_id: i64,
    pub code_challenge: String,
    pub device: String,
}

pub struct State {
    pub cfg: ServerConfig,
    pub public: url::Url,
    pub db: Db,
    /// Signs access tickets (used from phase 3); its public half is in `/api/info`.
    pub key: iroh_base::SecretKey,
    /// `None` when `public_url` is an IP address (passkeys need a domain name).
    pub webauthn: Option<Webauthn>,
    pub http: reqwest::Client,
    /// The embedded relay's plain-HTTP address (on loopback); `/relay` is proxied there.
    pub relay_http: Mutex<Option<SocketAddr>>,
    pub proxy: crate::relay::ProxyClient,
    /// Endpoints connected to the relay right now.
    pub online: crate::relay::Online,
    pub flows: Mutex<HashMap<String, Flow>>,
    pub codes: Mutex<HashMap<String, PendingCode>>,
    /// Wakes a machine's long poll when something concerns it.
    pub machine_events: Mutex<HashMap<i64, Arc<tokio::sync::Notify>>>,
}

pub type Shared = Arc<State>;

impl State {
    pub fn new(cfg: ServerConfig, db: Db, key: iroh_base::SecretKey) -> Result<Shared> {
        let public = cfg.public_url()?;
        let rp_id = public.host_str().context("public_url has no host")?.to_string();
        let webauthn = match WebauthnBuilder::new(&rp_id, &public).and_then(|b| b.rp_name(&cfg.name).build()) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!("passkeys are off ({e}): they need public_url to use a domain name");
                None
            }
        };
        let http =
            reqwest::Client::builder().timeout(Duration::from_secs(20)).build().context("creating the HTTP client")?;
        Ok(Arc::new(Self {
            cfg,
            public,
            db,
            key,
            webauthn,
            http,
            relay_http: Mutex::new(None),
            proxy: crate::relay::proxy_client(),
            online: Default::default(),
            flows: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
            machine_events: Mutex::new(HashMap::new()),
        }))
    }

    /// `public_url` + `path`, without a doubled slash.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.public_url.trim_end_matches('/'))
    }

    /// The relay URL clients use (served by us under `/relay`).
    pub fn relay_url(&self) -> String {
        self.url("/")
    }

    pub fn machine_notify(&self, id: i64) -> Arc<tokio::sync::Notify> {
        self.machine_events.lock().unwrap().entry(id).or_default().clone()
    }

    /// Tell these machines' hosts to sync now.
    pub fn wake(&self, ids: &[i64]) {
        let ev = self.machine_events.lock().unwrap();
        for id in ids {
            if let Some(n) = ev.get(id) {
                n.notify_waiters();
            }
        }
    }

    /// Forget sign-ins and codes that ran out of time.
    pub fn sweep(&self) {
        self.flows.lock().unwrap().retain(|_, f| f.created.elapsed() < FLOW_TTL);
        self.codes.lock().unwrap().retain(|_, c| c.created.elapsed() < CODE_TTL);
    }
}
