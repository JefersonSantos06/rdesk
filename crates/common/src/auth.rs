//! The authentication handshake on the control stream.
//!
//! Every method is bound to the QUIC/TLS session through the TLS exporter value
//! `ek` (see [`crate::transport::exporter`]) plus a fresh host nonce, so a
//! signature or PAKE transcript captured on one connection is useless on any
//! other. Methods:
//!
//! * **PublicKey** – the client asks its SSH agent (1Password) for an SSHSIG over
//!   `{ek, nonce, host cert fingerprint, public key, client name}` in the
//!   `rdesk-auth-v1@rdesk` namespace. The host checks `authorized_keys`.
//! * **Password** – OPAQUE (opaque-ke, Ristretto255 + Argon2id). The password
//!   never leaves the client; the host stores only an OPAQUE registration record.
//! * **Resume** – after a successful login the host issues a short-lived token so
//!   a dropped connection can reconnect without another 1Password prompt.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration, ClientRegistrationFinishParameters,
    CredentialFinalization, CredentialRequest, CredentialResponse, Identifiers, ServerLogin, ServerLoginParameters,
    ServerRegistration, ServerSetup,
};
use rand::RngCore;
use rand::rngs::OsRng;
use rdesk_proto::{
    AuthMethod, AuthOk, ClientMsg, HostInfo, HostMsg, MAX_CONTROL_MSG, PROTOCOL_VERSION, SessionId, expect_msg,
    write_msg,
};
use serde::Serialize;
use sha2::Sha256;
use ssh_key::public::KeyData;
use ssh_key::{HashAlg, PrivateKey, PublicKey, SshSig};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroizing;

use crate::agent::{Agent, canonical, key_fingerprint};

pub const SSHSIG_NAMESPACE: &str = "rdesk-auth-v1@rdesk";
pub const OPAQUE_CREDENTIAL_ID: &[u8] = b"rdesk-user";
pub const RESUME_TTL: Duration = Duration::from_secs(600);
const MAX_AUTH_MESSAGES: usize = 24;

// ---------------------------------------------------------------------------
// OPAQUE configuration
// ---------------------------------------------------------------------------

pub struct Suite;

impl opaque_ke::CipherSuite for Suite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, sha2::Sha512>;
    type Ksf = argon2::Argon2<'static>;
}

/// Argon2id, 64 MiB, 3 passes. Runs on the client at login and on the host
/// only during `set-password`; it is what an attacker who steals the host's
/// `password.opaque` would have to grind through per guess.
pub fn ksf() -> argon2::Argon2<'static> {
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(64 * 1024, 3, 1, None).expect("valid argon2 params"),
    )
}

pub fn new_server_setup() -> ServerSetup<Suite> {
    ServerSetup::<Suite>::new(&mut OsRng)
}

pub fn server_setup_from_bytes(b: &[u8]) -> Result<ServerSetup<Suite>> {
    ServerSetup::<Suite>::deserialize(b).map_err(|e| anyhow!("invalid OPAQUE server setup: {e:?}"))
}

pub fn server_setup_to_bytes(s: &ServerSetup<Suite>) -> Vec<u8> {
    s.serialize().to_vec()
}

/// Run both halves of OPAQUE registration locally (used by `rdesk host set-password`).
pub fn register_password(setup: &ServerSetup<Suite>, password: &[u8]) -> Result<Vec<u8>> {
    let mut rng = OsRng;
    let start = ClientRegistration::<Suite>::start(&mut rng, password).map_err(|e| anyhow!("{e:?}"))?;
    let resp =
        ServerRegistration::<Suite>::start(setup, start.message, OPAQUE_CREDENTIAL_ID).map_err(|e| anyhow!("{e:?}"))?;
    let ksf = ksf();
    let fin = start
        .state
        .finish(
            &mut rng,
            password,
            resp.message,
            ClientRegistrationFinishParameters::new(Identifiers::default(), Some(&ksf)),
        )
        .map_err(|e| anyhow!("{e:?}"))?;
    Ok(ServerRegistration::<Suite>::finish(fin.message).serialize().to_vec())
}

