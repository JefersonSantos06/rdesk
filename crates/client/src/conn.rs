//! Connection manager: connects, authenticates, runs the streams, and
//! reconnects automatically (resume token → no new 1Password prompt) until told to quit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use rdesk_common::account::{NotSignedIn, Session};
use rdesk_common::agent::Agent;
use rdesk_common::auth::{self, AskPassword, AuthFailed, ClientAuth, KeySource, PasswordCancelled, PasswordSource};
use rdesk_common::clipboard::ClipboardSync;
use rdesk_common::config::{AuthPref, HostEntry, NetworkConfig, TransportTuning};
use rdesk_common::files;
use rdesk_common::tiles::{Frame, TileRect};
use rdesk_common::transport::{self, Connection, ConnectionError, Endpoint, RecvStream, Role, SendStream, close};
use rdesk_proto::{
    ClientMsg, DirEntry, FrameAck, FrameHeader, HostInfo, HostMsg, InputEvent, MAX_CONTROL_MSG, MAX_VIDEO_MSG,
    MonitorInfo, SessionId, StreamOpen, Tile, expect_msg, read_msg, write_msg,
};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// What the user typed in the password dialog.
pub struct PasswordAnswer {
    pub password: Zeroizing<String>,
    /// Save it in the system credential store if the login succeeds.
    pub remember: bool,
}

impl std::fmt::Debug for PasswordAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordAnswer").field("remember", &self.remember).finish_non_exhaustive()
    }
}

/// Where the dialog's answer goes (answer once; `None` = cancelled).
#[derive(Clone, Debug)]
pub struct PasswordReply(Arc<Mutex<Option<tokio::sync::oneshot::Sender<Option<PasswordAnswer>>>>>);

impl PasswordReply {
    pub fn send(&self, answer: Option<PasswordAnswer>) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(answer);
        }
    }
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Status(String),
    /// Show a password dialog for `host`; `error` explains a previous failed attempt.
    PasswordNeeded {
        host: String,
        error: Option<String>,
        can_remember: bool,
        reply: PasswordReply,
    },
    /// Something the host wants the user to know (shown briefly, e.g. input is blocked).
    Notice(String),
    Connected(HostInfo),
    /// What this session may do (view only, files only…); sent right after `Connected`.
    Access(rdesk_proto::Access),
    Monitors {
        monitors: Vec<MonitorInfo>,
        active: u32,
    },
    FrameUpdated,
    DirListing {
        path: String,
        result: Result<Vec<DirEntry>, String>,
    },
    Transfer {
        id: u64,
        name: String,
        done: u64,
        total: u64,
        finished: Option<Result<String, String>>,
    },
    Stats {
        rtt_ms: u32,
        fps: f32,
        kbps: f32,
        /// "direct" or "relay".
        route: &'static str,
    },
    /// The connection will not be retried (auth failure, replaced by another client...).
    Fatal(String),
    Quit,
}

#[derive(Debug)]
pub enum Cmd {
    Input(InputEvent),
    SelectMonitor(u32),
    FullFrame,
    SetQuality { max_fps: u8, jpeg_quality: u8 },
    Push(PathBuf),
    Pull { remote: String, local_dir: PathBuf },
    ListDir(String),
    Quit,
}

/// The remote screen as last decoded, plus which parts changed since the UI looked.
pub struct View {
    pub frame: Frame,
    pub monitor: u32,
    pub dirty: Vec<TileRect>,
    pub full_dirty: bool,
    pub frames: u64,
    /// Set when a frame arrived and the UI has not looked yet (avoids flooding the event loop).
    pending: Arc<AtomicBool>,
    /// Newest frame id applied to each tile position (late streams must not overwrite newer tiles).
    tile_frame: HashMap<(u32, u32), u64>,
}

