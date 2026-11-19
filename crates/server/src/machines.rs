//! Machines API: register, list, rename, visibility, sharing, tickets, and the host's own
//! sync (long poll with its machine token).

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use base64::Engine as _;
use rdesk_proto::api::{
    AgentSync, AgentSyncReply, GroupRef, HistoryEntry, Machine, Managers, RegisterMachine, Registered, Revocation,
    SetManagers, SetShares, Shares, TicketRequest, TicketResponse, UpdateMachine, UserRef, Visibility,
    register_message,
};
use rdesk_proto::ticket::{SignedTicket, TICKET_MAX_SECS, TicketClaims};

use crate::access::MachineRow;
use crate::api::{Admin, Authed, Error};
use crate::db::{User, now};
use crate::state::Shared;

fn err(code: StatusCode, msg: impl Into<String>) -> Error {
    Error(code, msg.into())
}

fn user_ref(u: &User) -> UserRef {
    UserRef { id: u.id, email: u.email.clone(), name: u.name.clone() }
}

fn parse_endpoint(hex_id: &str) -> Result<[u8; 32], Error> {
    let b = hex::decode(hex_id.trim()).map_err(|_| err(StatusCode::BAD_REQUEST, "endpoint id is not hex"))?;
    b.try_into().map_err(|_| err(StatusCode::BAD_REQUEST, "endpoint id must be 32 bytes"))
}

/// The API view of a machine, as seen by `viewer`.
pub fn view(st: &Shared, m: &MachineRow, viewer: Option<i64>) -> Result<Machine, Error> {
    let owner = st.db.user(m.owner_id)?.map(|u| user_ref(&u)).unwrap_or(UserRef {
        id: m.owner_id,
        email: "(removed)".into(),
        name: String::new(),
    });
    let online = parse_endpoint(&m.endpoint_id)
        .ok()
        .and_then(|b| iroh_base::EndpointId::from_bytes(&b).ok())
        .is_some_and(|id| st.online.contains(&id));
    let (access, can_manage) = match viewer {
        Some(u) => (st.db.access_of(u, m)?, st.db.can_manage(u, m)?),
        None => (rdesk_proto::Access::NONE, false),
    };
    Ok(Machine {
        id: m.id,
        endpoint_id: m.endpoint_id.clone(),
        name: m.name.clone(),
        os: m.os.clone(),
        owner,
        visibility: m.visibility,
        online,
        in_session: online && m.in_session,
        last_seen: m.last_seen,
        addrs: m.addrs.clone(),
        can_access: !access.is_none(),
        access,
        is_owner: viewer == Some(m.owner_id),
        can_manage,
    })
}

/// Only who registered it (unregister, choose managers).
fn owned(st: &Shared, id: i64, user: &User) -> Result<MachineRow, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if m.owner_id != user.id {
        return Err(err(StatusCode::FORBIDDEN, "only who registered this computer can do this"));
    }
    Ok(m)
}

/// The owner or a co-manager (rename, visibility, sharing, history).
fn managed(st: &Shared, id: i64, user: &User) -> Result<MachineRow, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if !st.db.can_manage(user.id, &m)? {
        return Err(err(StatusCode::FORBIDDEN, "only who registered this computer and its managers can change it"));
    }
    Ok(m)
}

/// `GET /api/machines`: the computers this user may access, plus their own.
pub async fn list(State(st): State<Shared>, Authed(s): Authed) -> Result<Json<Vec<Machine>>, Error> {
    let mut out = Vec::new();
    for m in st.db.machines()? {
        if m.owner_id == s.user.id || st.db.can_access(s.user.id, &m)? {
            out.push(view(&st, &m, Some(s.user.id))?);
        }
    }
    Ok(Json(out))
}

/// `GET /api/admin/machines`: every computer (system admins only; seeing is not accessing).
pub async fn admin_list(State(st): State<Shared>, Admin(s): Admin) -> Result<Json<Vec<Machine>>, Error> {
    let ms = st.db.machines()?;
    Ok(Json(ms.iter().map(|m| view(&st, m, Some(s.user.id))).collect::<Result<_, _>>()?))
}

