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
