//! New rdesk versions. The server looks at the GitHub releases every hour, keeps the ones
//! whose `release.json` is signed by an rdesk release key, and an admin releases a version
//! to the test computers, then to everyone. Hosts get it in their sync; they check the
//! signature again themselves and download the files straight from GitHub.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use rdesk_proto::api::{ReleaseInfo, ReleaseTo, Releases, Ring, SetRing, UpdateOffer};
use rdesk_proto::release::{RELEASE_KEYS, ReleaseManifest, parse_version, signed_message};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;

use crate::api::{Admin, Error};
use crate::db::Db;
use crate::state::Shared;

const KEY_TEST: &str = "released.test";
const KEY_ALL: &str = "released.all";

#[derive(Debug, Clone)]
pub struct ReleaseRow {
    pub version: String,
    pub manifest: String,
    pub signature: String,
    pub notes: String,
    pub published_at: i64,
}

/// When GitHub was last asked, and the error if that failed.
#[derive(Debug, Default)]
pub struct CheckState {
    pub last: Option<i64>,
    pub error: Option<String>,
}

fn err(code: StatusCode, msg: &str) -> Error {
    Error(code, msg.to_string())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Db {
    pub fn add_release(&self, r: &ReleaseRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO releases (version, manifest, signature, notes, published_at, found_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![r.version, r.manifest, r.signature, r.notes, r.published_at, now()],
            )
        })?;
        Ok(())
    }

    pub fn releases(&self) -> Result<Vec<ReleaseRow>> {
        let mut rows = self.with(|c| {
            let mut st = c.prepare("SELECT version, manifest, signature, notes, published_at FROM releases")?;
            st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        rows.sort_by_key(|r| std::cmp::Reverse(parse_version(&r.version)));
        Ok(rows)
    }

    pub fn release(&self, version: &str) -> Result<Option<ReleaseRow>> {
        self.with(|c| {
            c.query_row(
                "SELECT version, manifest, signature, notes, published_at FROM releases WHERE version = ?1",
                [version],
                row,
            )
            .optional()
        })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
                params![key, value],
            )
        })?;
        Ok(())
    }

    pub fn set_ring(&self, machine_id: i64, ring: Ring) -> Result<bool> {
        let ring = if ring == Ring::Test { "test" } else { "all" };
        Ok(self.with(|c| c.execute("UPDATE machines SET update_ring = ?2 WHERE id = ?1", params![machine_id, ring]))?
            == 1)
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<ReleaseRow> {
    Ok(ReleaseRow {
        version: r.get(0)?,
        manifest: r.get(1)?,
        signature: r.get(2)?,
        notes: r.get(3)?,
        published_at: r.get(4)?,
    })
}

/// A `release.json` signed by one of the release keys built into rdesk.
pub fn verify(manifest: &str, signature_hex: &str) -> Result<ReleaseManifest> {
    let sig: [u8; 64] = hex::decode(signature_hex.trim())
        .context("signature is not hex")?
        .try_into()
        .map_err(|_| anyhow!("signature has the wrong length"))?;
    let sig = iroh_base::Signature::from_bytes(&sig);
    let msg = signed_message(manifest.as_bytes());
    let ok = RELEASE_KEYS.iter().any(|k| {
        let Ok(bytes) = hex::decode(k) else { return false };
        let Ok(bytes) = <[u8; 32]>::try_from(bytes) else { return false };
        iroh_base::PublicKey::from_bytes(&bytes).is_ok_and(|key| key.verify(&msg, &sig).is_ok())
    });
    ensure!(ok, "not signed by an rdesk release key");
    serde_json::from_str(manifest).context("reading release.json")
}

/// The version released for a computer in `ring` (test computers also get what everyone
/// gets, whichever is newer).
pub fn released_for(db: &Db, ring: Ring) -> Result<Option<String>> {
    let all = db.setting(KEY_ALL)?;
    if ring == Ring::All {
        return Ok(all);
    }
    let test = db.setting(KEY_TEST)?;
    Ok(match (test, all) {
        (Some(t), Some(a)) => Some(if parse_version(&t) >= parse_version(&a) { t } else { a }),
        (t, a) => t.or(a),
    })
}

pub fn offer(st: &Shared, ring: Ring) -> Result<Option<UpdateOffer>> {
    let Some(version) = released_for(&st.db, ring)? else { return Ok(None) };
    let Some(r) = st.db.release(&version)? else { return Ok(None) };
    Ok(Some(UpdateOffer {
        download_base: format!("https://github.com/{}/releases/download/v{}/", st.cfg.update_repo, r.version),
        version: r.version,
        manifest: r.manifest,
        signature: r.signature,
    }))
}

// ---- GitHub -----------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

async fn text(st: &Shared, url: &str) -> Result<String> {
    let r = st
        .http
        .get(url)
        .header("User-Agent", concat!("rdesk-server/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !r.status().is_success() {
        bail!("GET {url}: HTTP {}", r.status());
    }
    Ok(r.text().await?)
}

/// Look for new signed releases; returns how many were added.
pub async fn check(st: &Shared) -> Result<usize> {
    let list: Vec<GhRelease> = serde_json::from_str(
        &text(st, &format!("https://api.github.com/repos/{}/releases?per_page=15", st.cfg.update_repo)).await?,
    )
    .context("reading the GitHub releases")?;
    let mut added = 0;
    for r in list.iter().filter(|r| !r.draft && !r.prerelease) {
        let Some(version) = r.tag_name.strip_prefix('v').filter(|v| parse_version(v).is_some()) else { continue };
        if st.db.release(version)?.is_some() {
            continue;
        }
        let asset = |name: &str| r.assets.iter().find(|a| a.name == name).map(|a| a.browser_download_url.clone());
        let (Some(m_url), Some(s_url)) = (asset("release.json"), asset("release.json.sig")) else {
            tracing::info!("release {version}: no release.json, skipped");
            continue;
        };
        let (manifest, signature) = (text(st, &m_url).await?, text(st, &s_url).await?);
        let m = match verify(&manifest, &signature) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("release {version}: {e:#}; ignored");
                continue;
            }
        };
        if m.version != version {
            tracing::warn!("release {version}: its release.json says {}; ignored", m.version);
            continue;
        }
        st.db.add_release(&ReleaseRow {
            version: version.to_string(),
            manifest,
            signature: signature.trim().to_string(),
            notes: r.body.clone().unwrap_or_default(),
            published_at: m.published_at,
        })?;
        tracing::info!("found rdesk {version}");
        added += 1;
    }
    Ok(added)
}

