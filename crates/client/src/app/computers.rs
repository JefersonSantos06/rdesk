//! The "Computers" page: the computers this user can reach, as the server lists them, with
//! live status (online and in use come from the relay and the hosts' own sync), and the
//! computers saved on this device ("On this network"), in one list with a detail panel.
//!
//! * Whoever registered a computer (and its managers) renames it and chooses who may reach
//!   it ("Manage access", see `access.rs`); the owner can unregister it. Everyone who may
//!   reach it sees who else may.
//! * The computer the app runs on comes first, even when someone else registered it and
//!   shared nothing (see `presence.rs`): whoever is signed in here sees it and, once the
//!   system confirms an administrator, manages it. It cannot be opened from itself (a
//!   picture of the screen showing itself): only its terminal.
//! * Every session runs as its own `rdesk connect <host ID>` process (its own window); the
//!   ticket from the server and the host ID are checked by the connection itself.
//! * "Login options" keep, per computer, how to answer its optional extra lock (its own
//!   password or SSH key).

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use egui::{Align, Color32, Key, Layout, RichText, Sense, Vec2};
use rdesk_common::account::Session;
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, AuthPref, ClientConfig, HostEntry, parse_host_id};
use rdesk_common::i18n::{access_label, role_label, visibility_label};
use rdesk_common::{t, tf};
use rdesk_proto::api::{Here, HistoryEntry, Machine, MachineRequest, Shares, UpdateMachine, UserRef, Visibility};
use tokio::sync::mpsc;

use super::access::{self, AccessDialog};
use super::account::spawn;
use super::presence::{self, AdminGate, GateEvent};
use crate::ui::{self, Palette, Tone, icons, size, space};

const REFRESH_EVERY: Duration = Duration::from_secs(5);
/// How often the computers saved on this device are read again.
const LOCAL_EVERY: Duration = Duration::from_secs(3);
/// The list never gets narrower than this next to the detail panel; below it the details
/// open as a dialog.
const LIST_MIN: f32 = 340.0;
const DETAIL_MIN: f32 = 300.0;
const DETAIL_MAX: f32 = 440.0;
const PANEL_GAP: f32 = 16.0;
const ROW_H: f32 = 52.0;

// ---------------------------------------------------------------------------
// Background worker: the list from the server
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub(super) struct Snapshot {
    machines: Option<Result<Vec<Machine>, String>>,
    /// Computers asking this user (or, for admins, anyone) to approve them.
    requests: Vec<MachineRequest>,
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
                    shared.lock().unwrap().machines =
                        Some(Err(tf!("cannot start worker: {e}", "não foi possível iniciar o worker: {e}")));
                    return;
                }
            };
            rt.block_on(worker(shared, rx, ctx));
        })
        .expect("spawn directory worker");
}

