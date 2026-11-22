//! Several accounts on one device (a personal server and two companies' ones, different
//! e-mails), one in use at a time; and the move from the single `session.json`.

use rdesk_common::account::{ACCOUNTS_FILE, SESSION_FILE, Session};
use rdesk_common::config;
use rdesk_proto::api::UserInfo;

fn session(server: &str, name: &str, user: i64, email: &str, days: i64) -> Session {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    Session {
        server: server.into(),
        token: format!("token-{email}"),
        expires_at: now + days * 86_400,
        user: UserInfo { id: user, email: email.into(), name: String::new(), is_admin: false },
        relay_url: format!("{server}/"),
        server_name: name.into(),
    }
}

#[test]
fn accounts_on_several_servers() {
    let dir = tempfile::tempdir().unwrap();
    config::set_config_dir(dir.path().to_path_buf());

    // An earlier version's one sign-in becomes the first account, in use.
    let home = session("https://rdesk.home.example", "Home", 1, "me@gmail.example", 5);
    std::fs::write(dir.path().join(SESSION_FILE), serde_json::to_string(&home).unwrap()).unwrap();
    assert_eq!(Session::load().as_ref(), Some(&home));
    assert!(!dir.path().join(SESSION_FILE).exists(), "moved, not copied");
    assert!(dir.path().join(ACCOUNTS_FILE).exists());

    // Signing in elsewhere adds an account and uses it; the first one stays signed in.
    let x = session("https://rdesk.x.example", "Company X", 7, "me@x.example", 10);
    x.save().unwrap();
    assert_eq!(Session::load().as_ref(), Some(&x));
    assert_eq!(Session::load_account(&home.key()).as_ref(), Some(&home));
    // A second user on the same server is another account.
    let x2 = session("https://rdesk.x.example", "Company X", 8, "admin@x.example", 10);
    x2.save().unwrap();
    assert_eq!(Session::all(), vec![home.clone(), x.clone(), x2.clone()]);

    // Signing in again to the same account replaces it, in its place.
    let mut x_again = x.clone();
    x_again.token = "fresh".into();
    x_again.save().unwrap();
    assert_eq!(Session::all().len(), 3);
    assert_eq!(Session::all()[1].token, "fresh");

    // Changing is explicit; the network is the account's own.
    Session::activate(&home.key()).unwrap();
    assert_eq!(Session::load().as_ref(), Some(&home));
    assert_eq!(config::load_network().token.as_deref(), Some("token-me@gmail.example"));
    assert!(Session::activate("https://nowhere#1").is_err());

    // Named by number, server, server name or e-mail; ambiguous names say so.
    assert_eq!(Session::find("1").unwrap(), home);
    assert_eq!(Session::find("rdesk.home.example").unwrap(), home);
    assert_eq!(Session::find("admin@x.example").unwrap(), x2);
    assert!(Session::find("Company X").is_err(), "two accounts on that server");
    assert!(Session::find("9").is_err());

    // The server ends one: it stays listed, to sign in to again, but is not usable.
    Session::end(&x2.key()).unwrap();
    assert!(Session::load_account(&x2.key()).is_none());
    assert_eq!(Session::all().len(), 3);
    Session::activate(&x2.key()).unwrap();
    assert!(Session::load().is_none(), "the account in use must sign in again");
    assert_eq!(Session::active_key(), Some(x2.key()));

    // Signing out forgets the account in use; none is in use until one is chosen.
    Session::clear().unwrap();
    assert!(Session::load().is_none());
    assert_eq!(Session::active_key(), None);
    assert_eq!(Session::all(), vec![home.clone(), x_again.clone()]);
    Session::remove(&home.key()).unwrap();
    assert_eq!(Session::all(), vec![x_again.clone()]);

    // A process pinned to an account keeps it whatever is in use.
    Session::activate(&x.key()).unwrap();
    Session::pin(&x.key());
    let y = session("https://rdesk.y.example", "Company Y", 3, "other@y.example", 10);
    y.save().unwrap();
    assert_eq!(Session::load().map(|s| s.key()), Some(x.key()));
}
