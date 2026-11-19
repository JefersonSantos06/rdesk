//! Signing in to the rdesk server, and its API.
//!
//! The sign-in runs in the browser (Google/GitHub + second factor) and comes back to a
//! one-shot HTTP listener on 127.0.0.1; the app then trades the code for a session
//! (PKCE). The session is kept in `session.json` (owner-only) and lasts 10 days.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use rdesk_proto::api::{
    self, API_VERSION, AddMember, AddUser, AdminUser, AgentSync, AgentSyncReply, ApiError, Group, GroupRef, GroupRole,
    HistoryEntry, Machine, Managers, NewGroup, RegisterMachine, Registered, ServerInfo, SetManagers, SetRole,
    SetShares, ShareRole, Shares, TicketRequest, TicketResponse, TokenRequest, TokenResponse, UpdateMachine,
    UpdateUser, UserInfo, UserRef, Visibility,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config;

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

impl Session {
    /// The saved session, if there is one that has not expired.
    pub fn load() -> Option<Self> {
        let path = config::config_path(SESSION_FILE).ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        let s: Self = serde_json::from_str(&text).ok()?;
        (!s.expired()).then_some(s)
    }

    pub fn save(&self) -> Result<()> {
        config::ensure_config_dir()?;
        config::write_private(&config::config_path(SESSION_FILE)?, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn clear() -> Result<()> {
        remove_if_present(&config::config_path(SESSION_FILE)?)
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
    http: reqwest::Client,
}

impl Api {
    pub fn new(server: &str, token: Option<String>) -> Result<Self> {
        install_crypto();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("rdesk/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { base: normalize_server(server)?, token, http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
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

    // ---- groups ---------------------------------------------------------------------

    pub async fn groups(&self) -> Result<Vec<Group>> {
        self.get(api::PATH_GROUPS).await
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
    let api = Api::new(server, None)?;
    let info = api.info().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("opening the sign-in callback")?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());
    let state = random_b64(16);
    let verifier = random_b64(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut u = url::Url::parse(&api.url(api::PATH_LOGIN))?;
    u.query_pairs_mut()
        .append_pair("redirect_uri", &redirect)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("device", device);
    open(u.as_str())?;
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_callback(&listener, &state, &info.name))
        .await
        .map_err(|_| anyhow!("the sign-in was not finished within {} minutes", LOGIN_TIMEOUT.as_secs() / 60))??;
    let tok = api.token(&code, &verifier).await?;
    let session = Session {
        server: api.base.clone(),
        token: tok.token,
        expires_at: tok.expires_at,
        user: tok.user,
        relay_url: info.relay_url,
        server_name: info.name,
    };
    session.save()?;
    Ok(session)
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

    #[test]
    fn server_addresses() {
        assert_eq!(normalize_server("rdesk.example.com").unwrap(), "https://rdesk.example.com");
        assert_eq!(normalize_server(" https://x.y/ ").unwrap(), "https://x.y");
        assert_eq!(normalize_server("http://127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert!(normalize_server("").is_err());
        assert!(normalize_server("ftp://x").is_err());
    }
}