async fn worker(shared: Arc<Mutex<Snapshot>>, mut rx: mpsc::UnboundedReceiver<WorkerCmd>, ctx: egui::Context) {
    let mut next = Instant::now();
    loop {
        let wait = tokio::time::sleep_until(next.into());
        tokio::select! {
            _ = wait => {}
            cmd = rx.recv() => match cmd {
                None => return,
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
        let (list, requests) = match Session::load() {
            // With a presence proof: this computer is listed whoever registered it.
            Some(s) => match presence::api(&s, Some(Here::View)).await {
                Ok(api) => {
                    let (list, requests) = tokio::join!(api.machines(), api.machine_requests());
                    (list.map_err(|e| short_error(&e)), requests.ok())
                }
                Err(e) => (Err(short_error(&e)), None),
            },
            None => (Err(t!("not signed in", "você não entrou em um servidor").into()), Some(Vec::new())),
        };
        {
            let mut snap = shared.lock().unwrap();
            if let Some(r) = requests {
                snap.requests = r;
            }
            // Keep the last good list through a short outage.
            if list.is_ok() || !matches!(snap.machines, Some(Ok(_))) {
                snap.machines = Some(list);
            }
            snap.refreshing = false;
        }
        ctx.request_repaint();
        next = Instant::now() + REFRESH_EVERY;
    }
}

async fn agent_keys(sock: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut a = Agent::connect(sock).await?;
    let keys = a.identities().await?;
    if keys.is_empty() {
        return Err(anyhow!(tf!(
            "the SSH agent at {} offers no keys",
            "o agente SSH em {} não oferece chaves",
            a.path
        )));
    }
    Ok(keys
        .iter()
        .map(|k| (agent::key_fingerprint(k.key_data()), format!("{} {}", k.algorithm(), k.comment())))
        .collect())
}

pub(super) fn load_client_config() -> ClientConfig {
    config::load_toml(config::CLIENT_CONFIG).unwrap_or_default()
}

/// First line of an error chain.
fn short_error(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or_default().to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Offline,
    Ready,
    Busy,
    /// A session window from this app is open.
    Open,
    /// Saved on this device: not known until connecting.
    Unknown,
}

impl Status {
    fn tone(self) -> Tone {
        match self {
            Status::Ready => Tone::Success,
            Status::Busy => Tone::Warning,
            Status::Open => Tone::Accent,
            Status::Offline | Status::Unknown => Tone::Neutral,
        }
    }

    fn color(self, p: &Palette) -> Color32 {
        self.tone().color(p)
    }

    /// Online first in the list.
    fn reachable(self) -> bool {
        matches!(self, Status::Ready | Status::Busy | Status::Open)
    }
}

struct Session_ {
    /// Host ID (what `rdesk connect` gets).
    target: String,
    label: String,
    child: std::process::Child,
    /// The window's standard input: `show`, `files` or `terminal` bring it forward.
    control: Option<std::process::ChildStdin>,
    started: Instant,
    log: PathBuf,
}

/// How to answer a computer's optional extra lock; kept in client.toml by host ID.
struct LoginOptions {
    endpoint_id: String,
    name: String,
    addr: String,
    auth: AuthPref,
    key: String,
    password_ref: String,
    identity: String,
    agent_socket: String,
    show_keys: bool,
    saved_password: bool,
    new_password: zeroize::Zeroizing<String>,
    forget_password: bool,
    /// A computer saved on this device, reached without the server: its name and ID are
    /// edited here too.
    local: Option<LocalFields>,
    error: Option<String>,
}

struct LocalFields {
    name: String,
    id: String,
    /// The ID it was saved under (editing), to replace that entry.
    original_id: Option<String>,
}

impl LoginOptions {
    fn empty() -> Self {
        Self {
            endpoint_id: String::new(),
            name: String::new(),
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
            local: None,
            error: None,
        }
    }

    /// A computer saved on this device (`None`: a new one).
    fn load_local(e: Option<&HostEntry>) -> Self {
        let mut o = Self::empty();
        if let Some(e) = e {
            o.endpoint_id = e.id.clone();
            o.name = e.name.clone();
            o.addr = e.addr.clone();
            o.auth = e.auth;
            o.key = e.key.clone().unwrap_or_default();
            o.password_ref = e.password_ref.clone().unwrap_or_default();
            o.identity = e.identity.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
            o.agent_socket = e.agent_socket.clone().unwrap_or_default();
            o.saved_password = crate::secrets::has(&e.name);
        }
        o.local =
            Some(LocalFields { name: o.name.clone(), id: o.endpoint_id.clone(), original_id: e.map(|e| e.id.clone()) });
        o
    }

    fn load(m: &Machine) -> Self {
        let e = load_client_config().hosts.get(&m.endpoint_id).cloned().unwrap_or_default();
        Self {
            endpoint_id: m.endpoint_id.clone(),
            name: m.name.clone(),
            addr: e.addr.clone(),
            auth: e.auth,
            key: e.key.clone().unwrap_or_default(),
            password_ref: e.password_ref.clone().unwrap_or_default(),
            identity: e.identity.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            agent_socket: e.agent_socket.clone().unwrap_or_default(),
            show_keys: false,
            saved_password: crate::secrets::has(&m.name),
            new_password: Default::default(),
            forget_password: false,
            local: None,
            error: None,
        }
    }
}

/// What a session window opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Screen,
    Files,
    Terminal,
}

/// Computers saved on this device to reach without the server, by name.
pub(super) fn local_computers() -> Vec<HostEntry> {
    let mut list: Vec<HostEntry> = load_client_config().hosts.into_values().filter(|e| !e.name.is_empty()).collect();
    list.sort_by_key(|e| e.name.to_lowercase());
    list
}

/// What `rdesk connect` is given for a computer saved on this device.
fn local_target(e: &HostEntry) -> String {
    if e.addr.trim().is_empty() { e.id.clone() } else { format!("{}@{}", e.id, e.addr.trim()) }
}

struct History {
    machine: Machine,
    data: Option<Result<Vec<HistoryEntry>, String>>,
    job: Option<Receiver<Result<Vec<HistoryEntry>, String>>>,
}

/// Shares, managers and (for who manages it) the history of the selected computer.
type DetailLoad = (Result<Shares, String>, Result<Vec<UserRef>, String>, Option<Result<Vec<HistoryEntry>, String>>);

/// What the detail panel shows about a computer this user may reach, manages, or is at
/// (loaded on selection).
struct Detail {
    id: i64,
    shares: Option<Result<Shares, String>>,
    managers: Option<Result<Vec<UserRef>, String>>,
    /// `None` also when this user does not manage it.
    history: Option<Result<Vec<HistoryEntry>, String>>,
    job: Option<Receiver<DetailLoad>>,
    /// Something changed: load again (keeping what is shown meanwhile).
    stale: bool,
}

enum Dialog {
    Login(Box<LoginOptions>),
    AddComputer(Box<super::add_computer::AddComputer>),
    /// The computers waiting for approval.
    Approvals,
    Approve {
        request: Box<MachineRequest>,
        name: String,
        visibility: Visibility,
    },
    Refuse(Box<MachineRequest>),
    Access(Box<AccessDialog>),
    History(Box<History>),
    Unregister(Machine),
    RemoveLocal(HostEntry),
    /// The selected computer's details, when the window is too narrow for the side panel.
    Details,
    /// Windows: managing this computer needs the app to run as administrator.
    RestartAsAdmin,
}

/// Managing a computer, once allowed (at the computer: after an administrator confirmed).
enum Manage {
    Rename(Machine),
    Access(Machine),
    History(Machine),
}

impl Manage {
    fn machine(&self) -> &Machine {
        match self {
            Manage::Rename(m) | Manage::Access(m) | Manage::History(m) => m,
        }
    }
}

fn when(t: i64) -> String {
    rdesk_common::directory::ago(t, now())
}

/// Which computer is selected.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Sel {
    Server(i64),
    /// By host ID.
    Local(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Filter {
    All,
    Mine,
    SharedWithMe,
    Local,
}

/// One line of the list.
#[derive(Clone)]
enum Item {
    Server(Machine),
    Local(HostEntry),
}

impl Item {
    fn sel(&self) -> Sel {
        match self {
            Item::Server(m) => Sel::Server(m.id),
            Item::Local(e) => Sel::Local(e.id.clone()),
        }
    }

    fn name(&self) -> &str {
        match self {
            Item::Server(m) => &m.name,
            Item::Local(e) => &e.name,
        }
    }

    fn can_connect(&self) -> bool {
        match self {
            Item::Server(m) => m.can_access,
            Item::Local(_) => true,
        }
    }

    fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let id_needle: String = needle.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let id_hit = |id: &str| !id_needle.is_empty() && id.to_lowercase().starts_with(&id_needle);
        match self {
            Item::Server(m) => {
                m.name.to_lowercase().contains(needle)
                    || m.owner.email.to_lowercase().contains(needle)
                    || id_hit(&m.endpoint_id)
            }
            Item::Local(e) => {
                e.name.to_lowercase().contains(needle) || e.addr.to_lowercase().contains(needle) || id_hit(&e.id)
            }
        }
    }
}

pub(super) struct Computers {
    exe: PathBuf,
    shared: Arc<Mutex<Snapshot>>,
    tx: mpsc::UnboundedSender<WorkerCmd>,
    /// Search, or what to connect to.
    search: String,
    search_id: Option<egui::Id>,
    focus_search: bool,
    filter: Filter,
    only_online: bool,
    selected: Option<Sel>,
    scroll_to_selected: bool,
    /// Renaming a server computer in the detail panel: its id and the new name.
    renaming: Option<(i64, String)>,
    rename_focus: bool,
    detail: Option<Detail>,
    /// Computers saved on this device, read now and then.
    locals: Vec<HostEntry>,
    locals_read: Option<Instant>,
    /// The gallery's sample: nothing is read from this device.
    frozen: bool,
    /// The details open as a dialog (the window is too narrow for the side panel).
    compact: bool,
    dialog: Option<Dialog>,
    sessions: Vec<Session_>,
    #[cfg(test)]
    fake_sessions: Vec<String>,
    job: Option<Receiver<Result<String, String>>>,
    /// Managing this computer as someone else's: waits for an administrator.
    gate: AdminGate<Manage>,
    pub(super) toast: Option<(String, bool, Instant)>,
    /// Another page to show (set by this page, taken by the window).
    pub(super) go_to: Option<super::Page>,
    /// The host ID of the computer the app runs on (set by the window), lowercase hex.
    pub(super) this_host: Option<String>,
    /// "name on server" when this computer's host says it is registered (set by the window).
    pub(super) this_registered: Option<String>,
    /// Asks the This computer page to register it, or to unregister it (taken by the window).
    pub(super) register_here: bool,
    pub(super) unregister_here: bool,
    /// The app restarts as administrator: quit (taken by the window).
    pub(super) quit_requested: bool,
}

impl Computers {
    pub(super) fn new(exe: PathBuf, shared: Arc<Mutex<Snapshot>>, tx: mpsc::UnboundedSender<WorkerCmd>) -> Self {
        Self {
            exe,
            shared,
            tx,
            search: String::new(),
            search_id: None,
            focus_search: false,
            filter: Filter::All,
            only_online: false,
            selected: None,
            scroll_to_selected: false,
            renaming: None,
            rename_focus: false,
            detail: None,
            locals: Vec::new(),
            locals_read: None,
            frozen: false,
            compact: false,
            dialog: None,
            sessions: Vec::new(),
            #[cfg(test)]
            fake_sessions: Vec::new(),
            job: None,
            gate: AdminGate::new(),
            toast: None,
            go_to: None,
            this_host: None,
            this_registered: None,
            register_here: false,
            unregister_here: false,
            quit_requested: false,
        }
    }

    /// The computer the app runs on.
    fn is_this(&self, endpoint_id: &str) -> bool {
        self.this_host.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(endpoint_id.trim()))
    }

    fn is_this_item(&self, it: &Item) -> bool {
        match it {
            Item::Server(m) => self.is_this(&m.endpoint_id),
            Item::Local(e) => self.is_this(&e.id),
        }
    }

    /// The computer the app runs on, as the server lists it.
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub(super) fn this_machine(&self) -> Option<Machine> {
        let snap = self.shared.lock().unwrap();
        let Some(Ok(list)) = &snap.machines else { return None };
        list.iter().find(|m| self.is_this(&m.endpoint_id)).cloned()
    }

    /// Opening a session to it: its screen and files make no sense from itself.
    fn connectable(&self, it: &Item) -> bool {
        it.can_connect() && !self.is_this_item(it)
    }

    /// Show "Manage access" from another page (This computer).
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub(super) fn manage_access_of(&mut self, ctx: &egui::Context, m: Machine) {
        self.selected = Some(Sel::Server(m.id));
        self.scroll_to_selected = true;
        self.manage(ctx, Manage::Access(m));
    }

    /// Do `what` now, or once an administrator confirmed when this user only manages it by
    /// being at the computer.
    fn manage(&mut self, ctx: &egui::Context, what: Manage) {
        let needed = presence::manage_scope(what.machine()).is_some();
        if let Some(what) = self.gate.request(ctx, needed, what) {
            self.do_manage(ctx, what);
        }
    }

    fn do_manage(&mut self, ctx: &egui::Context, what: Manage) {
        match what {
            Manage::Rename(m) => {
                self.renaming = Some((m.id, m.name.clone()));
                self.rename_focus = true;
                if self.compact {
                    self.dialog = Some(Dialog::Details);
                }
            }
            Manage::Access(m) => self.dialog = Some(Dialog::Access(Box::new(AccessDialog::new(ctx, &m)))),
            Manage::History(m) => self.start_history(ctx, &m),
        }
    }

    fn poll_gate(&mut self, ctx: &egui::Context) {
        match self.gate.poll() {
            GateEvent::Nothing => {}
            GateEvent::Go(what) => self.do_manage(ctx, what),
            GateEvent::NeedsRestart => self.dialog = Some(Dialog::RestartAsAdmin),
            GateEvent::Refused(e) => self.notify(e, true),
        }
    }

    /// Unregister: this computer through its This computer page (which also forgets the
    /// registration here), another one here.
    fn ask_unregister(&mut self, m: &Machine) {
        if self.is_this(&m.endpoint_id) {
            self.unregister_here = true;
            #[cfg(feature = "host")]
            {
                self.go_to = Some(super::Page::ThisComputer);
            }
        } else {
            self.dialog = Some(Dialog::Unregister(m.clone()));
        }
    }

    fn notify(&mut self, text: String, error: bool) {
        self.toast = Some((text, error, Instant::now()));
    }

    pub(super) fn refresh(&self) {
        let _ = self.tx.send(WorkerCmd::Refresh);
    }

    fn session_open(&self, target: &str) -> bool {
        #[cfg(test)]
        if self.fake_sessions.iter().any(|t| t == target) {
            return true;
        }
        self.sessions.iter().any(|s| s.target == target)
    }

    /// The status, its full text, and a short one for narrow rows.
    fn status(&self, m: &Machine) -> (Status, String, String) {
        if self.session_open(&m.endpoint_id) {
            let s: String = t!("Session open", "Sessão aberta").into();
            return (Status::Open, s.clone(), s);
        }
        if m.in_session {
            let s: String = t!("In use", "Em uso").into();
            return (Status::Busy, s.clone(), s);
        }
        if m.online {
            let s: String = t!("Online", "Online").into();
            return (Status::Ready, s.clone(), s);
        }
        let seen = match m.last_seen {
            Some(t) => tf!("Offline · seen {}", "Offline · visto {}", rdesk_common::directory::ago(t, now())),
            None => t!("Offline · never connected", "Offline · nunca conectado").into(),
        };
        (Status::Offline, seen, t!("Offline", "Offline").into())
    }

    fn item_status(&self, it: &Item) -> (Status, String, String) {
        match it {
            Item::Server(m) => self.status(m),
            Item::Local(e) => {
                if self.session_open(&local_target(e)) {
                    let s: String = t!("Session open", "Sessão aberta").into();
                    (Status::Open, s.clone(), s)
                } else {
                    (Status::Unknown, String::new(), String::new())
                }
            }
        }
    }

    /// Run a change on the server in the background, then refresh.
    fn run<F, Fut>(&mut self, ctx: &egui::Context, f: F)
    where
        F: FnOnce(rdesk_common::account::Api) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>>,
    {
        self.run_here(ctx, None, f);
    }

    /// [`Computers::run`], with a presence proof of `scope` (managing this computer as
    /// someone else's).
    fn run_here<F, Fut>(&mut self, ctx: &egui::Context, scope: Option<Here>, f: F)
    where
        F: FnOnce(rdesk_common::account::Api) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let Some(s) = Session::load() else {
            self.notify(t!("Not signed in.", "Você não entrou em um servidor.").into(), true);
            return;
        };
        self.job = Some(spawn(ctx, move || async move {
            match presence::api(&s, scope).await {
                Ok(api) => f(api).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            }
        }));
    }

    /// The computers saved on this device, read again every few seconds.
    fn read_locals(&mut self) {
        if self.frozen || self.locals_read.is_some_and(|t| t.elapsed() < LOCAL_EVERY) {
            return;
        }
        self.locals = local_computers();
        self.locals_read = Some(Instant::now());
    }

    // ---- actions ----------------------------------------------------------

    /// Quick connect: a computer's name or host ID, or ID@address (unregistered, LAN).
    fn quick_connect(&mut self, text: &str, list: &[Machine]) {
        let t = text.trim();
        let lower = t.to_lowercase();
        if let Some(m) = list.iter().find(|m| m.name.to_lowercase() == lower || m.endpoint_id.starts_with(&lower)) {
            let m = m.clone();
            self.open(&m, Open::Screen);
            return;
        }
        if let Some(e) = self.locals.iter().find(|e| e.name.to_lowercase() == lower).cloned() {
            self.spawn_session(&local_target(&e), &e.name, Open::Screen);
            return;
        }
        match t.split_once('@') {
            Some((id, _)) if parse_host_id(id).is_ok() => self.spawn_session(t, t, Open::Screen),
            _ => self.notify(
                tf!(
                    "{t}: no computer with that name or ID (ID@address for one on the LAN).",
                    "{t}: nenhum computador com esse nome ou ID (ID@endereço para um na rede local)."
                ),
                true,
            ),
        }
    }

    fn open(&mut self, m: &Machine, how: Open) {
        if !m.can_access {
            self.notify(tf!("{} is not shared with you.", "{} não está compartilhado com você.", m.name), true);
            return;
        }
        self.spawn_session(&m.endpoint_id, &m.name, how);
    }

    fn open_item(&mut self, it: &Item, how: Open) {
        match it {
            Item::Server(m) => self.open(m, how),
            Item::Local(e) => self.spawn_session(&local_target(e), &e.name, how),
        }
    }

    fn spawn_session(&mut self, target: &str, label: &str, how: Open) {
        // Its own screen (a picture of itself, endlessly) or files: only its terminal.
        let id = target.split('@').next().unwrap_or(target);
        let id = parse_host_id(id).map(|b| config::format_host_id(&b)).unwrap_or_else(|_| id.to_string());
        if how != Open::Terminal && self.is_this(&id) {
            self.notify(
                tf!(
                    "{label} is the computer you are on: only its terminal opens from here.",
                    "{label} é o computador em que você está: daqui só abre o terminal dele."
                ),
                true,
            );
            return;
        }
        if self.session_open(target) {
            self.raise(target, label, how);
            return;
        }
        let log = match session_log(label) {
            Ok(p) => p,
            Err(e) => {
                self.notify(
                    tf!("Cannot create the session log: {e:#}", "Não foi possível criar o log da sessão: {e:#}"),
                    true,
                );
                return;
            }
        };
        let stderr = match std::fs::File::create(&log) {
            Ok(f) => std::process::Stdio::from(f),
            Err(_) => std::process::Stdio::null(),
        };
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.args(["connect", target, "--controlled"]);
        match how {
            Open::Screen => {}
            Open::Files => {
                cmd.arg("--files");
            }
            Open::Terminal => {
                cmd.arg("--terminal");
            }
        }
        cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(stderr);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                self.sessions.push(Session_ {
                    target: target.to_string(),
                    label: label.to_string(),
                    control: child.stdin.take(),
                    child,
                    started: Instant::now(),
                    log,
                });
                self.notify(tf!("Opening {label}…", "Abrindo {label}…"), false);
            }
            Err(e) => {
                self.notify(tf!("Cannot start {}: {e}", "Não foi possível iniciar {}: {e}", self.exe.display()), true)
            }
        }
    }

    /// Bring an open session window forward (on its file panel or terminal if asked).
    fn raise(&mut self, target: &str, label: &str, how: Open) {
        use std::io::Write as _;
        let what = match how {
            Open::Screen => crate::conn::Raise::Window,
            Open::Files => crate::conn::Raise::Files,
            Open::Terminal => crate::conn::Raise::Terminal,
        };
        let Some(s) = self.sessions.iter_mut().find(|s| s.target == target) else { return };
        // This app has the focus (it was just clicked): let the window take it.
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(s.child.id());
        }
        let sent = s.control.as_mut().is_some_and(|w| writeln!(w, "{}", what.line()).and_then(|_| w.flush()).is_ok());
        if !sent {
            self.notify(tf!("A window for {label} is already open.", "Uma janela para {label} já está aberta."), false);
        }
    }

    /// Notice session windows that closed; report the ones that died right away.
    fn reap_sessions(&mut self) {
        let mut failed = Vec::new();
        self.sessions.retain_mut(|s| match s.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                if !status.success() && s.started.elapsed() < Duration::from_secs(30) {
                    failed.push((s.label.clone(), last_error_line(&s.log)));
                }
                false
            }
            Err(_) => false,
        });
        for (t, why) in failed {
            self.notify(tf!("Session to {t} failed: {why}", "A sessão com {t} falhou: {why}"), true);
        }
    }

    /// Check a computer saved on this device before saving it; fills in its name and ID.
    fn check_local(o: &mut LoginOptions) -> Result<(), String> {
        let Some(l) = &o.local else { return Ok(()) };
        let name = l.name.trim().to_string();
        if name.is_empty() {
            return Err(t!("Give it a name.", "Dê um nome a ele.").into());
        }
        let id = config::parse_host_id(l.id.trim()).map_err(|_| {
            t!(
                "The host ID is the 64 characters shown on that computer's This computer page.",
                "O ID do host são os 64 caracteres mostrados na página Este computador daquele computador."
            )
            .to_string()
        })?;
        let id = config::format_host_id(&id);
        let taken = local_computers()
            .into_iter()
            .any(|e| e.name.eq_ignore_ascii_case(&name) && Some(&e.id) != l.original_id.as_ref());
        if taken {
            return Err(tf!(
                "Another computer here is already called {name}.",
                "Outro computador aqui já se chama {name}."
            ));
        }
        o.name = name;
        o.endpoint_id = id;
        Ok(())
    }

    fn save_login(&mut self, o: &LoginOptions) {
        let mut cfg = load_client_config();
        let opt = |s: &str| if s.trim().is_empty() { None } else { Some(s.trim().to_string()) };
        let name = match &o.local {
            Some(_) => o.name.clone(),
            // Login options of a server computer keep the name it may also be saved under here.
            None => cfg.hosts.get(&o.endpoint_id).map(|e| e.name.clone()).unwrap_or_default(),
        };
        if let Some(old) = o.local.as_ref().and_then(|l| l.original_id.as_ref()).filter(|old| **old != o.endpoint_id) {
            cfg.hosts.remove(old);
        }
        let entry = HostEntry {
            id: o.endpoint_id.clone(),
            name,
            addr: o.addr.trim().to_string(),
            auth: o.auth,
            key: opt(&o.key),
            password_ref: opt(&o.password_ref),
            agent_socket: opt(&o.agent_socket),
            identity: opt(&o.identity).map(PathBuf::from),
        };
        if entry == (HostEntry { id: o.endpoint_id.clone(), ..Default::default() }) {
            cfg.hosts.remove(&o.endpoint_id);
        } else {
            cfg.hosts.insert(o.endpoint_id.clone(), entry);
        }
        if let Err(e) = config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &cfg)) {
            self.notify(tf!("Saving: {e:#}", "Ao salvar: {e:#}"), true);
            return;
        }
        self.locals_read = None;
        let secret = (|| {
            if o.forget_password {
                crate::secrets::forget(&o.name)?;
            }
            if !o.new_password.is_empty() {
                crate::secrets::set(&o.name, &o.new_password)?;
            }
            anyhow::Ok(())
        })();
        match secret {
            Ok(()) => {
                self.notify(tf!("Saved the login options of {}.", "Opções de login de {} salvas.", o.name), false)
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn remove_local(&mut self, e: &HostEntry) {
        let mut cfg = load_client_config();
        cfg.hosts.remove(&e.id);
        let _ = crate::secrets::forget(&e.name);
        match config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &cfg)) {
            Ok(()) => {
                self.notify(tf!("Removed {} from this device.", "{} foi removido deste dispositivo.", e.name), false)
            }
            Err(err) => self.notify(tf!("Saving: {err:#}", "Ao salvar: {err:#}"), true),
        }
        self.locals_read = None;
        if self.selected == Some(Sel::Local(e.id.clone())) {
            self.selected = None;
        }
    }

    fn start_history(&mut self, ctx: &egui::Context, m: &Machine) {
        let Some(s) = Session::load() else { return };
        let (id, scope) = (m.id, presence::manage_scope(m));
        let job = spawn(ctx, move || async move {
            match presence::api(&s, scope).await {
                Ok(api) => api.history(id).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            }
        });
        self.dialog = Some(Dialog::History(Box::new(History { machine: m.clone(), data: None, job: Some(job) })));
    }

    /// This user may change it: manages it, or is at the computer.
    fn may_manage(m: &Machine) -> bool {
        m.can_manage || m.here.is_some()
    }

    fn start_rename(&mut self, ctx: &egui::Context, m: &Machine) {
        if Self::may_manage(m) {
            self.manage(ctx, Manage::Rename(m.clone()));
        }
    }

    /// The machine as last listed.
    fn listed(&self, id: i64) -> Option<Machine> {
        let snap = self.shared.lock().unwrap();
        let Some(Ok(list)) = &snap.machines else { return None };
        list.iter().find(|m| m.id == id).cloned()
    }

    fn save_rename(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.renaming.take() else { return };
        let new = name.trim().to_string();
        if new.is_empty() {
            return;
        }
        let scope = self.listed(id).as_ref().and_then(presence::manage_scope);
        self.run_here(ctx, scope, move |api| async move {
            let m = api.update_machine(id, &UpdateMachine { name: Some(new), visibility: None }).await?;
            Ok(tf!("Renamed to {}.", "Renomeado para {}.", m.name))
        });
    }

    /// Load (or reload) what the detail panel shows about a computer: who it is shared
    /// with and who manages it, and its history for who manages it.
    fn load_detail(&mut self, ctx: &egui::Context, m: &Machine) {
        let keep = self.detail.take().filter(|d| d.id == m.id);
        let Some(s) = Session::load() else { return };
        let (id, manages, scope) = (m.id, m.can_manage, presence::view_scope(m));
        let job = spawn(ctx, move || async move {
            let api = match presence::api(&s, scope).await {
                Ok(a) => a,
                Err(e) => {
                    let e = format!("{e:#}");
                    return (Err(e.clone()), Err(e.clone()), manages.then_some(Err(e)));
                }
            };
            let (shares, managers, history) = tokio::join!(
                async { api.shares(id).await.map_err(|e| short_error(&e)) },
                async { api.managers(id).await.map(|m| m.users).map_err(|e| short_error(&e)) },
                async { if manages { Some(api.history(id).await.map_err(|e| short_error(&e))) } else { None } },
            );
            (shares, managers, history)
        });
        self.detail = Some(match keep {
            Some(mut d) => {
                d.job = Some(job);
                d.stale = false;
                d
            }
            None => Detail { id, shares: None, managers: None, history: None, job: Some(job), stale: false },
        });
    }

    fn poll_detail(&mut self) {
        if let Some(d) = &mut self.detail
            && let Some(rx) = &d.job
            && let Ok((shares, managers, history)) = rx.try_recv()
        {
            d.job = None;
            d.shares = Some(shares);
            d.managers = Some(managers);
            d.history = history;
        }
    }

    // ---- drawing -----------------------------------------------------------

    /// Title, Add and refresh.
    fn header(&mut self, ui: &mut egui::Ui, snap: &Snapshot, signed_in: bool) {
        let mut add = false;
        let mut refresh = false;
        ui::row(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui::page_title(
                        ui,
                        t!("Computers", "Computadores"),
                        Some(if signed_in {
                            t!(
                                "The computers you can reach, and the ones saved on this device.",
                                "Os computadores que você pode acessar e os salvos neste dispositivo."
                            )
                        } else {
                            t!(
                                "The computers saved on this device, reached on this network.",
                                "Os computadores salvos neste dispositivo, acessados nesta rede."
                            )
                        }),
                    );
                });
            },
            |ui| {
                if ui::secondary(ui, ui::with_icon(icons::PLUS, t!("Add", "Adicionar")))
                    .on_hover_text(t!("Add a computer", "Adicionar um computador"))
                    .clicked()
                {
                    add = true;
                }
                if signed_in {
                    if snap.refreshing || self.job.is_some() {
                        ui.add_sized(Vec2::splat(30.0), egui::Spinner::new());
                    } else if ui::icon_button(ui, icons::ARROWS_CLOCKWISE, t!("Refresh now", "Atualizar agora"))
                        .clicked()
                    {
                        refresh = true;
                    }
                }
            },
        );
        if add {
            self.add_computer(ui.ctx(), signed_in);
        }
        if refresh {
            self.refresh();
        }
    }

    fn add_computer(&mut self, ctx: &egui::Context, signed_in: bool) {
        self.dialog = Some(if signed_in {
            Dialog::AddComputer(Box::new(super::add_computer::AddComputer::new(ctx)))
        } else {
            Dialog::Login(Box::new(LoginOptions::load_local(None)))
        });
    }

    /// Every computer: this one first, then online first, then by name.
    fn items(&self, machines: &[Machine]) -> Vec<Item> {
        let mut v: Vec<Item> = machines.iter().cloned().map(Item::Server).collect();
        v.extend(self.locals.iter().cloned().map(Item::Local));
        v.sort_by_cached_key(|it| {
            (!self.is_this_item(it), !self.item_status(it).0.reachable(), it.name().to_lowercase())
        });
        v
    }

    fn in_filter(&self, it: &Item, f: Filter) -> bool {
        match (f, it) {
            (Filter::All, _) => true,
            (Filter::Mine, Item::Server(m)) => m.is_owner,
            (Filter::SharedWithMe, Item::Server(m)) => !m.is_owner,
            (Filter::Local, Item::Local(_)) => true,
            _ => false,
        }
    }

    fn visible(&self, items: &[Item], signed_in: bool) -> Vec<Item> {
        let needle = self.search.trim().to_lowercase();
        let filter = if signed_in { self.filter } else { Filter::All };
        items
            .iter()
            .filter(|it| self.in_filter(it, filter))
            .filter(|it| !self.only_online || self.item_status(it).0.reachable())
            .filter(|it| it.matches(&needle))
            .cloned()
            .collect()
    }

    /// Ctrl+K, arrows, Enter, F2 and Delete.
    fn keys(&mut self, ctx: &egui::Context, visible: &[Item]) {
        if self.dialog.is_some() || self.renaming.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::K)) {
            self.focus_search = true;
        }
        let focused = ctx.memory(|m| m.focused());
        let in_search = focused.is_some() && focused == self.search_id;
        let free = focused.is_none();
        if !(free || in_search) {
            return;
        }
        let (down, up, enter, f2, del) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::F2),
                i.key_pressed(Key::Delete),
            )
        });
        if (down || up) && !visible.is_empty() {
            let at = self.selected.as_ref().and_then(|s| visible.iter().position(|it| it.sel() == *s));
            let next = match (at, down) {
                (None, true) => 0,
                (None, false) => visible.len() - 1,
                (Some(i), true) => (i + 1).min(visible.len() - 1),
                (Some(i), false) => i.saturating_sub(1),
            };
            self.selected = Some(visible[next].sel());
            self.scroll_to_selected = true;
        }
        let current = self.selected.as_ref().and_then(|s| visible.iter().find(|it| it.sel() == *s)).cloned();
        let Some(it) = current else { return };
        if enter && free {
            self.open_item(&it, Open::Screen);
        }
        if f2 && let Item::Server(m) = &it {
            self.start_rename(ctx, m);
        }
        if del && free {
            match &it {
                Item::Server(m) if m.is_owner || m.here.is_some() => self.ask_unregister(m),
                Item::Local(e) => self.dialog = Some(Dialog::RemoveLocal(e.clone())),
                _ => {}
            }
        }
    }

    /// Enter in the search field: the selected or only match, else quick connect.
    fn search_enter(&mut self, visible: &[Item], machines: &[Machine]) {
        let text = self.search.trim().to_string();
        let selected = self.selected.as_ref().and_then(|s| visible.iter().find(|it| it.sel() == *s)).cloned();
        let target = selected.or_else(|| (visible.len() == 1).then(|| visible[0].clone()));
        match target {
            Some(it) => self.open_item(&it, Open::Screen),
            None if !text.is_empty() => self.quick_connect(&text, machines),
            None => {}
        }
    }

    /// The search field and the filter chips.
    fn search_and_filters(
        &mut self,
        ui: &mut egui::Ui,
        items: &[Item],
        visible: &[Item],
        machines: &[Machine],
        signed_in: bool,
    ) {
        let resp = ui::search(
            ui,
            &mut self.search,
            t!("Search or connect: name, ID or ID@address", "Buscar ou conectar: nome, ID ou ID@endereço"),
        );
        let resp = resp.on_hover_text(t!(
            "Type to filter by name, e-mail or host ID; Enter connects (Ctrl+K)",
            "Digite para filtrar por nome, e-mail ou ID do host; Enter conecta (Ctrl+K)"
        ));
        self.search_id = Some(resp.id);
        if std::mem::take(&mut self.focus_search) {
            resp.request_focus();
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            self.search_enter(visible, machines);
        }
        if !signed_in || items.is_empty() {
            return;
        }
        ui.add_space(space::XS);
        let counts: Vec<usize> = [Filter::All, Filter::Mine, Filter::SharedWithMe, Filter::Local]
            .into_iter()
            .map(|f| items.iter().filter(|it| self.in_filter(it, f)).count())
            .collect();
        let count = |f: Filter| counts[f as usize];
        let online = items.iter().filter(|it| self.item_status(it).0.reachable()).count();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(space::XS + 2.0, space::XS + 2.0);
            for (f, label) in [
                (Filter::All, t!("All", "Todos")),
                (Filter::Mine, t!("Mine", "Meus")),
                (Filter::SharedWithMe, t!("Shared with me", "Compartilhados comigo")),
                (Filter::Local, t!("On this network", "Nesta rede")),
            ] {
                if ui::chip(ui, self.filter == f, &format!("{label}  {}", count(f))).clicked() {
                    self.filter = f;
                }
            }
            ui.add_space(space::S);
            let label = format!("{}  {}", ui::with_icon(icons::CIRCLE, t!("Online only", "Só online")), online);
            if ui::chip(ui, self.only_online, &label).clicked() {
                self.only_online = !self.only_online;
            }
        });
    }

    /// One computer in the list.
    fn row_ui(&mut self, ui: &mut egui::Ui, it: &Item, compact: bool) {
        let p = ui::pal(ui);
        let sel = it.sel();
        let selected = self.selected.as_ref() == Some(&sel);
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click());
        let hovered = ui.rect_contains_pointer(rect);
        let fill = if selected {
            ui::tint(p.accent, if p.dark { 46 } else { 28 })
        } else if hovered {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, egui::CornerRadius::same(ui::RADIUS), fill);
        if selected {
            let bar = egui::Rect::from_min_size(rect.left_top() + egui::vec2(0.0, 10.0), Vec2::new(3.0, ROW_H - 20.0));
            ui.painter().rect_filled(bar, egui::CornerRadius::same(2), p.accent);
        }
        let (status, long, short) = self.item_status(it);
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(egui::vec2(space::M, 0.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        let this = self.is_this_item(it);
        let show_button = (hovered || selected) && self.connectable(it);
        let mut connect = false;
        let mut details = false;
        let label = if status == Status::Open { t!("Focus", "Mostrar") } else { t!("Connect", "Conectar") };
        let subtitle = self.subtitle(it);
        ui::row(
            &mut child,
            |ui| {
                status_dot(ui, status);
                ui.label(RichText::new(item_icon(it)).size(size::ICON + 2.0).color(p.muted));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui::truncate(ui, RichText::new(it.name()).strong());
                    ui::truncate(ui, RichText::new(&subtitle).size(size::SMALL).color(p.muted));
                });
            },
            |ui| {
                if compact && ui::icon_button(ui, icons::CARET_RIGHT, t!("Details", "Detalhes")).clicked() {
                    details = true;
                }
                if show_button && ui::secondary(ui, label).clicked() {
                    connect = true;
                }
                let text = if w < 520.0 { &short } else { &long };
                if !(text.is_empty() || show_button && w < 420.0) {
                    let b = ui::badge(ui, text, status.tone());
                    if text != &long {
                        b.on_hover_text(&long);
                    }
                }
                if this {
                    ui::badge(ui, t!("This computer", "Este computador"), Tone::Accent)
                        .on_hover_text(t!("The computer you are on", "O computador em que você está"));
                }
            },
        );
        if self.scroll_to_selected && selected {
            resp.scroll_to_me(None);
            self.scroll_to_selected = false;
        }
        if resp.clicked() || resp.secondary_clicked() {
            self.selected = Some(sel.clone());
        }
        if details {
            self.selected = Some(sel.clone());
            self.dialog = Some(Dialog::Details);
        }
        if (resp.double_clicked() && !this) || connect {
            self.open_item(it, Open::Screen);
        }
        let ctx = ui.ctx().clone();
        resp.context_menu(|ui| self.menu_items(ui, &ctx, it, true));
    }

    /// Under the name: whose it is and what this user may do (or where it is).
    fn subtitle(&self, it: &Item) -> String {
        match it {
            Item::Server(m) => {
                let owner = if m.is_owner { t!("Yours", "Seu").to_string() } else { m.owner.email.clone() };
                let what = if m.is_owner || m.can_manage {
                    visibility_label(m.visibility).to_string()
                } else if m.can_access {
                    access_label(m.access)
                } else {
                    t!("no access", "sem acesso").to_string()
                };
                format!("{owner} · {what}")
            }
            Item::Local(e) => format!("{} · {}", t!("On this network", "Nesta rede"), local_where(e)),
        }
    }

    /// The actions of a computer, as a menu; `connect` adds Connect/Files/Terminal (right-click).
    fn menu_items(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, it: &Item, connect: bool) {
        ui.set_min_width(200.0);
        if connect {
            let (can, files, terminal) = match it {
                Item::Server(m) => (m.can_access, m.can_access && m.access.files, m.can_access && m.access.terminal),
                Item::Local(_) => (true, true, true),
            };
            // The computer you are on: only its terminal.
            let this = self.is_this_item(it);
            if !this
                && ui
                    .add_enabled(can, egui::Button::new(ui::with_icon(icons::MONITOR, t!("Connect", "Conectar"))))
                    .clicked()
            {
                self.open_item(it, Open::Screen);
                ui.close();
            }
            if !this
                && ui
                    .add_enabled(files, egui::Button::new(ui::with_icon(icons::FOLDER, t!("Files", "Arquivos"))))
                    .clicked()
            {
                self.open_item(it, Open::Files);
                ui.close();
            }
            if ui
                .add_enabled(
                    terminal,
                    egui::Button::new(ui::with_icon(icons::TERMINAL_WINDOW, t!("Terminal", "Terminal"))),
                )
                .clicked()
            {
                self.open_item(it, Open::Terminal);
                ui.close();
            }
            ui.separator();
        }
        match it {
            Item::Server(m) => {
                if ui.button(ui::with_icon(icons::COPY, t!("Copy host ID", "Copiar ID do host"))).clicked() {
                    ui.ctx().copy_text(m.endpoint_id.clone());
                    self.notify(t!("Host ID copied.", "ID do host copiado.").into(), false);
                    ui.close();
                }
                if ui
                    .button(ui::with_icon(icons::KEY, t!("Login options…", "Opções de login…")))
                    .on_hover_text(t!(
                        "For computers that also ask for their own password or key",
                        "Para computadores que também pedem a própria senha ou chave"
                    ))
                    .clicked()
                {
                    self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load(m))));
                    ui.close();
                }
                if Self::may_manage(m) {
                    ui.separator();
                    if ui.button(ui::with_icon(icons::PENCIL_SIMPLE, t!("Rename  (F2)", "Renomear  (F2)"))).clicked() {
                        self.selected = Some(Sel::Server(m.id));
                        self.start_rename(ctx, m);
                        ui.close();
                    }
                    if ui.button(ui::with_icon(icons::USERS, t!("Manage access…", "Gerenciar acesso…"))).clicked() {
                        self.manage(ctx, Manage::Access(m.clone()));
                        ui.close();
                    }
                    if ui.button(ui::with_icon(icons::CLOCK_COUNTER_CLOCKWISE, t!("History…", "Histórico…"))).clicked()
                    {
                        self.manage(ctx, Manage::History(m.clone()));
                        ui.close();
                    }
                }
                if m.is_owner || m.here.is_some() {
                    ui.separator();
                    let p = ui::pal(ui);
                    let text =
                        RichText::new(ui::with_icon(icons::TRASH, t!("Unregister…", "Descadastrar…"))).color(p.danger);
                    if ui.button(text).clicked() {
                        self.ask_unregister(m);
                        ui.close();
                    }
                }
            }
            Item::Local(e) => {
                if ui.button(ui::with_icon(icons::COPY, t!("Copy host ID", "Copiar ID do host"))).clicked() {
                    ui.ctx().copy_text(e.id.clone());
                    self.notify(t!("Host ID copied.", "ID do host copiado.").into(), false);
                    ui.close();
                }
                if ui.button(ui::with_icon(icons::PENCIL_SIMPLE, t!("Edit…", "Editar…"))).clicked() {
                    self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load_local(Some(e)))));
                    ui.close();
                }
                ui.separator();
                let p = ui::pal(ui);
                let text = RichText::new(ui::with_icon(icons::TRASH, t!("Remove…", "Remover…"))).color(p.danger);
                if ui.button(text).clicked() {
                    self.dialog = Some(Dialog::RemoveLocal(e.clone()));
                    ui.close();
                }
            }
        }
    }

    /// The selected computer's details (side panel or dialog).
    fn detail_ui(&mut self, ui: &mut egui::Ui, it: &Item) {
        match it {
            Item::Server(m) => self.server_detail(ui, m),
            Item::Local(e) => self.local_detail(ui, e),
        }
    }

    fn server_detail(&mut self, ui: &mut egui::Ui, m: &Machine) {
        let ctx = ui.ctx().clone();
        let p = ui::pal(ui);
        let (status, long, _) = self.status(m);
        let it = Item::Server(m.clone());

        // Name (rename in place).
        let renaming = self.renaming.as_ref().is_some_and(|(id, _)| *id == m.id);
        if renaming {
            let mut save_b = false;
            let mut cancel_b = false;
            let (save_k, cancel_k) = ui::row(
                ui,
                |ui| {
                    let (mut save, mut cancel) = (false, false);
                    if let Some((_, name)) = &mut self.renaming {
                        let r = ui.add(
                            egui::TextEdit::singleline(name)
                                .desired_width(ui.available_width())
                                .font(egui::FontId::proportional(size::SECTION)),
                        );
                        if std::mem::take(&mut self.rename_focus) {
                            r.request_focus();
                        }
                        if r.lost_focus() {
                            if ui.input(|i| i.key_pressed(Key::Enter)) {
                                save = true;
                            } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                                cancel = true;
                            }
                        }
                    }
                    (save, cancel)
                },
                |ui| {
                    if ui::icon_button(ui, icons::X, t!("Cancel", "Cancelar")).clicked() {
                        cancel_b = true;
                    }
                    if ui::icon_button(ui, icons::CHECK, t!("Save the name", "Salvar o nome")).clicked() {
                        save_b = true;
                    }
                },
            );
            let (save, cancel) = (save_k || save_b, cancel_k || cancel_b);
            if save {
                self.save_rename(&ctx);
            } else if cancel {
                self.renaming = None;
            }
        } else {
            let mut rename = false;
            ui::row(
                ui,
                |ui| {
                    ui.label(RichText::new(os_icon(&m.os)).size(22.0).color(p.muted));
                    ui.add(egui::Label::new(RichText::new(&m.name).size(size::SECTION + 1.0).strong()).wrap());
                },
                |ui| {
                    // More actions next to the name, so the action buttons below keep one line.
                    more_menu(ui, |ui| self.menu_items(ui, &ctx, &it, false));
                    if Self::may_manage(m)
                        && ui::icon_button(ui, icons::PENCIL_SIMPLE, t!("Rename  (F2)", "Renomear  (F2)")).clicked()
                    {
                        rename = true;
                    }
                },
            );
            if rename {
                self.start_rename(&ctx, m);
            }
        }
        ui.horizontal_wrapped(|ui| {
            status_dot(ui, status);
            ui.label(RichText::new(&long).color(status.color(&p)));
        });
        let mut facts = vec![os_name(&m.os)];
        if !m.version.is_empty() {
            facts.push(format!("rdesk {}", m.version));
        }
        facts.retain(|s| !s.is_empty());
        if !facts.is_empty() {
            ui::caption(ui, &facts.join(" · "));
        }
        ui.add_space(space::M);

        // Actions (the computer you are on: only its terminal).
        let this = self.is_this(&m.endpoint_id);
        ui.horizontal_wrapped(|ui| {
            let label = if status == Status::Open { t!("Focus", "Mostrar") } else { t!("Connect", "Conectar") };
            if !this {
                let r =
                    ui.add_enabled_ui(m.can_access, |ui| ui::primary(ui, ui::with_icon(icons::MONITOR, label))).inner;
                if r.clicked() {
                    self.open(m, Open::Screen);
                }
            }
            if !this
                && m.can_access
                && m.access.files
                && ui::secondary(ui, ui::with_icon(icons::FOLDER, t!("Files", "Arquivos")))
                    .on_hover_text(t!(
                        "Open a session on the file-transfer panel",
                        "Abrir uma sessão no painel de transferência de arquivos"
                    ))
                    .clicked()
            {
                self.open(m, Open::Files);
            }
            if m.can_access
                && m.access.terminal
                && ui::secondary(ui, ui::with_icon(icons::TERMINAL_WINDOW, t!("Terminal", "Terminal")))
                    .on_hover_text(t!(
                        "A shell on that computer, as whoever is signed in there",
                        "Um shell naquele computador, como quem estiver logado lá"
                    ))
                    .clicked()
            {
                self.open(m, Open::Terminal);
            }
            // Off: another computer on its network can wake it.
            if !this
                && !m.online
                && m.can_access
                && m.can_wake
                && ui::secondary(ui, ui::with_icon(icons::POWER, t!("Turn on", "Ligar")))
                    .on_hover_text(t!(
                        "Wake-on-LAN: a computer that is on, on the same network, sends the signal (the network                          card must allow it, in the BIOS and in Windows)",
                        "Wake-on-LAN: um computador ligado, na mesma rede, manda o sinal (a placa de rede precisa                          permitir, na BIOS e no Windows)"
                    ))
                    .clicked()
            {
                let (id, name) = (m.id, m.name.clone());
                self.run_here(&ctx, None, move |api| async move {
                    let r = api.wake(id).await?;
                    Ok(tf!(
                        "Asked {} computer(s) on its network to wake {name}; it shows as online in a minute or two.",
                        "{} computador(es) da rede dele foram pedidos para ligar {name}; ele aparece online em um                          ou dois minutos.",
                        r.helpers
                    ))
                });
            }
        });
        if this {
            ui::caption(
                ui,
                t!(
                    "This is the computer you are on: its screen and files open from other computers.",
                    "Este é o computador em que você está: a tela e os arquivos dele abrem de outros computadores."
                ),
            );
        } else if !m.can_access {
            ui::caption(ui, t!("It is not shared with you.", "Ele não está compartilhado com você."));
        }
        if m.here.is_some() && !m.can_manage {
            ui.add_space(space::S);
            ui::banner(
                ui,
                Tone::Accent,
                &tf!(
                    "Registered by {}. You are at this computer, so you may manage it too: the system asks for \
                     an administrator first.",
                    "Cadastrado por {}. Você está neste computador, então também pode gerenciá-lo: o sistema pede \
                     um administrador antes.",
                    m.owner.email
                ),
                None,
            );
        }
        if m.direct {
            ui.add_space(space::S);
            ui::banner(
                ui,
                Tone::Warning,
                t!(
                    "Direct access is on: its own password or key also opens it on its network, without the \
                     server's permission (turned on at that computer, in Options on its This computer page).",
                    "Acesso direto ligado: a senha ou chave dele também o abre na rede dele, sem a permissão \
                     do servidor (ligado naquele computador, em Opções, na página Este computador)."
                ),
                None,
            );
        }
        ui.add_space(space::M);

        // Host ID.
        ui::section(ui, t!("Host ID", "ID do host"));
        id_line(ui, &m.endpoint_id, &mut self.toast);
        ui.add_space(space::M);

        // Who has access (everyone who may reach it, manages it, or is at it sees it).
        ui::section(ui, t!("Who has access", "Quem tem acesso"));
        self.who_ui(ui, m);
        if !m.can_manage {
            let mine = if m.can_access { access_label(m.access) } else { t!("no access", "sem acesso").into() };
            ui::caption(ui, &tf!("You: {mine}", "Você: {mine}"));
        }
        if Self::may_manage(m)
            && ui::link(ui, ui::with_icon(icons::USERS, t!("Manage access", "Gerenciar acesso"))).clicked()
        {
            self.manage(&ctx, Manage::Access(m.clone()));
        }

        // Recent sessions.
        if m.can_manage {
            ui.add_space(space::M);
            ui::section(ui, t!("Recent sessions", "Sessões recentes"));
            match self.detail.as_ref().filter(|d| d.id == m.id).and_then(|d| d.history.as_ref()) {
                None => ui::busy(ui, t!("Loading…", "Carregando…")),
                Some(Err(e)) => {
                    ui::caption(ui, &tf!("Cannot load the history: {e}", "Não foi possível carregar o histórico: {e}"));
                }
                Some(Ok(list)) if list.is_empty() => {
                    ui::caption(ui, t!("No sessions yet.", "Nenhuma sessão ainda."));
                }
                Some(Ok(list)) => {
                    for e in list.iter().take(5) {
                        ui::row(
                            ui,
                            |ui| {
                                ui::truncate(ui, e.who.as_str()).on_hover_text(format!("{} · {}", e.client, e.method));
                            },
                            |ui| {
                                ui::caption(ui, &format!("{} · {}", when(e.started_at), duration(e)));
                            },
                        );
                    }
                }
            }
            if ui::link(ui, t!("Full history", "Histórico completo")).clicked() {
                self.manage(&ctx, Manage::History(m.clone()));
            }
        }

        // Advanced.
        ui.add_space(space::M);
        egui::CollapsingHeader::new(RichText::new(t!("Advanced", "Avançado")).strong())
            .id_salt(("computers-advanced", m.id))
            .show(ui, |ui| {
                ui::caption(
                    ui,
                    t!(
                        "Login options: for computers whose owner turned on the extra lock (their own password \
                         or SSH key).",
                        "Opções de login: para computadores cujo dono ligou o cadeado extra (a senha ou chave SSH \
                         do próprio computador)."
                    ),
                );
                if ui::secondary(ui, ui::with_icon(icons::KEY, t!("Login options…", "Opções de login…"))).clicked()
                {
                    self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load(m))));
                }
                if m.is_owner || m.here.is_some() {
                    ui.add_space(space::S);
                    ui::caption(
                        ui,
                        t!(
                            "Unregister: it leaves the server and nobody reaches it through the server any more.",
                            "Descadastrar: ele sai do servidor e ninguém mais o acessa pelo servidor."
                        ),
                    );
                    if ui::danger(ui, t!("Unregister from the server…", "Descadastrar do servidor…")).clicked() {
                        self.ask_unregister(m);
                    }
                }
            });
    }

    /// "Who has access": the owner, the managers and who it is shared with.
    fn who_ui(&mut self, ui: &mut egui::Ui, m: &Machine) {
        let p = ui::pal(ui);
        let d = self.detail.as_ref().filter(|d| d.id == m.id);
        let managers: Vec<UserRef> =
            d.and_then(|d| d.managers.as_ref()).and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();
        let line = |ui: &mut egui::Ui, icon_or_avatar: Result<&str, &str>, name: &str, role: &str| {
            ui::row(
                ui,
                |ui| {
                    match icon_or_avatar {
                        Ok(icon) => {
                            let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                            ui::paint_centered(ui, rect, icon, ui::icon_font(size::ICON), p.muted);
                        }
                        Err(email) => {
                            ui::avatar(ui, email, 24.0);
                        }
                    }
                    ui::truncate(ui, name);
                },
                |ui| {
                    ui::badge(ui, role, Tone::Neutral);
                },
            );
        };
        let owner_text = if m.is_owner { t!("You (owner)", "Você (dono)").to_string() } else { m.owner.email.clone() };
        line(ui, Err(&m.owner.email), &owner_text, t!("owner", "dono"));
        for u in &managers {
            line(ui, Err(&u.email), access::person_label(u), t!("manager", "gestor"));
        }
        match m.visibility {
            Visibility::Private if m.is_owner => {
                ui::caption(ui, t!("Nobody else: only me.", "Ninguém mais: só eu."));
            }
            Visibility::Private => {
                ui::caption(ui, t!("Nobody else: only its owner.", "Ninguém mais: só o dono."));
            }
            Visibility::Everyone => {
                line(
                    ui,
                    Ok(icons::GLOBE),
                    t!("Everyone on the server", "Todos do servidor"),
                    role_label(Default::default()),
                );
            }
            Visibility::Shared => match d.and_then(|d| d.shares.as_ref()) {
                None => ui::busy(ui, t!("Loading…", "Carregando…")),
                Some(Err(e)) => {
                    ui::caption(ui, &tf!("Cannot load the list: {e}", "Não foi possível carregar a lista: {e}"));
                }
                Some(Ok(s)) if s.users.is_empty() && s.groups.is_empty() => {
                    ui::caption(ui, t!("Shared with nobody yet.", "Ainda não compartilhado com ninguém."));
                }
                Some(Ok(s)) => {
                    let total = s.groups.len() + s.users.len();
                    for (g, r) in s.groups.iter().take(6) {
                        line(ui, Ok(icons::USERS_THREE), &g.name, role_label(*r));
                    }
                    for (u, r) in s.users.iter().take(6usize.saturating_sub(s.groups.len())) {
                        if managers.iter().any(|x| x.id == u.id) {
                            continue;
                        }
                        line(ui, Err(&u.email), access::person_label(u), role_label(*r));
                    }
                    if total > 6 {
                        ui::caption(ui, &tf!("and {} more", "e mais {}", total - 6));
                    }
                }
            },
        }
    }

    fn local_detail(&mut self, ui: &mut egui::Ui, e: &HostEntry) {
        let ctx = ui.ctx().clone();
        let p = ui::pal(ui);
        let it = Item::Local(e.clone());
        ui::row(
            ui,
            |ui| {
                ui.label(RichText::new(icons::WIFI_HIGH).size(22.0).color(p.muted));
                ui.add(egui::Label::new(RichText::new(&e.name).size(size::SECTION + 1.0).strong()).wrap());
            },
            |_| {},
        );
        let open = self.session_open(&local_target(e));
        ui::caption(
            ui,
            &if open {
                format!("{} · {}", t!("On this network", "Nesta rede"), t!("Session open", "Sessão aberta"))
            } else {
                t!("On this network", "Nesta rede").to_string()
            },
        );
        ui.add_space(space::M);
        let this = self.is_this(&e.id);
        ui.horizontal_wrapped(|ui| {
            let label = if open { t!("Focus", "Mostrar") } else { t!("Connect", "Conectar") };
            if !this && ui::primary(ui, ui::with_icon(icons::MONITOR, label)).clicked() {
                self.open_item(&it, Open::Screen);
            }
            if !this && ui::secondary(ui, ui::with_icon(icons::FOLDER, t!("Files", "Arquivos"))).clicked() {
                self.open_item(&it, Open::Files);
            }
            if ui::secondary(ui, ui::with_icon(icons::TERMINAL_WINDOW, t!("Terminal", "Terminal"))).clicked() {
                self.open_item(&it, Open::Terminal);
            }
            more_menu(ui, |ui| self.menu_items(ui, &ctx, &it, false));
        });
        ui.add_space(space::M);
        ui::section(ui, t!("Host ID", "ID do host"));
        id_line(ui, &e.id, &mut self.toast);
        ui.add_space(space::M);
        ui::section(ui, t!("Address", "Endereço"));
        ui.add(egui::Label::new(local_where(e)).wrap());
        ui.add_space(space::S);
        ui::caption(
            ui,
            t!(
                "Saved on this device; reached directly with the computer's own password or SSH key.",
                "Salvo neste dispositivo; acessado diretamente com a senha ou chave SSH do próprio computador."
            ),
        );
        ui.add_space(space::M);
        ui.horizontal_wrapped(|ui| {
            if ui::secondary(ui, ui::with_icon(icons::PENCIL_SIMPLE, t!("Edit…", "Editar…"))).clicked() {
                self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load_local(Some(e)))));
            }
            if ui::danger(ui, ui::with_icon(icons::TRASH, t!("Remove…", "Remover…"))).clicked() {
                self.dialog = Some(Dialog::RemoveLocal(e.clone()));
            }
        });
    }

    /// This computer, when the server does not list it: not registered (register it), or
    /// registered with another server.
    fn this_card(&mut self, ui: &mut egui::Ui, machines: &[Machine]) {
        if self.this_host.is_none()
            || machines.iter().any(|m| self.is_this(&m.endpoint_id))
            || self.filter != Filter::All
            || !self.search.trim().is_empty()
        {
            return;
        }
        let p = ui::pal(ui);
        let (detail, button) = match &self.this_registered {
            Some(name) => (
                tf!(
                    "Registered as {name}, but not on the server you are signed in to.",
                    "Cadastrado como {name}, mas não no servidor em que você entrou."
                ),
                t!("Open", "Abrir"),
            ),
            None => (
                t!(
                    "Not registered on the server: register it to reach it from anywhere and choose who may.",
                    "Não cadastrado no servidor: cadastre-o para acessá-lo de qualquer lugar e escolher quem pode."
                )
                .to_string(),
                t!("Register…", "Cadastrar…"),
            ),
        };
        let mut go = false;
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, p.border))
            .corner_radius(egui::CornerRadius::same(ui::RADIUS))
            .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8))
            .show(ui, |ui| {
                ui::row(
                    ui,
                    |ui| {
                        ui.label(RichText::new(icons::HOUSE).size(size::ICON + 2.0).color(p.muted));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(RichText::new(t!("This computer", "Este computador")).strong());
                            ui.add(egui::Label::new(RichText::new(&detail).size(size::SMALL).color(p.muted)).wrap());
                        });
                    },
                    |ui| {
                        if ui::secondary(ui, button).clicked() {
                            go = true;
                        }
                    },
                );
            });
        ui.add_space(space::XS);
        if go {
            self.register_here = self.this_registered.is_none();
            #[cfg(feature = "host")]
            {
                self.go_to = Some(super::Page::ThisComputer);
            }
        }
    }

    /// The list and its empty states.
    fn list_ui(&mut self, ui: &mut egui::Ui, items: &[Item], visible: &[Item], compact: bool, signed_in: bool) {
        if visible.is_empty() {
            self.empty_ui(ui, items, signed_in);
            return;
        }
        egui::ScrollArea::vertical().id_salt("computers-list").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for it in visible {
                self.row_ui(ui, it, compact);
            }
        });
    }

    fn empty_ui(&mut self, ui: &mut egui::Ui, items: &[Item], signed_in: bool) {
        let q = self.search.trim().to_string();
        let mut add = None;
        if items.is_empty() || (!signed_in && self.locals.is_empty()) {
            ui::empty_state(
                ui,
                icons::DESKTOP,
                if signed_in {
                    t!("No computers yet", "Nenhum computador ainda")
                } else {
                    t!("No computers on this network yet", "Nenhum computador nesta rede ainda")
                },
                if signed_in {
                    t!(
                        "Add this computer or another one to reach it from anywhere.",
                        "Adicione este computador ou outro para acessá-lo de qualquer lugar."
                    )
                } else {
                    t!(
                        "Add a computer on the local network or Tailscale, reached with its own password or SSH key.",
                        "Adicione um computador da rede local ou do Tailscale, acessado com a senha ou chave SSH dele."
                    )
                },
            );
            add = Some(signed_in);
        } else if !q.is_empty() {
            ui::empty_state(
                ui,
                icons::MAGNIFYING_GLASS,
                t!("Nothing found", "Nada encontrado"),
                &tf!(
                    "No computer matches “{q}”. Press Enter to connect by host ID or ID@address.",
                    "Nenhum computador corresponde a “{q}”. Pressione Enter para conectar pelo ID do host ou ID@endereço."
                ),
            );
        } else if self.only_online {
            ui::empty_state(
                ui,
                icons::WIFI_SLASH,
                t!("No computer online", "Nenhum computador online"),
                t!("Turn off “Online only” to see them all.", "Desligue “Só online” para ver todos."),
            );
        } else {
            match self.filter {
                Filter::Mine => ui::empty_state(
                    ui,
                    icons::DESKTOP,
                    t!("None of yours yet", "Nenhum computador seu ainda"),
                    t!(
                        "Register a computer on its This computer page, or approve one that asked.",
                        "Cadastre um computador na página Este computador dele, ou aprove um que pediu."
                    ),
                ),
                Filter::SharedWithMe => ui::empty_state(
                    ui,
                    icons::USERS,
                    t!("Nothing shared with you yet", "Nada compartilhado com você ainda"),
                    t!(
                        "When someone shares a computer with you, it shows up here.",
                        "Quando alguém compartilhar um computador com você, ele aparece aqui."
                    ),
                ),
                _ => {
                    ui::empty_state(
                        ui,
                        icons::WIFI_HIGH,
                        t!("No computers on this network yet", "Nenhum computador nesta rede ainda"),
                        t!(
                            "Add a computer on the local network or Tailscale, reached with its own password or SSH key.",
                            "Adicione um computador da rede local ou do Tailscale, acessado com a senha ou chave SSH dele."
                        ),
                    );
                    add = Some(false);
                }
            }
        }
        if let Some(server) = add {
            ui.vertical_centered(|ui| {
                if ui::primary(ui, ui::with_icon(icons::PLUS, t!("Add a computer", "Adicionar um computador")))
                    .clicked()
                {
                    self.add_computer(ui.ctx(), server && signed_in);
                }
            });
        }
    }

    // ---- dialogs -----------------------------------------------------------

    fn dialogs(&mut self, ctx: &egui::Context, snap: &Snapshot, items: &[Item], compact: bool) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        match &mut dialog {
            Dialog::Login(o) => match self.login_dialog(ctx, o, snap) {
                Some(true) => match Self::check_local(o) {
                    Ok(()) => {
                        let o = std::mem::replace(o, Box::new(LoginOptions::empty()));
                        self.save_login(&o);
                        keep = false;
                    }
                    Err(e) => o.error = Some(e),
                },
                Some(false) => keep = false,
                None => {}
            },
            Dialog::Details => {
                let current = self.selected.as_ref().and_then(|s| items.iter().find(|it| it.sel() == *s)).cloned();
                match current {
                    Some(it) if compact => {
                        let title = t!("Details", "Detalhes");
                        let r = ui::dialog(ctx, "computers-details", title, 480.0, |ui| self.detail_ui(ui, &it));
                        if r.closed {
                            keep = false;
                            self.renaming = None;
                        }
                    }
                    _ => keep = false,
                }
            }
            Dialog::Access(d) => match access::show(ctx, d) {
                access::Outcome::Open => {}
                access::Outcome::Closed => keep = false,
                access::Outcome::Save(plan) => {
                    let id = d.machine_id();
                    self.run_here(ctx, presence::manage_scope(d.machine()), move |api| plan.apply(api));
                    if let Some(det) = &mut self.detail
                        && det.id == id
                    {
                        det.stale = true;
                    }
                    keep = false;
                }
            },
            Dialog::History(h) => {
                if let Some(rx) = &h.job
                    && let Ok(r) = rx.try_recv()
                {
                    h.job = None;
                    h.data = Some(r);
                }
                let title = tf!("History · {}", "Histórico · {}", h.machine.name);
                let r = ui::dialog_with_footer(
                    ctx,
                    "computers-history",
                    &title,
                    620.0,
                    |ui| history_body(ui, h),
                    close_button,
                );
                if r.closed || r.inner.1 {
                    keep = false;
                }
            }
            Dialog::AddComputer(d) => match super::add_computer::show(ctx, d) {
                super::add_computer::Outcome::Open => {}
                super::add_computer::Outcome::Closed => keep = false,
                super::add_computer::Outcome::GoToThisComputer => {
                    keep = false;
                    #[cfg(feature = "host")]
                    {
                        self.go_to = Some(super::Page::ThisComputer);
                    }
                }
                super::add_computer::Outcome::AddLocal => {
                    keep = false;
                    self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load_local(None))));
                }
            },
            Dialog::Approvals => {
                if snap.requests.is_empty() {
                    keep = false;
                } else {
                    let mut next = None;
                    let title = t!("Waiting for your approval", "Aguardando sua aprovação");
                    let r = ui::dialog_with_footer(
                        ctx,
                        "computers-approvals",
                        title,
                        560.0,
                        |ui| {
                            ui::paragraph(
                                ui,
                                t!(
                                    "Approve only computers you expect: compare the code with the one the computer shows.",
                                    "Aprove só computadores que você espera: compare o código com o que o computador mostra."
                                ),
                            );
                            ui.add_space(space::S);
                            for r in &snap.requests {
                                if let Some(d) = request_card(ui, r) {
                                    next = Some(d);
                                }
                                ui.add_space(space::S);
                            }
                        },
                        close_button,
                    );
                    if let Some(d) = next {
                        self.dialog = Some(d);
                    } else if r.closed || r.inner.1 {
                        keep = false;
                    }
                }
            }
            Dialog::Approve { request, name, visibility } => {
                let title = tf!("Approve {}?", "Aprovar {}?", request.name);
                let r = ui::dialog_with_footer(
                    ctx,
                    "computers-approve",
                    &title,
                    480.0,
                    |ui| {
                        ui::paragraph(
                            ui,
                            &tf!(
                                "Only if that computer shows the code {}. Once approved it is yours: you choose who may \
                             reach it, and can share, rename or unregister it.",
                                "Só se aquele computador mostrar o código {}. Depois de aprovado, ele é seu: você escolhe \
                             quem pode acessá-lo e pode compartilhá-lo, renomeá-lo ou descadastrá-lo.",
                                request.code
                            ),
                        );
                        ui.add_space(space::S);
                        ui.label(RichText::new(&request.code).monospace().size(26.0).strong());
                        ui.add_space(space::S);
                        ui.label(RichText::new(t!("Name", "Nome")).strong());
                        let resp = ui.add(egui::TextEdit::singleline(name).desired_width(ui.available_width()));
                        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        ui.add_space(space::S);
                        ui.label(RichText::new(t!("Who can reach it", "Quem pode acessar")).strong());
                        for (v, sub) in [
                            (Visibility::Private, t!("Only you, for now.", "Só você, por enquanto.")),
                            (
                                Visibility::Shared,
                                t!(
                                    "Choose them afterwards in Manage access.",
                                    "Escolha quem depois em Gerenciar acesso."
                                ),
                            ),
                            (
                                Visibility::Everyone,
                                t!(
                                    "Everyone with an account on this server, with full control.",
                                    "Todas as pessoas com conta neste servidor, com controle total."
                                ),
                            ),
                        ] {
                            ui.radio_value(visibility, v, access::mode_label(v));
                            ui.indent(("approve-sub", v as u8), |ui| ui::caption(ui, sub));
                        }
                        enter
                    },
                    |ui| ok_cancel(ui, t!("Approve", "Aprovar")),
                );
                let back = || if snap.requests.len() > 1 { Some(Dialog::Approvals) } else { None };
                let (enter, button) = r.inner;
                match button.or(enter.then_some(true)) {
                    Some(true) => {
                        let (id, n, v) = (request.id, name.trim().to_string(), *visibility);
                        self.run(ctx, move |api| async move {
                            let m = api.approve_machine(id, Some(n), v).await?;
                            Ok(tf!(
                                "{} was added: it shows up online in a few seconds.",
                                "{} foi adicionado: ele aparece online em alguns segundos.",
                                m.name
                            ))
                        });
                        keep = false;
                        self.dialog = back();
                    }
                    Some(false) => {
                        keep = false;
                        self.dialog = back();
                    }
                    None if r.closed => keep = false,
                    None => {}
                }
            }
            Dialog::Refuse(request) => {
                let title = tf!("Refuse {}?", "Recusar {}?", request.name);
                let body = tf!(
                    "{} is not added to the server. Do this when you do not recognize it.",
                    "{} não é adicionado ao servidor. Faça isso quando não o reconhecer.",
                    request.name
                );
                match ui::confirm(ctx, "computers-refuse", &title, &body, t!("Refuse", "Recusar"), true) {
                    Some(true) => {
                        let (id, name) = (request.id, request.name.clone());
                        self.run(ctx, move |api| async move {
                            api.reject_machine(id).await?;
                            Ok(tf!("{name} was refused.", "{name} foi recusado."))
                        });
                        keep = false;
                        if snap.requests.len() > 1 {
                            self.dialog = Some(Dialog::Approvals);
                        }
                    }
                    Some(false) => {
                        keep = false;
                        self.dialog = Some(Dialog::Approvals);
                    }
                    None => {}
                }
            }
            Dialog::Unregister(machine) => {
                let title = tf!("Unregister {}?", "Descadastrar {}?", machine.name);
                let body = t!(
                    "It leaves the server: nobody can reach it through the server any more, and its \
                     sharing is lost. Register it again from that computer's This computer page.",
                    "Ele sai do servidor: ninguém mais consegue acessá-lo pelo servidor, e o compartilhamento \
                     dele se perde. Cadastre-o de novo na página Este computador daquele computador.",
                );
                match ui::confirm(ctx, "computers-unregister", &title, body, t!("Unregister", "Descadastrar"), true) {
                    Some(true) => {
                        let (id, name) = (machine.id, machine.name.clone());
                        self.run_here(ctx, presence::remove_scope(machine), move |api| async move {
                            api.remove_machine(id).await?;
                            Ok(tf!("{name} was unregistered.", "{name} foi descadastrado."))
                        });
                        if self.selected == Some(Sel::Server(id)) {
                            self.selected = None;
                        }
                        keep = false;
                    }
                    Some(false) => keep = false,
                    None => {}
                }
            }
            Dialog::RestartAsAdmin => {
                let (title, body) = presence::restart_question();
                match ui::confirm(
                    ctx,
                    "computers-admin",
                    title,
                    body,
                    t!("Open as administrator", "Abrir como administrador"),
                    false,
                ) {
                    Some(true) => {
                        keep = false;
                        match super::autostart::restart_as_admin(&self.exe) {
                            Ok(()) => self.quit_requested = true,
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
                    }
                    Some(false) => keep = false,
                    None => {}
                }
            }
            Dialog::RemoveLocal(e) => {
                let title = tf!("Remove {}?", "Remover {}?", e.name);
                let body = t!(
                    "It leaves the On this network list and its saved password is forgotten. The computer \
                     itself does not change.",
                    "Ele sai da lista Nesta rede e a senha salva dele é esquecida. O computador em si não muda.",
                );
                match ui::confirm(ctx, "computers-remove-local", &title, body, t!("Remove", "Remover"), true) {
                    Some(true) => {
                        let e = e.clone();
                        self.remove_local(&e);
                        keep = false;
                    }
                    Some(false) => keep = false,
                    None => {}
                }
            }
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    /// Login options (or a computer saved on this device). `Some(true)`: save.
    fn login_dialog(&mut self, ctx: &egui::Context, o: &mut LoginOptions, snap: &Snapshot) -> Option<bool> {
        let title = match &o.local {
            Some(l) if l.original_id.is_some() => t!("Computer on this network", "Computador nesta rede").to_string(),
            Some(_) => t!("Add a computer on this network", "Adicionar um computador nesta rede").to_string(),
            None => tf!("Login options · {}", "Opções de login · {}", o.name),
        };
        let r = ui::dialog_with_footer(
            ctx,
            "computers-login",
            &title,
            520.0,
            |ui| {
                let full = |ui: &egui::Ui| ui.available_width();
                if let Some(l) = &mut o.local {
                    ui::paragraph(
                        ui,
                        t!(
                            "Reached without the server: on the local network or Tailscale, with that computer's \
                         own password or SSH key (its owner allows this on its This computer page).",
                            "Acessado sem o servidor: na rede local ou no Tailscale, com a senha ou chave SSH do \
                         próprio computador (o dono dele permite isso na página Este computador).",
                        ),
                    );
                    ui.add_space(space::S);
                    field_label(ui, t!("Name", "Nome"));
                    ui.add(
                        egui::TextEdit::singleline(&mut l.name)
                            .hint_text(t!("e.g. PC da sala", "ex.: PC da sala"))
                            .desired_width(full(ui)),
                    );
                    field_label(ui, t!("Host ID", "ID do host"));
                    ui.add(
                        egui::TextEdit::singleline(&mut l.id)
                            .hint_text(t!(
                                "64 characters, from its This computer page",
                                "64 caracteres, da página Este computador dele"
                            ))
                            .desired_width(full(ui)),
                    );
                    field_label(ui, t!("Address", "Endereço"));
                    ui.add(
                        egui::TextEdit::singleline(&mut o.addr)
                            .hint_text(t!(
                                "192.168.0.10, pc.tailnet.ts.net… (empty: find it by mDNS)",
                                "192.168.0.10, pc.tailnet.ts.net… (vazio: encontrar por mDNS)"
                            ))
                            .desired_width(full(ui)),
                    );
                } else {
                    ui::paragraph(
                        ui,
                        t!(
                            "Only for computers whose owner turned on the extra lock (their own password or \
                         SSH key, asked after the server's permission).",
                            "Só para computadores cujo dono ligou o cadeado extra (a senha ou chave SSH do \
                         próprio computador, pedida depois da permissão do servidor).",
                        ),
                    );
                }
                ui.add_space(space::S);
                field_label(ui, t!("Login", "Login"));
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(&mut o.auth, AuthPref::Auto, t!("Key, then password", "Chave, depois senha"));
                    ui.radio_value(&mut o.auth, AuthPref::Agent, t!("1Password key", "Chave do 1Password"));
                    ui.radio_value(&mut o.auth, AuthPref::Password, t!("Password", "Senha"));
                });
                if o.auth != AuthPref::Password {
                    field_label(ui, t!("SSH key", "Chave SSH"));
                    ui.add(
                        egui::TextEdit::singleline(&mut o.key)
                            .hint_text(t!("any key the computer accepts", "qualquer chave que o computador aceite"))
                            .desired_width(full(ui)),
                    );
                    if ui::link(ui, t!("Choose from 1Password…", "Escolher do 1Password…")).clicked() {
                        let sock = (!o.agent_socket.trim().is_empty()).then(|| o.agent_socket.trim().to_string());
                        self.shared.lock().unwrap().agent_keys = None;
                        let _ = self.tx.send(WorkerCmd::AgentKeys(sock));
                        o.show_keys = true;
                    }
                    if o.show_keys {
                        match &snap.agent_keys {
                            None => ui::busy(ui, t!("Asking the SSH agent…", "Perguntando ao agente SSH…")),
                            Some(Err(e)) => {
                                ui::banner(ui, Tone::Danger, e, None);
                            }
                            Some(Ok(keys)) => {
                                for (fp, label) in keys {
                                    if pick_line(ui, label, fp, o.key == *fp).clicked() {
                                        o.key = fp.clone();
                                        o.show_keys = false;
                                    }
                                }
                            }
                        }
                    }
                }
                if o.auth != AuthPref::Agent {
                    field_label(ui, t!("1Password reference", "Referência do 1Password"));
                    ui.add(
                        egui::TextEdit::singleline(&mut o.password_ref)
                            .hint_text(t!(
                                "op://Private/rdesk/password (optional)",
                                "op://Private/rdesk/password (opcional)"
                            ))
                            .desired_width(full(ui)),
                    );
                    if crate::secrets::available() {
                        field_label(ui, t!("Saved password", "Senha salva"));
                        if o.saved_password && !o.forget_password {
                            ui.horizontal_wrapped(|ui| {
                                let p = ui::pal(ui);
                                ui.label(RichText::new(icons::CHECK_CIRCLE).color(p.success));
                                ui.label(tf!("In {}", "Em {}", crate::secrets::store_name()));
                                if ui::ghost(ui, t!("Forget", "Esquecer")).clicked() {
                                    o.forget_password = true;
                                }
                            });
                        } else {
                            ui.add(
                                egui::TextEdit::singleline(&mut *o.new_password)
                                    .password(true)
                                    .hint_text(t!("type to save it", "digite para salvar"))
                                    .desired_width(full(ui)),
                            );
                        }
                    }
                }
                ui.add_space(space::S);
                egui::CollapsingHeader::new(RichText::new(t!("Advanced", "Avançado")).strong())
                    .id_salt("login-advanced")
                    .show(ui, |ui| {
                        if o.local.is_none() {
                            field_label(ui, t!("Address", "Endereço"));
                            ui.add(
                                egui::TextEdit::singleline(&mut o.addr)
                                    .hint_text(t!(
                                        "extra direct address (optional)",
                                        "endereço direto extra (opcional)"
                                    ))
                                    .desired_width(full(ui)),
                            );
                        }
                        field_label(ui, t!("Agent socket", "Socket do agente"));
                        ui.add(
                            egui::TextEdit::singleline(&mut o.agent_socket)
                                .hint_text(t!("default: 1Password", "padrão: 1Password"))
                                .desired_width(full(ui)),
                        );
                        field_label(ui, t!("Key file", "Arquivo de chave"));
                        ui.add(
                            egui::TextEdit::singleline(&mut o.identity)
                                .hint_text(t!("unencrypted OpenSSH key", "chave OpenSSH sem criptografia"))
                                .desired_width(full(ui)),
                        );
                    });
                if let Some(e) = &o.error {
                    ui.add_space(space::S);
                    ui::banner(ui, Tone::Danger, e, None);
                }
            },
            |ui| ok_cancel(ui, t!("Save", "Salvar")),
        );
        match r.inner.1 {
            Some(x) => Some(x),
            None if r.closed => Some(false),
            None => None,
        }
    }
}

