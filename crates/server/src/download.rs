//! Getting rdesk from your own server: `/download` lists the version released to everyone
//! for each system, `/download/windows` hands out the Windows program already named
//! `rdesk@<this server>.exe` (it then knows which server to use), and `/install.sh` installs
//! it on Linux. Every file comes from the signed release and its hash is checked.

use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use rdesk_proto::api::Ring;
use rdesk_proto::release::{FileKind, ReleaseFile, ReleaseManifest};
use sha2::{Digest, Sha256};

use crate::pages::{esc, page};
use crate::state::Shared;

/// The version released to everyone, with its checked manifest.
fn released(st: &Shared) -> Option<ReleaseManifest> {
    let version = crate::updates::released_for(&st.db, Ring::All).ok()??;
    let row = st.db.release(&version).ok()??;
    crate::updates::verify(&row.manifest, &row.signature).ok()
}

fn file(m: &ReleaseManifest, kind: FileKind, platform: &str) -> Option<ReleaseFile> {
    m.files.iter().find(|f| f.kind == kind && f.platform == platform).cloned()
}

fn github(st: &Shared, m: &ReleaseManifest, f: &ReleaseFile) -> String {
    format!("https://github.com/{}/releases/download/v{}/{}", st.cfg.update_repo, m.version, f.name)
}

fn host(st: &Shared) -> String {
    st.public.host_str().unwrap_or("rdesk").to_string()
}

/// `GET /download`.
pub async fn page_handler(State(st): State<Shared>) -> Html<String> {
    let Some(m) = released(&st) else {
        return page(
            &st.cfg.name,
            "Download rdesk",
            "<h1>Download rdesk</h1><p>No version has been released here yet.</p>",
        );
    };
    let server = esc(&host(&st));
    let link = |kind, platform: &str, label: &str| {
        file(&m, kind, platform)
            .map(|f| format!("<a href=\"{}\">{label}</a> ({} MB)", esc(&github(&st, &m, &f)), f.size / 1_000_000))
    };
    let mut body = format!("<h1>Download rdesk {}</h1>", esc(&m.version));
    body.push_str(&format!("<p>For the rdesk server <b>{server}</b>.</p>"));
    if file(&m, FileKind::Exe, "windows-x86_64").is_some() {
        body.push_str(&format!(
            "<h2>Windows</h2><p><a href=\"/download/windows\">rdesk@{server}.exe</a>: open it and sign in. To install it \
             (reachable before sign-in and kept up to date): <i>This computer → Install rdesk on this computer</i>. \
             Windows may warn the first time (the program is not signed with a certificate): <i>More info → Run anyway</i>.</p>"
        ));
    } else if let Some(l) = link(FileKind::App, "windows-x86_64", "rdesk (zip)") {
        // Releases before the plain .exe: the zip, and type the server on the sign-in page.
        body.push_str(&format!(
            "<h2>Windows</h2><p>{l}: unpack it, open <i>rdesk.exe</i> and sign in to <b>{server}</b>.</p>"
        ));
    }
    if let Some(l) = link(FileKind::App, rdesk_proto::release::ANDROID, "rdesk.apk") {
        body.push_str(&format!(
            "<h2>Android</h2><p>{l}: open it on the phone and allow installing from this source when Android asks. \
             Then sign in to <b>{server}</b>. The app tells you when a newer version is released here.</p>"
        ));
    }
    if let Some(l) = link(FileKind::App, rdesk_proto::release::IOS, "rdesk.ipa") {
        body.push_str(&format!(
            "<h2>iPhone</h2><p>{l}: not on the App Store. Install it from a computer with              <a href=\"https://sideloadly.io\">Sideloadly</a> or <a href=\"https://altstore.io\">AltStore</a>,              which sign it with your Apple ID (a free one must reinstall every 7 days; a paid developer              account lasts a year). Then sign in to <b>{server}</b>.</p>"
        ));
    }
    if let Some(l) = link(FileKind::App, "macos-aarch64", "rdesk.app (Apple Silicon)") {
        body.push_str(&format!(
            "<h2>Mac</h2><p>{l}. Unpack it, move <i>rdesk</i> to Applications and open it the first time with \
             right-click → Open (it is not notarized by Apple).</p>"
        ));
    }
    body.push_str(&format!(
        "<h2>Linux</h2><p>Debian, Ubuntu 24.04+, Raspberry Pi OS: \
         <code>curl -fsSL https://{server}/install.sh | sudo sh</code> (installs the package and keeps it up to date). \
         A computer that is only accessed: <code>… | sudo sh -s -- --host --key rdk_…</code> (or <code>--owner you@example.com</code>).</p>"
    ));
    let debs: Vec<String> =
        ["linux-x86_64", "linux-aarch64"].iter().filter_map(|p| link(FileKind::Deb, p, &format!(".deb {p}"))).collect();
    if !debs.is_empty() {
        body.push_str(&format!("<p>Or by hand: {}.</p>", debs.join(" · ")));
    }
    page(&st.cfg.name, "Download rdesk", &body)
}