pub struct OpaqueServer {
    pub setup: ServerSetup<Suite>,
    pub record: ServerRegistration<Suite>,
}

impl OpaqueServer {
    pub fn from_bytes(setup: &[u8], record: &[u8]) -> Result<Self> {
        Ok(Self {
            setup: server_setup_from_bytes(setup)?,
            record: ServerRegistration::<Suite>::deserialize(record)
                .map_err(|e| anyhow!("invalid password record: {e:?}"))?,
        })
    }
}

fn opaque_context(ek: &[u8; 32], nonce: &[u8; 32], host_fp: &[u8; 32]) -> Vec<u8> {
    let mut ctx = b"rdesk-opaque-v1".to_vec();
    ctx.extend_from_slice(ek);
    ctx.extend_from_slice(nonce);
    ctx.extend_from_slice(host_fp);
    ctx
}

// ---------------------------------------------------------------------------
// Public-key payload
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PubkeyPayload<'a> {
    domain: &'static str,
    ek: &'a [u8; 32],
    nonce: &'a [u8; 32],
    host_fp: &'a [u8; 32],
    public_key: &'a str,
    client_name: &'a str,
}

fn pubkey_payload(ek: &[u8; 32], nonce: &[u8; 32], host_fp: &[u8; 32], key: &str, client: &str) -> Vec<u8> {
    postcard::to_stdvec(&PubkeyPayload {
        domain: SSHSIG_NAMESPACE,
        ek,
        nonce,
        host_fp,
        public_key: key,
        client_name: client,
    })
    .expect("postcard serialization of plain struct cannot fail")
}

// ---------------------------------------------------------------------------
// Resume tokens
// ---------------------------------------------------------------------------

fn resume_mac(token: &[u8; 32], sid: &SessionId, ek: &[u8; 32], nonce: &[u8; 32]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(b"rdesk-resume-v1");
    mac.update(sid);
    mac.update(ek);
    mac.update(nonce);
    mac.finalize().into_bytes().into()
}

type ResumeEntry = (Zeroizing<[u8; 32]>, Instant);

/// Host-side store of resumable sessions (memory only: a host restart forces a full login).
#[derive(Default)]
pub struct ResumeStore {
    inner: Mutex<HashMap<SessionId, ResumeEntry>>,
}

