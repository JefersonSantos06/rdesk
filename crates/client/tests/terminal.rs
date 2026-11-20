//! A terminal end to end: a real host on loopback, a password login, then a shell on the
//! terminal stream that runs a command and exits.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdesk_client::conn::{self, PasswordAnswer, UiEvent};
use rdesk_common::auth;
use rdesk_common::config::{AuthPref, HostConfig, HostEntry, NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, Role};
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};
use rdesk_proto::{TermClient, TermHost};

const PASSWORD: &str = "correct horse battery staple";

/// Answer the questions the shell's terminal asks (as the app's terminal window does).
fn answer(link: &rdesk_client::terminal::TerminalLink, output: &[u8]) {
    let reply = rdesk_client::terminal::answers(output, (0, 0));
    if !reply.is_empty() {
        link.tx.send(TermClient::Input(reply)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_runs_a_command() {
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

    // Log in (the password dialog answers at once).
    let addr = format!("127.0.0.1:{port}");
    let net = NetworkConfig::default();
    let ep = transport::bind(None, &net, &TransportTuning::default(), Role::Client).await.unwrap();
    let entry = HostEntry { addr: addr.clone(), auth: AuthPref::Password, ..Default::default() };
    let dialog: conn::Notify = Arc::new(|ev| {
        if let UiEvent::PasswordNeeded { reply, .. } = ev {
            reply.send(Some(PasswordAnswer { password: zeroize::Zeroizing::new(PASSWORD.into()), remember: false }));
        }
    });
    let target = format!("term-test-{}", std::process::id());
    let asker = conn::password_asker(&target, &entry, Arc::new(Mutex::new(None)), Some((dialog, None)));
    let mut creds = conn::credentials(&entry, None, asker).await;
    let c = transport::connect(&ep, fp, Some(&addr), &net, Duration::from_secs(5)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let ok = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();
    assert!(rdesk_proto::Access::from(ok.access).terminal, "a local login may open a terminal");

    // As administrator: refused unless this host has those rights (tests run unelevated).
    let mut admin = rdesk_client::terminal::open(c.conn.clone(), 100, 30, true, || {}).await.unwrap();
    match tokio::time::timeout(Duration::from_secs(45), admin.rx.recv()).await.unwrap() {
        Some(TermHost::Error(e)) => assert!(e.contains("administrator"), "{e}"),
        Some(TermHost::Started { user, .. }) => assert!(user.ends_with("(administrator)"), "{user}"),
        other => panic!("unexpected {other:?}"),
    }
    drop(admin);

    let mut link = rdesk_client::terminal::open(c.conn.clone(), 100, 30, false, || {}).await.unwrap();
    let start = Instant::now();
    let mut output = String::new();
    let mut started = false;
    let mut exited = None;
    let next =
        async |link: &mut rdesk_client::terminal::TerminalLink, phase: &str, seen: &str| match tokio::time::timeout(
            Duration::from_secs(45),
            link.rx.recv(),
        )
        .await
        {
            Ok(m) => m,
            Err(_) => panic!("the terminal went quiet ({phase}); so far: {seen:?}"),
        };
    while !started {
        match next(&mut link, "starting", &output).await.expect("the terminal closed") {
            TermHost::Started { .. } => started = true,
            TermHost::Error(e) => panic!("the terminal did not start: {e}"),
            TermHost::Output(b) => {
                answer(&link, &b);
                output.push_str(&String::from_utf8_lossy(&b));
            }
            TermHost::Exited(c) => panic!("the shell exited at once ({c})"),
        }
    }
    // The command's output is the joined word, which its echo (two words) is not.
    link.tx.send(TermClient::Input(b"echo rdesk-term ok-marker\r".to_vec())).unwrap();
    link.tx.send(TermClient::Resize { cols: 120, rows: 40 }).unwrap();
    while !output.contains("rdesk-term ok-marker\r\n") && !output.contains("rdesk-term ok-marker\n") {
        assert!(start.elapsed() < Duration::from_secs(60), "no output from the command: {output:?}");
        match next(&mut link, "running the command", &output).await.expect("the terminal closed") {
            TermHost::Output(b) => {
                answer(&link, &b);
                output.push_str(&String::from_utf8_lossy(&b));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    link.tx.send(TermClient::Input(b"exit\r".to_vec())).unwrap();
    while exited.is_none() {
        match next(&mut link, "exiting", &output).await.expect("the terminal closed before saying how the shell ended")
        {
            TermHost::Exited(code) => exited = Some(code),
            TermHost::Output(b) => answer(&link, &b),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(exited, Some(0));
    c.conn.close(transport::close::NORMAL, b"");
    h.shutdown().await;
}
