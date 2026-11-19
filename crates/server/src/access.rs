//! Machines, groups, sharing and who may access what.
//!
//! Access is a set of permissions ([`Access`]: screen, input, files). The owner and the
//! co-managers have all of them; "everyone" gives all of them to every user; "shared"
//! gives each user what their shares grant (directly or through groups), combined.
//! System admins see every machine but follow the same rule to access one.
//!
//! When a change takes access away (or narrows it), a revocation is recorded for that
//! machine and user: the host refuses the user's older tickets (they last at most 12 h, so
//! revocations older than that are dropped) and ends their session.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use rdesk_proto::Access;
use rdesk_proto::api::{AccessRecord, GroupRole, HistoryEntry, ShareRole, Visibility};
use rusqlite::{OptionalExtension, params};

use crate::db::{Db, hash_token, now, random_token};

pub fn visibility_str(v: Visibility) -> &'static str {
    match v {
        Visibility::Private => "private",
        Visibility::Shared => "shared",
        Visibility::Everyone => "everyone",
    }
}

fn parse_visibility(s: &str) -> Visibility {
    match s {
        "shared" => Visibility::Shared,
        "everyone" => Visibility::Everyone,
        _ => Visibility::Private,
    }
}

fn role_str(r: GroupRole) -> &'static str {
    match r {
        GroupRole::Admin => "admin",
        GroupRole::Member => "member",
    }
}

