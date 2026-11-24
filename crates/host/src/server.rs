//! The listening host: accepts connections on its iroh endpoint (directly or through the
//! relay), authenticates them, and runs one session at a time (a new session replaces the
//! old one).

use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use rdesk_common::account::Registration;
use rdesk_common::auth::{self, AuthKind, FailureLimiter, HostAuthConfig, OpaqueServer, ResumeStore};
use rdesk_common::config::{self, DirectConfig, HostConfig, format_host_id};
use rdesk_common::ticket::{TicketPolicy, TicketUser};
use rdesk_common::transport::{self, Connection, HostIdentity, Incoming, IncomingAddr, Role, close};
use rdesk_common::{t, tf};
use rdesk_proto::{ClientMsg, HostInfo, HostMsg, MAX_CONTROL_MSG, expect_msg, write_msg};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::capture::Capturer;
use crate::control::{Ctl, HostState, Reporter, SessionInfo};
use crate::input::Injector;
use crate::session::{self, Shared};

#[derive(Default, Clone)]
pub struct RunOpts {
    /// Overrides `port` from host.toml (0 = any free port).
    pub port: Option<u16>,
    pub fake_capture: bool,
    pub fake_input: bool,
    /// Started by the Windows service: stop when standard input closes, answer the app on
    /// the control pipe.
    pub agent: bool,
}

/// Sessions open now (for the update watcher of `run`, which has no `Server`).
static SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Failed-login bookkeeping is per IP; connections through the relay share one bucket
/// (their client keys are throwaway, so per-key limits would be useless).
const RELAYED: IpAddr = IpAddr::V6(Ipv6Addr::UNSPECIFIED);

struct Current {
    conn: Connection,
    task: tokio::task::JoinHandle<()>,
    /// Who the server's ticket let in.
    user: Option<TicketUser>,
}

pub(crate) struct Server {
    reporter: Reporter,
    shared: Arc<Shared>,
    host_fp: [u8; 32],
    /// Registered with a server: tickets are required (and checked against revocations).
    pub(crate) ticket: Option<TicketPolicy>,
    require_local: bool,
    /// No screen to share ([`crate::capture::screenless`]; never with the test picture).
    screenless: bool,
    /// Direct access is on: who may log in with this computer's own password or key alone.
    direct: Option<DirectConfig>,
    /// Run by the Windows service: it installs released versions by itself.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) agent: bool,
    /// The networks this computer is on ("local network" includes them).
    local_nets: crate::localnet::LocalNets,
    /// Ask the sync loop to report now (a session started or ended).
    pub(crate) sync_now: tokio::sync::Notify,
    /// Sessions that started or ended, for the server's history (sent by the sync).
    pub(crate) records: std::sync::Mutex<Vec<rdesk_proto::api::AccessRecord>>,
    limiter: FailureLimiter,
    resume: ResumeStore,
    handshakes: Semaphore,
    current: Mutex<Option<Current>>,
}

impl Server {
    /// Direct access is on (the sync tells the server, so the owner sees it).
    pub(crate) fn direct_on(&self) -> bool {
        self.direct.is_some()
    }

    pub(crate) async fn in_session(&self) -> bool {
        self.current.lock().await.as_ref().is_some_and(|c| c.conn.close_reason().is_none())
    }

    /// End the session if its user lost access (after a sync brought revocations).
    pub(crate) async fn enforce_revocations(&self) {
        let Some(policy) = &self.ticket else { return };
        let mut cur = self.current.lock().await;
        if let Some(c) = cur.as_ref()
            && let Some(u) = &c.user
            && policy.is_revoked(u)
        {
            self.reporter.event(
                tf!(
                    "{}: access removed on the server, session ended",
                    "{}: acesso removido no servidor, sessão encerrada",
                    u.email
                ),
                true,
            );
            c.conn.close(close::KICKED, b"your access to this computer was removed");
            *cur = None;
        }
    }
}

/// The registration, if it is for this host identity.
pub fn registration_for(host_fp: &[u8; 32]) -> Option<Registration> {
    Registration::load().filter(|r| r.endpoint_id == hex_id(host_fp))
}

fn hex_id(id: &[u8; 32]) -> String {
    format_host_id(id)
}

fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn load_auth_config(
    host_fp: [u8; 32],
    ticket: Option<TicketPolicy>,
    require_local: bool,
) -> Result<HostAuthConfig> {
    let keys_path = config::config_path(config::AUTHORIZED_KEYS)?;
    let authorized_keys = match std::fs::read_to_string(&keys_path) {
        Ok(t) => auth::parse_authorized_keys(&t)?.into_iter().map(|k| k.key_data().clone()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).context("reading authorized_keys"),
    };
    let setup = std::fs::read(config::config_path(config::OPAQUE_SETUP)?).ok();
    let record = std::fs::read(config::config_path(config::OPAQUE_PASSWORD)?).ok();
    let opaque = match (setup, record) {
        (Some(s), Some(r)) => Some(OpaqueServer::from_bytes(&s, &r)?),
        _ => None,
    };
    Ok(HostAuthConfig {
        authorized_keys,
        opaque,
        host_fp,
        host_name: rdesk_common::local_hostname(),
        ticket,
        require_local,
        direct: false,
        // Set per connection, by the running host.
        screenless: false,
    })
}

#[cfg(windows)]
fn platform_init() {
    use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    // Without this Windows virtualizes coordinates and capture sizes on scaled displays.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(not(windows))]
fn platform_init() {}

/// Run by the service as SYSTEM: files are read and written as the signed-in user.
fn files_as_user() -> bool {
    #[cfg(windows)]
    {
        crate::win::proc::is_system()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The bar at the top of the screen while someone is connected.
#[cfg(windows)]
fn start_notice(reporter: &Reporter, ctl: mpsc::UnboundedSender<Ctl>) {
    crate::win::notice::start(move || {
        let _ = ctl.send(Ctl::Disconnect);
    });
    let reporter = reporter.clone();
    tokio::spawn(async move {
        let mut shown: Option<String> = None;
        loop {
            // A new terminal (or one as administrator) changes the text: the bar opens again.
            let text =
                reporter.snapshot().session.map(|s| tf!("{} connected{}", "{} conectado{}", s.client, s.extras()));
            if text != shown {
                crate::win::notice::show(text.clone());
                shown = text;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
}

/// Run the host until Ctrl+C (the `rdesk host run` command).
pub async fn run(opts: RunOpts) -> Result<()> {
    let (ctl, rx) = mpsc::unbounded_channel();
    if opts.agent {
        // Started in safe mode by a session's request: the next boot is a normal one.
        crate::restart::leave_safe_mode();
        // The service closes our standard input to stop us.
        let ctl = ctl.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while matches!(std::io::Read::read(&mut std::io::stdin(), &mut buf), Ok(n) if n > 0) {}
            tracing::info!("the service asked the host to stop");
            let _ = ctl.send(Ctl::Stop);
        });
    }
    // Updated underneath (a package, the Windows service): stop when nobody is connected,
    // and whatever started us (systemd, the service) starts the new program.
    if let Ok(exe) = std::env::current_exe() {
        let ctl = ctl.clone();
        let written = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let first = written(&exe);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if first.is_some() && written(&exe) != first && SESSIONS.load(std::sync::atomic::Ordering::Relaxed) == 0
                {
                    tracing::info!("this program was updated: stopping so the new version starts");
                    let _ = ctl.send(Ctl::Stop);
                    return;
                }
            }
        });
    }
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
            let _ = ctl.send(Ctl::Stop);
        }
    });
    serve(opts, Reporter::silent(), rx).await
}

