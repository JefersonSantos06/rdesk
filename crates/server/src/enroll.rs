//! Adding a computer without anyone signing in on it.
//!
//! * **Owner's e-mail**: the host asks `POST /api/enroll` with its key's signature and an
//!   e-mail; a request waits until that user approves it in the app (whoever approves owns
//!   it). Without an e-mail, the system admins approve it.
//! * **Enrollment key**: created in the app by a user; a host that presents it is
//!   registered at once, owned by that user, with the key's visibility and shares.
//!
//! Either way the host proves it holds its key, and only gets its machine token once it is
//! a machine. A pending host is not let into the relay. The server answers the same way
//! whether or not the e-mail belongs to someone, and caps the pending requests.

use std::time::Duration;

use anyhow::{Result, bail};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rdesk_proto::api::{
    ApproveMachine, CreatedEnrollKey, ENROLL_KEY_PREFIX, ENROLL_MAX_WAIT_SECS, EnrollKey, EnrollReply, EnrollRequest,
    Machine, MachineRequest, NewEnrollKey, Registered, SetShares, UserRef, Visibility, approval_code, enroll_message,
};
use rusqlite::{OptionalExtension, params};

use crate::access::{MachineRow, clean_name, parse_visibility, visibility_str};
use crate::api::{Authed, Error};
use crate::db::{Db, hash_token, normalize_email, now, random_token};
use crate::machines::{shares_view, view};
use crate::state::Shared;

/// Pending requests one user may have (a stranger cannot flood someone's list).
pub const MAX_PENDING_PER_OWNER: i64 = 10;
/// Pending requests on the whole server.
pub const MAX_PENDING_TOTAL: i64 = 100;
/// A pending request the host stopped asking about is dropped after this.
pub const PENDING_TTL_SECS: i64 = 2 * 24 * 3600;
/// A refusal is remembered this long (the host is told, and stops asking often).
pub const REJECTED_TTL_SECS: i64 = 30 * 24 * 3600;
pub const MAX_KEY_DAYS: u32 = 365;

#[derive(Debug, Clone, PartialEq)]
pub struct RequestRow {
    pub id: i64,
    pub endpoint_id: String,
    pub name: String,
    pub os: String,
    pub owner_id: Option<i64>,
    pub rejected: bool,
    pub created_at: i64,
    pub last_seen: i64,
}

impl RequestRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            endpoint_id: r.get("endpoint_id")?,
            name: r.get("name")?,
            os: r.get("os")?,
            owner_id: r.get("owner_id")?,
            rejected: r.get("rejected")?,
            created_at: r.get("created_at")?,
            last_seen: r.get("last_seen")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyRow {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub prefix: String,
    pub reusable: bool,
    pub visibility: Visibility,
    pub shares: SetShares,
    pub created_at: i64,
    pub expires_at: i64,
    pub uses: i64,
    pub revoked: bool,
}

impl KeyRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        let vis: String = r.get("visibility")?;
        let shares: String = r.get("shares")?;
        Ok(Self {
            id: r.get("id")?,
            user_id: r.get("user_id")?,
            name: r.get("name")?,
            prefix: r.get("prefix")?,
            reusable: r.get("reusable")?,
            visibility: parse_visibility(&vis),
            shares: serde_json::from_str(&shares).unwrap_or_default(),
            created_at: r.get("created_at")?,
            expires_at: r.get("expires_at")?,
            uses: r.get("uses")?,
            revoked: r.get("revoked")?,
        })
    }

    /// Can still add a computer.
    pub fn usable(&self) -> bool {
        !self.revoked && self.expires_at > now() && (self.reusable || self.uses == 0)
    }
}

fn clean_os(os: &str) -> String {
    os.chars().filter(|c| !c.is_control()).take(32).collect()
}

impl Db {
    // ---- approval requests --------------------------------------------------------------

