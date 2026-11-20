//! The picture starts only when the client opens its acks stream: a session opened for a
//! terminal gets no frames (the host captures nothing) until the screen is asked for.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdesk_client::conn::{self, PasswordAnswer, UiEvent};
use rdesk_common::auth;
use rdesk_common::config::{AuthPref, HostConfig, HostEntry, NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, Role};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};
use rdesk_proto::{FrameHeader, MAX_VIDEO_MSG, StreamOpen, expect_msg, write_msg};

const PASSWORD: &str = "correct horse battery staple";

#[tokio::test(flavor = "multi_thread")]
async fn no_frames_until_the_acks_stream_opens() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let fp = rdesk_host::admin::ensure_initialized().unwrap();
    rdesk_host::admin::set_password(PASSWORD).unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    rdesk_host::admin::save_config(&HostConfig { port, ..Default::default() }).unwrap();
    let h = HostHandle::start(
        RunOpts { fake_capture: true, fake_input: true, ..Default::default() },
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
    let entry = HostEntry { addr: addr.clone(), auth: AuthPref::Password, ..Default::default() };
    let dialog: conn::Notify = Arc::new(|ev| {
        if let UiEvent::PasswordNeeded { reply, .. } = ev {
            reply.send(Some(PasswordAnswer { password: zeroize::Zeroizing::new(PASSWORD.into()), remember: false }));
        }
    });
    let target = format!("video-test-{}", std::process::id());
    let asker = conn::password_asker(&target, &entry, Arc::new(Mutex::new(None)), Some((dialog, None)));
    let mut creds = conn::credentials(&entry, None, asker).await;
    let c = transport::connect(&ep, fp, Some(&addr), &net, Duration::from_secs(5)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let ok = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();
    assert!(rdesk_proto::Access::from(ok.access).screen, "a local login may see the screen");

    // Signed in with the right to see the screen, but no acks stream: nothing comes.
    let early = tokio::time::timeout(Duration::from_secs(2), c.conn.accept_uni()).await;
    assert!(early.is_err(), "the host sent a frame before the client asked for the picture");

    let mut acks = c.conn.open_uni().await.unwrap();
    write_msg(&mut acks, &StreamOpen::Acks).await.unwrap();
    let mut s = tokio::time::timeout(Duration::from_secs(10), c.conn.accept_uni())
        .await
        .expect("no frame after the acks stream opened")
        .unwrap();
    let header: FrameHeader = expect_msg(&mut s, MAX_VIDEO_MSG).await.unwrap();
    assert!(header.full && header.width > 0, "the first frame is a full one: {header:?}");

    c.conn.close(transport::close::NORMAL, b"");
    h.shutdown().await;
}
