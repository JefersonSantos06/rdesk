//! JSON types of the rdesk-server HTTP API, shared by the server and the app.
//!
//! Authentication: `Authorization: Bearer <session token>` (from [`TokenResponse`]).
//! Errors come back as a non-2xx status with an [`ApiError`] body.

use serde::{Deserialize, Serialize};

/// Bumped on incompatible API changes.
pub const API_VERSION: u32 = 1;

pub const PATH_INFO: &str = "/api/info";
pub const PATH_TOKEN: &str = "/api/token";
pub const PATH_ME: &str = "/api/me";
pub const PATH_LOGOUT: &str = "/api/logout";
pub const PATH_ADMIN_USERS: &str = "/api/admin/users";
/// Browser login page; query: `redirect_uri`, `state`, `code_challenge`, `device`.
pub const PATH_LOGIN: &str = "/login";

/// `GET /api/info`, no authentication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub api: u32,
    pub version: String,
    /// The relay clients and hosts use (needs a session token).
    pub relay_url: String,
    /// UDP port of the relay's QUIC address discovery, when enabled.
    pub relay_quic_port: Option<u16>,
    /// Ed25519 public key (hex) the server signs access tickets with.
    pub server_key: String,
    /// Login providers offered on the login page: "google", "github", "dev".
    pub providers: Vec<String>,
}

/// `POST /api/token`: trade the code from the browser login for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRequest {
    pub code: String,
    /// PKCE verifier whose SHA-256 (base64url) was sent as `code_challenge`.
    pub code_verifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub token: String,
    /// Unix seconds; the session cannot be extended past this (log in again).
    pub expires_at: i64,
    pub user: UserInfo,
}

/// `GET /api/me`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub is_admin: bool,
}

/// `GET /api/admin/users` (system admins only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminUser {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub is_admin: bool,
    pub disabled: bool,
    /// Has finished the 2FA setup (TOTP and/or passkeys).
    pub has_totp: bool,
    pub passkeys: u32,
    pub created_at: i64,
    pub last_login: Option<i64>,
}

/// `POST /api/admin/users`: allow an e-mail to log in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddUser {
    pub email: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// `PATCH /api/admin/users/{id}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateUser {
    #[serde(default)]
    pub is_admin: Option<bool>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// ---------------------------------------------------------------------------
// Machines, sharing, groups, tickets (phase 3)
// ---------------------------------------------------------------------------

pub const PATH_MACHINES: &str = "/api/machines";
pub const PATH_GROUPS: &str = "/api/groups";
/// Everyone on the server (to pick whom to share with).
pub const PATH_USERS: &str = "/api/users";
pub const PATH_ADMIN_MACHINES: &str = "/api/admin/machines";
/// The machine's own long poll (authenticated with the machine token).
pub const PATH_AGENT_SYNC: &str = "/api/agent/sync";

/// Signed by the host key when registering, proving the app runs on that machine.
pub const REGISTER_DOMAIN: &[u8] = b"rdesk-register-v1";

/// What the registration signature covers.
pub fn register_message(user_id: i64, endpoint: &[u8; 32], timestamp: i64) -> Vec<u8> {
    let mut m = REGISTER_DOMAIN.to_vec();
    m.extend_from_slice(&user_id.to_le_bytes());
    m.extend_from_slice(endpoint);
    m.extend_from_slice(&timestamp.to_le_bytes());
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Only whoever registered it.
    #[default]
    Private,
    /// Also the users and groups it is shared with.
    Shared,
    /// Everyone on the server.
    Everyone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRef {
    pub id: i64,
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub id: i64,
    /// Host ID (hex).
    pub endpoint_id: String,
    pub name: String,
    pub os: String,
    pub owner: UserRef,
    pub visibility: Visibility,
    /// Connected to the server's relay right now.
    pub online: bool,
    /// Someone is controlling it.
    pub in_session: bool,
    pub last_seen: Option<i64>,
    /// Addresses it reported (LAN, public): tried directly before the relay.
    pub addrs: Vec<String>,
    /// The requesting user may connect.
    pub can_access: bool,
    /// What the requesting user may do there.
    pub access: crate::Access,
    /// The requesting user registered it (may also unregister it and choose its managers).
    pub is_owner: bool,
    /// The requesting user may rename it, change who sees it, share it, see its history.
    pub can_manage: bool,
}

/// What a share grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShareRole {
    /// Screen, keyboard and mouse, clipboard, files.
    #[default]
    Control,
    /// Watch the screen only.
    View,
    /// Files only (no screen).
    Files,
}

