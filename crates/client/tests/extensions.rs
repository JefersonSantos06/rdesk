//! The extensions stream end to end, as the phone app uses it: the host scales the picture
//! down to the size asked for and reports the pointer.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use rdesk_client::conn::{self, ConnectOpts, Extensions, PasswordAnswer, UiEvent};
use rdesk_common::config::{AuthPref, HostConfig, HostEntry, NetworkConfig, TransportTuning};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};
use rdesk_proto::Feature;

const PASSWORD: &str = "correct horse battery staple";

#[tokio::test(flavor = "multi_thread")]
async fn picture_is_scaled_and_the_pointer_reported() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let fp = rdesk_host::admin::ensure_initialized().unwrap();
    rdesk_host::admin::set_password(PASSWORD).unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    rdesk_host::admin::save_config(&HostConfig { port, ..Default::default() }).unwrap();
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

    let (tx, rx) = mpsc::channel::<UiEvent>();
    let opts = ConnectOpts {
        name: format!("ext-test-{}", std::process::id()),
        ask_password_in_ui: true,
        entry: HostEntry { addr: format!("127.0.0.1:{port}"), auth: AuthPref::Password, ..Default::default() },
        id: fp,
        machine: None,
        monitor: None,
        transport: TransportTuning::default(),
        net: NetworkConfig::default(),
        clipboard: false,
        video: true,
        extensions: Extensions { max_size: Some((320, 320)), cursor: true, clipboard_in_app: true },
        device_key: None,
    };
    let handle = conn::spawn(opts, move |ev| {
        if let UiEvent::PasswordNeeded { reply, .. } = &ev {
            reply.send(Some(PasswordAnswer { password: zeroize::Zeroizing::new(PASSWORD.into()), remember: false }));
        }
        let _ = tx.send(ev);
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut features, mut cursor, mut size) = (None, false, None);
    while Instant::now() < deadline && (features.is_none() || !cursor || size.is_none()) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UiEvent::Extensions(f)) => features = Some(f),
            Ok(UiEvent::Cursor { .. }) => cursor = true,
            Ok(UiEvent::Fatal(m)) => panic!("session failed: {m}"),
            _ => {}
        }
        if let Some((w, h, _)) = conn::take_frame(&handle.view) {
            size = Some((w, h));
        }
    }
    let features = features.expect("no answer on the extensions stream");
    assert!(features.contains(&Feature::MaxSize) && features.contains(&Feature::Cursor), "{features:?}");
    assert!(cursor, "no pointer position came");
    let (w, h) = size.expect("no picture came");
    assert!(w <= 320 && h <= 320 && (w == 320 || h == 320), "picture {w}x{h} does not fit 320x320 snugly");

    // Back to the full size: the next frames are bigger.
    handle.cmd.send(conn::Cmd::SetMaxSize(0, 0)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut bigger = false;
    while Instant::now() < deadline && !bigger {
        let _ = rx.recv_timeout(Duration::from_millis(200));
        if let Some((w2, h2, _)) = conn::take_frame(&handle.view) {
            bigger = w2 > w && h2 > h;
        }
    }
    assert!(bigger, "the picture did not grow back after asking for the full size");

    handle.cmd.send(conn::Cmd::Quit).unwrap();
    let _ = tokio::task::spawn_blocking(move || handle.thread.join()).await;
    host.shutdown().await;
}
