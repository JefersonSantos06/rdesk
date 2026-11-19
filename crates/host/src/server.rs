//! The listening host: accepts QUIC connections on the tailnet, authenticates
//! them, and runs one session at a time (a new session replaces the old one).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use rdesk_common::auth::{self, AuthKind, FailureLimiter, HostAuthConfig, OpaqueServer, ResumeStore};
use rdesk_common::config::{self, HostConfig, format_fingerprint};
use rdesk_common::transport::{self, HostIdentity, close};
use rdesk_proto::{ClientMsg, HostInfo, HostMsg, MAX_CONTROL_MSG, PROTOCOL_VERSION, expect_msg, write_msg};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::capture::Capturer;
use crate::control::{Ctl, HostState, Reporter, SessionInfo};
use crate::input::Injector;
use crate::session::{self, Shared};

#[derive(Default, Clone)]
pub struct RunOpts {
    /// Overrides `listen` from host.toml.
    pub listen: Vec<String>,
    pub port: Option<u16>,
    pub allow_non_tailnet: bool,
    pub fake_capture: bool,
    pub fake_input: bool,
}

struct Current {
    conn: quinn::Connection,
    task: tokio::task::JoinHandle<()>,
}

struct Server {
    reporter: Reporter,
    shared: Arc<Shared>,
    host_fp: [u8; 32],
    limiter: FailureLimiter,
    resume: ResumeStore,
    handshakes: Semaphore,
    current: Mutex<Option<Current>>,
}

pub fn load_auth_config(host_fp: [u8; 32]) -> Result<HostAuthConfig> {
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
    Ok(HostAuthConfig { authorized_keys, opaque, host_fp, host_name: rdesk_common::local_hostname() })
}

