//! The "Computers" page: the computers this user can reach, as the server lists them, with
//! live status (online and in use come from the relay and the hosts' own sync), in the
//! spirit of TeamViewer's computer list.
//!
//! * Whoever registered a computer renames it, chooses who may see it (only them, the
//!   users and groups they share it with, or everyone) and can unregister it.
//! * Every session runs as its own `rdesk connect <host ID>` process (its own window); the
//!   ticket from the server and the host ID are checked by the connection itself.
//! * "Login options" keep, per computer, how to answer its optional extra lock (its own
//!   password or SSH key).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use egui::{Color32, RichText, Sense, Vec2};
use rdesk_common::account::Session;
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, AuthPref, ClientConfig, HostEntry, parse_host_id};
use rdesk_common::i18n::{access_label, role_label, visibility_label};
use rdesk_common::{t, tf};
use rdesk_proto::api::{
    CreatedEnrollKey, EnrollKey, GroupRef, HistoryEntry, Machine, MachineRequest, NewEnrollKey, SetShares, ShareRole,
    UpdateMachine, UserRef, Visibility,
};
use tokio::sync::mpsc;

use super::account::spawn;

const REFRESH_EVERY: Duration = Duration::from_secs(5);
const CARD: Vec2 = Vec2::new(260.0, 128.0);
const ERROR: Color32 = Color32::from_rgb(230, 70, 70);

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
            Some(s) => match s.api() {
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
}

