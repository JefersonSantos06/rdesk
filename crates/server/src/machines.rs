//! Machines API: register, list, rename, visibility, sharing, tickets, and the host's own
//! sync (long poll with its machine token).

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use base64::Engine as _;
use rdesk_proto::api::{
    AgentSync, AgentSyncReply, GroupRef, HEADER_HERE, HERE_MAX_AGE, Here, HereProof, HistoryEntry, Machine, Managers,
    RegisterMachine, Registered, Revocation, SetManagers, SetShares, ShareRole, Shares, TicketRequest, TicketResponse,
    UpdateMachine, UserRef, Visibility, register_message,
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

pub(crate) fn parse_endpoint(hex_id: &str) -> Result<[u8; 32], Error> {
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
        direct: m.direct,
        version: m.version.clone(),
        ring: m.ring,
        here: None,
        can_wake: !m.macs.is_empty(),
    })
}

/// The /24 (IPv4) of each private address in `addrs`.
fn lan_nets(addrs: &[String]) -> Vec<[u8; 3]> {
    addrs
        .iter()
        .filter_map(|a| a.parse::<std::net::SocketAddr>().map(|s| s.ip()).or_else(|_| a.parse()).ok())
        .filter_map(|ip| match ip {
            std::net::IpAddr::V4(v4) if v4.is_private() => Some([v4.octets()[0], v4.octets()[1], v4.octets()[2]]),
            _ => None,
        })
        .collect()
}

/// `POST /api/machines/{id}/wake`: ask computers that are on and on the same network as
/// `id` (it reported an address in the same /24) to send it Wake-on-LAN packets.
pub async fn wake(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
) -> Result<Json<rdesk_proto::api::WakeReply>, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if st.db.access_of(s.user.id, &m)?.is_none() {
        return Err(err(StatusCode::FORBIDDEN, "you do not have access to this computer"));
    }
    if m.macs.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            "this computer never reported its network cards (it needs rdesk 0.7 or newer)",
        ));
    }
    let nets = lan_nets(&m.addrs);
    let online = |r: &crate::access::MachineRow| {
        parse_endpoint(&r.endpoint_id)
            .ok()
            .and_then(|b| iroh_base::EndpointId::from_bytes(&b).ok())
            .is_some_and(|e| st.online.contains(&e))
    };
    let helpers: Vec<i64> = st
        .db
        .machines()?
        .into_iter()
        .filter(|r| r.id != id && online(r) && lan_nets(&r.addrs).iter().any(|n| nets.contains(n)))
        .map(|r| r.id)
        .take(3)
        .collect();
    if helpers.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            "no computer that is on shares its network: one of them must be on to wake it",
        ));
    }
    let req = rdesk_proto::api::WakeRequest { macs: m.macs.clone(), addrs: m.addrs.clone() };
    for h in &helpers {
        st.wakes.lock().unwrap().entry(*h).or_default().push(req.clone());
        st.machine_notify(*h).notify_one();
    }
    st.db.audit(Some(s.user.id), "machine.wake", &m.name);
    Ok(Json(rdesk_proto::api::WakeReply { helpers: helpers.len() as u32 }))
}

/// A presence proof that came with the request (`x-rdesk-here`), its signature and age
/// checked: the app runs on that computer. Whom it is for and which computer is checked
/// per request ([`Presence::on`]).
pub struct Presence(Result<Option<HereProof>, String>);

impl axum::extract::FromRequestParts<Shared> for Presence {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _: &Shared) -> Result<Self, Self::Rejection> {
        let Some(v) = parts.headers.get(HEADER_HERE) else { return Ok(Self(Ok(None))) };
        Ok(Self(check_proof(v.to_str().unwrap_or_default(), now())))
    }
}

fn check_proof(header: &str, now: i64) -> Result<Option<HereProof>, String> {
    let p = HereProof::parse(header).ok_or("malformed presence proof")?;
    if (now - p.timestamp).abs() > HERE_MAX_AGE {
        return Err("this computer's clock is off by more than 5 minutes".into());
    }
    let key = iroh_base::PublicKey::from_bytes(&p.endpoint).map_err(|_| "invalid host ID in the presence proof")?;
    key.verify(&p.signed_message(), &iroh_base::Signature::from_bytes(&p.signature))
        .map_err(|_| "the presence proof was not signed by that computer")?;
    Ok(Some(p))
}

impl Presence {
    /// What the proof allows `user` on `m` (`None`: no proof, or one for someone or
    /// somewhere else).
    pub fn on(&self, user: &User, m: &MachineRow) -> Option<Here> {
        let p = self.0.as_ref().ok()?.as_ref()?;
        (p.user_id == user.id && hex::encode(p.endpoint) == m.endpoint_id).then_some(p.scope)
    }

    /// A refusal, with why a proof that came along did not count.
    fn refuse(&self, msg: &str) -> Error {
        match &self.0 {
            Err(p) => err(StatusCode::FORBIDDEN, format!("{msg} ({p})")),
            Ok(_) => err(StatusCode::FORBIDDEN, msg),
        }
    }
}