impl Computers {
    /// Computers waiting for this user's approval.
    pub(super) fn pending_requests(&self) -> usize {
        self.shared.lock().unwrap().requests.len()
    }

    pub(super) fn open_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Bookkeeping that must run even while the window is hidden.
    pub(super) fn tick(&mut self) {
        self.reap_sessions();
        if let Some(rx) = &self.job
            && let Ok(r) = rx.try_recv()
        {
            self.job = None;
            match r {
                Ok(msg) => self.notify(msg, false),
                Err(e) => self.notify(e, true),
            }
            if let Some(d) = &mut self.detail {
                d.stale = true;
            }
            self.refresh();
        }
    }

    /// `signed_in`: the server's computers are shown too (not in the local-network mode).
    pub(super) fn show(&mut self, ui: &mut egui::Ui, signed_in: bool) {
        let ctx = ui.ctx().clone();
        let snap = self.shared.lock().unwrap().clone();
        self.read_locals();
        self.poll_detail();
        let machines: Vec<Machine> = match (&snap.machines, signed_in) {
            (Some(Ok(l)), true) => l.clone(),
            _ => Vec::new(),
        };
        let items = self.items(&machines);
        // The selected computer may have gone (unregistered, removed).
        if let Some(s) = &self.selected
            && !items.iter().any(|it| it.sel() == *s)
            && (snap.machines.is_some() || matches!(s, Sel::Local(_)))
        {
            self.selected = None;
        }
        self.poll_gate(&ctx);
        let selected = self.selected.as_ref().and_then(|s| items.iter().find(|it| it.sel() == *s)).cloned();
        if let Some(Item::Server(m)) = &selected {
            let need = (m.can_manage || m.can_access || m.here.is_some())
                && match &self.detail {
                    Some(d) => d.id != m.id || (d.stale && d.job.is_none()),
                    None => true,
                };
            if need {
                self.load_detail(&ctx, m);
            }
        }
        if self.renaming.as_ref().is_some_and(|(id, _)| !matches!(&selected, Some(Item::Server(m)) if m.id == *id)) {
            self.renaming = None;
        }

        self.header(ui, &snap, signed_in);
        ui.add_space(space::M);
        if !signed_in {
            ui::banner(
                ui,
                Tone::Accent,
                t!(
                    "Not signed in to a server: only computers on this network (local network or Tailscale).",
                    "Sem servidor: só computadores nesta rede (rede local ou Tailscale)."
                ),
                None,
            );
            ui.add_space(space::S);
        } else {
            if let Some(Err(e)) = &snap.machines
                && ui::banner(
                    ui,
                    Tone::Danger,
                    &tf!("Cannot get the list from the server: {e}", "Não foi possível obter a lista do servidor: {e}"),
                    Some(t!("Try again", "Tentar de novo")),
                )
            {
                self.refresh();
            }
            if !snap.requests.is_empty() {
                let n = snap.requests.len();
                let text = if n == 1 {
                    t!("1 computer is waiting for your approval.", "1 computador aguarda sua aprovação.").to_string()
                } else {
                    tf!("{n} computers are waiting for your approval.", "{n} computadores aguardam sua aprovação.")
                };
                if ui::banner(ui, Tone::Warning, &text, Some(t!("Review", "Revisar"))) {
                    self.dialog = Some(Dialog::Approvals);
                }
                ui.add_space(space::S);
            } else if matches!(&snap.machines, Some(Err(_))) {
                ui.add_space(space::S);
            }
        }

        let loading = signed_in && snap.machines.is_none();
        let wide = ui.available_width() >= LIST_MIN + DETAIL_MIN + PANEL_GAP;
        let compact = !wide;
        self.compact = compact;
        let visible = self.visible(&items, signed_in);
        self.keys(&ctx, &visible);

        if wide && !items.is_empty() {
            let max = (ui.available_width() - LIST_MIN - PANEL_GAP).clamp(DETAIL_MIN, DETAIL_MAX);
            let p = ui::pal(ui);
            egui::Panel::right("computers-detail")
                .resizable(true)
                .show_separator_line(false)
                .size_range(DETAIL_MIN + PANEL_GAP..=max + PANEL_GAP)
                .default_size(360.0 + PANEL_GAP)
                .frame(egui::Frame::new().inner_margin(egui::Margin { left: PANEL_GAP as i8, ..Default::default() }))
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(p.surface)
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .corner_radius(egui::CornerRadius::same(ui::RADIUS_CARD))
                        .inner_margin(egui::Margin::same(space::L as i8))
                        .show(ui, |ui| {
                            ui.set_min_size(ui.available_size());
                            egui::ScrollArea::vertical().id_salt("computers-detail-scroll").auto_shrink([false, false]).show(
                                ui,
                                |ui| match &selected {
                                    Some(it) => self.detail_ui(ui, it),
                                    None => ui::empty_state(
                                        ui,
                                        icons::CURSOR_CLICK,
                                        t!("No computer selected", "Nenhum computador selecionado"),
                                        t!(
                                            "Select a computer to see its details. Double-click or Enter connects.",
                                            "Selecione um computador para ver os detalhes. Clique duas vezes ou Enter conecta."
                                        ),
                                    ),
                                },
                            );
                        });
                });
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            self.search_and_filters(ui, &items, &visible, &machines, signed_in);
            ui.add_space(space::S);
            if loading {
                ui::busy(ui, t!("Loading the computers…", "Carregando os computadores…"));
                if self.locals.is_empty() {
                    return;
                }
            }
            if loading && visible.is_empty() {
                return;
            }
            if signed_in && matches!(&snap.machines, Some(Ok(_))) {
                self.this_card(ui, &machines);
            }
            self.list_ui(ui, &items, &visible, compact, signed_in);
        });
        self.dialogs(&ctx, &snap, &items, compact);
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A round status dot; a ring when unknown (saved on this device).
fn status_dot(ui: &mut egui::Ui, s: Status) {
    let p = ui::pal(ui);
    if s == Status::Unknown {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_stroke(rect.center(), 4.0, egui::Stroke::new(1.5, p.muted));
    } else {
        ui::dot(ui, s.color(&p));
    }
}

