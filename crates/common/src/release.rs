//! Checking a published release before trusting it: the manifest's signature against the
//! release keys built into this binary, then each downloaded file against the manifest.
//! Nothing here trusts the server or the download host (see `rdesk_proto::release`).

use anyhow::{Context, Result, ensure};
use rdesk_proto::release::{RELEASE_KEYS, ReleaseFile, ReleaseManifest, signed_message};
use sha2::{Digest, Sha256};

/// The release keys built into this binary.
pub fn trusted_keys() -> Vec<[u8; 32]> {
    RELEASE_KEYS.iter().filter_map(|k| hex::decode(k).ok()?.try_into().ok()).collect()
}

/// Parse `release.json` if `signature_hex` (the `release.json.sig` contents) is a valid
/// signature over its exact bytes by one of the built-in release keys.
pub fn verify_manifest(manifest_json: &[u8], signature_hex: &str) -> Result<ReleaseManifest> {
    verify_manifest_with(&trusted_keys(), manifest_json, signature_hex)
}

pub fn verify_manifest_with(keys: &[[u8; 32]], manifest_json: &[u8], signature_hex: &str) -> Result<ReleaseManifest> {
    let sig: [u8; 64] = hex::decode(signature_hex.trim())
        .context("the release signature is not hex")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the release signature has the wrong length"))?;
    let sig = iroh::Signature::from_bytes(&sig);
    let msg = signed_message(manifest_json);
    let ok = keys.iter().any(|k| iroh::PublicKey::from_bytes(k).is_ok_and(|key| key.verify(&msg, &sig).is_ok()));
    ensure!(ok, "the release is not signed by an rdesk release key");
    serde_json::from_slice(manifest_json).context("reading release.json")
}

/// Does `data` (a downloaded file) match its manifest entry?
pub fn check_file(file: &ReleaseFile, data: &[u8]) -> Result<()> {
    ensure!(data.len() as u64 == file.size, "{}: size differs from the release manifest", file.name);
    let sha256 = hex::encode(Sha256::digest(data));
    ensure!(sha256.eq_ignore_ascii_case(&file.sha256), "{}: SHA-256 differs from the release manifest", file.name);
    Ok(())
}

/// The file of `kind` for `platform` in a release.
pub fn file_for<'a>(
    m: &'a ReleaseManifest,
    kind: rdesk_proto::release::FileKind,
    platform: &str,
) -> Option<&'a ReleaseFile> {
    m.files.iter().find(|f| f.kind == kind && f.platform == platform)
}

/// This build's platform as release files name it (`windows-x86_64`, `linux-aarch64`…).
pub fn this_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Download a release file (following GitHub's redirect), refusing anything larger than
/// `max` bytes. Check it with [`check_file`] before using it.
pub async fn download(url: &str, max: u64) -> Result<Vec<u8>> {
    let client = crate::account::http_client().timeout(std::time::Duration::from_secs(900)).build()?;
    let mut r = client.get(url).send().await.with_context(|| format!("downloading {url}"))?;
    ensure!(r.status().is_success(), "downloading {url}: HTTP {}", r.status());
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        out.extend_from_slice(&chunk);
        ensure!(out.len() as u64 <= max, "{url}: larger than the release manifest says");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdesk_proto::release::FileKind;

    fn manifest_json() -> Vec<u8> {
        let m = ReleaseManifest {
            version: "1.0.0".into(),
            protocol: rdesk_proto::PROTOCOL_VERSION,
            published_at: 1,
            files: vec![ReleaseFile {
                name: "rdesk-1.0.0-linux-x86_64.tar.gz".into(),
                kind: FileKind::App,
                platform: "linux-x86_64".into(),
                sha256: hex::encode(Sha256::digest(b"payload")),
                size: 7,
            }],
        };
        serde_json::to_vec_pretty(&m).unwrap()
    }

    #[test]
    fn signature_and_files() {
        let (key, other) = (iroh::SecretKey::generate(), iroh::SecretKey::generate());
        let json = manifest_json();
        let sig = hex::encode(key.sign(&signed_message(&json)).to_bytes());
        let keys = [*other.public().as_bytes(), *key.public().as_bytes()];

        let m = verify_manifest_with(&keys, &json, &sig).unwrap();
        assert!(verify_manifest_with(&keys[..1], &json, &sig).is_err(), "unknown key");
        assert!(verify_manifest(&json, &sig).is_err(), "not a built-in release key");
        let mut tampered = json.clone();
        tampered[20] ^= 1;
        assert!(verify_manifest_with(&keys, &tampered, &sig).is_err());
        // Signed without the release context: not a release signature.
        let bare = hex::encode(key.sign(&json).to_bytes());
        assert!(verify_manifest_with(&keys, &json, &bare).is_err());

        check_file(&m.files[0], b"payload").unwrap();
        assert!(check_file(&m.files[0], b"pAyload").is_err());
        assert!(check_file(&m.files[0], b"payload!").is_err());
    }

    /// RFC 8032 test 1: `rdesk-release` signs with ed25519-dalek; the app checks with iroh's
    /// keys. Both must be plain Ed25519.
    #[test]
    fn standard_ed25519() {
        let public = hex::decode("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a").unwrap();
        let sig = hex::decode(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        )
        .unwrap();
        let key = iroh::PublicKey::from_bytes(&public.try_into().unwrap()).unwrap();
        key.verify(b"", &iroh::Signature::from_bytes(&sig.try_into().unwrap())).unwrap();
    }

    #[test]
    fn platform_names() {
        let p = this_platform();
        assert!(rdesk_proto::release::PLATFORMS.contains(&p.as_str()) || p.ends_with("x86"), "{p}");
    }

    #[test]
    fn built_in_keys() {
        let keys = trusted_keys();
        assert_eq!(keys.len(), RELEASE_KEYS.len());
        assert!(keys.iter().all(|k| iroh::PublicKey::from_bytes(k).is_ok()));
    }
}