/// `user`'s view of `m`, with what the proof gives them there.
fn view_here(st: &Shared, m: &MachineRow, user: &User, here: &Presence) -> Result<Machine, Error> {
    let mut v = view(st, m, Some(user.id))?;
    v.here = here.on(user, m);
    Ok(v)
}

/// Marks the audit line when only the presence proof allowed the change.
fn at_the_computer(by_presence: bool) -> &'static str {
    if by_presence { " (at the computer)" } else { "" }
}

/// Only who registered it (choose managers).
fn owned(st: &Shared, id: i64, user: &User) -> Result<MachineRow, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if m.owner_id != user.id {
        return Err(err(StatusCode::FORBIDDEN, "only who registered this computer can do this"));
    }
    Ok(m)
}

/// Who registered it, or whoever manages it at the computer itself (unregister). Also
/// says whether only the presence proof allowed it.
fn owned_or_here(st: &Shared, id: i64, user: &User, here: &Presence) -> Result<(MachineRow, bool), Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if m.owner_id == user.id {
        return Ok((m, false));
    }
    if here.on(user, &m) == Some(Here::Manage) {
        return Ok((m, true));
    }
    Err(here.refuse("only who registered this computer can do this"))
}

/// The owner, a co-manager, or whoever manages it at the computer itself (rename,
/// visibility, sharing, history). Also says whether only the presence proof allowed it.
fn managed(st: &Shared, id: i64, user: &User, here: &Presence) -> Result<(MachineRow, bool), Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if st.db.can_manage(user.id, &m)? {
        return Ok((m, false));
    }
    if here.on(user, &m) == Some(Here::Manage) {
        return Ok((m, true));
    }
    Err(here.refuse("only who registered this computer and its managers can change it"))
}

/// Who may see who it is shared with and who manages it: whoever may reach it, manage it,
/// or is at the computer.
fn seen(st: &Shared, id: i64, user: &User, here: &Presence) -> Result<MachineRow, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if st.db.can_manage(user.id, &m)? || st.db.can_access(user.id, &m)? || here.on(user, &m).is_some() {
        return Ok(m);
    }
    Err(here.refuse("this computer is not shared with you"))
}

