//! Whoever is signed in on a registered computer sees it (and who it is shared with), even
//! when someone else registered it and shared nothing with them; with a "manage" proof
//! (the app asks the system's administrator first) they manage it like its managers do.
//! Who it is shared with is also shown to everyone it is shared with.

use rdesk_common::account::{Api, Session};
use rdesk_proto::api::{Here, ShareRole, UpdateMachine, UserInfo, Visibility};
use rdesk_server::config::{ServerConfig, Tls};
use rdesk_server::db::Db;

fn err_text<T: std::fmt::Debug>(r: anyhow::Result<T>) -> String {
    format!("{:#}", r.unwrap_err())
}

#[tokio::test(flavor = "multi_thread")]
async fn at_the_computer_you_see_it_and_may_manage_it() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe { std::env::set_var("RDESK_CONFIG_DIR", dir.path().join("host")) };

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{port}");
    let db = Db::open(&dir.path().join("rdesk.db")).unwrap();
    let ana = db.add_user("ana@example.com", false).unwrap();
    let bia = db.add_user("bia@example.com", false).unwrap();
    let caio = db.add_user("caio@example.com", false).unwrap();
    let (ana_token, ana_exp) = db.create_session(ana.id, "ana-laptop").unwrap();
    let (bia_token, _) = db.create_session(bia.id, "bia-laptop").unwrap();
    let (caio_token, _) = db.create_session(caio.id, "caio-laptop").unwrap();
    let cfg = ServerConfig {
        name: "Family".into(),
        public_url: server_url.clone(),
        listen: format!("127.0.0.1:{port}"),
        tls: Tls::None,
        ..Default::default()
    };
    let _server =
        rdesk_server::start_with(cfg, db.clone(), iroh_base::SecretKey::generate(), dir.path()).await.unwrap();

    // Ana registers this computer, private. Then Bia signs in on it.
    rdesk_host::admin::ensure_initialized().unwrap();
    let ana_session = Session {
        server: server_url.clone(),
        token: ana_token,
        expires_at: ana_exp,
        user: UserInfo { id: ana.id, email: ana.email.clone(), name: String::new(), is_admin: false },
        relay_url: format!("{server_url}/"),
        server_name: "Family".into(),
    };
    let reg = rdesk_host::admin::register(&ana_session, "Sala", Visibility::Private).await.unwrap();
    let id = reg.machine_id;
    let bia_api = || Api::new(&server_url, Some(bia_token.clone())).unwrap();
    let proof = |user: i64, scope: Here| async move { rdesk_host::admin::here_proof(user, scope).await.unwrap() };

    // Elsewhere, Bia does not see it at all.
    assert!(bia_api().machines().await.unwrap().is_empty());

    // At the computer she does: whose it is, that she cannot reach it, who it is shared with.
    let here = bia_api().with_here(proof(bia.id, Here::View).await);
    let list = here.machines().await.unwrap();
    assert_eq!(list.len(), 1);
    let m = &list[0];
    assert_eq!((m.id, m.here, m.can_access, m.can_manage, m.is_owner), (id, Some(Here::View), false, false, false));
    assert_eq!(m.owner.email, "ana@example.com");
    assert!(here.shares(id).await.unwrap().users.is_empty());
    assert!(here.managers(id).await.unwrap().users.is_empty());
    // Seeing is not managing.
    assert!(err_text(here.set_shares(id, vec![(bia.id, ShareRole::Control)], vec![]).await).contains("managers"));
    let rename = UpdateMachine { name: Some("Minha".into()), visibility: None };
    assert!(err_text(here.update_machine(id, &rename).await).contains("managers"));
    assert!(err_text(here.history(id).await).contains("history"));
    assert!(err_text(here.remove_machine(id).await).contains("registered"));

    // A proof is for one person: Ana's proof does nothing for Bia.
    let borrowed = bia_api().with_here(proof(ana.id, Here::Manage).await);
    assert!(borrowed.machines().await.unwrap().is_empty());
    assert!(borrowed.set_shares(id, vec![(bia.id, ShareRole::Control)], vec![]).await.is_err());

    // Managing at the computer: share it (with herself and Caio), rename it, see its history.
    let manage = bia_api().with_here(proof(bia.id, Here::Manage).await);
    let shares =
        manage.set_shares(id, vec![(bia.id, ShareRole::Control), (caio.id, ShareRole::View)], vec![]).await.unwrap();
    assert_eq!(shares.users.len(), 2);
    let m = manage
        .update_machine(id, &UpdateMachine { name: Some("Minha".into()), visibility: Some(Visibility::Shared) })
        .await
        .unwrap();
    assert_eq!((m.name.as_str(), m.here, m.can_manage), ("Minha", Some(Here::Manage), false));
    manage.history(id).await.unwrap();
    // Choosing the managers stays with the owner.
    assert!(manage.set_managers(id, vec![bia.id]).await.is_err());

    // Now shared with them: anywhere, both see who else it is shared with (and its managers).
    let bia_far = bia_api().machines().await.unwrap();
    assert_eq!((bia_far.len(), bia_far[0].here, bia_far[0].can_access), (1, None, true));
    let caio_api = Api::new(&server_url, Some(caio_token)).unwrap();
    let seen = caio_api.shares(id).await.unwrap();
    assert!(seen.users.iter().any(|(u, r)| u.email == "bia@example.com" && *r == ShareRole::Control));
    caio_api.managers(id).await.unwrap();
    // ...but only see it: changing it stays with its managers.
    assert!(caio_api.set_shares(id, vec![(caio.id, ShareRole::Control)], vec![]).await.is_err());

    // Removing it from the server, at the computer.
    manage.remove_machine(id).await.unwrap();
    let ana_api = ana_session.api().unwrap();
    assert!(ana_api.machines().await.unwrap().is_empty());
}
