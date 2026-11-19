use super::*;
use rdesk_proto::HostInfo;
use ssh_key::Algorithm;
use tokio::io::AsyncWriteExt;

const HOST_FP: [u8; 32] = [7; 32];
/// The client's endpoint ID as the host sees it.
const PEER: [u8; 32] = [5; 32];

fn host_info() -> HostInfo {
    HostInfo { hostname: "test".into(), os: "test".into(), monitors: vec![], active_monitor: 0 }
}

fn ed25519() -> PrivateKey {
    PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
}

fn host_cfg(keys: &[&PrivateKey], password: Option<&str>) -> HostAuthConfig {
    let opaque = password.map(|p| {
        let setup = new_server_setup();
        let record = register_password(&setup, p.as_bytes()).unwrap();
        OpaqueServer::from_bytes(&server_setup_to_bytes(&setup), &record).unwrap()
    });
    HostAuthConfig {
        authorized_keys: keys.iter().map(|k| k.public_key().key_data().clone()).collect(),
        opaque,
        host_fp: HOST_FP,
        host_name: "host".into(),
        ticket: None,
        require_local: false,
        direct: false,
    }
}

fn client(keys: Vec<PrivateKey>, password: Option<&str>) -> ClientAuth {
    ClientAuth {
        client_name: "client".into(),
        resume: None,
        ticket: None,
        keys: if keys.is_empty() { None } else { Some(KeySource::Local(keys)) },
        key_filter: None,
        password: password.map(|p| PasswordSource::Fixed(Zeroizing::new(p.to_string()))),
    }
}

async fn run(
    cfg: &HostAuthConfig,
    store: &ResumeStore,
    host_ek: [u8; 32],
    client_ek: [u8; 32],
    client_fp: [u8; 32],
    auth: &mut ClientAuth,
) -> (Result<Authenticated>, Result<AuthOk>) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);
    let host = async {
        let r = host_handshake(cfg, store, &mut ar, &mut aw, host_ek, PEER, host_info).await;
        let _ = aw.shutdown().await; // EOF so a waiting client stops
        r
    };
    let client = async {
        let r = client_handshake(&mut br, &mut bw, client_ek, client_fp, auth).await;
        let _ = bw.shutdown().await;
        r
    };
    tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(host, client) }).await.expect("handshake hung")
}

#[tokio::test]
async fn pubkey_ed25519_ok() {
    let key = ed25519();
    let cfg = host_cfg(&[&key], None);
    let store = ResumeStore::default();
    let mut auth = client(vec![key.clone()], None);
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    let h = h.unwrap();
    let c = c.unwrap();
    assert_eq!(h.session_id, c.session_id);
    assert!(matches!(h.kind, AuthKind::PublicKey { .. }));
    assert_eq!(h.client_name, "client");
}

#[tokio::test]
async fn pubkey_rsa_sha512_ok() {
    let rsa_key = rsa::RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
    let public = KeyData::Rsa(ssh_key::public::RsaPublicKey::try_from(rsa_key.to_public_key()).unwrap());
    let mut cfg = host_cfg(&[], None);
    cfg.authorized_keys.push(public);
    let store = ResumeStore::default();
    let mut auth = client(vec![], None);
    auth.keys = Some(KeySource::RsaRaw(Box::new(rsa_key)));
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    c.unwrap();
    h.unwrap();
}

#[tokio::test]
async fn picks_authorized_key_among_several() {
    let good = ed25519();
    let other = ed25519();
    let cfg = host_cfg(&[&good], None);
    let store = ResumeStore::default();
    let mut auth = client(vec![other, good], None);
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    h.unwrap();
    c.unwrap();
}

#[tokio::test]
async fn unauthorized_key_rejected() {
    let good = ed25519();
    let cfg = host_cfg(&[&good], None);
    let store = ResumeStore::default();
    let mut auth = client(vec![ed25519()], None);
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    assert!(h.is_err());
    let e = c.unwrap_err().to_string();
    assert!(e.contains("not authorized"), "{e}");
}

#[tokio::test]
async fn channel_binding_mismatch_fails() {
    // Simulates a MITM relaying between two different TLS sessions.
    let key = ed25519();
    let cfg = host_cfg(&[&key], None);
    let store = ResumeStore::default();
    let mut auth = client(vec![key], None);
    let (h, c) = run(&cfg, &store, [1; 32], [2; 32], HOST_FP, &mut auth).await;
    assert!(h.unwrap_err().to_string().contains("bad signature"));
    assert!(c.is_err());
}

#[tokio::test]
async fn host_fingerprint_mismatch_fails() {
    let key = ed25519();
    let cfg = host_cfg(&[&key], None);
    let store = ResumeStore::default();
    let mut auth = client(vec![key], None);
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], [9; 32], &mut auth).await;
    assert!(h.is_err());
    assert!(c.is_err());
}

