//! Published versions: the `release.json` manifest attached to each GitHub Release and
//! the keys that sign it.
//!
//! The signature covers the manifest's exact bytes (prefixed with [`SIGNING_CONTEXT`]),
//! and the manifest lists the SHA-256 of every file. An app that trusts [`RELEASE_KEYS`]
//! can therefore download from anywhere (GitHub, a mirror, the rdesk server) and still
//! refuse a file someone altered. The private keys never touch the server.

use serde::{Deserialize, Serialize};

/// Public keys (Ed25519, hex) allowed to sign releases: the one CI uses, then a reserve
/// kept offline, so the first can be replaced by a release signed with the second.
pub const RELEASE_KEYS: [&str; 2] = [
    "986dfe1bedfd2d6b461ec6ca49bf45fcbede11926e69b1eb756dbb94f8cd9492",
    "44b531e246cac5eacc37039a53ff51b88c9e4551f87573e9f872ee4b316d08f2",
];

/// Prepended to the manifest bytes before signing, so a release signature can never be
/// mistaken for (or replayed as) any other Ed25519 signature.
pub const SIGNING_CONTEXT: &[u8] = b"rdesk-release-v1\0";

/// Name of the manifest asset, and of its detached signature (hex).
pub const MANIFEST_NAME: &str = "release.json";
pub const SIGNATURE_NAME: &str = "release.json.sig";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// `MAJOR.MINOR.PATCH`, the workspace version (the tag without its `v`).
    pub version: String,
    /// [`crate::PROTOCOL_VERSION`] of this release.
    pub protocol: u16,
    /// Unix seconds.
    pub published_at: i64,
    pub files: Vec<ReleaseFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFile {
    /// Asset name on the release, e.g. `rdesk-0.2.0-windows-x86_64.zip`.
    pub name: String,
    pub kind: FileKind,
    /// `windows-x86_64`, `linux-x86_64` or `linux-aarch64`.
    pub platform: String,
    /// Hex.
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// The app (client + host): a `.zip` on Windows, a `.tar.gz` on Linux.
    App,
    /// The app as a Debian package.
    Deb,
    /// `rdesk-server` (`.tar.gz`).
    Server,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::App => "app",
            FileKind::Deb => "deb",
            FileKind::Server => "server",
        }
    }
}

pub const PLATFORMS: [&str; 3] = ["windows-x86_64", "linux-x86_64", "linux-aarch64"];

/// What an asset is, from its name:
/// `rdesk-<v>-<platform>.zip|.tar.gz`, `rdesk-server-<v>-<platform>.tar.gz`,
/// `rdesk_<v>_amd64.deb|arm64.deb`.
pub fn classify(name: &str, version: &str) -> Option<(FileKind, &'static str)> {
    if let Some(rest) = name.strip_prefix(&format!("rdesk_{version}_")) {
        let platform = match rest {
            "amd64.deb" => "linux-x86_64",
            "arm64.deb" => "linux-aarch64",
            _ => return None,
        };
        return Some((FileKind::Deb, platform));
    }
    let (kind, rest) = match name.strip_prefix(&format!("rdesk-server-{version}-")) {
        Some(rest) => (FileKind::Server, rest),
        None => (FileKind::App, name.strip_prefix(&format!("rdesk-{version}-"))?),
    };
    PLATFORMS
        .into_iter()
        .find(|p| {
            let ext = if *p == "windows-x86_64" && kind == FileKind::App { ".zip" } else { ".tar.gz" };
            rest.strip_prefix(p) == Some(ext)
        })
        .map(|p| (kind, p))
}

/// The bytes a release key signs.
pub fn signed_message(manifest_json: &[u8]) -> Vec<u8> {
    [SIGNING_CONTEXT, manifest_json].concat()
}

/// `MAJOR.MINOR.PATCH` as numbers, for ordering versions (no pre-release suffixes).
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split('.').map(|p| p.parse::<u64>().ok());
    let r = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names() {
        let v = "0.2.0";
        assert_eq!(classify("rdesk-0.2.0-windows-x86_64.zip", v), Some((FileKind::App, "windows-x86_64")));
        assert_eq!(classify("rdesk-0.2.0-linux-aarch64.tar.gz", v), Some((FileKind::App, "linux-aarch64")));
        assert_eq!(classify("rdesk-server-0.2.0-linux-x86_64.tar.gz", v), Some((FileKind::Server, "linux-x86_64")));
        assert_eq!(classify("rdesk_0.2.0_arm64.deb", v), Some((FileKind::Deb, "linux-aarch64")));
        assert_eq!(classify("rdesk-0.1.0-linux-x86_64.tar.gz", v), None, "other version");
        assert_eq!(classify("rdesk-0.2.0-windows-x86_64.tar.gz", v), None);
        assert_eq!(classify("rdesk-0.2.0-linux-x86_64.zip", v), None);
        assert_eq!(classify("notes.txt", v), None);
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.20.3"), Some((1, 20, 3)));
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.3-rc1"), None);
    }

    #[test]
    fn keys_are_hex() {
        for k in RELEASE_KEYS {
            assert_eq!(k.len(), 64);
            assert!(k.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        assert_ne!(RELEASE_KEYS[0], RELEASE_KEYS[1]);
    }
}
