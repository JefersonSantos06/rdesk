//! Signing in to the rdesk server, and its API.
//!
//! The sign-in runs in the browser (Google/GitHub + second factor) and comes back to a
//! one-shot HTTP listener on 127.0.0.1; the app then trades the code for a session
//! (PKCE). A session lasts 10 days. Every account signed in to (one per server and user)
//! is kept in `accounts.json` (owner-only); one of them is in use at a time.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use rdesk_proto::api::{
    self, API_VERSION, AddMember, AddUser, AdminUser, AgentSync, AgentSyncReply, ApiError, ApproveMachine,
    CreatedEnrollKey, EnrollKey, EnrollReply, EnrollRequest, Group, GroupRef, GroupRole, HistoryEntry, Machine,
    MachineRequest, Managers, NewEnrollKey, NewGroup, RegisterMachine, Registered, ServerInfo, SetManagers, SetRole,
    SetShares, ShareRole, Shares, TicketRequest, TicketResponse, TokenRequest, TokenResponse, UpdateMachine,
    UpdateUser, UserInfo, UserRef, Visibility,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config;

/// The accounts this person signed in to (one per server and user), and which one is in use.
pub const ACCOUNTS_FILE: &str = "accounts.json";
/// Before several accounts: the one sign-in. Moved into [`ACCOUNTS_FILE`] the first time.
pub const SESSION_FILE: &str = "session.json";
/// This computer's registration with the server (its own credential).
pub const MACHINE_FILE: &str = "machine.json";
/// How long the browser part may take.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// Not signed in, or the server ended the session (expired, revoked, user disabled).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NotSignedIn(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Base URL of the server, e.g. `https://rdesk.example.com`.
    pub server: String,
    pub token: String,
    /// Unix seconds.
    pub expires_at: i64,
    pub user: UserInfo,
    pub relay_url: String,
    pub server_name: String,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `accounts.json`: every account signed in to on this device, and the one in use. Only one
/// is used at a time: the app shows one server's computers, and changing is explicit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Accounts {
    /// [`Session::key`] of the account in use.
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    sessions: Vec<Session>,
}

impl Accounts {
    fn read() -> Self {
        let Ok(path) = config::config_path(ACCOUNTS_FILE) else { return Self::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::migrate(),
        }
    }

    /// The one `session.json` of earlier versions becomes the first account (and is in use).
    fn migrate() -> Self {
        let Ok(old) = config::config_path(SESSION_FILE) else { return Self::default() };
        let Some(s) = std::fs::read_to_string(&old).ok().and_then(|t| serde_json::from_str::<Session>(&t).ok()) else {
            return Self::default();
        };
        let a = Self { active: Some(s.key()), sessions: vec![s] };
        match a.write() {
            Ok(()) => {
                let _ = remove_if_present(&old);
            }
            Err(e) => tracing::warn!("moving {SESSION_FILE} into {ACCOUNTS_FILE}: {e:#}"),
        }
        a
    }

    fn write(&self) -> Result<()> {
        config::ensure_config_dir()?;
        config::write_private(&config::config_path(ACCOUNTS_FILE)?, serde_json::to_string_pretty(self)?.as_bytes())
    }

    fn get(&self, key: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.key() == key)
    }
}

/// The account this process uses instead of the one in use (see [`Session::pin`]).
static PINNED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

impl Session {
    /// Identifies an account: the server and the user on it. One server may have several
    /// (different e-mails), and every server has its own.
    pub fn key(&self) -> String {
        format!("{}#{}", self.server, self.user.id)
    }

    /// "Server name · e-mail", how the app names an account.
    pub fn label(&self) -> String {
        format!("{} · {}", self.server_name, self.user.email)
    }

    /// The account in use (or the one this process is [pinned](Self::pin) to), if it has not
    /// expired.
    pub fn load() -> Option<Self> {
        match PINNED.get() {
            Some(key) => Self::load_account(key),
            None => {
                let a = Accounts::read();
                a.get(a.active.as_deref()?).filter(|s| !s.expired()).cloned()
            }
        }
    }

    /// The account `key`, if it is saved and has not expired.
    pub fn load_account(key: &str) -> Option<Self> {
        Accounts::read().get(key).filter(|s| !s.expired()).cloned()
    }

