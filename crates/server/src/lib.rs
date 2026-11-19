//! rdesk-server: the private server of an rdesk installation.
//!
//! * Accounts: only e-mails an admin allowed; sign-in with Google or GitHub plus a mandatory
//!   second factor (TOTP or passkey) on every sign-in; sessions last 10 days.
//! * The iroh relay, only for signed-in endpoints, proxied under `/relay` on the same port.
//! * HTTPS with Let's Encrypt (or plain HTTP for local tests).

pub mod access;
pub mod api;
pub mod certfile;
pub mod config;
pub mod db;
pub mod enroll;
pub mod groups;
pub mod login;
pub mod machines;
pub mod oauth;
pub mod pages;
pub mod relay;
pub mod selfupdate;
pub mod state;
pub mod updates;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{any, get, post};
use futures::StreamExt as _;
use rdesk_proto::api as paths;

use crate::config::{ServerConfig, Tls};
use crate::state::{Shared, State};

/// Use ring for every rustls user in the process (reqwest, the relay, ACME).
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn router(st: Shared) -> Router {
    Router::new()
        .route("/", get(home))
        .route(paths::PATH_INFO, get(api::info))
        .route(paths::PATH_TOKEN, post(api::token))
        .route(paths::PATH_ME, get(api::me))
        .route(paths::PATH_LOGOUT, post(api::logout))
        .route(paths::PATH_ADMIN_USERS, get(api::admin_users).post(api::admin_add_user))
        .route(
            &format!("{}/{{id}}", paths::PATH_ADMIN_USERS),
            axum::routing::patch(api::admin_update_user).delete(api::admin_remove_user),
        )
        .route(paths::PATH_MACHINES, get(machines::list).post(machines::register))
        .route(
            &format!("{}/{{id}}", paths::PATH_MACHINES),
            axum::routing::patch(machines::update).delete(machines::remove),
        )
        .route(&format!("{}/{{id}}/shares", paths::PATH_MACHINES), get(machines::get_shares).put(machines::set_shares))
        .route(&format!("{}/{{id}}/ticket", paths::PATH_MACHINES), post(machines::ticket))
        .route(
            &format!("{}/{{id}}/managers", paths::PATH_MACHINES),
            get(machines::get_managers).put(machines::set_managers),
        )
        .route(&format!("{}/{{id}}/history", paths::PATH_MACHINES), get(machines::history))
        .route(paths::PATH_ADMIN_MACHINES, get(machines::admin_list))
        .route(paths::PATH_AGENT_SYNC, post(machines::agent_sync))
        .route(paths::PATH_ADMIN_RELEASES, get(updates::list))
        .route(&format!("{}/check", paths::PATH_ADMIN_RELEASES), post(updates::check_now))
        .route(&format!("{}/{{version}}/release", paths::PATH_ADMIN_RELEASES), post(updates::release))
        .route(&format!("{}/{{version}}/server", paths::PATH_ADMIN_RELEASES), post(updates::update_server))
        .route(&format!("{}/{{id}}/ring", paths::PATH_ADMIN_MACHINES), axum::routing::put(updates::set_ring))
        .route(paths::PATH_UPDATE_LATEST, get(updates::latest))
        .route(paths::PATH_ENROLL, post(enroll::enroll))
        .route(paths::PATH_MACHINE_REQUESTS, get(enroll::list_requests))
        .route(&format!("{}/{{id}}/approve", paths::PATH_MACHINE_REQUESTS), post(enroll::approve))
        .route(&format!("{}/{{id}}/reject", paths::PATH_MACHINE_REQUESTS), post(enroll::reject))
        .route(paths::PATH_ENROLL_KEYS, get(enroll::list_keys).post(enroll::create_key))
        .route(&format!("{}/{{id}}", paths::PATH_ENROLL_KEYS), axum::routing::delete(enroll::revoke_key))
        .route(paths::PATH_USERS, get(machines::users))
        .route(paths::PATH_GROUPS, get(groups::list).post(groups::create))
        .route(&format!("{}/all", paths::PATH_GROUPS), get(groups::all))
        .route(&format!("{}/{{id}}", paths::PATH_GROUPS), axum::routing::patch(groups::rename).delete(groups::delete))
        .route(&format!("{}/{{id}}/members", paths::PATH_GROUPS), post(groups::add_member))
        .route(
            &format!("{}/{{id}}/members/{{user}}", paths::PATH_GROUPS),
            axum::routing::patch(groups::set_role).delete(groups::remove_member),
        )
        .route(paths::PATH_LOGIN, get(login::start))
        .route("/login/oauth/{provider}", get(login::oauth_go))
        .route("/login/oauth/{provider}/callback", get(login::oauth_back))
        .route("/login/dev", post(login::dev))
        .route("/login/2fa", get(login::second_factor))
        .route("/login/2fa/totp-setup", get(login::totp_setup_page).post(login::totp_setup_confirm))
        .route("/login/2fa/totp", post(login::totp_verify))
        .route("/login/2fa/passkey/register/start", post(login::passkey_register_start))
        .route("/login/2fa/passkey/register/finish", post(login::passkey_register_finish))
        .route("/login/2fa/passkey/auth/start", post(login::passkey_auth_start))
        .route("/login/2fa/passkey/auth/finish", post(login::passkey_auth_finish))
        .route("/relay", any(relay::proxy))
        .route("/ping", any(relay::proxy))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .with_state(st)
}

