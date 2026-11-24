//! A computer that has a screen but no monitor right now (a Raspberry Pi started with its
//! monitor unplugged): a login with full access gets a session with the terminal and files,
//! told why there is no picture, instead of a connection closed for want of a monitor.

use std::time::Duration;

use anyhow::Result;
use rand::rngs::OsRng;
use rdesk_common::auth::{self, ClientAuth, KeySource};
use rdesk_common::config::{HostConfig, NetworkConfig, TransportTuning};
use rdesk_common::tiles::Frame;
use rdesk_common::transport::{self, Role};
use rdesk_host::capture::{Backend, MonitorDesc};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};
use rdesk_proto::{ClientMsg, HostMsg, MAX_CONTROL_MSG, expect_msg, write_msg};
use ssh_key::{Algorithm, PrivateKey};

/// A screen with no monitor on it.
struct NoMonitor;

impl Backend for NoMonitor {
    fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        Ok(Vec::new())
    }
    fn grab(&mut self, id: u32) -> Result<Frame> {
        anyhow::bail!("monitor {id} not found")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_monitor_still_gives_the_terminal_and_files() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe {
        std::env::set_var("RDESK_CONFIG_DIR", dir.path());
        // Linux: a desktop (an X display), not a server without one.
        std::env::set_var("DISPLAY", ":99");
    }
    rdesk_host::capture::set_screen_source(|| Box::new(NoMonitor));
    let fp = rdesk_host::admin::ensure_initialized().unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    rdesk_host::admin::save_config(&HostConfig { port, ..Default::default() }).unwrap();
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    rdesk_host::admin::add_key(key.public_key()).unwrap();
    let h = HostHandle::start(
        RunOpts { fake_input: true, ..Default::default() },
        &tokio::runtime::Handle::current(),
        Reporter::silent(),
    );
    for _ in 0..100 {
        if matches!(h.status().state, HostState::Listening(_)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let addr = format!("127.0.0.1:{port}");
    let net = NetworkConfig::default();
    let ep = transport::bind(None, &net, &TransportTuning::default(), Role::Client).await.unwrap();
    let c = transport::connect(&ep, fp, Some(&addr), &net, Duration::from_secs(5)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut creds = ClientAuth {
        client_name: "laptop".into(),
        resume: None,
        ticket: None,
        keys: Some(KeySource::Local(vec![key])),
        key_filter: None,
        password: None,
    };
    let ok = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();
    let access = ok.session_access();
    assert!(ok.host.monitors.is_empty());
    assert!(!access.screen && !access.input, "no picture to see or control: {access:?}");
    assert!(access.files && access.terminal, "the terminal and files are left: {access:?}");

    // Told why, then the session answers: it was not closed.
    let notice: HostMsg = tokio::time::timeout(Duration::from_secs(5), expect_msg(&mut r, MAX_CONTROL_MSG))
        .await
        .expect("a notice")
        .unwrap();
    assert!(matches!(&notice, HostMsg::Notice { text } if text.contains("no screen")), "{notice:?}");
    write_msg(&mut w, &ClientMsg::Ping { t_us: 7 }).await.unwrap();
    let pong = loop {
        let m: HostMsg =
            tokio::time::timeout(Duration::from_secs(5), expect_msg(&mut r, MAX_CONTROL_MSG)).await.unwrap().unwrap();
        if matches!(m, HostMsg::Pong { .. }) {
            break m;
        }
    };
    assert!(matches!(pong, HostMsg::Pong { t_us: 7 }));

    c.conn.close(transport::close::NORMAL, b"");
    h.shutdown().await;
}