/// Every hour, or when an admin asks.
pub fn spawn_checker(st: Shared) {
    if st.cfg.update_repo.is_empty() {
        return;
    }
    tokio::spawn(async move {
        loop {
            let res = check(&st).await;
            {
                let mut c = st.update_check.lock().unwrap();
                c.last = Some(now());
                c.error = res.err().map(|e| format!("{e:#}"));
                if let Some(e) = &c.error {
                    tracing::warn!("checking for new versions: {e}");
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(3600), st.update_wake.notified()).await;
        }
    });
}

// ---- API -------------------------------------------------------------------------------

fn platforms(manifest: &str) -> (u16, Vec<String>) {
    match serde_json::from_str::<ReleaseManifest>(manifest) {
        Ok(m) => {
            let mut p: Vec<String> = m.files.into_iter().map(|f| f.platform).collect();
            p.sort();
            p.dedup();
            (m.protocol, p)
        }
        Err(_) => (0, Vec::new()),
    }
}

/// `GET /api/admin/releases`.
pub async fn list(State(st): State<Shared>, _: Admin) -> Result<Json<Releases>, Error> {
    let machines = st.db.machines()?;
    let releases = st
        .db
        .releases()?
        .into_iter()
        .map(|r| {
            let (protocol, platforms) = platforms(&r.manifest);
            let running = machines.iter().filter(|m| m.version == r.version).count();
            ReleaseInfo {
                version: r.version,
                notes: r.notes,
                published_at: r.published_at,
                protocol,
                platforms,
                running,
            }
        })
        .collect();
    let check = st.update_check.lock().unwrap();
    Ok(Json(Releases {
        releases,
        test: st.db.setting(KEY_TEST)?,
        all: st.db.setting(KEY_ALL)?,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        last_check: check.last,
        check_error: check.error.clone(),
        server_can_update: crate::selfupdate::supported(),
        server_update: st.data_dir.get().and_then(|d| crate::selfupdate::last_result(d)),
    }))
}

/// `POST /api/admin/releases/{version}/server`: update this server (in the background).
pub async fn update_server(
    State(st): State<Shared>,
    Admin(s): Admin,
    Path(version): Path<String>,
) -> Result<StatusCode, Error> {
    let Some(data) = st.data_dir.get().cloned() else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "the server is still starting"));
    };
    if !crate::selfupdate::supported() {
        return Err(err(
            StatusCode::CONFLICT,
            "this server can not update itself (only under systemd, outside containers)",
        ));
    }
    st.db.audit(Some(s.user.id), "server update", &version);
    tracing::info!("{} asked to update the server to {version}", s.user.email);
    tokio::spawn(async move {
        if let Err(e) = crate::selfupdate::prepare(&st, &data, &version).await {
            tracing::warn!("server update to {version}: {e:#}");
            let dir = data.join(crate::selfupdate::UPDATE_DIR);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(crate::selfupdate::RESULT), format!("update to {version} failed: {e:#}"));
        }
    });
    Ok(StatusCode::ACCEPTED)
}