/// `POST /api/machines`: register the computer the app runs on.
pub async fn register(
    State(st): State<Shared>,
    Authed(s): Authed,
    Json(req): Json<RegisterMachine>,
) -> Result<Json<Registered>, Error> {
    let endpoint = parse_endpoint(&req.endpoint_id)?;
    // Proof that the app holds this host's key (nobody can claim someone else's ID).
    let key =
        iroh_base::PublicKey::from_bytes(&endpoint).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid host ID"))?;
    let sig: [u8; 64] = hex::decode(&req.signature)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad signature"))?;
    if (now() - req.timestamp).abs() > 300 {
        return Err(err(StatusCode::BAD_REQUEST, "this computer's clock is off by more than 5 minutes"));
    }
    key.verify(&register_message(s.user.id, &endpoint, req.timestamp), &iroh_base::Signature::from_bytes(&sig))
        .map_err(|_| err(StatusCode::BAD_REQUEST, "the registration was not signed by this computer"))?;
    let endpoint_hex = hex::encode(endpoint);
    let (m, token) = st.db.register_machine(s.user.id, &endpoint_hex, &req.name, &req.os, req.visibility)?;
    st.db.audit(Some(s.user.id), "machine-registered", &format!("{} {}", m.name, &endpoint_hex[..12]));
    Ok(Json(Registered {
        machine: view(&st, &m, Some(s.user.id))?,
        token,
        server_key: hex::encode(st.key.public().as_bytes()),
        relay_url: st.relay_url(),
        server_name: st.cfg.name.clone(),
    }))
}

/// `PATCH /api/machines/{id}`: rename, change who may see it.
pub async fn update(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
    Json(req): Json<UpdateMachine>,
) -> Result<Json<Machine>, Error> {
    let m = managed(&st, id, &s.user)?;
    if let Some(n) = &req.name {
        st.db.rename_machine(id, n)?;
    }
    if let Some(v) = req.visibility {
        let before = st.db.snapshot(Some(&[id]))?;
        st.db.set_visibility(id, v)?;
        let touched = st.db.revoke_lost(before)?;
        st.wake(&touched);
        st.db.audit(Some(s.user.id), "machine-visibility", &format!("{} → {v:?}", m.name));
    }
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    Ok(Json(view(&st, &m, Some(s.user.id))?))
}

