//! The "Computers" page: an address book of computers with live status, in the spirit
//! of AnyDesk/TeamViewer's start screen.
//!
//! * "My computers" are the hosts saved in client.toml, each known by its host ID.
//! * A background worker probes each one ([`rdesk_common::directory::probe`]) through the
//!   relay and/or its address to see whether rdesk is running and busy.
//! * Every session runs as its own `rdesk connect` process (its own window). The host ID
//!   is checked by the connection itself, so there is no trust dialog.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use egui::{Color32, RichText, Sense, Vec2};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, AuthPref, ClientConfig, HostEntry, NetworkConfig, format_host_id, parse_host_id};
use rdesk_common::directory::{self, ProbeInfo};
use rdesk_common::transport::{self, Endpoint, Role};
use tokio::sync::mpsc;

const REFRESH_EVERY: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const CARD: Vec2 = Vec2::new(260.0, 128.0);

// ---------------------------------------------------------------------------
// Background worker: probes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(super) enum Probe {
    Ok(ProbeInfo),
    Err(String),
}

#[derive(Default, Clone)]
pub(super) struct Snapshot {
    /// The relay as this launcher sees it: `None` = none configured.
    relay: Option<Result<String, String>>,
    /// Keyed by the saved host's name.
    probes: HashMap<String, (Probe, Instant)>,
    agent_keys: Option<Result<Vec<(String, String)>, String>>,
    refreshing: bool,
}

pub(super) enum WorkerCmd {
    Refresh,
    AgentKeys(Option<String>),
}

pub(super) fn spawn_worker(shared: Arc<Mutex<Snapshot>>, rx: mpsc::UnboundedReceiver<WorkerCmd>, ctx: egui::Context) {
    std::thread::Builder::new()
        .name("rdesk-directory".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    shared.lock().unwrap().relay = Some(Err(format!("cannot start worker: {e}")));
                    return;
                }
            };
            rt.block_on(worker(shared, rx, ctx));
        })
        .expect("spawn directory worker");
}

#[derive(Clone)]
struct Prober {
    ep: Endpoint,
    net: NetworkConfig,
    shared: Arc<Mutex<Snapshot>>,
    ctx: egui::Context,
}

impl Prober {
    async fn probe(&self, name: String, entry: HostEntry) {
        let result = async {
            let id = parse_host_id(&entry.id).map_err(|_| anyhow!("no host ID"))?;
            directory::probe(&self.ep, id, Some(&entry.addr), &self.net, PROBE_TIMEOUT).await
        }
        .await;
        let p = match result {
            Ok(info) => Probe::Ok(info),
            Err(e) => Probe::Err(short_error(&e)),
        };
        self.shared.lock().unwrap().probes.insert(name, (p, Instant::now()));
        self.ctx.request_repaint();
    }
}

/// An endpoint for the current network settings (re-created when they change).
async fn prober(current: &mut Option<Prober>, shared: &Arc<Mutex<Snapshot>>, ctx: &egui::Context) -> Option<Prober> {
    let net = config::load_network();
    if let Some(p) = current
        && p.net == net
    {
        return Some(p.clone());
    }
    if let Some(old) = current.take() {
        old.ep.close().await;
    }
    let tuning = load_client_config().transport;
    match transport::bind(None, &net, &tuning, Role::Client).await {
        Ok(ep) => {
            let p = Prober { ep, net: net.clone(), shared: shared.clone(), ctx: ctx.clone() };
            // Report the relay in the background: probes don't wait for it.
            let (ep, shared, ctx) = (p.ep.clone(), shared.clone(), ctx.clone());
            tokio::spawn(async move {
                let url = net.relay.clone().unwrap_or_default();
                let state = match transport::wait_relay(&ep, &net, Duration::from_secs(10)).await {
                    Ok(true) => Some(Ok(url)),
                    Ok(false) => None,
                    Err(e) => Some(Err(format!("{url}: {e:#}"))),
                };
                shared.lock().unwrap().relay = state;
                ctx.request_repaint();
            });
            *current = Some(p.clone());
            Some(p)
        }
        Err(e) => {
            shared.lock().unwrap().relay = Some(Err(format!("{e:#}")));
            None
        }
    }
}

