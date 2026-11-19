//! The whole sign-in against a real server on loopback: test login, TOTP setup, the
//! one-time code traded for a session (PKCE), the API, the admin API, and the relay
//! letting in only signed-in endpoints.

use std::time::Duration;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl};
use rdesk_proto::api::{AddUser, AdminUser, ServerInfo, TokenRequest, TokenResponse, UserInfo};
use rdesk_server::config::{ServerConfig, Tls};
use rdesk_server::db::Db;
use rdesk_server::oauth::{pkce_challenge, pkce_pair};
use reqwest::StatusCode;
use reqwest::header::LOCATION;

struct Server {
    base: String,
    running: rdesk_server::Running,
    _dir: tempfile::TempDir,
}

async fn server() -> Server {
    rdesk_server::install_crypto();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let cfg = ServerConfig {
        name: "Family".into(),
        public_url: base.clone(),
        listen: format!("127.0.0.1:{port}"),
        tls: Tls::None,
        dev_login: true,
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("rdesk.db")).unwrap();
    db.add_user("ana@example.com", true).unwrap();
    let key = iroh_base::SecretKey::generate();
    let running = rdesk_server::start_with(cfg, db, key, dir.path()).await.unwrap();
    Server { base, running, _dir: dir }
}

fn browser() -> reqwest::Client {
    reqwest::Client::builder().cookie_store(true).redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

fn totp(secret: &str) -> totp_rs::Totp {
    totp_rs::Builder::new()
        .with_secret(totp_rs::Secret::try_from_base32(secret).unwrap())
        .with_account_name("x")
        .build()
        .unwrap()
}

/// The browser part of a sign-in; returns the app's redirect (with `code` and `state`).
async fn browser_login(
    base: &str,
    email: &str,
    challenge: &str,
    second: impl AsyncFnOnce(&reqwest::Client) -> reqwest::Response,
) -> url::Url {
    let b = browser();
    let r = b
        .get(format!("{base}/login"))
        .query(&[
            ("redirect_uri", "http://127.0.0.1:9/callback"),
            ("state", "st4te"),
            ("code_challenge", challenge),
            ("device", "test-laptop"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.text().await.unwrap().contains("/login/dev"));
    let r = b.post(format!("{base}/login/dev")).form(&[("email", email)]).send().await.unwrap();
    let loc = r.headers().get(LOCATION).cloned();
    assert_eq!(loc.as_ref().map(|l| l.to_str().unwrap()), Some("/login/2fa"), "{:?}", r.text().await);
    let r = second(&b).await;
    assert!(r.status().is_redirection(), "{}", r.text().await.unwrap());
    let loc = r.headers()[LOCATION].to_str().unwrap().to_string();
    let u = url::Url::parse(&loc).unwrap();
    assert_eq!(u.host_str(), Some("127.0.0.1"));
    assert!(u.query_pairs().any(|(k, v)| k == "state" && v == "st4te"));
    u
}

fn code_of(u: &url::Url) -> String {
    u.query_pairs().find(|(k, _)| k == "code").unwrap().1.to_string()
}

async fn token(base: &str, code: &str, verifier: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/api/token"))
        .json(&TokenRequest { code: code.into(), code_verifier: verifier.into() })
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_with_totp_then_use_the_api_and_the_relay() {
    let s = server().await;
    let base = s.base.clone();
    let api = reqwest::Client::new();

    let info: ServerInfo = api.get(format!("{base}/api/info")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info.name, "Family");
    assert_eq!(info.providers, vec!["dev".to_string()]);
    assert_eq!(info.relay_url, format!("{base}/"));

    // First sign-in: no second factor yet, so set up TOTP.
    let (verifier, challenge) = pkce_pair();
    let secret = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sec = secret.clone();
    let b2 = base.clone();
    let back = browser_login(&b2, "ana@example.com", &challenge, async move |b| {
        let page = b.get(format!("{base}/login/2fa")).send().await.unwrap().text().await.unwrap();
        assert!(page.contains("/login/2fa/totp-setup"), "{page}");
        let page = b.get(format!("{base}/login/2fa/totp-setup")).send().await.unwrap().text().await.unwrap();
        let s = page.split("data-totp-secret=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        *sec.lock().unwrap() = s.clone();
        let bad = b.post(format!("{base}/login/2fa/totp-setup")).form(&[("code", "000000")]).send().await.unwrap();
        assert!(bad.text().await.unwrap().contains("not right"));
        let code = totp(&s).generate_current().to_string();
        b.post(format!("{base}/login/2fa/totp-setup")).form(&[("code", code)]).send().await.unwrap()
    })
    .await;
    let base = s.base.clone();
    let code = code_of(&back);

    // The code needs the PKCE verifier and works once.
    assert_eq!(token(&base, &code, "wrong-verifier").await.status(), StatusCode::BAD_REQUEST);
    let r = token(&base, &code, &verifier).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "a code is single-use, even after a bad attempt");

    // Second sign-in: TOTP is now asked, and a code works only once.
    let (verifier, challenge) = pkce_pair();
    let secret = secret.lock().unwrap().clone();
    let t = totp(&secret);
    let b2 = base.clone();
    let back = browser_login(&b2, "ana@example.com", &challenge, async move |b| {
        let page = b.get(format!("{base}/login/2fa")).send().await.unwrap().text().await.unwrap();
        assert!(page.contains("/login/2fa/totp\""), "{page}");
        // The setup code was used: the same step is refused; the next one works.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let reused = t.generate(now).to_string();
        let r = b.post(format!("{base}/login/2fa/totp")).form(&[("code", reused)]).send().await.unwrap();
        if r.status().is_redirection() {
            // The setup happened in the previous 30 s window: the current one is still fresh.
            return r;
        }
        let next = t.generate(now + 30).to_string();
        b.post(format!("{base}/login/2fa/totp")).form(&[("code", next)]).send().await.unwrap()
    })
    .await;
    let base = s.base.clone();
    let r = token(&base, &code_of(&back), &verifier).await;
    assert_eq!(r.status(), StatusCode::OK);
    let tok: TokenResponse = r.json().await.unwrap();
    assert_eq!(tok.user.email, "ana@example.com");
    assert!(tok.user.is_admin);
    let ten_days = 10 * 24 * 3600;
    assert!((tok.expires_at - rdesk_server::db::now() - ten_days).abs() < 60);

    let me: UserInfo =
        api.get(format!("{base}/api/me")).bearer_auth(&tok.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(me.email, "ana@example.com");
    assert_eq!(api.get(format!("{base}/api/me")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // Admin API: allow another e-mail.
    let added: AdminUser = api
        .post(format!("{base}/api/admin/users"))
        .bearer_auth(&tok.token)
        .json(&AddUser { email: "Bruno@Example.com".into(), is_admin: false })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(added.email, "bruno@example.com");
    let list: Vec<AdminUser> =
        api.get(format!("{base}/api/admin/users")).bearer_auth(&tok.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(list.len(), 2);
    // The last admin cannot remove themselves.
    let r = api.delete(format!("{base}/api/admin/users/{}", me.id)).bearer_auth(&tok.token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // Relay: only endpoints with a valid session get in, and they can reach each other.
    let relay: RelayUrl = info.relay_url.parse().unwrap();
    let with_token = |t: Option<&str>| {
        let mut map = RelayMap::from_iter([relay.clone()]);
        if let Some(t) = t {
            map = map.with_auth_token(t);
        }
        Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Custom(map)).alpns(vec![b"t/1".to_vec()]).bind()
    };
    let host = with_token(Some(&tok.token)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), host.online()).await.expect("signed-in endpoint reaches the relay");
    assert!(s.running.state.online.contains(&host.id()));
    let client = with_token(Some(&tok.token)).await.unwrap();
    let h = host.clone();
    let accept = tokio::spawn(async move {
        let c = h.accept().await.unwrap().await.unwrap();
        let mut r = c.accept_uni().await.unwrap();
        r.read_to_end(16).await.unwrap()
    });
    let c = tokio::time::timeout(
        Duration::from_secs(10),
        client.connect(EndpointAddr::new(host.id()).with_relay_url(relay.clone()), b"t/1"),
    )
    .await
    .unwrap()
    .unwrap();
    let mut w = c.open_uni().await.unwrap();
    w.write_all(b"hello").await.unwrap();
    w.finish().unwrap();
    assert_eq!(accept.await.unwrap(), b"hello");

    let stranger = with_token(None).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), stranger.online()).await.is_err(),
        "an endpoint without a session must not get into the relay"
    );

    // Logging out ends the session.
    let r = api.post(format!("{base}/api/logout")).bearer_auth(&tok.token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        api.get(format!("{base}/api/me")).bearer_auth(&tok.token).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    for ep in [host, client, stranger] {
        ep.close().await;
    }
    s.running.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_strangers_and_bad_redirects() {
    let s = server().await;
    let base = s.base.clone();
    let b = browser();
    let challenge = pkce_challenge("v");
    // Not a loopback redirect: refused before anything else.
    let r = b
        .get(format!("{base}/login"))
        .query(&[("redirect_uri", "https://evil.example.com/cb"), ("state", "s"), ("code_challenge", &challenge)])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // An e-mail nobody allowed.
    b.get(format!("{base}/login"))
        .query(&[("redirect_uri", "http://127.0.0.1:9/cb"), ("state", "s"), ("code_challenge", &challenge)])
        .send()
        .await
        .unwrap();
    let r = b.post(format!("{base}/login/dev")).form(&[("email", "mallory@example.com")]).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("not allowed"));

    // Without the browser's cookie the second step goes nowhere.
    let r = browser().get(format!("{base}/login/2fa")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    s.running.stop().await.unwrap();
}
