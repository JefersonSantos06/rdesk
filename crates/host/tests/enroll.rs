//! Adding computers without signing in on them: a host started with only the server and
//! the owner's e-mail waits for the approval and then comes online; enrollment keys add
//! computers at once; refusals, unknown e-mails and unregistered computers behave.

use std::time::Duration;

use rdesk_common::account::{Api, Registration};
use rdesk_proto::Access;
use rdesk_proto::api::{EnrollReply, NewEnrollKey, SetShares, ShareRole, Visibility, approval_code};
use rdesk_server::config::{ServerConfig, Tls};
use rdesk_server::db::Db;

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    for _ in 0..300 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_join_by_approval_or_key() {
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{port}");
    // SAFETY: the only test in this binary; set before any thread reads the environment.
    unsafe {
        std::env::set_var("RDESK_CONFIG_DIR", dir.path().join("host"));
        std::env::set_var("RDESK_SERVER", &server_url);
        std::env::set_var("RDESK_OWNER", "Bia@Example.com");
        std::env::set_var("RDESK_NAME", "Pi da sala");
        std::env::remove_var("RDESK_ENROLL_KEY");
    }

    let db = Db::open(&dir.path().join("rdesk.db")).unwrap();
    let ana = db.add_user("ana@example.com", true).unwrap();
    let bia = db.add_user("bia@example.com", false).unwrap();
    let (ana_token, _) = db.create_session(ana.id, "ana-laptop").unwrap();
    let (bia_token, _) = db.create_session(bia.id, "bia-laptop").unwrap();
    let cfg = ServerConfig {
        name: "Family".into(),
        public_url: server_url.clone(),
        listen: format!("127.0.0.1:{port}"),
        tls: Tls::None,
        ..Default::default()
    };
    let server = rdesk_server::start_with(cfg, db.clone(), iroh_base::SecretKey::generate(), dir.path()).await.unwrap();
    let ana_api = Api::new(&server_url, Some(ana_token)).unwrap();
    let bia_api = Api::new(&server_url, Some(bia_token)).unwrap();

    // A brand-new host (no identity yet, nobody signed in) asks to join.
    let host =
        tokio::spawn(rdesk_host::run(rdesk_host::RunOpts { port: Some(0), fake_capture: true, fake_input: true }));
    wait_for("Bia to see the request", || async { !bia_api.machine_requests().await.unwrap().is_empty() }).await;
    let requests = bia_api.machine_requests().await.unwrap();
    let host_id = rdesk_host::admin::fingerprint().unwrap();
    let req = &requests[0];
    assert_eq!(req.code, approval_code(&hex::encode(host_id)));
    assert_eq!(req.name, "Pi da sala");
    assert_eq!(req.owner.as_ref().unwrap().id, bia.id);
    assert!(ana_api.machine_requests().await.unwrap().is_empty(), "an admin does not decide for Bia");
    assert!(ana_api.approve_machine(req.id, None, Visibility::Private).await.is_err());
    assert!(Registration::load().is_none());

    // Bia approves: the waiting host hears it, registers and comes online on the relay.
    let m = bia_api.approve_machine(req.id, Some("Pi".into()), Visibility::Private).await.unwrap();
    assert!(m.is_owner);
    wait_for("the host to come online", || async {
        bia_api.machines().await.unwrap().iter().any(|x| x.id == m.id && x.online)
    })
    .await;
    let reg = Registration::load().unwrap();
    assert_eq!((reg.machine_id, reg.name.as_str()), (m.id, "Pi"));
    assert!(bia_api.machine_requests().await.unwrap().is_empty());
    host.abort();

    // An e-mail that is nobody's: answered "pending" like any other, but nothing is stored.
    let anon = Api::new(&server_url, None).unwrap();
    let stranger = iroh_base::SecretKey::generate();
    let r = anon.enroll(&stranger, "x", Some("nobody@example.com"), None, 0).await.unwrap();
    assert!(matches!(r, EnrollReply::Pending { .. }), "{r:?}");
    assert!(ana_api.machine_requests().await.unwrap().is_empty());
    assert!(bia_api.machine_requests().await.unwrap().is_empty());

    // No owner: for the system admins.
    let r = anon.enroll(&stranger, "Lab", None, None, 0).await.unwrap();
    assert!(matches!(r, EnrollReply::Pending { .. }));
    let pending = ana_api.machine_requests().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].owner.is_none());
    assert!(bia_api.machine_requests().await.unwrap().is_empty());
    // Ana refuses it: the host is told, also when it asks again.
    ana_api.reject_machine(pending[0].id).await.unwrap();
    assert!(matches!(anon.enroll(&stranger, "Lab", None, None, 0).await.unwrap(), EnrollReply::Rejected));
    assert!(ana_api.machine_requests().await.unwrap().is_empty());

    // A single-use key from Bia: added at once, shared as the key says.
    let created = bia_api
        .create_enroll_key(&NewEnrollKey {
            name: "Pis".into(),
            reusable: false,
            expires_in_days: 7,
            visibility: Visibility::Shared,
            shares: SetShares { users: vec![(ana.id, ShareRole::View)], groups: vec![] },
        })
        .await
        .unwrap();
    let pi2 = iroh_base::SecretKey::generate();
    let EnrollReply::Registered(r) = anon.enroll(&pi2, "Pi 2", None, Some(&created.key), 0).await.unwrap() else {
        panic!("the key did not register it")
    };
    assert_eq!(r.machine.owner.id, bia.id);
    assert_eq!(r.machine.visibility, Visibility::Shared);
    let seen = ana_api.machines().await.unwrap();
    assert_eq!(seen.iter().find(|x| x.id == r.machine.id).unwrap().access, Access::VIEW);
    // Its token works for the machine's own sync.
    assert_eq!(
        Api::new(&server_url, Some(r.token)).unwrap().agent_sync(&Default::default()).await.unwrap().machine.id,
        r.machine.id
    );
    // Used up.
    let e = anon.enroll(&iroh_base::SecretKey::generate(), "Pi 3", None, Some(&created.key), 0).await.unwrap_err();
    assert!(format!("{e:#}").contains("not valid"), "{e:#}");
    let keys = bia_api.enroll_keys().await.unwrap();
    assert_eq!((keys.len(), keys[0].uses), (1, 1));

    // Unregistered computers do not come back on their own with a (reusable) key.
    let lab = bia_api
        .create_enroll_key(&NewEnrollKey {
            name: "Lab".into(),
            reusable: true,
            expires_in_days: 1,
            visibility: Visibility::Private,
            shares: SetShares::default(),
        })
        .await
        .unwrap();
    bia_api.remove_machine(r.machine.id).await.unwrap();
    let again = anon.enroll(&pi2, "Pi 2", None, Some(&lab.key), 0).await.unwrap();
    assert!(matches!(again, EnrollReply::Pending { .. }), "{again:?}");
    let pending = bia_api.machine_requests().await.unwrap();
    assert_eq!(pending.len(), 1, "Bia decides about it again");
    // A revoked key is refused.
    bia_api.revoke_enroll_key(lab.info.id).await.unwrap();
    let e = anon.enroll(&iroh_base::SecretKey::generate(), "x", None, Some(&lab.key), 0).await.unwrap_err();
    assert!(format!("{e:#}").contains("not valid"), "{e:#}");

    server.stop().await.unwrap();
}
