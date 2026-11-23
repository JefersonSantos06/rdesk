//! A group's computers (`GET /api/groups/{id}/machines`) against a real server on loopback:
//! its members see what is shared with it, a private computer's share only its manager (it
//! lets nobody in), people outside the group nothing; the groups list counts them.

use rdesk_proto::api::{Group, GroupMachine, GroupRole, ShareRole, Visibility};
use rdesk_server::config::{ServerConfig, Tls};
use rdesk_server::db::Db;
use reqwest::StatusCode;

#[tokio::test]
async fn members_see_the_computers_shared_with_their_group() {
    rdesk_server::install_crypto();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let cfg = ServerConfig {
        name: "Family".into(),
        public_url: base.clone(),
        listen: format!("127.0.0.1:{port}"),
        tls: Tls::None,
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("rdesk.db")).unwrap();
    // Ana owns the computers; Bia is in the group; Caio is not; Root administers the server.
    let ana = db.add_user("ana@example.com", false).unwrap();
    let bia = db.add_user("bia@example.com", false).unwrap();
    let caio = db.add_user("caio@example.com", false).unwrap();
    let root = db.add_user("root@example.com", true).unwrap();
    let token = |id| db.create_session(id, "test").unwrap().0;
    let (ana_t, bia_t, caio_t, root_t) = (token(ana.id), token(bia.id), token(caio.id), token(root.id));

    let group = db.create_group("Oficina", ana.id).unwrap();
    db.set_member(group, bia.id, GroupRole::Member).unwrap();
    let (sala, _) = db.register_machine(ana.id, &"ab".repeat(32), "PC da sala", "windows", Visibility::Shared).unwrap();
    let (pi, _) = db.register_machine(ana.id, &"cd".repeat(32), "Raspberry Pi", "linux", Visibility::Private).unwrap();
    let (alone, _) = db.register_machine(ana.id, &"ef".repeat(32), "Notebook", "macos", Visibility::Shared).unwrap();
    db.set_shares(sala.id, &[], &[(group, ShareRole::Control)]).unwrap();
    // Shared with the group, but private: the share lets nobody in.
    db.set_shares(pi.id, &[], &[(group, ShareRole::Terminal)]).unwrap();
    // Shared with Bia herself, not through the group.
    db.set_shares(alone.id, &[(bia.id, ShareRole::View)], &[]).unwrap();

    let _server = rdesk_server::start_with(cfg, db, iroh_base::SecretKey::generate(), dir.path()).await.unwrap();
    let http = reqwest::Client::new();
    let get = async |path: &str, t: &str| http.get(format!("{base}{path}")).bearer_auth(t).send().await.unwrap();
    let machines = async |t: &str| -> Vec<GroupMachine> {
        let r = get(&format!("/api/groups/{group}/machines"), t).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    };
    let names = |l: &[GroupMachine]| l.iter().map(|m| (m.machine.name.clone(), m.role, m.active)).collect::<Vec<_>>();

    // Bia: only what the group reaches now, as she may use it.
    let seen = machines(&bia_t).await;
    assert_eq!(names(&seen), [("PC da sala".to_string(), ShareRole::Control, true)]);
    assert!(seen[0].machine.can_access && !seen[0].machine.is_owner);

    // Ana manages both: the private one too, marked as not letting the group in.
    let seen = machines(&ana_t).await;
    assert_eq!(
        names(&seen),
        [
            ("PC da sala".to_string(), ShareRole::Control, true),
            ("Raspberry Pi".to_string(), ShareRole::Terminal, false)
        ]
    );

    // A system admin sees what its managers see; someone outside the group nothing.
    assert_eq!(machines(&root_t).await.len(), 2);
    assert_eq!(get(&format!("/api/groups/{group}/machines"), &caio_t).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(get("/api/groups/999/machines", &bia_t).await.status(), StatusCode::NOT_FOUND);

    // The list counts the same.
    let count = async |t: &str| -> u32 {
        let groups: Vec<Group> = get("/api/groups", t).await.json().await.unwrap();
        groups.iter().find(|g| g.id == group).unwrap().machine_count
    };
    assert_eq!(count(&bia_t).await, 1);
    assert_eq!(count(&ana_t).await, 2);
}
