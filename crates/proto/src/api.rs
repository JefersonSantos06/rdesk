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
    /// Forget their second factors (authenticator and passkeys) and end their sessions:
    /// their next sign-in sets up a new one (a lost phone). Older servers ignore it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_second_factor: bool,
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

/// The header with a presence proof: the app making the request runs on that computer.
pub const HEADER_HERE: &str = "x-rdesk-here";
/// Signed by the host key for a presence proof.
pub const HERE_DOMAIN: &[u8] = b"rdesk-here-v1";
/// A presence proof counts this long (seconds, either way: clocks drift).
pub const HERE_MAX_AGE: i64 = 300;

/// What a presence proof allows on the computer it was signed on, whoever is signed in
/// there: seeing it (its owner and who it is shared with), or managing it like its
/// managers do (the app asks for the system's administrator first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Here {
    View,
    Manage,
}

impl Here {
    fn byte(self) -> u8 {
        match self {
            Here::View => 1,
            Here::Manage => 2,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Here::View => "view",
            Here::Manage => "manage",
        }
    }
}

/// A presence proof, as sent in [`HEADER_HERE`]: `scope.user.timestamp.endpoint.signature`
/// (the last two in hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HereProof {
    pub scope: Here,
    pub user_id: i64,
    pub timestamp: i64,
    /// The host ID of the computer.
    pub endpoint: [u8; 32],
    pub signature: [u8; 64],
}

impl HereProof {
    /// What the signature covers.
    pub fn message(scope: Here, user_id: i64, endpoint: &[u8; 32], timestamp: i64) -> Vec<u8> {
        let mut m = HERE_DOMAIN.to_vec();
        m.push(scope.byte());
        m.extend_from_slice(&user_id.to_le_bytes());
        m.extend_from_slice(endpoint);
        m.extend_from_slice(&timestamp.to_le_bytes());
        m
    }

    pub fn signed_message(&self) -> Vec<u8> {
        Self::message(self.scope, self.user_id, &self.endpoint, self.timestamp)
    }

    pub fn to_header(&self) -> String {
        format!(
            "{}.{}.{}.{}.{}",
            self.scope.as_str(),
            self.user_id,
            self.timestamp,
            hex_encode(&self.endpoint),
            hex_encode(&self.signature)
        )
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.trim().split('.');
        let scope = match it.next()? {
            "view" => Here::View,
            "manage" => Here::Manage,
            _ => return None,
        };
        let user_id = it.next()?.parse().ok()?;
        let timestamp = it.next()?.parse().ok()?;
        let endpoint = hex_decode(it.next()?)?.try_into().ok()?;
        let signature = hex_decode(it.next()?)?.try_into().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(Self { scope, user_id, timestamp, endpoint, signature })
    }
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
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
    /// It also lets people in with its own password or SSH key on the local network (or
    /// Tailscale), without the server's permission.
    #[serde(default)]
    pub direct: bool,
    /// The rdesk version it runs (from its last sync).
    #[serde(default)]
    pub version: String,
    /// Gets released versions with the test computers.
    #[serde(default)]
    pub ring: Ring,
    /// The request proved it comes from this computer (see [`HereProof`]): whoever is
    /// signed in there sees it, and may manage it with [`Here::Manage`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub here: Option<Here>,
    /// Its network cards are known: another computer on its network can wake it
    /// ([`PATH_MACHINES`]`/{id}/wake`).
    #[serde(default)]
    pub can_wake: bool,
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
    /// A terminal only (no screen): for servers and boards without a desktop.
    Terminal,
}

impl ShareRole {
    pub const ALL: [ShareRole; 4] = [ShareRole::Control, ShareRole::View, ShareRole::Files, ShareRole::Terminal];

    pub fn access(self) -> crate::Access {
        match self {
            ShareRole::Control => crate::Access::FULL,
            ShareRole::View => crate::Access::VIEW,
            ShareRole::Files => crate::Access::FILES,
            ShareRole::Terminal => crate::Access::TERMINAL,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ShareRole::Control => "control",
            ShareRole::View => "view",
            ShareRole::Files => "files",
            ShareRole::Terminal => "terminal",
        }
    }