/// Load the config, listen, and serve clients until told to stop.
pub async fn serve(opts: RunOpts, reporter: Reporter, mut outer: mpsc::UnboundedReceiver<Ctl>) -> Result<()> {
    platform_init();
    if cfg!(windows) && !opts.fake_input && !crate::elevation::is_elevated() {
        tracing::warn!("{}", crate::elevation::NOT_ELEVATED_HINT);
    }
    // Commands from the caller, the control pipe and the session notice.
    let (ctl_tx, mut ctl) = mpsc::unbounded_channel();
    {
        let tx = ctl_tx.clone();
        tokio::spawn(async move {
            while let Some(c) = outer.recv().await {
                if tx.send(c).is_err() {
                    return;
                }
            }
            // The controller went away: stop.
            let _ = tx.send(Ctl::Stop);
        });
    }
    if opts.agent {
        crate::ipc::serve(reporter.clone(), ctl_tx.clone());
    }
    #[cfg(windows)]
    if !opts.fake_capture {
        start_notice(&reporter, ctl_tx.clone());
    }
    // A fresh install (or a ready-made image's first boot) gets its own identity here.
    crate::admin::ensure_initialized()?;
    let mut cfg: HostConfig = config::load_toml(config::HOST_CONFIG)?;
    if let Some(p) = opts.port {
        cfg.port = p;
    }
    let identity = HostIdentity::load()?;
    let host_fp = identity.fingerprint();
    reporter.fingerprint(host_fp);
    // Registered: the server's relay with this computer's own token, and tickets required.
    let mut registration = registration_for(&host_fp);
    // Not registered but told which server to join: ask it, and wait for the approval.
    if registration.is_none()
        && let Some(e) = crate::enroll::settings(&cfg.enroll)
    {
        match crate::enroll::until_registered(&identity, &e, &reporter, &mut ctl).await? {
            Some(r) => registration = Some(r),
            None => {
                reporter.event(t!("Host stopped", "O host parou"), false);
                return Ok(());
            }
        }
    }
    let net = if registration.is_some() { config::host_network() } else { config::NetworkConfig::default() };
    let ticket = match &registration {
        Some(r) => Some(TicketPolicy {
            server_keys: std::sync::Arc::new(std::sync::RwLock::new(vec![r.server_key()?])),
            machine: host_fp,
            revoked: Default::default(),
        }),
        None => None,
    };
    reporter.registration(registration.as_ref().map(|r| Ok(tf!("{} on {}", "{} em {}", r.name, r.server_name))));

    let auth_cfg = load_auth_config(host_fp, ticket.clone(), cfg.require_local)?;
    let local = !auth_cfg.authorized_keys.is_empty() || auth_cfg.opaque.is_some();
    let direct = cfg.direct.is_on(ticket.is_some());
    if ticket.is_none() && !direct {
        bail!(t!(
            "direct access is off and this computer is not registered with a server: nobody could reach it",
            "o acesso direto está desligado e este computador não está cadastrado em um servidor: ninguém conseguiria acessá-lo"
        ));
    }
    if direct && ticket.is_some() {
        reporter.event(
            tf!(
                "Direct access is on: this computer's own password or key also open it from {}",
                "Acesso direto ligado: a senha ou chave deste computador também o abrem a partir de {}",
                cfg.direct.from.join(t!(" and ", " e "))
            ),
            false,
        );
    }
    if ticket.is_none() && !local {
        bail!(t!(
            "no way to log in yet: add this computer to your rdesk server (`rdesk host enroll`, or register it              in the app), or set a password",
            "ainda não há como entrar: adicione este computador ao seu servidor rdesk (`rdesk host enroll`, ou              cadastre-o no app), ou defina uma senha"
        ));
    }
    if ticket.is_some() && cfg.require_local && !local {
        bail!(t!(
            "the extra lock is on but this computer has no password or SSH key: set one, or turn it off",
            "o cadeado extra está ligado, mas este computador não tem senha nem chave SSH: defina uma ou desligue-o"
        ));
    }

    let shared = Arc::new(Shared {
        capturer: Capturer::spawn(opts.fake_capture),
        injector: Injector::spawn(opts.fake_input),
        cfg: cfg.clone(),
        files_as_user: files_as_user(),
        reporter: reporter.clone(),
    });
    match shared.capturer.monitors().await {
        Ok(m) if m.is_empty() && !opts.fake_capture && !crate::capture::screenless() => reporter.event(
            t!(
                "No monitor right now: sessions get the terminal, files and tunnels until one is connected",
                "Nenhuma tela agora: as sessões têm terminal, arquivos e túneis até uma ser ligada"
            ),
            true,
        ),
        Ok(m) => {
            for d in &m {
                tracing::info!(
                    "monitor {} \"{}\" {}x{} at {},{} scale {}{}",
                    d.info.id,
                    d.info.name,
                    d.info.width,
                    d.info.height,
                    d.info.x,
                    d.info.y,
                    d.info.scale,
                    if d.info.primary { " (primary)" } else { "" }
                );
            }
        }
        Err(e) => {
            reporter.event(tf!("Cannot list monitors yet: {e:#}", "Ainda não é possível listar as telas: {e:#}"), true)
        }
    }

    let server = Arc::new(Server {
        shared,
        host_fp,
        ticket,
        require_local: cfg.require_local,
        screenless: !opts.fake_capture && crate::capture::screenless(),
        direct: direct.then(|| cfg.direct.clone()),
        agent: opts.agent,
        local_nets: if direct { crate::localnet::LocalNets::watch() } else { Default::default() },
        sync_now: tokio::sync::Notify::new(),
        records: std::sync::Mutex::new(Vec::new()),
        limiter: FailureLimiter::default(),
        resume: ResumeStore::default(),
        handshakes: Semaphore::new(4),
        current: Mutex::new(None),
        reporter: reporter.clone(),
    });

    let ep = transport::bind(Some(identity.secret_key()), &net, &cfg.transport, Role::Host { port: cfg.port }).await?;
    // Announced on the local network (opt-in): others connect with the host ID alone.
    if server.direct.as_ref().is_some_and(|d| d.mdns) {
        match transport::local_discovery(&ep, true) {
            Ok(_) => {
                reporter.event(t!("Announced on the local network (mDNS)", "Anunciado na rede local (mDNS)"), false)
            }
            Err(e) => reporter.event(
                tf!("Could not announce on the local network: {e:#}", "Não foi possível anunciar na rede local: {e:#}"),
                true,
            ),
        }
    }
    // The system asks the person at the computer for what the host still needs.
    #[cfg(target_os = "macos")]
    crate::mac_permissions::request();
    let bound = ep.bound_sockets();
    tracing::info!("host {} listening on {:?}", format_host_id(&host_fp), bound);
    reporter.state(HostState::Listening(bound.clone()));
    let port = bound.first().map(|a| a.port()).unwrap_or_default();
    reporter.event(
        tf!("Host ready (direct connections on UDP {port})", "Host pronto (conexões diretas na porta UDP {port})"),
        false,
    );
    println!("rdesk host ready. ID: {}", format_host_id(&host_fp));
    {
        let (ep, net, reporter) = (ep.clone(), net.clone(), reporter.clone());
        tokio::spawn(async move {
            let relay = net.relay.clone().unwrap_or_default();
            match transport::wait_relay(&ep, &net, Duration::from_secs(10)).await {
                Ok(true) => {
                    reporter.relay(Some(Ok(relay.clone())));
                    reporter.event(tf!("Connected to the relay {relay}", "Conectado ao relay {relay}"), false);
                }
                Ok(false) => {
                    reporter.relay(None);
                    reporter.event(
                        t!(
                            "Not registered with an rdesk server: reachable only by address (LAN)",
                            "Não cadastrado em um servidor rdesk: acessível só pelo endereço (rede local)"
                        ),
                        true,
                    );
                }
                Err(e) => {
                    reporter.relay(Some(Err(format!("{e:#}"))));
                    reporter.event(
                        tf!(
                            "Relay {relay}: {e:#} (direct connections still work)",
                            "Relay {relay}: {e:#} (conexões diretas continuam funcionando)"
                        ),
                        true,
                    );
                }
            }
        });
    }
    if let Some(reg) = registration {
        tokio::spawn(crate::sync::run(reg, ep.clone(), server.clone(), reporter.clone()));
    }
    {
        let (ep, server) = (ep.clone(), server.clone());
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let server = server.clone();
                tokio::spawn(async move {
                    if let Err(e) = server.handle(incoming).await {
                        tracing::info!("connection ended: {e:#}");
                    }
                });
            }
        });
    }

    // Anything but Disconnect (Stop, or the controller going away) ends the host.
    while let Some(Ctl::Disconnect) = ctl.recv().await {
        if let Some(cur) = server.current.lock().await.take() {
            reporter.event(t!("Session ended from this computer", "Sessão encerrada neste computador"), false);
            cur.conn.close(close::KICKED, b"the host ended the session");
        }
    }
    if let Some(cur) = server.current.lock().await.take() {
        cur.conn.close(close::SHUTDOWN, b"host shutting down");
    }
    // Closes what is left gracefully and releases the sockets.
    let _ = tokio::time::timeout(Duration::from_secs(3), ep.close()).await;
    reporter.event(t!("Host stopped", "O host parou"), false);
    Ok(())
}

