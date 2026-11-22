//! A port tunnel end to end: a port here leads to an echo server reached from the host.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use rdesk_client::conn::{self, ConnectOpts, Extensions, PasswordAnswer, Status, UiEvent};
use rdesk_client::tunnel::TunnelSpec;
use rdesk_common::config::{AuthPref, HostConfig, HostEntry, NetworkConfig, TransportTuning};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "correct horse battery staple";

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tunnel_carries_a_tcp_connection() {
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

    // What the tunnel leads to: an echo server "on the host's network".
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = echo.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let (tx, rx) = mpsc::channel::<UiEvent>();
    let opts = ConnectOpts {
        name: format!("tunnel-test-{}", std::process::id()),
        ask_password_in_ui: true,
        entry: HostEntry { addr: format!("127.0.0.1:{port}"), auth: AuthPref::Password, ..Default::default() },
        id: fp,
        machine: None,
        monitor: None,
        transport: TransportTuning::default(),
        net: NetworkConfig::default(),
        clipboard: false,
        video: false,
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "not connected");
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UiEvent::Status(Status::Connected { .. })) => break,
            Ok(UiEvent::Fatal(m)) => panic!("session failed: {m}"),
            _ => {}
        }
    }
    let local = free_tcp_port();
    handle.cmd.send(conn::Cmd::Tunnel(TunnelSpec { local, host: "localhost".into(), port: echo_port })).unwrap();

    let mut s = None;
    for _ in 0..50 {
        if let Ok(c) = tokio::net::TcpStream::connect(("127.0.0.1", local)).await {
            s = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut s = s.expect("the tunnel's port never opened");
    s.write_all(b"hello through the tunnel").await.unwrap();
    let mut buf = [0u8; 24];
    tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut buf)).await.expect("no echo").unwrap();
    assert_eq!(&buf, b"hello through the tunnel");

    handle.cmd.send(conn::Cmd::Quit).unwrap();
    let _ = tokio::task::spawn_blocking(move || handle.thread.join()).await;
    host.shutdown().await;
}
