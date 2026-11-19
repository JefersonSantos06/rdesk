//! The "Computers" page: an address book of computers with live status, in the spirit
//! of AnyDesk/TeamViewer's start screen.
//!
//! * "My computers" are the hosts saved in client.toml.
//! * "Tailnet" lists the other devices Tailscale knows about, so a new machine shows up
//!   without typing its address.
//! * A background worker asks Tailscale who is online and probes each machine
//!   ([`rdesk_common::directory::probe`]) to see whether rdesk is running and busy.
//! * Every session runs as its own `rdesk connect` process (its own window); the
//!   launcher handles first-contact trust with a dialog instead of a terminal prompt.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use egui::{Color32, RichText, Sense, Vec2};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, AuthPref, ClientConfig, HostEntry, KnownHosts, format_fingerprint};
use rdesk_common::directory::{self, ProbeInfo, Tailnet, TailnetDevice};
use rdesk_common::transport;
use tokio::sync::mpsc;

const REFRESH_EVERY: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const CARD: Vec2 = Vec2::new(260.0, 128.0);

// ---------------------------------------------------------------------------
// Background worker: Tailscale status + probes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(super) enum Probe {
    Ok(ProbeInfo),
    Err(String),
}

#[derive(Default, Clone)]
pub(super) struct Snapshot {
    tailnet: Option<Result<Tailnet, String>>,
    /// Keyed by the address string that was probed (a saved host's `addr` or a device IP).
    probes: HashMap<String, (Probe, Instant)>,
    agent_keys: Option<Result<Vec<(String, String)>, String>>,
    refreshing: bool,
}

pub(super) enum WorkerCmd {
    Refresh,
    ProbeNow(String),
    AgentKeys(Option<String>),
}

pub(super) fn spawn_worker(shared: Arc<Mutex<Snapshot>>, rx: mpsc::UnboundedReceiver<WorkerCmd>, ctx: egui::Context) {
    std::thread::Builder::new()
        .name("rdesk-directory".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    shared.lock().unwrap().tailnet = Some(Err(format!("cannot start worker: {e}")));
                    return;
                }
            };
            rt.block_on(worker(shared, rx, ctx));
        })
        .expect("spawn directory worker");
}

#[derive(Clone)]
struct Prober {
    v4: Option<quinn::Endpoint>,
    v6: Option<quinn::Endpoint>,
    shared: Arc<Mutex<Snapshot>>,
    ctx: egui::Context,
}

impl Prober {
    async fn probe(&self, target: String, tuning: config::TransportTuning) {
        let result = async {
            let addr: SocketAddr = transport::resolve(&target, rdesk_proto::DEFAULT_PORT).await?;
            let ep = if addr.is_ipv6() { self.v6.as_ref() } else { self.v4.as_ref() }
                .ok_or_else(|| anyhow!("no {} socket", if addr.is_ipv6() { "IPv6" } else { "IPv4" }))?;
            directory::probe(ep, addr, &tuning, PROBE_TIMEOUT).await
        }
        .await;
        let p = match result {
            Ok(info) => Probe::Ok(info),
            Err(e) => Probe::Err(short_error(&e)),
        };
        self.shared.lock().unwrap().probes.insert(target, (p, Instant::now()));
        self.ctx.request_repaint();
    }
}