async fn worker(shared: Arc<Mutex<Snapshot>>, mut rx: mpsc::UnboundedReceiver<WorkerCmd>, ctx: egui::Context) {
    let mut current: Option<Prober> = None;
    let mut next = Instant::now();
    loop {
        let wait = tokio::time::sleep_until(next.into());
        tokio::select! {
            _ = wait => {}
            cmd = rx.recv() => match cmd {
                None => break,
                Some(WorkerCmd::Refresh) => {}
                Some(WorkerCmd::AgentKeys(sock)) => {
                    let keys = agent_keys(sock.as_deref()).await.map_err(|e| short_error(&e));
                    shared.lock().unwrap().agent_keys = Some(keys);
                    ctx.request_repaint();
                    continue;
                }
            },
        }

        shared.lock().unwrap().refreshing = true;
        ctx.request_repaint();
        let cfg = load_client_config();
        if let Some(p) = prober(&mut current, &shared, &ctx).await {
            let mut set = tokio::task::JoinSet::new();
            for (name, entry) in cfg.hosts {
                let p = p.clone();
                set.spawn(async move { p.probe(name, entry).await });
            }
            while set.join_next().await.is_some() {}
        }
        shared.lock().unwrap().refreshing = false;
        ctx.request_repaint();
        next = Instant::now() + REFRESH_EVERY;
    }
    if let Some(p) = current {
        p.ep.close().await;
    }
}

async fn agent_keys(sock: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut a = Agent::connect(sock).await?;
    let keys = a.identities().await?;
    if keys.is_empty() {
        return Err(anyhow!("the SSH agent at {} offers no keys", a.path));
    }
    Ok(keys
        .iter()
        .map(|k| (agent::key_fingerprint(k.key_data()), format!("{} {}", k.algorithm(), k.comment())))
        .collect())
}

pub(super) fn load_client_config() -> ClientConfig {
    config::load_toml(config::CLIENT_CONFIG).unwrap_or_default()
}

/// First line of an error chain, without the noisy QUIC prefixes.
fn short_error(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    let s = s.lines().next().unwrap_or_default().to_string();
    if s.contains("timed out") || s.contains("no answer") { "no answer".into() } else { s }
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

const ERROR: Color32 = Color32::from_rgb(230, 70, 70);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Checking,
    Unreachable,
    /// Saved without a valid host ID.
    NoId,
    Ready,
    Busy,
    /// A session window from this launcher is open.
    Open,
}

impl Status {
    fn color(self) -> Color32 {
        match self {
            Status::Ready => Color32::from_rgb(46, 184, 92),
            Status::Busy => Color32::from_rgb(240, 160, 40),
            Status::Open => Color32::from_rgb(70, 140, 240),
            Status::NoId => ERROR,
            Status::Checking | Status::Unreachable => Color32::from_gray(120),
        }
    }

    fn connectable(self) -> bool {
        matches!(self, Status::Ready | Status::Busy | Status::Open)
    }
}

struct Card {
    /// The saved name: what `rdesk connect` gets.
    target: String,
    title: String,
    /// Address, or the start of the ID when there is none.
    addr: String,
    id: String,
    probe: Option<Probe>,
    status: Status,
    detail: String,
}

struct Session {
    target: String,
    child: std::process::Child,
    started: Instant,
    log: PathBuf,
}

struct Editor {
    /// Alias being edited (`None` for a new host).
    original: Option<String>,
    alias: String,
    id: String,
    addr: String,
    auth: AuthPref,
    key: String,
    password_ref: String,
    identity: String,
    agent_socket: String,
    /// The "From 1Password…" key list is open.
    show_keys: bool,
    /// A password for this host is in the system credential store.
    saved_password: bool,
    /// Typed here to be saved in the credential store.
    new_password: zeroize::Zeroizing<String>,
    forget_password: bool,
    error: Option<String>,
}