impl View {
    fn new() -> Self {
        Self {
            frame: Frame::new(1, 1),
            monitor: u32::MAX,
            dirty: Vec::new(),
            full_dirty: true,
            frames: 0,
            pending: Arc::new(AtomicBool::new(false)),
            tile_frame: HashMap::new(),
        }
    }
}

pub type SharedView = Arc<Mutex<View>>;

pub struct ConnectOpts {
    /// Display name (alias or address) used in prompts; also the key for a remembered password.
    pub name: String,
    /// Ask for passwords with [`UiEvent::PasswordNeeded`] (the viewer) instead of the terminal.
    pub ask_password_in_ui: bool,
    pub entry: HostEntry,
    /// The host ID to dial (also what authenticates the host).
    pub id: [u8; 32],
    /// The computer as the server knows it: tickets are asked for it, and its reported
    /// addresses are tried directly. `None` for an unregistered computer on the LAN.
    pub machine: Option<MachineTarget>,
    pub monitor: Option<u32>,
    pub transport: TransportTuning,
    pub net: NetworkConfig,
    pub clipboard: bool,
}

#[derive(Debug, Clone)]
pub struct MachineTarget {
    pub id: i64,
    pub addrs: Vec<String>,
}

/// A ticket and when it expires (Unix seconds).
pub type CachedTicket = Option<(Vec<u8>, i64)>;

/// A still-valid ticket for `machine` and this endpoint, from the cache or the server.
/// `Err((fatal, message))`.
pub async fn get_ticket(machine: i64, client: &[u8; 32], cache: &mut CachedTicket) -> Result<Vec<u8>, (bool, String)> {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    if let Some((t, exp)) = cache
        && *exp > now + 600
    {
        return Ok(t.clone());
    }
    let Some(session) = Session::load() else {
        return Err((true, "Not signed in to rdesk: sign in in the app (or `rdesk login`).".into()));
    };
    let api = session.api().map_err(|e| (true, format!("{e:#}")))?;
    match api.ticket(machine, client).await {
        Ok((bytes, r)) => {
            *cache = Some((bytes.clone(), r.expires_at));
            Ok(bytes)
        }
        Err(e) if e.downcast_ref::<NotSignedIn>().is_some() => {
            Err((true, format!("The server ended your sign-in ({e:#}): sign in again in the app.")))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            // No access (any more) or the computer is gone: retrying will not help.
            let fatal = msg.contains("do not have access") || msg.contains("no such computer");
            Err((fatal, msg))
        }
    }
}

pub struct Handle {
    pub cmd: mpsc::UnboundedSender<Cmd>,
    pub view: SharedView,
    pub thread: std::thread::JoinHandle<()>,
}

/// Start the connection manager on its own thread with its own tokio runtime.
pub fn spawn(opts: ConnectOpts, notify: impl Fn(UiEvent) + Send + Sync + 'static) -> Handle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let view = Arc::new(Mutex::new(View::new()));
    let v2 = view.clone();
    let thread = std::thread::Builder::new()
        .name("rdesk-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
            let notify: Arc<dyn Fn(UiEvent) + Send + Sync> = Arc::new(notify);
            rt.block_on(manager(opts, cmd_rx, v2, notify.clone()));
            notify(UiEvent::Quit);
        })
        .expect("spawn network thread");
    Handle { cmd: cmd_tx, view, thread }
}

enum End {
    Quit,
    Fatal(String),
    Retry(String),
}

pub type Notify = Arc<dyn Fn(UiEvent) + Send + Sync>;

type ClientEndpoint = Option<Endpoint>;

async fn manager(opts: ConnectOpts, cmd_rx: mpsc::UnboundedReceiver<Cmd>, view: SharedView, notify: Notify) {
    let mut endpoint: ClientEndpoint = None;
    reconnect_loop(opts, cmd_rx, view, notify, &mut endpoint).await;
    // `close()` only queues CONNECTION_CLOSE; without this the runtime shuts down before it is
    // sent and the host only notices the client is gone after its idle timeout.
    if let Some(ep) = endpoint {
        let _ = tokio::time::timeout(Duration::from_secs(3), ep.close()).await;
    }
}