/// `DELETE /api/machines/{id}`: unregister (the host stops being reachable through the server).
pub async fn remove(State(st): State<Shared>, Authed(s): Authed, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    let m = owned(&st, id, &s.user)?;
    st.db.remove_machine(id)?;
    st.wake(&[id]);
    st.db.audit(Some(s.user.id), "machine-unregistered", &m.name);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/machines/{id}/shares`.
pub async fn get_shares(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
) -> Result<Json<Shares>, Error> {
    managed(&st, id, &s.user)?;
    let (users, groups) = st.db.shares(id)?;
    let users =
        users.into_iter().filter_map(|(u, r)| st.db.user(u).ok().flatten().map(|u| (user_ref(&u), r))).collect();
    let groups = groups
        .into_iter()
        .filter_map(|(g, r)| st.db.group(g).ok().flatten().map(|g| (GroupRef { id: g.id, name: g.name }, r)))
        .collect();
    Ok(Json(Shares { users, groups }))
}

/// `PUT /api/machines/{id}/shares`: replace the list.
pub async fn set_shares(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
    Json(req): Json<SetShares>,
) -> Result<Json<Shares>, Error> {
    let m = managed(&st, id, &s.user)?;
    for (u, _) in &req.users {
        st.db.user(*u)?.ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("no user {u}")))?;
    }
    for (g, _) in &req.groups {
        st.db.group(*g)?.ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("no group {g}")))?;
    }
    let before = st.db.snapshot(Some(&[id]))?;
    st.db.set_shares(id, &req.users, &req.groups)?;
    let touched = st.db.revoke_lost(before)?;
    st.wake(&touched);
    st.db.audit(
        Some(s.user.id),
        "machine-shared",
        &format!("{}: {} user(s), {} group(s)", m.name, req.users.len(), req.groups.len()),
    );
    get_shares(State(st), Authed(s), Path(id)).await
}

/// `GET /api/machines/{id}/managers` (owner only).
pub async fn get_managers(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
) -> Result<Json<Managers>, Error> {
    owned(&st, id, &s.user)?;
    let users =
        st.db.managers(id)?.into_iter().filter_map(|u| st.db.user(u).ok().flatten()).map(|u| user_ref(&u)).collect();
    Ok(Json(Managers { users }))
}

/// `PUT /api/machines/{id}/managers` (owner only): who else may manage it.
pub async fn set_managers(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
    Json(req): Json<SetManagers>,
) -> Result<Json<Managers>, Error> {
    let m = owned(&st, id, &s.user)?;
    for u in &req.users {
        st.db.user(*u)?.ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("no user {u}")))?;
    }
    let users: Vec<i64> = req.users.iter().copied().filter(|u| *u != m.owner_id).collect();
    let before = st.db.snapshot(Some(&[id]))?;
    st.db.set_managers(id, &users)?;
    let touched = st.db.revoke_lost(before)?;
    st.wake(&touched);
    st.db.audit(Some(s.user.id), "machine-managers", &format!("{}: {} manager(s)", m.name, users.len()));
    get_managers(State(st), Authed(s), Path(id)).await
}

/// `GET /api/machines/{id}/history`: the last sessions (managers, and system admins).
pub async fn history(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
) -> Result<Json<Vec<HistoryEntry>>, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if !s.user.is_admin && !st.db.can_manage(s.user.id, &m)? {
        return Err(err(StatusCode::FORBIDDEN, "only who manages this computer can see its history"));
    }
    Ok(Json(st.db.history(id, 200)?))
}

/// `POST /api/machines/{id}/ticket`: permission to connect, for one client endpoint.
pub async fn ticket(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
    Json(req): Json<TicketRequest>,
) -> Result<Json<TicketResponse>, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    let access = st.db.access_of(s.user.id, &m)?;
    if access.is_none() {
        return Err(err(StatusCode::FORBIDDEN, "you do not have access to this computer"));
    }
    let client = parse_endpoint(&req.client)?;
    let machine = parse_endpoint(&m.endpoint_id)?;
    // Hosts void tickets issued at or before a revocation (second resolution): one issued
    // right after it, in the same second, must still count as newer.
    let t = match st.db.last_revocation(m.id, s.user.id)? {
        Some(r) => now().max(r + 1),
        None => now(),
    };
    let mut ticket_id = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut ticket_id);
    let claims = TicketClaims {
        ticket_id,
        machine,
        client,
        user_id: s.user.id,
        user_email: s.user.email.clone(),
        user_name: s.user.name.clone(),
        role: access.to_text(),
        issued_at: t,
        expires_at: t + TICKET_MAX_SECS,
    };
    let bytes = claims.to_bytes();
    let sig = st.key.sign(&SignedTicket::signed_message(&bytes));
    st.db.audit(Some(s.user.id), "ticket", &format!("{} ({})", m.name, access.label()));
    let signed = SignedTicket { claims: bytes, signature: sig.to_bytes() };
    Ok(Json(TicketResponse {
        ticket: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signed.to_bytes()),
        expires_at: claims.expires_at,
        machine: view(&st, &m, Some(s.user.id))?,
    }))
}

/// The machine behind a machine token.
pub struct MachineAuth(pub MachineRow);

impl axum::extract::FromRequestParts<Shared> for MachineAuth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, st: &Shared) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no machine token"))?;
        match st.db.machine_by_token(token.trim())? {
            Some(m) => Ok(Self(m)),
            None => Err(err(StatusCode::UNAUTHORIZED, "this computer is not registered (any more)")),
        }
    }
}

/// `POST /api/agent/sync`: the host reports itself, then waits up to 25 s for news.
pub async fn agent_sync(
    State(st): State<Shared>,
    MachineAuth(m): MachineAuth,
    Json(req): Json<AgentSync>,
) -> Result<Json<AgentSyncReply>, Error> {
    st.db.machine_seen(m.id, &req.addrs, &req.os, req.in_session)?;
    if !req.sessions.is_empty() {
        st.db.record_sessions(m.id, &req.sessions)?;
    }
    let notify = st.machine_notify(m.id);
    let wait = notify.notified();
    tokio::pin!(wait);
    wait.as_mut().enable();
    let (cursor, _) = st.db.revocations(m.id)?;
    let current_key = hex::encode(st.key.public().as_bytes());
    if cursor <= req.cursor && req.server_key == current_key {
        let _ = tokio::time::timeout(Duration::from_secs(25), wait).await;
    }
    // Unregistered meanwhile: the next call gets 401.
    let m = st.db.machine(m.id)?.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "this computer was unregistered"))?;
    let (cursor, revs) = st.db.revocations(m.id)?;
    Ok(Json(AgentSyncReply {
        cursor,
        revocations: revs.into_iter().map(|(user_id, before)| Revocation { user_id, before }).collect(),
        machine: view(&st, &m, None)?,
        server_keys: st.ticket_keys(),
    }))
}

/// `GET /api/users`: everyone on the server (to share with).
pub async fn users(State(st): State<Shared>, _: Authed) -> Result<Json<Vec<UserRef>>, Error> {
    Ok(Json(st.db.users()?.iter().filter(|u| !u.disabled).map(user_ref).collect()))
}

/// Visibility as text for logs and the CLI.
pub fn visibility_label(v: Visibility) -> &'static str {
    crate::access::visibility_str(v)
}
