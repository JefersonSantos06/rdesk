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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use egui::{Color32, RichText, Sense, Vec2};
use rdesk_common::account::Session;
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, AuthPref, ClientConfig, HostEntry, parse_host_id};
use rdesk_proto::api::{GroupRef, Machine, UpdateMachine, UserRef, Visibility};
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
                    shared.lock().unwrap().machines = Some(Err(format!("cannot start worker: {e}")));
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
        let list = match Session::load() {
            Some(s) => match s.api() {
                Ok(api) => api.machines().await.map_err(|e| short_error(&e)),
                Err(e) => Err(short_error(&e)),
            },
            None => Err("not signed in".into()),
        };
        {
            let mut snap = shared.lock().unwrap();
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

fn vis_label(v: Visibility) -> &'static str {
    match v {
        Visibility::Private => "only me",
        Visibility::Shared => "shared",
        Visibility::Everyone => "everyone",
    }
}

struct Session_ {
    /// Host ID (what `rdesk connect` gets).
    target: String,
    label: String,
    child: std::process::Child,
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
}

impl LoginOptions {
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
        }
    }
}

/// Everyone and every group, to pick from.
type Choices = (Vec<UserRef>, Vec<GroupRef>);
/// The choices plus who it is shared with now (user ids, group ids).
type ShareLoad = Result<(Vec<UserRef>, Vec<GroupRef>, Vec<i64>, Vec<i64>), String>;

struct ShareEdit {
    machine: Machine,
    users: HashSet<i64>,
    groups: HashSet<i64>,
    /// Loaded in the background.
    choices: Option<Result<Choices, String>>,
    job: Option<Receiver<ShareLoad>>,
}

