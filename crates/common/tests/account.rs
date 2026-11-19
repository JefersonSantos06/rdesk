//! The app's sign-in against a real rdesk-server, with a scripted "browser" (test login +
//! TOTP), then a host and a client reaching each other through the server's relay with
//! only the host ID.

use std::time::Duration;

use rdesk_common::account::{self, Api, NotSignedIn, Session};
use rdesk_common::config::{self, TransportTuning};
use rdesk_common::transport::{self, HostIdentity, Role};
use rdesk_server::config::{ServerConfig, Tls};
use rdesk_server::db::Db;

fn totp_code(secret: &str) -> String {
    totp_rs::Builder::new()
        .with_secret(totp_rs::Secret::try_from_base32(secret).unwrap())
        .with_account_name("x")
        .build()
        .unwrap()
        .generate_current()
        .to_string()
}

/// What a person does in the browser: test login, set up TOTP, get sent back to the app.
async fn act_as_browser(url: String, email: &'static str) {
    let b = reqwest::Client::builder().cookie_store(true).redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let base = url.split("/login").next().unwrap().to_string();
    b.get(&url).send().await.unwrap().error_for_status().unwrap();
    b.post(format!("{base}/login/dev")).form(&[("email", email)]).send().await.unwrap();
    let page = b.get(format!("{base}/login/2fa/totp-setup")).send().await.unwrap().text().await.unwrap();
    let secret = page.split("data-totp-secret=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let r = b.post(format!("{base}/login/2fa/totp-setup")).form(&[("code", totp_code(&secret))]).send().await.unwrap();
    let back = r.headers()["location"].to_str().unwrap().to_string();
    let page = reqwest::get(back).await.unwrap().text().await.unwrap();
    assert!(page.contains("You are signed in"), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_then_connect_through_the_servers_relay() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path().join("client")) };

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{port}");
    let db = Db::open(&dir.path().join("rdesk.db")).unwrap();
    db.add_user("ana@example.com", true).unwrap();
    let cfg = ServerConfig {
        name: "Family".into(),
        public_url: server_url.clone(),
        listen: format!("127.0.0.1:{port}"),
        tls: Tls::None,
        dev_login: true,
        ..Default::default()
    };
    let server = rdesk_server::start_with(cfg, db, iroh_base::SecretKey::generate(), dir.path()).await.unwrap();

    assert!(Session::load().is_none());
    assert!(config::load_network().relay.is_none(), "no relay before signing in");

    let session = account::login(&server_url, "test-laptop", |url| {
        tokio::spawn(act_as_browser(url.to_string(), "ana@example.com"));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(session.user.email, "ana@example.com");
    assert_eq!(session.server_name, "Family");
    assert_eq!(session.days_left(), 10);
    assert_eq!(Session::load().as_ref(), Some(&session), "saved for the next start");
    assert_eq!(session.api().unwrap().me().await.unwrap().email, "ana@example.com");

    // The relay comes from the session, with its token.
    let net = config::load_network();
    assert_eq!(net.relay.as_deref(), Some(format!("{server_url}/").as_str()));
    assert_eq!(net.token.as_deref(), Some(session.token.as_str()));

    let t = TransportTuning::default();
    let id = HostIdentity::generate();
    let host = transport::bind(Some(id.secret_key()), &net, &t, Role::Host { port: 0 }).await.unwrap();
    assert!(transport::wait_relay(&host, &net, Duration::from_secs(10)).await.unwrap());
    let accept = {
        let host = host.clone();
        tokio::spawn(async move {
            let conn = host.accept().await.unwrap().await.unwrap();
            // Before login the host allows only the control stream (bidirectional).
            let (_w, mut r) = conn.accept_bi().await.unwrap();
            let got = r.read_to_end(16).await.unwrap();
            conn.close(transport::close::NORMAL, b"");
            got
        })
    };
    let client = transport::bind(None, &net, &t, Role::Client).await.unwrap();
    // No address at all: the server's relay is the only way to find the host.
    let c = transport::connect(&client, id.fingerprint(), None, &net, Duration::from_secs(10)).await.unwrap();
    let (mut w, _r) = c.conn.open_bi().await.unwrap();
    w.write_all(b"hi").await.unwrap();
    w.finish().unwrap();
    assert_eq!(accept.await.unwrap(), b"hi");

    // Signing out ends the session on the server too.
    session.api().unwrap().logout().await.unwrap();
    Session::clear().unwrap();
    let err = Api::new(&server_url, Some(session.token.clone())).unwrap().me().await.unwrap_err();
    assert!(err.downcast_ref::<NotSignedIn>().is_some(), "{err:#}");

    for ep in [host, client] {
        ep.close().await;
    }
    server.stop().await.unwrap();
}
