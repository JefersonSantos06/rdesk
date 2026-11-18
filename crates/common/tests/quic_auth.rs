//! End-to-end: QUIC over loopback + certificate pinning + auth handshake.

use std::time::Duration;

use rand::rngs::OsRng;
use rdesk_common::auth::{self, ClientAuth, HostAuthConfig, KeySource, ResumeStore};
use rdesk_common::config::TransportTuning;
use rdesk_common::transport::{self, HostIdentity};
use rdesk_proto::HostInfo;
use ssh_key::{Algorithm, PrivateKey};

fn info() -> HostInfo {
    HostInfo { hostname: "h".into(), os: "test".into(), monitors: vec![], active_monitor: 0 }
}

async fn start_host(
    id: &HostIdentity,
    key: &PrivateKey,
) -> (quinn::Endpoint, tokio::task::JoinHandle<anyhow::Result<auth::Authenticated>>) {
    let t = TransportTuning::default();
    let ep = transport::server_endpoint("127.0.0.1:0".parse().unwrap(), id, &t).unwrap();
    let cfg = HostAuthConfig {
        authorized_keys: vec![key.public_key().key_data().clone()],
        opaque: None,
        host_fp: id.fingerprint(),
        host_name: "h".into(),
    };
    let ep2 = ep.clone();
    let task = tokio::spawn(async move {
        let conn = ep2.accept().await.unwrap().await?;
        let ek = transport::exporter(&conn)?;
        let (mut w, mut r) = conn.accept_bi().await?;
        let store = ResumeStore::default();
        let a = auth::host_handshake(&cfg, &store, &mut r, &mut w, ek, info).await?;
        transport::raise_stream_limits(&conn);
        // Keep the connection open until the client hangs up.
        let _ = conn.closed().await;
        Ok(a)
    });
    (ep, task)
}

#[tokio::test]
async fn quic_pubkey_login_with_tofu_then_pin() {
    let id = HostIdentity::generate().unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (ep, host) = start_host(&id, &key).await;
    let addr = ep.local_addr().unwrap();
    let t = TransportTuning::default();

    let cep = transport::client_endpoint(&addr).unwrap();
    // First contact: no pin; we learn the fingerprint.
    let c = transport::connect(&cep, addr, None, &t, Duration::from_secs(5)).await.unwrap();
    assert_eq!(c.host_fingerprint, id.fingerprint());
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut a = ClientAuth {
        client_name: "c".into(),
        resume: None,
        keys: Some(KeySource::Local(vec![key])),
        key_filter: None,
        password: None,
    };
    let ok = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut a).await.unwrap();
    // After auth the client may open more streams.
    let mut uni = c.conn.open_uni().await.unwrap();
    uni.write_all(b"x").await.unwrap();
    c.conn.close(transport::close::NORMAL, b"bye");
    let h = host.await.unwrap().unwrap();
    assert_eq!(h.session_id, ok.session_id);
}

#[tokio::test]
async fn quic_pin_mismatch_refuses_connection() {
    let id = HostIdentity::generate().unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (ep, _host) = start_host(&id, &key).await;
    let addr = ep.local_addr().unwrap();
    let cep = transport::client_endpoint(&addr).unwrap();
    let err = transport::connect(&cep, addr, Some([0u8; 32]), &TransportTuning::default(), Duration::from_secs(5))
        .await
        .err()
        .expect("must fail");
    assert!(format!("{err:#}").contains("IDENTITY CHANGED"), "{err:#}");
}

#[tokio::test]
async fn quic_no_extra_streams_before_auth() {
    let id = HostIdentity::generate().unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (ep, _host) = start_host(&id, &key).await;
    let addr = ep.local_addr().unwrap();
    let cep = transport::client_endpoint(&addr).unwrap();
    let c = transport::connect(&cep, addr, None, &TransportTuning::default(), Duration::from_secs(5)).await.unwrap();
    // Host allows zero uni streams until auth succeeds: opening one must stall.
    let r = tokio::time::timeout(Duration::from_millis(500), c.conn.open_uni()).await;
    assert!(r.is_err(), "uni stream opened before auth");
}

#[tokio::test]
async fn connect_timeout_to_silent_port() {
    // Nothing listens here: connect must give up quickly instead of hanging.
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    let cep = transport::client_endpoint(&addr).unwrap();
    let start = std::time::Instant::now();
    let r = transport::connect(&cep, addr, None, &TransportTuning::default(), Duration::from_secs(2)).await;
    assert!(r.is_err());
    assert!(start.elapsed() < Duration::from_secs(4));
}
