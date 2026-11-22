//! One remote session: the picture, the pointer and keys, the clipboard, files and
//! terminals. The connection itself (reconnects, sign-in to the computer) is
//! `rdesk_core::conn`, as on the desktop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use flutter_rust_bridge::{DartFnFuture, frb};
use rdesk_common::config::{self, ClientConfig, NetworkConfig};
use rdesk_core::conn::{
    self, Cmd, ConnectOpts, Extensions, Handle, MachineTarget, PasswordAnswer, PasswordReply, UiEvent,
};
use rdesk_proto::{InputEvent, Key, MouseButton, TermClient, TermHost};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::frb_generated::StreamSink;

/// Which computer to open.
#[derive(Debug, Clone)]
pub struct Target {
    /// The name shown (and the key its remembered password is saved under).
    pub name: String,
    /// Host ID (hex).
    pub host_id: String,
    /// Its id on the server; `None` for a computer saved on the phone (no server).
    pub machine_id: Option<i64>,
    /// Addresses to try directly: the server's list, or the saved address.
    pub addrs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// The phone screen in pixels (the picture is scaled to fit it; 0: full size).
    pub max_width: u32,
    pub max_height: u32,
    /// Start with the picture (false: a terminal-only session).
    pub video: bool,
}

/// What happened in the session, for the app to show. One flat struct (the fields used
/// depend on `kind`) so the Dart side needs no generated classes.
#[derive(Debug, Clone, Default)]
pub struct SessionEvent {
    pub kind: EventKind,
    /// Status line, notice, error, the remote clipboard's text, a terminal's shell, the
    /// computer a password is asked for…
    pub text: String,
    /// More about it: why the last password failed, a listing's error.
    pub detail: String,
    /// The password may be remembered on the phone.
    pub can_remember: bool,
    /// Pointer (0..=65535) or transfer progress (bytes).
    pub x: i64,
    pub y: i64,
    pub visible: bool,
    pub monitors: Vec<Monitor>,
    pub active_monitor: u32,
    pub access: Option<Access>,
    pub stats: Option<Stats>,
    /// Transfer id, terminal id, reconnect attempt.
    pub id: u32,
    /// Terminal output, or a directory listing's entries.
    pub bytes: Vec<u8>,
    pub entries: Vec<DirItem>,
    /// `Status`: connected (signed in to the computer); `TransferDone` / `TerminalExited` /
    /// `DirListing`: whether it went well.
    pub ok: bool,
    pub cursor_supported: bool,
    pub scaling_supported: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EventKind {
    #[default]
    Status,
    PasswordNeeded,
    Notice,
    Connected,
    Access,
    Monitors,
    Frame,
    Stats,
    Fatal,
    Reconnecting,
    Extensions,
    Cursor,
    Clipboard,
    DirListing,
    TransferProgress,
    TransferDone,
    TerminalOpened,
    TerminalOutput,
    TerminalExited,
    Closed,
}

#[derive(Debug, Clone)]
pub struct Monitor {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Access {
    pub screen: bool,
    pub input: bool,
    pub files: bool,
    pub terminal: bool,
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub rtt_ms: u32,
    pub fps: f32,
    pub kbps: f32,
    pub route: String,
}

#[derive(Debug, Clone)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
}

/// The picture as RGBA (tightly packed).
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// An open session (see [`RdSession::start`]).
#[frb(opaque)]
pub struct RdSession {
    target: Target,
    options: SessionOptions,
    device_key: Mutex<Option<Arc<dyn rdesk_common::auth::ExternalSigner>>>,
    handle: Mutex<Option<Handle>>,
    shared: Arc<Shared>,
}

#[frb(ignore)]
#[derive(Default)]
struct Shared {
    password: Mutex<Option<PasswordReply>>,
    terminals: Mutex<HashMap<u32, mpsc::UnboundedSender<TermClient>>>,
    next_terminal: AtomicU32,
}

fn key_named(name: &str) -> Option<Key> {
    Key::ALL.iter().copied().find(|k| k.name() == name)
}

fn norm(v: f64) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

impl RdSession {
    #[frb(sync)]
    pub fn new(target: Target, options: SessionOptions) -> RdSession {
        RdSession {
            target,
            options,
            device_key: Mutex::new(None),
            handle: Mutex::new(None),
            shared: Arc::new(Shared::default()),
        }
    }

