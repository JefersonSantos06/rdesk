//! SSH agent client (1Password's agent, or any OpenSSH-compatible agent) and
//! the 1Password CLI helper for secrets.

use anyhow::{Context, Result, anyhow, bail};
use ssh_agent_lib::agent::Session;
use ssh_agent_lib::proto::SignRequest;
use ssh_key::public::KeyData;
use ssh_key::{HashAlg, PublicKey, SshSig};
use zeroize::Zeroizing;

/// `SSH_AGENT_RSA_SHA2_512` flag: ask for rsa-sha2-512 instead of legacy SHA-1.
const SSH_AGENT_RSA_SHA2_512: u32 = 4;

#[cfg(windows)]
pub const WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// 1Password's documented agent socket for this OS, if it exists.
pub fn onepassword_socket() -> Option<String> {
    #[cfg(unix)]
    {
        let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
        let candidates = if cfg!(target_os = "macos") {
            vec![home.join("Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock")]
        } else {
            vec![home.join(".1password/agent.sock")]
        };
        candidates.into_iter().find(|p| p.exists()).map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(windows)]
    {
        // 1Password on Windows serves the standard OpenSSH agent pipe.
        Some(WINDOWS_AGENT_PIPE.to_string())
    }
}

/// Where to find the agent: explicit override, then 1Password, then `SSH_AUTH_SOCK`.
pub fn agent_path(override_path: Option<&str>) -> Result<String> {
    if let Some(p) = override_path.filter(|p| !p.is_empty()) {
        return Ok(p.to_string());
    }
    if let Some(p) = onepassword_socket() {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("SSH_AUTH_SOCK")
        && !p.is_empty()
    {
        return Ok(p);
    }
    bail!(
        "no SSH agent found. Enable the 1Password SSH agent (Settings → Developer) \
         or set SSH_AUTH_SOCK / `agent_socket`"
    )
}

pub struct Agent {
    session: Box<dyn Session>,
    pub path: String,
}

impl Agent {
    pub async fn connect(override_path: Option<&str>) -> Result<Self> {
        let path = agent_path(override_path)?;
        let session = open(&path).await.with_context(|| format!("connecting to SSH agent at {path}"))?;
        Ok(Self { session, path })
    }

    pub async fn identities(&mut self) -> Result<Vec<PublicKey>> {
        let ids = self.session.request_identities().await.map_err(|e| anyhow!("agent: {e}"))?;
        Ok(ids.into_iter().map(|id| PublicKey::new(id.credential.key_data().clone(), id.comment)).collect())
    }

    /// Produce an SSHSIG over `msg` using the agent. 1Password may show an
    /// approval / Touch ID prompt here.
    pub async fn sign_sshsig(&mut self, key: &KeyData, namespace: &str, msg: &[u8]) -> Result<SshSig> {
        let data = SshSig::signed_data(namespace, HashAlg::Sha512, msg)?;
        let flags = if matches!(key, KeyData::Rsa(_)) { SSH_AGENT_RSA_SHA2_512 } else { 0 };
        let sig = self
            .session
            .sign(SignRequest { credential: key.clone().into(), data, flags })
            .await
            .map_err(|e| anyhow!("agent refused to sign: {e}"))?;
        Ok(SshSig::new(key.clone(), namespace, HashAlg::Sha512, sig)?)
    }
}

#[cfg(unix)]
async fn open(path: &str) -> Result<Box<dyn Session>> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    Ok(Box::new(ssh_agent_lib::client::Client::new(stream)))
}

#[cfg(windows)]
async fn open(path: &str) -> Result<Box<dyn Session>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut tries = 0;
    let pipe = loop {
        match ClientOptions::new().open(path) {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 40 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };
    Ok(Box::new(ssh_agent_lib::client::Client::new(pipe)))
}

/// SHA256 fingerprint in OpenSSH's format (`SHA256:base64`).
pub fn key_fingerprint(key: &KeyData) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Canonical one-line OpenSSH encoding without comment.
pub fn canonical(key: &KeyData) -> Result<String> {
    Ok(PublicKey::new(key.clone(), "").to_openssh()?.trim().to_string())
}

/// Read a secret with the 1Password CLI (`op read op://vault/item/field`).
pub async fn op_read(reference: &str) -> Result<Zeroizing<String>> {
    let out = tokio::process::Command::new("op")
        .arg("read")
        .arg("--no-newline")
        .arg(reference)
        .stdin(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("running the 1Password CLI `op` (is it installed and in PATH?)")?;
    if !out.status.success() {
        bail!("`op read {reference}` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let s = Zeroizing::new(String::from_utf8(out.stdout).context("secret is not UTF-8")?);
    Ok(s)
}