/// `GET /api/machines`: the computers this user may access, plus their own, plus the one
/// the app runs on (with a presence proof), whoever registered it.
pub async fn list(State(st): State<Shared>, Authed(s): Authed, here: Presence) -> Result<Json<Vec<Machine>>, Error> {
    let mut out = Vec::new();
    for m in st.db.machines()? {
        if m.owner_id == s.user.id || st.db.can_access(s.user.id, &m)? || here.on(&s.user, &m).is_some() {
            out.push(view_here(&st, &m, &s.user, &here)?);
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
    here: Presence,
    Path(id): Path<i64>,
    Json(req): Json<UpdateMachine>,
) -> Result<Json<Machine>, Error> {
    let (m, by_presence) = managed(&st, id, &s.user, &here)?;
    if let Some(n) = &req.name {
        st.db.rename_machine(id, n)?;
        if by_presence {
            st.db.audit(Some(s.user.id), "machine-renamed", &format!("{} → {n} (at the computer)", m.name));
        }
    }
    if let Some(v) = req.visibility {
        let before = st.db.snapshot(Some(&[id]))?;
        st.db.set_visibility(id, v)?;
        let touched = st.db.revoke_lost(before)?;
        st.wake(&touched);
        st.db.audit(
            Some(s.user.id),
            "machine-visibility",
            &format!("{} → {v:?}{}", m.name, at_the_computer(by_presence)),
        );
    }
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    Ok(Json(view_here(&st, &m, &s.user, &here)?))
}

/// `DELETE /api/machines/{id}`: unregister (the host stops being reachable through the server).
pub async fn remove(
    State(st): State<Shared>,
    Authed(s): Authed,
    here: Presence,
    Path(id): Path<i64>,
) -> Result<StatusCode, Error> {
    let (m, by_presence) = owned_or_here(&st, id, &s.user, &here)?;
    st.db.remove_machine(id)?;
    st.wake(&[id]);
    st.db.audit(Some(s.user.id), "machine-unregistered", &format!("{}{}", m.name, at_the_computer(by_presence)));
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/machines/{id}/shares`: for whoever may reach it, manage it, or is at it.
pub async fn get_shares(
    State(st): State<Shared>,
    Authed(s): Authed,
    here: Presence,
    Path(id): Path<i64>,
) -> Result<Json<Shares>, Error> {
    seen(&st, id, &s.user, &here)?;
    let (users, groups) = st.db.shares(id)?;
    Ok(Json(shares_view(&st, &users, &groups)))
}

/// Ids and roles as names and roles (users and groups that no longer exist are left out).
pub(crate) fn shares_view(st: &Shared, users: &[(i64, ShareRole)], groups: &[(i64, ShareRole)]) -> Shares {
    let users = users.iter().filter_map(|&(u, r)| st.db.user(u).ok().flatten().map(|u| (user_ref(&u), r))).collect();
    let groups = groups
        .iter()
        .filter_map(|&(g, r)| st.db.group(g).ok().flatten().map(|g| (GroupRef { id: g.id, name: g.name }, r)))
        .collect();
    Shares { users, groups }
}

/// `PUT /api/machines/{id}/shares`: replace the list.
pub async fn set_shares(
    State(st): State<Shared>,
    Authed(s): Authed,
    here: Presence,
    Path(id): Path<i64>,
    Json(req): Json<SetShares>,
) -> Result<Json<Shares>, Error> {
    let (m, by_presence) = managed(&st, id, &s.user, &here)?;
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
        &format!(
            "{}: {} user(s), {} group(s){}",
            m.name,
            req.users.len(),
            req.groups.len(),
            at_the_computer(by_presence)
        ),
    );
    let (users, groups) = st.db.shares(id)?;
    Ok(Json(shares_view(&st, &users, &groups)))
}

fn managers_view(st: &Shared, id: i64) -> Result<Managers, Error> {
    let users =
        st.db.managers(id)?.into_iter().filter_map(|u| st.db.user(u).ok().flatten()).map(|u| user_ref(&u)).collect();
    Ok(Managers { users })
}

/// `GET /api/machines/{id}/managers`: for whoever may reach it, manage it, or is at it.
pub async fn get_managers(
    State(st): State<Shared>,
    Authed(s): Authed,
    here: Presence,
    Path(id): Path<i64>,
) -> Result<Json<Managers>, Error> {
    seen(&st, id, &s.user, &here)?;
    Ok(Json(managers_view(&st, id)?))
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
    Ok(Json(managers_view(&st, id)?))
}

/// `GET /api/machines/{id}/history`: the last sessions (managers, whoever manages it at the
/// computer, and system admins).
pub async fn history(
    State(st): State<Shared>,
    Authed(s): Authed,
    here: Presence,
    Path(id): Path<i64>,
) -> Result<Json<Vec<HistoryEntry>>, Error> {
    let m = st.db.machine(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such computer"))?;
    if !s.user.is_admin && !st.db.can_manage(s.user.id, &m)? && here.on(&s.user, &m) != Some(Here::Manage) {
        return Err(here.refuse("only who manages this computer can see its history"));
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
    st.db.machine_seen(m.id, &req.addrs, &req)?;
    if !req.sessions.is_empty() {
        st.db.record_sessions(m.id, &req.sessions)?;
    }
    let notify = st.machine_notify(m.id);
    let wait = notify.notified();
    tokio::pin!(wait);
    wait.as_mut().enable();
    let (cursor, _) = st.db.revocations(m.id)?;
    let current_key = hex::encode(st.key.public().as_bytes());
    // A version the host has not seen yet, or computers to wake: answer at once.
    let new_offer = crate::updates::offer(&st, m.ring)?.is_some_and(|o| o.version != req.update_seen);
    let has_wakes = st.wakes.lock().unwrap().get(&m.id).is_some_and(|w| !w.is_empty());
    if cursor <= req.cursor && req.server_key == current_key && !new_offer && !has_wakes {
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
        update: crate::updates::offer(&st, m.ring)?,
        wake: st.wakes.lock().unwrap().remove(&m.id).unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn proof(key: &iroh_base::SecretKey, scope: Here, user_id: i64, timestamp: i64) -> HereProof {
        let endpoint = *key.public().as_bytes();
        let sig = key.sign(&HereProof::message(scope, user_id, &endpoint, timestamp));
        HereProof { scope, user_id, timestamp, endpoint, signature: sig.to_bytes() }
    }

    #[test]
    fn presence_proofs_are_checked() {
        let key = iroh_base::SecretKey::generate();
        let t = 1_700_000_000;
        let good = proof(&key, Here::Manage, 7, t);
        assert_eq!(check_proof(&good.to_header(), t + 10), Ok(Some(good.clone())));
        // Too old, or from the future.
        assert!(check_proof(&good.to_header(), t + HERE_MAX_AGE + 1).unwrap_err().contains("clock"));
        assert!(check_proof(&good.to_header(), t - HERE_MAX_AGE - 1).is_err());
        // Someone changed what it says (manage instead of view, another person).
        let view = proof(&key, Here::View, 7, t);
        let forged = HereProof { scope: Here::Manage, ..view };
        assert!(check_proof(&forged.to_header(), t).unwrap_err().contains("not signed"));
        let other = HereProof { user_id: 8, ..good.clone() };
        assert!(check_proof(&other.to_header(), t).is_err());
        // Signed by another computer's key.
        let stranger = iroh_base::SecretKey::generate();
        let claimed = HereProof { endpoint: *key.public().as_bytes(), ..proof(&stranger, Here::Manage, 7, t) };
        assert!(check_proof(&claimed.to_header(), t).is_err());
        assert!(check_proof("nonsense", t).is_err());
    }
}