async fn reconnect_loop(
    opts: ConnectOpts,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    view: SharedView,
    notify: Notify,
    endpoint: &mut ClientEndpoint,
) {
    let mut resume: Option<(SessionId, [u8; 32])> = None;
    let mut ticket: CachedTicket = None;
    let mut monitor = opts.monitor;
    let mut failures = 0u32;
    let mut password_hint: Option<String> = None;
    loop {
        notify(UiEvent::Status(if failures == 0 { "Connecting…".into() } else { "Reconnecting…".into() }));
        let started = Instant::now();
        let end = once(
            &opts,
            endpoint,
            &mut resume,
            &mut ticket,
            &mut monitor,
            &mut password_hint,
            &mut cmd_rx,
            &view,
            &notify,
        )
        .await;
        match end {
            End::Quit => return,
            End::Fatal(msg) => {
                notify(UiEvent::Fatal(msg));
                // Stay alive until the UI quits so it can show the message.
                while let Some(c) = cmd_rx.recv().await {
                    if matches!(c, Cmd::Quit) {
                        return;
                    }
                }
                return;
            }
            End::Retry(msg) => {
                // A session that lasted a while resets the backoff.
                if started.elapsed() > Duration::from_secs(10) {
                    failures = 0;
                }
                let delay = [0, 250, 500, 1000, 2000][(failures as usize).min(4)];
                failures += 1;
                tracing::info!("disconnected: {msg}; retrying in {delay} ms");
                notify(UiEvent::Status(format!("Disconnected ({msg}). Reconnecting…")));
                let sleep = tokio::time::sleep(Duration::from_millis(delay));
                tokio::pin!(sleep);
                // Drop input while offline, but honor Quit.
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        c = cmd_rx.recv() => match c {
                            None | Some(Cmd::Quit) => return,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

/// Which password ended up being used, so the outcome can be acted on (remember it,
/// or forget a stored one that no longer works).
pub enum PasswordUse {
    OnePassword,
    Keychain,
    Typed { password: Zeroizing<String>, remember: bool },
}

pub type PasswordUsed = Arc<Mutex<Option<PasswordUse>>>;

/// 1Password could not provide the password (not signed in, wrong reference...).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PasswordUnavailable(String);

/// Find the password lazily, only if key login did not work:
/// the entry's 1Password reference, then the system credential store, then ask the user
/// (dialog when `ui` is given, terminal otherwise).
pub fn password_asker(
    target: &str,
    entry: &HostEntry,
    used: PasswordUsed,
    ui: Option<(Notify, Option<String>)>,
) -> AskPassword {
    let reference = entry.password_ref.clone().filter(|r| !r.trim().is_empty());
    let target = target.to_string();
    Arc::new(move |host: String| {
        let (reference, target, used, ui) = (reference.clone(), target.clone(), used.clone(), ui.clone());
        Box::pin(async move {
            if let Some(r) = reference {
                let p = rdesk_common::agent::op_read(&r)
                    .await
                    .map_err(|e| PasswordUnavailable(format!("1Password: {e:#}")))?;
                *used.lock().unwrap() = Some(PasswordUse::OnePassword);
                return Ok(p);
            }
            let t = target.clone();
            if let Ok(Some(p)) = tokio::task::spawn_blocking(move || crate::secrets::get(&t)).await {
                *used.lock().unwrap() = Some(PasswordUse::Keychain);
                return Ok(p);
            }
            let (password, remember) = match ui {
                Some((notify, error)) => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    notify(UiEvent::PasswordNeeded {
                        host: if host.is_empty() { target.clone() } else { host },
                        error,
                        can_remember: crate::secrets::available(),
                        reply: PasswordReply(Arc::new(Mutex::new(Some(tx)))),
                    });
                    match rx.await {
                        Ok(Some(a)) => (a.password, a.remember),
                        _ => return Err(PasswordCancelled.into()),
                    }
                }
                None => {
                    let prompt = format!("rdesk password for {target}: ");
                    let p = tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt)).await??;
                    (Zeroizing::new(p), false)
                }
            };
            *used.lock().unwrap() = Some(PasswordUse::Typed { password: password.clone(), remember });
            Ok(password)
        })
    })
}

/// Build the credential set for a host entry: SSH key (file or agent) and/or a password.
pub async fn credentials(e: &HostEntry, resume: Option<(SessionId, [u8; 32])>, password: AskPassword) -> ClientAuth {
    let keys = if let Some(path) = &e.identity {
        match std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|t| ssh_key::PrivateKey::from_openssh(t).map_err(anyhow::Error::from))
        {
            Ok(k) if k.is_encrypted() => {
                tracing::error!("{} is passphrase-protected; use the 1Password agent instead", path.display());
                None
            }
            Ok(k) => Some(KeySource::Local(vec![k])),
            Err(err) => {
                tracing::error!("reading identity {}: {err:#}", path.display());
                None
            }
        }
    } else if matches!(e.auth, AuthPref::Auto | AuthPref::Agent) {
        match Agent::connect(e.agent_socket.as_deref()).await {
            Ok(a) => Some(KeySource::Agent(a)),
            Err(err) => {
                tracing::warn!("SSH agent unavailable: {err:#}");
                None
            }
        }
    } else {
        None
    };
    let password = matches!(e.auth, AuthPref::Auto | AuthPref::Password).then_some(PasswordSource::Ask(password));
    ClientAuth {
        client_name: rdesk_common::local_hostname(),
        resume,
        ticket: None,
        keys,
        key_filter: e.key.clone(),
        password,
    }
}

/// After a login attempt: remember a typed password, or drop a stored one that failed.
/// Returns the hint for the next password dialog when the password was the problem.
pub async fn settle_password(target: &str, used: Option<PasswordUse>, ok: bool) -> Result<Option<String>, String> {
    let t = target.to_string();
    match (used, ok) {
        (Some(PasswordUse::Typed { password, remember: true }), true) => {
            tokio::task::spawn_blocking(move || crate::secrets::set(&t, &password))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("{e:#}"))?;
            Ok(None)
        }
        (Some(PasswordUse::Keychain), false) => {
            let _ = tokio::task::spawn_blocking(move || crate::secrets::forget(&t)).await;
            Ok(Some("The saved password was not accepted. Type it again.".into()))
        }
        (Some(PasswordUse::Typed { .. }), false) => Ok(Some("Wrong password, try again.".into())),
        _ => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn once(
    opts: &ConnectOpts,
    endpoint: &mut ClientEndpoint,
    resume: &mut Option<(SessionId, [u8; 32])>,
    ticket: &mut CachedTicket,
    monitor: &mut Option<u32>,
    password_hint: &mut Option<String>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    view: &SharedView,
    notify: &Notify,
) -> End {
    // One endpoint for all reconnects: it keeps its relay connection and path state.
    // Registered computers also need a ticket from the server for this endpoint; the first
    // time, both happen at once (the endpoint's ID comes from a key made beforehand).
    let ticket_res = if let Some(ep) = endpoint.clone() {
        match &opts.machine {
            Some(m) => Some(get_ticket(m.id, ep.id().as_bytes(), ticket).await),
            None => None,
        }
    } else {
        let key = transport::SecretKey::generate();
        let client_id = *key.public().as_bytes();
        let bind = transport::bind(Some(key), &opts.net, &opts.transport, Role::Client);
        let fetch = async {
            match &opts.machine {
                Some(m) => Some(get_ticket(m.id, &client_id, ticket).await),
                None => None,
            }
        };
        let (ep, t) = tokio::join!(bind, fetch);
        match ep {
            Ok(ep) => *endpoint = Some(ep),
            Err(e) => return End::Retry(format!("{e:#}")),
        }
        t
    };
    let ep = endpoint.clone().unwrap();
    let ticket_bytes = match ticket_res {
        Some(Ok(t)) => Some(t),
        Some(Err((true, msg))) => return End::Fatal(msg),
        Some(Err((false, msg))) => return End::Retry(msg),
        None => None,
    };
    let mut direct: Vec<String> = opts.machine.as_ref().map(|m| m.addrs.clone()).unwrap_or_default();
    if !opts.entry.addr.trim().is_empty() {
        direct.insert(0, opts.entry.addr.clone());
    }
    let c = match transport::connect_with(&ep, opts.id, &direct, &opts.net, Duration::from_secs(8)).await {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{e:#}");
            // Retrying cannot help without a way to reach it, or when someone else answers.
            let fatal = msg.contains("no way to reach") || msg.starts_with(transport::WRONG_HOST_ID);
            return if fatal { End::Fatal(msg) } else { End::Retry(msg) };
        }
    };
    let conn = c.conn.clone();
    let (mut ctrl_w, mut ctrl_r) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => return End::Retry(e.to_string()),
    };
    notify(UiEvent::Status("Authenticating…".into()));
    let used: PasswordUsed = Arc::new(Mutex::new(None));
    let ui = opts.ask_password_in_ui.then(|| (notify.clone(), password_hint.take()));
    let asker = password_asker(&opts.name, &opts.entry, used.clone(), ui);
    let mut creds = credentials(&opts.entry, resume.take(), asker).await;
    creds.ticket = ticket_bytes;
    // Generous: the user may be typing a password or approving 1Password.
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        auth::client_handshake(&mut ctrl_r, &mut ctrl_w, c.ek, c.host_fingerprint, &mut creds),
    )
    .await;
    let use_ = used.lock().unwrap().take();
    let ok = match result {
        Ok(Ok(ok)) => {
            if let Err(e) = settle_password(&opts.name, use_, true).await {
                notify(UiEvent::Notice(format!("Could not remember the password: {e}")));
            }
            ok
        }
        Ok(Err(e)) => {
            conn.close(close::NORMAL, b"");
            if e.downcast_ref::<PasswordCancelled>().is_some() {
                return End::Fatal("Login cancelled.".into());
            }
            if e.downcast_ref::<PasswordUnavailable>().is_some() {
                return End::Fatal(format!("{e:#}"));
            }
            if e.downcast_ref::<AuthFailed>().is_some() {
                let retry =
                    opts.ask_password_in_ui && matches!(use_, Some(PasswordUse::Typed { .. } | PasswordUse::Keychain));
                let hint = settle_password(&opts.name, use_, false).await.ok().flatten();
                if retry {
                    // Ask again; the host's per-IP backoff keeps this from becoming a guessing loop.
                    *password_hint = hint;
                    return End::Retry("wrong password".into());
                }
                return End::Fatal(format!("{e:#}"));
            }
            if format!("{e:#}").contains("no usable authentication") {
                return End::Fatal(format!("{e:#}"));
            }
            return End::Retry(format!("{e:#}"));
        }
        Err(_) => {
            conn.close(close::NORMAL, b"");
            return End::Retry("authentication timed out".into());
        }
    };
    *resume = Some((ok.session_id, ok.resume_token));
    notify(UiEvent::Connected(ok.host.clone()));
    notify(UiEvent::Access(ok.access));
    notify(UiEvent::Status(format!("Connected to {}", ok.host.hostname)));

    // The clipboard goes both ways: only with full control.
    let clipboard = opts.clipboard && ok.access.input;
    match run_session(&conn, ctrl_w, ctrl_r, monitor, cmd_rx, view, notify, clipboard, ok.access.input).await {
        Ok(true) => {
            conn.close(close::NORMAL, b"bye");
            End::Quit
        }
        Ok(false) | Err(_) => match conn.close_reason() {
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::REPLACED => {
                End::Fatal("Another rdesk client connected to this host; this session was closed.".into())
            }
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::KICKED => {
                End::Fatal("The session was ended from the host computer.".into())
            }
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::SHUTDOWN => {
                End::Retry("host restarting".into())
            }
            Some(e) => End::Retry(e.to_string()),
            None => End::Retry("connection lost".into()),
        },
    }
}