/// `GET /download/windows`: the released Windows program, named after this server.
pub async fn windows(State(st): State<Shared>) -> Response {
    match windows_exe(&st).await {
        Ok((bytes, name)) => (
            [
                (header::CONTENT_TYPE, "application/vnd.microsoft.portable-executable".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            ],
            Body::from(bytes),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!("download for Windows: {e:#}");
            (StatusCode::SERVICE_UNAVAILABLE, "Not available right now; try the release page.").into_response()
        }
    }
}

async fn windows_exe(st: &Shared) -> anyhow::Result<(Vec<u8>, String)> {
    let m = released(st).ok_or_else(|| anyhow::anyhow!("nothing released"))?;
    let f = file(&m, FileKind::Exe, "windows-x86_64").ok_or_else(|| anyhow::anyhow!("{} has no .exe", m.version))?;
    let name = format!("rdesk@{}.exe", host(st));
    // Kept next to the data after the first download.
    let cache = st.data_dir.get().map(|d| d.join("downloads").join(&m.version).join(&f.name));
    if let Some(c) = &cache
        && let Ok(bytes) = std::fs::read(c)
        && hex::encode(Sha256::digest(&bytes)) == f.sha256
    {
        return Ok((bytes, name));
    }
    let url = github(st, &m, &f);
    let resp = st.http.get(&url).timeout(std::time::Duration::from_secs(300)).send().await?;
    anyhow::ensure!(resp.status().is_success(), "GET {url}: HTTP {}", resp.status());
    let bytes = resp.bytes().await?.to_vec();
    anyhow::ensure!(
        bytes.len() as u64 == f.size && hex::encode(Sha256::digest(&bytes)) == f.sha256,
        "{}: does not match the signed manifest",
        f.name
    );
    if let Some(c) = &cache {
        let _ = std::fs::create_dir_all(c.parent().unwrap());
        let _ = std::fs::write(c, &bytes);
    }
    Ok((bytes, name))
}

/// `GET /install.sh`: installs the released .deb (amd64 or arm64), turns on updates, and
/// with `--host` makes it a computer that is only accessed.
pub async fn install_sh(State(st): State<Shared>) -> Response {
    let Some(m) = released(&st) else {
        return (StatusCode::NOT_FOUND, "echo 'No rdesk version has been released on this server yet.' >&2; exit 1\n")
            .into_response();
    };
    let deb = |p: &str| file(&m, FileKind::Deb, p);
    let (Some(amd), Some(arm)) = (deb("linux-x86_64"), deb("linux-aarch64")) else {
        return (StatusCode::NOT_FOUND, "echo 'This version has no Debian packages.' >&2; exit 1\n").into_response();
    };
    let script = SCRIPT
        .replace("@SERVER@", &host(&st))
        .replace("@VERSION@", &m.version)
        .replace("@AMD_URL@", &github(&st, &m, &amd))
        .replace("@AMD_SHA@", &amd.sha256)
        .replace("@ARM_URL@", &github(&st, &m, &arm))
        .replace("@ARM_SHA@", &arm.sha256);
    ([(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script).into_response()
}

const SCRIPT: &str = r#"#!/bin/sh
# rdesk @VERSION@ for Linux, from the rdesk server @SERVER@.
#   curl -fsSL https://@SERVER@/install.sh | sudo sh
#   … | sudo sh -s -- --host --key rdk_…              only accessed: added at once
#   … | sudo sh -s -- --host --owner you@example.com   only accessed: its owner approves it
#   (--name "PC da sala" names it on the server)
set -eu
SERVER="@SERVER@"
HOST=0; KEY=""; OWNER=""; NAME=""
while [ $# -gt 0 ]; do
  case "$1" in
    --host) HOST=1 ;;
    --key) KEY="$2"; shift ;;
    --owner) OWNER="$2"; shift ;;
    --name) NAME="$2"; shift ;;
    *) echo "unknown option $1" >&2; exit 1 ;;
  esac
  shift
done
[ "$(id -u)" = 0 ] || { echo "Run it with sudo." >&2; exit 1; }
case "$(uname -m)" in
  x86_64) URL="@AMD_URL@"; SHA="@AMD_SHA@" ;;
  aarch64|arm64) URL="@ARM_URL@"; SHA="@ARM_SHA@" ;;
  *) echo "No rdesk package for $(uname -m)." >&2; exit 1 ;;
esac
DEB="/tmp/rdesk_@VERSION@.deb"
echo "Downloading rdesk @VERSION@…"
curl -fsSL -o "$DEB" "$URL"
echo "$SHA  $DEB" | sha256sum -c --quiet
apt-get install -y "$DEB" >/dev/null
rm -f "$DEB"

# Updates: the version released on the server, every hour.
printf 'RDESK_SERVER=%s\n' "$SERVER" > /etc/default/rdesk-update
systemctl daemon-reload
systemctl enable --now rdesk-update.timer >/dev/null

if [ "$HOST" = 1 ]; then
  U="${SUDO_USER:-}"
  [ -n "$U" ] && [ "$U" != root ] || { echo "--host: run it with sudo from the desktop user's account." >&2; exit 1; }
  HOME_U=$(getent passwd "$U" | cut -d: -f6)
  UID_U=$(id -u "$U")
  install -d -o "$U" -g "$(id -gn "$U")" "$HOME_U/.config"
  ENV="$HOME_U/.config/rdesk-enroll.env"
  {
    printf 'RDESK_SERVER=%s\n' "$SERVER"
    [ -n "$KEY" ] && printf 'RDESK_ENROLL_KEY=%s\n' "$KEY"
    [ -n "$OWNER" ] && printf 'RDESK_OWNER=%s\n' "$OWNER"
    [ -n "$NAME" ] && printf 'RDESK_NAME=%s\n' "$NAME"
  } > "$ENV"
  chown "$U" "$ENV"; chmod 600 "$ENV"
  loginctl enable-linger "$U"
  runuser -u "$U" -- env XDG_RUNTIME_DIR="/run/user/$UID_U" systemctl --user daemon-reload
  runuser -u "$U" -- env XDG_RUNTIME_DIR="/run/user/$UID_U" systemctl --user enable --now rdesk-host
  echo "rdesk @VERSION@ is installed; this computer asks $SERVER to add it (the host needs the X11 desktop session)."
else
  echo "rdesk @VERSION@ is installed: open it from the applications menu, or run rdesk."
fi
"#;