impl ResumeStore {
    fn issue(&self, sid: SessionId) -> [u8; 32] {
        let mut token = [0u8; 32];
        OsRng.fill_bytes(&mut token);
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, (_, exp)| *exp > now);
        map.insert(sid, (Zeroizing::new(token), now + RESUME_TTL));
        token
    }

    /// Check a resume MAC. The token is consumed either way (single use).
    fn redeem(&self, sid: &SessionId, mac: &[u8; 32], ek: &[u8; 32], nonce: &[u8; 32]) -> bool {
        let mut map = self.inner.lock().unwrap();
        let Some((token, exp)) = map.remove(sid) else {
            return false;
        };
        if exp < Instant::now() {
            return false;
        }
        let expected = resume_mac(&token, sid, ek, nonce);
        expected.ct_eq(mac).into()
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

// ---------------------------------------------------------------------------
// Brute-force limiter
// ---------------------------------------------------------------------------

/// Exponential backoff per source IP after failed logins.
#[derive(Default)]
pub struct FailureLimiter {
    inner: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl FailureLimiter {
    /// `Err(wait)` if this IP must wait before trying again.
    pub fn check(&self, ip: IpAddr) -> Result<(), Duration> {
        let map = self.inner.lock().unwrap();
        if let Some(&(fails, last)) = map.get(&ip)
            && fails >= 3
        {
            let wait = Duration::from_secs(1u64 << (fails - 3).min(8)); // 1s .. 256s
            let elapsed = last.elapsed();
            if elapsed < wait {
                return Err(wait - elapsed);
            }
        }
        Ok(())
    }

    pub fn failure(&self, ip: IpAddr) {
        let mut map = self.inner.lock().unwrap();
        let e = map.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() > Duration::from_secs(3600) {
            e.0 = 0;
        }
        e.0 += 1;
        e.1 = Instant::now();
    }

    pub fn success(&self, ip: IpAddr) {
        self.inner.lock().unwrap().remove(&ip);
    }
}

// ---------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------

pub struct HostAuthConfig {
    pub authorized_keys: Vec<KeyData>,
    pub opaque: Option<OpaqueServer>,
    pub host_fp: [u8; 32],
    pub host_name: String,
}

/// Parse an OpenSSH `authorized_keys` file (options are not supported and rejected).
pub fn parse_authorized_keys(text: &str) -> Result<Vec<PublicKey>> {
    let mut keys = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = PublicKey::from_openssh(line).with_context(|| format!("authorized_keys line {}", i + 1))?;
        keys.push(key);
    }
    Ok(keys)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthKind {
    PublicKey { fingerprint: String },
    Password,
    Resume,
}

#[derive(Debug, Clone)]
pub struct Authenticated {
    pub session_id: SessionId,
    pub kind: AuthKind,
    pub client_name: String,
}

#[derive(Debug, thiserror::Error)]
#[error("authentication failed: {0}")]
pub struct AuthFailed(pub String);

async fn fail<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, reason: &str) -> anyhow::Error {
    let _ = write_msg(w, &HostMsg::AuthFail { reason: reason.to_string() }).await;
    AuthFailed(reason.to_string()).into()
}

/// Run the host side of the handshake. On success `AuthOk` has been sent.
/// The caller should wrap this in a timeout.
pub async fn host_handshake<R, W>(
    cfg: &HostAuthConfig,
    store: &ResumeStore,
    r: &mut R,
    w: &mut W,
    ek: [u8; 32],
    host_info: impl FnOnce() -> HostInfo,
) -> Result<Authenticated>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let ClientMsg::Hello { proto, client_name, .. } = expect_msg(r, MAX_CONTROL_MSG).await? else {
        return Err(fail(w, "expected Hello").await);
    };
    if proto != PROTOCOL_VERSION {
        return Err(fail(w, &format!("protocol version {proto} not supported (host speaks {PROTOCOL_VERSION})")).await);
    }
    let client_name: String = client_name.chars().take(64).collect();

    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    let mut methods = vec![AuthMethod::Resume, AuthMethod::PublicKey];
    if cfg.opaque.is_some() {
        methods.push(AuthMethod::Password);
    }
    write_msg(
        w,
        &HostMsg::Hello { proto: PROTOCOL_VERSION, host_name: cfg.host_name.clone(), server_nonce: nonce, methods },
    )
    .await?;

    let authorized = |k: &KeyData| cfg.authorized_keys.iter().any(|a| a == k);

    for _ in 0..MAX_AUTH_MESSAGES {
        let msg: ClientMsg = expect_msg(r, MAX_CONTROL_MSG).await?;
        let (session_id, kind) = match msg {
            ClientMsg::Resume { session_id, mac } => {
                if store.redeem(&session_id, &mac, &ek, &nonce) {
                    (session_id, AuthKind::Resume)
                } else {
                    write_msg(w, &HostMsg::ResumeRejected).await?;
                    continue;
                }
            }
            ClientMsg::PubkeyQuery { public_key } => {
                let ok = PublicKey::from_openssh(&public_key).is_ok_and(|k| authorized(k.key_data()));
                write_msg(w, if ok { &HostMsg::PubkeyOk } else { &HostMsg::PubkeyRejected }).await?;
                continue;
            }
            ClientMsg::PubkeyAuth { public_key, sshsig } => {
                let Ok(key) = PublicKey::from_openssh(&public_key) else {
                    return Err(fail(w, "malformed public key").await);
                };
                if !authorized(key.key_data()) {
                    return Err(fail(w, "key not authorized").await);
                }
                let Ok(sig) = SshSig::from_pem(sshsig.as_bytes()) else {
                    return Err(fail(w, "malformed signature").await);
                };
                let canon = canonical(key.key_data())?;
                let payload = pubkey_payload(&ek, &nonce, &cfg.host_fp, &canon, &client_name);
                if sig.public_key() != key.key_data() || key.verify(SSHSIG_NAMESPACE, &payload, &sig).is_err() {
                    return Err(fail(w, "bad signature").await);
                }
                (new_session_id(), AuthKind::PublicKey { fingerprint: key_fingerprint(key.key_data()) })
            }
            ClientMsg::PasswordStart { ke1 } => {
                let Some(op) = &cfg.opaque else {
                    return Err(fail(w, "password login is not enabled on this host").await);
                };
                let Ok(req) = CredentialRequest::<Suite>::deserialize(&ke1) else {
                    return Err(fail(w, "malformed KE1").await);
                };
                let ctx = opaque_context(&ek, &nonce, &cfg.host_fp);
                let params = ServerLoginParameters { context: Some(&ctx), identifiers: Identifiers::default() };
                let start = ServerLogin::start(
                    &mut OsRng,
                    &op.setup,
                    Some(op.record.clone()),
                    req,
                    OPAQUE_CREDENTIAL_ID,
                    params.clone(),
                )
                .map_err(|e| anyhow!("OPAQUE: {e:?}"))?;
                write_msg(w, &HostMsg::PasswordKe2 { ke2: start.message.serialize().to_vec() }).await?;
                let ClientMsg::PasswordFinish { ke3 } = expect_msg(r, MAX_CONTROL_MSG).await? else {
                    return Err(fail(w, "expected KE3").await);
                };
                let Ok(fin) = CredentialFinalization::<Suite>::deserialize(&ke3) else {
                    return Err(fail(w, "malformed KE3").await);
                };
                if start.state.finish(fin, params).is_err() {
                    return Err(fail(w, "wrong password").await);
                }
                (new_session_id(), AuthKind::Password)
            }
            _ => return Err(fail(w, "unexpected message during authentication").await),
        };

        let resume_token = store.issue(session_id);
        write_msg(
            w,
            &HostMsg::AuthOk(AuthOk {
                session_id,
                resume_token,
                resume_ttl_s: RESUME_TTL.as_secs() as u32,
                host: host_info(),
            }),
        )
        .await?;
        return Ok(Authenticated { session_id, kind, client_name });
    }
    Err(fail(w, "too many authentication attempts").await)
}