enum Dialog {
    Login(Box<LoginOptions>),
    Rename { machine: Machine, name: String },
    Share(Box<ShareEdit>),
    Unregister(Machine),
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
            return (Status::Open, "Session open".into());
        }
        if m.in_session {
            return (Status::Busy, "In use".into());
        }
        if m.online {
            return (Status::Ready, "Online".into());
        }
        let seen = match m.last_seen {
            Some(t) => format!("Offline · seen {}", rdesk_common::directory::ago(t, now())),
            None => "Offline · never connected".into(),
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
            self.notify("Not signed in.".into(), true);
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
            self.open(&m, false);
            return;
        }
        match t.split_once('@') {
            Some((id, _)) if parse_host_id(id).is_ok() => self.spawn_session(t, t, false),
            _ => self.notify(format!("{t}: no computer with that name or ID (ID@address for one on the LAN)."), true),
        }
    }

    fn open(&mut self, m: &Machine, files: bool) {
        if !m.can_access {
            self.notify(format!("{} is not shared with you.", m.name), true);
            return;
        }
        self.spawn_session(&m.endpoint_id, &m.name, files);
    }

    fn spawn_session(&mut self, target: &str, label: &str, files: bool) {
        if self.session_open(target) {
            let hint = if files { " Use Files in its ▼ rdesk menu." } else { "" };
            self.notify(format!("A window for {label} is already open.{hint}"), false);
            return;
        }
        let log = match session_log(label) {
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
                self.sessions.push(Session_ {
                    target: target.to_string(),
                    label: label.to_string(),
                    child,
                    started: Instant::now(),
                    log,
                });
                self.notify(format!("Opening {label}…"), false);
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
                    failed.push((s.label.clone(), last_error_line(&s.log)));
                }
                false
            }
            Err(_) => false,
        });
        for (t, why) in failed {
            self.notify(format!("Session to {t} failed: {why}"), true);
        }
    }

    fn save_login(&mut self, o: &LoginOptions) {
        let mut cfg = load_client_config();
        let opt = |s: &str| if s.trim().is_empty() { None } else { Some(s.trim().to_string()) };
        let entry = HostEntry {
            id: o.endpoint_id.clone(),
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
            self.notify(format!("Saving: {e:#}"), true);
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
            Ok(()) => self.notify(format!("Saved the login options of {}.", o.name), false),
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    // ---- drawing -----------------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui, snap: &Snapshot, list: &[Machine]) {
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.quick)
                    .hint_text("Computer name, host ID, or ID@address…")
                    .desired_width(280.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Connect").clicked() || enter) && !self.quick.trim().is_empty() {
                let t = self.quick.trim().to_string();
                self.quick_connect(&t, list);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳").on_hover_text("Refresh now").clicked() {
                    self.refresh();
                }
                if snap.refreshing || self.job.is_some() {
                    ui.spinner();
                }
            });
        });
        if let Some(Err(e)) = &snap.machines {
            ui.colored_label(ERROR, format!("Cannot get the list from the server: {e}"));
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
                    let owner = if m.is_owner { "Yours".to_string() } else { format!("By {}", m.owner.email) };
                    ui.label(RichText::new(format!("{owner} · {}", vis_label(m.visibility))).weak().small());
                    ui.label(RichText::new(detail).color(status.color()));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let label = if status == Status::Open { "Focus" } else { "Connect" };
                        if ui.add_enabled(m.can_access, egui::Button::new(label)).clicked() {
                            self.open(m, false);
                        }
                        if ui
                            .add_enabled(m.can_access, egui::Button::new("Files").small())
                            .on_hover_text("Open a session on the file-transfer panel")
                            .clicked()
                        {
                            self.open(m, true);
                        }
                        if m.is_owner && ui.small_button("Share…").clicked() {
                            self.start_share(&ctx, m);
                        }
                    });
                });
        });
        let resp = resp.response;
        if resp.double_clicked() {
            self.open(m, false);
        }
        resp.context_menu(|ui| {
            if ui.add_enabled(m.can_access, egui::Button::new("Connect")).clicked() {
                self.open(m, false);
                ui.close();
            }
            if ui.add_enabled(m.can_access, egui::Button::new("Files…")).clicked() {
                self.open(m, true);
                ui.close();
            }
            if ui
                .button("Login options…")
                .on_hover_text("For computers that also ask for their own password or key")
                .clicked()
            {
                self.dialog = Some(Dialog::Login(Box::new(LoginOptions::load(m))));
                ui.close();
            }
            if ui.button("Copy host ID").clicked() {
                ui.ctx().copy_text(m.endpoint_id.clone());
                ui.close();
            }
            if m.is_owner {
                ui.separator();
                if ui.button("Rename…").clicked() {
                    self.dialog = Some(Dialog::Rename { machine: m.clone(), name: m.name.clone() });
                    ui.close();
                }
                ui.menu_button("Who can see it", |ui| {
                    for v in [Visibility::Private, Visibility::Shared, Visibility::Everyone] {
                        let text = match v {
                            Visibility::Private => "Only me",
                            Visibility::Shared => "Me and whom I share it with",
                            Visibility::Everyone => "Everyone on the server",
                        };
                        if ui.radio(m.visibility == v, text).clicked() {
                            let id = m.id;
                            let name = m.name.clone();
                            self.run(&ctx, move |api| async move {
                                api.update_machine(id, &UpdateMachine { name: None, visibility: Some(v) }).await?;
                                Ok(format!("{name}: {}.", vis_label(v)))
                            });
                            ui.close();
                        }
                    }
                });
                if ui.button("Share…").clicked() {
                    self.start_share(&ctx, m);
                    ui.close();
                }
                if ui.button("Unregister…").clicked() {
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
                    users,
                    groups,
                    cur.users.iter().map(|u| u.id).collect(),
                    cur.groups.iter().map(|g| g.id).collect(),
                ))
            };
            f.await.map_err(|e| format!("{e:#}"))
        });
        self.dialog = Some(Dialog::Share(Box::new(ShareEdit {
            machine: m.clone(),
            users: HashSet::new(),
            groups: HashSet::new(),
            choices: None,
            job: Some(job),
        })));
    }

    fn dialogs(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        match &mut dialog {
            Dialog::Login(o) => {
                let m = egui::Modal::new(egui::Id::unique("login-opts")).show(ctx, |ui| {
                    ui.set_width(480.0);
                    ui.heading(format!("Login options · {}", o.name));
                    ui.label(
                        "Only for computers whose owner turned on the extra lock (their own password or \
                         SSH key, asked after the server's permission).",
                    );
                    ui.add_space(6.0);
                    egui::Grid::new("login-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label("Login");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut o.auth, AuthPref::Auto, "Key, then password");
                            ui.radio_value(&mut o.auth, AuthPref::Agent, "1Password key");
                            ui.radio_value(&mut o.auth, AuthPref::Password, "Password");
                        });
                        ui.end_row();
                        if o.auth != AuthPref::Password {
                            ui.label("SSH key");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut o.key)
                                        .hint_text("any key the computer accepts")
                                        .desired_width(250.0),
                                );
                                if ui.button("From 1Password…").clicked() {
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
                                    .hint_text("op://Private/rdesk/password (optional)")
                                    .desired_width(330.0),
                            );
                            ui.end_row();
                            if crate::secrets::available() {
                                ui.label("Saved password");
                                ui.horizontal(|ui| {
                                    if o.saved_password && !o.forget_password {
                                        ui.label(format!("✔ in {}", crate::secrets::store_name()));
                                        if ui.small_button("Forget").clicked() {
                                            o.forget_password = true;
                                        }
                                    } else {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut *o.new_password)
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
                        egui::Grid::new("login-adv").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                            ui.label("Address");
                            ui.add(
                                egui::TextEdit::singleline(&mut o.addr).hint_text("extra direct address (optional)"),
                            );
                            ui.end_row();
                            ui.label("Agent socket");
                            ui.add(egui::TextEdit::singleline(&mut o.agent_socket).hint_text("default: 1Password"));
                            ui.end_row();
                            ui.label("Key file");
                            ui.add(egui::TextEdit::singleline(&mut o.identity).hint_text("unencrypted OpenSSH key"));
                            ui.end_row();
                        });
                    });
                    ui.add_space(6.0);
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
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
                        let o = std::mem::replace(o, Box::new(LoginOptions::empty()));
                        self.save_login(&o);
                        keep = false;
                    }
                    Some(false) => keep = false,
                    None if m.should_close() => keep = false,
                    None => {}
                }
            }
            Dialog::Rename { machine, name } => {
                let m = egui::Modal::new(egui::Id::unique("rename-machine")).show(ctx, |ui| {
                    ui.heading(format!("Rename {}", machine.name));
                    let resp = ui.add(egui::TextEdit::singleline(name).desired_width(300.0));
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() || enter {
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
                        let (id, new) = (machine.id, name.trim().to_string());
                        self.run(ctx, move |api| async move {
                            let m =
                                api.update_machine(id, &UpdateMachine { name: Some(new), visibility: None }).await?;
                            Ok(format!("Renamed to {}.", m.name))
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
                        Ok((users, groups, cur_u, cur_g)) => {
                            sh.users = cur_u.into_iter().collect();
                            sh.groups = cur_g.into_iter().collect();
                            sh.choices = Some(Ok((users, groups)));
                        }
                        Err(e) => sh.choices = Some(Err(e)),
                    }
                }
                let me = Session::load().map(|s| s.user.id);
                let m = egui::Modal::new(egui::Id::unique("share-machine")).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.heading(format!("Share {}", sh.machine.name));
                    if sh.machine.visibility == Visibility::Everyone {
                        ui.colored_label(
                            Status::Busy.color(),
                            "It is visible to everyone right now; saving a list makes it shared with these only.",
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
                                ui.label(RichText::new("Groups").strong());
                                if groups.is_empty() {
                                    ui.weak("No groups yet (create them on the Groups page).");
                                }
                                for g in &groups {
                                    let mut on = sh.groups.contains(&g.id);
                                    if ui.checkbox(&mut on, &g.name).changed() {
                                        if on {
                                            sh.groups.insert(g.id)
                                        } else {
                                            sh.groups.remove(&g.id)
                                        };
                                    }
                                }
                                ui.add_space(6.0);
                                ui.label(RichText::new("People").strong());
                                for u in users.iter().filter(|u| Some(u.id) != me) {
                                    let label = if u.name.is_empty() {
                                        u.email.clone()
                                    } else {
                                        format!("{} ({})", u.name, u.email)
                                    };
                                    let mut on = sh.users.contains(&u.id);
                                    if ui.checkbox(&mut on, label).changed() {
                                        if on {
                                            sh.users.insert(u.id)
                                        } else {
                                            sh.users.remove(&u.id)
                                        };
                                    }
                                }
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui.button("Save").clicked() {
                                    done = Some(true);
                                }
                                if ui.button("Cancel").clicked() {
                                    done = Some(false);
                                }
                            });
                        }
                    }
                    if !matches!(sh.choices, Some(Ok(_))) && ui.button("Close").clicked() {
                        done = Some(false);
                    }
                    done
                });
                match m.inner {
                    Some(true) => {
                        let id = sh.machine.id;
                        let name = sh.machine.name.clone();
                        let users: Vec<i64> = sh.users.iter().copied().collect();
                        let groups: Vec<i64> = sh.groups.iter().copied().collect();
                        let vis = sh.machine.visibility;
                        self.run(ctx, move |api| async move {
                            api.set_shares(id, users.clone(), groups.clone()).await?;
                            if vis != Visibility::Shared && !(users.is_empty() && groups.is_empty()) {
                                let change = UpdateMachine { name: None, visibility: Some(Visibility::Shared) };
                                api.update_machine(id, &change).await?;
                            }
                            Ok(format!(
                                "{name} is shared with {} person(s) and {} group(s).",
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
            Dialog::Unregister(machine) => {
                let m = egui::Modal::new(egui::Id::unique("unregister-machine")).show(ctx, |ui| {
                    ui.heading(format!("Unregister {}?", machine.name));
                    ui.label(
                        "It leaves the server: nobody can reach it through the server any more, and its \
                         sharing is lost. Register it again from that computer's This computer page.",
                    );
                    ui.add_space(6.0);
                    let mut done = None;
                    ui.horizontal(|ui| {
                        if ui.button("Unregister").clicked() {
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
                        let (id, name) = (machine.id, machine.name.clone());
                        self.run(ctx, move |api| async move {
                            api.remove_machine(id).await?;
                            Ok(format!("{name} was unregistered."))
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

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let snap = self.shared.lock().unwrap().clone();
        let list: Vec<Machine> = match &snap.machines {
            Some(Ok(l)) => l.clone(),
            _ => Vec::new(),
        };
        self.header(ui, &snap, &list);
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Search").desired_width(220.0));
                ui.checkbox(&mut self.show_offline, "show offline");
            });
            let needle = self.search.trim().to_lowercase();
            let matches = |m: &Machine| {
                (needle.is_empty() || m.name.to_lowercase().contains(&needle) || m.owner.email.contains(&needle))
                    && (self.show_offline || m.online)
            };
            let mine: Vec<Machine> = list.iter().filter(|m| m.is_owner && matches(m)).cloned().collect();
            let others: Vec<Machine> = list.iter().filter(|m| !m.is_owner && matches(m)).cloned().collect();
            ui.add_space(10.0);
            ui.label(RichText::new("My computers").strong().size(17.0));
            ui.add_space(4.0);
            if mine.is_empty() {
                ui.weak("None yet. Register a computer on its This computer page.");
            }
            self.grid(ui, &mine);
            ui.add_space(18.0);
            ui.label(RichText::new("Shared with me").strong().size(17.0));
            ui.add_space(4.0);
            if others.is_empty() {
                ui.weak(match &snap.machines {
                    None => "Loading…",
                    _ => "Nothing shared with you yet.",
                });
            }
            self.grid(ui, &others);
        });
        self.dialogs(&ctx, &snap);
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