impl Status {
    fn color(self) -> Color32 {
        match self {
            Status::Ready => Color32::from_rgb(46, 184, 92),
            Status::Busy => Color32::from_rgb(240, 160, 40),
            Status::Open => Color32::from_rgb(70, 140, 240),
            Status::Offline => Color32::from_gray(120),
        }
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

/// Everyone and every group, to pick from.
type Choices = (Vec<UserRef>, Vec<GroupRef>);
/// Who it is shared with now, with roles.
type Current = (Vec<(i64, ShareRole)>, Vec<(i64, ShareRole)>);
/// The choices plus the current shares.
type ShareLoad = Result<(Choices, Current), String>;

struct ShareEdit {
    machine: Machine,
    users: HashMap<i64, ShareRole>,
    groups: HashMap<i64, ShareRole>,
    /// Loaded in the background.
    choices: Option<Result<Choices, String>>,
    job: Option<Receiver<ShareLoad>>,
}

/// Everyone, and the current managers' ids.
type ManagersLoad = Result<(Vec<UserRef>, Vec<i64>), String>;

struct ManagersEdit {
    machine: Machine,
    selected: HashSet<i64>,
    people: Option<Result<Vec<UserRef>, String>>,
    job: Option<Receiver<ManagersLoad>>,
}

struct History {
    machine: Machine,
    data: Option<Result<Vec<HistoryEntry>, String>>,
    job: Option<Receiver<Result<Vec<HistoryEntry>, String>>>,
}

/// The user's enrollment keys and every group (to share the computers a key adds).
type KeysLoad = Result<(Vec<EnrollKey>, Vec<GroupRef>), String>;

struct NewKeyForm {
    name: String,
    reusable: bool,
    days: u32,
    visibility: Visibility,
    groups: HashMap<i64, ShareRole>,
    error: Option<String>,
}

/// "Add a computer": the ways to add one, and the enrollment keys.
struct AddComputer {
    keys: Option<KeysLoad>,
    job: Option<Receiver<KeysLoad>>,
    form: Option<NewKeyForm>,
    create_job: Option<Receiver<Result<CreatedEnrollKey, String>>>,
    /// The key just created: shown once.
    created: Option<String>,
}

enum Dialog {
    Login(Box<LoginOptions>),
    AddComputer(Box<AddComputer>),
    Approve { request: Box<MachineRequest>, name: String, visibility: Visibility },
    Rename { machine: Machine, name: String },
    Share(Box<ShareEdit>),
    Managers(Box<ManagersEdit>),
    History(Box<History>),
    Unregister(Machine),
}

/// A share's role, as a small drop-down.
fn role_picker(ui: &mut egui::Ui, salt: impl std::hash::Hash + std::fmt::Debug, role: &mut ShareRole) {
    egui::ComboBox::from_id_salt(salt).selected_text(role_label(*role)).width(110.0).show_ui(ui, |ui| {
        for r in ShareRole::ALL {
            ui.selectable_value(role, r, role_label(r));
        }
    });
}

fn when(t: i64) -> String {
    rdesk_common::directory::ago(t, now())
}

pub(super) struct Computers {
    exe: PathBuf,
    shared: Arc<Mutex<Snapshot>>,
    tx: mpsc::UnboundedSender<WorkerCmd>,
    search: String,
    quick: String,
    show_offline: bool,
    dialog: Option<Dialog>,
    sessions: Vec<Session_>,
    job: Option<Receiver<Result<String, String>>>,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl Computers {
    pub(super) fn new(exe: PathBuf, shared: Arc<Mutex<Snapshot>>, tx: mpsc::UnboundedSender<WorkerCmd>) -> Self {
        Self {
            exe,
            shared,
            tx,
            search: String::new(),
            quick: String::new(),
            show_offline: true,
            dialog: None,
            sessions: Vec::new(),
            job: None,
            toast: None,
        }
    }

    fn notify(&mut self, text: String, error: bool) {
        self.toast = Some((text, error, Instant::now()));
    }

    pub(super) fn refresh(&self) {
        let _ = self.tx.send(WorkerCmd::Refresh);
    }

    fn session_open(&self, target: &str) -> bool {
        self.sessions.iter().any(|s| s.target == target)
    }

    fn status(&self, m: &Machine) -> (Status, String) {
        if self.session_open(&m.endpoint_id) {
            return (Status::Open, t!("Session open", "Sessão aberta").into());
        }
        if m.in_session {
            return (Status::Busy, t!("In use", "Em uso").into());
        }
        if m.online {
            return (Status::Ready, t!("Online", "Online").into());
        }
        let seen = match m.last_seen {
            Some(t) => tf!("Offline · seen {}", "Offline · visto {}", rdesk_common::directory::ago(t, now())),
            None => t!("Offline · never connected", "Offline · nunca conectado").into(),
        };
        (Status::Offline, seen)
    }

    /// Run a change on the server in the background, then refresh.
    fn run<F, Fut>(&mut self, ctx: &egui::Context, f: F)
    where
        F: FnOnce(rdesk_common::account::Api) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let Some(s) = Session::load() else {
            self.notify(t!("Not signed in.", "Você não entrou em um servidor.").into(), true);
            return;
        };
        self.job = Some(spawn(ctx, move || async move {
            match s.api() {
                Ok(api) => f(api).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            }
        }));
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

    fn spawn_session(&mut self, target: &str, label: &str, how: Open) {
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

    // ---- drawing -----------------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui, snap: &Snapshot, list: &[Machine]) {
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.quick)
                    .hint_text(t!(
                        "Computer name, host ID, or ID@address…",
                        "Nome do computador, ID do host ou ID@endereço…"
                    ))
                    .desired_width(280.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button(t!("Connect", "Conectar")).clicked() || enter) && !self.quick.trim().is_empty() {
                let t = self.quick.trim().to_string();
                self.quick_connect(&t, list);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳").on_hover_text(t!("Refresh now", "Atualizar agora")).clicked() {
                    self.refresh();
                }
                if ui.button(t!("＋ Add a computer…", "＋ Adicionar um computador…")).clicked() {
                    let d = AddComputer {
                        keys: None,
                        job: Some(load_keys(ui.ctx(), None)),
                        form: None,
                        create_job: None,
                        created: None,
                    };
                    self.dialog = Some(Dialog::AddComputer(Box::new(d)));
                }
                if snap.refreshing || self.job.is_some() {
                    ui.spinner();
                }
            });
        });
        if let Some(Err(e)) = &snap.machines {
            ui.colored_label(
                ERROR,
                tf!("Cannot get the list from the server: {e}", "Não foi possível obter a lista do servidor: {e}"),
            );
        }
    }

    fn card_ui(&mut self, ui: &mut egui::Ui, m: &Machine) {
        let (status, detail) = self.status(m);
        let fill = ui.visuals().faint_bg_color;
        let ctx = ui.ctx().clone();
        let resp = ui.scope_builder(egui::UiBuilder::new().id_salt(&m.endpoint_id).sense(Sense::click()), |ui| {
            egui::Frame::group(ui.style())
                .fill(fill)
                .corner_radius(10.0)
                .inner_margin(12.0)
                .stroke(egui::Stroke::new(1.0, status.color().gamma_multiply(0.6)))
                .show(ui, |ui| {
                    ui.set_min_size(CARD);
                    ui.set_max_width(CARD.x);
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        ui.painter().circle_filled(rect.center(), 6.0, status.color());
                        ui.label(RichText::new(os_icon(&m.os)).size(18.0));
                        ui.label(RichText::new(&m.name).strong().size(16.0));
                    });
                    let owner = if m.is_owner { t!("Yours", "Seu").to_string() } else { tf!("By {}", "De {}", m.owner.email) };
                    let mine = if m.access != rdesk_proto::Access::FULL && m.can_access {
                        tf!(" · you: {}", " · você: {}", access_label(m.access))
                    } else {
                        String::new()
                    };
                    ui.label(RichText::new(format!("{owner} · {}{mine}", visibility_label(m.visibility))).weak().small());
                    if m.direct {
                        ui.label(
                            RichText::new(t!(
                                "⚠ direct access on: its own password/key also open it on its network",
                                "⚠ acesso direto ligado: a senha/chave dele também o abre na rede dele"
                            ))
                                .color(Color32::from_rgb(240, 160, 40))
                                .small(),
                        )
                        .on_hover_text(
                            t!(
                                "Turned on at that computer (This computer → Options): a second way in, besides sharing",
                                "Ligado naquele computador (Este computador → Opções): uma segunda forma de entrar, além do compartilhamento"
                            ),
                        );
                    }
                    ui.label(RichText::new(detail).color(status.color()));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let label = if status == Status::Open { t!("Focus", "Mostrar") } else { t!("Connect", "Conectar") };
                        if ui.add_enabled(m.can_access, egui::Button::new(label)).clicked() {
                            self.open(m, Open::Screen);
                        }
                        if ui
                            .add_enabled(m.can_access, egui::Button::new(t!("Files", "Arquivos")).small())
                            .on_hover_text(t!(
                                "Open a session on the file-transfer panel",
                                "Abrir uma sessão no painel de transferência de arquivos"
                            ))
                            .clicked()
                        {
                            self.open(m, Open::Files);
                        }
                        if m.access.terminal
                            && ui
                                .add_enabled(m.can_access, egui::Button::new(t!("Terminal", "Terminal")).small())
                                .on_hover_text(t!(
                                    "A shell on that computer, as whoever is signed in there",
                                    "Um shell naquele computador, como quem estiver logado lá"
                                ))
                                .clicked()
                        {
                            self.open(m, Open::Terminal);
                        }
                        if m.can_manage && ui.small_button(t!("Share…", "Compartilhar…")).clicked() {
                            self.start_share(&ctx, m);
                        }
                    });
                });
        });
        let resp = resp.response;
        if resp.double_clicked() {
            self.open(m, Open::Screen);
        }
        resp.context_menu(|ui| {
            if ui.add_enabled(m.can_access, egui::Button::new(t!("Connect", "Conectar"))).clicked() {
                self.open(m, Open::Screen);
                ui.close();
            }
            if ui.add_enabled(m.can_access, egui::Button::new(t!("Files…", "Arquivos…"))).clicked() {
                self.open(m, Open::Files);
                ui.close();
            }
            if ui
                .add_enabled(m.can_access && m.access.terminal, egui::Button::new(t!("Terminal…", "Terminal…")))
                .clicked()
            {
                self.open(m, Open::Terminal);
                ui.close();
            }
            if ui
                .button(t!("Login options…", "Opções de login…"))
                .on_hover_text(t!(
                    "For computers that also ask for their own password or key",
                    "Para computadores que também pedem a própria senha ou chave"
                ))
                .clicked()
            {
                self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load(m))));
                ui.close();
            }
            if ui.button(t!("Copy host ID", "Copiar ID do host")).clicked() {
                ui.ctx().copy_text(m.endpoint_id.clone());
                ui.close();
            }
            if m.can_manage {
                ui.separator();
                if ui.button(t!("History…", "Histórico…")).clicked() {
                    self.start_history(&ctx, m);
                    ui.close();
                }
                if ui.button(t!("Rename…", "Renomear…")).clicked() {
                    self.dialog = Some(Dialog::Rename { machine: m.clone(), name: m.name.clone() });
                    ui.close();
                }
                ui.menu_button(t!("Who can see it", "Quem pode ver"), |ui| {
                    for v in [Visibility::Private, Visibility::Shared, Visibility::Everyone] {
                        let text = match v {
                            Visibility::Private => t!("Only me", "Só eu"),
                            Visibility::Shared => t!("Me and whom I share it with", "Eu e com quem eu compartilho"),
                            Visibility::Everyone => t!("Everyone on the server", "Todos no servidor"),
                        };
                        if ui.radio(m.visibility == v, text).clicked() {
                            let id = m.id;
                            let name = m.name.clone();
                            self.run(&ctx, move |api| async move {
                                api.update_machine(id, &UpdateMachine { name: None, visibility: Some(v) }).await?;
                                Ok(format!("{name}: {}.", visibility_label(v)))
                            });
                            ui.close();
                        }
                    }
                });
                if ui.button(t!("Share…", "Compartilhar…")).clicked() {
                    self.start_share(&ctx, m);
                    ui.close();
                }
            }
            if m.is_owner {
                if ui
                    .button(t!("Managers…", "Gestores…"))
                    .on_hover_text(t!(
                        "Others who may share, rename and see the history",
                        "Outras pessoas que podem compartilhar, renomear e ver o histórico"
                    ))
                    .clicked()
                {
                    self.start_managers(&ctx, m);
                    ui.close();
                }
                if ui.button(t!("Unregister…", "Descadastrar…")).clicked() {
                    self.dialog = Some(Dialog::Unregister(m.clone()));
                    ui.close();
                }
            }
        });
    }

    fn start_share(&mut self, ctx: &egui::Context, m: &Machine) {
        let Some(s) = Session::load() else { return };
        let id = m.id;
        let job = spawn(ctx, move || async move {
            let api = s.api().map_err(|e| format!("{e:#}"))?;
            let f = async {
                let users = api.directory().await?;
                let groups = api.all_groups().await?;
                let cur = api.shares(id).await?;
                anyhow::Ok((
                    (users, groups),
                    (
                        cur.users.iter().map(|(u, r)| (u.id, *r)).collect(),
                        cur.groups.iter().map(|(g, r)| (g.id, *r)).collect(),
                    ),
                ))
            };
            f.await.map_err(|e| format!("{e:#}"))
        });
        self.dialog = Some(Dialog::Share(Box::new(ShareEdit {
            machine: m.clone(),
            users: HashMap::new(),
            groups: HashMap::new(),
            choices: None,
            job: Some(job),
        })));
    }

    fn start_managers(&mut self, ctx: &egui::Context, m: &Machine) {
        let Some(s) = Session::load() else { return };
        let id = m.id;
        let job = spawn(ctx, move || async move {
            let api = s.api().map_err(|e| format!("{e:#}"))?;
            let f = async {
                let people = api.directory().await?;
                let cur = api.managers(id).await?;
                anyhow::Ok((people, cur.users.iter().map(|u| u.id).collect()))
            };
            f.await.map_err(|e| format!("{e:#}"))
        });
        self.dialog = Some(Dialog::Managers(Box::new(ManagersEdit {
            machine: m.clone(),
            selected: HashSet::new(),
            people: None,
            job: Some(job),
        })));
    }

    fn start_history(&mut self, ctx: &egui::Context, m: &Machine) {
        let Some(s) = Session::load() else { return };
        let id = m.id;
        let job = spawn(ctx, move || async move {
            match s.api() {
                Ok(api) => api.history(id).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            }
        });
        self.dialog = Some(Dialog::History(Box::new(History { machine: m.clone(), data: None, job: Some(job) })));
    }

    fn dialogs(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        match &mut dialog {
            Dialog::Login(o) => {
                let m = egui::Modal::new(egui::Id::unique("login-opts")).show(ctx, |ui| {
                    ui.set_width(480.0);
                    if let Some(l) = &mut o.local {
                        ui.heading(if l.original_id.is_some() {
                            t!("Computer on this network", "Computador nesta rede")
                        } else {
                            t!("Add a computer on this network", "Adicionar um computador nesta rede")
                        });
                        ui.label(t!(
                            "Reached without the server: on the local network or Tailscale, with that computer's \
                             own password or SSH key (its owner allows this on its This computer page).",
                            "Acessado sem o servidor: na rede local ou no Tailscale, com a senha ou chave SSH do \
                             próprio computador (o dono dele permite isso na página Este computador).",
                        ));
                        ui.add_space(6.0);
                        egui::Grid::new("local-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                            ui.label(t!("Name", "Nome"));
                            ui.add(
                                egui::TextEdit::singleline(&mut l.name)
                                    .hint_text(t!("e.g. PC da sala", "ex.: PC da sala")),
                            );
                            ui.end_row();
                            ui.label(t!("Host ID", "ID do host"));
                            ui.add(
                                egui::TextEdit::singleline(&mut l.id)
                                    .hint_text(t!(
                                        "64 characters, from its This computer page",
                                        "64 caracteres, da página Este computador dele"
                                    ))
                                    .desired_width(330.0),
                            );
                            ui.end_row();
                            ui.label(t!("Address", "Endereço"));
                            ui.add(
                                egui::TextEdit::singleline(&mut o.addr)
                                    .hint_text(t!(
                                        "192.168.0.10, pc.tailnet.ts.net… (empty: find it by mDNS)",
                                        "192.168.0.10, pc.tailnet.ts.net… (vazio: encontrar por mDNS)"
                                    ))
                                    .desired_width(330.0),
                            );
                            ui.end_row();
                        });
                    } else {
                        ui.heading(tf!("Login options · {}", "Opções de login · {}", o.name));
                        ui.label(t!(
                            "Only for computers whose owner turned on the extra lock (their own password or \
                             SSH key, asked after the server's permission).",
                            "Só para computadores cujo dono ligou o cadeado extra (a senha ou chave SSH do \
                             próprio computador, pedida depois da permissão do servidor).",
                        ));
                    }
                    ui.add_space(6.0);
                    egui::Grid::new("login-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label(t!("Login", "Login"));
                        ui.horizontal(|ui| {
                            ui.radio_value(
                                &mut o.auth,
                                AuthPref::Auto,
                                t!("Key, then password", "Chave, depois senha"),
                            );
                            ui.radio_value(&mut o.auth, AuthPref::Agent, t!("1Password key", "Chave do 1Password"));
                            ui.radio_value(&mut o.auth, AuthPref::Password, t!("Password", "Senha"));
                        });
                        ui.end_row();
                        if o.auth != AuthPref::Password {
                            ui.label(t!("SSH key", "Chave SSH"));
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut o.key)
                                        .hint_text(t!(
                                            "any key the computer accepts",
                                            "qualquer chave que o computador aceite"
                                        ))
                                        .desired_width(250.0),
                                );
                                if ui.button(t!("From 1Password…", "Do 1Password…")).clicked() {
                                    let sock =
                                        (!o.agent_socket.trim().is_empty()).then(|| o.agent_socket.trim().to_string());
                                    self.shared.lock().unwrap().agent_keys = None;
                                    let _ = self.tx.send(WorkerCmd::AgentKeys(sock));
                                    o.show_keys = true;
                                }
                            });
                            ui.end_row();
                            if o.show_keys {
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
                                            if ui.selectable_label(o.key == *fp, format!("{label}\n{fp}")).clicked() {
                                                o.key = fp.clone();
                                                o.show_keys = false;
                                            }
                                        }
                                    }
                                });
                                ui.end_row();
                            }
                        }
                        if o.auth != AuthPref::Agent {
                            ui.label("1Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut o.password_ref)
                                    .hint_text(t!(
                                        "op://Private/rdesk/password (optional)",
                                        "op://Private/rdesk/password (opcional)"
                                    ))
                                    .desired_width(330.0),
                            );
                            ui.end_row();
                            if crate::secrets::available() {
                                ui.label(t!("Saved password", "Senha salva"));
                                ui.horizontal(|ui| {
                                    if o.saved_password && !o.forget_password {
                                        ui.label(tf!("✔ in {}", "✔ em {}", crate::secrets::store_name()));
                                        if ui.small_button(t!("Forget", "Esquecer")).clicked() {
                                            o.forget_password = true;
                                        }
                                    } else {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut *o.new_password)
                                                .password(true)
                                                .hint_text(t!("type to save it", "digite para salvar"))
                                                .desired_width(250.0),
                                        );
                                    }
                                });
                                ui.end_row();
                            }
                        }
                    });
                    egui::CollapsingHeader::new(t!("Advanced", "Avançado")).id_salt("login-advanced").show(ui, |ui| {
                        egui::Grid::new("login-adv").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                            ui.label(t!("Address", "Endereço"));
                            ui.add(
                                egui::TextEdit::singleline(&mut o.addr).hint_text(t!(
                                    "extra direct address (optional)",
                                    "endereço direto extra (opcional)"
                                )),
                            );
                            ui.end_row();
                            ui.label(t!("Agent socket", "Socket do agente"));
                            ui.add(
                                egui::TextEdit::singleline(&mut o.agent_socket)
                                    .hint_text(t!("default: 1Password", "padrão: 1Password")),
                            );
                            ui.end_row();
                            ui.label(t!("Key file", "Arquivo de chave"));
                            ui.add(
                                egui::TextEdit::singleline(&mut o.identity)
                                    .hint_text(t!("unencrypted OpenSSH key", "chave OpenSSH sem criptografia")),
                            );
                            ui.end_row();
                        });
                    });
                    ui.add_space(6.0);
                    if let Some(e) = &o.error {
                        ui.colored_label(ERROR, e);
                    }
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button(t!("Save", "Salvar")).clicked() {
                            done = Some(true);
                        }
                        if ui.button(t!("Cancel", "Cancelar")).clicked() {
                            done = Some(false);
                        }
                    });
                    done
                });
                match m.inner {
                    Some(true) => match Self::check_local(o) {
                        Ok(()) => {
                            let o = std::mem::replace(o, Box::new(LoginOptions::empty()));
                            self.save_login(&o);
                            keep = false;
                        }
                        Err(e) => o.error = Some(e),
                    },
                    Some(false) => keep = false,
                    None if m.should_close() => keep = false,
                    None => {}
                }
            }
            Dialog::Rename { machine, name } => {
                let m = egui::Modal::new(egui::Id::unique("rename-machine")).show(ctx, |ui| {
                    ui.heading(tf!("Rename {}", "Renomear {}", machine.name));
                    let resp = ui.add(egui::TextEdit::singleline(name).desired_width(300.0));
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button(t!("Rename", "Renomear")).clicked() || enter {
                            done = Some(true);
                        }
                        if ui.button(t!("Cancel", "Cancelar")).clicked() {
                            done = Some(false);
                        }
                    });
                    done
                });
                match m.inner {
                    Some(true) => {
                        let (id, new) = (machine.id, name.trim().to_string());
                        self.run(ctx, move |api| async move {
                            let m =
                                api.update_machine(id, &UpdateMachine { name: Some(new), visibility: None }).await?;
                            Ok(tf!("Renamed to {}.", "Renomeado para {}.", m.name))
                        });
                        keep = false;
                    }
                    Some(false) => keep = false,
                    None if m.should_close() => keep = false,
                    None => {}
                }
            }
            Dialog::Share(sh) => {
                if let Some(rx) = &sh.job
                    && let Ok(r) = rx.try_recv()
                {
                    sh.job = None;
                    match r {
                        Ok(((users, groups), (cur_u, cur_g))) => {
                            sh.users = cur_u.into_iter().collect();
                            sh.groups = cur_g.into_iter().collect();
                            sh.choices = Some(Ok((users, groups)));
                        }
                        Err(e) => sh.choices = Some(Err(e)),
                    }
                }
                let me = Session::load().map(|s| s.user.id);
                let m = egui::Modal::new(egui::Id::unique("share-machine")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading(tf!("Share {}", "Compartilhar {}", sh.machine.name));
                    ui.weak(t!(
                        "Full control: screen, keyboard, mouse, clipboard and files. View only: just the screen.",
                        "Controle total: tela, teclado, mouse, área de transferência e arquivos. Só visualizar: só a tela."
                    ));
                    if sh.machine.visibility == Visibility::Everyone {
                        ui.colored_label(
                            Status::Busy.color(),
                            t!(
                                "It is visible to everyone right now; saving a list makes it shared with these only.",
                                "Agora ele está visível para todos; salvar uma lista o compartilha só com estes."
                            ),
                        );
                    }
                    let mut done = None;
                    match &sh.choices {
                        None => {
                            ui.spinner();
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ERROR, e);
                        }
                        Some(Ok((users, groups))) => {
                            let (users, groups) = (users.clone(), groups.clone());
                            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                                ui.label(RichText::new(t!("Groups", "Grupos")).strong());
                                if groups.is_empty() {
                                    ui.weak(t!("No groups yet (create them on the Groups page).", "Nenhum grupo ainda (crie-os na página Grupos)."));
                                }
                                for g in &groups {
                                    ui.horizontal(|ui| {
                                        let mut on = sh.groups.contains_key(&g.id);
                                        if ui.checkbox(&mut on, &g.name).changed() {
                                            if on {
                                                sh.groups.insert(g.id, ShareRole::Control);
                                            } else {
                                                sh.groups.remove(&g.id);
                                            }
                                        }
                                        if let Some(r) = sh.groups.get_mut(&g.id) {
                                            role_picker(ui, ("g", g.id), r);
                                        }
                                    });
                                }
                                ui.add_space(6.0);
                                ui.label(RichText::new(t!("People", "Pessoas")).strong());
                                for u in users.iter().filter(|u| Some(u.id) != me) {
                                    let label = if u.name.is_empty() {
                                        u.email.clone()
                                    } else {
                                        format!("{} ({})", u.name, u.email)
                                    };
                                    ui.horizontal(|ui| {
                                        let mut on = sh.users.contains_key(&u.id);
                                        if ui.checkbox(&mut on, label).changed() {
                                            if on {
                                                sh.users.insert(u.id, ShareRole::Control);
                                            } else {
                                                sh.users.remove(&u.id);
                                            }
                                        }
                                        if let Some(r) = sh.users.get_mut(&u.id) {
                                            role_picker(ui, ("u", u.id), r);
                                        }
                                    });
                                }
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui.button(t!("Save", "Salvar")).clicked() {
                                    done = Some(true);
                                }
                                if ui.button(t!("Cancel", "Cancelar")).clicked() {
                                    done = Some(false);
                                }
                            });
                        }
                    }
                    if !matches!(sh.choices, Some(Ok(_))) && ui.button(t!("Close", "Fechar")).clicked() {
                        done = Some(false);
                    }
                    done
                });
                match m.inner {
                    Some(true) => {
                        let id = sh.machine.id;
                        let name = sh.machine.name.clone();
                        let users: Vec<(i64, ShareRole)> = sh.users.iter().map(|(k, v)| (*k, *v)).collect();
                        let groups: Vec<(i64, ShareRole)> = sh.groups.iter().map(|(k, v)| (*k, *v)).collect();
                        let vis = sh.machine.visibility;
                        self.run(ctx, move |api| async move {
                            api.set_shares(id, users.clone(), groups.clone()).await?;
                            if vis != Visibility::Shared && !(users.is_empty() && groups.is_empty()) {
                                let change = UpdateMachine { name: None, visibility: Some(Visibility::Shared) };
                                api.update_machine(id, &change).await?;
                            }
                            Ok(tf!(
                                "{name} is shared with {} person(s) and {} group(s).",
                                "{name} está compartilhado com {} pessoa(s) e {} grupo(s).",
                                users.len(),
                                groups.len()
                            ))
                        });
                        keep = false;
                    }
                    Some(false) => keep = false,
                    None if m.should_close() => keep = false,
                    None => {}
                }
            }
            Dialog::Managers(me_) => {
                if let Some(rx) = &me_.job
                    && let Ok(r) = rx.try_recv()
                {
                    me_.job = None;
                    match r {
                        Ok((people, cur)) => {
                            me_.selected = cur.into_iter().collect();
                            me_.people = Some(Ok(people));
                        }
                        Err(e) => me_.people = Some(Err(e)),
                    }
                }
                let owner = me_.machine.owner.id;
                let m = egui::Modal::new(egui::Id::unique("managers")).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.heading(tf!("Managers of {}", "Gestores de {}", me_.machine.name));
                    ui.label(t!(
                        "Managers have full access and may share it, rename it, change who sees it and see its history.",
                        "Gestores têm acesso total e podem compartilhá-lo, renomeá-lo, mudar quem o vê e ver o histórico dele."
                    ));
                    let mut done = None;
                    match &me_.people {
                        None => {
                            ui.spinner();
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ERROR, e);
                        }
                        Some(Ok(people)) => {
                            let people = people.clone();
                            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                                for p in people.iter().filter(|p| p.id != owner) {
                                    let mut on = me_.selected.contains(&p.id);
                                    if ui.checkbox(&mut on, &p.email).changed() {
                                        if on {
                                            me_.selected.insert(p.id);
                                        } else {
                                            me_.selected.remove(&p.id);
                                        }
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                if ui.button(t!("Save", "Salvar")).clicked() {
                                    done = Some(true);
                                }
                                if ui.button(t!("Cancel", "Cancelar")).clicked() {
                                    done = Some(false);
                                }
                            });
                        }
                    }
                    if !matches!(me_.people, Some(Ok(_))) && ui.button(t!("Close", "Fechar")).clicked() {
                        done = Some(false);
                    }
                    done
                });
                match m.inner {
                    Some(true) => {
                        let (id, name) = (me_.machine.id, me_.machine.name.clone());
                        let users: Vec<i64> = me_.selected.iter().copied().collect();
                        self.run(ctx, move |api| async move {
                            let r = api.set_managers(id, users).await?;
                            Ok(tf!("{name}: {} manager(s).", "{name}: {} gestor(es).", r.users.len()))
                        });
                        keep = false;
                    }
                    Some(false) => keep = false,
                    None if m.should_close() => keep = false,
                    None => {}
                }
            }
            Dialog::History(h) => {
                if let Some(rx) = &h.job
                    && let Ok(r) = rx.try_recv()
                {
                    h.job = None;
                    h.data = Some(r);
                }
                let m = egui::Modal::new(egui::Id::unique("history")).show(ctx, |ui| {
                    ui.set_width(620.0);
                    ui.heading(tf!("History of {}", "Histórico de {}", h.machine.name));
                    match &h.data {
                        None => {
                            ui.spinner();
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ERROR, e);
                        }
                        Some(Ok(list)) if list.is_empty() => {
                            ui.weak(t!("No sessions yet.", "Nenhuma sessão ainda."));
                        }
                        Some(Ok(list)) => {
                            egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                                egui::Grid::new("hist").num_columns(4).striped(true).spacing([12.0, 6.0]).show(
                                    ui,
                                    |ui| {
                                        ui.strong(t!("Who", "Quem"));
                                        ui.strong(t!("When", "Quando"));
                                        ui.strong(t!("How long", "Duração"));
                                        ui.strong(t!("Access", "Acesso"));
                                        ui.end_row();
                                        for e in list {
                                            ui.label(format!("{} ({})", e.who, e.client)).on_hover_text(&e.method);
                                            ui.label(when(e.started_at));
                                            ui.label(match e.ended_at {
                                                Some(t) => format!("{} min", ((t - e.started_at).max(0) + 59) / 60),
                                                None => t!("still connected", "ainda conectado").into(),
                                            });
                                            ui.label(&e.access);
                                            ui.end_row();
                                        }
                                    },
                                );
                            });
                        }
                    }
                    ui.button(t!("Close", "Fechar")).clicked()
                });
                if m.inner || m.should_close() {
                    keep = false;
                }
            }
            Dialog::AddComputer(d) => {
                if !self.add_computer_ui(ctx, d) {
                    keep = false;
                }
            }
            Dialog::Approve { request, name, visibility } => {
                let m = egui::Modal::new(egui::Id::unique("approve-machine")).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.heading(tf!("Approve {}?", "Aprovar {}?", request.name));
                    ui.label(tf!(
                        "Only if that computer shows the code {}. Once approved it is yours: you choose who may \
                         reach it, and can share, rename or unregister it.",
                        "Só se aquele computador mostrar o código {}. Depois de aprovado, ele é seu: você escolhe \
                         quem pode acessá-lo e pode compartilhá-lo, renomeá-lo ou descadastrá-lo.",
                        request.code
                    ));
                    ui.add_space(6.0);
                    egui::Grid::new("approve-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label(t!("Name", "Nome"));
                        ui.add(egui::TextEdit::singleline(name).desired_width(260.0));
                        ui.end_row();
                        ui.label(t!("Who can see it", "Quem pode ver"));
                        ui.vertical(|ui| {
                            for v in [Visibility::Private, Visibility::Shared, Visibility::Everyone] {
                                ui.radio_value(visibility, v, visibility_label(v));
                            }
                        });
                        ui.end_row();
                    });
                    ui.add_space(6.0);
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button(t!("Approve", "Aprovar")).clicked() {
                            done = Some(true);
                        }
                        if ui.button(t!("Cancel", "Cancelar")).clicked() {
                            done = Some(false);
                        }
                    });
                    done
                });
                match m.inner {
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
                    }
                    Some(false) => keep = false,
                    None if m.should_close() => keep = false,
                    None => {}
                }
            }
            Dialog::Unregister(machine) => {
                let m = egui::Modal::new(egui::Id::unique("unregister-machine")).show(ctx, |ui| {
                    ui.heading(tf!("Unregister {}?", "Descadastrar {}?", machine.name));
                    ui.label(t!(
                        "It leaves the server: nobody can reach it through the server any more, and its \
                         sharing is lost. Register it again from that computer's This computer page.",
                        "Ele sai do servidor: ninguém mais consegue acessá-lo pelo servidor, e o compartilhamento \
                         dele se perde. Cadastre-o de novo na página Este computador daquele computador.",
                    ));
                    ui.add_space(6.0);
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button(t!("Unregister", "Descadastrar")).clicked() {
                            done = Some(true);
                        }
                        if ui.button(t!("Cancel", "Cancelar")).clicked() {
                            done = Some(false);
                        }
                    });
                    done
                });
                match m.inner {
                    Some(true) => {
                        let (id, name) = (machine.id, machine.name.clone());
                        self.run(ctx, move |api| async move {
                            api.remove_machine(id).await?;
                            Ok(tf!("{name} was unregistered.", "{name} foi descadastrado."))
                        });
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
}

impl Computers {
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
            self.refresh();
        }
    }

    /// `signed_in`: the server's computers are shown too (not in the local-network mode).
    pub(super) fn show(&mut self, ui: &mut egui::Ui, signed_in: bool) {
        let ctx = ui.ctx().clone();
        let snap = self.shared.lock().unwrap().clone();
        if !signed_in {
            ui.colored_label(
                Color32::from_rgb(240, 160, 40),
                t!(
                    "Not signed in to a server: only computers on this network (local network or Tailscale).",
                    "Sem servidor: só computadores nesta rede (rede local ou Tailscale)."
                ),
            );
            ui.add_space(6.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.local_ui(ui));
            self.dialogs(&ctx, &snap);
            return;
        }
        let list: Vec<Machine> = match &snap.machines {
            Some(Ok(l)) => l.clone(),
            _ => Vec::new(),
        };
        self.header(ui, &snap, &list);
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text(t!("🔍 Search", "🔍 Buscar"))
                        .desired_width(220.0),
                );
                ui.checkbox(&mut self.show_offline, t!("show offline", "mostrar offline"));
            });
            if !snap.requests.is_empty() {
                ui.add_space(10.0);
                self.requests_ui(ui, &snap.requests);
            }
            let needle = self.search.trim().to_lowercase();
            let matches = |m: &Machine| {
                (needle.is_empty() || m.name.to_lowercase().contains(&needle) || m.owner.email.contains(&needle))
                    && (self.show_offline || m.online)
            };
            let mine: Vec<Machine> = list.iter().filter(|m| m.is_owner && matches(m)).cloned().collect();
            let others: Vec<Machine> = list.iter().filter(|m| !m.is_owner && matches(m)).cloned().collect();
            ui.add_space(10.0);
            ui.label(RichText::new(t!("My computers", "Meus computadores")).strong().size(17.0));
            ui.add_space(4.0);
            if mine.is_empty() {
                ui.weak(t!(
                    "None yet. Register a computer on its This computer page.",
                    "Nenhum ainda. Cadastre um computador na página Este computador dele."
                ));
            }
            self.grid(ui, &mine);
            ui.add_space(18.0);
            ui.label(RichText::new(t!("Shared with me", "Compartilhados comigo")).strong().size(17.0));
            ui.add_space(4.0);
            if others.is_empty() {
                ui.weak(match &snap.machines {
                    None => t!("Loading…", "Carregando…"),
                    _ => t!("Nothing shared with you yet.", "Nada compartilhado com você ainda."),
                });
            }
            self.grid(ui, &others);
            ui.add_space(18.0);
            self.local_ui(ui);
        });
        self.dialogs(&ctx, &snap);
    }

    /// Computers saved on this device, reached without the server.
    fn local_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(t!("On this network", "Nesta rede")).strong().size(17.0));
            if ui
                .button(t!("＋ Add…", "＋ Adicionar…"))
                .on_hover_text(t!(
                    "A computer on the local network or Tailscale",
                    "Um computador na rede local ou no Tailscale"
                ))
                .clicked()
            {
                self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load_local(None))));
            }
        });
        ui.weak(t!(
            "Saved on this device; reached directly with the computer's own password or SSH key.",
            "Salvos neste dispositivo; acessados diretamente com a senha ou chave SSH do próprio computador."
        ));
        ui.add_space(4.0);
        let list = local_computers();
        if list.is_empty() {
            ui.weak(t!("None yet.", "Nenhum ainda."));
        }
        let mut remove = None;
        for e in &list {
            egui::Frame::group(ui.style()).corner_radius(8.0).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&e.name).strong());
                    let where_ = if e.addr.trim().is_empty() {
                        t!("found by mDNS", "encontrado por mDNS").to_string()
                    } else {
                        e.addr.clone()
                    };
                    ui.weak(format!("{} · {}", &e.id[..12.min(e.id.len())], where_));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(t!("Remove", "Remover")).clicked() {
                            remove = Some(e.clone());
                        }
                        if ui.small_button(t!("Edit…", "Editar…")).clicked() {
                            self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load_local(Some(e)))));
                        }
                        if ui.button(t!("Terminal", "Terminal")).clicked() {
                            self.spawn_session(&local_target(e), &e.name, Open::Terminal);
                        }
                        if ui.button(t!("Files", "Arquivos")).clicked() {
                            self.spawn_session(&local_target(e), &e.name, Open::Files);
                        }
                        if ui.button(t!("Connect", "Conectar")).clicked() {
                            self.spawn_session(&local_target(e), &e.name, Open::Screen);
                        }
                    });
                });
            });
        }
        if let Some(e) = remove {
            let mut cfg = load_client_config();
            cfg.hosts.remove(&e.id);
            let _ = crate::secrets::forget(&e.name);
            match config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &cfg)) {
                Ok(()) => self
                    .notify(tf!("Removed {} from this device.", "{} foi removido deste dispositivo.", e.name), false),
                Err(err) => self.notify(tf!("Saving: {err:#}", "Ao salvar: {err:#}"), true),
            }
        }
    }

    /// Computers that asked to be added, with Approve / Refuse.
    fn requests_ui(&mut self, ui: &mut egui::Ui, requests: &[MachineRequest]) {
        let ctx = ui.ctx().clone();
        ui.label(RichText::new(t!("Waiting for your approval", "Aguardando sua aprovação")).strong().size(17.0));
        ui.weak(t!(
            "Approve only computers you expect: compare the code with the one the computer shows.",
            "Aprove só computadores que você espera: compare o código com o que o computador mostra."
        ));
        ui.add_space(4.0);
        egui::Grid::new("requests").num_columns(5).spacing([14.0, 6.0]).striped(true).show(ui, |ui| {
            for r in requests {
                ui.label(format!("{} {}", os_icon(&r.os), r.name));
                ui.monospace(&r.code);
                ui.weak(match &r.owner {
                    Some(o) => tf!("for {}", "para {}", o.email),
                    None => t!("no owner (for admins)", "sem dono (para admins)").into(),
                });
                ui.weak(tf!("asked {}", "pediu {}", when(r.created_at)));
                ui.horizontal(|ui| {
                    if ui.button(t!("Approve…", "Aprovar…")).clicked() {
                        self.dialog = Some(Dialog::Approve {
                            request: Box::new(r.clone()),
                            name: r.name.clone(),
                            visibility: Visibility::Private,
                        });
                    }
                    if ui.button(t!("Refuse", "Recusar")).clicked() {
                        let (id, name) = (r.id, r.name.clone());
                        self.run(&ctx, move |api| async move {
                            api.reject_machine(id).await?;
                            Ok(tf!("{name} was refused.", "{name} foi recusado."))
                        });
                    }
                });
                ui.end_row();
            }
        });
        ui.add_space(8.0);
    }

    /// The "Add a computer" window; `false` once closed.
    fn add_computer_ui(&mut self, ctx: &egui::Context, d: &mut AddComputer) -> bool {
        if let Some(rx) = &d.job
            && let Ok(r) = rx.try_recv()
        {
            d.job = None;
            d.keys = Some(r);
        }
        if let Some(rx) = &d.create_job
            && let Ok(r) = rx.try_recv()
        {
            d.create_job = None;
            match r {
                Ok(c) => {
                    d.created = Some(c.key);
                    d.form = None;
                    d.job = Some(load_keys(ctx, None));
                }
                Err(e) => {
                    if let Some(f) = &mut d.form {
                        f.error = Some(e);
                    }
                }
            }
        }
        let (server, email) = Session::load().map(|s| (s.server.clone(), s.user.email.clone())).unwrap_or_default();
        let mut open = true;
        egui::Window::new(t!("Add a computer", "Adicionar um computador"))
            .id(egui::Id::unique("add-computer"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(560.0).show(ui, |ui| {
                    ui.label(RichText::new(t!("Signing in on it", "Entrando nele")).strong());
                    ui.label(t!(
                        "Install rdesk there, sign in, and register it on its This computer page.",
                        "Instale o rdesk lá, entre e cadastre-o na página Este computador dele."
                    ));
                    ui.add_space(10.0);

                    ui.label(RichText::new(t!("Without signing in on it", "Sem entrar nele")).strong());
                    ui.label(t!(
                        "On its sign-in screen, choose \"Only share this computer\" and enter your e-mail. Or, on \
                         the command line:",
                        "Na tela de entrada dele, escolha \"Só compartilhar este computador\" e digite seu e-mail. \
                         Ou, na linha de comando:",
                    ));
                    copy_box(ui, &format!("rdesk host enroll --server {server} --owner {email}\nrdesk host run"));
                    ui.weak(t!(
                        "It then shows up here, under Waiting for your approval, with a code to compare.",
                        "Ele então aparece aqui, em Aguardando sua aprovação, com um código para comparar."
                    ));
                    ui.add_space(10.0);

                    ui.label(
                        RichText::new(t!(
                            "Ready-made images (Raspberry Pi, Orange Pi…)",
                            "Imagens prontas (Raspberry Pi, Orange Pi…)"
                        ))
                        .strong(),
                    );
                    ui.label(t!(
                        "An enrollment key adds computers at once, with no approval: whoever has the key can add \
                         computers to your account until it expires or you revoke it.",
                        "Uma chave de cadastro adiciona computadores na hora, sem aprovação: quem tiver a chave \
                         pode adicionar computadores à sua conta até ela expirar ou você revogá-la.",
                    ));
                    if let Some(k) = &d.created {
                        ui.add_space(4.0);
                        ui.colored_label(
                            Color32::from_rgb(46, 184, 92),
                            t!("New key (shown only now, copy it):", "Nova chave (mostrada só agora, copie-a):"),
                        );
                        copy_box(ui, &format!("RDESK_SERVER={server}\nRDESK_ENROLL_KEY={k}"));
                        ui.weak(t!(
                            "Put these in the host service's environment, or run `rdesk host enroll --key …`.",
                            "Coloque isto no ambiente do serviço do host, ou rode `rdesk host enroll --key …`."
                        ));
                    }
                    ui.add_space(4.0);
                    let revoke = keys_table(ui, d.keys.as_ref());
                    if let Some(id) = revoke {
                        d.job = Some(load_keys(ctx, Some(id)));
                    }
                    ui.add_space(6.0);
                    let groups = match &d.keys {
                        Some(Ok((_, g))) => g.clone(),
                        _ => Vec::new(),
                    };
                    match &mut d.form {
                        None => {
                            if ui.button(t!("New key…", "Nova chave…")).clicked() {
                                d.created = None;
                                d.form = Some(NewKeyForm {
                                    name: String::new(),
                                    reusable: true,
                                    days: 7,
                                    visibility: Visibility::Private,
                                    groups: HashMap::new(),
                                    error: None,
                                });
                            }
                        }
                        Some(f) => match new_key_form(ui, f, &groups, d.create_job.is_some()) {
                            Some(true) => {
                                let req = NewEnrollKey {
                                    name: f.name.trim().to_string(),
                                    reusable: f.reusable,
                                    expires_in_days: f.days,
                                    visibility: f.visibility,
                                    shares: SetShares {
                                        users: Vec::new(),
                                        groups: if f.visibility == Visibility::Shared {
                                            f.groups.iter().map(|(g, r)| (*g, *r)).collect()
                                        } else {
                                            Vec::new()
                                        },
                                    },
                                };
                                f.error = None;
                                d.create_job = Some(spawn(ctx, move || async move {
                                    let s = Session::load().ok_or_else(|| {
                                        t!("not signed in", "você não entrou em um servidor").to_string()
                                    })?;
                                    let api = s.api().map_err(|e| format!("{e:#}"))?;
                                    api.create_enroll_key(&req).await.map_err(|e| format!("{e:#}"))
                                }));
                            }
                            Some(false) => d.form = None,
                            None => {}
                        },
                    }
                });
            });
        open
    }

    fn grid(&mut self, ui: &mut egui::Ui, list: &[Machine]) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(10.0);
            for m in list {
                self.card_ui(ui, m);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The user's keys and all groups, after revoking `revoke`.
fn load_keys(ctx: &egui::Context, revoke: Option<i64>) -> Receiver<KeysLoad> {
    spawn(ctx, move || async move {
        let s = Session::load().ok_or_else(|| t!("not signed in", "você não entrou em um servidor").to_string())?;
        let api = s.api().map_err(|e| format!("{e:#}"))?;
        if let Some(id) = revoke {
            api.revoke_enroll_key(id).await.map_err(|e| format!("{e:#}"))?;
        }
        let (keys, groups) = tokio::join!(api.enroll_keys(), api.all_groups());
        Ok((keys.map_err(|e| format!("{e:#}"))?, groups.map_err(|e| format!("{e:#}"))?))
    })
}

/// Read-only text with a Copy button.
fn copy_box(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        let mut shown = text;
        ui.add(egui::TextEdit::multiline(&mut shown).code_editor().desired_rows(2).desired_width(480.0));
        if ui.button(t!("Copy", "Copiar")).clicked() {
            ui.ctx().copy_text(text.to_string());
        }
    });
}

