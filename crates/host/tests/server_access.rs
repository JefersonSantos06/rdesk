//! Phase 3 end to end: a real rdesk-server, a registered host, and clients holding tickets.
//! The owner registers the computer; someone else gets in only once it is shared with
//! them, through the relay with only the host ID; unsharing ends their session at once.

use std::time::Duration;

use rdesk_common::account::{Api, Session};
use rdesk_common::auth::{self, ClientAuth};
use rdesk_common::config::{self, NetworkConfig, TransportTuning};
use rdesk_common::transport::{self, ConnectionError, Role, close};
use rdesk_proto::Access;
use rdesk_proto::api::{ShareRole, UpdateMachine, UserInfo, Visibility};
use rdesk_server::config::{ServerConfig, Tls};
use rdesk_server::db::Db;

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    for _ in 0..150 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sharing_controls_access_and_unsharing_kicks() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path().join("host")) };

    // The server, with two people.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{port}");
    let db = Db::open(&dir.path().join("rdesk.db")).unwrap();
    let ana = db.add_user("ana@example.com", true).unwrap();
    let bia = db.add_user("bia@example.com", false).unwrap();
    let (ana_token, ana_exp) = db.create_session(ana.id, "ana-laptop").unwrap();
    let (bia_token, _) = db.create_session(bia.id, "bia-laptop").unwrap();
    let cfg = ServerConfig {
        name: "Family".into(),
        public_url: server_url.clone(),
        listen: format!("127.0.0.1:{port}"),
        tls: Tls::None,
        ..Default::default()
    };
    let old_key = iroh_base::SecretKey::generate();
    let server = rdesk_server::start_with(cfg.clone(), db.clone(), old_key.clone(), dir.path()).await.unwrap();
    let relay = format!("{server_url}/");

    // Ana registers this computer (private) and starts its host: no password, no keys.
    let id = rdesk_host::admin::ensure_initialized().unwrap();
    let ana_session = Session {
        server: server_url.clone(),
        token: ana_token.clone(),
        expires_at: ana_exp,
        user: UserInfo { id: ana.id, email: ana.email.clone(), name: String::new(), is_admin: true },
        relay_url: relay.clone(),
        server_name: "Family".into(),
    };
    let reg = rdesk_host::admin::register(&ana_session, "Sala", Visibility::Private).await.unwrap();
    assert_eq!(config::host_network().token.as_deref(), Some(reg.token.as_str()));
    let host =
        tokio::spawn(rdesk_host::run(rdesk_host::RunOpts { port: Some(0), fake_capture: true, fake_input: true }));

    let ana_api = Api::new(&server_url, Some(ana_token)).unwrap();
    let bia_api = Api::new(&server_url, Some(bia_token.clone())).unwrap();
    wait_for("the host on the relay", || async {
        ana_api.machines().await.unwrap().iter().any(|m| m.id == reg.machine_id && m.online)
    })
    .await;

    // Bia's side: her session's relay token, a fresh endpoint.
    let net = NetworkConfig { relay: Some(relay.clone()), token: Some(bia_token) };
    let ep = transport::bind(None, &net, &TransportTuning::default(), Role::Client).await.unwrap();

    // Private: Bia does not even see it, and gets no ticket.
    assert!(bia_api.machines().await.unwrap().is_empty());
    let err = bia_api.ticket(reg.machine_id, ep.id().as_bytes()).await.unwrap_err();
    assert!(format!("{err:#}").contains("do not have access"), "{err:#}");

    // Without a ticket the host refuses (nothing else works on a registered host).
    let c = transport::connect(&ep, id, None, &net, Duration::from_secs(10)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut no_ticket = ClientAuth {
        client_name: "bia".into(),
        resume: None,
        ticket: None,
        keys: None,
        key_filter: None,
        password: None,
    };
    let e = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut no_ticket).await.unwrap_err();
    assert!(format!("{e:#}").contains("rdesk server"), "{e:#}");
    c.conn.close(close::NORMAL, b"");

    // Ana shares it with Bia.
    ana_api.set_shares(reg.machine_id, vec![(bia.id, ShareRole::Control)], vec![]).await.unwrap();
    ana_api
        .update_machine(reg.machine_id, &UpdateMachine { name: None, visibility: Some(Visibility::Shared) })
        .await
        .unwrap();
    let seen = bia_api.machines().await.unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].can_access && !seen[0].is_owner);

    // Ticket for this endpoint, then in through the relay with only the host ID.
    let (ticket, _) = bia_api.ticket(reg.machine_id, ep.id().as_bytes()).await.unwrap();
    let c = transport::connect(&ep, id, None, &net, Duration::from_secs(10)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut creds = ClientAuth { ticket: Some(ticket.clone()), ..no_ticket };
    let ok = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await.unwrap();
    assert_eq!(ok.access, Access::FULL);
    wait_for("the host to report the session", || async {
        bia_api.machines().await.unwrap().first().is_some_and(|m| m.in_session)
    })
    .await;

    // Ana unshares: the host hears it right away and ends Bia's session.
    ana_api.set_shares(reg.machine_id, vec![], vec![]).await.unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(15), c.conn.closed()).await.expect("session not ended");
    match reason {
        ConnectionError::ApplicationClosed(a) => assert_eq!(a.error_code, close::KICKED),
        other => panic!("unexpected close: {other:?}"),
    }
    // And her old ticket no longer works.
    let c = transport::connect(&ep, id, None, &net, Duration::from_secs(10)).await.unwrap();
    let (mut w, mut r) = c.conn.open_bi().await.unwrap();
    let mut old = ClientAuth {
        client_name: "bia".into(),
        resume: None,
        ticket: Some(ticket),
        keys: None,
        key_filter: None,
        password: None,
    };
    let e = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut old).await.unwrap_err();
    assert!(format!("{e:#}").contains("removed"), "{e:#}");

    // The host reported the session (and its end) to the server's history.
    wait_for("the history", || async {
        ana_api
            .history(reg.machine_id)
            .await
            .unwrap()
            .iter()
            .any(|h| h.who == "bia@example.com" && h.ended_at.is_some())
    })
    .await;
    assert!(bia_api.history(reg.machine_id).await.is_err(), "only managers see the history");

    // View only: the host is told, and says so in its answer.
    ana_api.set_shares(reg.machine_id, vec![(bia.id, ShareRole::View)], vec![]).await.unwrap();
    let login = async |api: &Api| -> anyhow::Result<rdesk_proto::AuthOk> {
        let (ticket, _) = api.ticket(reg.machine_id, ep.id().as_bytes()).await?;
        let c = transport::connect(&ep, id, None, &net, Duration::from_secs(10)).await?;
        let (mut w, mut r) = c.conn.open_bi().await?;
        let mut creds = ClientAuth {
            client_name: "bia".into(),
            resume: None,
            ticket: Some(ticket),
            keys: None,
            key_filter: None,
            password: None,
        };
        let ok = auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await;
        c.conn.close(close::NORMAL, b"");
        ok
    };
    assert_eq!(login(&bia_api).await.unwrap().access, Access::VIEW);

    // A co-manager may share it too (but not choose managers).
    ana_api.set_managers(reg.machine_id, vec![bia.id]).await.unwrap();
    let as_bia = &bia_api.machines().await.unwrap()[0];
    assert!(as_bia.can_manage && !as_bia.is_owner && as_bia.access == Access::FULL);
    bia_api.set_shares(reg.machine_id, vec![], vec![]).await.unwrap();
    assert!(bia_api.set_managers(reg.machine_id, vec![]).await.is_err());
    ana_api.set_managers(reg.machine_id, vec![]).await.unwrap();

    // Key rotation: the server restarts with a new key; the host learns it by its sync and
    // accepts the new tickets without being registered again.
    ana_api.set_shares(reg.machine_id, vec![(bia.id, ShareRole::Control)], vec![]).await.unwrap();
    server.stop().await.unwrap();
    let new_key = iroh_base::SecretKey::generate();
    let server = rdesk_server::start_full(cfg, db, new_key.clone(), Some(old_key.public()), dir.path()).await.unwrap();
    // The host picks the new key up with its next sync (it retries a few seconds after the
    // server went away).
    let new_hex = hex::encode(new_key.public().as_bytes());
    wait_for("the host to learn the new key", || async {
        rdesk_common::account::Registration::load().is_some_and(|r| r.server_key == new_hex)
    })
    .await;
    let mut ok = None;
    for _ in 0..20 {
        match login(&bia_api).await {
            Ok(a) => {
                ok = Some(a);
                break;
            }
            // The relays reconnect after the restart; only connecting can fail meanwhile.
            Err(e) => {
                eprintln!("after rotation: {e:#}");
                tokio::time::sleep(Duration::from_millis(500)).await
            }
        }
    }
    assert_eq!(ok.expect("a ticket from the new key never worked").access, Access::FULL);
    let saved = rdesk_common::account::Registration::load().unwrap();
    assert_eq!(saved.server_key, hex::encode(new_key.public().as_bytes()), "the new key is remembered");

    // Unregistering: the host's sync notices and forgets the registration.
    rdesk_host::admin::unregister(&ana_session).await.unwrap();
    assert!(rdesk_common::account::Registration::load().is_none());

    host.abort();
    ep.close().await;
    server.stop().await.unwrap();
}
