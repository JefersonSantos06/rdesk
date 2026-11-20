//! The JSON API used by the app (`rdesk_proto::api`).

use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rdesk_proto::api::{
    API_VERSION, AddUser, AdminUser, ApiError, ServerInfo, TokenRequest, TokenResponse, UpdateUser, UserInfo,
};

use crate::db::Session;
use crate::oauth::pkce_challenge;
use crate::state::{CODE_TTL, Shared};

pub struct Error(pub StatusCode, pub String);

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> Error {
    Error(code, msg.into())
}

/// A request with a valid session (`Authorization: Bearer …`).
pub struct Authed(pub Session);

impl FromRequestParts<Shared> for Authed {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, st: &Shared) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "not signed in"))?;
        match st.db.session(token.trim())? {
            Some(s) => Ok(Self(s)),
            None => Err(err(StatusCode::UNAUTHORIZED, "session expired or revoked: sign in again")),
        }
    }
}

/// A system admin's session.
pub struct Admin(pub Session);

impl FromRequestParts<Shared> for Admin {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, st: &Shared) -> Result<Self, Self::Rejection> {
        let Authed(s) = Authed::from_request_parts(parts, st).await?;
        if !s.user.is_admin {
            return Err(err(StatusCode::FORBIDDEN, "admins only"));
        }
        Ok(Self(s))
    }
}

pub async fn info(State(st): State<Shared>) -> Json<ServerInfo> {
    Json(ServerInfo {
        name: st.cfg.name.clone(),
        api: API_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        relay_url: st.relay_url(),
        relay_quic_port: st.cfg.relay_quic().then_some(st.cfg.relay_quic_port),
        server_key: hex::encode(st.key.public().as_bytes()),
        providers: st.cfg.providers(),
    })
}

/// `POST /api/token`: the app trades the browser's one-time code for a session.
pub async fn token(State(st): State<Shared>, Json(req): Json<TokenRequest>) -> Result<Json<TokenResponse>, Error> {
    let pending = st.codes.lock().unwrap().remove(&req.code);
    let p = pending.ok_or_else(|| err(StatusCode::BAD_REQUEST, "unknown or used code: sign in again"))?;
    if p.created.elapsed() > CODE_TTL {
        return Err(err(StatusCode::BAD_REQUEST, "the sign-in took too long: sign in again"));
    }
    if pkce_challenge(&req.code_verifier) != p.code_challenge {
        return Err(err(StatusCode::BAD_REQUEST, "code verifier does not match"));
    }
    let user =
        st.db.user(p.user_id)?.filter(|u| !u.disabled).ok_or_else(|| err(StatusCode::FORBIDDEN, "account disabled"))?;
    let (token, expires_at) = st.db.create_session(user.id, &p.device)?;
    Ok(Json(TokenResponse { token, expires_at, user: user.info() }))
}

pub async fn me(Authed(s): Authed) -> Json<UserInfo> {
    Json(s.user.info())
}

pub async fn logout(State(st): State<Shared>, Authed(s): Authed) -> Result<StatusCode, Error> {
    st.db.revoke_session(s.id)?;
    st.db.audit(Some(s.user.id), "logout", "");
    Ok(StatusCode::NO_CONTENT)
}

fn admin_view(st: &Shared, u: &crate::db::User) -> AdminUser {
    AdminUser {
        id: u.id,
        email: u.email.clone(),
        name: u.name.clone(),
        is_admin: u.is_admin,
        disabled: u.disabled,
        has_totp: u.totp_secret.is_some(),
        passkeys: st.db.passkeys(u.id).map(|p| p.len() as u32).unwrap_or(0),
        created_at: u.created_at,
        last_login: u.last_login,
    }
}

pub async fn admin_users(State(st): State<Shared>, _: Admin) -> Result<Json<Vec<AdminUser>>, Error> {
    Ok(Json(st.db.users()?.iter().map(|u| admin_view(&st, u)).collect()))
}

pub async fn admin_add_user(
    State(st): State<Shared>,
    Admin(me): Admin,
    Json(req): Json<AddUser>,
) -> Result<Json<AdminUser>, Error> {
    let u = st.db.add_user(&req.email, req.is_admin)?;
    st.db.audit(Some(me.user.id), "user-added", &format!("{}{}", u.email, if u.is_admin { " (admin)" } else { "" }));
    Ok(Json(admin_view(&st, &u)))
}

pub async fn admin_update_user(
    State(st): State<Shared>,
    Admin(me): Admin,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUser>,
) -> Result<Json<AdminUser>, Error> {
    let u = st.db.user(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))?;
    if let Some(a) = req.is_admin {
        st.db.set_admin(id, a)?;
    }
    if let Some(d) = req.disabled {
        if d {
            st.db.ensure_not_last_admin(id)?;
        }
        st.db.set_disabled(id, d)?;
        if d {
            // Their tickets stop working on every computer right away.
            let touched = st.db.revoke_user(id)?;
            st.wake(&touched);
        }
    }
    if req.reset_second_factor {
        st.db.reset_second_factor(id)?;
        st.db.audit(Some(me.user.id), "2fa-reset", &u.email);
    }
    st.db.audit(Some(me.user.id), "user-updated", &format!("{} {:?}", u.email, (req.is_admin, req.disabled)));
    let u = st.db.user(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))?;
    Ok(Json(admin_view(&st, &u)))
}

pub async fn admin_remove_user(
    State(st): State<Shared>,
    Admin(me): Admin,
    Path(id): Path<i64>,
) -> Result<StatusCode, Error> {
    let u = st.db.user(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such user"))?;
    st.db.ensure_not_last_admin(id)?;
    let touched = st.db.revoke_user(id)?;
    st.db.remove_user(id)?;
    st.wake(&touched);
    st.db.audit(Some(me.user.id), "user-removed", &u.email);
    Ok(StatusCode::NO_CONTENT)
}