    /// Every saved account, expired ones too (they ask to sign in again), in the order they
    /// were added.
    pub fn all() -> Vec<Self> {
        Accounts::read().sessions
    }

    /// [`Session::key`] of the account in use (or pinned), expired or not.
    pub fn active_key() -> Option<String> {
        PINNED.get().cloned().or_else(|| Accounts::read().active)
    }

    /// For the rest of this process, use the account `key` instead of the one in use: a
    /// session window keeps the account it was opened with when the app changes to another.
    pub fn pin(key: &str) {
        let _ = PINNED.set(key.to_string());
    }

    /// The saved account `sel` names: its key, its number in [`Session::all`] (from 1), its
    /// server's address or name, or its e-mail. Several match: an error naming them.
    pub fn find(sel: &str) -> Result<Self> {
        let all = Self::all();
        let sel = sel.trim();
        if let Some(s) = all.iter().find(|s| s.key() == sel) {
            return Ok(s.clone());
        }
        if let Ok(n) = sel.parse::<usize>() {
            return all.get(n.wrapping_sub(1)).cloned().with_context(|| format!("there is no account number {n}"));
        }
        let lower = sel.to_lowercase();
        let host = normalize_server(sel).ok();
        let found: Vec<&Session> = all
            .iter()
            .filter(|s| {
                Some(&s.server) == host.as_ref()
                    || s.server_name.to_lowercase() == lower
                    || s.user.email.to_lowercase() == lower
            })
            .collect();
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no saved account matches {sel:?} (see `rdesk server list`)"),
            many => bail!(
                "{sel:?} matches several accounts: {} (use its number from `rdesk server list`)",
                many.iter().map(|s| s.label()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Save this account (replacing an older sign-in of the same one) and use it.
    pub fn save(&self) -> Result<()> {
        let mut a = Accounts::read();
        let key = self.key();
        match a.sessions.iter_mut().find(|s| s.key() == key) {
            Some(s) => *s = self.clone(),
            None => a.sessions.push(self.clone()),
        }
        a.active = Some(key);
        a.write()
    }

    /// Use the saved account `key` from now on. It may have expired: then the app asks to
    /// sign in to it again.
    pub fn activate(key: &str) -> Result<()> {
        let mut a = Accounts::read();
        if a.get(key).is_none() {
            bail!("no saved account {key}");
        }
        a.active = Some(key.to_string());
        a.write()
    }

    /// The server ended the account `key` (revoked, user disabled): keep it in the list, to
    /// sign in to again.
    pub fn end(key: &str) -> Result<()> {
        let mut a = Accounts::read();
        let Some(s) = a.sessions.iter_mut().find(|s| s.key() == key) else { return Ok(()) };
        s.expires_at = 0;
        s.token.clear();
        a.write()
    }

    /// Forget the account `key` (signed out). If it was in use, none is until one is chosen.
    pub fn remove(key: &str) -> Result<()> {
        let mut a = Accounts::read();
        a.sessions.retain(|s| s.key() != key);
        if a.active.as_deref() == Some(key) {
            a.active = None;
        }
        a.write()
    }

    /// Forget the account in use (signed out).
    pub fn clear() -> Result<()> {
        match Self::active_key() {
            Some(key) => Self::remove(&key),
            None => Ok(()),
        }
    }

    /// How to reach others with this account: its server's relay and its token.
    pub fn network(&self) -> config::NetworkConfig {
        config::NetworkConfig { relay: Some(self.relay_url.clone()), token: Some(self.token.clone()) }
    }

    pub fn expired(&self) -> bool {
        self.expires_at <= now()
    }

    /// Whole days left, for "signed in for N more days".
    pub fn days_left(&self) -> i64 {
        ((self.expires_at - now()).max(0) + 86_399) / 86_400
    }

    pub fn api(&self) -> Result<Api> {
        Api::new(&self.server, Some(self.token.clone()))
    }
}

/// This computer, registered with an rdesk server: the host uses its own token (not a
/// user's session) for the relay and to sync, so it stays reachable when nobody is signed in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registration {
    pub server: String,
    pub server_name: String,
    pub machine_id: i64,
    pub name: String,
    /// The machine token (secret).
    pub token: String,
    /// Hex public key that signs tickets.
    pub server_key: String,
    pub relay_url: String,
    /// The host ID this registration is for.
    pub endpoint_id: String,
}

impl Registration {
    pub fn load() -> Option<Self> {
        let path = config::config_path(MACHINE_FILE).ok()?;
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    pub fn save(&self) -> Result<()> {
        config::ensure_config_dir()?;
        config::write_private(&config::config_path(MACHINE_FILE)?, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn clear() -> Result<()> {
        remove_if_present(&config::config_path(MACHINE_FILE)?)
    }

    pub fn server_key(&self) -> Result<[u8; 32]> {
        hex::decode(&self.server_key)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow!("{MACHINE_FILE}: bad server key"))
    }

    pub fn api(&self) -> Result<Api> {
        Api::new(&self.server, Some(self.token.clone()))
    }
}

/// Delete a file; fine if it is already gone, also when someone else is deleting it at the
/// same moment (Windows then answers "access denied" until it disappears).
fn remove_if_present(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            for _ in 0..20 {
                if !path.exists() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(anyhow::Error::from(e).context(format!("removing {}", path.display())))
        }
    }
}

/// Use ring for rustls in this process (reqwest has no provider of its own here).
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// An HTTPS client builder with rdesk's user agent. The system's certificate store checks
/// the server, except on Android: reaching it there needs the app's Java side, so the
/// Mozilla roots built into the app are used instead.
pub fn http_client() -> reqwest::ClientBuilder {
    install_crypto();
    let b = reqwest::Client::builder().user_agent(concat!("rdesk/", env!("CARGO_PKG_VERSION")));
    #[cfg(target_os = "android")]
    let b = {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let tls = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        b.tls_backend_preconfigured(tls)
    };
    b
}

/// `https://host` from what the user typed (`host`, `host:port`, a URL).
pub fn normalize_server(s: &str) -> Result<String> {
    let s = s.trim().trim_end_matches('/');
    if s.is_empty() {
        bail!("enter the server address");
    }
    let with_scheme = if s.contains("://") { s.to_string() } else { format!("https://{s}") };
    let u = url::Url::parse(&with_scheme).with_context(|| format!("{s:?} is not a server address"))?;
    if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() {
        bail!("{s:?} is not a server address");
    }
    Ok(with_scheme)
}

pub struct Api {
    base: String,
    token: Option<String>,
    /// A presence proof sent with every request (see [`Api::with_here`]).
    here: Option<String>,
    http: reqwest::Client,
}

/// A presence proof for the computer whose host key is `key`, for `user_id` (see
/// [`api::HereProof`]), as the header value.
pub fn here_proof(key: &iroh::SecretKey, user_id: i64, scope: api::Here) -> String {
    let endpoint = *key.public().as_bytes();
    let timestamp = now();
    let sig = key.sign(&api::HereProof::message(scope, user_id, &endpoint, timestamp));
    api::HereProof { scope, user_id, timestamp, endpoint, signature: sig.to_bytes() }.to_header()
}

impl Api {
    pub fn new(server: &str, token: Option<String>) -> Result<Self> {
        let http = http_client().timeout(Duration::from_secs(20)).build()?;
        Ok(Self { base: normalize_server(server)?, token, here: None, http })
    }

    /// Send a presence proof (from [`here_proof`]) with every request: the server then
    /// also shows (and, with [`api::Here::Manage`], lets manage) the computer it was signed on.
    pub fn with_here(mut self, proof: String) -> Self {
        self.here = Some(proof);
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = match &self.here {
            Some(h) => rb.header(api::HEADER_HERE, h),
            None => rb,
        };
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let msg = resp.json::<ApiError>().await.map(|e| e.error).unwrap_or_else(|_| status.to_string());
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(NotSignedIn(msg).into());
        }
        Err(anyhow!("{msg}"))
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let r = self
            .auth(self.http.get(self.url(path)))
            .send()
            .await
            .with_context(|| format!("contacting {}", self.base))?;
        Ok(Self::check(r).await?.json().await?)
    }

    async fn send<B: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let r = self
            .auth(self.http.request(method, self.url(path)).json(body))
            .send()
            .await
            .with_context(|| format!("contacting {}", self.base))?;
        Ok(Self::check(r).await?.json().await?)
    }

    pub async fn info(&self) -> Result<ServerInfo> {
        let info: ServerInfo = self.get(api::PATH_INFO).await.context("this does not look like an rdesk server")?;
        if info.api != API_VERSION {
            bail!("the server speaks API v{} and this app v{API_VERSION}: update one of them", info.api);
        }
        Ok(info)
    }

    pub async fn me(&self) -> Result<UserInfo> {
        self.get(api::PATH_ME).await
    }

    pub async fn logout(&self) -> Result<()> {
        let r = self.auth(self.http.post(self.url(api::PATH_LOGOUT))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn users(&self) -> Result<Vec<AdminUser>> {
        self.get(api::PATH_ADMIN_USERS).await
    }

    pub async fn add_user(&self, email: &str, is_admin: bool) -> Result<AdminUser> {
        self.send(reqwest::Method::POST, api::PATH_ADMIN_USERS, &AddUser { email: email.into(), is_admin }).await
    }

    pub async fn update_user(&self, id: i64, change: &UpdateUser) -> Result<AdminUser> {
        self.send(reqwest::Method::PATCH, &format!("{}/{id}", api::PATH_ADMIN_USERS), change).await
    }

    pub async fn remove_user(&self, id: i64) -> Result<()> {
        let r = self.auth(self.http.delete(self.url(&format!("{}/{id}", api::PATH_ADMIN_USERS)))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    // ---- machines -------------------------------------------------------------------

    pub async fn machines(&self) -> Result<Vec<Machine>> {
        self.get(api::PATH_MACHINES).await
    }

    pub async fn admin_machines(&self) -> Result<Vec<Machine>> {
        self.get(api::PATH_ADMIN_MACHINES).await
    }

    /// Register this computer; `host_key` proves the app runs on it.
    pub async fn register_machine(
        &self,
        user_id: i64,
        host_key: &iroh::SecretKey,
        name: &str,
        visibility: Visibility,
    ) -> Result<Registered> {
        let endpoint = *host_key.public().as_bytes();
        let timestamp = now();
        let sig = host_key.sign(&api::register_message(user_id, &endpoint, timestamp));
        let req = RegisterMachine {
            endpoint_id: hex::encode(endpoint),
            name: name.to_string(),
            os: std::env::consts::OS.to_string(),
            visibility,
            timestamp,
            signature: hex::encode(sig.to_bytes()),
        };
        self.send(reqwest::Method::POST, api::PATH_MACHINES, &req).await
    }

    pub async fn update_machine(&self, id: i64, change: &UpdateMachine) -> Result<Machine> {
        self.send(reqwest::Method::PATCH, &format!("{}/{id}", api::PATH_MACHINES), change).await
    }

    pub async fn remove_machine(&self, id: i64) -> Result<()> {
        let r = self.auth(self.http.delete(self.url(&format!("{}/{id}", api::PATH_MACHINES)))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn shares(&self, id: i64) -> Result<Shares> {
        self.get(&format!("{}/{id}/shares", api::PATH_MACHINES)).await
    }

    pub async fn set_shares(
        &self,
        id: i64,
        users: Vec<(i64, ShareRole)>,
        groups: Vec<(i64, ShareRole)>,
    ) -> Result<Shares> {
        self.send(reqwest::Method::PUT, &format!("{}/{id}/shares", api::PATH_MACHINES), &SetShares { users, groups })
            .await
    }

    pub async fn managers(&self, id: i64) -> Result<Managers> {
        self.get(&format!("{}/{id}/managers", api::PATH_MACHINES)).await
    }

    pub async fn set_managers(&self, id: i64, users: Vec<i64>) -> Result<Managers> {
        self.send(reqwest::Method::PUT, &format!("{}/{id}/managers", api::PATH_MACHINES), &SetManagers { users }).await
    }

    /// Ask computers on `id`'s network to wake it (Wake-on-LAN).
    pub async fn wake(&self, id: i64) -> Result<rdesk_proto::api::WakeReply> {
        self.send(reqwest::Method::POST, &format!("{}/{id}/wake", api::PATH_MACHINES), &serde_json::json!({})).await
    }

    pub async fn history(&self, id: i64) -> Result<Vec<HistoryEntry>> {
        self.get(&format!("{}/{id}/history", api::PATH_MACHINES)).await
    }

    /// A ticket to connect to machine `id` from the endpoint `client`.
    pub async fn ticket(&self, id: i64, client: &[u8; 32]) -> Result<(Vec<u8>, TicketResponse)> {
        let r: TicketResponse = self
            .send(
                reqwest::Method::POST,
                &format!("{}/{id}/ticket", api::PATH_MACHINES),
                &TicketRequest { client: hex::encode(client) },
            )
            .await?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&r.ticket)
            .map_err(|_| anyhow!("the server sent a malformed ticket"))?;
        Ok((bytes, r))
    }

    pub async fn directory(&self) -> Result<Vec<UserRef>> {
        self.get(api::PATH_USERS).await
    }

    /// The machine's long poll (with the machine token).
    pub async fn agent_sync(&self, req: &AgentSync) -> Result<AgentSyncReply> {
        let r = self
            .auth(self.http.post(self.url(api::PATH_AGENT_SYNC)).json(req))
            .timeout(Duration::from_secs(40))
            .send()
            .await
            .with_context(|| format!("contacting {}", self.base))?;
        Ok(Self::check(r).await?.json().await?)
    }

    // ---- adding computers without signing in on them ---------------------------------

    /// Ask to be added (no session): `owner` approves it in the app, or `key` adds it at
    /// once. The server may hold the answer up to `wait_secs` for the approval.
    pub async fn enroll(
        &self,
        host_key: &iroh::SecretKey,
        name: &str,
        owner: Option<&str>,
        key: Option<&str>,
        wait_secs: u32,
    ) -> Result<EnrollReply> {
        let endpoint = *host_key.public().as_bytes();
        let timestamp = now();
        let sig = host_key.sign(&api::enroll_message(&endpoint, timestamp));
        let req = EnrollRequest {
            endpoint_id: hex::encode(endpoint),
            name: name.to_string(),
            os: std::env::consts::OS.to_string(),
            timestamp,
            signature: hex::encode(sig.to_bytes()),
            owner: owner.map(str::to_string),
            key: key.map(str::to_string),
            wait_secs,
        };
        let r = self
            .http
            .post(self.url(api::PATH_ENROLL))
            .json(&req)
            .timeout(Duration::from_secs(u64::from(wait_secs) + 20))
            .send()
            .await
            .with_context(|| format!("contacting {}", self.base))?;
        Ok(Self::check(r).await?.json().await?)
    }

    pub async fn machine_requests(&self) -> Result<Vec<MachineRequest>> {
        self.get(api::PATH_MACHINE_REQUESTS).await
    }

    pub async fn approve_machine(&self, id: i64, name: Option<String>, visibility: Visibility) -> Result<Machine> {
        self.send(
            reqwest::Method::POST,
            &format!("{}/{id}/approve", api::PATH_MACHINE_REQUESTS),
            &ApproveMachine { name, visibility },
        )
        .await
    }

    pub async fn reject_machine(&self, id: i64) -> Result<()> {
        let path = format!("{}/{id}/reject", api::PATH_MACHINE_REQUESTS);
        let r = self.auth(self.http.post(self.url(&path))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn enroll_keys(&self) -> Result<Vec<EnrollKey>> {
        self.get(api::PATH_ENROLL_KEYS).await
    }

    pub async fn create_enroll_key(&self, req: &NewEnrollKey) -> Result<CreatedEnrollKey> {
        self.send(reqwest::Method::POST, api::PATH_ENROLL_KEYS, req).await
    }

    // ---- updates (admins) -------------------------------------------------------------

    pub async fn releases(&self) -> Result<api::Releases> {
        self.get(api::PATH_ADMIN_RELEASES).await
    }

    /// Ask the server to look for new versions now (the list updates a moment later).
    pub async fn check_releases(&self) -> Result<()> {
        let r = self.auth(self.http.post(self.url(&format!("{}/check", api::PATH_ADMIN_RELEASES)))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn release_version(&self, version: &str, ring: api::Ring) -> Result<()> {
        let path = format!("{}/{version}/release", api::PATH_ADMIN_RELEASES);
        let r = self.auth(self.http.post(self.url(&path)).json(&api::ReleaseTo { ring })).send().await?;
        Self::check(r).await.map(|_| ())
    }

    /// Update the server itself to `version` (it restarts a moment later).
    pub async fn update_server(&self, version: &str) -> Result<()> {
        let path = format!("{}/{version}/server", api::PATH_ADMIN_RELEASES);
        let r = self.auth(self.http.post(self.url(&path))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn set_ring(&self, machine_id: i64, ring: api::Ring) -> Result<()> {
        let path = format!("{}/{machine_id}/ring", api::PATH_ADMIN_MACHINES);
        let r = self.auth(self.http.put(self.url(&path)).json(&api::SetRing { ring })).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn revoke_enroll_key(&self, id: i64) -> Result<()> {
        let r = self.auth(self.http.delete(self.url(&format!("{}/{id}", api::PATH_ENROLL_KEYS)))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    // ---- groups ---------------------------------------------------------------------

    pub async fn groups(&self) -> Result<Vec<Group>> {
        self.get(api::PATH_GROUPS).await
    }

    /// The computers shared with a group (its members and system admins).
    pub async fn group_machines(&self, id: i64) -> Result<Vec<rdesk_proto::api::GroupMachine>> {
        // A server from before this list has no such route: a bare 404, unlike "no such group".
        self.get(&format!("{}/{id}/machines", api::PATH_GROUPS)).await.map_err(|e| {
            if e.to_string() == reqwest::StatusCode::NOT_FOUND.to_string() {
                anyhow!(crate::t!(
                    "This server does not list a group's computers yet: update it (Admin, Versions).",
                    "Este servidor ainda não lista os computadores dos grupos: atualize-o (Admin, Versões)."
                ))
            } else {
                e
            }
        })
    }

    pub async fn all_groups(&self) -> Result<Vec<GroupRef>> {
        self.get(&format!("{}/all", api::PATH_GROUPS)).await
    }

    pub async fn create_group(&self, name: &str) -> Result<Group> {
        self.send(reqwest::Method::POST, api::PATH_GROUPS, &NewGroup { name: name.into() }).await
    }

    pub async fn rename_group(&self, id: i64, name: &str) -> Result<Group> {
        self.send(reqwest::Method::PATCH, &format!("{}/{id}", api::PATH_GROUPS), &NewGroup { name: name.into() }).await
    }

    pub async fn delete_group(&self, id: i64) -> Result<()> {
        let r = self.auth(self.http.delete(self.url(&format!("{}/{id}", api::PATH_GROUPS)))).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn add_member(&self, group: i64, email: &str, role: GroupRole) -> Result<Group> {
        self.send(
            reqwest::Method::POST,
            &format!("{}/{group}/members", api::PATH_GROUPS),
            &AddMember { email: email.into(), role },
        )
        .await
    }

    pub async fn set_member_role(&self, group: i64, user: i64, role: GroupRole) -> Result<Group> {
        self.send(reqwest::Method::PATCH, &format!("{}/{group}/members/{user}", api::PATH_GROUPS), &SetRole { role })
            .await
    }

    pub async fn remove_member(&self, group: i64, user: i64) -> Result<()> {
        let r = self
            .auth(self.http.delete(self.url(&format!("{}/{group}/members/{user}", api::PATH_GROUPS))))
            .send()
            .await?;
        Self::check(r).await.map(|_| ())
    }

    async fn token(&self, code: &str, verifier: &str) -> Result<TokenResponse> {
        self.send(
            reqwest::Method::POST,
            api::PATH_TOKEN,
            &TokenRequest { code: code.into(), code_verifier: verifier.into() },
        )
        .await
    }
}

fn random_b64(n: usize) -> String {
    use rand::RngCore as _;
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Sign in to `server`: opens the browser with `open`, waits for it to come back, saves
/// and returns the session.
pub async fn login(server: &str, device: &str, open: impl FnOnce(&str) -> Result<()>) -> Result<Session> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("opening the sign-in callback")?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());
    let pending = begin_login(server, device, &redirect).await?;
    open(pending.url())?;
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_callback(&listener, &pending.state, &pending.info.name))
        .await
        .map_err(|_| anyhow!("the sign-in was not finished within {} minutes", LOGIN_TIMEOUT.as_secs() / 60))??;
    pending.complete(&code).await
}

/// The phone app's redirect (its own URL scheme; see the server's `login` module).
pub const APP_REDIRECT: &str = "rdesk://callback";

/// A sign-in started in the browser and not finished yet: the app opens [`url`](Self::url)
/// and gives the code that comes back to [`complete`](Self::complete) (or the whole
/// callback URL to [`complete_from_callback`](Self::complete_from_callback)).
pub struct PendingLogin {
    api: Api,
    info: ServerInfo,
    url: String,
    state: String,
    verifier: String,
}

/// Start a sign-in to `server` that comes back to `redirect`.
pub async fn begin_login(server: &str, device: &str, redirect: &str) -> Result<PendingLogin> {
    let api = Api::new(server, None)?;
    let info = api.info().await?;
    let state = random_b64(16);
    let verifier = random_b64(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut u = url::Url::parse(&api.url(api::PATH_LOGIN))?;
    u.query_pairs_mut()
        .append_pair("redirect_uri", redirect)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("device", device);
    Ok(PendingLogin { api, info, url: u.into(), state, verifier })
}

impl PendingLogin {
    /// What to open in the browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The server's name, as its sign-in pages show it.
    pub fn server_name(&self) -> &str {
        &self.info.name
    }

    /// Finish with the redirect the browser came back with (`…/callback?code=…&state=…`).
    pub async fn complete_from_callback(self, callback: &str) -> Result<Session> {
        let u = url::Url::parse(callback).context("the sign-in came back with an invalid address")?;
        let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
        if q.get("state").map(String::as_str) != Some(self.state.as_str()) {
            bail!("this answer does not match the sign-in started by the app; try again");
        }
        let code = q.get("code").context("the sign-in came back without a code")?.clone();
        self.complete(&code).await
    }

    /// Trade the one-time code for a session, save it and return it.
    pub async fn complete(self, code: &str) -> Result<Session> {
        let tok = self.api.token(code, &self.verifier).await?;
        let session = Session {
            server: self.api.base.clone(),
            token: tok.token,
            expires_at: tok.expires_at,
            user: tok.user,
            relay_url: self.info.relay_url,
            server_name: self.info.name,
        };
        session.save()?;
        Ok(session)
    }
}

/// Serve the browser's redirect: `GET /callback?code=…&state=…`.
async fn wait_callback(listener: &tokio::net::TcpListener, state: &str, server_name: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = Vec::with_capacity(2048);
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
            match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
        let Ok(u) = url::Url::parse(&format!("http://localhost{target}")) else { continue };
        if u.path() != "/callback" {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            continue;
        }
        let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
        let ok = q.get("state").map(String::as_str) == Some(state) && q.contains_key("code");
        let (title, text) = if ok {
            ("Signed in", "You are signed in. You can close this tab and go back to rdesk.")
        } else {
            ("Sign-in failed", "This answer does not match the sign-in started by the app. Try again from rdesk.")
        };
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>{title}</title>\
             <body style=\"font:16px system-ui;display:flex;min-height:90vh;align-items:center;justify-content:center\">\
             <div><p style=\"color:#667085\">{}</p><h2>{title}</h2><p>{text}</p></div>",
            server_name.replace('<', "&lt;")
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.shutdown().await;
        if ok {
            return Ok(q["code"].clone());
        }
    }
}

/// Open `url` in the default browser.
pub fn open_browser(url: &str) -> Result<()> {
    webbrowser::open(url).with_context(|| format!("opening the browser; open this address yourself: {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registration belongs to the computer: with the Windows service it lives in the
    /// machine folder, next to the host's identity.
    #[test]
    fn registration_is_a_host_file() {
        assert!(config::HOST_FILES.contains(&MACHINE_FILE));
        assert!(!config::HOST_FILES.contains(&ACCOUNTS_FILE), "the sign-ins belong to the person");
        assert!(!config::HOST_FILES.contains(&SESSION_FILE), "the sign-in belongs to the person");
    }

    #[test]
    fn server_addresses() {
        assert_eq!(normalize_server("rdesk.example.com").unwrap(), "https://rdesk.example.com");
        assert_eq!(normalize_server(" https://x.y/ ").unwrap(), "https://x.y");
        assert_eq!(normalize_server("http://127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert!(normalize_server("").is_err());
        assert!(normalize_server("ftp://x").is_err());
    }
}