    pub fn machine_request(&self, id: i64) -> Result<Option<RequestRow>> {
        self.with(|c| {
            c.query_row("SELECT * FROM machine_requests WHERE id = ?1", [id], RequestRow::from_row).optional()
        })
    }

    pub fn machine_request_by_endpoint(&self, endpoint_id: &str) -> Result<Option<RequestRow>> {
        self.with(|c| {
            c.query_row("SELECT * FROM machine_requests WHERE endpoint_id = ?1", [endpoint_id], RequestRow::from_row)
                .optional()
        })
    }

    /// Record (or refresh) a host's request. `None` when the limits are reached: the host is
    /// answered "pending" all the same, and nothing is stored.
    pub fn upsert_request(
        &self,
        endpoint_id: &str,
        name: &str,
        os: &str,
        owner_id: Option<i64>,
    ) -> Result<Option<RequestRow>> {
        let name = clean_name(name).unwrap_or_else(|_| "computer".into());
        let os = clean_os(os);
        if let Some(r) = self.machine_request_by_endpoint(endpoint_id)? {
            if !r.rejected {
                self.with(|c| {
                    c.execute(
                        "UPDATE machine_requests SET name = ?2, os = ?3, owner_id = ?4, last_seen = ?5 WHERE id = ?1",
                        params![r.id, name, os, owner_id, now()],
                    )
                })?;
            }
            return self.machine_request(r.id);
        }
        let (mine, total): (i64, i64) = self.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FILTER (WHERE owner_id IS ?1), COUNT(*) FROM machine_requests WHERE rejected = 0",
                [owner_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })?;
        if mine >= MAX_PENDING_PER_OWNER || total >= MAX_PENDING_TOTAL {
            tracing::warn!("too many computers waiting for approval: ignoring {}", &endpoint_id[..12]);
            return Ok(None);
        }
        self.with(|c| {
            c.execute(
                "INSERT INTO machine_requests (endpoint_id, name, os, owner_id, created_at, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![endpoint_id, name, os, owner_id, now()],
            )
        })?;
        self.machine_request_by_endpoint(endpoint_id)
    }

    /// What `user_id` may approve: requests for them, and ownerless ones for admins.
    pub fn machine_requests_for(&self, user_id: i64, is_admin: bool) -> Result<Vec<RequestRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT * FROM machine_requests WHERE rejected = 0 AND (owner_id = ?1 OR (owner_id IS NULL AND ?2))
                 ORDER BY created_at",
            )?;
            st.query_map(params![user_id, is_admin], RequestRow::from_row)?.collect()
        })
    }

    pub fn reject_request(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE machine_requests SET rejected = 1, last_seen = ?2 WHERE id = ?1", params![id, now()])
        })?;
        Ok(())
    }

    /// Make the requested computer a machine of `owner_id`. Its host picks the token up on
    /// its next request (it proves it holds the key each time).
    pub fn approve_request(&self, r: &RequestRow, owner_id: i64, name: &str, v: Visibility) -> Result<MachineRow> {
        let (m, _) = self.register_machine(owner_id, &r.endpoint_id, name, &r.os, v)?;
        self.with(|c| {
            c.execute("DELETE FROM machine_requests WHERE id = ?1", [r.id])?;
            c.execute("DELETE FROM removed_machines WHERE endpoint_id = ?1", [&r.endpoint_id])
        })?;
        Ok(m)
    }

    pub fn was_removed(&self, endpoint_id: &str) -> Result<bool> {
        Ok(self
            .with(|c| {
                c.query_row("SELECT 1 FROM removed_machines WHERE endpoint_id = ?1", [endpoint_id], |_| Ok(()))
                    .optional()
            })?
            .is_some())
    }

    /// Forget requests nobody asks about any more, and old refusals.
    pub fn sweep_requests(&self) -> Result<()> {
        let t = now();
        self.with(|c| {
            c.execute(
                "DELETE FROM machine_requests WHERE (rejected = 0 AND last_seen < ?1) OR (rejected = 1 AND last_seen < ?2)",
                params![t - PENDING_TTL_SECS, t - REJECTED_TTL_SECS],
            )
        })?;
        Ok(())
    }

    // ---- enrollment keys ----------------------------------------------------------------

    pub fn create_enroll_key(
        &self,
        user_id: i64,
        name: &str,
        reusable: bool,
        days: u32,
        visibility: Visibility,
        shares: &SetShares,
    ) -> Result<(KeyRow, String)> {
        let name = clean_name(name)?;
        if !(1..=MAX_KEY_DAYS).contains(&days) {
            bail!("a key lasts from 1 to {MAX_KEY_DAYS} days");
        }
        let secret = format!("{ENROLL_KEY_PREFIX}{}", random_token());
        let prefix: String = secret.chars().take(ENROLL_KEY_PREFIX.len() + 6).collect();
        let t = now();
        let id = self.with(|c| {
            c.execute(
                "INSERT INTO enroll_keys (user_id, name, key_hash, prefix, reusable, visibility, shares, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    user_id,
                    name,
                    hash_token(&secret),
                    prefix,
                    reusable,
                    visibility_str(visibility),
                    serde_json::to_string(shares).unwrap_or_else(|_| "{}".into()),
                    t,
                    t + i64::from(days) * 86_400
                ],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        Ok((self.enroll_key(id)?.expect("just added"), secret))
    }

    pub fn enroll_key(&self, id: i64) -> Result<Option<KeyRow>> {
        self.with(|c| c.query_row("SELECT * FROM enroll_keys WHERE id = ?1", [id], KeyRow::from_row).optional())
    }

    pub fn enroll_key_by_secret(&self, secret: &str) -> Result<Option<KeyRow>> {
        self.with(|c| {
            c.query_row("SELECT * FROM enroll_keys WHERE key_hash = ?1", [hash_token(secret)], KeyRow::from_row)
                .optional()
        })
    }

    pub fn enroll_keys(&self, user_id: i64) -> Result<Vec<KeyRow>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM enroll_keys WHERE user_id = ?1 ORDER BY created_at DESC")?;
            st.query_map([user_id], KeyRow::from_row)?.collect()
        })
    }

    pub fn revoke_enroll_key(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("UPDATE enroll_keys SET revoked = 1 WHERE id = ?1", [id]))?;
        Ok(())
    }

    fn count_enroll_key_use(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("UPDATE enroll_keys SET uses = uses + 1 WHERE id = ?1", [id]))?;
        Ok(())
    }
}