/// A "⋯" button with a menu.
fn more_menu(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let r = ui.menu_button(RichText::new(icons::DOTS_THREE).size(size::ICON + 1.0), add);
    r.response.on_hover_text(t!("More actions", "Mais ações"));
}

/// The short host ID with a copy button.
fn id_line(ui: &mut egui::Ui, id: &str, toast: &mut Option<(String, bool, Instant)>) {
    ui::row(
        ui,
        |ui| {
            ui::mono(ui, &ui::short_id(id)).on_hover_text(id);
        },
        |ui| {
            if ui::icon_button(ui, icons::COPY, t!("Copy host ID", "Copiar ID do host")).clicked() {
                ui.ctx().copy_text(id.to_string());
                *toast = Some((t!("Host ID copied.", "ID do host copiado.").into(), false, Instant::now()));
            }
        },
    );
}

/// A form field's label.
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(space::XS);
    ui.add(egui::Label::new(RichText::new(text).strong()).wrap());
}

/// A choosable line with a title and a muted second line (both cut with "…").
fn pick_line(ui: &mut egui::Ui, title: &str, sub: &str, selected: bool) -> egui::Response {
    let p = ui::pal(ui);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 44.0), Sense::click());
    let fill = if selected {
        ui::tint(p.accent, if p.dark { 46 } else { 28 })
    } else if resp.hovered() {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, egui::CornerRadius::same(ui::RADIUS), fill);
    let mut child = ui.new_child(
        egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(space::S, 2.0))).layout(Layout::top_down(Align::Min)),
    );
    child.spacing_mut().item_spacing.y = 2.0;
    ui::truncate(&mut child, title);
    ui::truncate(&mut child, RichText::new(sub).monospace().size(size::SMALL).color(p.muted));
    resp
}

