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
use rdesk_common::{t, tf};
use rdesk_proto::{
    ClientMsg, CursorShape, DirEntry, EXT_REV, ExtClient, ExtHost, Feature, FrameAck, FrameHeader, HostInfo, HostMsg,
    InputEvent, MAX_CONTROL_MSG, MAX_SMALL_MSG, MAX_VIDEO_MSG, MonitorInfo, SessionId, StreamOpen, Tile, Tile2,
    TileData, VideoCaps, expect_msg, read_msg, write_msg,
};

use crate::video2::{DTile, Decoded, Pix, V2};
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
    /// A reply that goes nowhere (the ui gallery).
    #[doc(hidden)]
    pub fn test() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub fn send(&self, answer: Option<PasswordAnswer>) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(answer);
        }
    }
}

/// Where the connection stands, in a word (the window title, the connecting card).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Reconnecting,
    Authenticating,
    Connected {
        host: String,
    },
    /// The connection dropped (why, as people read it) and is being tried again.
    Retrying(String),
    /// Ended for good (the window shows why).
    Disconnected,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Connecting => f.write_str(t!("Connecting…", "Conectando…")),
            Status::Reconnecting => f.write_str(t!("Reconnecting…", "Reconectando…")),
            Status::Authenticating => f.write_str(t!("Authenticating…", "Autenticando…")),
            Status::Connected { host } => f.write_str(&tf!("Connected to {host}", "Conectado a {host}")),
            Status::Retrying(reason) => {
                f.write_str(&tf!("Disconnected ({reason}). Reconnecting…", "Desconectado ({reason}). Reconectando…"))
            }
            Status::Disconnected => f.write_str(t!("Disconnected", "Desconectado")),
        }
    }
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Status(Status),
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
    /// The connection will not be retried (auth failure, replaced by another client...)
    /// unless the UI asks with [`Cmd::Reconnect`].
    Fatal(String),
    /// The connection dropped; trying again (`attempt` counts from 1 since it was last up).
    Reconnecting {
        reason: String,
        attempt: u32,
    },
    /// The app asked this session window to come forward (the window is already open).
    Raise(Raise),
    /// A terminal opened (or why not); `admin` as asked in [`Cmd::OpenTerminal`].
    Terminal {
        admin: bool,
        result: Result<crate::terminal::TermSlot, String>,
    },
    /// What the host's extensions stream offers (empty: an older host, none of them).
    /// Only when [`Extensions::wanted`].
    Extensions(Vec<Feature>),
    /// Where the host's pointer is on the shown monitor (normalized 0..=65535), with
    /// [`Extensions::cursor`].
    Cursor {
        x: u16,
        y: u16,
        visible: bool,
    },
    /// Text the host copied, with [`Extensions::clipboard_in_app`] (the app puts it on its
    /// own clipboard).
    Clipboard(String),
    /// What automatic quality chose ([`Cmd::AutoQuality`]).
    Quality {
        fps: u8,
        jpeg_quality: u8,
        scale_pct: u8,
    },
    /// The host's pointer image changed ([`Extensions::cursor_shape`]): `image` the first time
    /// an id is seen (keep it), then only the id; id 0 means hidden.
    CursorShape {
        id: u64,
        image: Option<CursorShape>,
    },
    /// Something to redraw (terminal output).
    Wake,
    Quit,
}

#[derive(Debug)]
pub enum Cmd {
    Input(InputEvent),
    SelectMonitor(u32),
    FullFrame,
    SetQuality {
        max_fps: u8,
        jpeg_quality: u8,
    },
    Push(PathBuf),
    Pull {
        remote: String,
        local_dir: PathBuf,
    },
    ListDir(String),
    /// Open a terminal on the host (answered with [`UiEvent::Terminal`]).
    OpenTerminal {
        cols: u16,
        rows: u16,
        /// With administrator rights (Windows hosts, full control).
        admin: bool,
    },
    /// Start receiving the remote screen (a session opened with `video: false`); it stays on
    /// for the rest of the session, reconnects included.
    ShowScreen,
    /// Try to connect again now: skips the wait between attempts, or starts over after a
    /// [`UiEvent::Fatal`].
    Reconnect,
    /// Ask for the picture scaled to fit this size (0×0: full size); kept across reconnects.
    SetMaxSize(u32, u32),
    /// Follow the host's pointer ([`UiEvent::Cursor`]) or stop.
    FollowCursor(bool),
    /// Text the user copied in the app, for the host's clipboard (with
    /// [`Extensions::clipboard_in_app`]).
    Clipboard(String),
    /// Let the host adapt quality and size to the link (kept across reconnects).
    AutoQuality(bool),
    /// Hear the computer's sound, or stop (with [`ConnectOpts::audio`]; kept across reconnects).
    Audio(bool),
    /// What the host's extensions offer with full control: restart, the black screen at the
    /// computer, blocking its keyboard and mouse.
    Ext(ExtClient),
    /// A port here that leads to a port reachable from the host (kept across reconnects).
    Tunnel(crate::tunnel::TunnelSpec),
    Quit,
}

/// The computer's sound, for the app to play ([`ConnectOpts::audio`]).
#[derive(Debug, Clone)]
pub enum AudioEvent {
    /// Opus packets follow at this rate and channel count.
    Start {
        sample_rate: u32,
        channels: u8,
    },
    Packet(Vec<u8>),
    /// No more for now (turned off, the host has none, the connection dropped).
    Stop,
}

/// Where the sound goes (called on the network thread: keep it quick).
pub type AudioSink = Arc<dyn Fn(AudioEvent) + Send + Sync>;