/// The enrollment keys; returns the one to revoke.
fn keys_table(ui: &mut egui::Ui, keys: Option<&KeysLoad>) -> Option<i64> {
    let mut revoke = None;
    match keys {
        None => {
            ui.spinner();
        }
        Some(Err(e)) => {
            ui.colored_label(ERROR, e);
        }
        Some(Ok((keys, _))) if keys.is_empty() => {
            ui.weak(t!("No keys yet.", "Nenhuma chave ainda."));
        }
        Some(Ok((keys, _))) => {
            egui::Grid::new("enroll-keys").num_columns(5).spacing([12.0, 4.0]).striped(true).show(ui, |ui| {
                for k in keys {
                    ui.label(&k.name);
                    ui.monospace(format!("{}…", k.prefix));
                    let used = if k.reusable {
                        tf!("{} computer(s)", "{} computador(es)", k.uses)
                    } else if k.uses > 0 {
                        t!("used", "usada").into()
                    } else {
                        t!("single use", "uso único").into()
                    };
                    ui.weak(format!("{} · {used}", visibility_label(k.visibility)));
                    let live = !k.revoked && k.expires_at > now();
                    if k.revoked {
                        ui.weak(t!("revoked", "revogada"));
                    } else if !live {
                        ui.weak(t!("expired", "expirada"));
                    } else {
                        ui.weak(tf!(
                            "{} day(s) left",
                            "{} dia(s) restante(s)",
                            (k.expires_at - now() + 86_399) / 86_400
                        ));
                    }
                    if live && ui.button(t!("Revoke", "Revogar")).clicked() {
                        revoke = Some(k.id);
                    }
                    ui.end_row();
                }
            });
        }
    }
    revoke
}

