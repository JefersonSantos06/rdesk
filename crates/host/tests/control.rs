//! The host as the desktop app runs it: start/stop, status, session tracking, events and
//! "Disconnect" (the client is told not to reconnect).

use std::time::Duration;

use rand::rngs::OsRng;
use rdesk_common::auth::{self, ClientAuth, KeySource};
use rdesk_common::config::{self, HostConfig, NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, ConnectionError, Role, close};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};
use ssh_key::{Algorithm, PrivateKey};

async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn start_report_disconnect_stop() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let fp = rdesk_host::admin::ensure_initialized().unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = HostConfig { port, ..Default::default() };
    rdesk_host::admin::save_config(&cfg).unwrap();

    let opts = RunOpts { fake_capture: true, fake_input: true, ..Default::default() };
    let rt = tokio::runtime::Handle::current();

    // No password and no key yet: the host refuses to start, and says why.
    let h = HostHandle::start(opts.clone(), &rt, Reporter::silent());
    wait_for("failure", || matches!(h.status().state, HostState::Failed(_))).await;
    assert!(format!("{:?}", h.status().state).contains("no way to log in"));

    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    assert!(rdesk_host::admin::add_key(key.public_key()).unwrap());
    assert!(!rdesk_host::admin::add_key(key.public_key()).unwrap(), "second add is a no-op");

    let h = HostHandle::start(opts, &rt, Reporter::silent());
    wait_for("listening", || matches!(h.status().state, HostState::Listening(_))).await;
    assert_eq!(h.status().fingerprint, Some(fp));

    // Log in: the status shows the session and an event.
    let addr = format!("127.0.0.1:{port}");
    let net = NetworkConfig::default();
    let ep = transport::bind(None, &net, &TransportTuning::default(), Role::Client).await.unwrap();
    let c = transport::connect(&ep, fp, Some(&addr), &net, Duration::from_secs(5)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut creds = ClientAuth {
        client_name: "laptop".into(),
        resume: None,
        keys: Some(KeySource::Local(vec![key])),
        key_filter: None,
        password: None,
    };
    auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();
    wait_for("session", || h.status().session.is_some()).await;
    let s = h.status();
    assert_eq!(s.session.as_ref().unwrap().client, "laptop");
    assert!(s.events.iter().any(|e| e.text.contains("laptop connected")), "{:?}", s.events);

    // Disconnect from the host side: the client sees KICKED (= do not reconnect).
    h.disconnect();
    let reason = tokio::time::timeout(Duration::from_secs(5), c.conn.closed()).await.unwrap();
    match reason {
        ConnectionError::ApplicationClosed(a) => assert_eq!(a.error_code, close::KICKED),
        other => panic!("unexpected close: {other:?}"),
    }
    wait_for("session cleared", || h.status().session.is_none()).await;

    // Stop: sockets close and the state says so.
    let status = h.status();
    assert!(status.events.iter().any(|e| e.text.contains("Session ended from this computer")));
    h.shutdown().await;
    let _ = config::config_dir();
}