    /// Use the phone's hardware key ([`crate::api::device_key`]) for computers with the
    /// extra lock. Call before [`start`](Self::start). `sign` returns the DER signature
    /// of its argument (ECDSA P-256 with SHA-256), or nothing when the user cancelled.
    pub fn use_device_key(
        &self,
        public_key: Vec<u8>,
        sign: impl Fn(Vec<u8>) -> DartFnFuture<Vec<u8>> + Send + Sync + 'static,
    ) -> Result<()> {
        let s = crate::api::device_key::DartSigner::new(&public_key, sign)?;
        *self.device_key.lock().unwrap() = Some(Arc::new(s));
        Ok(())
    }

    /// Connect, and keep reconnecting until [`close`](Self::close). Events arrive on the
    /// returned stream; it ends with `Closed`.
    pub fn start(&self, sink: StreamSink<SessionEvent>) -> Result<()> {
        let id = config::parse_host_id(&self.target.host_id)?;
        let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
        let key = config::format_host_id(&id);
        let mut entry = cfg.hosts.get(&key).cloned().unwrap_or_default();
        entry.id = key;
        let machine = self.target.machine_id.map(|id| MachineTarget { id, addrs: self.target.addrs.clone() });
        if machine.is_none()
            && let Some(a) = self.target.addrs.first()
        {
            entry.addr = a.clone();
        }
        // No SSH agent or 1Password CLI on a phone: the hardware key or the password.
        entry.password_ref = None;
        entry.agent_socket = None;
        entry.identity = None;
        let net = if machine.is_some() { config::load_network() } else { NetworkConfig::default() };
        let (w, h) = (self.options.max_width, self.options.max_height);
        let opts = ConnectOpts {
            name: self.target.name.clone(),
            ask_password_in_ui: true,
            entry,
            id,
            machine,
            monitor: None,
            transport: cfg.transport.clone(),
            net,
            clipboard: true,
            video: self.options.video,
            extensions: Extensions {
                max_size: (w > 0 && h > 0).then_some((w, h)),
                cursor: true,
                clipboard_in_app: true,
                // 16 MB of tiles, and a picture that follows the phone's link.
                cache_slots: 1024,
                auto_quality: true,
                ..Extensions::default()
            },
            device_key: self.device_key.lock().unwrap().clone(),
            audio: None,
        };
        let shared = self.shared.clone();
        let sink = Arc::new(sink);
        let handle = conn::spawn(opts, move |ev| forward(&shared, &sink, ev));
        if let Some(old) = self.handle.lock().unwrap().replace(handle) {
            let _ = old.cmd.send(Cmd::Quit);
        }
        Ok(())
    }

    fn send(&self, c: Cmd) {
        if let Some(h) = self.handle.lock().unwrap().as_ref() {
            let _ = h.cmd.send(c);
        }
    }

    /// The whole picture, if it changed since the last call (after a `Frame` event).
    #[frb(sync)]
    pub fn take_frame(&self) -> Option<Frame> {
        let h = self.handle.lock().unwrap();
        let (width, height, rgba) = conn::take_frame(&h.as_ref()?.view)?;
        Some(Frame { width, height, rgba })
    }

    // ---- input ----

    /// Move the remote pointer to (`x`, `y`), each 0..1 across the shown monitor.
    #[frb(sync)]
    pub fn pointer(&self, x: f64, y: f64) {
        self.send(Cmd::Input(InputEvent::MouseMove { x: norm(x), y: norm(y) }));
    }