fn new_session_id() -> SessionId {
    let mut sid = [0u8; 16];
    OsRng.fill_bytes(&mut sid);
    sid
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

pub enum KeySource {
    Agent(Agent),
    /// In-memory keys (tests, or a key file).
    Local(Vec<PrivateKey>),
    /// RSA signed the way an SSH agent does it (rsa-sha2-512). ssh-key 0.6's own
    /// RSA signer is broken, so tests use this to exercise the host's RSA verification.
    #[cfg(test)]
    RsaRaw(Box<rsa::RsaPrivateKey>),
}

impl KeySource {
    async fn identities(&mut self) -> Result<Vec<KeyData>> {
        match self {
            KeySource::Agent(a) => Ok(a.identities().await?.into_iter().map(|k| k.key_data().clone()).collect()),
            KeySource::Local(keys) => Ok(keys.iter().map(|k| k.public_key().key_data().clone()).collect()),
            #[cfg(test)]
            KeySource::RsaRaw(k) => Ok(vec![KeyData::Rsa(ssh_key::public::RsaPublicKey::try_from(k.to_public_key())?)]),
        }
    }

    async fn sign(&mut self, key: &KeyData, msg: &[u8]) -> Result<SshSig> {
        match self {
            KeySource::Agent(a) => a.sign_sshsig(key, SSHSIG_NAMESPACE, msg).await,
            KeySource::Local(keys) => {
                let k = keys.iter().find(|k| k.public_key().key_data() == key).context("unknown local key")?;
                Ok(SshSig::sign(k, SSHSIG_NAMESPACE, HashAlg::Sha512, msg)?)
            }
            #[cfg(test)]
            KeySource::RsaRaw(k) => {
                use rsa::signature::{SignatureEncoding, Signer};
                let data = SshSig::signed_data(SSHSIG_NAMESPACE, HashAlg::Sha512, msg)?;
                let sig = rsa::pkcs1v15::SigningKey::<sha2::Sha512>::new((**k).clone()).sign(&data);
                let sig =
                    ssh_key::Signature::new(ssh_key::Algorithm::Rsa { hash: Some(HashAlg::Sha512) }, sig.to_vec())?;
                Ok(SshSig::new(key.clone(), SSHSIG_NAMESPACE, HashAlg::Sha512, sig)?)
            }
        }
    }
}

/// Produces the password when (and only when) the handshake needs it. Gets the host name.
pub type AskPassword = std::sync::Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Zeroizing<String>>> + Send>>
        + Send
        + Sync,
>;

pub enum PasswordSource {
    Fixed(Zeroizing<String>),
    /// 1Password secret reference, read with `op read`.
    OnePassword(String),
    /// Ask on the terminal.
    Prompt,
    /// Anything else: a dialog, the system keychain…
    Ask(AskPassword),
}

/// The user closed the password dialog.
#[derive(Debug, thiserror::Error)]
#[error("login cancelled")]
pub struct PasswordCancelled;

impl PasswordSource {
    async fn get(&self, host: &str) -> Result<Zeroizing<String>> {
        match self {
            PasswordSource::Fixed(p) => Ok(p.clone()),
            PasswordSource::OnePassword(r) => crate::agent::op_read(r).await,
            PasswordSource::Prompt => {
                let prompt = format!("rdesk password for {host}: ");
                let p = tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt)).await??;
                Ok(Zeroizing::new(p))
            }
            PasswordSource::Ask(f) => f(host.to_string()).await,
        }
    }
}