/// One request in the approvals dialog; returns the dialog to open.
fn request_card(ui: &mut egui::Ui, r: &MachineRequest) -> Option<Dialog> {
    let mut out = None;
    ui::card(ui, |ui| {
        let p = ui::pal(ui);
        ui::row(
            ui,
            |ui| {
                ui.label(RichText::new(os_icon(&r.os)).size(22.0).color(p.muted));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui::truncate(ui, RichText::new(&r.name).strong());
                    let whom = match &r.owner {
                        Some(o) => tf!("for {}", "para {}", o.email),
                        None => t!("no owner (for admins)", "sem dono (para admins)").into(),
                    };
                    ui::truncate(
                        ui,
                        RichText::new(format!("{whom} · {}", tf!("asked {}", "pediu {}", when(r.created_at))))
                            .size(size::SMALL)
                            .color(p.muted),
                    );
                });
            },
            |_| {},
        );
        ui.add_space(space::S);
        ui.horizontal_wrapped(|ui| {
            ui::caption(ui, t!("Code", "Código"));
            ui.label(RichText::new(&r.code).monospace().size(24.0).strong());
        });
        ui.add_space(space::XS);
        ui.horizontal_wrapped(|ui| {
            if ui::secondary(ui, ui::with_icon(icons::CHECK, t!("Approve…", "Aprovar…"))).clicked() {
                out = Some(Dialog::Approve {
                    request: Box::new(r.clone()),
                    name: r.name.clone(),
                    visibility: Visibility::Private,
                });
            }
            if ui::secondary(ui, ui::with_icon(icons::X, t!("Refuse…", "Recusar…"))).clicked() {
                out = Some(Dialog::Refuse(Box::new(r.clone())));
            }
        });
    });
    out
}