// ---- HTTP -----------------------------------------------------------------------------

fn err(code: StatusCode, msg: impl Into<String>) -> Error {
    Error(code, msg.into())
}

const BAD_KEY: &str = "this enrollment key is not valid (expired, revoked or already used)";

/// The machine's credentials, with a fresh token (only the host holding the key gets here).
fn issue(st: &Shared, m: &MachineRow) -> Result<EnrollReply, Error> {
    let token = st.db.rotate_machine_token(m.id)?;
    Ok(EnrollReply::Registered(Box::new(Registered {
        machine: view(st, m, None)?,
        token,
        server_key: hex::encode(st.key.public().as_bytes()),
        relay_url: st.relay_url(),
        server_name: st.cfg.name.clone(),
    })))
}

/// `POST /api/enroll` (no session: the host's signature is the proof).
pub async fn enroll(State(st): State<Shared>, Json(req): Json<EnrollRequest>) -> Result<Json<EnrollReply>, Error> {
    let endpoint = crate::machines::parse_endpoint(&req.endpoint_id)?;
    let key =
        iroh_base::PublicKey::from_bytes(&endpoint).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid host ID"))?;
    let sig: [u8; 64] = hex::decode(&req.signature)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad signature"))?;
    if (now() - req.timestamp).abs() > 300 {
        return Err(err(StatusCode::BAD_REQUEST, "this computer's clock is off by more than 5 minutes"));
    }
    key.verify(&enroll_message(&endpoint, req.timestamp), &iroh_base::Signature::from_bytes(&sig))
        .map_err(|_| err(StatusCode::BAD_REQUEST, "the request was not signed by this computer"))?;
    let hex_id = hex::encode(endpoint);
    let code = approval_code(&hex_id);
    let wait = Duration::from_secs(u64::from(req.wait_secs.min(ENROLL_MAX_WAIT_SECS)));

    // Already a machine (approved meanwhile, or it lost its machine.json).
    if let Some(m) = st.db.machine_by_endpoint(&hex_id)? {
        return Ok(Json(issue(&st, &m)?));
    }

    // Who should own it. `known = false`: an e-mail that is nobody's (answered like any other).
    let (owner, known) = match req.key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        Some(secret) => {
            let k = st.db.enroll_key_by_secret(secret)?.filter(KeyRow::usable);
            let k = k.ok_or_else(|| err(StatusCode::FORBIDDEN, BAD_KEY))?;
            let user = st.db.user(k.user_id)?.filter(|u| !u.disabled);
            let user = user.ok_or_else(|| err(StatusCode::FORBIDDEN, BAD_KEY))?;
            if !st.db.was_removed(&hex_id)? {
                let (m, _) = st.db.register_machine(user.id, &hex_id, &req.name, &clean_os(&req.os), k.visibility)?;
                // Shares of the key, minus users and groups removed since.
                let users: Vec<_> =
                    k.shares.users.iter().copied().filter(|(u, _)| st.db.user(*u).ok().flatten().is_some()).collect();
                let groups: Vec<_> =
                    k.shares.groups.iter().copied().filter(|(g, _)| st.db.group(*g).ok().flatten().is_some()).collect();
                st.db.set_shares(m.id, &users, &groups)?;
                st.db.count_enroll_key_use(k.id)?;
                st.db.audit(
                    Some(user.id),
                    "machine-enrolled",
                    &format!("{} {} (key {})", m.name, &hex_id[..12], k.name),
                );
                return Ok(Json(issue(&st, &m)?));
            }
            // Unregistered before: a key does not bring it back on its own; its owner decides.
            (Some(user.id), true)
        }
        None => match req.owner.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            Some(email) => {
                let user = match normalize_email(email) {
                    Ok(e) => st.db.user_by_email(&e)?.filter(|u| !u.disabled),
                    Err(_) => None,
                };
                (user.as_ref().map(|u| u.id), user.is_some())
            }
            None => (None, true),
        },
    };

    if st.db.machine_request_by_endpoint(&hex_id)?.is_some_and(|r| r.rejected) {
        return Ok(Json(EnrollReply::Rejected));
    }
    let stored = if known { st.db.upsert_request(&hex_id, &req.name, &req.os, owner)? } else { None };
    if stored.is_none() {
        tokio::time::sleep(wait).await;
        return Ok(Json(EnrollReply::Pending { code }));
    }

    // Wait (a little) for the approval, so the host hears it right away.
    let notify = st.enroll_notify(&hex_id);
    let woken = notify.notified();
    tokio::pin!(woken);
    woken.as_mut().enable();
    if st.db.machine_by_endpoint(&hex_id)?.is_none() {
        let _ = tokio::time::timeout(wait, woken).await;
    }
    if let Some(m) = st.db.machine_by_endpoint(&hex_id)? {
        return Ok(Json(issue(&st, &m)?));
    }
    if st.db.machine_request_by_endpoint(&hex_id)?.is_some_and(|r| r.rejected) {
        return Ok(Json(EnrollReply::Rejected));
    }
    Ok(Json(EnrollReply::Pending { code }))
}

