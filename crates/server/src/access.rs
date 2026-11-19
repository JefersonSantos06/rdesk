//! Machines, groups, sharing and who may access what.
//!
//! A user may access a machine when they registered it, when it is visible to everyone,
//! or when it is "shared" and shared with them directly or with a group they belong to.
//! System admins see every machine but follow the same rule to access one.
//!
//! When a change takes access away, a revocation is recorded for that machine and user:
//! the host refuses the user's older tickets (they last at most 12 h, so revocations
//! older than that are dropped).

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use rdesk_proto::api::{GroupRole, Visibility};
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

/// Who may access each machine before a change, to find who lost access after it.
pub type Snapshot = HashMap<i64, HashSet<i64>>;

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

    /// `(user_ids, group_ids)` the machine is shared with.
    pub fn shares(&self, machine_id: i64) -> Result<(Vec<i64>, Vec<i64>)> {
        let rows: Vec<(Option<i64>, Option<i64>)> = self.with(|c| {
            let mut st = c.prepare("SELECT user_id, group_id FROM machine_shares WHERE machine_id = ?1")?;
            st.query_map([machine_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
        })?;
        Ok((rows.iter().filter_map(|r| r.0).collect(), rows.iter().filter_map(|r| r.1).collect()))
    }

    pub fn set_shares(&self, machine_id: i64, users: &[i64], groups: &[i64]) -> Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM machine_shares WHERE machine_id = ?1", [machine_id])?;
        for u in users.iter().collect::<HashSet<_>>() {
            tx.execute("INSERT INTO machine_shares (machine_id, user_id) VALUES (?1, ?2)", params![machine_id, u])?;
        }
        for g in groups.iter().collect::<HashSet<_>>() {
            tx.execute("INSERT INTO machine_shares (machine_id, group_id) VALUES (?1, ?2)", params![machine_id, g])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Everyone who may access `m` right now.
    pub fn access_set(&self, m: &MachineRow) -> Result<HashSet<i64>> {
        let mut set = HashSet::from([m.owner_id]);
        match m.visibility {
            Visibility::Private => {}
            Visibility::Everyone => {
                let all: Vec<i64> = self.with(|c| {
                    let mut st = c.prepare("SELECT id FROM users WHERE disabled = 0")?;
                    st.query_map([], |r| r.get(0))?.collect()
                })?;
                set.extend(all);
            }
            Visibility::Shared => {
                let ids: Vec<i64> = self.with(|c| {
                    let mut st = c.prepare(
                        "SELECT user_id FROM machine_shares WHERE machine_id = ?1 AND user_id IS NOT NULL
                         UNION
                         SELECT gm.user_id FROM machine_shares s JOIN group_members gm ON gm.group_id = s.group_id
                         WHERE s.machine_id = ?1",
                    )?;
                    st.query_map([m.id], |r| r.get(0))?.collect()
                })?;
                set.extend(ids);
            }
        }
        Ok(set)
    }

    pub fn can_access(&self, user_id: i64, m: &MachineRow) -> Result<bool> {
        Ok(self.access_set(m)?.contains(&user_id))
    }

    /// Who may access the given machines (all of them when `None`).
    pub fn snapshot(&self, machines: Option<&[i64]>) -> Result<Snapshot> {
        let mut out = HashMap::new();
        for m in self.machines()? {
            if machines.is_none_or(|ids| ids.contains(&m.id)) {
                out.insert(m.id, self.access_set(&m)?);
            }
        }
        Ok(out)
    }

    /// Record a revocation for everyone who had access in `before` and lost it.
    /// Returns the machines with new revocations (to wake their hosts).
    pub fn revoke_lost(&self, before: Snapshot) -> Result<Vec<i64>> {
        let t = now();
        let mut touched = Vec::new();
        for (mid, had) in before {
            let Some(m) = self.machine(mid)? else { continue };
            let has = self.access_set(&m)?;
            let lost: Vec<i64> = had.difference(&has).copied().collect();
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

        // Private: only the owner, even for a system admin who is not the owner.
        assert_eq!(db.access_set(&m).unwrap(), HashSet::from([ana]));
        let (m2, _) = db.register_machine(bia, &"cd".repeat(32), "Notebook", "macos", Visibility::Private).unwrap();
        assert!(!db.can_access(ana, &m2).unwrap(), "admins see machines but do not access them");

        // Shared with a user and with a group.
        let fam = db.create_group("Família", caio).unwrap();
        db.set_shares(m.id, &[bia], &[fam]).unwrap();
        assert_eq!(db.access_set(&m).unwrap(), HashSet::from([ana]), "shares only count when shared");
        db.set_visibility(m.id, Visibility::Shared).unwrap();
        let m = db.machine(m.id).unwrap().unwrap();
        assert_eq!(db.access_set(&m).unwrap(), HashSet::from([ana, bia, caio]));

        // Caio leaves the group → loses access → revocation.
        db.set_member(fam, bia, GroupRole::Admin).unwrap();
        let before = db.snapshot(Some(&db.machines_shared_with_group(fam).unwrap())).unwrap();
        db.remove_member(fam, caio).unwrap();
        assert_eq!(db.revoke_lost(before).unwrap(), vec![m.id]);
        let (cursor, revs) = db.revocations(m.id).unwrap();
        assert!(cursor > 0);
        assert_eq!(revs.iter().map(|r| r.0).collect::<Vec<_>>(), vec![caio]);

        // Everyone: all enabled users.
        db.set_visibility(m.id, Visibility::Everyone).unwrap();
        let m = db.machine(m.id).unwrap().unwrap();
        assert_eq!(db.access_set(&m).unwrap(), HashSet::from([ana, bia, caio]));

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
