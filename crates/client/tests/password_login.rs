//! Password login the way the app does it: the host's password is set with the same
//! function as the "Set password" dialog, and the client gets it from the viewer's
//! password dialog (simulated) through `password_asker`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdesk_client::conn::{self, PasswordAnswer, PasswordUse, UiEvent};
use rdesk_common::auth::{self, AuthFailed, PasswordCancelled};
use rdesk_common::config::{AuthPref, HostConfig, HostEntry, TransportTuning};
use rdesk_common::transport;
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts};

/// A fake viewer: answers every password dialog with `answer` and counts them.
fn dialog(answer: Option<&'static str>, asked: Arc<Mutex<Vec<Option<String>>>>) -> conn::Notify {
    Arc::new(move |ev| {
        if let UiEvent::PasswordNeeded { reply, error, .. } = ev {
            asked.lock().unwrap().push(error);
            reply.send(answer.map(|p| PasswordAnswer { password: zeroize::Zeroizing::new(p.into()), remember: false }));
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn password_from_the_dialog_logs_in() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path()) };
    let fp = rdesk_host::admin::ensure_initialized().unwrap();
    rdesk_host::admin::set_password("correct horse battery staple").unwrap();
    assert!(rdesk_host::admin::has_password());
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = HostConfig { listen: vec![format!("127.0.0.1:{port}")], allow_non_tailnet: true, ..Default::default() };
    rdesk_host::admin::save_config(&cfg).unwrap();
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

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let ep = transport::client_endpoint(&addr).unwrap();
    // A target name nobody has a saved password for.
    let target = format!("pw-test-{}", std::process::id());
    let entry = HostEntry { addr: addr.to_string(), auth: AuthPref::Password, ..Default::default() };

    let attempt = |answer: Option<&'static str>| {
        let (ep, entry, target) = (ep.clone(), entry.clone(), target.clone());
        async move {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let used = Arc::new(Mutex::new(None));
            let asker =
                conn::password_asker(&target, &entry, used.clone(), Some((dialog(answer, asked.clone()), None)));
            let mut creds = conn::credentials(&entry, None, asker).await;
            let c = transport::connect(&ep, addr, Some(fp), &TransportTuning::default(), Duration::from_secs(5))
                .await
                .unwrap();
            let (mut w, mut r) = c.conn.open_bi().await.unwrap();
            let res = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await;
            let n = asked.lock().unwrap().len();
            let typed = matches!(used.lock().unwrap().take(), Some(PasswordUse::Typed { .. }));
            c.conn.close(transport::close::NORMAL, b"");
            (res, n, typed)
        }
    };

    let (res, asked, typed) = attempt(Some("correct horse battery staple")).await;
    assert!(res.is_ok(), "{res:?}");
    assert_eq!((asked, typed), (1, true));

    let (res, _, typed) = attempt(Some("wrong")).await;
    assert!(res.unwrap_err().downcast_ref::<AuthFailed>().is_some());
    assert!(typed);

    let (res, _, _) = attempt(None).await;
    assert!(res.unwrap_err().downcast_ref::<PasswordCancelled>().is_some());

    h.shutdown().await;
}