impl ShareRole {
    pub fn access(self) -> crate::Access {
        match self {
            ShareRole::Control => crate::Access::FULL,
            ShareRole::View => crate::Access::VIEW,
            ShareRole::Files => crate::Access::FILES,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ShareRole::Control => "control",
            ShareRole::View => "view",
            ShareRole::Files => "files",
        }
    }

    pub fn parse(s: &str) -> Option<ShareRole> {
        match s {
            "control" => Some(ShareRole::Control),
            "view" => Some(ShareRole::View),
            "files" => Some(ShareRole::Files),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ShareRole::Control => "full control",
            ShareRole::View => "view only",
            ShareRole::Files => "files only",
        }
    }
}

/// `POST /api/machines`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterMachine {
    pub endpoint_id: String,
    pub name: String,
    pub os: String,
    pub visibility: Visibility,
    pub timestamp: i64,
    /// Hex Ed25519 signature of [`register_message`] by the host key.
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registered {
    pub machine: Machine,
    /// The machine's own credential (relay + sync); keep it secret.
    pub token: String,
    /// Hex public key that signs tickets.
    pub server_key: String,
    pub relay_url: String,
    pub server_name: String,
}

/// `PATCH /api/machines/{id}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateMachine {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

/// `GET`/`PUT /api/machines/{id}/shares`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Shares {
    pub users: Vec<(UserRef, ShareRole)>,
    pub groups: Vec<(GroupRef, ShareRole)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetShares {
    /// `(user id, role)`.
    pub users: Vec<(i64, ShareRole)>,
    /// `(group id, role)`.
    pub groups: Vec<(i64, ShareRole)>,
}

/// `GET`/`PUT /api/machines/{id}/managers` (owner only): who else may manage it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Managers {
    pub users: Vec<UserRef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetManagers {
    pub users: Vec<i64>,
}

/// One session on a computer, as its host reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccessRecord {
    /// The host's session id (hex), to update the record when it ends.
    pub session: String,
    /// The server user (sessions through a ticket); `None` for a local login on the LAN.
    pub user_id: Option<i64>,
    /// User e-mail or the client's computer name.
    pub who: String,
    pub client: String,
    /// "server ticket", "password", "key …".
    pub method: String,
    pub access: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// `GET /api/machines/{id}/history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub who: String,
    pub client: String,
    pub method: String,
    pub access: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// `POST /api/machines/{id}/ticket`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketRequest {
    /// The endpoint ID (hex) the client will connect from.
    pub client: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketResponse {
    /// Base64url of the signed ticket.
    pub ticket: String,
    pub expires_at: i64,
    pub machine: Machine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupRole {
    /// Manages the group (members, roles, name).
    Admin,
    Member,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMember {
    pub user: UserRef,
    pub role: GroupRole,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    /// The requesting user's role (`None` for a system admin looking from outside).
    pub my_role: Option<GroupRole>,
    pub members: Vec<GroupMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewGroup {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMember {
    pub email: String,
    pub role: GroupRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRole {
    pub role: GroupRole,
}

/// `POST /api/agent/sync`: the machine reports itself and waits (up to ~25 s) for news.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSync {
    /// Last [`AgentSyncReply::cursor`] seen (0 at start).
    pub cursor: i64,
    pub addrs: Vec<String>,
    pub os: String,
    pub version: String,
    pub in_session: bool,
    /// Sessions that started or ended since the last sync.
    #[serde(default)]
    pub sessions: Vec<AccessRecord>,
    /// The ticket key (hex) the host trusts as current; the server answers at once when
    /// it changed (key rotation).
    #[serde(default)]
    pub server_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSyncReply {
    pub cursor: i64,
    /// Every revocation of the last 12 h (tickets can not be older).
    pub revocations: Vec<Revocation>,
    pub machine: Machine,
    /// Keys (hex) whose tickets are valid: the current one first, then the previous one
    /// for a while after a rotation.
    #[serde(default)]
    pub server_keys: Vec<String>,
}

/// Tickets of `user_id` issued at or before `before` are void on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revocation {
    pub user_id: i64,
    pub before: i64,
}