fn parse_role(s: &str) -> GroupRole {
    if s == "admin" { GroupRole::Admin } else { GroupRole::Member }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MachineRow {
    pub id: i64,
    pub endpoint_id: String,
    pub name: String,
    pub os: String,
    pub owner_id: i64,
    pub visibility: Visibility,
    pub addrs: Vec<String>,
    pub in_session: bool,
    pub created_at: i64,
    pub last_seen: Option<i64>,
}

impl MachineRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        let addrs: String = r.get("addrs")?;
        let vis: String = r.get("visibility")?;
        Ok(Self {
            id: r.get("id")?,
            endpoint_id: r.get("endpoint_id")?,
            name: r.get("name")?,
            os: r.get("os")?,
            owner_id: r.get("owner_id")?,
            visibility: parse_visibility(&vis),
            addrs: addrs.split(',').filter(|a| !a.is_empty()).map(str::to_string).collect(),
            in_session: r.get("in_session")?,
            created_at: r.get("created_at")?,
            last_seen: r.get("last_seen")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupRow {
    pub id: i64,
    pub name: String,
    /// `(user_id, role)`.
    pub members: Vec<(i64, GroupRole)>,
}

/// `(user or group id, role)` pairs.
pub type Grants = Vec<(i64, ShareRole)>;

/// What each user may do on each machine before a change, to find who lost access after it.
pub type Snapshot = HashMap<i64, HashMap<i64, Access>>;

impl Db {
    // ---- machines ---------------------------------------------------------------------

    /// Register a machine (or, for its owner, re-register it with a fresh token).
    /// Returns the row and the machine token.
    pub fn register_machine(
        &self,
        owner_id: i64,
        endpoint_id: &str,
        name: &str,
        os: &str,
        visibility: Visibility,
    ) -> Result<(MachineRow, String)> {
        let name = clean_name(name)?;
        let token = random_token();
        if let Some(existing) = self.machine_by_endpoint(endpoint_id)? {
            if existing.owner_id != owner_id {
                bail!("this computer is already registered by someone else; they must unregister it first");
            }
            self.with(|c| {
                c.execute(
                    "UPDATE machines SET token_hash = ?2, name = ?3, os = ?4 WHERE id = ?1",
                    params![existing.id, hash_token(&token), name, os],
                )
            })?;
            return Ok((self.machine(existing.id)?.expect("exists"), token));
        }
        self.with(|c| {
            c.execute(
                "INSERT INTO machines (endpoint_id, name, os, owner_id, visibility, token_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![endpoint_id, name, os, owner_id, visibility_str(visibility), hash_token(&token), now()],
            )
        })?;
        Ok((self.machine_by_endpoint(endpoint_id)?.expect("just added"), token))
    }

    pub fn machine(&self, id: i64) -> Result<Option<MachineRow>> {
        self.with(|c| c.query_row("SELECT * FROM machines WHERE id = ?1", [id], MachineRow::from_row).optional())
    }

    pub fn machine_by_endpoint(&self, endpoint_id: &str) -> Result<Option<MachineRow>> {
        self.with(|c| {
            c.query_row("SELECT * FROM machines WHERE endpoint_id = ?1", [endpoint_id], MachineRow::from_row).optional()
        })
    }

    pub fn machine_by_token(&self, token: &str) -> Result<Option<MachineRow>> {
        self.with(|c| {
            c.query_row("SELECT * FROM machines WHERE token_hash = ?1", [hash_token(token)], MachineRow::from_row)
                .optional()
        })
    }

    pub fn machines(&self) -> Result<Vec<MachineRow>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM machines ORDER BY name COLLATE NOCASE")?;
            st.query_map([], MachineRow::from_row)?.collect()
        })
    }

    pub fn rename_machine(&self, id: i64, name: &str) -> Result<()> {
        let name = clean_name(name)?;
        self.with(|c| c.execute("UPDATE machines SET name = ?2 WHERE id = ?1", params![id, name]))?;
        Ok(())
    }

    pub fn set_visibility(&self, id: i64, v: Visibility) -> Result<()> {
        self.with(|c| c.execute("UPDATE machines SET visibility = ?2 WHERE id = ?1", params![id, visibility_str(v)]))?;
        Ok(())
    }

    pub fn remove_machine(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM machines WHERE id = ?1", [id]))?;
        Ok(())
    }

    /// What the machine reported about itself.
    pub fn machine_seen(&self, id: i64, addrs: &[String], os: &str, in_session: bool) -> Result<()> {
        let addrs: Vec<&str> =
            addrs.iter().map(String::as_str).filter(|a| !a.contains(',') && a.len() < 64).take(16).collect();
        let os: String = os.chars().take(32).collect();
        self.with(|c| {
            c.execute(
                "UPDATE machines SET addrs = ?2, os = ?3, in_session = ?4, last_seen = ?5 WHERE id = ?1",
                params![id, addrs.join(","), os, in_session, now()],
            )
        })?;
        Ok(())
    }

    // ---- sharing ----------------------------------------------------------------------

    /// `(users, groups)` the machine is shared with, with their roles.
    pub fn shares(&self, machine_id: i64) -> Result<(Grants, Grants)> {
        let rows: Vec<(Option<i64>, Option<i64>, String)> = self.with(|c| {
            let mut st = c.prepare("SELECT user_id, group_id, role FROM machine_shares WHERE machine_id = ?1")?;
            st.query_map([machine_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
        })?;
        let role = |s: &str| ShareRole::parse(s).unwrap_or_default();
        Ok((
            rows.iter().filter_map(|r| r.0.map(|u| (u, role(&r.2)))).collect(),
            rows.iter().filter_map(|r| r.1.map(|g| (g, role(&r.2)))).collect(),
        ))
    }

    pub fn set_shares(&self, machine_id: i64, users: &[(i64, ShareRole)], groups: &[(i64, ShareRole)]) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM machine_shares WHERE machine_id = ?1", [machine_id])?;
        let users: HashMap<i64, ShareRole> = users.iter().copied().collect();
        let groups: HashMap<i64, ShareRole> = groups.iter().copied().collect();
        for (u, r) in users {
            tx.execute(
                "INSERT INTO machine_shares (machine_id, user_id, role) VALUES (?1, ?2, ?3)",
                params![machine_id, u, r.as_str()],
            )?;
        }
        for (g, r) in groups {
            tx.execute(
                "INSERT INTO machine_shares (machine_id, group_id, role) VALUES (?1, ?2, ?3)",
                params![machine_id, g, r.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- co-managers ------------------------------------------------------------------

    pub fn managers(&self, machine_id: i64) -> Result<Vec<i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT user_id FROM machine_managers WHERE machine_id = ?1 ORDER BY user_id")?;
            st.query_map([machine_id], |r| r.get(0))?.collect()
        })
    }

    pub fn set_managers(&self, machine_id: i64, users: &[i64]) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM machine_managers WHERE machine_id = ?1", [machine_id])?;
        for u in users.iter().collect::<HashSet<_>>() {
            tx.execute("INSERT INTO machine_managers (machine_id, user_id) VALUES (?1, ?2)", params![machine_id, u])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The owner or a co-manager.
    pub fn can_manage(&self, user_id: i64, m: &MachineRow) -> Result<bool> {
        Ok(m.owner_id == user_id || self.managers(m.id)?.contains(&user_id))
    }

    /// What every user may do on `m` right now (users with no access are left out).
    pub fn access_map(&self, m: &MachineRow) -> Result<HashMap<i64, Access>> {
        let mut map: HashMap<i64, Access> = HashMap::new();
        let mut grant = |u: i64, a: Access| {
            let e = map.entry(u).or_insert(Access::NONE);
            *e = e.union(a);
        };
        grant(m.owner_id, Access::FULL);
        for u in self.managers(m.id)? {
            grant(u, Access::FULL);
        }
        match m.visibility {
            Visibility::Private => {}
            Visibility::Everyone => {
                let all: Vec<i64> = self.with(|c| {
                    let mut st = c.prepare("SELECT id FROM users WHERE disabled = 0")?;
                    st.query_map([], |r| r.get(0))?.collect()
                })?;
                for u in all {
                    grant(u, Access::FULL);
                }
            }
            Visibility::Shared => {
                let rows: Vec<(i64, String)> = self.with(|c| {
                    let mut st = c.prepare(
                        "SELECT user_id, role FROM machine_shares WHERE machine_id = ?1 AND user_id IS NOT NULL
                         UNION ALL
                         SELECT gm.user_id, s.role FROM machine_shares s JOIN group_members gm ON gm.group_id = s.group_id
                         WHERE s.machine_id = ?1",
                    )?;
                    st.query_map([m.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
                })?;
                for (u, role) in rows {
                    grant(u, ShareRole::parse(&role).unwrap_or_default().access());
                }
            }
        }
        Ok(map)
    }

    pub fn access_of(&self, user_id: i64, m: &MachineRow) -> Result<Access> {
        Ok(self.access_map(m)?.get(&user_id).copied().unwrap_or(Access::NONE))
    }

    pub fn can_access(&self, user_id: i64, m: &MachineRow) -> Result<bool> {
        Ok(!self.access_of(user_id, m)?.is_none())
    }

    /// Who may access the given machines (all of them when `None`).
    pub fn snapshot(&self, machines: Option<&[i64]>) -> Result<Snapshot> {
        let mut out = HashMap::new();
        for m in self.machines()? {
            if machines.is_none_or(|ids| ids.contains(&m.id)) {
                out.insert(m.id, self.access_map(&m)?);
            }
        }
        Ok(out)
    }

    /// Record a revocation for everyone whose access in `before` is not fully kept now (lost,
    /// or narrowed: their tickets carry the old permissions). Returns the machines with new
    /// revocations (to wake their hosts).
    pub fn revoke_lost(&self, before: Snapshot) -> Result<Vec<i64>> {
        let t = now();
        let mut touched = Vec::new();
        for (mid, had) in before {
            let Some(m) = self.machine(mid)? else { continue };
            let has = self.access_map(&m)?;
            let lost: Vec<i64> = had
                .iter()
                .filter(|(u, old)| !has.get(u).copied().unwrap_or(Access::NONE).covers(**old))
                .map(|(u, _)| *u)
                .collect();
            if lost.is_empty() {
                continue;
            }
            self.with(|c| {
                for u in &lost {
                    c.execute(
                        "INSERT INTO revocations (machine_id, user_id, before) VALUES (?1, ?2, ?3)",
                        params![mid, u, t],
                    )?;
                }
                Ok(())
            })?;
            touched.push(mid);
        }
        Ok(touched)
    }

    /// Void all of a user's tickets everywhere (disabled or removed). Returns the machines.
    pub fn revoke_user(&self, user_id: i64) -> Result<Vec<i64>> {
        let t = now();
        let ids: Vec<i64> = self.machines()?.into_iter().map(|m| m.id).collect();
        self.with(|c| {
            for mid in &ids {
                c.execute(
                    "INSERT INTO revocations (machine_id, user_id, before) VALUES (?1, ?2, ?3)",
                    params![mid, user_id, t],
                )?;
            }
            Ok(())
        })?;
        Ok(ids)
    }

    /// The latest revocation of `user` on `machine`, if any.
    pub fn last_revocation(&self, machine_id: i64, user_id: i64) -> Result<Option<i64>> {
        self.with(|c| {
            c.query_row(
                "SELECT MAX(before) FROM revocations WHERE machine_id = ?1 AND user_id = ?2",
                params![machine_id, user_id],
                |r| r.get(0),
            )
        })
    }

    /// `(cursor, revocations of the last 12 h)` for a machine.
    pub fn revocations(&self, machine_id: i64) -> Result<(i64, Vec<(i64, i64)>)> {
        let since = now() - rdesk_proto::ticket::TICKET_MAX_SECS - rdesk_proto::ticket::CLOCK_SKEW_SECS;
        self.with(|c| {
            c.execute("DELETE FROM revocations WHERE before < ?1", [since])?;
            let cursor: i64 =
                c.query_row("SELECT COALESCE(MAX(id), 0) FROM revocations WHERE machine_id = ?1", [machine_id], |r| {
                    r.get(0)
                })?;
            let mut st = c.prepare(
                "SELECT user_id, MAX(before) FROM revocations WHERE machine_id = ?1 GROUP BY user_id ORDER BY user_id",
            )?;
            let rows = st.query_map([machine_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            Ok((cursor, rows))
        })
    }

    // ---- access history ------------------------------------------------------------------

    /// What a host reported: new sessions and ended ones (by session id).
    pub fn record_sessions(&self, machine_id: i64, records: &[AccessRecord]) -> Result<()> {
        let short = |s: &str, n: usize| s.chars().take(n).collect::<String>();
        self.with(|c| {
            for r in records.iter().take(50) {
                c.execute(
                    "INSERT INTO access_log (machine_id, session, user_id, who, client, method, access, started_at, ended_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT (machine_id, session) DO UPDATE SET ended_at = excluded.ended_at",
                    params![
                        machine_id,
                        short(&r.session, 64),
                        r.user_id,
                        short(&r.who, 128),
                        short(&r.client, 128),
                        short(&r.method, 128),
                        short(&r.access, 64),
                        r.started_at,
                        r.ended_at
                    ],
                )?;
            }
            // Keep the last 500 sessions per machine.
            c.execute(
                "DELETE FROM access_log WHERE machine_id = ?1 AND id NOT IN
                 (SELECT id FROM access_log WHERE machine_id = ?1 ORDER BY started_at DESC LIMIT 500)",
                [machine_id],
            )?;
            Ok(())
        })
    }

    pub fn history(&self, machine_id: i64, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT who, client, method, access, started_at, ended_at FROM access_log
                 WHERE machine_id = ?1 ORDER BY started_at DESC LIMIT ?2",
            )?;
            st.query_map(params![machine_id, limit as i64], |r| {
                Ok(HistoryEntry {
                    who: r.get(0)?,
                    client: r.get(1)?,
                    method: r.get(2)?,
                    access: r.get(3)?,
                    started_at: r.get(4)?,
                    ended_at: r.get(5)?,
                })
            })?
            .collect()
        })
    }

    // ---- groups -------------------------------------------------------------------------

    pub fn create_group(&self, name: &str, creator: i64) -> Result<i64> {
        let name = clean_name(name)?;
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO groups (name, created_at) VALUES (?1, ?2)", params![name, now()])?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO group_members (group_id, user_id, role) VALUES (?1, ?2, 'admin')",
            params![id, creator],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn group(&self, id: i64) -> Result<Option<GroupRow>> {
        let name: Option<String> =
            self.with(|c| c.query_row("SELECT name FROM groups WHERE id = ?1", [id], |r| r.get(0)).optional())?;
        let Some(name) = name else { return Ok(None) };
        let members: Vec<(i64, String)> = self.with(|c| {
            let mut st = c.prepare(
                "SELECT gm.user_id, gm.role FROM group_members gm JOIN users u ON u.id = gm.user_id
                 WHERE gm.group_id = ?1 ORDER BY u.email",
            )?;
            st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
        })?;
        Ok(Some(GroupRow { id, name, members: members.into_iter().map(|(u, r)| (u, parse_role(&r))).collect() }))
    }

    pub fn groups(&self) -> Result<Vec<GroupRow>> {
        let ids: Vec<i64> = self.with(|c| {
            let mut st = c.prepare("SELECT id FROM groups ORDER BY name COLLATE NOCASE")?;
            st.query_map([], |r| r.get(0))?.collect()
        })?;
        Ok(ids.into_iter().filter_map(|id| self.group(id).ok().flatten()).collect())
    }

    pub fn rename_group(&self, id: i64, name: &str) -> Result<()> {
        let name = clean_name(name)?;
        self.with(|c| c.execute("UPDATE groups SET name = ?2 WHERE id = ?1", params![id, name]))?;
        Ok(())
    }

    pub fn delete_group(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM groups WHERE id = ?1", [id]))?;
        Ok(())
    }

    /// Add a member, or change the role of one.
    pub fn set_member(&self, group: i64, user: i64, role: GroupRole) -> Result<()> {
        if role == GroupRole::Member {
            self.ensure_group_admin_left(group, user)?;
        }
        self.with(|c| {
            c.execute(
                "INSERT INTO group_members (group_id, user_id, role) VALUES (?1, ?2, ?3)
                 ON CONFLICT (group_id, user_id) DO UPDATE SET role = excluded.role",
                params![group, user, role_str(role)],
            )
        })?;
        Ok(())
    }

    pub fn remove_member(&self, group: i64, user: i64) -> Result<()> {
        self.ensure_group_admin_left(group, user)?;
        self.with(|c| {
            c.execute("DELETE FROM group_members WHERE group_id = ?1 AND user_id = ?2", params![group, user])
        })?;
        Ok(())
    }

    /// A group keeps at least one admin (delete the group instead).
    fn ensure_group_admin_left(&self, group: i64, leaving: i64) -> Result<()> {
        let Some(g) = self.group(group)? else { bail!("no such group") };
        let is_admin = g.members.iter().any(|(u, r)| *u == leaving && *r == GroupRole::Admin);
        let others = g.members.iter().filter(|(u, r)| *u != leaving && *r == GroupRole::Admin).count();
        if is_admin && others == 0 {
            bail!("a group needs at least one admin: make someone else admin first, or delete the group");
        }
        Ok(())
    }

    /// Machines shared with a group (their access changes with its membership).
    pub fn machines_shared_with_group(&self, group: i64) -> Result<Vec<i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT DISTINCT machine_id FROM machine_shares WHERE group_id = ?1")?;
            st.query_map([group], |r| r.get(0))?.collect()
        })
    }
}

fn clean_name(name: &str) -> Result<String> {
    let n: String = name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    if n.is_empty() {
        bail!("the name cannot be empty");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_rules_and_revocations() {
        let db = Db::memory().unwrap();
        let ana = db.add_user("ana@example.com", true).unwrap().id;
        let bia = db.add_user("bia@example.com", false).unwrap().id;
        let caio = db.add_user("caio@example.com", false).unwrap().id;
        let (m, token) =
            db.register_machine(ana, &"ab".repeat(32), "PC da sala", "windows", Visibility::Private).unwrap();
        assert_eq!(db.machine_by_token(&token).unwrap().unwrap().id, m.id);

        let users = |m: &MachineRow| db.access_map(m).unwrap().keys().copied().collect::<HashSet<_>>();
        // Private: only the owner, even for a system admin who is not the owner.
        assert_eq!(users(&m), HashSet::from([ana]));
        let (m2, _) = db.register_machine(bia, &"cd".repeat(32), "Notebook", "macos", Visibility::Private).unwrap();
        assert!(!db.can_access(ana, &m2).unwrap(), "admins see machines but do not access them");

        // Shared with a user and with a group.
        let fam = db.create_group("Família", caio).unwrap();
        db.set_shares(m.id, &[(bia, ShareRole::View)], &[(fam, ShareRole::Control)]).unwrap();
        assert_eq!(users(&m), HashSet::from([ana]), "shares only count when shared");
        db.set_visibility(m.id, Visibility::Shared).unwrap();
        let m = db.machine(m.id).unwrap().unwrap();
        assert_eq!(users(&m), HashSet::from([ana, bia, caio]));
        assert_eq!(db.access_of(bia, &m).unwrap(), Access::VIEW);
        assert_eq!(db.access_of(caio, &m).unwrap(), Access::FULL);

        // Caio leaves the group → loses access → revocation.
        db.set_member(fam, bia, GroupRole::Admin).unwrap();
        db.set_member(fam, ana, GroupRole::Admin).unwrap();
        let before = db.snapshot(Some(&db.machines_shared_with_group(fam).unwrap())).unwrap();
        db.remove_member(fam, caio).unwrap();
        assert_eq!(db.revoke_lost(before).unwrap(), vec![m.id]);
        let (cursor, revs) = db.revocations(m.id).unwrap();
        assert!(cursor > 0);
        assert_eq!(revs.iter().map(|r| r.0).collect::<Vec<_>>(), vec![caio]);

        // Narrowing access revokes too (Bia: full control through the group → view only).
        let before = db.snapshot(Some(&[m.id])).unwrap();
        db.remove_member(fam, bia).unwrap();
        assert_eq!(db.revoke_lost(before).unwrap(), vec![m.id]);
        assert_eq!(db.access_of(bia, &m).unwrap(), Access::VIEW);
        // A co-manager has full access and may manage.
        db.set_managers(m.id, &[bia]).unwrap();
        assert_eq!(db.access_of(bia, &m).unwrap(), Access::FULL);
        assert!(db.can_manage(bia, &m).unwrap() && !db.can_manage(caio, &m).unwrap());
        db.set_managers(m.id, &[]).unwrap();

        // Everyone: all enabled users.
        db.set_visibility(m.id, Visibility::Everyone).unwrap();
        let m = db.machine(m.id).unwrap().unwrap();
        assert_eq!(users(&m), HashSet::from([ana, bia, caio]));

        // Someone else cannot take a registered machine over.
        assert!(db.register_machine(bia, &"ab".repeat(32), "x", "", Visibility::Private).is_err());
        // The owner can re-register it (new token).
        let (_, t2) = db.register_machine(ana, &"ab".repeat(32), "PC", "windows", Visibility::Private).unwrap();
        assert!(db.machine_by_token(&token).unwrap().is_none());
        assert!(db.machine_by_token(&t2).unwrap().is_some());
    }

    #[test]
    fn groups_keep_an_admin() {
        let db = Db::memory().unwrap();
        let a = db.add_user("a@example.com", false).unwrap().id;
        let b = db.add_user("b@example.com", false).unwrap().id;
        let g = db.create_group("G", a).unwrap();
        db.set_member(g, b, GroupRole::Member).unwrap();
        assert!(db.remove_member(g, a).is_err(), "last admin cannot leave");
        assert!(db.set_member(g, a, GroupRole::Member).is_err(), "nor be demoted");
        db.set_member(g, b, GroupRole::Admin).unwrap();
        db.remove_member(g, a).unwrap();
        assert_eq!(db.group(g).unwrap().unwrap().members, vec![(b, GroupRole::Admin)]);
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn sessions_are_recorded_and_closed() {
        let db = Db::memory().unwrap();
        let ana = db.add_user("ana@example.com", false).unwrap().id;
        let (m, _) = db.register_machine(ana, &"ab".repeat(32), "PC", "", Visibility::Private).unwrap();
        let mut r = AccessRecord {
            session: "s1".into(),
            user_id: Some(ana),
            who: "ana@example.com".into(),
            client: "laptop".into(),
            method: "server ticket".into(),
            access: "screen,input,files".into(),
            started_at: 100,
            ended_at: None,
        };
        db.record_sessions(m.id, &[r.clone()]).unwrap();
        r.ended_at = Some(160);
        db.record_sessions(m.id, &[r]).unwrap();
        let h = db.history(m.id, 10).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].ended_at, Some(160));
    }
}