/// Returns `Ok(true)` if the user quit, `Ok(false)` if the connection dropped.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    conn: &Connection,
    mut ctrl_w: SendStream,
    mut ctrl_r: RecvStream,
    monitor: &mut Option<u32>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    view: &SharedView,
    notify: &Notify,
    clipboard_on: bool,
    input_allowed: bool,
) -> Result<bool> {
    let mut input = conn.open_uni().await?;
    let _ = input.set_priority(20);
    write_msg(&mut input, &StreamOpen::Input).await?;
    write_msg(&mut input, &InputEvent::ReleaseAll).await?;
    let mut acks = conn.open_uni().await?;
    let _ = acks.set_priority(20);
    write_msg(&mut acks, &StreamOpen::Acks).await?;

    if let Some(m) = *monitor {
        write_msg(&mut ctrl_w, &ClientMsg::SelectMonitor { id: m }).await?;
    }

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<u64>();
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<String>();
    let clipboard = if clipboard_on { ClipboardSync::spawn(clip_tx) } else { None };
    let frame_pending = view.lock().unwrap().pending.clone();

    let mut tasks = tokio::task::JoinSet::<Result<()>>::new();

    // Control writer.
    tasks.spawn(async move {
        while let Some(m) = ctrl_rx.recv().await {
            write_msg(&mut ctrl_w, &m).await?;
        }
        Ok(())
    });

    // Ack writer.
    tasks.spawn(async move {
        while let Some(id) = ack_rx.recv().await {
            write_msg(&mut acks, &FrameAck { frame_id: id }).await?;
        }
        Ok(())
    });

    // Ping + stats.
    {
        let ctrl = ctrl_tx.clone();
        let conn = conn.clone();
        let notify = notify.clone();
        let view = view.clone();
        tasks.spawn(async move {
            let mut last_frames = 0u64;
            let mut last_bytes = conn.stats().udp_rx.bytes;
            let mut last_t = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if ctrl.send(ClientMsg::Ping { t_us: rdesk_common::now_us() }).is_err() {
                    return Ok(());
                }
                let frames = view.lock().unwrap().frames;
                let bytes = conn.stats().udp_rx.bytes;
                let dt = last_t.elapsed().as_secs_f32().max(0.001);
                let (rtt, route) = transport::path_info(&conn);
                notify(UiEvent::Stats {
                    rtt_ms: rtt.as_millis() as u32,
                    route: route.map_or("", transport::Route::label),
                    fps: (frames - last_frames) as f32 / dt,
                    kbps: (bytes - last_bytes) as f32 * 8.0 / dt / 1000.0,
                });
                last_frames = frames;
                last_bytes = bytes;
                last_t = Instant::now();
            }
        });
    }

    // Local clipboard → host.
    {
        let ctrl = ctrl_tx.clone();
        tasks.spawn(async move {
            while let Some(text) = clip_rx.recv().await {
                if ctrl.send(ClientMsg::Clipboard { text }).is_err() {
                    break;
                }
            }
            Ok(())
        });
    }

    // Control reader.
    let (mon_tx, mut mon_rx) = mpsc::unbounded_channel::<u32>();
    {
        let notify = notify.clone();
        tasks.spawn(async move {
            while let Some(msg) = read_msg::<_, HostMsg>(&mut ctrl_r, MAX_CONTROL_MSG).await? {
                match msg {
                    HostMsg::Pong { .. } => {}
                    HostMsg::Monitors { monitors, active } => {
                        let _ = mon_tx.send(active);
                        notify(UiEvent::Monitors { monitors, active });
                    }
                    HostMsg::Clipboard { text } => {
                        if let Some(c) = &clipboard {
                            c.set(text);
                        }
                    }
                    HostMsg::DirListing { path, result, .. } => notify(UiEvent::DirListing { path, result }),
                    HostMsg::Notice { text } => notify(UiEvent::Notice(text)),
                    HostMsg::Replaced => return Ok(()),
                    other => tracing::debug!("ignoring {other:?}"),
                }
            }
            Ok(())
        });
    }

    // Video frames: one uni stream each.
    {
        let conn = conn.clone();
        let view = view.clone();
        let notify = notify.clone();
        let pending = frame_pending.clone();
        tasks.spawn(async move {
            loop {
                let mut s = match conn.accept_uni().await {
                    Ok(s) => s,
                    Err(_) => return Ok(()),
                };
                let view = view.clone();
                let notify = notify.clone();
                let pending = pending.clone();
                let ack_tx = ack_tx.clone();
                tokio::spawn(async move {
                    match read_frame(&mut s, &view).await {
                        Ok(id) => {
                            let _ = ack_tx.send(id);
                        }
                        Err(e) => tracing::trace!("frame stream: {e:#}"),
                    }
                    if !pending.swap(true, Ordering::AcqRel) {
                        notify(UiEvent::FrameUpdated);
                    }
                });
            }
        });
    }
    let mut transfer_id = 0u64;
    let mut list_req = 0u32;
    let result = loop {
        tokio::select! {
            r = tasks.join_next() => {
                break match r {
                    Some(Ok(Err(e))) => Err(e),
                    _ => Ok(false),
                };
            }
            Some(active) = mon_rx.recv() => { *monitor = Some(active); }
            c = cmd_rx.recv() => {
                let Some(c) = c else { break Ok(true) };
                match c {
                    Cmd::Quit => break Ok(true),
                    // View-only sessions: the host would drop it anyway.
                    Cmd::Input(_) if !input_allowed => {}
                    Cmd::Input(ev) => {
                        if let Err(e) = write_msg(&mut input, &ev).await {
                            break Err(e.into());
                        }
                    }
                    Cmd::SelectMonitor(id) => {
                        *monitor = Some(id);
                        let _ = ctrl_tx.send(ClientMsg::SelectMonitor { id });
                    }
                    Cmd::FullFrame => { let _ = ctrl_tx.send(ClientMsg::RequestFullFrame); }
                    Cmd::SetQuality { max_fps, jpeg_quality } => {
                        let _ = ctrl_tx.send(ClientMsg::SetQuality { max_fps, jpeg_quality });
                    }
                    Cmd::ListDir(path) => {
                        list_req += 1;
                        let _ = ctrl_tx.send(ClientMsg::ListDir { req: list_req, path });
                    }
                    Cmd::Push(path) => {
                        transfer_id += 1;
                        spawn_push(conn.clone(), path, transfer_id, notify.clone());
                    }
                    Cmd::Pull { remote, local_dir } => {
                        transfer_id += 1;
                        spawn_pull(conn.clone(), remote, local_dir, transfer_id, notify.clone());
                    }
                }
            }
            _ = conn.closed() => break Ok(false),
        }
    };
    tasks.abort_all();
    result
}

