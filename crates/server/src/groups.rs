//! Groups API. Anyone can create a group (and becomes its admin); group admins manage
//! members, roles and the name. There is no owner: only admins, and at least one.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rdesk_proto::api::{
    AddMember, Group, GroupMachine, GroupMember, GroupRef, GroupRole, NewGroup, SetRole, ShareRole, UserRef, Visibility,
};

use crate::access::{GroupRow, MachineRow};
use crate::api::{Authed, Error};
use crate::db::{Session, User};
use crate::state::Shared;

fn err(code: StatusCode, msg: impl Into<String>) -> Error {
    Error(code, msg.into())
}

fn view(st: &Shared, g: &GroupRow, viewer: &User) -> Result<Group, Error> {
    Ok(Group {
        id: g.id,
        name: g.name.clone(),
        machine_count: shared_with(st, g, viewer)?.len() as u32,
        my_role: g.members.iter().find(|(u, _)| *u == viewer.id).map(|(_, r)| *r),
        members: g
            .members
            .iter()
            .filter_map(|(u, r)| {
                st.db
                    .user(*u)
                    .ok()
                    .flatten()
                    .map(|u| GroupMember { user: UserRef { id: u.id, email: u.email, name: u.name }, role: *r })
            })
            .collect(),
    })
}

/// The computers shared with `g` that `viewer` sees listed, with the group's role and whether
/// the share counts now: every active one, and inactive ones (a private computer) only for
/// whoever manages that computer or a system admin.
fn shared_with(st: &Shared, g: &GroupRow, viewer: &User) -> Result<Vec<(MachineRow, ShareRole, bool)>, Error> {
    let mut out = Vec::new();
    for id in st.db.machines_shared_with_group(g.id)? {
        let Some(m) = st.db.machine(id)? else { continue };
        let (_, groups) = st.db.shares(id)?;
        let Some(role) = groups.iter().find(|(gid, _)| *gid == g.id).map(|(_, r)| *r) else { continue };
        let active = m.visibility != Visibility::Private;
        if active || viewer.is_admin || st.db.can_manage(viewer.id, &m)? {
            out.push((m, role, active));
        }
    }
    out.sort_by_key(|(m, _, _)| m.name.to_lowercase());
    Ok(out)
}

fn group(st: &Shared, id: i64) -> Result<GroupRow, Error> {
    st.db.group(id)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such group"))
}

/// Group admins (and system admins, to fix a stuck group) may manage it.
fn managed(st: &Shared, id: i64, s: &Session) -> Result<GroupRow, Error> {
    let g = group(st, id)?;
    let admin = g.members.iter().any(|(u, r)| *u == s.user.id && *r == GroupRole::Admin);
    if !admin && !s.user.is_admin {
        return Err(err(StatusCode::FORBIDDEN, "only the group's admins can change it"));
    }
    Ok(g)
}

/// Membership changes change who may access the machines shared with the group.
fn with_revocations<T>(st: &Shared, group: i64, f: impl FnOnce() -> anyhow::Result<T>) -> Result<T, Error> {
    let machines = st.db.machines_shared_with_group(group)?;
    let before = st.db.snapshot(Some(&machines))?;
    let out = f()?;
    let touched = st.db.revoke_lost(before)?;
    st.wake(&touched);
    Ok(out)
}

/// `GET /api/groups`: the groups I am in (system admins: all of them).
pub async fn list(State(st): State<Shared>, Authed(s): Authed) -> Result<Json<Vec<Group>>, Error> {
    let gs = st.db.groups()?;
    Ok(Json(
        gs.iter()
            .filter(|g| s.user.is_admin || g.members.iter().any(|(u, _)| *u == s.user.id))
            .map(|g| view(&st, g, &s.user))
            .collect::<Result<_, _>>()?,
    ))
}

/// `GET /api/groups/{id}/machines`: the computers shared with the group (its members and
/// system admins).
pub async fn machines(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
) -> Result<Json<Vec<GroupMachine>>, Error> {
    let g = group(&st, id)?;
    if !s.user.is_admin && !g.members.iter().any(|(u, _)| *u == s.user.id) {
        return Err(err(StatusCode::FORBIDDEN, "only the group's members see its computers"));
    }
    let list = shared_with(&st, &g, &s.user)?
        .into_iter()
        .map(|(m, role, active)| {
            Ok(GroupMachine { machine: crate::machines::view(&st, &m, Some(s.user.id))?, role, active })
        })
        .collect::<Result<_, Error>>()?;
    Ok(Json(list))
}

/// `GET /api/groups/all`: every group's name (to share a computer with).
pub async fn all(State(st): State<Shared>, _: Authed) -> Result<Json<Vec<GroupRef>>, Error> {
    Ok(Json(st.db.groups()?.into_iter().map(|g| GroupRef { id: g.id, name: g.name }).collect()))
}

pub async fn create(
    State(st): State<Shared>,
    Authed(s): Authed,
    Json(req): Json<NewGroup>,
) -> Result<Json<Group>, Error> {
    let id = st.db.create_group(&req.name, s.user.id)?;
    st.db.audit(Some(s.user.id), "group-created", req.name.trim());
    Ok(Json(view(&st, &group(&st, id)?, &s.user)?))
}

pub async fn rename(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
    Json(req): Json<NewGroup>,
) -> Result<Json<Group>, Error> {
    managed(&st, id, &s)?;
    st.db.rename_group(id, &req.name)?;
    Ok(Json(view(&st, &group(&st, id)?, &s.user)?))
}

pub async fn delete(State(st): State<Shared>, Authed(s): Authed, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    let g = managed(&st, id, &s)?;
    with_revocations(&st, id, || st.db.delete_group(id))?;
    st.db.audit(Some(s.user.id), "group-deleted", &g.name);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_member(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path(id): Path<i64>,
    Json(req): Json<AddMember>,
) -> Result<Json<Group>, Error> {
    managed(&st, id, &s)?;
    let u = st
        .db
        .user_by_email(&req.email)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("{} is not a user of this server", req.email.trim())))?;
    with_revocations(&st, id, || st.db.set_member(id, u.id, req.role))?;
    Ok(Json(view(&st, &group(&st, id)?, &s.user)?))
}

pub async fn set_role(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path((id, user)): Path<(i64, i64)>,
    Json(req): Json<SetRole>,
) -> Result<Json<Group>, Error> {
    let g = managed(&st, id, &s)?;
    if !g.members.iter().any(|(u, _)| *u == user) {
        return Err(err(StatusCode::NOT_FOUND, "not a member"));
    }
    st.db.set_member(id, user, req.role)?;
    Ok(Json(view(&st, &group(&st, id)?, &s.user)?))
}

/// Remove a member; anyone may remove themselves (leave).
pub async fn remove_member(
    State(st): State<Shared>,
    Authed(s): Authed,
    Path((id, user)): Path<(i64, i64)>,
) -> Result<StatusCode, Error> {
    if user != s.user.id {
        managed(&st, id, &s)?;
    } else {
        group(&st, id)?;
    }
    with_revocations(&st, id, || st.db.remove_member(id, user))?;
    Ok(StatusCode::NO_CONTENT)
}