/// What the session asks of the host's extensions stream.
#[derive(Debug, Clone)]
pub struct Extensions {
    /// Scale the picture down to fit this size.
    pub max_size: Option<(u32, u32)>,
    /// Report the host's pointer ([`UiEvent::Cursor`]).
    pub cursor: bool,
    /// The clipboard goes through the app ([`UiEvent::Clipboard`], [`Cmd::Clipboard`]) instead
    /// of being watched here.
    pub clipboard_in_app: bool,
    /// Frames in the second format (in order, cached tiles, copies) when the host has it.
    pub video2: bool,
    /// Tiles kept for the second format (64×64 RGBA each: 16 KB).
    pub cache_slots: u32,
    /// Let the host adapt quality and size to the link.
    pub auto_quality: bool,
    /// Report the host's pointer image ([`UiEvent::CursorShape`]).
    pub cursor_shape: bool,
    /// Hear the computer (with [`ConnectOpts::audio`]).
    pub audio: bool,
    /// Port tunnels to open in every connection.
    pub tunnels: Vec<crate::tunnel::TunnelSpec>,
}

impl Default for Extensions {
    fn default() -> Self {
        Self {
            max_size: None,
            cursor: false,
            clipboard_in_app: false,
            video2: true,
            // 64 MB.
            cache_slots: 4096,
            auto_quality: false,
            cursor_shape: false,
            audio: true,
            tunnels: Vec::new(),
        }
    }
}

impl Extensions {
    /// Whether the session opens the host's extensions stream.
    pub fn wanted(&self) -> bool {
        self.max_size.is_some() || self.cursor || self.video2 || self.auto_quality || self.cursor_shape
    }

    /// Keep what a command asks for, for the next connection too.
    fn remember(&mut self, c: &Cmd) {
        match *c {
            Cmd::SetMaxSize(0, _) | Cmd::SetMaxSize(_, 0) => self.max_size = None,
            Cmd::SetMaxSize(w, h) => self.max_size = Some((w, h)),
            Cmd::FollowCursor(on) => self.cursor = on,
            Cmd::AutoQuality(on) => self.auto_quality = on,
            Cmd::Audio(on) => self.audio = on,
            Cmd::Tunnel(ref t) => {
                self.tunnels.retain(|x| x.local != t.local);
                self.tunnels.push(t.clone());
            }
            _ => {}
        }
    }
}

/// What the app wants an already open session window to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raise {
    Window,
    Files,
    Terminal,
}

impl Raise {
    /// The line the app writes to the session's standard input.
    pub fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "show" => Some(Raise::Window),
            "files" => Some(Raise::Files),
            "terminal" => Some(Raise::Terminal),
            _ => None,
        }
    }

    pub fn line(self) -> &'static str {
        match self {
            Raise::Window => "show",
            Raise::Files => "files",
            Raise::Terminal => "terminal",
        }
    }
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
    /// Frame ids only order frames within one connection.
    tile_frame: HashMap<(u32, u32), u64>,
    /// Which connection is painting; streams of an older one are ignored.
    session: u64,
    /// The second frame format, when this connection uses it.
    v2: Option<V2>,
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
            session: 0,
            v2: None,
        }
    }

    /// A new connection: the host numbers its frames from 1 again, so the ids of the
    /// last one must not hide the new tiles (the picture froze after a reconnect).
    fn begin_session(&mut self) -> u64 {
        self.session += 1;
        self.tile_frame.clear();
        self.v2 = None;
        self.session
    }

    /// Paint decoded tiles of frame `frame_id` of `monitor`, all under one lock (the caller
    /// holds it), unless the connection or the monitor changed meanwhile.
    fn apply<'a>(
        &mut self,
        session: u64,
        monitor: u32,
        frame_id: u64,
        tiles: impl IntoIterator<Item = (TileRect, &'a [u8])>,
    ) -> Result<()> {
        if self.session != session {
            return Err(anyhow!("frame from an old connection"));
        }
        if self.monitor != monitor {
            return Err(anyhow!("monitor switched mid-frame"));
        }
        for (rect, px) in tiles {
            self.paint(frame_id, rect, px)?;
        }
        Ok(())
    }

    /// Mark a painted rectangle for the UI.
    fn dirty(&mut self, rect: TileRect) {
        if !self.full_dirty {
            self.dirty.push(rect);
            let (cols, rows) = self.frame.grid();
            if self.dirty.len() > (cols * rows) as usize {
                self.full_dirty = true;
                self.dirty.clear();
            }
        }
    }

    /// Paint a decoded tile of frame `frame_id`. False when a newer frame already painted it.
    fn paint(&mut self, frame_id: u64, rect: TileRect, px: &[u8]) -> Result<bool> {
        let key = (rect.x, rect.y);
        if self.tile_frame.get(&key).is_some_and(|&f| f > frame_id) {
            return Ok(false);
        }
        self.frame.blit(rect, px)?;
        self.tile_frame.insert(key, frame_id);
        if !self.full_dirty {
            self.dirty.push(rect);
            // Nothing drawn for a while (a minimized window): don't pile up rectangles.
            let (cols, rows) = self.frame.grid();
            if self.dirty.len() > (cols * rows) as usize {
                self.full_dirty = true;
                self.dirty.clear();
            }
        }
        Ok(true)
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
    /// Receive the remote screen from the start. Without it the host captures nothing
    /// until [`Cmd::ShowScreen`] (a terminal-only window).
    pub video: bool,
    pub extensions: Extensions,
    /// Use this device key (a phone's secure hardware) for hosts that ask for a key.
    pub device_key: Option<Arc<dyn rdesk_common::auth::ExternalSigner>>,
    /// Play the computer's sound here (`None`: never asked for).
    pub audio: Option<AudioSink>,
}

#[derive(Debug, Clone)]
pub struct MachineTarget {
    pub id: i64,
    pub addrs: Vec<String>,
    /// [`Session::key`] of the account it was listed with: tickets come from that account
    /// even after the app changed to another one. `None`: the account in use.
    pub account: Option<String>,
}

/// A ticket and when it expires (Unix seconds).
pub type CachedTicket = Option<(Vec<u8>, i64)>;

