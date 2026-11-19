//! A real host on loopback tells nothing about itself before login: a status probe gets a
//! refusal (no name, OS, version or "in use"), does not count as a failed login, and a
//! normal login still works afterwards.

use std::time::Duration;

use rand::rngs::OsRng;
use rdesk_common::auth::{self, ClientAuth, KeySource};
use rdesk_common::config::{self, NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, HostIdentity, Role};
use rdesk_proto::{ClientMsg, HostMsg, MAX_SMALL_MSG, expect_msg, write_msg};
use ssh_key::{Algorithm, PrivateKey};

#[tokio::test(flavor = "multi_thread")]
async fn probes_learn_nothing_and_are_not_logins() {
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
    let probe = async || -> anyhow::Result<HostMsg> {
        let c = transport::connect(&ep, id.fingerprint(), Some(&addr), &net, Duration::from_secs(2)).await?;
        let (mut w, mut r) = c.conn.open_bi().await?;
        write_msg(&mut w, &ClientMsg::Probe).await?;
        let _ = w.finish();
        let answer = expect_msg::<_, HostMsg>(&mut r, MAX_SMALL_MSG).await?;
        c.conn.close(transport::close::NORMAL, b"");
        Ok(answer)
    };

    // Wait for the host to listen.
    let mut first = None;
    for _ in 0..50 {
        if let Ok(a) = probe().await {
            first = Some(a);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let answer = first.expect("host never answered");
    assert!(matches!(answer, HostMsg::AuthFail { .. }), "a probe must learn nothing: {answer:?}");

    // Many probes in a row must not trip the failed-login backoff.
    for _ in 0..8 {
        assert!(matches!(probe().await.unwrap(), HostMsg::AuthFail { .. }));
    }

    // A real login still works.
    let c = transport::connect(&ep, id.fingerprint(), Some(&addr), &net, Duration::from_secs(5)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut creds = ClientAuth {
        client_name: "test".into(),
        resume: None,
        ticket: None,
        keys: Some(KeySource::Local(vec![key])),
        key_filter: None,
        password: None,
    };
    let ok = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();
    assert_eq!(ok.access, rdesk_proto::Access::FULL, "a local login may do everything");
    c.conn.close(transport::close::NORMAL, b"bye");
    host.abort();
}