impl Server {
    /// The monitors to offer a session. None now (no monitor connected, the display server
    /// gone or not answering) means no screen for that session: the terminal, files and
    /// tunnels still work, and the next session looks again.
    async fn screens(&self) -> Vec<crate::capture::MonitorDesc> {
        if self.screenless {
            return Vec::new();
        }
        let why = match self.shared.capturer.monitors().await {
            Ok(m) if !m.is_empty() => return m,
            Ok(_) => "no monitor connected".to_string(),
            Err(e) => format!("{e:#}"),
        };
        tracing::info!("no screen to show now ({why}): the session gets the terminal, files and tunnels");
        Vec::new()
    }

    async fn handle(self: Arc<Self>, incoming: Incoming) -> Result<()> {
        let (ip, peer, straight) = match incoming.remote_addr() {
            IncomingAddr::Ip(sa) => (sa.ip().to_canonical(), sa.ip().to_canonical().to_string(), true),
            IncomingAddr::Relay { endpoint_id, .. } => (RELAYED, format!("relay ({})", endpoint_id.fmt_short()), false),
            other => (RELAYED, format!("{other:?}"), false),
        };
        // Its own password or key alone: only straight from the local network or Tailscale.
        let direct = straight
            && self
                .direct
                .as_ref()
                .is_some_and(|d| d.allows(ip) || (d.from.iter().any(|c| c == "lan") && self.local_nets.contains(ip)));
        if self.ticket.is_none() && !direct {
            incoming.refuse();
            bail!("refused {peer}: without a server, only the local network or Tailscale may connect");
        }
        if let Err(wait) = self.limiter.check(ip) {
            incoming.refuse();
            bail!("refused {peer}: too many failed logins, retry in {}s", wait.as_secs() + 1);
        }
        let Ok(_permit) = self.handshakes.try_acquire() else {
            incoming.refuse();
            bail!("refused {peer}: too many concurrent handshakes");
        };

        let conn = incoming.await.with_context(|| format!("handshake with {peer}"))?;
        let ek = transport::exporter(&conn)?;
        let (mut w, mut r) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
            .await
            .context("client did not open the control stream")??;

        let first: ClientMsg = tokio::time::timeout(Duration::from_secs(10), expect_msg(&mut r, MAX_CONTROL_MSG))
            .await
            .context("client sent nothing on the control stream")??;
        if matches!(first, ClientMsg::Probe) {
            // Nothing about this computer is told before login (status comes from the server).
            let reason = "status checks are not answered; ask the rdesk server".to_string();
            write_msg(&mut w, &HostMsg::AuthFail { reason }).await?;
            let _ = w.finish();
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            return Ok(());
        }
        // Not a probe: hand the message back to the handshake as if it had not been read.
        let replay = rdesk_proto::encode(&first)?;

        let mut auth_cfg = load_auth_config(self.host_fp, self.ticket.clone(), self.require_local)?;
        auth_cfg.direct = direct;
        let monitors = self.screens().await;
        // No screen now: this session gets the terminal, files and tunnels, not a refusal.
        auth_cfg.screenless = monitors.is_empty();
        let peer_id = *conn.remote_id().as_bytes();
        let initial = session::pick_initial(&monitors).map(|m| m.info.id).unwrap_or(0);
        let info = HostInfo {
            hostname: rdesk_common::local_hostname(),
            os: std::env::consts::OS.to_string(),
            monitors: session::infos(&monitors),
            active_monitor: initial,
        };
        // 60 s leaves time for a 1Password approval / Touch ID prompt on the client.
        let res = {
            let mut r = std::io::Cursor::new(replay).chain(&mut r);
            tokio::time::timeout(
                Duration::from_secs(60),
                auth::host_handshake(&auth_cfg, &self.resume, &mut r, &mut w, ek, peer_id, || info),
            )
            .await
        };
        let authed = match res {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                self.limiter.failure(ip);
                // Perhaps the server rotated its key: ask it now rather than at the next poll.
                if format!("{e:#}").contains("not signed by this computer's rdesk server") {
                    self.sync_now.notify_one();
                }
                // Let the reason (already sent) reach the client before closing; it hangs up
                // as soon as it reads it.
                let _ = w.finish();
                let _ = tokio::time::timeout(Duration::from_secs(1), conn.closed()).await;
                conn.close(close::AUTH_FAILED, b"authentication failed");
                self.reporter.event(tf!("Failed login from {peer}: {e:#}", "Falha de login de {peer}: {e:#}"), true);
                bail!("{peer}: {e:#}");
            }
            Err(_) => {
                self.limiter.failure(ip);
                conn.close(close::AUTH_FAILED, b"authentication timed out");
                self.reporter.event(tf!("Login from {peer} timed out", "O login de {peer} expirou"), true);
                bail!("{peer}: authentication timed out");
            }
        };
        self.limiter.success(ip);
        drop(_permit);
        transport::raise_stream_limits(&conn);