    pub fn parse(s: &str) -> Option<ShareRole> {
        match s {
            "control" => Some(ShareRole::Control),
            "view" => Some(ShareRole::View),
            "files" => Some(ShareRole::Files),
            "terminal" => Some(ShareRole::Terminal),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ShareRole::Control => "full control",
            ShareRole::View => "view only",
            ShareRole::Files => "files only",
            ShareRole::Terminal => "terminal only",
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Direct access is on: its own password or key also open it on the local network.
    #[serde(default)]
    pub direct: bool,
    /// The newest update offer the host has already seen (its version): the server answers
    /// at once only when there is a newer one.
    #[serde(default)]
    pub update_seen: String,
    /// Its network cards on local networks ("aa:bb:cc:dd:ee:ff"), to be woken up later.
    #[serde(default)]
    pub macs: Vec<String>,
}

/// Wake a computer on this one's network: send Wake-on-LAN packets for `macs`, also to the
/// broadcast address of the networks of `addrs` (the sleeping computer's last addresses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeRequest {
    pub macs: Vec<String>,
    pub addrs: Vec<String>,
}

/// Answer to `POST /api/machines/{id}/wake`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeReply {
    /// How many computers on its network were asked to send the packets.
    pub helpers: u32,
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
    /// The version released for this computer, if any.
    #[serde(default)]
    pub update: Option<UpdateOffer>,
    /// Computers on this one's network to wake up.
    #[serde(default)]
    pub wake: Vec<WakeRequest>,
}

/// Tickets of `user_id` issued at or before `before` are void on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revocation {
    pub user_id: i64,
    pub before: i64,
}

// ---------------------------------------------------------------------------
// Adding a computer without signing in on it: the owner's e-mail (approved in the app)
// or an enrollment key (no approval).
// ---------------------------------------------------------------------------

/// No authentication: the host proves it holds its key by signing [`enroll_message`].
pub const PATH_ENROLL: &str = "/api/enroll";
/// Computers waiting for approval (the requesting user's, plus ownerless ones for admins).
pub const PATH_MACHINE_REQUESTS: &str = "/api/machine-requests";
pub const PATH_ENROLL_KEYS: &str = "/api/enroll-keys";

/// Signed by the host key when it asks to be added.
pub const ENROLL_DOMAIN: &[u8] = b"rdesk-enroll-v1";
/// Longest the server holds an enrollment request open waiting for the approval.
pub const ENROLL_MAX_WAIT_SECS: u32 = 25;
/// Enrollment keys start with this, so they are recognizable in config files.
pub const ENROLL_KEY_PREFIX: &str = "rdk_";

/// What the enrollment signature covers.
pub fn enroll_message(endpoint: &[u8; 32], timestamp: i64) -> Vec<u8> {
    let mut m = ENROLL_DOMAIN.to_vec();
    m.extend_from_slice(endpoint);
    m.extend_from_slice(&timestamp.to_le_bytes());
    m
}

/// The code shown on the host and next to the request in the app, to tell computers
/// apart before approving: the first 8 hex digits of the host ID, `3F2A-91C0`.
pub fn approval_code(endpoint_hex: &str) -> String {
    let h: String = endpoint_hex.chars().filter(char::is_ascii_hexdigit).take(8).collect::<String>().to_uppercase();
    if h.len() < 8 { h } else { format!("{}-{}", &h[..4], &h[4..]) }
}

