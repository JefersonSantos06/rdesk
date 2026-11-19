//! SQLite storage. One connection behind a mutex: the load of a family server is tiny and
//! every query is short.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// A login lasts this long, used or not; then the user logs in again (with 2FA).
pub const SESSION_LIFETIME_SECS: i64 = 10 * 24 * 3600;

const SCHEMA: &[&str] = &[
    // v1: accounts, second factors, sessions, audit.
    r#"
    CREATE TABLE users (
        id INTEGER PRIMARY KEY,
        email TEXT NOT NULL UNIQUE COLLATE NOCASE,
        name TEXT NOT NULL DEFAULT '',
        is_admin INTEGER NOT NULL DEFAULT 0,
        disabled INTEGER NOT NULL DEFAULT 0,
        totp_secret TEXT,
        totp_last_step INTEGER NOT NULL DEFAULT 0,
        webauthn_id TEXT,
        created_at INTEGER NOT NULL,
        last_login INTEGER
    );
    CREATE TABLE identities (
        provider TEXT NOT NULL,
        subject TEXT NOT NULL,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        PRIMARY KEY (provider, subject)
    );
    CREATE TABLE passkeys (
        id INTEGER PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        data TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_used INTEGER
    );
    CREATE TABLE sessions (
        id INTEGER PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        token_hash BLOB NOT NULL UNIQUE,
        device TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL,
        revoked INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE audit (
        id INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        user_id INTEGER,
        event TEXT NOT NULL,
        detail TEXT NOT NULL DEFAULT ''
    );
    "#,
    // v2: machines, groups, sharing, ticket revocations.
    r#"
    CREATE TABLE groups (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE group_members (
        group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        role TEXT NOT NULL,
        PRIMARY KEY (group_id, user_id)
    );
    CREATE TABLE machines (
        id INTEGER PRIMARY KEY,
        endpoint_id TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        os TEXT NOT NULL DEFAULT '',
        owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        visibility TEXT NOT NULL DEFAULT 'private',
        token_hash BLOB NOT NULL UNIQUE,
        addrs TEXT NOT NULL DEFAULT '',
        in_session INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        last_seen INTEGER
    );
    CREATE TABLE machine_shares (
        machine_id INTEGER NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
        user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
        group_id INTEGER REFERENCES groups(id) ON DELETE CASCADE,
        role TEXT NOT NULL DEFAULT 'control'
    );
    CREATE UNIQUE INDEX machine_shares_user ON machine_shares(machine_id, user_id) WHERE user_id IS NOT NULL;
    CREATE UNIQUE INDEX machine_shares_group ON machine_shares(machine_id, group_id) WHERE group_id IS NOT NULL;
    CREATE TABLE revocations (
        id INTEGER PRIMARY KEY,
        machine_id INTEGER NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL,
        before INTEGER NOT NULL
    );
    "#,
    // v3: co-managers, access history.
    r#"
    CREATE TABLE machine_managers (
        machine_id INTEGER NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        PRIMARY KEY (machine_id, user_id)
    );
    CREATE TABLE access_log (
        id INTEGER PRIMARY KEY,
        machine_id INTEGER NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
        session TEXT NOT NULL,
        user_id INTEGER,
        who TEXT NOT NULL,
        client TEXT NOT NULL,
        method TEXT NOT NULL,
        access TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        ended_at INTEGER,
        UNIQUE (machine_id, session)
    );
    "#,
    // v4: adding computers without signing in on them (approval requests, enrollment keys).
    r#"
    CREATE TABLE machine_requests (
        id INTEGER PRIMARY KEY,
        endpoint_id TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        os TEXT NOT NULL DEFAULT '',
        owner_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
        rejected INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL
    );
    CREATE TABLE enroll_keys (
        id INTEGER PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        key_hash BLOB NOT NULL UNIQUE,
        prefix TEXT NOT NULL,
        reusable INTEGER NOT NULL,
        visibility TEXT NOT NULL,
        shares TEXT NOT NULL DEFAULT '{}',
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        uses INTEGER NOT NULL DEFAULT 0,
        revoked INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE removed_machines (
        endpoint_id TEXT PRIMARY KEY,
        removed_at INTEGER NOT NULL
    );
    "#,
    // v5: the host reports whether its own password/key also open it on the LAN.
    r#"
    ALTER TABLE machines ADD COLUMN direct_access INTEGER NOT NULL DEFAULT 0;
    "#,
    // v6: updates. Versions found on GitHub (signed), what is released, and per computer
    // the version it runs and whether it is a test computer.
    r#"
    CREATE TABLE releases (
        version TEXT PRIMARY KEY,
        manifest TEXT NOT NULL,
        signature TEXT NOT NULL,
        notes TEXT NOT NULL DEFAULT '',
        published_at INTEGER NOT NULL,
        found_at INTEGER NOT NULL
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ALTER TABLE machines ADD COLUMN version TEXT NOT NULL DEFAULT '';
    ALTER TABLE machines ADD COLUMN update_ring TEXT NOT NULL DEFAULT 'all';
    "#,
];

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// A random URL-safe secret (32 bytes).
pub fn random_token() -> String {
    use base64::Engine as _;
    use rand::RngCore as _;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub is_admin: bool,
    pub disabled: bool,
    pub totp_secret: Option<String>,
    pub totp_last_step: i64,
    pub webauthn_id: Option<String>,
    pub created_at: i64,
    pub last_login: Option<i64>,
}

impl User {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            email: r.get("email")?,
            name: r.get("name")?,
            is_admin: r.get("is_admin")?,
            disabled: r.get("disabled")?,
            totp_secret: r.get("totp_secret")?,
            totp_last_step: r.get("totp_last_step")?,
            webauthn_id: r.get("webauthn_id")?,
            created_at: r.get("created_at")?,
            last_login: r.get("last_login")?,
        })
    }

    pub fn info(&self) -> rdesk_proto::api::UserInfo {
        rdesk_proto::api::UserInfo {
            id: self.id,
            email: self.email.clone(),
            name: self.name.clone(),
            is_admin: self.is_admin,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredPasskey {
    pub id: i64,
    pub name: String,
    /// `webauthn_rs::prelude::Passkey` as JSON.
    pub data: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub user: User,
    pub expires_at: i64,
}

#[derive(Clone)]
pub struct Db(pub(crate) Arc<Mutex<Connection>>);

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Db")
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    /// A consistent copy of the database (works while the server runs).
    pub fn backup(&self, to: &Path) -> Result<()> {
        if to.exists() {
            bail!("{} already exists", to.display());
        }
        let target = to.to_string_lossy().to_string();
        self.with(|c| c.execute("VACUUM INTO ?1", [target]))?;
        Ok(())
    }

    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, step) in SCHEMA.iter().enumerate().skip(version as usize) {
            conn.execute_batch(&format!("BEGIN; {step} PRAGMA user_version = {}; COMMIT;", i + 1))
                .with_context(|| format!("migrating the database to v{}", i + 1))?;
        }
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.0.lock().unwrap();
        Ok(f(&conn)?)
    }

    // ---- users ------------------------------------------------------------------

    /// Allow `email` to log in. Fails if it is already there.
    pub fn add_user(&self, email: &str, is_admin: bool) -> Result<User> {
        let email = normalize_email(email)?;
        if self.user_by_email(&email)?.is_some() {
            bail!("{email} is already allowed");
        }
        self.with(|c| {
            c.execute(
                "INSERT INTO users (email, is_admin, created_at) VALUES (?1, ?2, ?3)",
                params![email, is_admin, now()],
            )
        })?;
        self.user_by_email(&email)?.context("user just added")
    }

    pub fn user(&self, id: i64) -> Result<Option<User>> {
        self.with(|c| c.query_row("SELECT * FROM users WHERE id = ?1", [id], User::from_row).optional())
    }

    pub fn user_by_email(&self, email: &str) -> Result<Option<User>> {
        self.with(|c| c.query_row("SELECT * FROM users WHERE email = ?1", [email.trim()], User::from_row).optional())
    }

    pub fn users(&self) -> Result<Vec<User>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM users ORDER BY email")?;
            st.query_map([], User::from_row)?.collect()
        })
    }

    pub fn remove_user(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM users WHERE id = ?1", [id]))?;
        Ok(())
    }

    pub fn set_admin(&self, id: i64, admin: bool) -> Result<()> {
        if !admin && self.admin_count_except(id)? == 0 {
            bail!("there must be at least one admin");
        }
        self.with(|c| c.execute("UPDATE users SET is_admin = ?2 WHERE id = ?1", params![id, admin]))?;
        Ok(())
    }

    /// Disabling also ends the user's sessions.
    pub fn set_disabled(&self, id: i64, disabled: bool) -> Result<()> {
        self.with(|c| c.execute("UPDATE users SET disabled = ?2 WHERE id = ?1", params![id, disabled]))?;
        if disabled {
            self.revoke_user_sessions(id)?;
        }
        Ok(())
    }

    fn admin_count_except(&self, id: i64) -> Result<i64> {
        self.with(|c| {
            c.query_row("SELECT COUNT(*) FROM users WHERE is_admin = 1 AND disabled = 0 AND id != ?1", [id], |r| {
                r.get(0)
            })
        })
    }

    /// Removing or demoting must keep one admin.
    pub fn ensure_not_last_admin(&self, id: i64) -> Result<()> {
        let is_admin = self.user(id)?.is_some_and(|u| u.is_admin && !u.disabled);
        if is_admin && self.admin_count_except(id)? == 0 {
            bail!("there must be at least one admin");
        }
        Ok(())
    }

    pub fn set_name_if_empty(&self, id: i64, name: &str) -> Result<()> {
        self.with(|c| c.execute("UPDATE users SET name = ?2 WHERE id = ?1 AND name = ''", params![id, name.trim()]))?;
        Ok(())
    }

    pub fn touch_login(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("UPDATE users SET last_login = ?2 WHERE id = ?1", params![id, now()]))?;
        Ok(())
    }

    // ---- OAuth identities -------------------------------------------------------------

    pub fn identity_user(&self, provider: &str, subject: &str) -> Result<Option<i64>> {
        self.with(|c| {
            c.query_row(
                "SELECT user_id FROM identities WHERE provider = ?1 AND subject = ?2",
                params![provider, subject],
                |r| r.get(0),
            )
            .optional()
        })
    }

    pub fn link_identity(&self, provider: &str, subject: &str, user_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO identities (provider, subject, user_id) VALUES (?1, ?2, ?3)",
                params![provider, subject, user_id],
            )
        })?;
        Ok(())
    }

    // ---- TOTP -------------------------------------------------------------------------

    pub fn set_totp(&self, id: i64, secret_base32: &str, step: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE users SET totp_secret = ?2, totp_last_step = ?3 WHERE id = ?1",
                params![id, secret_base32, step],
            )
        })?;
        Ok(())
    }

    /// Record a used code's time step; `false` if it (or a later one) was already used.
    pub fn use_totp_step(&self, id: i64, step: i64) -> Result<bool> {
        let n = self.with(|c| {
            c.execute("UPDATE users SET totp_last_step = ?2 WHERE id = ?1 AND totp_last_step < ?2", params![id, step])
        })?;
        Ok(n == 1)
    }

    // ---- passkeys -----------------------------------------------------------------------

    pub fn webauthn_id(&self, id: i64) -> Result<String> {
        if let Some(w) = self.user(id)?.and_then(|u| u.webauthn_id) {
            return Ok(w);
        }
        let w = uuid::Uuid::new_v4().to_string();
        self.with(|c| c.execute("UPDATE users SET webauthn_id = ?2 WHERE id = ?1", params![id, w]))?;
        Ok(w)
    }

    pub fn passkeys(&self, user_id: i64) -> Result<Vec<StoredPasskey>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, name, data FROM passkeys WHERE user_id = ?1 ORDER BY id")?;
            st.query_map([user_id], |r| Ok(StoredPasskey { id: r.get(0)?, name: r.get(1)?, data: r.get(2)? }))?
                .collect()
        })
    }

    pub fn add_passkey(&self, user_id: i64, name: &str, data: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO passkeys (user_id, name, data, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![user_id, name, data, now()],
            )
        })?;
        Ok(())
    }

    pub fn update_passkey(&self, id: i64, data: &str) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE passkeys SET data = ?2, last_used = ?3 WHERE id = ?1", params![id, data, now()])
        })?;
        Ok(())
    }

    // ---- sessions -------------------------------------------------------------------------

    /// A new session; returns the token (only its hash is stored).
    pub fn create_session(&self, user_id: i64, device: &str) -> Result<(String, i64)> {
        let token = random_token();
        let t = now();
        let expires = t + SESSION_LIFETIME_SECS;
        let device: String = device.chars().take(80).collect();
        self.with(|c| {
            c.execute(
                "INSERT INTO sessions (user_id, token_hash, device, created_at, expires_at, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?4)",
                params![user_id, hash_token(&token), device, t, expires],
            )
        })?;
        Ok((token, expires))
    }

    /// The live session for `token`: not revoked, not expired, user not disabled.
    pub fn session(&self, token: &str) -> Result<Option<Session>> {
        let row: Option<(i64, i64, i64)> = self.with(|c| {
            c.query_row(
                "SELECT s.id, s.user_id, s.expires_at FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ?1 AND s.revoked = 0 AND s.expires_at > ?2 AND u.disabled = 0",
                params![hash_token(token), now()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
        })?;
        let Some((id, user_id, expires_at)) = row else { return Ok(None) };
        let Some(user) = self.user(user_id)? else { return Ok(None) };
        self.with(|c| c.execute("UPDATE sessions SET last_seen = ?2 WHERE id = ?1", params![id, now()]))?;
        Ok(Some(Session { id, user, expires_at }))
    }

    pub fn revoke_session(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("UPDATE sessions SET revoked = 1 WHERE id = ?1", [id]))?;
        Ok(())
    }

    pub fn revoke_user_sessions(&self, user_id: i64) -> Result<()> {
        self.with(|c| c.execute("UPDATE sessions SET revoked = 1 WHERE user_id = ?1", [user_id]))?;
        Ok(())
    }

    // ---- audit -----------------------------------------------------------------------------

    pub fn audit(&self, user_id: Option<i64>, event: &str, detail: &str) {
        if let Err(e) = self.with(|c| {
            c.execute(
                "INSERT INTO audit (at, user_id, event, detail) VALUES (?1, ?2, ?3, ?4)",
                params![now(), user_id, event, detail],
            )
        }) {
            tracing::warn!("audit: {e:#}");
        }
        tracing::info!(user = ?user_id, "{event} {detail}");
    }
}

