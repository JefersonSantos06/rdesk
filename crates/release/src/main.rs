//! `rdesk-release`: the signing side of rdesk releases (see `docs/RELEASE.md`).
//!
//! * `keygen`   creates a release key pair (once; the private key goes to GitHub Secrets
//!   and to the password manager, the public key into `rdesk_proto::release::RELEASE_KEYS`).
//! * `manifest` writes `release.json` for the files CI built.
//! * `sign`     writes `release.json.sig` with the key from an environment variable.
//! * `verify`   checks the signature against the built-in keys and every file's hash.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rdesk_proto::release::{
    MANIFEST_NAME, RELEASE_KEYS, ReleaseFile, ReleaseManifest, SIGNATURE_NAME, classify, parse_version, signed_message,
};
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(name = "rdesk-release", about = "Build, sign and check rdesk release manifests")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a key pair: `<dir>/rdesk-release-<name>.key` (private, hex) and `.pub`.
    Keygen {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "main")]
        name: String,
    },
    /// Write `<dir>/release.json` listing every release file in `<dir>`.
    Manifest {
        #[arg(long)]
        version: String,
        #[arg(long)]
        dir: PathBuf,
    },
    /// Sign `<dir>/release.json` into `<dir>/release.json.sig`.
    Sign {
        #[arg(long)]
        dir: PathBuf,
        /// Environment variable holding the private key (hex).
        #[arg(long, default_value = "RDESK_RELEASE_KEY")]
        key_env: String,
    },
    /// Check the signature (against the built-in release keys) and the files' hashes.
    Verify {
        #[arg(long)]
        dir: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Keygen { out, name } => keygen(&out, &name),
        Cmd::Manifest { version, dir } => {
            let m = manifest(&version, &dir, now())?;
            let json = serde_json::to_vec_pretty(&m)?;
            std::fs::write(dir.join(MANIFEST_NAME), json)?;
            println!("{MANIFEST_NAME}: {} files for {}", m.files.len(), m.version);
            Ok(())
        }
        Cmd::Sign { dir, key_env } => {
            let hex_key = std::env::var(&key_env).with_context(|| format!("{key_env} is not set"))?;
            let key = signing_key(hex_key.trim())?;
            let sig = sign(&key, &std::fs::read(dir.join(MANIFEST_NAME))?);
            std::fs::write(dir.join(SIGNATURE_NAME), format!("{}\n", hex::encode(sig.to_bytes())))?;
            println!("signed with {}", hex::encode(key.verifying_key().as_bytes()));
            Ok(())
        }
        Cmd::Verify { dir } => {
            let m = verify(&dir, &trusted_keys()?)?;
            println!("{} ok: {} files, signature and hashes match", m.version, m.files.len());
            Ok(())
        }
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn keygen(out: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let (secret, public) =
        (out.join(format!("rdesk-release-{name}.key")), out.join(format!("rdesk-release-{name}.pub")));
    ensure!(!secret.exists(), "{} already exists", secret.display());
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&secret)?, format!("{}\n", hex::encode(key.to_bytes())).as_bytes())?;
    let public_hex = hex::encode(key.verifying_key().as_bytes());
    std::fs::write(&public, format!("{public_hex}\n"))?;
    println!("private key: {} (never commit it)", secret.display());
    println!("public key:  {public_hex}");
    Ok(())
}

fn signing_key(hex_key: &str) -> Result<SigningKey> {
    let bytes: [u8; 32] = hex::decode(hex_key)
        .context("the release key is not hex")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the release key must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn trusted_keys() -> Result<Vec<VerifyingKey>> {
    RELEASE_KEYS
        .iter()
        .map(|k| {
            let bytes: [u8; 32] = hex::decode(k)?.try_into().map_err(|_| anyhow::anyhow!("bad key length"))?;
            Ok(VerifyingKey::from_bytes(&bytes)?)
        })
        .collect()
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let size = std::io::copy(&mut f, &mut h)?;
    Ok((hex::encode(h.finalize()), size))
}

fn manifest(version: &str, dir: &Path, published_at: i64) -> Result<ReleaseManifest> {
    ensure!(parse_version(version).is_some(), "version {version:?} is not MAJOR.MINOR.PATCH");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if !path.is_file() || name == MANIFEST_NAME || name == SIGNATURE_NAME {
            continue;
        }
        let Some((kind, platform)) = classify(&name, version) else {
            bail!("{name}: not a release file name for {version}");
        };
        let (sha256, size) = sha256_file(&path)?;
        files.push(ReleaseFile { name, kind, platform: platform.into(), sha256, size });
    }
    ensure!(!files.is_empty(), "no release files in {}", dir.display());
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ReleaseManifest { version: version.into(), protocol: rdesk_proto::PROTOCOL_VERSION, published_at, files })
}

fn sign(key: &SigningKey, manifest_json: &[u8]) -> Signature {
    key.sign(&signed_message(manifest_json))
}

fn verify(dir: &Path, keys: &[VerifyingKey]) -> Result<ReleaseManifest> {
    let json = std::fs::read(dir.join(MANIFEST_NAME))?;
    let sig_hex = std::fs::read_to_string(dir.join(SIGNATURE_NAME))?;
    let sig: [u8; 64] = hex::decode(sig_hex.trim())?.try_into().map_err(|_| anyhow::anyhow!("bad signature length"))?;
    let sig = Signature::from_bytes(&sig);
    let msg = signed_message(&json);
    ensure!(keys.iter().any(|k| k.verify(&msg, &sig).is_ok()), "the signature is not from a release key");
    let m: ReleaseManifest = serde_json::from_slice(&json)?;
    for f in &m.files {
        let (sha256, size) = sha256_file(&dir.join(&f.name))?;
        ensure!(sha256 == f.sha256 && size == f.size, "{}: hash or size differs from the manifest", f.name);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_sign_verify() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("rdesk-1.2.3-windows-x86_64.zip"), b"zip").unwrap();
        std::fs::write(p.join("rdesk_1.2.3_arm64.deb"), b"deb").unwrap();
        std::fs::write(p.join("rdesk-server-1.2.3-linux-x86_64.tar.gz"), b"tgz").unwrap();

        let m = manifest("1.2.3", p, 1).unwrap();
        assert_eq!(m.files.len(), 3);
        std::fs::write(p.join(MANIFEST_NAME), serde_json::to_vec_pretty(&m).unwrap()).unwrap();

        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let other = SigningKey::generate(&mut rand::rngs::OsRng);
        let sig = sign(&key, &std::fs::read(p.join(MANIFEST_NAME)).unwrap());
        std::fs::write(p.join(SIGNATURE_NAME), hex::encode(sig.to_bytes())).unwrap();

        assert_eq!(verify(p, &[other.verifying_key(), key.verifying_key()]).unwrap(), m);
        assert!(verify(p, &[other.verifying_key()]).is_err(), "unknown key");

        std::fs::write(p.join("rdesk_1.2.3_arm64.deb"), b"evil").unwrap();
        assert!(verify(p, &[key.verifying_key()]).is_err(), "altered file");
    }

    #[test]
    fn rejects_stray_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rdesk-1.0.0-linux-x86_64.tar.gz"), b"x").unwrap();
        std::fs::write(dir.path().join("rdesk-0.9.0-linux-x86_64.tar.gz"), b"x").unwrap();
        assert!(manifest("1.0.0", dir.path(), 0).is_err());
        assert!(manifest("1.0", dir.path(), 0).is_err());
    }

    #[test]
    fn built_in_keys_parse() {
        assert_eq!(trusted_keys().unwrap().len(), 2);
    }
}