/// A still-valid ticket for `machine` and this endpoint, from the cache or the server of the
/// account it was listed with. `Err((fatal, message))`.
pub async fn get_ticket(
    machine: &MachineTarget,
    client: &[u8; 32],
    cache: &mut CachedTicket,
) -> Result<Vec<u8>, (bool, String)> {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    if let Some((t, exp)) = cache
        && *exp > now + 600
    {
        return Ok(t.clone());
    }
    let session = match &machine.account {
        Some(key) => Session::load_account(key),
        None => Session::load(),
    };
    let Some(session) = session else {
        return Err((
            true,
            t!(
                "Not signed in to this computer's rdesk server: sign in in the app (or `rdesk login`).",
                "Você não está conectado ao servidor rdesk deste computador: entre pelo app (ou `rdesk login`)."
            )
            .into(),
        ));
    };
    let api = session.api().map_err(|e| (true, format!("{e:#}")))?;
    match api.ticket(machine.id, client).await {
        Ok((bytes, r)) => {
            *cache = Some((bytes.clone(), r.expires_at));
            Ok(bytes)
        }
        Err(e) if e.downcast_ref::<NotSignedIn>().is_some() => Err((
            true,
            tf!(
                "The server ended your sign-in ({e:#}): sign in again in the app.",
                "O servidor encerrou seu acesso ({e:#}): entre de novo pelo app."
            ),
        )),
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

/// A handle to no connection: commands go nowhere (the ui gallery).
#[doc(hidden)]
pub fn test_handle() -> Handle {
    let (cmd, _) = mpsc::unbounded_channel();
    Handle { cmd, view: Arc::new(Mutex::new(View::new())), thread: std::thread::spawn(|| {}) }
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
    /// The typed (or saved) password was rejected: ask again right away.
    WrongPassword,
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
    let mut video = opts.video;
    let mut ext = opts.extensions.clone();
    // Frames per second and JPEG quality chosen in the app (until then, the host's own).
    let mut quality: Option<(u8, u8)> = None;
    let mut failures = 0u32;
    let mut password_hint: Option<String> = None;
    loop {
        notify(UiEvent::Status(if failures == 0 { Status::Connecting } else { Status::Reconnecting }));
        let started = Instant::now();
        let end = once(
            &opts,
            endpoint,
            &mut resume,
            &mut ticket,
            &mut monitor,
            &mut video,
            &mut ext,
            &mut quality,
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
                // Stay alive until the UI quits (so it can show the message), or asks to try again.
                loop {
                    match cmd_rx.recv().await {
                        None | Some(Cmd::Quit) => return,
                        Some(Cmd::Reconnect) => break,
                        Some(Cmd::ShowScreen) => video = true,
                        Some(Cmd::SetQuality { max_fps, jpeg_quality }) => quality = Some((max_fps, jpeg_quality)),
                        Some(c) => ext.remember(&c),
                    }
                }
                failures = 0;
            }
            end @ (End::Retry(_) | End::WrongPassword) => {
                // A session that lasted a while resets the backoff.
                if started.elapsed() > Duration::from_secs(10) {
                    failures = 0;
                }
                let delay = [0, 250, 500, 1000, 2000][(failures as usize).min(4)];
                failures += 1;
                match end {
                    End::Retry(msg) => {
                        tracing::info!("disconnected: {msg}; retrying in {delay} ms");
                        notify(UiEvent::Status(Status::Retrying(msg.clone())));
                        notify(UiEvent::Reconnecting { reason: msg, attempt: failures });
                    }
                    // A rejected password is asked again right away: the session was not lost.
                    _ => {
                        tracing::info!("disconnected: wrong password; retrying in {delay} ms");
                        notify(UiEvent::Status(Status::Retrying(t!("wrong password", "senha incorreta").into())));
                    }
                }
                let sleep = tokio::time::sleep(Duration::from_millis(delay));
                tokio::pin!(sleep);
                // Drop input while offline, but honor Quit and "try now".
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        c = cmd_rx.recv() => match c {
                            None | Some(Cmd::Quit) => return,
                            Some(Cmd::Reconnect) => break,
                            Some(Cmd::ShowScreen) => video = true,
                            Some(Cmd::SetQuality { max_fps, jpeg_quality }) => {
                                quality = Some((max_fps, jpeg_quality));
                            }
                            Some(c) => ext.remember(&c),
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
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                None => {
                    let prompt = tf!("rdesk password for {target}: ", "Senha do rdesk para {target}: ");
                    let p = tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt)).await??;
                    (Zeroizing::new(p), false)
                }
                #[cfg(any(target_os = "android", target_os = "ios"))]
                None => return Err(PasswordCancelled.into()),
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
            Ok(Some(
                t!(
                    "The saved password was not accepted. Type it again.",
                    "A senha salva não foi aceita. Digite-a de novo."
                )
                .into(),
            ))
        }
        (Some(PasswordUse::Typed { .. }), false) => {
            Ok(Some(t!("Wrong password, try again.", "Senha incorreta, tente de novo.").into()))
        }
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
    video: &mut bool,
    ext: &mut Extensions,
    quality: &mut Option<(u8, u8)>,
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
            Some(m) => Some(get_ticket(m, ep.id().as_bytes(), ticket).await),
            None => None,
        }
    } else {
        let key = transport::SecretKey::generate();
        let client_id = *key.public().as_bytes();
        let bind = transport::bind(Some(key), &opts.net, &opts.transport, Role::Client);
        let fetch = async {
            match &opts.machine {
                Some(m) => Some(get_ticket(m, &client_id, ticket).await),
                None => None,
            }
        };
        let (ep, t) = tokio::join!(bind, fetch);
        match ep {
            Ok(ep) => {
                // Not through the server: also look for it on the local network (mDNS).
                if opts.machine.is_none()
                    && let Err(e) = transport::local_discovery(&ep, false)
                {
                    tracing::debug!("{e:#}");
                }
                *endpoint = Some(ep)
            }
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
    notify(UiEvent::Status(Status::Authenticating));
    let used: PasswordUsed = Arc::new(Mutex::new(None));
    let ui = opts.ask_password_in_ui.then(|| (notify.clone(), password_hint.take()));
    let asker = password_asker(&opts.name, &opts.entry, used.clone(), ui);
    let mut creds = credentials(&opts.entry, resume.take(), asker).await;
    creds.ticket = ticket_bytes;
    if creds.keys.is_none()
        && let Some(k) = &opts.device_key
    {
        creds.keys = Some(KeySource::External(k.clone()));
    }
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
                notify(UiEvent::Notice(tf!(
                    "Could not remember the password: {e}",
                    "Não foi possível guardar a senha: {e}"
                )));
            }
            ok
        }
        Ok(Err(e)) => {
            conn.close(close::NORMAL, b"");
            if e.downcast_ref::<PasswordCancelled>().is_some() {
                return End::Fatal(t!("Login cancelled.", "Login cancelado.").into());
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
                    return End::WrongPassword;
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
            return End::Retry(t!("authentication timed out", "a autenticação expirou").into());
        }
    };
    *resume = Some((ok.session_id, ok.resume_token));
    notify(UiEvent::Connected(ok.host.clone()));
    let access: rdesk_proto::Access = ok.access.into();
    notify(UiEvent::Access(access));
    notify(UiEvent::Status(Status::Connected { host: ok.host.hostname.clone() }));

    // The clipboard goes both ways: only with full control.
    let clipboard = opts.clipboard && access.input;
    let r = run_session(
        &conn,
        ctrl_w,
        ctrl_r,
        monitor,
        video,
        ext,
        quality,
        cmd_rx,
        view,
        notify,
        clipboard,
        access.input,
        opts.audio.clone(),
    )
    .await;
    if let Some(a) = &opts.audio {
        a(AudioEvent::Stop);
    }
    match r {
        Ok(true) => {
            conn.close(close::NORMAL, b"bye");
            End::Quit
        }
        Ok(false) | Err(_) => match conn.close_reason() {
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::REPLACED => End::Fatal(
                t!(
                    "Another rdesk client connected to this host; this session was closed.",
                    "Outro cliente rdesk se conectou a este host; esta sessão foi fechada."
                )
                .into(),
            ),
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::KICKED => End::Fatal(
                t!("The session was ended from the host computer.", "A sessão foi encerrada no computador host.")
                    .into(),
            ),
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::DENIED => End::Fatal(
                t!(
                    "The person at that computer did not allow the connection.",
                    "A pessoa nesse computador não permitiu a conexão."
                )
                .into(),
            ),
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::BUSY => {
                let why = String::from_utf8_lossy(&a.reason).into_owned();
                End::Fatal(tf!(
                    "That computer is busy ({why}). Try again when the other session ends.",
                    "Esse computador está ocupado ({why}). Tente de novo quando a outra sessão terminar."
                ))
            }
            Some(ConnectionError::ApplicationClosed(a)) if a.error_code == close::SHUTDOWN => {
                End::Retry(t!("host restarting", "o host está reiniciando").into())
            }
            Some(e) => End::Retry(e.to_string()),
            None => End::Retry(t!("connection lost", "conexão perdida").into()),
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
    video: &mut bool,
    ext: &mut Extensions,
    quality: &mut Option<(u8, u8)>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    view: &SharedView,
    notify: &Notify,
    clipboard_on: bool,
    input_allowed: bool,
    audio: Option<AudioSink>,
) -> Result<bool> {
    let mut input = conn.open_uni().await?;
    let _ = input.set_priority(20);
    write_msg(&mut input, &StreamOpen::Input).await?;
    write_msg(&mut input, &InputEvent::ReleaseAll).await?;

    if let Some(m) = *monitor {
        write_msg(&mut ctrl_w, &ClientMsg::SelectMonitor { id: m }).await?;
    }
    // A reconnect starts with the host's own settings: keep what was chosen in the app.
    if let Some((max_fps, jpeg_quality)) = *quality {
        write_msg(&mut ctrl_w, &ClientMsg::SetQuality { max_fps, jpeg_quality }).await?;
    }

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<u64>();
    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<String>();
    let (img_tx, mut img_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let clipboard = if clipboard_on && !ext.clipboard_in_app {
        ClipboardSync::spawn(clip_tx, Some(img_tx)).map(Arc::new)
    } else {
        None
    };
    let clipboard_to_app = clipboard_on && ext.clipboard_in_app;
    let (frame_pending, session) = {
        let mut v = view.lock().unwrap();
        (v.pending.clone(), v.begin_session())
    };

    let mut tasks = tokio::task::JoinSet::<Result<()>>::new();

    // Control writer.
    tasks.spawn(async move {
        while let Some(m) = ctrl_rx.recv().await {
            write_msg(&mut ctrl_w, &m).await?;
        }
        Ok(())
    });

    // Ack writer. The host starts the picture when this stream opens, so it opens only once
    // the screen is wanted. (Older hosts send frames regardless: without acks they stop
    // after a couple, and the acks held here let them go on when the screen is asked for.)
    let (want_video, video_wanted) = tokio::sync::oneshot::channel::<()>();
    let mut want_video = Some(want_video);
    if *video && let Some(tx) = want_video.take() {
        let _ = tx.send(());
    }
    // With a size limit, the picture waits for the extensions stream to pass it on: the
    // first frame (the biggest) then already comes scaled.
    let (ext_ready, ext_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let ext_ready = if ext.wanted() {
        Some(ext_ready)
    } else {
        let _ = ext_ready.send(());
        None
    };
    let ack_conn = conn.clone();
    // What the host's extensions stream offers (filled in before `ext_ready`).
    let host_features: Arc<Mutex<Vec<Feature>>> = Arc::new(Mutex::new(Vec::new()));
    let (ack_view, ack_features, ack_ext) = (view.clone(), host_features.clone(), ext.clone());
    tasks.spawn(async move {
        if video_wanted.await.is_err() {
            // The session is ending without the picture.
            std::future::pending::<()>().await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), ext_ready_rx).await;
        let mut acks = ack_conn.open_uni().await?;
        let _ = acks.set_priority(20);
        // The second frame format when both sides have it; frames will then be read that way.
        let second = ack_ext.video2 && ack_features.lock().unwrap().contains(&Feature::Video2);
        if second {
            let mut v = ack_view.lock().unwrap();
            if v.session == session {
                v.v2 = Some(V2::new(ack_ext.cache_slots));
            }
        }
        let open = if second {
            StreamOpen::Acks2(VideoCaps { cache_slots: ack_ext.cache_slots, h264: crate::video2::decodes_h264() })
        } else {
            StreamOpen::Acks
        };
        write_msg(&mut acks, &open).await?;
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
    let ext_clipboard = clipboard.clone();
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
                        } else if clipboard_to_app {
                            notify(UiEvent::Clipboard(text));
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
        let full_tx = ctrl_tx.clone();
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
                let full_tx = full_tx.clone();
                tokio::spawn(async move {
                    let second = view.lock().unwrap().v2.is_some();
                    if second {
                        if let Err(e) = read_frame2(&mut s, &view, session, &ack_tx).await {
                            tracing::debug!("frame stream: {e:#}");
                            if e.downcast_ref::<BadFrame>().is_some() {
                                // What is on screen is no longer what the host thinks: start over.
                                let _ = full_tx.send(ClientMsg::RequestFullFrame);
                            }
                        }
                        // Frames waiting for a missing one: look again once it is given up on.
                        if view.lock().unwrap().v2.as_ref().is_some_and(V2::blocked) {
                            let (view, ack_tx, pending) = (view.clone(), ack_tx.clone(), pending.clone());
                            let (notify, full_tx) = (notify.clone(), full_tx.clone());
                            tokio::spawn(async move {
                                tokio::time::sleep(crate::video2::GAP_WAIT).await;
                                if drain2(&view, session, &ack_tx).is_err() {
                                    let _ = full_tx.send(ClientMsg::RequestFullFrame);
                                }
                                if !pending.swap(true, Ordering::AcqRel) {
                                    notify(UiEvent::FrameUpdated);
                                }
                            });
                        }
                    } else {
                        match read_frame(&mut s, &view, session).await {
                            Ok(id) => {
                                let _ = ack_tx.send(id);
                            }
                            Err(e) => tracing::trace!("frame stream: {e:#}"),
                        }
                    }
                    if !pending.swap(true, Ordering::AcqRel) {
                        notify(UiEvent::FrameUpdated);
                    }
                });
            }
        });
    }
    // The extensions stream (picture size, pointer), for clients that ask for it.
    let (ext_tx, ext_rx) = mpsc::unbounded_channel::<ExtClient>();
    let (feat_tx, feat_rx) = tokio::sync::watch::channel::<Option<Vec<Feature>>>(None);
    if let Some(ready) = ext_ready {
        tasks.spawn(extensions(
            conn.clone(),
            ext.clone(),
            ext_rx,
            ready,
            notify.clone(),
            host_features,
            feat_tx,
            ext_clipboard,
        ));
    }
    // Images copied here go on the extensions stream (once the host takes them).
    {
        let ext_tx = ext_tx.clone();
        tasks.spawn(async move {
            while let Some(png) = img_rx.recv().await {
                let _ = ext_tx.send(ExtClient::ClipImage(png));
            }
            std::future::pending::<Result<()>>().await
        });
    }
    // Port tunnels: each listens here while the session lasts.
    for t in ext.tunnels.clone() {
        tasks.spawn(tunnel_task(conn.clone(), t, notify.clone()));
    }
    // The computer's sound, while wanted.
    let (audio_on_tx, audio_on_rx) = tokio::sync::watch::channel(ext.audio);
    if let Some(sink) = audio {
        tasks.spawn(audio_stream(conn.clone(), feat_rx, audio_on_rx, sink));
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
                    // Already connected.
                    Cmd::Reconnect => {}
                    Cmd::SetMaxSize(width, height) => {
                        ext.remember(&Cmd::SetMaxSize(width, height));
                        let _ = ext_tx.send(ExtClient::MaxSize { width, height });
                    }
                    Cmd::FollowCursor(on) => {
                        ext.remember(&Cmd::FollowCursor(on));
                        let _ = ext_tx.send(ExtClient::Cursor { on });
                    }
                    Cmd::AutoQuality(on) => {
                        ext.remember(&Cmd::AutoQuality(on));
                        let _ = ext_tx.send(ExtClient::AutoQuality { on });
                    }
                    Cmd::Audio(on) => {
                        ext.remember(&Cmd::Audio(on));
                        let _ = audio_on_tx.send(on);
                    }
                    Cmd::Ext(m) => {
                        let _ = ext_tx.send(m);
                    }
                    Cmd::Tunnel(t) => {
                        if !ext.tunnels.contains(&t) {
                            ext.remember(&Cmd::Tunnel(t.clone()));
                            tasks.spawn(tunnel_task(conn.clone(), t, notify.clone()));
                        }
                    }
                    Cmd::Clipboard(text) => {
                        if clipboard_to_app {
                            let _ = ctrl_tx.send(ClientMsg::Clipboard { text });
                        }
                    }
                    Cmd::ShowScreen => {
                        *video = true;
                        if let Some(tx) = want_video.take() {
                            let _ = tx.send(());
                        }
                    }
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
                        *quality = Some((max_fps, jpeg_quality));
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
                    Cmd::OpenTerminal { cols, rows, admin } => {
                        let (conn, notify) = (conn.clone(), notify.clone());
                        tokio::spawn(async move {
                            let wake = notify.clone();
                            let r = crate::terminal::open(conn, cols, rows, admin, move || wake(UiEvent::Wake)).await;
                            notify(UiEvent::Terminal {
                                admin,
                                result: r
                                    .map(|l| {
                                        crate::terminal::TermSlot(std::sync::Arc::new(std::sync::Mutex::new(Some(l))))
                                    })
                                    .map_err(|e| format!("{e:#}")),
                            });
                        });
                    }
                }
            }
            _ = conn.closed() => break Ok(false),
        }
    };
    tasks.abort_all();
    result
}