/// The history dialog's body.
fn history_body(ui: &mut egui::Ui, h: &History) {
    let p = ui::pal(ui);
    match &h.data {
        None => ui::busy(ui, t!("Loading…", "Carregando…")),
        Some(Err(e)) => {
            ui::banner(ui, Tone::Danger, e, None);
        }
        Some(Ok(list)) if list.is_empty() => {
            ui::caption(ui, t!("No sessions yet.", "Nenhuma sessão ainda."));
        }
        Some(Ok(list)) => {
            for e in list {
                ui::row(
                    ui,
                    |ui| {
                        ui::avatar(ui, &e.who, 28.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui::truncate(ui, RichText::new(&e.who).strong());
                            ui::truncate(
                                ui,
                                RichText::new(format!("{} · {} · {}", e.client, e.method, e.access))
                                    .size(size::SMALL)
                                    .color(p.muted),
                            );
                        });
                    },
                    |ui| {
                        ui.with_layout(Layout::top_down(Align::Max), |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(when(e.started_at));
                            ui.label(RichText::new(duration(e)).size(size::SMALL).color(p.muted));
                        });
                    },
                );
                ui.separator();
            }
        }
    }
}

/// A dialog's footer with just Close; `true` when clicked.
fn close_button(ui: &mut egui::Ui) -> bool {
    ui.add_space(space::S);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui::secondary(ui, t!("Close", "Fechar")).clicked()).inner
}