impl Editor {
    fn new(alias: String, id: String) -> Self {
        Self {
            original: None,
            alias,
            id,
            addr: String::new(),
            auth: AuthPref::Auto,
            key: String::new(),
            password_ref: String::new(),
            identity: String::new(),
            agent_socket: String::new(),
            show_keys: false,
            saved_password: false,
            new_password: Default::default(),
            forget_password: false,
            error: None,
        }
    }

    fn edit(alias: &str, e: &HostEntry) -> Self {
        Self {
            original: Some(alias.to_string()),
            alias: alias.to_string(),
            id: e.id.clone(),
            addr: e.addr.clone(),
            auth: e.auth,
            key: e.key.clone().unwrap_or_default(),
            password_ref: e.password_ref.clone().unwrap_or_default(),
            identity: e.identity.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            agent_socket: e.agent_socket.clone().unwrap_or_default(),
            show_keys: false,
            saved_password: crate::secrets::has(alias),
            new_password: Default::default(),
            forget_password: false,
            error: None,
        }
    }
}

enum Dialog {
    Edit(Box<Editor>),
    Remove(String),
}

pub(super) struct Computers {
    exe: PathBuf,
    shared: Arc<Mutex<Snapshot>>,
    tx: mpsc::UnboundedSender<WorkerCmd>,
    cfg: ClientConfig,
    loaded: Instant,
    search: String,
    quick: String,
    dialog: Option<Dialog>,
    sessions: Vec<Session>,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl Computers {
    pub(super) fn new(exe: PathBuf, shared: Arc<Mutex<Snapshot>>, tx: mpsc::UnboundedSender<WorkerCmd>) -> Self {
        let mut l = Self {
            exe,
            shared,
            tx,
            cfg: ClientConfig::default(),
            loaded: Instant::now(),
            search: String::new(),
            quick: String::new(),
            dialog: None,
            sessions: Vec::new(),
            toast: None,
        };
        l.reload();
        l
    }

    /// Re-read client.toml (the CLI may change it behind our back).
    fn reload(&mut self) {
        match config::load_toml::<ClientConfig>(config::CLIENT_CONFIG) {
            Ok(c) => self.cfg = c,
            Err(e) => self.notify(format!("client.toml: {e:#}"), true),
        }
        self.loaded = Instant::now();
    }

    fn notify(&mut self, text: String, error: bool) {
        self.toast = Some((text, error, Instant::now()));
    }

    fn refresh(&self) {
        let _ = self.tx.send(WorkerCmd::Refresh);
    }

    fn session_open(&self, target: &str) -> bool {
        self.sessions.iter().any(|s| s.target == target)
    }

    fn cards(&self, snap: &Snapshot) -> Vec<Card> {
        self.cfg
            .hosts
            .iter()
            .map(|(alias, e)| {
                let probe = snap.probes.get(alias).map(|(p, _)| p.clone());
                self.card(alias, e, probe)
            })
            .collect()
    }

    fn card(&self, alias: &str, e: &HostEntry, probe: Option<Probe>) -> Card {
        let valid_id = parse_host_id(&e.id).is_ok();
        let (status, detail) = match &probe {
            _ if self.session_open(alias) => (Status::Open, "Session open".to_string()),
            _ if !valid_id => (Status::NoId, "No host ID: edit it".into()),
            Some(Probe::Ok(info)) => {
                let route = info.route.map(|r| format!(" · {}", r.label())).unwrap_or_default();
                let rtt = format!("{} ms{route}", info.rtt.as_millis());
                if info.in_session {
                    (Status::Busy, format!("In use · {rtt}"))
                } else {
                    (Status::Ready, format!("Ready · {rtt}"))
                }
            }
            Some(Probe::Err(err)) => (Status::Unreachable, err.clone()),
            None => (Status::Checking, "Checking…".into()),
        };
        let addr = if e.addr.trim().is_empty() {
            format!("ID {}…", e.id.chars().take(12).collect::<String>())
        } else {
            e.addr.clone()
        };
        Card { target: alias.to_string(), title: alias.to_string(), addr, id: e.id.clone(), probe, status, detail }
    }

    // ---- actions ----------------------------------------------------------

    fn connect(&mut self, target: &str) {
        self.open(target, false);
    }

    /// Open a session window (`files`: start on the file-transfer panel). `target` is a saved
    /// name or `ID[@address]`; the connection itself verifies the host ID.
    fn open(&mut self, target: &str, files: bool) {
        let known = self.cfg.hosts.contains_key(target);
        let id_part = target.split('@').next().unwrap_or_default();
        if !known && parse_host_id(id_part).is_err() {
            self.notify(format!("{target}: not a saved computer nor a host ID (ID or ID@address)."), true);
            return;
        }
        self.spawn_session(target, files);
    }

    fn spawn_session(&mut self, target: &str, files: bool) {
        if self.session_open(target) {
            let hint = if files { " Use Files in its ▼ rdesk menu." } else { "" };
            self.notify(format!("A window for {target} is already open.{hint}"), false);
            return;
        }
        let log = match session_log(target) {
            Ok(p) => p,
            Err(e) => {
                self.notify(format!("Cannot create the session log: {e:#}"), true);
                return;
            }
        };
        let stderr = match std::fs::File::create(&log) {
            Ok(f) => std::process::Stdio::from(f),
            Err(_) => std::process::Stdio::null(),
        };
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.args(["connect", target]);
        if files {
            cmd.arg("--files");
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(stderr);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.spawn() {
            Ok(child) => {
                self.sessions.push(Session { target: target.to_string(), child, started: Instant::now(), log });
                self.notify(format!("Opening {target}…"), false);
            }
            Err(e) => self.notify(format!("Cannot start {}: {e}", self.exe.display()), true),
        }
    }

    /// Notice session windows that closed; report the ones that died right away.
    fn reap_sessions(&mut self) {
        let mut failed = Vec::new();
        self.sessions.retain_mut(|s| match s.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                if !status.success() && s.started.elapsed() < Duration::from_secs(30) {
                    failed.push((s.target.clone(), last_error_line(&s.log)));
                }
                false
            }
            Err(_) => false,
        });
        for (t, why) in failed {
            self.notify(format!("Session to {t} failed: {why}"), true);
        }
    }