/// The new-key form: `Some(true)` create, `Some(false)` cancel.
fn new_key_form(ui: &mut egui::Ui, f: &mut NewKeyForm, groups: &[GroupRef], busy: bool) -> Option<bool> {
    let mut out = None;
    ui.group(|ui| {
        egui::Grid::new("new-key").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(t!("Name", "Nome"));
            ui.add(
                egui::TextEdit::singleline(&mut f.name)
                    .hint_text(t!("e.g. Raspberry Pis", "ex.: Raspberry Pis"))
                    .desired_width(240.0),
            );
            ui.end_row();
            ui.label(t!("Use", "Uso"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut f.reusable, true, t!("many computers", "vários computadores"));
                ui.radio_value(&mut f.reusable, false, t!("one computer", "um computador"));
            });
            ui.end_row();
            ui.label(t!("Valid for", "Válida por"));
            ui.add(egui::DragValue::new(&mut f.days).range(1..=365).suffix(t!(" day(s)", " dia(s)")));
            ui.end_row();
            ui.label(t!("Who can see them", "Quem pode vê-los"));
            ui.vertical(|ui| {
                for v in [Visibility::Private, Visibility::Shared, Visibility::Everyone] {
                    ui.radio_value(&mut f.visibility, v, visibility_label(v));
                }
            });
            ui.end_row();
            if f.visibility == Visibility::Shared {
                ui.label(t!("Shared with", "Compartilhados com"));
                ui.vertical(|ui| {
                    if groups.is_empty() {
                        ui.weak(t!(
                            "No groups yet (create one on the Groups page).",
                            "Nenhum grupo ainda (crie um na página Grupos)."
                        ));
                    }
                    for g in groups {
                        ui.horizontal(|ui| {
                            let mut on = f.groups.contains_key(&g.id);
                            if ui.checkbox(&mut on, &g.name).changed() {
                                if on {
                                    f.groups.insert(g.id, ShareRole::Control);
                                } else {
                                    f.groups.remove(&g.id);
                                }
                            }
                            if let Some(role) = f.groups.get_mut(&g.id) {
                                role_picker(ui, ("key-role", g.id), role);
                            }
                        });
                    }
                });
                ui.end_row();
            }
        });
        if let Some(e) = &f.error {
            ui.colored_label(ERROR, e);
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy && !f.name.trim().is_empty(), egui::Button::new(t!("Create key", "Criar chave")))
                .clicked()
            {
                out = Some(true);
            }
            if ui.button(t!("Cancel", "Cancelar")).clicked() {
                out = Some(false);
            }
            if busy {
                ui.spinner();
            }
        });
    });
    out
}

fn os_icon(os: &str) -> &'static str {
    match os.to_ascii_lowercase().as_str() {
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
}