    #[frb(sync)]
    pub fn button(&self, button: Button, down: bool) {
        let button = match button {
            Button::Left => MouseButton::Left,
            Button::Right => MouseButton::Right,
            Button::Middle => MouseButton::Middle,
        };
        self.send(Cmd::Input(InputEvent::MouseButton { button, down }));
    }

    /// Scroll in notches (positive: up / right; fractions are fine).
    #[frb(sync)]
    pub fn scroll(&self, dx: f64, dy: f64) {
        self.send(Cmd::Input(InputEvent::Scroll { dx: dx as f32, dy: dy as f32 }));
    }

    /// A key by its physical code (W3C `code`: `KeyA`, `ControlLeft`, `F5`, `ArrowUp`…).
    #[frb(sync)]
    pub fn key(&self, code: String, down: bool) -> Result<()> {
        let key = key_named(&code).with_context(|| format!("unknown key {code}"))?;
        self.send(Cmd::Input(InputEvent::Key { key, down }));
        Ok(())
    }

    /// Type text as it is (the phone keyboard's characters, accents included).
    #[frb(sync)]
    pub fn text(&self, text: String) {
        if !text.is_empty() {
            self.send(Cmd::Input(InputEvent::Text { text }));
        }
    }

    /// Let go of every key and button (the app went to the background mid-press).
    #[frb(sync)]
    pub fn release_all(&self) {
        self.send(Cmd::Input(InputEvent::ReleaseAll));
    }

    // ---- view ----

    #[frb(sync)]
    pub fn select_monitor(&self, id: u32) {
        self.send(Cmd::SelectMonitor(id));
    }

    #[frb(sync)]
    pub fn set_quality(&self, max_fps: u8, jpeg_quality: u8) {
        self.send(Cmd::SetQuality { max_fps, jpeg_quality });
    }

    /// Fit the picture to this size (0×0: full size), e.g. after a rotation or zoom.
    #[frb(sync)]
    pub fn set_max_size(&self, width: u32, height: u32) {
        self.send(Cmd::SetMaxSize(width, height));
    }

    #[frb(sync)]
    pub fn follow_cursor(&self, enabled: bool) {
        self.send(Cmd::FollowCursor(enabled));
    }

    #[frb(sync)]
    pub fn full_frame(&self) {
        self.send(Cmd::FullFrame);
    }

    #[frb(sync)]
    pub fn show_screen(&self) {
        self.send(Cmd::ShowScreen);
    }

    /// Put text on the computer's clipboard.
    #[frb(sync)]
    pub fn send_clipboard(&self, text: String) {
        self.send(Cmd::Clipboard(text));
    }

    /// Try again now (after `Fatal`, or to skip the wait between attempts).
    #[frb(sync)]
    pub fn reconnect(&self) {
        self.send(Cmd::Reconnect);
    }

    /// Answer `PasswordNeeded` (`None`: cancel).
    #[frb(sync)]
    pub fn answer_password(&self, password: Option<String>, remember: bool) {
        if let Some(reply) = self.shared.password.lock().unwrap().take() {
            reply.send(password.map(|p| PasswordAnswer { password: Zeroizing::new(p), remember }));
        }
    }

    /// End the session (the stream then ends with `Closed`).
    #[frb(sync)]
    pub fn close(&self) {
        self.send(Cmd::Quit);
    }

    // ---- files ----

    #[frb(sync)]
    pub fn list_dir(&self, path: String) {
        self.send(Cmd::ListDir(path));
    }

    /// Download a file or folder from the computer into `local_dir`.
    #[frb(sync)]
    pub fn download(&self, remote: String, local_dir: String) {
        self.send(Cmd::Pull { remote, local_dir: local_dir.into() });
    }

    /// Send a file to the computer (into its received-files folder).
    #[frb(sync)]
    pub fn upload(&self, path: String) {
        self.send(Cmd::Push(path.into()));
    }