fn request_view(st: &Shared, r: &RequestRow) -> Result<MachineRequest, Error> {
    let owner = match r.owner_id {
        Some(id) => st.db.user(id)?.map(|u| UserRef { id: u.id, email: u.email, name: u.name }),
        None => None,
    };
    Ok(MachineRequest {
        id: r.id,
        endpoint_id: r.endpoint_id.clone(),
        code: approval_code(&r.endpoint_id),
        name: r.name.clone(),
        os: r.os.clone(),
        owner,
        created_at: r.created_at,
        last_seen: r.last_seen,
    })
}

/// The request, if `s` may decide on it (its owner; for ownerless ones, a system admin).
fn decidable(st: &Shared, id: i64, s: &crate::db::Session) -> Result<RequestRow, Error> {
    let r = st.db.machine_request(id)?.filter(|r| !r.rejected);
    let r = r.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such request (approved, refused or expired)"))?;
    let ok = match r.owner_id {
        Some(o) => o == s.user.id,
        None => s.user.is_admin,
    };
    if !ok {
        return Err(err(StatusCode::FORBIDDEN, "this computer asked someone else"));
    }
    Ok(r)
}

/// `GET /api/machine-requests`.
pub async fn list_requests(State(st): State<Shared>, Authed(s): Authed) -> Result<Json<Vec<MachineRequest>>, Error> {
    let rows = st.db.machine_requests_for(s.user.id, s.user.is_admin)?;
    Ok(Json(rows.iter().map(|r| request_view(&st, r)).collect::<Result<_, _>>()?))
}