async fn worker(shared: Arc<Mutex<Snapshot>>, mut rx: mpsc::UnboundedReceiver<WorkerCmd>, ctx: egui::Context) {
    let prober = Prober {
        v4: transport::client_endpoint(&"0.0.0.0:0".parse().unwrap()).ok(),
        v6: transport::client_endpoint(&"[::]:0".parse().unwrap()).ok(),
        shared: shared.clone(),
        ctx: ctx.clone(),
    };
    let mut next = Instant::now();
    loop {
        let wait = tokio::time::sleep_until(next.into());
        tokio::select! {
            _ = wait => {}
            cmd = rx.recv() => match cmd {
                None => return,
                Some(WorkerCmd::Refresh) => {}
                Some(WorkerCmd::ProbeNow(target)) => {
                    let p = prober.clone();
                    let tuning = load_client_config().transport;
                    tokio::spawn(async move { p.probe(target, tuning).await });
                    continue;
                }
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
        let tailnet = directory::tailnet_status().await;
        let targets = probe_targets(&cfg, tailnet.as_ref().ok());
        shared.lock().unwrap().tailnet = Some(tailnet.map_err(|e| short_error(&e)));
        ctx.request_repaint();

        let mut set = tokio::task::JoinSet::new();
        for t in targets {
            let p = prober.clone();
            let tuning = cfg.transport.clone();
            set.spawn(async move { p.probe(t, tuning).await });
        }
        while set.join_next().await.is_some() {}
        shared.lock().unwrap().refreshing = false;
        ctx.request_repaint();
        next = Instant::now() + REFRESH_EVERY;
    }
}

/// Saved hosts (unless Tailscale says they are offline) and every online tailnet device.
fn probe_targets(cfg: &ClientConfig, tailnet: Option<&Tailnet>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for e in cfg.hosts.values() {
        let offline = tailnet.and_then(|t| t.find(&e.addr)).is_some_and(|d| !d.online);
        if !offline && seen.insert(e.addr.clone()) {
            out.push(e.addr.clone());
        }
    }
    for d in tailnet.map(|t| t.devices.as_slice()).unwrap_or_default() {
        if let Some(ip) = d.ip()
            && d.online
            && seen.insert(ip.to_string())
        {
            out.push(ip.to_string());
        }
    }
    out
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

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Checking,
    Offline,
    /// Online in Tailscale, but nothing answers on the rdesk port.
    NoRdesk,
    Unreachable,
    Ready,
    Busy,
    IdentityChanged,
    /// A session window from this launcher is open.
    Open,
}

impl Status {
    fn color(self) -> Color32 {
        match self {
            Status::Ready => Color32::from_rgb(46, 184, 92),
            Status::Busy => Color32::from_rgb(240, 160, 40),
            Status::Open => Color32::from_rgb(70, 140, 240),
            Status::IdentityChanged => Color32::from_rgb(230, 70, 70),
            Status::NoRdesk => Color32::from_rgb(170, 150, 90),
            Status::Checking | Status::Offline | Status::Unreachable => Color32::from_gray(120),
        }
    }

    fn connectable(self) -> bool {
        matches!(self, Status::Ready | Status::Busy | Status::Open)
    }
}

struct Card {
    /// What `rdesk connect` gets, and the known_hosts key: alias, MagicDNS name or IP.
    target: String,
    title: String,
    addr: String,
    saved: bool,
    device: Option<TailnetDevice>,
    probe: Option<Probe>,
    known_fp: Option<[u8; 32]>,
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
    fn new(alias: String, addr: String) -> Self {
        Self {
            original: None,
            alias,
            addr,
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
    Edit(Editor),
    Remove(String),
    Trust { target: String, fp: [u8; 32], old: Option<[u8; 32]>, about: String, files: bool },
}

pub(super) struct Computers {
    exe: PathBuf,
    shared: Arc<Mutex<Snapshot>>,
    tx: mpsc::UnboundedSender<WorkerCmd>,
    cfg: ClientConfig,
    known: KnownHosts,
    loaded: Instant,
    search: String,
    quick: String,
    show_offline: bool,
    dialog: Option<Dialog>,
    sessions: Vec<Session>,
    /// Waiting for a probe of this target before connecting (trust check); `bool` = files.
    pending_connect: Option<(String, Instant, bool)>,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl Computers {
    pub(super) fn new(exe: PathBuf, shared: Arc<Mutex<Snapshot>>, tx: mpsc::UnboundedSender<WorkerCmd>) -> Self {
        let mut l = Self {
            exe,
            shared,
            tx,
            cfg: ClientConfig::default(),
            known: KnownHosts::default(),
            loaded: Instant::now(),
            search: String::new(),
            quick: String::new(),
            show_offline: false,
            dialog: None,
            sessions: Vec::new(),
            pending_connect: None,
            toast: None,
        };
        l.reload();
        l
    }

    /// Re-read client.toml and known_hosts (the CLI may change them behind our back).
    fn reload(&mut self) {
        match config::load_toml::<ClientConfig>(config::CLIENT_CONFIG) {
            Ok(c) => self.cfg = c,
            Err(e) => self.notify(format!("client.toml: {e:#}"), true),
        }
        self.known = KnownHosts::load().unwrap_or_default();
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

    fn cards(&self, snap: &Snapshot) -> (Vec<Card>, Vec<Card>) {
        let tailnet = snap.tailnet.as_ref().and_then(|t| t.as_ref().ok());
        let probe_of = |key: &str| snap.probes.get(key).map(|(p, _)| p.clone());
        let mut claimed: HashSet<String> = HashSet::new();

        let mut saved = Vec::new();
        for (alias, e) in &self.cfg.hosts {
            let device = tailnet.and_then(|t| t.find(&e.addr)).cloned();
            if let Some(d) = &device {
                claimed.insert(d.dns_name.clone());
            }
            let probe = probe_of(&e.addr);
            saved.push(self.card(alias.clone(), alias.clone(), e.addr.clone(), true, device, probe));
        }

        let mut others = Vec::new();
        for d in tailnet.map(|t| t.devices.as_slice()).unwrap_or_default() {
            if d.is_self || claimed.contains(&d.dns_name) {
                continue;
            }
            let ip = d.ip().map(|i| i.to_string()).unwrap_or_default();
            let target = if d.dns_name.is_empty() { ip.clone() } else { d.dns_name.clone() };
            let probe = probe_of(&ip);
            others.push(self.card(target, d.name.clone(), ip, false, Some(d.clone()), probe));
        }
        (saved, others)
    }

    fn card(
        &self,
        target: String,
        title: String,
        addr: String,
        saved: bool,
        device: Option<TailnetDevice>,
        probe: Option<Probe>,
    ) -> Card {
        let known_fp = self.known.entries.get(&target).copied();
        let now = unix_now();
        let (status, detail) = match (&device, &probe) {
            _ if self.session_open(&target) => (Status::Open, "Session open".to_string()),
            (Some(d), _) if !d.online => (
                Status::Offline,
                match d.last_seen {
                    Some(t) => format!("Offline · seen {}", directory::ago(t, now)),
                    None => "Offline".into(),
                },
            ),
            (_, Some(Probe::Ok(info))) => {
                let link = match &device {
                    Some(d) if d.direct => " · direct".to_string(),
                    Some(d) if !d.relay.is_empty() => format!(" · relay {}", d.relay),
                    _ => String::new(),
                };
                let rtt = format!("{} ms{link}", info.rtt.as_millis());
                match known_fp {
                    Some(k) if k != info.fingerprint => (Status::IdentityChanged, "Identity changed!".into()),
                    _ if info.in_session => (Status::Busy, format!("In use · {rtt}")),
                    None => (Status::Ready, format!("Ready (new) · {rtt}")),
                    Some(_) => (Status::Ready, format!("Ready · {rtt}")),
                }
            }
            (Some(_), Some(Probe::Err(_))) => (Status::NoRdesk, "Online · rdesk not running".into()),
            (None, Some(Probe::Err(e))) => (Status::Unreachable, e.clone()),
            (_, None) => (Status::Checking, "Checking…".into()),
        };
        Card { target, title, addr, saved, device, probe, known_fp, status, detail }
    }

    // ---- actions ----------------------------------------------------------

    fn connect(&mut self, target: &str) {
        self.open(target, false);
    }

    /// Open a session window (`files`: start on the file-transfer panel), checking the
    /// host's identity first.
    fn open(&mut self, target: &str, files: bool) {
        let snap = self.shared.lock().unwrap();
        let addr = self.cfg.hosts.get(target).map(|e| e.addr.clone()).unwrap_or_else(|| target.to_string());
        let device_ip = snap
            .tailnet
            .as_ref()
            .and_then(|t| t.as_ref().ok())
            .and_then(|t| t.find(&addr))
            .and_then(|d| d.ip())
            .map(|ip| ip.to_string());
        let probe = snap
            .probes
            .get(&addr)
            .or_else(|| device_ip.as_ref().and_then(|ip| snap.probes.get(ip)))
            .map(|(p, _)| p.clone());
        drop(snap);
        let known = self.known.entries.get(target).copied();

        match (known, probe) {
            (Some(k), Some(Probe::Ok(info))) if k != info.fingerprint => {
                self.dialog = Some(Dialog::Trust {
                    target: target.to_string(),
                    fp: info.fingerprint,
                    old: Some(k),
                    about: describe(&info),
                    files,
                });
            }
            (None, Some(Probe::Ok(info))) => {
                self.dialog = Some(Dialog::Trust {
                    target: target.to_string(),
                    fp: info.fingerprint,
                    old: None,
                    about: describe(&info),
                    files,
                });
            }
            (None, Some(Probe::Err(e))) if self.pending_connect.is_some() => {
                self.pending_connect = None;
                self.notify(format!("Cannot reach {target} to check its identity: {e}"), true);
            }
            (None, _) => {
                // Never seen and no fresh probe: ask for one, then come back here.
                self.pending_connect = Some((target.to_string(), Instant::now(), files));
                let _ = self.tx.send(WorkerCmd::ProbeNow(addr));
            }
            (Some(_), _) => self.spawn_session(target, files),
        }
    }

    fn trust_and_connect(&mut self, target: &str, fp: [u8; 32], files: bool) {
        self.known.entries.insert(target.to_string(), fp);
        let res = config::ensure_config_dir().and_then(|_| self.known.save());
        match res {
            Ok(()) => self.spawn_session(target, files),
            Err(e) => self.notify(format!("Saving known_hosts: {e:#}"), true),
        }
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
        if addr.is_empty() {
            ed.error = Some("Enter an address: MagicDNS name, Tailscale IP, or host:port.".into());
            return false;
        }
        let mut cfg = load_client_config();
        if ed.original.as_deref() != Some(alias.as_str()) && cfg.hosts.contains_key(&alias) {
            ed.error = Some(format!("There is already a computer called {alias}."));
            return false;
        }
        let mut entry = ed.original.as_ref().and_then(|o| cfg.hosts.remove(o)).unwrap_or_default();
        let opt = |s: &str| if s.trim().is_empty() { None } else { Some(s.trim().to_string()) };
        entry.addr = addr;
        entry.auth = ed.auth;
        entry.key = opt(&ed.key);
        entry.password_ref = opt(&ed.password_ref);
        entry.agent_socket = opt(&ed.agent_socket);
        entry.identity = opt(&ed.identity).map(PathBuf::from);
        cfg.hosts.insert(alias.clone(), entry);

        let mut known = KnownHosts::load().unwrap_or_default();
        if let Some(old) = &ed.original
            && *old != alias
            && let Some(fp) = known.entries.remove(old)
        {
            known.entries.insert(alias.clone(), fp);
        }
        let res = config::ensure_config_dir()
            .and_then(|_| config::save_toml(config::CLIENT_CONFIG, &cfg))
            .and_then(|_| known.save());
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
        let mut known = KnownHosts::load().unwrap_or_default();
        known.entries.remove(alias);
        let res = config::save_toml(config::CLIENT_CONFIG, &cfg)
            .and_then(|_| known.save())
            .and_then(|_| crate::secrets::forget(alias));
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
                    .hint_text("Name or address to connect…")
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
                ui.label(self_summary(snap));
            });
        });
        match &snap.tailnet {
            Some(Err(e)) => {
                ui.colored_label(Status::IdentityChanged.color(), format!("Tailscale: {e}"));
            }
            Some(Ok(t)) if !t.running() => {
                ui.colored_label(
                    Status::Busy.color(),
                    format!("Tailscale is {} — start it and log in to see your computers.", t.state),
                );
            }
            _ => {}
        }
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
                            c.status.connectable() || c.status == Status::IdentityChanged,
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
                        if c.saved {
                            if ui.small_button("Edit").clicked()
                                && let Some(e) = self.cfg.hosts.get(&c.target)
                            {
                                self.dialog = Some(Dialog::Edit(Editor::edit(&c.target, e)));
                            }
                        } else if ui.small_button("Save").clicked() {
                            let alias = c.title.to_lowercase().replace(char::is_whitespace, "-");
                            self.dialog = Some(Dialog::Edit(Editor::new(alias, c.target.clone())));
                        }
                    });
                });
        });
        let resp = resp.response;
        if resp.double_clicked() && c.status.connectable() {
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
            if let Some(fp) = c.known_fp {
                if ui.button("Copy trusted fingerprint").clicked() {
                    ui.ctx().copy_text(format_fingerprint(&fp));
                    ui.close();
                }
                if ui.button("Forget trusted fingerprint").on_hover_text("Ask again on the next connection").clicked() {
                    self.known.entries.remove(&c.target);
                    if let Err(e) = self.known.save() {
                        self.notify(format!("Saving known_hosts: {e:#}"), true);
                    }
                    ui.close();
                }
            }
            if c.saved {
                ui.separator();
                if ui.button("Edit…").clicked() {
                    if let Some(e) = self.cfg.hosts.get(&c.target) {
                        self.dialog = Some(Dialog::Edit(Editor::edit(&c.target, e)));
                    }
                    ui.close();
                }
                if ui.button("Remove…").clicked() {
                    self.dialog = Some(Dialog::Remove(c.target.clone()));
                    ui.close();
                }
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
                        ui.label("Address");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.addr)
                                    .hint_text("casa-pc.tailnet.ts.net or 100.x.y.z")
                                    .desired_width(250.0),
                            );
                            let devices: Vec<TailnetDevice> = snap
                                .tailnet
                                .as_ref()
                                .and_then(|t| t.as_ref().ok())
                                .map(|t| t.devices.iter().filter(|d| !d.is_self).cloned().collect())
                                .unwrap_or_default();
                            if !devices.is_empty() {
                                egui::ComboBox::from_id_salt("pick-device").selected_text("Tailnet…").show_ui(
                                    ui,
                                    |ui| {
                                        for d in devices {
                                            let addr = if d.dns_name.is_empty() {
                                                d.ip().map(|i| i.to_string()).unwrap_or_default()
                                            } else {
                                                d.dns_name.clone()
                                            };
                                            let label = format!(
                                                "{} ({}){}",
                                                d.name,
                                                d.os,
                                                if d.online { "" } else { " · offline" }
                                            );
                                            if ui.selectable_label(ed.addr == addr, label).clicked() {
                                                if ed.alias.is_empty() {
                                                    ed.alias = d.name.to_lowercase().replace(char::is_whitespace, "-");
                                                }
                                                ed.addr = addr;
                                            }
                                        }
                                    },
                                );
                            }
                        });
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
                                        ui.colored_label(Status::IdentityChanged.color(), e);
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
                        ui.colored_label(Status::IdentityChanged.color(), e);
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
                    ui.label(
                        "Its saved settings and trusted fingerprint are deleted. The computer itself is not touched.",
                    );
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
            Dialog::Trust { target, fp, old, about, files } => {
                let m = egui::Modal::new(egui::Id::unique("trust-host")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    match old {
                        None => {
                            ui.heading(format!("First connection to {target}"));
                            ui.label(format!("This computer says it is: {about}"));
                            ui.label("Check that its fingerprint matches `rdesk host fingerprint` on that machine:");
                        }
                        Some(old) => {
                            ui.heading(
                                RichText::new(format!("{target}: IDENTITY CHANGED"))
                                    .color(Status::IdentityChanged.color()),
                            );
                            ui.label(
                                "The computer answering at this address has a different certificate than before. \
                                 That is expected only if rdesk was reinstalled there (`rdesk host init --force`). \
                                 Otherwise someone may be impersonating it.",
                            );
                            ui.label(
                                RichText::new(format!("Trusted: {}", format_fingerprint(old))).monospace().small(),
                            );
                        }
                    }
                    ui.label(RichText::new(format_fingerprint(fp)).monospace().strong());
                    ui.add_space(6.0);
                    let mut done = None;
                    ui.horizontal(|ui| {
                        let label =
                            if old.is_some() { "Trust the new identity and connect" } else { "Trust and connect" };
                        if ui.button(label).clicked() {
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
                        let (t, f, files) = (target.clone(), *fp, *files);
                        keep = false;
                        self.trust_and_connect(&t, f, files);
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
        // A connect that was waiting for a probe result.
        if let Some((target, since, files)) = self.pending_connect.clone() {
            let addr = self.cfg.hosts.get(&target).map(|e| e.addr.clone()).unwrap_or_else(|| target.clone());
            let ready = self.shared.lock().unwrap().probes.get(&addr).is_some_and(|(_, at)| *at >= since);
            if ready {
                self.open(&target, files);
                self.pending_connect = None;
            } else if since.elapsed() > PROBE_TIMEOUT * 4 {
                self.pending_connect = None;
                self.notify(format!("{target} did not answer."), true);
            }
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let snap = self.shared.lock().unwrap().clone();
        let (saved, others) = self.cards(&snap);

        self.header(ui, &snap);
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Search").desired_width(220.0));
                if ui.button("＋ Add computer").clicked() {
                    self.dialog = Some(Dialog::Edit(Editor::new(String::new(), String::new())));
                }
            });
            ui.add_space(10.0);
            ui.label(RichText::new("My computers").strong().size(17.0));
            ui.add_space(4.0);
            self.section(ui, &saved, "No saved computers yet. Add one, or save a device from your tailnet below.");

            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Tailnet").strong().size(17.0));
                ui.checkbox(&mut self.show_offline, "show offline");
            });
            ui.add_space(4.0);
            let others: Vec<Card> = others
                .into_iter()
                .filter(|c| self.show_offline || c.device.as_ref().is_none_or(|d| d.online))
                .collect();
            let empty = match &snap.tailnet {
                None => "Asking Tailscale…",
                Some(Err(_)) => "Tailscale is not available.",
                Some(Ok(_)) => "No other devices.",
            };
            self.section(ui, &others, empty);
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
    let os = match (&c.probe, &c.device) {
        (Some(Probe::Ok(i)), _) => i.os.to_ascii_lowercase(),
        (_, Some(d)) => d.os.to_ascii_lowercase(),
        _ => String::new(),
    };
    match os.as_str() {
        "macos" => "💻",
        "ios" | "android" => "📱",
        _ => "🖥",
    }
}

fn self_summary(snap: &Snapshot) -> String {
    let Some(Ok(t)) = &snap.tailnet else { return String::new() };
    let Some(me) = t.me() else { return String::new() };
    let ip = me.ip().map(|i| i.to_string()).unwrap_or_default();
    let host = match snap.probes.get(&ip) {
        Some((Probe::Ok(i), _)) if i.in_session => "host: in use",
        Some((Probe::Ok(_), _)) => "host: running",
        Some((Probe::Err(_), _)) => "host: not running",
        None => "",
    };
    format!("This computer: {} · {ip}  {host}", me.name)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
    fn targets_skip_offline_and_dedupe() {
        let tailnet = directory::parse_status(
            br#"{"BackendState":"Running",
                "Self":{"HostName":"me","DNSName":"me.t.ts.net.","TailscaleIPs":["100.64.0.1"]},
                "Peer":{"a":{"HostName":"casa","DNSName":"casa.t.ts.net.","TailscaleIPs":["100.64.0.2"],"Online":true},
                        "b":{"HostName":"off","DNSName":"off.t.ts.net.","TailscaleIPs":["100.64.0.3"],"Online":false}}}"#,
        )
        .unwrap();
        let mut cfg = ClientConfig::default();
        cfg.hosts.insert("casa".into(), HostEntry { addr: "100.64.0.2".into(), ..Default::default() });
        cfg.hosts.insert("off".into(), HostEntry { addr: "off.t.ts.net".into(), ..Default::default() });
        cfg.hosts.insert("lab".into(), HostEntry { addr: "127.0.0.1:7421".into(), ..Default::default() });
        let t = probe_targets(&cfg, Some(&tailnet));
        assert_eq!(t, vec!["100.64.0.2", "127.0.0.1:7421", "100.64.0.1"]);
    }
}