async fn home(axum::extract::State(st): axum::extract::State<Shared>) -> axum::response::Html<String> {
    pages::page(&st.cfg.name, "rdesk", "<h1>rdesk server</h1><p>Sign in from the rdesk app.</p>")
}

/// A running server (for `run` and tests).
pub struct Running {
    pub addr: SocketAddr,
    pub state: Shared,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<()>>,
    relay: relay::Relay,
}

impl Running {
    pub async fn stop(self) -> Result<()> {
        // Hosts waiting in their long poll get an answer now instead of holding the shutdown.
        self.state.wake_all();
        let _ = self.shutdown.send(());
        let r = self.task.await?;
        let _ = self.relay.server.shutdown().await;
        r
    }

    /// Wait until the server stops on its own (error).
    pub async fn wait(&mut self) -> Result<()> {
        (&mut self.task).await?
    }
}

/// Start everything for the data directory `data` (config, database, key).
pub async fn start(data: &Path) -> Result<Running> {
    let cfg = ServerConfig::load(data)?;
    let db = db::Db::open(&data.join(config::DB_FILE))?;
    let key = config::load_or_create_key(data)?;
    let prev = config::load_prev_key(data)?;
    start_full(cfg, db, key, prev, data).await
}

pub async fn start_with(cfg: ServerConfig, db: db::Db, key: iroh_base::SecretKey, data: &Path) -> Result<Running> {
    start_full(cfg, db, key, None, data).await
}

pub async fn start_full(
    cfg: ServerConfig,
    db: db::Db,
    key: iroh_base::SecretKey,
    prev_key: Option<iroh_base::PublicKey>,
    data: &Path,
) -> Result<Running> {
    install_crypto();
    let st = State::with_prev(cfg.clone(), db.clone(), key, prev_key)?;
    let listen: SocketAddr = cfg.listen.parse().with_context(|| format!("listen {:?}", cfg.listen))?;

    // Certificates first: the relay's QUIC address discovery reuses them.
    let acme = match cfg.tls {
        Tls::Acme => {
            let host = st.public.host_str().context("public_url has no host")?.to_string();
            let contact = format!("mailto:{}", cfg.acme_contact.clone().unwrap_or_default());
            Some(
                rustls_acme::AcmeConfig::new([host])
                    .contact_push(contact)
                    .cache(rustls_acme::caches::DirCache::new(data.join(config::ACME_DIR)))
                    .directory_lets_encrypt(!cfg.acme_staging)
                    .state(),
            )
        }
        Tls::None => None,
    };
    let resolver: Option<std::sync::Arc<dyn rustls::server::ResolvesServerCert>> =
        match (&acme, &cfg.tls_cert, &cfg.tls_key) {
            _ if !cfg.relay_quic() => None,
            (Some(a), _, _) => Some(a.resolver()),
            (None, Some(cert), Some(key)) => Some(std::sync::Arc::new(certfile::FileCert::new(cert, key)?)),
            _ => None,
        };
    // Behind a proxy the TCP side listens on a private address; QUIC is for everyone.
    let quic_ip = if acme.is_some() { listen.ip() } else { std::net::Ipv6Addr::UNSPECIFIED.into() };
    let quic = resolver.map(|r| {
        let tls = rustls::ServerConfig::builder().with_no_client_auth().with_cert_resolver(r);
        (SocketAddr::new(quic_ip, cfg.relay_quic_port), tls)
    });
    let relay = relay::spawn(db, st.online.clone(), quic).await?;
    *st.relay_http.lock().unwrap() = Some(relay.http);

    let _ = st.data_dir.set(data.to_path_buf());
    updates::spawn_checker(st.clone());

    // Housekeeping: forget abandoned sign-ins.
    {
        let st = st.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                st.sweep();
            }
        });
    }

    let app = router(st.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let (addr, task) = match acme {
        None => {
            let l = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
            let addr = l.local_addr()?;
            let task = tokio::spawn(async move {
                axum::serve(l, app)
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await?;
                Ok(())
            });
            (addr, task)
        }
        Some(mut state) => {
            let acceptor = state.axum_acceptor(state.default_rustls_config());
            tokio::spawn(async move {
                while let Some(ev) = state.next().await {
                    match ev {
                        Ok(ok) => tracing::info!("certificate: {ok:?}"),
                        Err(e) => tracing::warn!("certificate: {e:?}"),
                    }
                }
            });
            let handle = axum_server::Handle::new();
            let h2 = handle.clone();
            tokio::spawn(async move {
                let _ = rx.await;
                h2.graceful_shutdown(Some(Duration::from_secs(5)));
            });
            let srv = axum_server::bind(listen).acceptor(acceptor).handle(handle.clone());
            let task = tokio::spawn(async move {
                srv.serve(app).await?;
                Ok(())
            });
            let addr = handle.listening().await.context("the HTTPS server did not start")?;
            (addr, task)
        }
    };
    tracing::info!("rdesk-server on {addr} ({}), relay at {}", cfg.public_url, st.relay_url());
    Ok(Running { addr, state: st, shutdown: tx, task, relay })
}
