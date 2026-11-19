//! End-to-end over loopback: iroh endpoints, host ID authentication, and the auth handshake,
//! both directly by address and through a (local) relay with no address at all.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use rand::rngs::OsRng;
use rdesk_common::auth::{self, ClientAuth, HostAuthConfig, KeySource, ResumeStore};
use rdesk_common::config::{NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, Endpoint, HostIdentity, Role};
use rdesk_proto::HostInfo;
use ssh_key::{Algorithm, PrivateKey};

fn info() -> HostInfo {
    HostInfo { hostname: "h".into(), os: "test".into(), monitors: vec![], active_monitor: 0 }
}

/// The loopback address of the host's IPv4 socket.
fn loopback(ep: &Endpoint) -> String {
    let port = ep.bound_sockets().into_iter().find(SocketAddr::is_ipv4).expect("an IPv4 socket").port();
    SocketAddr::from((Ipv4Addr::LOCALHOST, port)).to_string()
}

async fn start_host(
    id: &HostIdentity,
    key: &PrivateKey,
    net: &NetworkConfig,
) -> (Endpoint, tokio::task::JoinHandle<anyhow::Result<auth::Authenticated>>) {
    let t = TransportTuning::default();
    let ep = transport::bind(Some(id.secret_key()), net, &t, Role::Host { port: 0 }).await.unwrap();
    let cfg = HostAuthConfig {
        authorized_keys: vec![key.public_key().key_data().clone()],
        opaque: None,
        host_fp: id.fingerprint(),
        host_name: "h".into(),
        ticket: None,
        require_local: false,
        direct: false,
    };
    let ep2 = ep.clone();
    let task = tokio::spawn(async move {
        let conn = ep2.accept().await.unwrap().await?;
        let ek = transport::exporter(&conn)?;
        let (mut w, mut r) = conn.accept_bi().await?;
        let store = ResumeStore::default();
        let peer = *conn.remote_id().as_bytes();
        let a = auth::host_handshake(&cfg, &store, &mut r, &mut w, ek, peer, info).await?;
        transport::raise_stream_limits(&conn);
        // Keep the connection open until the client hangs up.
        let _ = conn.closed().await;
        Ok(a)
    });
    (ep, task)
}

async fn client(net: &NetworkConfig) -> Endpoint {
    transport::bind(None, net, &TransportTuning::default(), Role::Client).await.unwrap()
}

/// Logs in and returns the session id.
async fn login(c: &transport::Connected, key: PrivateKey) -> rdesk_proto::SessionId {
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut a = ClientAuth {
        client_name: "c".into(),
        resume: None,
        ticket: None,
        keys: Some(KeySource::Local(vec![key])),
        key_filter: None,
        password: None,
    };
    auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut a).await.unwrap().session_id
}

#[tokio::test]
async fn direct_pubkey_login() {
    let net = NetworkConfig::default();
    let id = HostIdentity::generate();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (ep, host) = start_host(&id, &key, &net).await;

    let cep = client(&net).await;
    let c =
        transport::connect(&cep, id.fingerprint(), Some(&loopback(&ep)), &net, Duration::from_secs(5)).await.unwrap();
    assert_eq!(c.host_fingerprint, id.fingerprint());
    assert_eq!(transport::path_info(&c.conn).1, Some(transport::Route::Direct));
    let ok = login(&c, key).await;
    // After auth the client may open more streams.
    let mut uni = c.conn.open_uni().await.unwrap();
    uni.write_all(b"x").await.unwrap();
    c.conn.close(transport::close::NORMAL, b"bye");
    let h = host.await.unwrap().unwrap();
    assert_eq!(h.session_id, ok);
}

#[tokio::test]
async fn wrong_host_id_never_connects() {
    let net = NetworkConfig::default();
    let id = HostIdentity::generate();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (ep, _host) = start_host(&id, &key, &net).await;
    let cep = client(&net).await;
    let someone_else = HostIdentity::generate().fingerprint();
    let start = std::time::Instant::now();
    let r = transport::connect(&cep, someone_else, Some(&loopback(&ep)), &net, Duration::from_secs(2)).await;
    let err = r.err().expect("connected to a host with another ID");
    assert!(format!("{err:#}").starts_with(transport::WRONG_HOST_ID), "{err:#}");
    assert!(start.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn no_extra_streams_before_auth() {
    let net = NetworkConfig::default();
    let id = HostIdentity::generate();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (ep, _host) = start_host(&id, &key, &net).await;
    let cep = client(&net).await;
    let c =
        transport::connect(&cep, id.fingerprint(), Some(&loopback(&ep)), &net, Duration::from_secs(5)).await.unwrap();
    // Host allows zero uni streams until auth succeeds: opening one must stall.
    let r = tokio::time::timeout(Duration::from_millis(500), c.conn.open_uni()).await;
    assert!(r.is_err(), "uni stream opened before auth");
}

#[tokio::test]
async fn no_address_and_no_relay_is_a_clear_error() {
    let net = NetworkConfig::default();
    let cep = client(&net).await;
    let err = transport::connect(&cep, HostIdentity::generate().fingerprint(), None, &net, Duration::from_secs(2))
        .await
        .err()
        .expect("must fail");
    assert!(format!("{err:#}").contains("no way to reach"), "{err:#}");
}

/// A relay like the one on your VPS, in-process and over plain HTTP.
async fn local_relay() -> (iroh_relay::server::Server, NetworkConfig) {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let srv = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}", srv.http_addr().unwrap());
    (srv, NetworkConfig { relay: Some(url), token: None })
}

#[tokio::test(flavor = "multi_thread")]
async fn login_through_the_relay_with_only_the_id() {
    let (_relay, net) = local_relay().await;
    let id = HostIdentity::generate();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (ep, host) = start_host(&id, &key, &net).await;
    assert!(transport::wait_relay(&ep, &net, Duration::from_secs(5)).await.unwrap());

    let cep = client(&net).await;
    // No address: the relay is the only way to find the host.
    let c = transport::connect(&cep, id.fingerprint(), None, &net, Duration::from_secs(10)).await.unwrap();
    assert_eq!(c.host_fingerprint, id.fingerprint());
    let ok = login(&c, key).await;
    c.conn.close(transport::close::NORMAL, b"bye");
    assert_eq!(host.await.unwrap().unwrap().session_id, ok);
}