/// `POST /api/machine-requests/{id}/approve`: the approver owns the new machine.
pub async fn approve(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
    Json(req): Json<ApproveMachine>,
) -> Result<Json<Machine>, Error> {
    let r = decidable(&st, id, &s)?;
    let name = req.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| r.name.clone());
    let m = st.db.approve_request(&r, s.user.id, &name, req.visibility)?;
    st.db.audit(Some(s.user.id), "machine-approved", &format!("{} {}", m.name, &r.endpoint_id[..12]));
    st.wake_enroll(&r.endpoint_id);
    Ok(Json(view(&st, &m, Some(s.user.id))?))
}

/// `POST /api/machine-requests/{id}/reject`.
pub async fn reject(State(st): State<Shared>, Authed(s): Authed, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    let r = decidable(&st, id, &s)?;
    st.db.reject_request(r.id)?;
    st.db.audit(Some(s.user.id), "machine-refused", &format!("{} {}", r.name, &r.endpoint_id[..12]));
    st.wake_enroll(&r.endpoint_id);
    Ok(StatusCode::NO_CONTENT)
}

fn key_view(st: &Shared, k: &KeyRow) -> EnrollKey {
    EnrollKey {
        id: k.id,
        name: k.name.clone(),
        prefix: k.prefix.clone(),
        reusable: k.reusable,
        visibility: k.visibility,
        shares: shares_view(st, &k.shares.users, &k.shares.groups),
        created_at: k.created_at,
        expires_at: k.expires_at,
        uses: k.uses,
        revoked: k.revoked,
    }
}

/// `GET /api/enroll-keys`: the user's own keys.
pub async fn list_keys(State(st): State<Shared>, Authed(s): Authed) -> Result<Json<Vec<EnrollKey>>, Error> {
    Ok(Json(st.db.enroll_keys(s.user.id)?.iter().map(|k| key_view(&st, k)).collect()))
}

/// `POST /api/enroll-keys`: the secret is in the answer, and only there.
pub async fn create_key(
    State(st): State<Shared>,
    Authed(s): Authed,
    Json(req): Json<NewEnrollKey>,
) -> Result<Json<CreatedEnrollKey>, Error> {
    for (u, _) in &req.shares.users {
        st.db.user(*u)?.ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("no user {u}")))?;
    }
    for (g, _) in &req.shares.groups {
        st.db.group(*g)?.ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("no group {g}")))?;
    }
    let (k, secret) = st.db.create_enroll_key(
        s.user.id,
        &req.name,
        req.reusable,
        req.expires_in_days,
        req.visibility,
        &req.shares,
    )?;
    st.db.audit(Some(s.user.id), "enroll-key-created", &format!("{} ({})", k.name, k.prefix));
    Ok(Json(CreatedEnrollKey { key: secret, info: key_view(&st, &k) }))
}