async fn listen_addrs(cfg: &HostConfig, reporter: &Reporter) -> Result<Vec<SocketAddr>> {
    let mut out = Vec::new();
    for l in &cfg.listen {
        if l.eq_ignore_ascii_case("tailscale") {
            let mut warned = false;
            let ips = loop {
                match transport::tailscale_ips().await {
                    Ok(ips) => break ips,
                    Err(e) => {
                        if !warned {
                            tracing::warn!("waiting for Tailscale: {e:#} (retrying every 2s)");
                            reporter.state(HostState::WaitingForTailscale(format!("{e:#}")));
                            warned = true;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            };
            out.extend(ips.into_iter().map(|ip| SocketAddr::new(ip, cfg.port)));
        } else if let Ok(sa) = l.parse::<SocketAddr>() {
            out.push(sa);
        } else if let Ok(ip) = l.parse::<IpAddr>() {
            out.push(SocketAddr::new(ip, cfg.port));
        } else {
            bail!("invalid listen address {l:?} (use an IP, IP:port or \"tailscale\")");
        }
    }
    if out.is_empty() {
        bail!("no listen addresses configured");
    }
    Ok(out)
}

#[cfg(windows)]
fn platform_init() {
    use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    // Without this Windows virtualizes coordinates and capture sizes on scaled displays.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(not(windows))]
fn platform_init() {}

/// Run the host until Ctrl+C (the `rdesk host run` command).
pub async fn run(opts: RunOpts) -> Result<()> {
    let (ctl, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
            let _ = ctl.send(Ctl::Stop);
        }
    });
    serve(opts, Reporter::silent(), rx).await
}

/// Load the config, listen, and serve clients until told to stop.
pub async fn serve(opts: RunOpts, reporter: Reporter, mut ctl: mpsc::UnboundedReceiver<Ctl>) -> Result<()> {
    platform_init();
    if cfg!(windows) && !opts.fake_input && !crate::elevation::is_elevated() {
        tracing::warn!("{}", crate::elevation::NOT_ELEVATED_HINT);
    }
    let mut cfg: HostConfig = config::load_toml(config::HOST_CONFIG)?;
    if !opts.listen.is_empty() {
        cfg.listen = opts.listen.clone();
    }
    if let Some(p) = opts.port {
        cfg.port = p;
    }
    cfg.allow_non_tailnet |= opts.allow_non_tailnet;
    let identity = HostIdentity::load()?;
    let host_fp = identity.fingerprint();
    reporter.fingerprint(host_fp);

    let auth_cfg = load_auth_config(host_fp)?;
    if auth_cfg.authorized_keys.is_empty() && auth_cfg.opaque.is_none() {
        bail!("no way to log in yet: set a password or authorize an SSH key");
    }

    let shared = Arc::new(Shared {
        capturer: Capturer::spawn(opts.fake_capture),
        injector: Injector::spawn(opts.fake_input),
        cfg: cfg.clone(),
    });
    match shared.capturer.monitors().await {
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
        Err(e) => reporter.event(format!("Cannot list monitors yet: {e:#}"), true),
    }

    let server = Arc::new(Server {
        shared,
        host_fp,
        limiter: FailureLimiter::default(),
        resume: ResumeStore::default(),
        handshakes: Semaphore::new(4),
        current: Mutex::new(None),
        reporter: reporter.clone(),
    });

    let addrs = tokio::select! {
        a = listen_addrs(&cfg, &reporter) => a?,
        // Nothing to disconnect yet; any command (or the UI going away) stops the wait.
        _ = ctl.recv() => return Ok(()),
    };
    let mut endpoints = Vec::new();
    for a in &addrs {
        let ep = transport::server_endpoint(*a, &identity, &cfg.transport).map_err(|e| {
            if format!("{e:#}").contains("10048") || format!("{e:#}").to_lowercase().contains("in use") {
                anyhow::anyhow!("{a} is already in use (is another rdesk host running?)")
            } else {
                e.context(format!("listening on {a}"))
            }
        })?;
        tracing::info!("listening on {}", ep.local_addr()?);
        endpoints.push(ep);
    }
    let bound: Vec<SocketAddr> = endpoints.iter().filter_map(|e| e.local_addr().ok()).collect();
    reporter.state(HostState::Listening(bound.clone()));
    reporter
        .event(format!("Host ready on {}", bound.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")), false);
    println!("rdesk host ready. Fingerprint: {}", format_fingerprint(&host_fp));

    for ep in &endpoints {
        let ep = ep.clone();
        let server = server.clone();
        let allow = cfg.allow_non_tailnet;
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let server = server.clone();
                tokio::spawn(async move {
                    if let Err(e) = server.handle(incoming, allow).await {
                        tracing::info!("connection ended: {e:#}");
                    }
                });
            }
        });
    }

    // Anything but Disconnect (Stop, or the controller going away) ends the host.
    while let Some(Ctl::Disconnect) = ctl.recv().await {
        if let Some(cur) = server.current.lock().await.take() {
            reporter.event("Session ended from this computer", false);
            cur.conn.close(close::KICKED, b"the host ended the session");
        }
    }
    if let Some(cur) = server.current.lock().await.take() {
        cur.conn.close(close::SHUTDOWN, b"host shutting down");
    }
    for ep in &endpoints {
        ep.close(close::SHUTDOWN, b"host shutting down");
    }
    for ep in endpoints {
        let _ = tokio::time::timeout(Duration::from_secs(2), ep.wait_idle()).await;
    }
    reporter.event("Host stopped", false);
    Ok(())
}

impl Server {
    async fn handle(self: Arc<Self>, incoming: quinn::Incoming, allow_non_tailnet: bool) -> Result<()> {
        let peer = incoming.remote_address();
        let ip = peer.ip().to_canonical();
        if !allow_non_tailnet && !transport::is_tailnet(ip) {
            incoming.refuse();
            bail!("refused {peer}: not a Tailscale address (set allow_non_tailnet to permit)");
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
            // Status check from a launcher: answer and hang up. Not a login attempt, so it
            // neither counts as a failure nor replaces the current session.
            let in_session = self.current.lock().await.as_ref().is_some_and(|c| c.conn.close_reason().is_none());
            let info = HostMsg::ProbeInfo {
                proto: PROTOCOL_VERSION,
                host_name: rdesk_common::local_hostname(),
                os: std::env::consts::OS.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                in_session,
            };
            write_msg(&mut w, &info).await?;
            let _ = w.finish();
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            return Ok(());
        }
        // Not a probe: hand the message back to the handshake as if it had not been read.
        let replay = rdesk_proto::encode(&first)?;

        let auth_cfg = load_auth_config(self.host_fp)?;
        let monitors = self.shared.capturer.monitors().await.unwrap_or_default();
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
                auth::host_handshake(&auth_cfg, &self.resume, &mut r, &mut w, ek, || info),
            )
            .await
        };
        let authed = match res {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                self.limiter.failure(ip);
                conn.close(close::AUTH_FAILED, b"authentication failed");
                self.reporter.event(format!("Failed login from {}: {e:#}", peer.ip()), true);
                bail!("{peer}: {e:#}");
            }
            Err(_) => {
                self.limiter.failure(ip);
                conn.close(close::AUTH_FAILED, b"authentication timed out");
                self.reporter.event(format!("Login from {} timed out", peer.ip()), true);
                bail!("{peer}: authentication timed out");
            }
        };
        self.limiter.success(ip);
        drop(_permit);
        transport::raise_stream_limits(&conn);

        if monitors.is_empty() {
            conn.close(close::PROTOCOL, b"host has no capturable monitor");
            bail!("no monitors to share (macOS: grant Screen Recording; Linux: needs X11)");
        }

        // One session at a time: the newest wins (the old link is probably half-dead).
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
        };
        let info = SessionInfo { client: authed.client_name.clone(), peer, method, since: SystemTime::now() };
        if !matches!(authed.kind, AuthKind::Resume) {
            self.reporter.event(format!("{} connected from {} ({})", info.client, peer.ip(), info.method), false);
        }
        self.reporter.session(Some(info.clone()));
        let shared = self.shared.clone();
        let reporter = self.reporter.clone();
        let c2 = conn.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = session::run(c2, w, r, authed, shared, monitors).await {
                tracing::warn!("session error: {e:#}");
            }
            reporter.session_ended(info.peer, info.since);
        });
        *cur = Some(Current { conn, task });
        Ok(())
    }
}