/// `POST /api/admin/releases/check`: ask GitHub now.
pub async fn check_now(State(st): State<Shared>, _: Admin) -> StatusCode {
    st.update_wake.notify_one();
    StatusCode::ACCEPTED
}

/// `POST /api/admin/releases/{version}/release`.
pub async fn release(
    State(st): State<Shared>,
    Admin(s): Admin,
    Path(version): Path<String>,
    Json(to): Json<ReleaseTo>,
) -> Result<StatusCode, Error> {
    if st.db.release(&version)?.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "no such version"));
    }
    let key = if to.ring == Ring::Test { KEY_TEST } else { KEY_ALL };
    // Hosts only move forward: releasing an older version would not take any of them back.
    if let Some(cur) = st.db.setting(key)?
        && parse_version(&version) < parse_version(&cur)
    {
        return Err(err(StatusCode::CONFLICT, &format!("{cur} is already released there; computers do not go back")));
    }
    st.db.set_setting(key, &version)?;
    let ring = if to.ring == Ring::Test { "the test computers" } else { "everyone" };
    st.db.audit(Some(s.user.id), "release", &format!("{version} to {ring}"));
    tracing::info!("{} released {version} to {ring}", s.user.email);
    st.wake_all();
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/admin/machines/{id}/ring`.
pub async fn set_ring(
    State(st): State<Shared>,
    Admin(s): Admin,
    Path(id): Path<i64>,
    Json(r): Json<SetRing>,
) -> Result<StatusCode, Error> {
    if !st.db.set_ring(id, r.ring)? {
        return Err(err(StatusCode::NOT_FOUND, "no such computer"));
    }
    st.db.audit(Some(s.user.id), "update ring", &format!("machine {id}: {:?}", r.ring));
    st.wake(&[id]);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct LatestQuery {
    #[serde(default)]
    machine: Option<String>,
}

/// `GET /api/update/latest?machine=<host ID>`: what that computer (or anyone) may install.
pub async fn latest(
    State(st): State<Shared>,
    Query(q): Query<LatestQuery>,
) -> Result<Json<Option<UpdateOffer>>, Error> {
    let ring = match q.machine {
        Some(id) => st.db.machine_by_endpoint(&id.to_lowercase())?.map(|m| m.ring).unwrap_or_default(),
        None => Ring::All,
    };
    Ok(Json(offer(&st, ring)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rings() {
        let db = Db::memory().unwrap();
        assert_eq!(released_for(&db, Ring::Test).unwrap(), None);
        db.set_setting(KEY_ALL, "0.3.1").unwrap();
        assert_eq!(released_for(&db, Ring::Test).unwrap().as_deref(), Some("0.3.1"));
        db.set_setting(KEY_TEST, "0.4.0").unwrap();
        assert_eq!(released_for(&db, Ring::Test).unwrap().as_deref(), Some("0.4.0"));
        assert_eq!(released_for(&db, Ring::All).unwrap().as_deref(), Some("0.3.1"));
        db.set_setting(KEY_ALL, "0.4.1").unwrap();
        assert_eq!(released_for(&db, Ring::Test).unwrap().as_deref(), Some("0.4.1"), "everyone's newer one");
    }

    #[test]
    fn signatures() {
        assert!(verify("{}", &"00".repeat(64)).is_err());
        assert!(verify("{}", "zz").is_err());
    }
}