/// `DELETE /api/enroll-keys/{id}`: revoke (computers already added stay).
pub async fn revoke_key(State(st): State<Shared>, Authed(s): Authed, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    let k = st.db.enroll_key(id)?.filter(|k| k.user_id == s.user.id);
    let k = k.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such key"))?;
    st.db.revoke_enroll_key(k.id)?;
    st.db.audit(Some(s.user.id), "enroll-key-revoked", &k.name);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdesk_proto::api::ShareRole;

    #[test]
    fn requests_are_capped_and_approved() {
        let db = Db::memory().unwrap();
        let ana = db.add_user("ana@example.com", false).unwrap().id;
        let ep = |i: u8| hex::encode([i; 32]);
        for i in 0..MAX_PENDING_PER_OWNER as u8 {
            assert!(db.upsert_request(&ep(i), "pi", "linux", Some(ana)).unwrap().is_some());
        }
        assert!(db.upsert_request(&ep(200), "pi", "linux", Some(ana)).unwrap().is_none(), "capped per owner");
        // Asking again refreshes, also over the limit.
        assert!(db.upsert_request(&ep(0), "pi 2", "linux", Some(ana)).unwrap().is_some());
        assert!(db.upsert_request(&ep(201), "x", "", None).unwrap().is_some(), "another owner has room");

        let r = db.machine_request_by_endpoint(&ep(0)).unwrap().unwrap();
        assert_eq!(r.name, "pi 2");
        assert_eq!(db.machine_requests_for(ana, false).unwrap().len(), MAX_PENDING_PER_OWNER as usize);
        assert_eq!(db.machine_requests_for(ana, true).unwrap().len(), MAX_PENDING_PER_OWNER as usize + 1);
        let m = db.approve_request(&r, ana, "Pi", Visibility::Shared).unwrap();
        assert_eq!((m.owner_id, m.visibility), (ana, Visibility::Shared));
        assert!(db.machine_request(r.id).unwrap().is_none());

        // Unregistering remembers the ID; a refusal sticks.
        db.remove_machine(m.id).unwrap();
        assert!(db.was_removed(&ep(0)).unwrap());
        let r = db.machine_request_by_endpoint(&ep(1)).unwrap().unwrap();
        db.reject_request(r.id).unwrap();
        let again = db.upsert_request(&ep(1), "pi", "linux", Some(ana)).unwrap().unwrap();
        assert!(again.rejected);
    }

    #[test]
    fn keys_expire_and_are_single_use_unless_reusable() {
        let db = Db::memory().unwrap();
        let ana = db.add_user("ana@example.com", false).unwrap().id;
        let shares = SetShares { users: vec![(ana, ShareRole::View)], groups: vec![] };
        let (k, secret) = db.create_enroll_key(ana, "pis", false, 7, Visibility::Shared, &shares).unwrap();
        assert!(secret.starts_with(ENROLL_KEY_PREFIX) && secret.starts_with(&k.prefix));
        let found = db.enroll_key_by_secret(&secret).unwrap().unwrap();
        assert_eq!(found.shares.users, vec![(ana, ShareRole::View)]);
        assert!(found.usable());
        db.count_enroll_key_use(k.id).unwrap();
        assert!(!db.enroll_key(k.id).unwrap().unwrap().usable(), "single use");

        let (k2, _) = db.create_enroll_key(ana, "lab", true, 1, Visibility::Private, &SetShares::default()).unwrap();
        db.count_enroll_key_use(k2.id).unwrap();
        assert!(db.enroll_key(k2.id).unwrap().unwrap().usable(), "reusable");
        db.revoke_enroll_key(k2.id).unwrap();
        assert!(!db.enroll_key(k2.id).unwrap().unwrap().usable());
        assert!(db.create_enroll_key(ana, "x", true, 0, Visibility::Private, &SetShares::default()).is_err());
    }
}
