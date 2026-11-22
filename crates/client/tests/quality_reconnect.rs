//! Frames per second chosen in the app outlive a reconnect: the new session starts with the
//! host's own settings, and the client asks for its choice again.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use rdesk_client::conn::{self, ConnectOpts, Extensions, PasswordAnswer, Status, UiEvent};
use rdesk_common::config::{AuthPref, HostConfig, HostEntry, NetworkConfig, TransportTuning};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};

const PASSWORD: &str = "correct horse battery staple";

async fn start_host() -> HostHandle {
    let host = HostHandle::start(
        RunOpts { fake_capture: true, fake_input: true, ..Default::default() },
        &tokio::runtime::Handle::current(),
        Reporter::silent(),
    );
    for _ in 0..100 {
        if matches!(host.status().state, HostState::Listening(_)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    host
}

/// The frame rates reported over `window`, from the second one on (the first may straddle a
/// change).
fn rates(rx: &mpsc::Receiver<UiEvent>, window: Duration) -> Vec<f32> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(UiEvent::Stats { fps, .. }) => out.push(fps),
            Ok(UiEvent::Fatal(m)) => panic!("session failed: {m}"),
            _ => {}
        }
    }
    out.into_iter().skip(1).collect()
}

fn wait_connected(rx: &mpsc::Receiver<UiEvent>, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UiEvent::Status(Status::Connected { .. })) => return,
            Ok(UiEvent::Fatal(m)) => panic!("session failed: {m}"),
            _ => {}
        }
    }
    panic!("not connected {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn frame_rate_is_kept_across_a_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let fp = rdesk_host::admin::ensure_initialized().unwrap();
    rdesk_host::admin::set_password(PASSWORD).unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    rdesk_host::admin::save_config(&HostConfig { port, max_fps: 30, ..Default::default() }).unwrap();
    let host = start_host().await;

    let (tx, rx) = mpsc::channel::<UiEvent>();
    let opts = ConnectOpts {
        name: format!("quality-test-{}", std::process::id()),
        ask_password_in_ui: true,
        entry: HostEntry { addr: format!("127.0.0.1:{port}"), auth: AuthPref::Password, ..Default::default() },
        id: fp,
        machine: None,
        monitor: None,
        transport: TransportTuning::default(),
        net: NetworkConfig::default(),
        clipboard: false,
        video: true,
        extensions: Extensions::default(),
        device_key: None,
        audio: None,
    };
    let handle = conn::spawn(opts, move |ev| {
        if let UiEvent::PasswordNeeded { reply, .. } = &ev {
            reply.send(Some(PasswordAnswer { password: zeroize::Zeroizing::new(PASSWORD.into()), remember: false }));
        }
        let _ = tx.send(ev);
    });
    wait_connected(&rx, "at first");
    let host_rate = rates(&rx, Duration::from_secs(3));
    assert!(host_rate.iter().any(|&f| f > 10.0), "the host's 30 fps: {host_rate:?}");

    handle.cmd.send(conn::Cmd::SetQuality { max_fps: 5, jpeg_quality: 70 }).unwrap();
    let chosen = rates(&rx, Duration::from_secs(3));
    assert!(chosen.iter().all(|&f| f <= 7.0), "asked for 5 fps: {chosen:?}");

    // The host restarts: the client reconnects to a session with the host's 30 fps.
    host.shutdown().await;
    let host = start_host().await;
    wait_connected(&rx, "again after the host restarted");
    let after = rates(&rx, Duration::from_secs(4));
    eprintln!("fps: {host_rate:?} (host), {chosen:?} (5 fps), {after:?} (after the reconnect)");
    assert!(!after.is_empty() && after.iter().all(|&f| f <= 7.0), "5 fps lost in the reconnect: {after:?}");

    handle.cmd.send(conn::Cmd::Quit).unwrap();
    let _ = tokio::task::spawn_blocking(move || handle.thread.join()).await;
    host.shutdown().await;
}