/// `POST /api/enroll`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub endpoint_id: String,
    pub name: String,
    pub os: String,
    pub timestamp: i64,
    /// Hex Ed25519 signature of [`enroll_message`] by the host key.
    pub signature: String,
    /// Who should own it (approves it in the app). `None` with no key: the system admins.
    #[serde(default)]
    pub owner: Option<String>,
    /// An enrollment key: registered at once, owned by whoever created the key.
    #[serde(default)]
    pub key: Option<String>,
    /// How long the server may wait for the approval before answering "pending"
    /// (at most [`ENROLL_MAX_WAIT_SECS`]).
    #[serde(default)]
    pub wait_secs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum EnrollReply {
    Registered(Box<Registered>),
    /// Waiting for the owner (or an admin) to approve it; ask again.
    Pending {
        code: String,
    },
    /// The owner refused it.
    Rejected,
}

/// `GET /api/machine-requests`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineRequest {
    pub id: i64,
    pub endpoint_id: String,
    /// See [`approval_code`].
    pub code: String,
    pub name: String,
    pub os: String,
    /// `None`: asked without an owner, for the system admins.
    pub owner: Option<UserRef>,
    pub created_at: i64,
    pub last_seen: i64,
}

/// `POST /api/machine-requests/{id}/approve`: whoever approves owns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveMachine {
    #[serde(default)]
    pub name: Option<String>,
    pub visibility: Visibility,
}

/// `GET /api/enroll-keys`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollKey {
    pub id: i64,
    pub name: String,
    /// The first characters, to recognize it (`rdk_AbC1…`).
    pub prefix: String,
    /// Can add many computers (otherwise only one).
    pub reusable: bool,
    /// What the computers it adds start with.
    pub visibility: Visibility,
    pub shares: Shares,
    pub created_at: i64,
    pub expires_at: i64,
    /// Computers added with it.
    pub uses: i64,
    pub revoked: bool,
}

/// `POST /api/enroll-keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEnrollKey {
    pub name: String,
    #[serde(default)]
    pub reusable: bool,
    /// 1 to 365.
    pub expires_in_days: u32,
    pub visibility: Visibility,
    #[serde(default)]
    pub shares: SetShares,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedEnrollKey {
    /// Shown only now: the server keeps just its hash.
    pub key: String,
    pub info: EnrollKey,
}

// ---- updates (phase 8) ------------------------------------------------------------

/// `GET` (admins): versions found on GitHub and what is released.
pub const PATH_ADMIN_RELEASES: &str = "/api/admin/releases";
/// `GET ?machine=<host ID>`, no authentication: the version released for that computer
/// (for the Linux updater, which runs apart from the host).
pub const PATH_UPDATE_LATEST: &str = "/api/update/latest";

/// What a computer may update to: the release manifest and its signature as published.
/// The host checks the signature with the keys built into it, so the server can not
/// hand it anything rdesk did not sign; files come from `download_base` + their name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateOffer {
    pub version: String,
    /// `release.json`, exactly as signed.
    pub manifest: String,
    /// `release.json.sig` (hex).
    pub signature: String,
    pub download_base: String,
}

/// Which computers get a released version first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Ring {
    /// Test computers (marked by an admin).
    Test,
    #[default]
    All,
}

/// `GET /api/admin/releases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Releases {
    /// Newest first.
    pub releases: Vec<ReleaseInfo>,
    /// Released to the test computers (they also get what is released to everyone).
    pub test: Option<String>,
    /// Released to everyone.
    pub all: Option<String>,
    /// This server's version.
    pub server_version: String,
    /// When GitHub was last asked, and what went wrong if it failed.
    pub last_check: Option<i64>,
    pub check_error: Option<String>,
    /// The server can update itself (systemd, not a container).
    #[serde(default)]
    pub server_can_update: bool,
    /// What its last update did ("updated to 0.4.0", "installing 0.4.1…", an error).
    #[serde(default)]
    pub server_update: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub version: String,
    /// The release notes (GitHub's text).
    pub notes: String,
    pub published_at: i64,
    pub protocol: u16,
    /// Platforms it has files for.
    pub platforms: Vec<String>,
    /// Computers running it now.
    pub running: usize,
}

/// `POST /api/admin/releases/{version}/release`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReleaseTo {
    pub ring: Ring,
}

/// `PUT /api/admin/machines/{id}/ring`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SetRing {
    pub ring: Ring,
}