#[tokio::test]
async fn password_ok() {
    let cfg = host_cfg(&[], Some("correct horse battery staple"));
    let store = ResumeStore::default();
    let mut auth = client(vec![], Some("correct horse battery staple"));
    let (h, c) = run(&cfg, &store, [3; 32], [3; 32], HOST_FP, &mut auth).await;
    assert_eq!(h.unwrap().kind, AuthKind::Password);
    c.unwrap();
}

#[tokio::test]
async fn password_wrong() {
    let cfg = host_cfg(&[], Some("correct horse battery staple"));
    let store = ResumeStore::default();
    let mut auth = client(vec![], Some("Tr0ub4dor&3"));
    let (h, c) = run(&cfg, &store, [3; 32], [3; 32], HOST_FP, &mut auth).await;
    assert!(h.is_err());
    assert!(c.unwrap_err().to_string().contains("wrong password"));
}

#[tokio::test]
async fn password_channel_binding_mismatch_fails() {
    let cfg = host_cfg(&[], Some("pw"));
    let store = ResumeStore::default();
    let mut auth = client(vec![], Some("pw"));
    let (h, c) = run(&cfg, &store, [3; 32], [4; 32], HOST_FP, &mut auth).await;
    assert!(h.is_err());
    assert!(c.is_err());
}

#[tokio::test]
async fn falls_back_from_unauthorized_key_to_password() {
    let cfg = host_cfg(&[], Some("pw"));
    let store = ResumeStore::default();
    let mut auth = client(vec![ed25519()], Some("pw"));
    let (h, c) = run(&cfg, &store, [3; 32], [3; 32], HOST_FP, &mut auth).await;
    assert_eq!(h.unwrap().kind, AuthKind::Password);
    c.unwrap();
}

#[tokio::test]
async fn password_disabled_on_host() {
    let cfg = host_cfg(&[], None);
    let store = ResumeStore::default();
    let mut auth = client(vec![], Some("pw"));
    let (h, c) = run(&cfg, &store, [3; 32], [3; 32], HOST_FP, &mut auth).await;
    assert!(h.is_err());
    assert!(c.is_err());
}

#[tokio::test]
async fn resume_is_single_use_and_bound() {
    let key = ed25519();
    let cfg = host_cfg(&[&key], None);
    let store = ResumeStore::default();

    let mut auth = client(vec![key.clone()], None);
    let (_, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    let ok = c.unwrap();

    // Resume on a new connection without keys: works.
    let mut resume_only = client(vec![], None);
    resume_only.resume = Some((ok.session_id, ok.resume_token));
    let (h, c) = run(&cfg, &store, [5; 32], [5; 32], HOST_FP, &mut resume_only).await;
    assert_eq!(h.unwrap().kind, AuthKind::Resume);
    let ok2 = c.unwrap();
    assert_eq!(ok2.session_id, ok.session_id);
    assert_ne!(ok2.resume_token, ok.resume_token, "token must rotate");

    // Old token is spent.
    let mut replay = client(vec![], None);
    replay.resume = Some((ok.session_id, ok.resume_token));
    let (h, c) = run(&cfg, &store, [6; 32], [6; 32], HOST_FP, &mut replay).await;
    assert!(h.is_err());
    assert!(c.is_err());

    // New token presented over a mismatched channel is rejected (and burned).
    let mut mitm = client(vec![], None);
    mitm.resume = Some((ok2.session_id, ok2.resume_token));
    let (h, c) = run(&cfg, &store, [7; 32], [8; 32], HOST_FP, &mut mitm).await;
    assert!(h.is_err());
    assert!(c.is_err());
}

#[test]
fn authorized_keys_parsing() {
    let k = ed25519();
    let line = k.public_key().to_openssh().unwrap();
    let text = format!("# comment\n\n{line}\n");
    let keys = parse_authorized_keys(&text).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key_data(), k.public_key().key_data());
    assert!(parse_authorized_keys("garbage").is_err());
}

#[test]
fn limiter_backs_off() {
    let l = FailureLimiter::default();
    let ip: IpAddr = "100.64.0.9".parse().unwrap();
    for _ in 0..2 {
        l.failure(ip);
    }
    assert!(l.check(ip).is_ok());
    l.failure(ip);
    assert!(l.check(ip).is_err());
    l.success(ip);
    assert!(l.check(ip).is_ok());
}

// ---------------------------------------------------------------------------
// Tickets (computers registered with an rdesk server)
// ---------------------------------------------------------------------------