    fn save_editor(&mut self, ed: &mut Editor) -> bool {
        let alias = ed.alias.trim().to_string();
        let addr = ed.addr.trim().to_string();
        if alias.is_empty() || alias.chars().any(char::is_whitespace) {
            ed.error = Some("The name must be one word (no spaces).".into());
            return false;
        }
        let id = match parse_host_id(&ed.id) {
            Ok(id) => format_host_id(&id),
            Err(_) => {
                ed.error =
                    Some("Paste the host ID shown on that computer (This computer page, or `rdesk host id`).".into());
                return false;
            }
        };
        if addr.is_empty() && config::load_network().relay.is_none() {
            ed.error = Some("Without a relay (Settings → Network) the address is needed: host or host:port.".into());
            return false;
        }
        let mut cfg = load_client_config();
        if ed.original.as_deref() != Some(alias.as_str()) && cfg.hosts.contains_key(&alias) {
            ed.error = Some(format!("There is already a computer called {alias}."));
            return false;
        }
        let mut entry = ed.original.as_ref().and_then(|o| cfg.hosts.remove(o)).unwrap_or_default();
        let opt = |s: &str| if s.trim().is_empty() { None } else { Some(s.trim().to_string()) };
        entry.id = id;
        entry.addr = addr;
        entry.auth = ed.auth;
        entry.key = opt(&ed.key);
        entry.password_ref = opt(&ed.password_ref);
        entry.agent_socket = opt(&ed.agent_socket);
        entry.identity = opt(&ed.identity).map(PathBuf::from);
        cfg.hosts.insert(alias.clone(), entry);

        let res = config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &cfg));
        if let Err(e) = res {
            ed.error = Some(format!("Saving: {e:#}"));
            return false;
        }
        // The remembered password follows the host's name.
        let secret = (|| {
            if let Some(old) = &ed.original
                && *old != alias
            {
                crate::secrets::rename(old, &alias)?;
            }
            if ed.forget_password {
                crate::secrets::forget(&alias)?;
            }
            if !ed.new_password.is_empty() {
                crate::secrets::set(&alias, &ed.new_password)?;
            }
            anyhow::Ok(())
        })();
        if let Err(e) = secret {
            self.notify(format!("{e:#}"), true);
        }
        self.reload();
        self.refresh();
        self.notify(format!("Saved {alias}."), false);
        true
    }

    fn remove(&mut self, alias: &str) {
        let mut cfg = load_client_config();
        cfg.hosts.remove(alias);
        let res = config::save_toml(config::CLIENT_CONFIG, &cfg).and_then(|_| crate::secrets::forget(alias));
        match res {
            Ok(()) => self.notify(format!("Removed {alias}."), false),
            Err(e) => self.notify(format!("Removing {alias}: {e:#}"), true),
        }
        self.reload();
    }

    // ---- drawing -----------------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.quick)
                    .hint_text("Name, or host ID[@address]…")
                    .desired_width(260.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Connect").clicked() || enter) && !self.quick.trim().is_empty() {
                let t = self.quick.trim().to_string();
                self.connect(&t);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳").on_hover_text("Refresh now").clicked() {
                    self.reload();
                    self.refresh();
                }
                if snap.refreshing {
                    ui.spinner();
                }
                match &snap.relay {
                    Some(Ok(url)) => ui.weak(format!("Relay: {url}")),
                    Some(Err(e)) => ui.colored_label(ERROR, format!("Relay unreachable ({e})")),
                    None => ui.weak("No relay: direct addresses only"),
                };
            });
        });
    }

    fn section(&mut self, ui: &mut egui::Ui, cards: &[Card], empty: &str) {
        let needle = self.search.trim().to_lowercase();
        let visible: Vec<&Card> = cards
            .iter()
            .filter(|c| {
                needle.is_empty() || c.title.to_lowercase().contains(&needle) || c.addr.to_lowercase().contains(&needle)
            })
            .collect();
        if visible.is_empty() {
            ui.weak(empty);
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(10.0);
            for c in visible {
                self.card_ui(ui, c);
            }
        });
    }

    fn card_ui(&mut self, ui: &mut egui::Ui, c: &Card) {
        let fill = ui.visuals().faint_bg_color;
        let resp = ui.scope_builder(egui::UiBuilder::new().id_salt(&c.target).sense(Sense::click()), |ui| {
            egui::Frame::group(ui.style())
                .fill(fill)
                .corner_radius(10.0)
                .inner_margin(12.0)
                .stroke(egui::Stroke::new(1.0, c.status.color().gamma_multiply(0.6)))
                .show(ui, |ui| {
                    ui.set_min_size(CARD);
                    ui.set_max_width(CARD.x);
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        ui.painter().circle_filled(rect.center(), 6.0, c.status.color());
                        ui.label(RichText::new(os_icon(c)).size(18.0));
                        ui.label(RichText::new(&c.title).strong().size(16.0));
                    });
                    ui.label(RichText::new(&c.addr).weak().small());
                    ui.label(RichText::new(&c.detail).color(c.status.color()));
                    if let Some(Probe::Ok(info)) = &c.probe {
                        ui.label(RichText::new(describe(info)).weak().small());
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let connect = ui.add_enabled(
                            c.status != Status::NoId,
                            egui::Button::new(if c.status == Status::Open { "Focus" } else { "Connect" }),
                        );
                        if connect.clicked() {
                            self.connect(&c.target);
                        }
                        if ui
                            .add_enabled(c.status.connectable(), egui::Button::new("Files").small())
                            .on_hover_text("Open a session on the file-transfer panel")
                            .clicked()
                        {
                            self.open(&c.target, true);
                        }
                        if ui.small_button("Edit").clicked()
                            && let Some(e) = self.cfg.hosts.get(&c.target)
                        {
                            self.dialog = Some(Dialog::Edit(Box::new(Editor::edit(&c.target, e))));
                        }
                    });
                });
        });
        let resp = resp.response;
        if resp.double_clicked() && c.status != Status::NoId {
            self.connect(&c.target);
        }
        resp.context_menu(|ui| {
            if ui.button("Connect").clicked() {
                self.connect(&c.target);
                ui.close();
            }
            if ui.button("Copy address").clicked() {
                ui.ctx().copy_text(c.addr.clone());
                ui.close();
            }
            if ui.button("Files…").clicked() {
                self.open(&c.target, true);
                ui.close();
            }
            if !c.id.is_empty() && ui.button("Copy host ID").clicked() {
                ui.ctx().copy_text(c.id.clone());
                ui.close();
            }
            ui.separator();
            if ui.button("Edit…").clicked() {
                if let Some(e) = self.cfg.hosts.get(&c.target) {
                    self.dialog = Some(Dialog::Edit(Box::new(Editor::edit(&c.target, e))));
                }
                ui.close();
            }
            if ui.button("Remove…").clicked() {
                self.dialog = Some(Dialog::Remove(c.target.clone()));
                ui.close();
            }
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        match &mut dialog {
            Dialog::Edit(ed) => {
                let title = if ed.original.is_some() { "Edit computer" } else { "Add computer" };
                let m = egui::Modal::new(egui::Id::unique("edit-host")).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.heading(title);
                    ui.add_space(6.0);
                    egui::Grid::new("edit-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label("Name");
                        ui.add(egui::TextEdit::singleline(&mut ed.alias).hint_text("casa"));
                        ui.end_row();
                        ui.label("Host ID");
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.id)
                                .hint_text("64 characters, from that computer's This computer page")
                                .font(egui::TextStyle::Monospace)
                                .desired_width(330.0),
                        );
                        ui.end_row();
                        ui.label("Address");
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.addr)
                                .hint_text("optional with a relay: 192.168.0.10 or host:port")
                                .desired_width(330.0),
                        );
                        ui.end_row();
                        ui.label("Login");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut ed.auth, AuthPref::Auto, "Key, then password");
                            ui.radio_value(&mut ed.auth, AuthPref::Agent, "1Password key");
                            ui.radio_value(&mut ed.auth, AuthPref::Password, "Password");
                        });
                        ui.end_row();
                        if ed.auth != AuthPref::Password {
                            ui.label("SSH key");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut ed.key)
                                        .hint_text("any key the host accepts")
                                        .desired_width(250.0),
                                );
                                if ui.button("From 1Password…").clicked() {
                                    let sock = if ed.agent_socket.trim().is_empty() {
                                        None
                                    } else {
                                        Some(ed.agent_socket.trim().to_string())
                                    };
                                    self.shared.lock().unwrap().agent_keys = None;
                                    let _ = self.tx.send(WorkerCmd::AgentKeys(sock));
                                    ed.show_keys = true;
                                }
                            });
                            ui.end_row();
                            if ed.show_keys {
                                ui.label("");
                                ui.vertical(|ui| match &snap.agent_keys {
                                    None => {
                                        ui.spinner();
                                    }
                                    Some(Err(e)) => {
                                        ui.colored_label(ERROR, e);
                                    }
                                    Some(Ok(keys)) => {
                                        for (fp, label) in keys {
                                            if ui.selectable_label(ed.key == *fp, format!("{label}\n{fp}")).clicked() {
                                                ed.key = fp.clone();
                                                ed.show_keys = false;
                                            }
                                        }
                                    }
                                });
                                ui.end_row();
                            }
                        }
                        if ed.auth != AuthPref::Agent {
                            ui.label("1Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.password_ref)
                                    .hint_text("op://Private/rdesk/password (optional)")
                                    .desired_width(330.0),
                            );
                            ui.end_row();
                            if crate::secrets::available() {
                                ui.label("Saved password");
                                ui.horizontal(|ui| {
                                    if ed.saved_password && !ed.forget_password {
                                        ui.label(format!("✔ in {}", crate::secrets::store_name()));
                                        if ui.small_button("Forget").clicked() {
                                            ed.forget_password = true;
                                        }
                                    } else {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut *ed.new_password)
                                                .password(true)
                                                .hint_text("type to save it")
                                                .desired_width(250.0),
                                        );
                                    }
                                });
                                ui.end_row();
                            }
                        }
                    });
                    egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
                        egui::Grid::new("edit-adv").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                            ui.label("Agent socket");
                            ui.add(egui::TextEdit::singleline(&mut ed.agent_socket).hint_text("default: 1Password"));
                            ui.end_row();
                            ui.label("Key file");
                            ui.add(egui::TextEdit::singleline(&mut ed.identity).hint_text("unencrypted OpenSSH key"));
                            ui.end_row();
                        });
                    });
                    if ed.auth != AuthPref::Agent {
                        ui.weak(
                            "Order used: 1Password reference, then the saved password, then a dialog \
                             when connecting (with an option to remember it).",
                        );
                    }
                    if let Some(e) = &ed.error {
                        ui.colored_label(ERROR, e);
                    }
                    ui.add_space(6.0);
                    let mut close = false;
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() && self.save_editor(ed) {
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                    close
                });
                if m.inner || m.should_close() {
                    keep = false;
                }
            }
            Dialog::Remove(alias) => {
                let m = egui::Modal::new(egui::Id::unique("remove-host")).show(ctx, |ui| {
                    ui.heading(format!("Remove {alias}?"));
                    ui.label("Its saved settings are deleted. The computer itself is not touched.");
                    ui.add_space(6.0);
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button("Remove").clicked() {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            done = Some(false);
                        }
                    });
                    done
                });
                match m.inner {
                    Some(true) => {
                        let a = alias.clone();
                        self.remove(&a);
                        keep = false;
                    }
                    Some(false) => keep = false,
                    None if m.should_close() => keep = false,
                    None => {}
                }
            }
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }
}