pub fn normalize_email(email: &str) -> Result<String> {
    let e = email.trim().to_lowercase();
    let ok = e.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.'))
        && !e.chars().any(char::is_whitespace);
    if !ok {
        bail!("{email:?} is not an e-mail address");
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_sessions_and_admins() {
        let db = Db::memory().unwrap();
        let a = db.add_user("Ana@Example.com ", true).unwrap();
        assert_eq!(a.email, "ana@example.com");
        assert!(db.add_user("ana@example.com", false).is_err());
        assert!(db.user_by_email("ANA@example.com").unwrap().is_some());
        let b = db.add_user("bruno@example.com", false).unwrap();

        // The last admin cannot be demoted.
        assert!(db.set_admin(a.id, false).is_err());
        db.set_admin(b.id, true).unwrap();
        db.set_admin(a.id, false).unwrap();

        let (tok, exp) = db.create_session(b.id, "laptop").unwrap();
        assert!(exp - now() > 9 * 24 * 3600);
        assert_eq!(db.session(&tok).unwrap().unwrap().user.id, b.id);
        assert!(db.session("nope").unwrap().is_none());
        db.set_disabled(b.id, true).unwrap();
        assert!(db.session(&tok).unwrap().is_none(), "disabling ends sessions");
    }

    #[test]
    fn totp_steps_are_single_use() {
        let db = Db::memory().unwrap();
        let u = db.add_user("c@example.com", false).unwrap();
        db.set_totp(u.id, "SECRET", 10).unwrap();
        assert!(!db.use_totp_step(u.id, 10).unwrap());
        assert!(db.use_totp_step(u.id, 11).unwrap());
        assert!(!db.use_totp_step(u.id, 11).unwrap());
    }

    #[test]
    fn emails_are_checked() {
        assert!(normalize_email("a@b.co").is_ok());
        assert!(normalize_email("nope").is_err());
        assert!(normalize_email("a b@c.d").is_err());
        assert!(normalize_email("@c.d").is_err());
    }
}