/// Run the extensions stream. Never ends on its own (the session must not end with it):
/// a host without extensions just leaves the stream unanswered.
#[allow(clippy::too_many_arguments)]
async fn extensions(
    conn: Connection,
    want: Extensions,
    mut rx: mpsc::UnboundedReceiver<ExtClient>,
    ready: tokio::sync::oneshot::Sender<()>,
    notify: Notify,
    host_features: Arc<Mutex<Vec<Feature>>>,
    feat_tx: tokio::sync::watch::Sender<Option<Vec<Feature>>>,
    clipboard: Option<Arc<ClipboardSync>>,
) -> Result<()> {
    // Dropped on any failure, which also lets the picture start.
    let mut ready = Some(ready);
    let r: Result<()> = async {
        let (mut w, mut r) = conn.open_bi().await?;
        write_msg(&mut w, &StreamOpen::Extensions).await?;
        write_msg(&mut w, &ExtClient::Hello { rev: EXT_REV }).await?;
        let features = match read_msg::<_, ExtHost>(&mut r, MAX_SMALL_MSG).await {
            Ok(Some(ExtHost::Hello { features, .. })) => features,
            _ => {
                notify(UiEvent::Extensions(Vec::new()));
                let _ = feat_tx.send(Some(Vec::new()));
                anyhow::bail!("this host has no extensions (an older rdesk)");
            }
        };
        notify(UiEvent::Extensions(features.clone()));
        *host_features.lock().unwrap() = features.clone();
        let _ = feat_tx.send(Some(features.clone()));
        let can = |f: Feature| features.contains(&f);
        if can(Feature::MaxSize)
            && let Some((width, height)) = want.max_size
        {
            write_msg(&mut w, &ExtClient::MaxSize { width, height }).await?;
        }
        if can(Feature::Cursor) && want.cursor {
            write_msg(&mut w, &ExtClient::Cursor { on: true }).await?;
        }
        if can(Feature::AutoQuality) && want.auto_quality {
            write_msg(&mut w, &ExtClient::AutoQuality { on: true }).await?;
        }
        if can(Feature::CursorShape) && want.cursor_shape {
            write_msg(&mut w, &ExtClient::CursorShape { on: true }).await?;
        }
        if can(Feature::ClipboardImage)
            && let Some(c) = &clipboard
        {
            write_msg(&mut w, &ExtClient::ClipboardImages { on: true }).await?;
            c.set_images(true);
        }
        if let Some(r) = ready.take() {
            let _ = r.send(());
        }
        let read = async {
            // Images make some messages big.
            while let Some(m) = read_msg::<_, ExtHost>(&mut r, MAX_CONTROL_MSG).await? {
                match m {
                    ExtHost::Cursor { x, y, visible } => notify(UiEvent::Cursor { x, y, visible }),
                    ExtHost::Quality { fps, jpeg_quality, scale_pct } => {
                        notify(UiEvent::Quality { fps, jpeg_quality, scale_pct })
                    }
                    ExtHost::CursorShape { id, image } => notify(UiEvent::CursorShape { id, image }),
                    ExtHost::ClipImage(png) => {
                        if let Some(c) = &clipboard {
                            c.set_image(png);
                        }
                    }
                    ExtHost::Notice(text) => notify(UiEvent::Notice(text)),
                    ExtHost::Hello { .. } => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let write = async {
            while let Some(m) = rx.recv().await {
                let allowed = match m {
                    ExtClient::MaxSize { .. } => can(Feature::MaxSize),
                    ExtClient::Cursor { .. } => can(Feature::Cursor),
                    ExtClient::AutoQuality { .. } => can(Feature::AutoQuality),
                    ExtClient::CursorShape { .. } => can(Feature::CursorShape),
                    ExtClient::ClipboardImages { .. } | ExtClient::ClipImage(_) => {
                        can(Feature::ClipboardImage) && clipboard.is_some()
                    }
                    ExtClient::Power(_) => can(Feature::Power),
                    ExtClient::Privacy { .. } => can(Feature::Privacy),
                    ExtClient::BlockInput { .. } => can(Feature::BlockInput),
                    ExtClient::Nav(_) => can(Feature::Nav),
                    ExtClient::Touch(_) => can(Feature::MultiTouch),
                    ExtClient::Locks { .. } => can(Feature::LockKeys),
                    ExtClient::Hello { .. } => false,
                };
                if allowed {
                    write_msg(&mut w, &m).await?;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            r = read => r,
            r = write => r,
        }
    }
    .await;
    drop(ready);
    if let Err(e) = r {
        tracing::debug!("extensions: {e:#}");
    }
    std::future::pending::<Result<()>>().await
}

/// A port tunnel for the session's lifetime. A port that cannot be opened is told once; the
/// session goes on.
async fn tunnel_task(conn: Connection, t: crate::tunnel::TunnelSpec, notify: Notify) -> Result<()> {
    let n = notify.clone();
    let what = format!("localhost:{} → {}:{}", t.local, t.host, t.port);
    if let Err(e) = crate::tunnel::forward(conn, t, move |e| n(UiEvent::Notice(tf!("Tunnel: {e}", "Túnel: {e}")))).await
    {
        notify(UiEvent::Notice(tf!("Tunnel {what}: {e:#}", "Túnel {what}: {e:#}")));
    }
    std::future::pending().await
}

/// The audio stream ([`StreamOpen::Audio`]): open while sound is wanted and the host has it.
/// Never ends on its own (the session must not end with it).
async fn audio_stream(
    conn: Connection,
    mut features: tokio::sync::watch::Receiver<Option<Vec<Feature>>>,
    mut on: tokio::sync::watch::Receiver<bool>,
    sink: AudioSink,
) -> Result<()> {
    use rdesk_proto::AudioHost;
    let has = features.wait_for(|f| f.is_some()).await.map(|f| f.as_ref().is_some_and(|f| f.contains(&Feature::Audio)));
    if !matches!(has, Ok(true)) {
        return std::future::pending().await;
    }
    loop {
        if on.wait_for(|&v| v).await.is_err() {
            return std::future::pending().await;
        }
        let r: Result<()> = async {
            let (mut w, mut r) = conn.open_bi().await?;
            write_msg(&mut w, &StreamOpen::Audio).await?;
            loop {
                tokio::select! {
                    m = read_msg::<_, AudioHost>(&mut r, MAX_SMALL_MSG) => match m? {
                        Some(AudioHost::Start { sample_rate, channels }) => sink(AudioEvent::Start { sample_rate, channels }),
                        Some(AudioHost::Packet(p)) => sink(AudioEvent::Packet(p)),
                        Some(AudioHost::Error(e)) => anyhow::bail!("{e}"),
                        None => return Ok(()),
                    },
                    c = on.changed() => if c.is_err() || !*on.borrow() {
                        let _ = w.finish();
                        return Ok(());
                    },
                }
            }
        }
        .await;
        sink(AudioEvent::Stop);
        if let Err(e) = r {
            tracing::info!("no sound: {e:#}");
            // Wait to be asked again (turned off and on).
            let _ = on.wait_for(|&v| !v).await;
        }
    }
}

async fn read_frame(s: &mut RecvStream, view: &SharedView, session: u64) -> Result<u64> {
    let h: FrameHeader = expect_msg(s, MAX_VIDEO_MSG).await?;
    {
        let mut v = view.lock().unwrap();
        if v.session != session {
            return Err(anyhow!("frame from an old connection"));
        }
        if v.monitor != h.monitor_id || v.frame.width != h.width || v.frame.height != h.height {
            v.frame = Frame::new(h.width, h.height);
            v.monitor = h.monitor_id;
            v.full_dirty = true;
            v.dirty.clear();
            v.tile_frame.clear();
        }
    }
    // A full frame (the first one, another monitor or size) is painted as it arrives: there
    // is nothing older worth keeping on screen, and on a slow link the picture shows up bit
    // by bit instead of all at once seconds later. Any other frame is applied whole, at once:
    // painted tile by tile, whoever looked at the picture in between (the window, the phone
    // app) could catch half of it, like something that moved drawn at both places.
    let progressive = h.full;
    let mut decoded: Vec<(TileRect, Vec<u8>)> = Vec::new();
    let mut scratch = Vec::new();
    while let Some(tile) = read_msg::<_, Tile>(s, MAX_VIDEO_MSG).await? {
        // Decode outside the lock.
        rdesk_common::tiles::decode_tile(&tile, &mut scratch)?;
        let rect = TileRect { x: tile.x, y: tile.y, w: tile.w as u32, h: tile.h as u32 };
        if progressive {
            view.lock().unwrap().apply(session, h.monitor_id, h.frame_id, [(rect, scratch.as_slice())])?;
        } else {
            decoded.push((rect, std::mem::take(&mut scratch)));
        }
    }
    let mut v = view.lock().unwrap();
    v.apply(session, h.monitor_id, h.frame_id, decoded.iter().map(|(r, px)| (*r, px.as_slice())))?;
    v.frames += 1;
    Ok(h.frame_id)
}

/// A frame of the second format that cannot be applied (the cache or the picture is not what
/// the host thinks): the client asks for a full frame.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct BadFrame(String);

/// Read a frame of the second format, decode it and apply what is ready, in order
/// (acknowledging each frame applied).
async fn read_frame2(
    s: &mut RecvStream,
    view: &SharedView,
    session: u64,
    acks: &mpsc::UnboundedSender<u64>,
) -> Result<()> {
    let h: FrameHeader = expect_msg(s, MAX_VIDEO_MSG).await?;
    {
        let mut v = view.lock().unwrap();
        if v.session != session {
            return Err(anyhow!("frame from an old connection"));
        }
        if h.full {
            if let Some(v2) = v.v2.as_mut() {
                v2.skip_to(h.frame_id);
            }
            if v.monitor != h.monitor_id || v.frame.width != h.width || v.frame.height != h.height {
                v.frame = Frame::new(h.width, h.height);
                v.monitor = h.monitor_id;
                v.full_dirty = true;
                v.dirty.clear();
            }
        }
    }
    let mut scratch = Vec::new();
    let mut tiles = Vec::new();
    while let Some(t) = read_msg::<_, Tile2>(s, MAX_VIDEO_MSG).await? {
        let rect = TileRect { x: t.x, y: t.y, w: t.w as u32, h: t.h as u32 };
        let pix = match t.data {
            TileData::Cached(slot) => Pix::Cached(slot),
            TileData::Copy { x, y } => Pix::Copy { x, y },
            TileData::H264(au) => Pix::H264(au),
            data => {
                rdesk_common::tiles::decode_tile2(&data, rect.w, rect.h, &mut scratch)?;
                Pix::Pixels(std::mem::take(&mut scratch))
            }
        };
        let tile = DTile { rect, pix, store: t.store };
        if h.full {
            // Painted as it arrives (see `read_frame`).
            let mut guard = view.lock().unwrap();
            let v = &mut *guard;
            if v.session != session
                || v.monitor != h.monitor_id
                || v.v2.as_ref().and_then(|x| x.next) != Some(h.frame_id)
            {
                return Err(anyhow!("a newer full frame took over"));
            }
            let v2 = v.v2.as_mut().expect("second format");
            let r = v2.paint_now(&mut v.frame, tile).map_err(|e| BadFrame(format!("{e:#}")))?;
            v.dirty(r);
        } else {
            tiles.push(tile);
        }
    }
    let mut v = view.lock().unwrap();
    if v.session != session {
        return Err(anyhow!("frame from an old connection"));
    }
    let v2 = v.v2.as_mut().ok_or_else(|| anyhow!("not the second format"))?;
    if h.full {
        if v2.next == Some(h.frame_id) {
            v2.done(h.frame_id);
            v.frames += 1;
            let _ = acks.send(h.frame_id);
        }
    } else {
        v2.push(h.frame_id, Decoded { monitor: h.monitor_id, width: h.width, height: h.height, tiles });
    }
    drop(v);
    drain2(view, session, acks)
}

/// Apply the frames of the second format that are ready, in order.
fn drain2(view: &SharedView, session: u64, acks: &mpsc::UnboundedSender<u64>) -> Result<()> {
    let mut guard = view.lock().unwrap();
    let v = &mut *guard;
    if v.session != session {
        return Ok(());
    }
    let Some(v2) = v.v2.as_mut() else { return Ok(()) };
    for (id, f) in v2.ready(Instant::now()) {
        if f.monitor != v.monitor || f.width != v.frame.width || f.height != v.frame.height {
            continue;
        }
        let painted = v2.apply(&mut v.frame, f).map_err(|e| BadFrame(format!("frame {id}: {e:#}")))?;
        for r in painted {
            if !v.full_dirty {
                v.dirty.push(r);
            }
        }
        let (cols, rows) = v.frame.grid();
        if v.dirty.len() > (cols * rows) as usize {
            v.full_dirty = true;
            v.dirty.clear();
        }
        v.frames += 1;
        let _ = acks.send(id);
    }
    Ok(())
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

/// The whole picture, if anything changed since the last look (for apps that redraw it
/// whole, like the phone app). Like [`take_updates`], it marks everything as seen.
pub fn take_frame(view: &SharedView) -> Option<(u32, u32, Vec<u8>)> {
    let mut v = view.lock().unwrap();
    if v.monitor == u32::MAX {
        // Nothing has arrived yet (the placeholder is not a picture).
        return None;
    }
    let changed = v.full_dirty || !v.dirty.is_empty();
    v.full_dirty = false;
    v.dirty.clear();
    v.pending.store(false, Ordering::Release);
    changed.then(|| (v.frame.width, v.frame.height, v.frame.data.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(v: u8) -> Vec<u8> {
        vec![v; (rdesk_proto::TILE * rdesk_proto::TILE * 4) as usize]
    }

    fn rect(col: u32) -> TileRect {
        rdesk_common::tiles::tile_rect(col, 0, 128, 64)
    }

    fn pixel(v: &View, col: u32) -> u8 {
        v.frame.data[(col * rdesk_proto::TILE * 4) as usize]
    }

    #[test]
    fn late_frame_does_not_overwrite_newer_tile() {
        let mut v = View::new();
        v.frame = Frame::new(128, 64);
        v.begin_session();
        assert!(v.paint(2, rect(0), &tile(2)).unwrap());
        assert!(!v.paint(1, rect(0), &tile(1)).unwrap(), "frame 1 arrived after frame 2");
        assert_eq!(pixel(&v, 0), 2);
    }

    #[test]
    fn reconnect_paints_although_frame_ids_restart() {
        let mut v = View::new();
        v.frame = Frame::new(128, 64);
        v.begin_session();
        for id in 1..=5000 {
            v.paint(id, rect(0), &tile(1)).unwrap();
        }
        v.paint(3, rect(1), &tile(1)).unwrap();
        // Same monitor and size after the reconnect, and the host counts from 1 again.
        v.begin_session();
        assert!(v.paint(1, rect(0), &tile(9)).unwrap());
        assert!(v.paint(1, rect(1), &tile(9)).unwrap());
        assert_eq!((pixel(&v, 0), pixel(&v, 1)), (9, 9));
    }

    #[test]
    fn a_new_session_retires_the_old_one() {
        let mut v = View::new();
        let old = v.begin_session();
        let new = v.begin_session();
        assert_ne!(old, new);
        assert_eq!(v.session, new, "read_frame drops streams still tagged with `old`");
    }

    #[test]
    fn a_frame_is_applied_whole_or_not_at_all() {
        let mut v = View::new();
        v.frame = Frame::new(128, 64);
        v.monitor = 1;
        let s = v.begin_session();
        let (a, b) = (tile(4), tile(5));
        v.apply(s, 1, 7, [(rect(0), a.as_slice()), (rect(1), b.as_slice())]).unwrap();
        assert_eq!((pixel(&v, 0), pixel(&v, 1)), (4, 5));
        // The monitor or the connection changed while the frame was arriving: none of it lands.
        let c = tile(9);
        assert!(v.apply(s, 2, 8, [(rect(0), c.as_slice())]).is_err());
        assert!(v.apply(s + 1, 1, 8, [(rect(0), c.as_slice())]).is_err());
        assert_eq!(pixel(&v, 0), 4);
    }

    #[test]
    fn raise_lines_round_trip() {
        for r in [Raise::Window, Raise::Files, Raise::Terminal] {
            assert_eq!(Raise::parse(r.line()), Some(r));
        }
        assert_eq!(Raise::parse("terminal\r"), Some(Raise::Terminal), "Windows line ends");
        assert_eq!(Raise::parse("quit"), None);
    }
}