async fn read_frame(s: &mut RecvStream, view: &SharedView) -> Result<u64> {
    let h: FrameHeader = expect_msg(s, MAX_VIDEO_MSG).await?;
    {
        let mut v = view.lock().unwrap();
        if v.monitor != h.monitor_id || v.frame.width != h.width || v.frame.height != h.height {
            v.frame = Frame::new(h.width, h.height);
            v.monitor = h.monitor_id;
            v.full_dirty = true;
            v.dirty.clear();
            v.tile_frame.clear();
        }
    }
    let mut scratch = Vec::new();
    while let Some(tile) = read_msg::<_, Tile>(s, MAX_VIDEO_MSG).await? {
        // Decode outside the lock.
        rdesk_common::tiles::decode_tile(&tile, &mut scratch)?;
        let rect = TileRect { x: tile.x, y: tile.y, w: tile.w as u32, h: tile.h as u32 };
        let mut v = view.lock().unwrap();
        if v.monitor != h.monitor_id {
            return Err(anyhow!("monitor switched mid-frame"));
        }
        let key = (tile.x, tile.y);
        if v.tile_frame.get(&key).is_some_and(|&f| f > h.frame_id) {
            continue; // a newer frame already painted this tile
        }
        v.tile_frame.insert(key, h.frame_id);
        v.frame.blit(rect, &scratch)?;
        if !v.full_dirty {
            v.dirty.push(rect);
        }
    }
    view.lock().unwrap().frames += 1;
    Ok(h.frame_id)
}