    // ---- terminal ----

    /// Open a terminal (answered with `TerminalOpened`, its id in `id`).
    #[frb(sync)]
    pub fn open_terminal(&self, cols: u16, rows: u16, admin: bool) {
        self.send(Cmd::OpenTerminal { cols, rows, admin });
    }

    #[frb(sync)]
    pub fn terminal_input(&self, id: u32, data: Vec<u8>) -> Result<()> {
        self.terminal(id, TermClient::Input(data))
    }

    #[frb(sync)]
    pub fn terminal_resize(&self, id: u32, cols: u16, rows: u16) -> Result<()> {
        self.terminal(id, TermClient::Resize { cols, rows })
    }

    /// Close a terminal (the shell ends).
    #[frb(sync)]
    pub fn terminal_close(&self, id: u32) {
        self.shared.terminals.lock().unwrap().remove(&id);
    }

    fn terminal(&self, id: u32, m: TermClient) -> Result<()> {
        let t = self.shared.terminals.lock().unwrap();
        t.get(&id)
            .ok_or_else(|| anyhow!("terminal {id} is closed"))?
            .send(m)
            .map_err(|_| anyhow!("terminal {id} is closed"))
    }
}

impl Drop for RdSession {
    fn drop(&mut self) {
        if let Some(h) = self.handle.get_mut().unwrap().take() {
            let _ = h.cmd.send(Cmd::Quit);
        }
    }
}

fn ev(kind: EventKind) -> SessionEvent {
    SessionEvent { kind, ..Default::default() }
}

fn text(kind: EventKind, text: impl Into<String>) -> SessionEvent {
    SessionEvent { kind, text: text.into(), ..Default::default() }
}

/// Turn the connection's events into the app's; terminals get a reader thread each.
fn forward(shared: &Arc<Shared>, sink: &Arc<StreamSink<SessionEvent>>, e: UiEvent) {
    let out = match e {
        UiEvent::Status(s) => SessionEvent {
            kind: EventKind::Status,
            text: s.to_string(),
            ok: matches!(s, conn::Status::Connected { .. }),
            ..Default::default()
        },
        UiEvent::PasswordNeeded { host, error, can_remember, reply } => {
            *shared.password.lock().unwrap() = Some(reply);
            SessionEvent {
                kind: EventKind::PasswordNeeded,
                text: host,
                detail: error.unwrap_or_default(),
                can_remember,
                ..Default::default()
            }
        }
        UiEvent::Notice(n) => text(EventKind::Notice, n),
        UiEvent::Connected(info) => SessionEvent {
            kind: EventKind::Connected,
            text: info.hostname,
            // The computer's system ("windows", "macos", "linux"): shortcuts use ⌘ on a Mac.
            detail: info.os,
            monitors: info.monitors.iter().map(monitor).collect(),
            active_monitor: info.active_monitor,
            ..Default::default()
        },
        UiEvent::Access(a) => SessionEvent {
            kind: EventKind::Access,
            access: Some(Access { screen: a.screen, input: a.input, files: a.files, terminal: a.terminal }),
            ..Default::default()
        },
        UiEvent::Monitors { monitors, active } => SessionEvent {
            kind: EventKind::Monitors,
            monitors: monitors.iter().map(monitor).collect(),
            active_monitor: active,
            ..Default::default()
        },
        UiEvent::FrameUpdated => ev(EventKind::Frame),
        UiEvent::DirListing { path, result } => match result {
            Ok(list) => SessionEvent {
                kind: EventKind::DirListing,
                text: path,
                ok: true,
                entries: list
                    .into_iter()
                    .map(|d| DirItem { name: d.name, is_dir: d.is_dir, size: d.size as i64 })
                    .collect(),
                ..Default::default()
            },
            Err(e) => SessionEvent { kind: EventKind::DirListing, detail: e, ok: false, ..Default::default() },
        },
        UiEvent::Transfer { id, name, done, total, finished } => match finished {
            None => SessionEvent {
                kind: EventKind::TransferProgress,
                id: id as u32,
                text: name,
                x: done as i64,
                y: total as i64,
                ..Default::default()
            },
            Some(r) => SessionEvent {
                kind: EventKind::TransferDone,
                id: id as u32,
                ok: r.is_ok(),
                text: match r {
                    Ok(p) => p,
                    Err(e) => format!("{name}: {e}"),
                },
                ..Default::default()
            },
        },
        UiEvent::Stats { rtt_ms, fps, kbps, route } => SessionEvent {
            kind: EventKind::Stats,
            stats: Some(Stats { rtt_ms, fps, kbps, route: route.to_string() }),
            ..Default::default()
        },
        UiEvent::Fatal(m) => text(EventKind::Fatal, m),
        UiEvent::Reconnecting { reason, attempt } => {
            SessionEvent { kind: EventKind::Reconnecting, text: reason, id: attempt, ..Default::default() }
        }
        UiEvent::Extensions(f) => SessionEvent {
            kind: EventKind::Extensions,
            cursor_supported: f.contains(&rdesk_proto::Feature::Cursor),
            scaling_supported: f.contains(&rdesk_proto::Feature::MaxSize),
            ..Default::default()
        },
        UiEvent::Cursor { x, y, visible } => {
            SessionEvent { kind: EventKind::Cursor, x: x as i64, y: y as i64, visible, ..Default::default() }
        }
        UiEvent::Clipboard(t) => text(EventKind::Clipboard, t),
        UiEvent::Terminal { result: Err(e), .. } => {
            SessionEvent { kind: EventKind::TerminalExited, text: e, ok: false, ..Default::default() }
        }
        UiEvent::Terminal { result: Ok(slot), .. } => {
            let Some(link) = slot.0.lock().unwrap().take() else { return };
            let id = shared.next_terminal.fetch_add(1, Ordering::Relaxed) + 1;
            shared.terminals.lock().unwrap().insert(id, link.tx);
            let (sink2, shared2) = (sink.clone(), shared.clone());
            let mut rx = link.rx;
            std::thread::Builder::new()
                .name(format!("rdesk-term-{id}"))
                .spawn(move || {
                    let mut exit = SessionEvent { kind: EventKind::TerminalExited, id, ok: true, ..Default::default() };
                    while let Some(m) = rx.blocking_recv() {
                        let e = match m {
                            TermHost::Started { shell, user } => SessionEvent {
                                kind: EventKind::TerminalOpened,
                                id,
                                text: if user.is_empty() { shell } else { format!("{shell} · {user}") },
                                ..Default::default()
                            },
                            TermHost::Output(bytes) => {
                                SessionEvent { kind: EventKind::TerminalOutput, id, bytes, ..Default::default() }
                            }
                            TermHost::Exited(code) => {
                                exit.text = format!("exit {code}");
                                exit.ok = code == 0;
                                break;
                            }
                            TermHost::Error(e) => {
                                exit.text = e;
                                exit.ok = false;
                                break;
                            }
                        };
                        if sink2.add(e).is_err() {
                            return;
                        }
                    }
                    shared2.terminals.lock().unwrap().remove(&id);
                    let _ = sink2.add(exit);
                })
                .ok();
            return;
        }
        // The phone draws its own pointer and lets the host choose the quality.
        UiEvent::Wake | UiEvent::Raise(_) | UiEvent::Quality { .. } | UiEvent::CursorShape { .. } => return,
        UiEvent::Quit => ev(EventKind::Closed),
    };
    let _ = sink.add(out);
}

fn monitor(m: &rdesk_proto::MonitorInfo) -> Monitor {
    Monitor { id: m.id, name: m.name.clone(), width: m.width, height: m.height, primary: m.primary }
}