/// A dialog's footer with `ok` and Cancel: `Some(true)` ok, `Some(false)` cancel.
fn ok_cancel(ui: &mut egui::Ui, ok: &str) -> Option<bool> {
    match ui::dialog_buttons(ui, ok, false, &[t!("Cancel", "Cancelar")]) {
        Some(ui::DialogButton::Primary) => Some(true),
        Some(ui::DialogButton::Other(_)) => Some(false),
        None => None,
    }
}

fn duration(e: &HistoryEntry) -> String {
    match e.ended_at {
        Some(t) => format!("{} min", ((t - e.started_at).max(0) + 59) / 60),
        None => t!("still connected", "ainda conectado").into(),
    }
}

/// Where a computer saved on this device is reached.
fn local_where(e: &HostEntry) -> String {
    if e.addr.trim().is_empty() {
        t!("found by mDNS", "encontrado por mDNS").to_string()
    } else {
        e.addr.trim().to_string()
    }
}

fn item_icon(it: &Item) -> &'static str {
    match it {
        Item::Server(m) => os_icon(&m.os),
        Item::Local(_) => icons::DESKTOP,
    }
}

fn os_icon(os: &str) -> &'static str {
    match os.to_ascii_lowercase().as_str() {
        "windows" => icons::WINDOWS_LOGO,
        "macos" | "ios" => icons::APPLE_LOGO,
        "linux" => icons::LINUX_LOGO,
        "android" => icons::ANDROID_LOGO,
        _ => icons::DESKTOP,
    }
}