fn spawn_push(conn: Connection, path: PathBuf, id: u64, notify: Notify) {
    tokio::spawn(async move {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut last = Instant::now();
        let n2 = notify.clone();
        let nm = name.clone();
        let mut progress = move |done: u64, total: u64| {
            if last.elapsed() > Duration::from_millis(200) || done == total {
                last = Instant::now();
                n2(UiEvent::Transfer { id, name: nm.clone(), done, total, finished: None });
            }
        };
        let r = files::push(&conn, &path, &mut progress).await.map_err(|e| format!("{e:#}"));
        notify(UiEvent::Transfer { id, name, done: 0, total: 0, finished: Some(r) });
    });
}

fn spawn_pull(conn: Connection, remote: String, dir: PathBuf, id: u64, notify: Notify) {
    tokio::spawn(async move {
        let name = remote.replace('\\', "/").rsplit('/').next().unwrap_or(&remote).to_string();
        let mut last = Instant::now();
        let n2 = notify.clone();
        let nm = name.clone();
        let mut progress = move |done: u64, total: u64| {
            if last.elapsed() > Duration::from_millis(200) || done == total {
                last = Instant::now();
                n2(UiEvent::Transfer { id, name: nm.clone(), done, total, finished: None });
            }
        };
        let r = files::pull(&conn, &remote, &dir, &mut progress)
            .await
            .map(|p| p.display().to_string())
            .map_err(|e| format!("{e:#}"));
        notify(UiEvent::Transfer { id, name, done: 0, total: 0, finished: Some(r) });
    });
}