fn ticket_for(server: &iroh::SecretKey, client: [u8; 32], user_id: i64, issued: i64) -> Vec<u8> {
    use rdesk_proto::ticket::{SignedTicket, TicketClaims};
    let claims = TicketClaims {
        ticket_id: [0; 16],
        machine: HOST_FP,
        client,
        user_id,
        user_email: "ana@example.com".into(),
        user_name: "Ana".into(),
        role: "screen,input,files".into(),
        issued_at: issued,
        expires_at: issued + 3600,
    }
    .to_bytes();
    let sig = server.sign(&SignedTicket::signed_message(&claims));
    SignedTicket { claims, signature: sig.to_bytes() }.to_bytes()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn registered(server: &iroh::SecretKey, keys: &[&PrivateKey], password: Option<&str>, local: bool) -> HostAuthConfig {
    let mut cfg = host_cfg(keys, password);
    cfg.ticket = Some(TicketPolicy {
        server_keys: std::sync::Arc::new(std::sync::RwLock::new(vec![*server.public().as_bytes()])),
        machine: HOST_FP,
        revoked: Default::default(),
    });
    cfg.require_local = local;
    cfg
}

#[tokio::test]
async fn ticket_alone_logs_in_and_keeps_the_user() {
    let server = iroh::SecretKey::generate();
    let cfg = registered(&server, &[], None, false);
    let store = ResumeStore::default();
    let mut auth = client(vec![], None);
    auth.ticket = Some(ticket_for(&server, PEER, 7, unix_now()));
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    let h = h.unwrap();
    assert_eq!(h.kind, AuthKind::Ticket { local: None });
    assert_eq!(h.user.as_ref().unwrap().user_id, 7);

    // Resuming keeps the user, until their access is revoked.
    let c = c.unwrap();
    let mut again = client(vec![], None);
    again.resume = Some((c.session_id, c.resume_token));
    let (h2, _) = run(&cfg, &store, [2; 32], [2; 32], HOST_FP, &mut again).await;
    let h2 = h2.unwrap();
    assert_eq!(h2.kind, AuthKind::Resume);
    assert_eq!(h2.user.unwrap().user_id, 7);
}

#[tokio::test]
async fn registered_hosts_need_a_ticket() {
    let server = iroh::SecretKey::generate();
    let key = ed25519();
    // Even an authorized key does not get in without a ticket.
    let cfg = registered(&server, &[&key], Some("pw"), true);
    let store = ResumeStore::default();
    let mut auth = client(vec![key.clone()], Some("pw"));
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    assert!(h.is_err() && c.is_err());

    // A ticket for another connection is refused.
    let mut auth = client(vec![], None);
    auth.ticket = Some(ticket_for(&server, [6; 32], 7, unix_now()));
    let (h, _) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    assert!(format!("{:#}", h.unwrap_err()).contains("another connection"));

    // A revoked user is refused.
    let cfg2 = registered(&server, &[], None, false);
    cfg2.ticket.as_ref().unwrap().revoked.write().unwrap().insert(7, unix_now());
    let mut auth = client(vec![], None);
    auth.ticket = Some(ticket_for(&server, PEER, 7, unix_now() - 5));
    let (h, _) = run(&cfg2, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    assert!(format!("{:#}", h.unwrap_err()).contains("removed"));
}

/// Direct access (local network, turned on by an administrator): a registered computer's
/// own key or password are enough there, without a ticket.
#[tokio::test]
async fn direct_access_lets_local_logins_in() {
    let server = iroh::SecretKey::generate();
    let key = ed25519();
    let mut cfg = registered(&server, &[&key], Some("pw"), false);
    cfg.direct = true;
    let store = ResumeStore::default();

    let mut auth = client(vec![key.clone()], None);
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    assert!(matches!(h.unwrap().kind, AuthKind::PublicKey { .. }));
    c.unwrap();

    let mut auth = client(vec![], Some("pw"));
    let (h, _) = run(&cfg, &store, [2; 32], [2; 32], HOST_FP, &mut auth).await;
    assert_eq!(h.unwrap().kind, AuthKind::Password);

    // Off (or from elsewhere): the ticket is required again.
    cfg.direct = false;
    let mut auth = client(vec![key.clone()], Some("pw"));
    let (h, _) = run(&cfg, &store, [3; 32], [3; 32], HOST_FP, &mut auth).await;
    assert!(h.is_err());
}

#[tokio::test]
async fn ticket_plus_local_password() {
    let server = iroh::SecretKey::generate();
    let cfg = registered(&server, &[], Some("correct horse"), true);
    let store = ResumeStore::default();

    let mut auth = client(vec![], Some("correct horse"));
    auth.ticket = Some(ticket_for(&server, PEER, 7, unix_now()));
    let (h, c) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    assert_eq!(h.unwrap().kind, AuthKind::Ticket { local: Some("password".into()) });
    assert!(c.is_ok());

    let mut auth = client(vec![], Some("wrong"));
    auth.ticket = Some(ticket_for(&server, PEER, 7, unix_now()));
    let (h, _) = run(&cfg, &store, [1; 32], [1; 32], HOST_FP, &mut auth).await;
    assert!(h.is_err(), "the ticket alone is not enough when the host wants its password");
}