pub struct ClientAuth {
    pub client_name: String,
    pub resume: Option<(SessionId, [u8; 32])>,
    pub keys: Option<KeySource>,
    /// Only use the agent key with this fingerprint (`SHA256:...`).
    pub key_filter: Option<String>,
    pub password: Option<PasswordSource>,
}

/// Run the client side of the handshake, trying resume → public key → password.
pub async fn client_handshake<R, W>(
    r: &mut R,
    w: &mut W,
    ek: [u8; 32],
    host_fp: [u8; 32],
    auth: &mut ClientAuth,
) -> Result<AuthOk>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    write_msg(
        w,
        &ClientMsg::Hello {
            proto: PROTOCOL_VERSION,
            client_name: auth.client_name.clone(),
            methods: vec![AuthMethod::Resume, AuthMethod::PublicKey, AuthMethod::Password],
        },
    )
    .await?;
    let (host_name, nonce, methods) = match expect_msg(r, MAX_CONTROL_MSG).await? {
        HostMsg::Hello { proto, host_name, server_nonce, methods } => {
            if proto != PROTOCOL_VERSION {
                bail!("host speaks protocol {proto}, we speak {PROTOCOL_VERSION}; update rdesk on both sides");
            }
            (host_name, server_nonce, methods)
        }
        HostMsg::AuthFail { reason } => bail!(AuthFailed(reason)),
        other => bail!("unexpected host message {other:?}"),
    };

    // 1. Resume
    if let Some((sid, token)) = auth.resume.take()
        && methods.contains(&AuthMethod::Resume)
    {
        let mac = resume_mac(&token, &sid, &ek, &nonce);
        write_msg(w, &ClientMsg::Resume { session_id: sid, mac }).await?;
        match expect_msg(r, MAX_CONTROL_MSG).await? {
            HostMsg::AuthOk(ok) => return Ok(ok),
            HostMsg::ResumeRejected => tracing::debug!("resume rejected, falling back"),
            HostMsg::AuthFail { reason } => bail!(AuthFailed(reason)),
            other => bail!("unexpected host message {other:?}"),
        }
    }

    // 2. Public key
    let mut key_errors = Vec::new();
    if let Some(keys) = auth.keys.as_mut()
        && methods.contains(&AuthMethod::PublicKey)
    {
        match keys.identities().await {
            Ok(ids) => {
                let ids: Vec<_> = ids
                    .into_iter()
                    .filter(|k| auth.key_filter.as_deref().is_none_or(|f| key_fingerprint(k) == f))
                    .collect();
                if ids.is_empty() {
                    key_errors.push("no matching keys in the SSH agent".to_string());
                }
                for key in ids {
                    let canon = canonical(&key)?;
                    write_msg(w, &ClientMsg::PubkeyQuery { public_key: canon.clone() }).await?;
                    match expect_msg(r, MAX_CONTROL_MSG).await? {
                        HostMsg::PubkeyOk => {}
                        HostMsg::PubkeyRejected => {
                            key_errors.push(format!("{} not authorized on host", key_fingerprint(&key)));
                            continue;
                        }
                        HostMsg::AuthFail { reason } => bail!(AuthFailed(reason)),
                        other => bail!("unexpected host message {other:?}"),
                    }
                    let payload = pubkey_payload(&ek, &nonce, &host_fp, &canon, &auth.client_name);
                    let sig = keys.sign(&key, &payload).await?;
                    let sshsig = sig.to_pem(ssh_key::LineEnding::LF)?;
                    write_msg(w, &ClientMsg::PubkeyAuth { public_key: canon, sshsig }).await?;
                    return match expect_msg(r, MAX_CONTROL_MSG).await? {
                        HostMsg::AuthOk(ok) => Ok(ok),
                        HostMsg::AuthFail { reason } => Err(AuthFailed(reason).into()),
                        other => Err(anyhow!("unexpected host message {other:?}")),
                    };
                }
            }
            Err(e) => key_errors.push(format!("SSH agent: {e:#}")),
        }
    }

    // 3. Password
    if let Some(src) = auth.password.as_ref()
        && methods.contains(&AuthMethod::Password)
    {
        let password = src.get(&host_name).await?;
        let mut rng = OsRng;
        let start = ClientLogin::<Suite>::start(&mut rng, password.as_bytes()).map_err(|e| anyhow!("{e:?}"))?;
        write_msg(w, &ClientMsg::PasswordStart { ke1: start.message.serialize().to_vec() }).await?;
        let ke2 = match expect_msg(r, MAX_CONTROL_MSG).await? {
            HostMsg::PasswordKe2 { ke2 } => ke2,
            HostMsg::AuthFail { reason } => bail!(AuthFailed(reason)),
            other => bail!("unexpected host message {other:?}"),
        };
        let resp = CredentialResponse::<Suite>::deserialize(&ke2).map_err(|e| anyhow!("malformed KE2: {e:?}"))?;
        let ctx = opaque_context(&ek, &nonce, &host_fp);
        // Argon2 with 64 MiB takes a few hundred ms: keep it off the async workers.
        let state = start.state;
        let finish = tokio::task::spawn_blocking(move || {
            let ksf = ksf();
            state.finish(
                &mut OsRng,
                password.as_bytes(),
                resp,
                ClientLoginFinishParameters::new(Some(&ctx), Identifiers::default(), Some(&ksf)),
            )
        })
        .await?;
        let Ok(finish) = finish else {
            bail!(AuthFailed("wrong password".into()));
        };
        write_msg(w, &ClientMsg::PasswordFinish { ke3: finish.message.serialize().to_vec() }).await?;
        return match expect_msg(r, MAX_CONTROL_MSG).await? {
            HostMsg::AuthOk(ok) => Ok(ok),
            HostMsg::AuthFail { reason } => Err(AuthFailed(reason).into()),
            other => Err(anyhow!("unexpected host message {other:?}")),
        };
    }

    if key_errors.is_empty() {
        bail!("no usable authentication method (host offers {methods:?})");
    }
    bail!("no usable authentication method: {}", key_errors.join("; "))
}

#[cfg(test)]
mod tests;