fn os_name(os: &str) -> String {
    match os.to_ascii_lowercase().as_str() {
        "windows" => "Windows".into(),
        "macos" => "macOS".into(),
        "linux" => "Linux".into(),
        "android" => "Android".into(),
        "ios" => "iOS".into(),
        _ => os.to_string(),
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
        .unwrap_or_else(|| tf!("see {}", "veja {}", log.display()))
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

    #[test]
    fn search_matches_names_emails_and_id_prefixes() {
        let m = sample::machine(1, "PC da Sala", "windows", "ana@exemplo.com", false, true, false);
        let it = Item::Server(m.clone());
        assert!(it.matches("sala"));
        assert!(it.matches("ana@"));
        assert!(it.matches(&m.endpoint_id[..6]));
        assert!(!it.matches("notebook"));
    }
}

// ---------------------------------------------------------------------------
// Gallery
// ---------------------------------------------------------------------------

#[cfg(test)]
mod sample {
    use super::*;
    use rdesk_proto::api::{GroupRef, ShareRole};

    pub(super) fn user(id: i64, email: &str, name: &str) -> UserRef {
        UserRef { id, email: email.into(), name: name.into() }
    }

    fn hex_id(seed: i64) -> String {
        (0..32)
            .map(|i| format!("{:02x}", (seed as u8).wrapping_mul(37).wrapping_add((i as u8).wrapping_mul(11))))
            .collect()
    }

    pub(super) fn machine(
        id: i64,
        name: &str,
        os: &str,
        owner: &str,
        mine: bool,
        online: bool,
        in_session: bool,
    ) -> Machine {
        Machine {
            id,
            endpoint_id: hex_id(id),
            name: name.into(),
            os: os.into(),
            owner: user(if mine { 1 } else { 100 + id }, owner, ""),
            visibility: Visibility::Shared,
            online,
            in_session,
            last_seen: Some(now() - 3 * 86_400 - id * 3600),
            addrs: Vec::new(),
            can_access: true,
            access: rdesk_proto::Access::FULL,
            is_owner: mine,
            can_manage: mine,
            direct: false,
            version: "0.4.0".into(),
            ring: Default::default(),
            here: None,
            can_wake: !online,
        }
    }

    pub(super) fn machines() -> Vec<Machine> {
        let me = "jeferson.santos.silva@exemplo.com.br";
        let mut v = vec![
            machine(1, "PC da sala", "windows", me, true, true, false),
            machine(
                2,
                "Notebook da Ana Beatriz com um nome de computador muito, muito comprido para caber",
                "macos",
                "ana.beatriz.figueiredo.de.albuquerque@consultoria-exemplo.com.br",
                false,
                true,
                true,
            ),
            machine(3, "Servidor do escritório", "linux", me, true, false, false),
            machine(4, "Raspberry Pi da garagem", "linux", "carlos@exemplo.com", false, false, false),
            machine(5, "Estação de edição de vídeo", "windows", me, true, true, false),
            machine(6, "MacBook do Marcos", "macos", "marcos.pereira@exemplo.com", false, true, false),
        ];
        v[0].direct = true;
        v[0].visibility = Visibility::Shared;
        v[2].visibility = Visibility::Private;
        v[4].visibility = Visibility::Everyone;
        v[3].access = rdesk_proto::Access::VIEW;
        v[5].access = rdesk_proto::Access::FILES;
        v[5].visibility = Visibility::Everyone;
        v[3].last_seen = None;
        v
    }

    pub(super) fn locals() -> Vec<HostEntry> {
        vec![
            HostEntry {
                id: hex_id(40),
                name: "Impressora 3D do laboratório de prototipagem".into(),
                addr: "192.168.0.42".into(),
                ..Default::default()
            },
            HostEntry { id: hex_id(41), name: "Mini PC da TV".into(), ..Default::default() },
        ]
    }

    pub(super) fn requests() -> Vec<MachineRequest> {
        vec![
            MachineRequest {
                id: 1,
                endpoint_id: hex_id(50),
                code: "4F2A-91C7".into(),
                name: "DESKTOP-7K2M9QX".into(),
                os: "windows".into(),
                owner: Some(user(1, "jeferson.santos.silva@exemplo.com.br", "")),
                created_at: now() - 600,
                last_seen: now() - 30,
            },
            MachineRequest {
                id: 2,
                endpoint_id: hex_id(51),
                code: "B803-55DE".into(),
                name: "Notebook novo da recepção do consultório odontológico".into(),
                os: "macos".into(),
                owner: None,
                created_at: now() - 7200,
                last_seen: now() - 60,
            },
        ]
    }

    pub(super) fn history() -> Vec<HistoryEntry> {
        let e = |who: &str, client: &str, ago: i64, mins: Option<i64>, access: &str| HistoryEntry {
            who: who.into(),
            client: client.into(),
            method: "server ticket".into(),
            access: access.into(),
            started_at: now() - ago,
            ended_at: mins.map(|m| now() - ago + m * 60),
        };
        vec![
            e(
                "ana.beatriz.figueiredo.de.albuquerque@consultoria-exemplo.com.br",
                "Notebook da Ana",
                300,
                None,
                "full control",
            ),
            e("jeferson.santos.silva@exemplo.com.br", "PC do escritório", 7200, Some(42), "full control"),
            e("carlos@exemplo.com", "Celular", 86_400 * 2, Some(3), "view only"),
            e("mariana.costa@exemplo.com", "MacBook", 86_400 * 4, Some(95), "files only"),
            e("jeferson.santos.silva@exemplo.com.br", "PC do escritório", 86_400 * 6, Some(12), "full control"),
            e("marcos.pereira@exemplo.com", "PC", 86_400 * 9, Some(7), "full control"),
        ]
    }

    pub(super) fn shares() -> Shares {
        Shares {
            users: vec![
                (
                    user(
                        2,
                        "ana.beatriz.figueiredo.de.albuquerque@consultoria-exemplo.com.br",
                        "Ana Beatriz Figueiredo de Albuquerque",
                    ),
                    ShareRole::View,
                ),
                (user(3, "carlos@exemplo.com", ""), ShareRole::Files),
            ],
            groups: vec![(
                GroupRef { id: 10, name: "Suporte técnico do escritório de São Paulo".into() },
                ShareRole::Control,
            )],
        }
    }

    impl Computers {
        /// A page with the given list, without the worker, a server or this device's files.
        pub(super) fn sample(
            machines: Option<Result<Vec<Machine>, String>>,
            requests: Vec<MachineRequest>,
            locals: Vec<HostEntry>,
        ) -> Self {
            let (tx, _rx) = mpsc::unbounded_channel();
            let snap = Snapshot { machines, requests, agent_keys: None, refreshing: false };
            let mut c = Self::new(PathBuf::from("rdesk"), Arc::new(Mutex::new(snap)), tx);
            c.locals = locals;
            c.frozen = true;
            c
        }

        /// The full sample, with "PC da sala" selected and its details loaded.
        pub(super) fn sample_full() -> Self {
            let mut c = Self::sample(Some(Ok(machines())), requests(), locals());
            c.fake_sessions.push(machines()[4].endpoint_id.clone());
            c.select_sample(1);
            c
        }

        pub(super) fn select_sample(&mut self, id: i64) {
            self.selected = Some(Sel::Server(id));
            self.detail = Some(Detail {
                id,
                shares: Some(Ok(shares())),
                managers: Some(Ok(vec![user(4, "mariana.costa@exemplo.com", "Mariana Costa")])),
                history: Some(Ok(history())),
                job: None,
                stale: false,
            });
        }
    }
}

/// Screens for the ui gallery (see `gallery.rs`): sample data, long texts.
#[cfg(test)]
pub(super) fn gallery() -> Vec<super::gallery::Screen> {
    use super::Page;
    use super::gallery::{screen, with_sidebar};

    fn page(mut c: Computers, signed_in: bool) -> impl FnMut(&mut egui::Ui) {
        move |ui: &mut egui::Ui| with_sidebar(ui, Page::Computers, |ui| c.show(ui, signed_in))
    }

    vec![
        screen("computers-list", || page(Computers::sample_full(), true)),
        screen("computers-list-local-selected", || {
            let mut c = Computers::sample_full();
            c.selected = Some(Sel::Local(sample::locals()[0].id.clone()));
            page(c, true)
        }),
        screen("computers-list-shared-selected", || {
            let mut c = Computers::sample_full();
            c.selected = Some(Sel::Server(2));
            c.filter = Filter::SharedWithMe;
            page(c, true)
        }),
        screen("computers-details-dialog", || {
            let mut c = Computers::sample_full();
            c.dialog = Some(Dialog::Details);
            page(c, true)
        }),
        screen("computers-rename", || {
            let mut c = Computers::sample_full();
            c.renaming = Some((1, "PC da sala (novo nome)".into()));
            page(c, true)
        }),
        screen("computers-approvals", || {
            let mut c = Computers::sample_full();
            c.dialog = Some(Dialog::Approvals);
            page(c, true)
        }),
        screen("computers-approve", || {
            let mut c = Computers::sample_full();
            let r = sample::requests()[1].clone();
            c.dialog =
                Some(Dialog::Approve { name: r.name.clone(), request: Box::new(r), visibility: Visibility::Private });
            page(c, true)
        }),
        screen("computers-access", || {
            let mut c = Computers::sample_full();
            let m = sample::machines()[0].clone();
            c.dialog = Some(Dialog::Access(Box::new(AccessDialog::sample(&m, Visibility::Shared, ""))));
            page(c, true)
        }),
        screen("computers-access-search", || {
            let mut c = Computers::sample_full();
            let m = sample::machines()[0].clone();
            c.dialog = Some(Dialog::Access(Box::new(AccessDialog::sample(&m, Visibility::Shared, "mar"))));
            page(c, true)
        }),
        screen("computers-access-private", || {
            let mut c = Computers::sample_full();
            let m = sample::machines()[0].clone();
            c.dialog = Some(Dialog::Access(Box::new(AccessDialog::sample(&m, Visibility::Private, ""))));
            page(c, true)
        }),
        screen("computers-access-loading", || {
            let mut c = Computers::sample_full();
            let m = sample::machines()[0].clone();
            c.dialog = Some(Dialog::Access(Box::new(AccessDialog::sample_loading(&m))));
            page(c, true)
        }),
        screen("computers-history", || {
            let mut c = Computers::sample_full();
            let m = sample::machines()[0].clone();
            c.dialog =
                Some(Dialog::History(Box::new(History { machine: m, data: Some(Ok(sample::history())), job: None })));
            page(c, true)
        }),
        screen("computers-login", || {
            let mut c = Computers::sample_full();
            let mut o = LoginOptions::empty();
            o.name = "PC da sala".into();
            o.endpoint_id = sample::machines()[0].endpoint_id.clone();
            o.key = "SHA256:k3f97a2c1111222233334444555566667777888899990000aaaa".into();
            c.dialog = Some(Dialog::Login(Box::new(o)));
            page(c, true)
        }),
        screen("computers-local-edit", || {
            let mut c = Computers::sample_full();
            let mut o = LoginOptions::empty();
            o.local = Some(LocalFields { name: String::new(), id: String::new(), original_id: None });
            o.error = Some(
                "O ID do host são os 64 caracteres mostrados na página Este computador daquele computador.".into(),
            );
            c.dialog = Some(Dialog::Login(Box::new(o)));
            page(c, true)
        }),
        screen("computers-unregister", || {
            let mut c = Computers::sample_full();
            c.dialog = Some(Dialog::Unregister(sample::machines()[0].clone()));
            page(c, true)
        }),
        screen("computers-no-results", || {
            let mut c = Computers::sample_full();
            c.search = "k3f9@192.168.0.77".into();
            page(c, true)
        }),
        // Signed in on a computer someone else registered and shared nothing with: listed
        // first, no Connect or Files, its owner and who it is shared with, manageable here.
        screen("computers-this-of-someone-else", || {
            let mut list = sample::machines();
            list[5].here = Some(Here::View);
            list[5].can_access = false;
            list[5].access = rdesk_proto::Access::NONE;
            list[5].visibility = Visibility::Shared;
            let mut c = Computers::sample(Some(Ok(list.clone())), Vec::new(), sample::locals());
            c.this_host = Some(list[5].endpoint_id.clone());
            c.select_sample(6);
            c.detail.as_mut().unwrap().history = None;
            page(c, true)
        }),
        screen("computers-this-mine", || {
            let mut c = Computers::sample_full();
            c.this_host = Some(sample::machines()[0].endpoint_id.clone());
            page(c, true)
        }),
        screen("computers-this-not-registered", || {
            let mut c = Computers::sample_full();
            c.this_host = Some("ab".repeat(32));
            page(c, true)
        }),
        screen("computers-this-other-server", || {
            let mut c = Computers::sample_full();
            c.this_host = Some("ab".repeat(32));
            c.this_registered = Some("Notebook do escritório em rdesk da Família Santos".into());
            page(c, true)
        }),
        screen("computers-restart-as-admin", || {
            let mut c = Computers::sample_full();
            c.dialog = Some(Dialog::RestartAsAdmin);
            page(c, true)
        }),
        screen("computers-empty", || page(Computers::sample(Some(Ok(Vec::new())), Vec::new(), Vec::new()), true)),
        screen("computers-loading", || page(Computers::sample(None, Vec::new(), Vec::new()), true)),
        screen("computers-error", || {
            let e = "error sending request for url (https://rdesk.exemplo.com.br/api/machines): connection refused";
            page(Computers::sample(Some(Err(e.into())), Vec::new(), sample::locals()), true)
        }),
        screen("computers-local-mode", || page(Computers::sample(None, Vec::new(), sample::locals()), false)),
        screen("computers-local-mode-empty", || page(Computers::sample(None, Vec::new(), Vec::new()), false)),
    ]
}
