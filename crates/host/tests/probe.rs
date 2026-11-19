//! A real host on loopback: launcher probes report status without logging in, and do not
//! count as failed logins.

use std::time::Duration;

use rand::rngs::OsRng;
use rdesk_common::auth::{self, ClientAuth, KeySource};
use rdesk_common::config::{self, NetworkConfig, TransportTuning};
use rdesk_common::directory;
use rdesk_common::transport::{self, HostIdentity, Role};
use ssh_key::{Algorithm, PrivateKey};

#[tokio::test(flavor = "multi_thread")]
async fn probe_reports_status_and_is_not_a_login() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let id = HostIdentity::generate();
    id.save().unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let line = key.public_key().to_openssh().unwrap();
    config::write_private(&config::config_path(config::AUTHORIZED_KEYS).unwrap(), line.as_bytes()).unwrap();

    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");
    let host =
        tokio::spawn(rdesk_host::run(rdesk_host::RunOpts { port: Some(port), fake_capture: true, fake_input: true }));

    let net = NetworkConfig::default();
    let ep = transport::bind(None, &net, &TransportTuning::default(), Role::Client).await.unwrap();
    let probe = || directory::probe(&ep, id.fingerprint(), Some(&addr), &net, Duration::from_secs(2));

    // Wait for the host to listen.
    let mut first = None;
    for _ in 0..50 {
        if let Ok(p) = probe().await {
            first = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let p = first.expect("host never answered a probe");
    assert_eq!(p.host_name, rdesk_common::local_hostname());
    assert_eq!(p.proto, rdesk_proto::PROTOCOL_VERSION);
    assert_eq!(p.route, Some(transport::Route::Direct));
    assert!(!p.in_session);

    // Many probes in a row must not trip the failed-login backoff.
    for _ in 0..8 {
        assert!(!probe().await.unwrap().in_session);
    }

    // Log in and keep the session open: probes now say "in use".
    let c = transport::connect(&ep, id.fingerprint(), Some(&addr), &net, Duration::from_secs(5)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut creds = ClientAuth {
        client_name: "test".into(),
        resume: None,
        keys: Some(KeySource::Local(vec![key])),
        key_filter: None,
        password: None,
    };
    auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();
    let mut busy = false;
    for _ in 0..20 {
        if probe().await.unwrap().in_session {
            busy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(busy, "probe should report the open session");

    // The probe did not disturb the session.
    assert!(c.conn.close_reason().is_none());
    c.conn.close(transport::close::NORMAL, b"bye");
    host.abort();
}