        // One session at a time. The same person's newer session wins (their old link is
        // probably half-dead); someone else's waits until it ends instead of throwing them out.
        // Logins without the server (this computer's own password or key) are all the same
        // person, as before.
        let who = match &authed.user {
            Some(u) => u.label(),
            None => authed.client_name.clone(),
        };
        {
            let cur = self.current.lock().await;
            if let Some(old) = cur.as_ref()
                && !old.task.is_finished()
                && let (Some(a), Some(b)) = (&old.user, &authed.user)
                && a.user_id != b.user_id
            {
                let reason = tf!("in use by {}", "em uso por {}", a.label());
                self.reporter.event(
                    tf!(
                        "{who} tried to connect while {} is connected",
                        "{who} tentou conectar enquanto {} está conectado",
                        a.label()
                    ),
                    false,
                );
                conn.close(close::BUSY, reason.as_bytes());
                bail!("{peer}: busy ({reason})");
            }
        }
        // Asking the person at the computer: not for a reconnect.
        if !matches!(authed.kind, AuthKind::Resume) {
            let what = authed.access.label();
            if !crate::consent::ask(&self.shared.cfg.consent, &who, &what).await {
                conn.close(close::DENIED, b"declined at the computer");
                self.reporter.event(
                    tf!("Access by {who} declined at the computer", "Acesso de {who} recusado no computador"),
                    true,
                );
                bail!("{peer}: declined at the computer");
            }
        }
        let mut cur = self.current.lock().await;
        if let Some(old) = cur.take() {
            tracing::info!("new session from {peer} replaces the previous one");
            old.conn.close(close::REPLACED, b"replaced by a newer session");
            let _ = tokio::time::timeout(Duration::from_secs(2), old.task).await;
        }
        let method = match &authed.kind {
            AuthKind::PublicKey { fingerprint } => format!("key {fingerprint}"),
            AuthKind::Password => "password".to_string(),
            AuthKind::Resume => "reconnect".to_string(),
            AuthKind::Ticket { local: None } => "server ticket".to_string(),
            AuthKind::Ticket { local: Some(l) } => format!("server ticket + {l}"),
        };
        let user = authed.user.clone();
        let access = authed.access;
        let session_hex = hex_bytes(&authed.session_id);
        let record = |ended: Option<i64>, started: i64| rdesk_proto::api::AccessRecord {
            session: session_hex.clone(),
            user_id: user.as_ref().map(|u| u.user_id),
            who: user.as_ref().map(|u| u.email.clone()).unwrap_or_else(|| authed.client_name.clone()),
            client: authed.client_name.clone(),
            method: method.clone(),
            access: access.label(),
            started_at: started,
            ended_at: ended,
        };
        let started = unix_now();
        if !matches!(authed.kind, AuthKind::Resume) {
            self.records.lock().unwrap().push(record(None, started));
        }
        let end_record = record(Some(0), started);
        let client = match &user {
            Some(u) => format!("{} ({})", u.label(), authed.client_name),
            None => authed.client_name.clone(),
        };
        let info = SessionInfo { client, peer, method, since: SystemTime::now(), terminals: 0, admin_terminals: 0 };
        if !matches!(authed.kind, AuthKind::Resume) {
            self.reporter.event(
                tf!("{} connected from {} ({})", "{} conectado de {} ({})", info.client, info.peer, info.method),
                false,
            );
        }
        self.reporter.session(Some(info.clone()));
        SESSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let shared = self.shared.clone();
        let reporter = self.reporter.clone();
        let c2 = conn.clone();
        let me = self.clone();
        if monitors.is_empty() && !self.screenless {
            // A computer that has one, without it now: say why the picture is missing.
            let text = t!(
                "This computer shows no screen right now (no monitor connected, or its desktop is not running): \
                 the terminal, files and tunnels still work.",
                "Este computador está sem tela agora (nenhum monitor ligado, ou a área de trabalho não está \
                 rodando): o terminal, os arquivos e os túneis continuam funcionando."
            );
            let _ = write_msg(&mut w, &HostMsg::Notice { text: text.into() }).await;
        }
        let task = tokio::spawn(async move {
            if let Err(e) = session::run(c2, w, r, authed, shared, monitors).await {
                tracing::warn!("session error: {e:#}");
            }
            reporter.session_ended(&info.peer, info.since);
            SESSIONS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            me.records
                .lock()
                .unwrap()
                .push(rdesk_proto::api::AccessRecord { ended_at: Some(unix_now()), ..end_record });
            me.sync_now.notify_one();
        });
        *cur = Some(Current { conn, task, user });
        self.sync_now.notify_one();
        Ok(())
    }
}