impl Computers {
    pub(super) fn open_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Bookkeeping that must run even while the window is hidden.
    pub(super) fn tick(&mut self) {
        self.reap_sessions();
        if self.loaded.elapsed() > REFRESH_EVERY && self.dialog.is_none() {
            self.reload();
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let snap = self.shared.lock().unwrap().clone();
        let saved = self.cards(&snap);

        self.header(ui, &snap);
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Search").desired_width(220.0));
                if ui.button("＋ Add computer").clicked() {
                    self.dialog = Some(Dialog::Edit(Box::new(Editor::new(String::new(), String::new()))));
                }
            });
            ui.add_space(10.0);
            ui.label(RichText::new("My computers").strong().size(17.0));
            ui.add_space(4.0);
            self.section(
                ui,
                &saved,
                "No computers yet. Add one with its host ID (shown on that computer's This computer page).",
            );
        });
        self.dialogs(&ctx, &snap);
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn describe(info: &ProbeInfo) -> String {
    format!("{} · {} · rdesk {}", info.host_name, pretty_os(&info.os), info.version)
}

fn pretty_os(os: &str) -> &str {
    match os.to_ascii_lowercase().as_str() {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        "ios" => "iOS",
        "android" => "Android",
        _ => os,
    }
}

fn os_icon(c: &Card) -> &'static str {
    let os = match &c.probe {
        Some(Probe::Ok(i)) => i.os.to_ascii_lowercase(),
        _ => String::new(),
    };
    match os.as_str() {
        "macos" => "💻",
        "ios" | "android" => "📱",
        _ => "🖥",
    }
}

fn session_log(target: &str) -> Result<PathBuf> {
    let dir = config::ensure_config_dir()?.join("logs");
    std::fs::create_dir_all(&dir)?;
    let safe: String =
        target.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    Ok(dir.join(format!("session-{safe}.log")))
}

/// The most useful line of a session's log: the last error, else the last line.
fn last_error_line(log: &PathBuf) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let clean = |l: &str| {
        // Strip ANSI colors from tracing output.
        let mut out = String::new();
        let mut esc = false;
        for ch in l.chars() {
            match (esc, ch) {
                (false, '\u{1b}') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out.trim().to_string()
    };
    let lines: Vec<String> = text.lines().map(clean).filter(|l| !l.is_empty()).collect();
    lines
        .iter()
        .rev()
        .find(|l| l.starts_with("Error") || l.contains("ERROR"))
        .or(lines.last())
        .cloned()
        .unwrap_or_else(|| format!("see {}", log.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_log_names_are_safe() {
        let p = session_log("abc@10.0.0.1:7420").unwrap();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "session-abc_10.0.0.1_7420.log");
    }
}