/// Take the dirty regions accumulated since the last call.
pub enum Update {
    None,
    Full { width: u32, height: u32, rgba: Vec<u8> },
    Partial { width: u32, height: u32, tiles: Vec<(TileRect, Vec<u8>)> },
}

pub fn take_updates(view: &SharedView) -> Update {
    let mut v = view.lock().unwrap();
    let (w, h) = (v.frame.width, v.frame.height);
    let total_tiles = (w.div_ceil(rdesk_proto::TILE) * h.div_ceil(rdesk_proto::TILE)) as usize;
    let upd = if v.full_dirty || v.dirty.len() > total_tiles / 2 {
        v.full_dirty = false;
        v.dirty.clear();
        Update::Full { width: w, height: h, rgba: v.frame.data.clone() }
    } else if v.dirty.is_empty() {
        Update::None
    } else {
        let mut rects = std::mem::take(&mut v.dirty);
        rects.sort_by_key(|r| (r.y, r.x));
        rects.dedup();
        let mut buf = Vec::new();
        let tiles = rects
            .into_iter()
            .map(|r| {
                v.frame.extract(r, &mut buf);
                (r, buf.clone())
            })
            .collect();
        Update::Partial { width: w, height: h, tiles }
    };
    v.pending.store(false, Ordering::Release);
    upd
}
