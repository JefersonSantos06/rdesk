//! "This computer": the built-in host. A dashboard: whether it is on and how others reach
//! it, who is connected, what is set up (server, service, password, keys), then tabs for
//! security, performance, activity and diagnostics.
//!
//! On Windows the host can instead be run by the rdesk service (as SYSTEM, so it also
//! works on the sign-in and lock screens and through UAC). Then this page shows that host's
//! status through its control pipe, and changing its settings needs administrator rights.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use egui::{Align, Layout, Margin, RichText, Stroke, Ui};
use rdesk_common::account::{Registration, Session};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, HostConfig, format_host_id};
use rdesk_common::directory;
use rdesk_common::{t, tf};
use rdesk_host::admin::{self, CheckLine};
use rdesk_host::control::HostEvent;
use rdesk_host::ipc::RemoteHost;
use rdesk_host::{HostHandle, HostState, HostStatus, Reporter, RunOpts};
use rdesk_proto::api::{Machine, Shares, UserRef, Visibility};
use ssh_key::PublicKey;
use zeroize::Zeroizing;

use super::autostart;
use super::presence::{self, AdminGate, GateEvent};
use crate::ui::{self, Tone, icons, space};

/// The page's content is at most this wide (lines stay readable on a large window).
const MAX_W: f32 = 880.0;
/// Rows of settings and of the checklist are at least this tall.
const ROW_H: f32 = 44.0;

enum Job {
    PasswordSet(Result<(), String>),
    OpRead(Result<Zeroizing<String>, String>),
    AgentKeys(Result<Vec<PublicKey>, String>),
    Diagnose(Vec<CheckLine>),
}

enum Dialog {
    Password {
        a: Zeroizing<String>,
        b: Zeroizing<String>,
        op_ref: String,
        shown: Option<String>,
        error: Option<String>,
    },
    AddKey {
        text: String,
        agent: Option<Result<Vec<PublicKey>, String>>,
        error: Option<String>,
    },
    Register {
        error: Option<String>,
    },
    RemovePassword,
    /// The key's fingerprint and how it is shown (algorithm and comment).
    RemoveKey {
        fp: String,
        name: String,
    },
    NewIdentity,
    /// The name it is registered as.
    Unregister(String),
    UninstallService,
    /// Who is connected.
    Disconnect(String),
    /// Windows: unregistering someone else's computer here needs the app as administrator.
    RestartAsAdmin,
}

/// Who it is shared with and who manages it (for the registration row), by machine id.
type AccessInfo = (i64, Result<(Shares, Vec<UserRef>), String>);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Tab {
    Overview,
    Security,
    Performance,
    Activity,
    Diagnostics,
}

/// Something a click asks for, done after drawing (closures only record it).
enum Act {
    GoComputers,
    /// "Manage access" for this computer (on the Computers page).
    ManageAccess,
    AskUnregister,
    Tab(Tab),
    Open(Dialog),
    AddKey,
    CancelEnroll,
    /// Install (`true`) or remove the Windows service.
    Service(bool),
    ManageAsAdmin,
    Toggle,
    RunCheck,
    Apply,
    Discard,
    PickInbox,
}

/// The gallery's stand-in for the machine: who runs the host and whether it runs.
#[cfg(test)]
struct Sample {
    service: bool,
    running: bool,
}

pub(super) struct HostPage {
    exe: PathBuf,
    /// Registration with the server: the name and visibility typed for it, and the job.
    reg_name: String,
    reg_visibility: Visibility,
    reg_job: Option<Receiver<Result<String, String>>>,
    /// This computer's registration (read from disk now and then; the service's is in its status).
    registration: Option<Registration>,
    /// "Only share this computer" on the sign-in screen: owner e-mail or enrollment key.
    join_use_key: bool,
    join_owner: String,
    join_key: String,
    rt: Arc<tokio::runtime::Runtime>,
    reporter: Reporter,
    handle: Option<HostHandle>,
    /// The host run by the Windows service, when it is installed (then `handle` stays empty).
    service: Option<RemoteHost>,
    service_job: Option<Receiver<Result<String, String>>>,
    service_checked: Instant,
    /// This app runs with administrator rights (needed to change the service's host).
    elevated: bool,
    /// The options being edited, and as saved (the host runs with those).
    cfg: HostConfig,
    saved_cfg: HostConfig,
    keys: Vec<PublicKey>,
    has_password: bool,
    fingerprint: Option<[u8; 32]>,
    tab: Tab,
    dialog: Option<Dialog>,
    jobs: (Sender<Job>, Receiver<Job>),
    busy: Option<&'static str>,
    diag: Option<Vec<CheckLine>>,
    pub(super) toast: Option<(String, bool, Instant)>,
    /// Another page to show (set by this page, taken by the window).
    pub(super) go_to: Option<super::Page>,
    /// Ask the shell to quit (after starting an elevated copy).
    pub(super) quit_requested: bool,
    /// This computer as the server lists it for whoever is signed in (set by the window).
    pub(super) this_machine: Option<Machine>,
    /// Who it is shared with, loaded now and then, and when.
    access: Option<(AccessInfo, Instant)>,
    access_job: Option<Receiver<AccessInfo>>,
    /// "Manage access" was clicked: the window opens it on the Computers page.
    pub(super) manage_access: Option<Machine>,
    /// Unregistering it as someone else's: waits for an administrator.
    gate: AdminGate<()>,
    #[cfg(test)]
    sample: Option<Sample>,
}

impl HostPage {
    pub(super) fn new(exe: PathBuf, ctx: egui::Context) -> Self {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("rdesk-host")
                .enable_all()
                .build()
                .expect("tokio runtime"),
        );
        let reporter = Reporter::new(move || ctx.request_repaint());
        let mut p = Self::with(exe, rt, reporter);
        p.elevated = rdesk_host::elevation::is_elevated();
        if config::service_mode() {
            p.service = Some(RemoteHost::start(p.rt.handle(), p.reporter.clone()));
        }
        p.reload();
        p
    }

    /// The page with nothing loaded yet.
    fn with(exe: PathBuf, rt: Arc<tokio::runtime::Runtime>, reporter: Reporter) -> Self {
        Self {
            exe,
            reg_name: rdesk_common::local_hostname(),
            reg_visibility: Visibility::Private,
            reg_job: None,
            registration: None,
            join_use_key: false,
            join_owner: String::new(),
            join_key: String::new(),
            rt,
            reporter,
            handle: None,
            service: None,
            service_job: None,
            service_checked: Instant::now(),
            elevated: false,
            cfg: HostConfig::default(),
            saved_cfg: HostConfig::default(),
            keys: Vec::new(),
            has_password: false,
            fingerprint: None,
            tab: Tab::Overview,
            dialog: None,
            jobs: channel(),
            busy: None,
            diag: None,
            toast: None,
            go_to: None,
            quit_requested: false,
            this_machine: None,
            access: None,
            access_job: None,
            manage_access: None,
            gate: AdminGate::new(),
            #[cfg(test)]
            sample: None,
        }
    }

    /// The Windows service runs this computer's host.
    fn by_service(&self) -> bool {
        #[cfg(test)]
        if let Some(s) = &self.sample {
            return s.service;
        }
        self.service.is_some()
    }

    /// Follow `rdesk service install|uninstall`, from this app or from a terminal.
    fn check_service_mode(&mut self) {
        let installed = config::service_mode();
        if installed && self.service.is_none() {
            // The service now runs the host, with the same identity: stop ours.
            if let Some(h) = self.handle.take() {
                self.rt.block_on(h.shutdown());
            }
            self.service = Some(RemoteHost::start(self.rt.handle(), self.reporter.clone()));
            self.reload();
        } else if !installed && self.service.is_some() {
            self.service = None;
            self.reporter.replace(HostStatus::default());
            self.reload();
            if remembered_enabled() {
                self.start();
            }
        }
    }

    /// Install or remove the service (asks for administrator permission).
    fn service_action(&mut self, ctx: &egui::Context, install: bool) {
        let (tx, rx) = channel();
        let (exe, ctx) = (self.exe.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(autostart::service_action(&exe, install).map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
        self.service_job = Some(rx);
    }

    /// Applying a change to the service's host: it restarts with the new settings.
    fn restart_service_host(&mut self) {
        if let Some(remote) = &self.service {
            match self.rt.block_on(remote.restart()) {
                Ok(()) => self.notify(
                    t!("Applied: the service's host is restarting.", "Aplicado: o host do serviço está reiniciando."),
                    false,
                ),
                Err(e) => self.notify(
                    tf!(
                        "Saved, but the host could not be restarted: {e:#}",
                        "Salvo, mas não foi possível reiniciar o host: {e:#}"
                    ),
                    true,
                ),
            }
        }
    }

    fn reload(&mut self) {
        self.cfg = admin::load_config().unwrap_or_default();
        self.saved_cfg = self.cfg.clone();
        self.keys = admin::load_keys().unwrap_or_default();
        self.has_password = admin::has_password();
        self.fingerprint = admin::fingerprint().ok();
        self.registration = Registration::load();
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some((text.into(), error, Instant::now()));
    }

    pub(super) fn status(&self) -> HostStatus {
        self.reporter.snapshot()
    }

    pub(super) fn running(&self) -> bool {
        if self.by_service() {
            return matches!(self.status().state, HostState::Listening(_) | HostState::Starting);
        }
        #[cfg(test)]
        if let Some(s) = &self.sample {
            return s.running;
        }
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// One word for the tray: "on", "off", "in use", "waiting", "error" (shown, never compared).
    pub(super) fn summary(&self) -> String {
        let st = self.status();
        match (&st.state, &st.session) {
            (_, Some(s)) if self.running() => tf!("in use by {}", "em uso por {}", s.client),
            (HostState::Starting, _) if st.enrollment.is_some() => {
                t!("waiting for approval", "aguardando aprovação").into()
            }
            (HostState::Listening(_), _) => t!("on", "ligado").into(),
            (HostState::Starting, _) => t!("starting", "iniciando").into(),
            (HostState::Failed(_), _) => t!("error", "erro").into(),
            (HostState::Stopped, _) => t!("off", "desligado").into(),
        }
    }

    /// The host ID, as clients enter it.
    pub(super) fn fingerprint_text(&self) -> Option<String> {
        self.fingerprint.or(self.status().fingerprint).map(|f| format_host_id(&f))
    }

    /// This computer's host ID, to find it in the Computers list.
    pub(super) fn this_id(&self) -> Option<String> {
        self.fingerprint_text()
    }

    /// "name on server" when registered.
    pub(super) fn registration_text(&self) -> Option<String> {
        self.registered_as(&self.status())
    }

    /// From the Computers page: register it.
    pub(super) fn ask_register(&mut self) {
        if !self.is_registered(&self.status()) {
            self.dialog = Some(Dialog::Register { error: None });
        }
    }

    /// From the Computers page, or this one: unregister it (as someone else's computer,
    /// once an administrator confirmed).
    pub(super) fn ask_unregister(&mut self, ctx: &egui::Context) {
        let needed = self.this_machine.as_ref().and_then(presence::remove_scope).is_some();
        if self.gate.request(ctx, needed, ()).is_some() {
            self.open_unregister();
        }
    }

    fn open_unregister(&mut self) {
        if let Some(name) = self.registered_as(&self.status()) {
            self.dialog = Some(Dialog::Unregister(name));
        }
    }

    fn poll_gate(&mut self) {
        match self.gate.poll() {
            GateEvent::Nothing => {}
            GateEvent::Go(()) => self.open_unregister(),
            GateEvent::NeedsRestart => self.dialog = Some(Dialog::RestartAsAdmin),
            GateEvent::Refused(e) => self.notify(e, true),
        }
    }

    /// Who it is shared with and who manages it, read again every 15 s while shown.
    fn load_access(&mut self) {
        if let Some(rx) = &self.access_job
            && let Ok(info) = rx.try_recv()
        {
            self.access_job = None;
            self.access = Some((info, Instant::now()));
        }
        let Some(m) = &self.this_machine else {
            self.access = None;
            return;
        };
        let fresh = self.access.as_ref().is_some_and(|((id, _), at)| *id == m.id && at.elapsed().as_secs() < 15);
        if fresh || self.access_job.is_some() {
            return;
        }
        let Some(s) = Session::load() else { return };
        let (id, scope) = (m.id, presence::view_scope(m));
        let (tx, rx) = channel();
        self.rt.spawn(async move {
            let r = async {
                let api = presence::api(&s, scope).await?;
                let shares = api.shares(id).await?;
                let managers = api.managers(id).await?.users;
                anyhow::Ok((shares, managers))
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send((id, r));
        });
        self.access_job = Some(rx);
    }

    /// What the registration row says about who may reach it.
    fn access_of_this(&self) -> Option<&Result<(Shares, Vec<UserRef>), String>> {
        let m = self.this_machine.as_ref()?;
        self.access.as_ref().filter(|((id, _), _)| *id == m.id).map(|((_, r), _)| r)
    }

    pub(super) fn start(&mut self) {
        if self.service.is_some() || self.running() {
            return;
        }
        match admin::ensure_initialized() {
            Ok(fp) => self.fingerprint = Some(fp),
            Err(e) => {
                self.notify(tf!("Cannot set up the host: {e:#}", "Não foi possível configurar o host: {e:#}"), true);
                return;
            }
        }
        self.handle = Some(HostHandle::start(RunOpts::default(), self.rt.handle(), self.reporter.clone()));
        remember_enabled(true);
    }

    pub(super) fn stop(&mut self) {
        if self.service.is_some() {
            self.notify(
                t!(
                    "The rdesk service runs this computer's host: it is always on.",
                    "O serviço do rdesk executa o host deste computador: ele fica sempre ligado."
                ),
                false,
            );
            return;
        }
        if let Some(h) = self.handle.take() {
            self.rt.block_on(h.shutdown());
        }
        remember_enabled(false);
    }

    /// Stop because the user signed out; it starts again on the next sign-in if it was on.
    pub(super) fn suspend(&mut self) {
        if let Some(h) = self.handle.take() {
            self.rt.block_on(h.shutdown());
        }
    }

    /// Restart if running (after a settings change).
    pub(super) fn restart(&mut self) {
        if self.service.is_some() {
            self.restart_service_host();
            return;
        }
        if let Some(h) = self.handle.take() {
            self.rt.block_on(h.shutdown());
            self.start();
        }
    }

    pub(super) fn toggle(&mut self) {
        if self.running() { self.stop() } else { self.start() }
    }

    /// Stop the host before the app exits (closes the sockets and the session cleanly).
    pub(super) fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            self.rt.block_on(h.shutdown());
        }
    }

    fn register(&mut self, ctx: &egui::Context) {
        let Some(s) = Session::load() else {
            self.notify(t!("Sign in first.", "Entre na sua conta primeiro."), true);
            return;
        };
        let (name, vis) = (self.reg_name.trim().to_string(), self.reg_visibility);
        self.reg_job = Some(super::account::spawn(ctx, move || async move {
            admin::register(&s, &name, vis)
                .await
                .map(|r| tf!("Registered as {} on {}.", "Cadastrado como {} em {}.", r.name, r.server_name))
                .map_err(|e| format!("{e:#}"))
        }));
    }

    fn unregister(&mut self, ctx: &egui::Context) {
        let Some(s) = Session::load() else {
            self.notify(t!("Sign in first.", "Entre na sua conta primeiro."), true);
            return;
        };
        self.reg_job = Some(super::account::spawn(ctx, move || async move {
            admin::unregister(&s)
                .await
                .map(|_| t!("Unregistered.", "Descadastrado.").to_string())
                .map_err(|e| format!("{e:#}"))
        }));
    }

    /// Stop asking a server to add this computer.
    fn cancel_enroll(&mut self) {
        self.stop();
        if let Err(e) = rdesk_host::enroll::save(Default::default()) {
            self.notify(format!("{e:#}"), true);
        }
    }

    /// Ask `server` to add this computer (its owner approves it, or the key adds it at once).
    fn ask_to_join(&mut self, server: &str) {
        let value = if self.join_use_key { &self.join_key } else { &self.join_owner }.trim().to_string();
        let cfg = rdesk_common::config::EnrollConfig {
            server: Some(server.trim().to_string()),
            owner: (!self.join_use_key).then(|| value.clone()),
            key: self.join_use_key.then_some(value),
            name: Some(self.reg_name.trim().to_string()).filter(|n| !n.is_empty()),
        };
        match rdesk_host::enroll::save(cfg) {
            Ok(()) => {
                if let Some(h) = self.handle.take() {
                    self.rt.block_on(h.shutdown());
                }
                self.start();
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn disconnect_session(&mut self) {
        if let Some(remote) = &self.service {
            if let Err(e) = self.rt.block_on(remote.disconnect()) {
                self.notify(format!("{e:#}"), true);
            }
        } else if let Some(h) = &self.handle {
            h.disconnect();
        }
    }

    /// Save the edited options; the host restarts with them.
    fn apply_options(&mut self) {
        match admin::save_config(&self.cfg) {
            Ok(()) => {
                self.saved_cfg = self.cfg.clone();
                let was = self.running();
                self.restart();
                if !self.by_service() {
                    let text = if was {
                        t!("Saved; host restarted.", "Salvo; host reiniciado.")
                    } else {
                        t!("Saved.", "Salvo.")
                    };
                    self.notify(text, false);
                }
            }
            Err(e) => self.notify(tf!("Saving: {e:#}", "Erro ao salvar: {e:#}"), true),
        }
    }

    fn run_check(&mut self, ctx: &egui::Context) {
        self.busy = Some("diagnose");
        let tx = self.jobs.0.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let _ = tx.send(Job::Diagnose(admin::diagnose().await));
            ctx.request_repaint();
        });
    }

    fn open_add_key(&mut self) {
        self.dialog = Some(Dialog::AddKey { text: String::new(), agent: None, error: None });
        self.load_agent_keys();
    }

    /// Do what a click asked for.
    fn act(&mut self, ctx: &egui::Context, a: Act) {
        match a {
            Act::GoComputers => self.go_to = Some(super::Page::Computers),
            Act::ManageAccess => self.manage_access = self.this_machine.clone(),
            Act::AskUnregister => self.ask_unregister(ctx),
            Act::Tab(t) => self.tab = t,
            Act::Open(d) => self.dialog = Some(d),
            Act::AddKey => self.open_add_key(),
            Act::CancelEnroll => self.cancel_enroll(),
            Act::Service(install) => self.service_action(ctx, install),
            Act::ManageAsAdmin => match autostart::restart_as_admin(&self.exe) {
                Ok(()) => self.quit_requested = true,
                Err(e) => self.notify(format!("{e:#}"), true),
            },
            Act::Toggle => self.toggle(),
            Act::RunCheck => self.run_check(ctx),
            Act::Apply => self.apply_options(),
            Act::Discard => self.cfg = self.saved_cfg.clone(),
            Act::PickInbox => {
                if let Some(d) = rfd::FileDialog::new().set_directory(self.cfg.inbox_dir()).pick_folder() {
                    self.cfg.inbox_dir = Some(d);
                }
            }
        }
    }

    pub(super) fn tick(&mut self) {
        if self.service_checked.elapsed().as_secs() >= 2 {
            self.service_checked = Instant::now();
            self.check_service_mode();
            // Approval of an enrollment (or a terminal) may have registered it meanwhile.
            self.registration = Registration::load();
        }
        if let Some(rx) = &self.service_job
            && let Ok(r) = rx.try_recv()
        {
            self.service_job = None;
            match r {
                Ok(msg) => self.notify(msg, false),
                Err(e) => self.notify(e, true),
            }
            self.check_service_mode();
        }
        if let Some(rx) = &self.reg_job
            && let Ok(r) = rx.try_recv()
        {
            self.reg_job = None;
            match r {
                Ok(msg) => {
                    self.notify(msg, false);
                    self.dialog = None;
                    // The host picks the registration (relay, tickets) up when it starts.
                    self.restart();
                }
                Err(e) => {
                    if let Some(Dialog::Register { error }) = &mut self.dialog {
                        *error = Some(e);
                    } else {
                        self.notify(e, true);
                    }
                }
            }
            self.registration = Registration::load();
        }
        while let Ok(job) = self.jobs.1.try_recv() {
            self.busy = None;
            match job {
                Job::PasswordSet(Ok(())) => {
                    self.has_password = true;
                    self.dialog = None;
                    self.notify(t!("Password saved.", "Senha salva."), false);
                    // A host waiting for a first login method can start now.
                    if self.service.is_some() {
                        self.restart_service_host();
                    } else if matches!(self.status().state, HostState::Failed(_)) && remembered_enabled() {
                        self.handle = None;
                        self.start();
                    }
                }
                Job::PasswordSet(Err(e)) => {
                    if let Some(Dialog::Password { error, .. }) = &mut self.dialog {
                        *error = Some(e);
                    }
                }
                Job::OpRead(r) => {
                    if let Some(Dialog::Password { a, b, error, .. }) = &mut self.dialog {
                        match r {
                            Ok(p) => {
                                *a = p.clone();
                                *b = p;
                                *error = None;
                            }
                            Err(e) => *error = Some(e),
                        }
                    }
                }
                Job::AgentKeys(r) => {
                    if let Some(Dialog::AddKey { agent, .. }) = &mut self.dialog {
                        *agent = Some(r);
                    }
                }
                Job::Diagnose(lines) => self.diag = Some(lines),
            }
        }
    }

    // ---- what the drawing needs -----------------------------------------------

    /// "name on server" when registered (the service's registration is in its status: its
    /// files are for administrators only).
    fn registered_as(&self, st: &HostStatus) -> Option<String> {
        if self.by_service() {
            return match &st.registration {
                Some(Ok(name)) => Some(name.clone()),
                _ => None,
            };
        }
        self.registration.as_ref().map(|r| tf!("{} on {}", "{} em {}", r.name, r.server_name))
    }

    fn is_registered(&self, st: &HostStatus) -> bool {
        st.registration.is_some() || self.registration.is_some()
    }

    /// Asking a server to add it: what is happening (e.g. waiting for the owner's approval).
    fn enrolling(&self, st: &HostStatus) -> Option<String> {
        st.enrollment.clone().filter(|_| self.running() && self.registered_as(st).is_none())
    }

    /// The status card's line: its tone, the headline, and a detail.
    fn headline(&self, st: &HostStatus) -> (Tone, String, Option<String>) {
        let running = self.running();
        match &st.state {
            HostState::Failed(e) => (Tone::Danger, t!("Not running", "Não está rodando").into(), Some(e.clone())),
            _ if !running => {
                (Tone::Neutral, t!("Off: nobody can connect", "Desligado: ninguém consegue se conectar").into(), None)
            }
            _ if st.session.is_some() => {
                let who = st.session.as_ref().map(|s| s.client.clone()).unwrap_or_default();
                (Tone::Warning, tf!("In use by {who}", "Em uso por {who}"), None)
            }
            HostState::Starting if st.enrollment.is_some() => {
                (Tone::Warning, t!("Waiting for approval", "Aguardando aprovação").into(), st.enrollment.clone())
            }
            HostState::Starting => (Tone::Warning, t!("Starting…", "Iniciando…").into(), None),
            HostState::Listening(_) => {
                (Tone::Success, t!("Ready for remote access", "Pronto para acesso remoto").into(), None)
            }
            HostState::Stopped => {
                (Tone::Neutral, t!("Off: nobody can connect", "Desligado: ninguém consegue se conectar").into(), None)
            }
        }
    }

    // ---- drawing ------------------------------------------------------------

    pub(super) fn show(&mut self, ui: &mut Ui) {
        let st = self.status();
        let ctx = ui.ctx().clone();
        self.poll_gate();
        #[cfg(test)]
        let live = self.sample.is_none();
        #[cfg(not(test))]
        let live = true;
        if live {
            self.load_access();
        }
        if self.cfg != self.saved_cfg
            && let Some(a) = self.pending_bar(ui)
        {
            self.act(&ctx, a);
        }
        egui::ScrollArea::vertical().id_salt(("host", self.tab)).auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(MAX_W));
            ui::page_title(
                ui,
                t!("This computer", "Este computador"),
                Some(t!(
                    "Let others reach this computer: whether it is on, who is connected and how people get in.",
                    "Deixe outros acessarem este computador: se está ligado, quem está conectado e como entram."
                )),
            );
            ui.add_space(space::M);
            ui::tabs(
                ui,
                &mut self.tab,
                &[
                    (Tab::Overview, t!("Overview", "Visão geral")),
                    (Tab::Security, t!("Security", "Segurança")),
                    (Tab::Performance, t!("Performance and files", "Desempenho e arquivos")),
                    (Tab::Activity, t!("Activity", "Atividade")),
                    (Tab::Diagnostics, t!("Diagnostics", "Diagnóstico")),
                ],
            );
            ui.add_space(space::L);
            let act = match self.tab {
                Tab::Overview => self.overview(ui, &st),
                Tab::Security => self.security_tab(ui, &st),
                Tab::Performance => self.performance_tab(ui),
                Tab::Activity => {
                    self.activity_tab(ui, &st);
                    None
                }
                Tab::Diagnostics => self.diagnostics_tab(ui),
            };
            ui.add_space(space::XL);
            if let Some(a) = act {
                self.act(&ctx, a);
            }
        });
        self.dialogs(&ctx);
    }

    /// "Unsaved changes" with Apply and Discard, at the bottom of the page.
    fn pending_bar(&self, ui: &mut Ui) -> Option<Act> {
        let p = ui::pal(ui);
        let text = if self.running() {
            t!(
                "Unsaved changes — the host restarts when you apply them",
                "Alterações não aplicadas — o host reinicia ao aplicar"
            )
        } else {
            t!("Unsaved changes", "Alterações não aplicadas")
        };
        let mut act = None;
        egui::Panel::bottom("host-pending")
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(Margin { top: space::M as i8, ..Default::default() }))
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(MAX_W));
                egui::Frame::new()
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, ui::tint(p.warning, 170)))
                    .corner_radius(ui::RADIUS_CARD)
                    .inner_margin(Margin::symmetric(space::L as i8, space::S as i8 + 2))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui::row(
                            ui,
                            |ui| {
                                ui.label(RichText::new(icons::WARNING).size(ui::size::ICON + 2.0).color(p.warning));
                                ui.add(egui::Label::new(RichText::new(text).strong()).wrap());
                            },
                            |ui| {
                                if ui::primary(ui, t!("Apply", "Aplicar")).clicked() {
                                    act = Some(Act::Apply);
                                }
                                if ui::secondary(ui, t!("Discard", "Descartar")).clicked() {
                                    act = Some(Act::Discard);
                                }
                            },
                        );
                    });
            });
        act
    }

    // ---- Overview -------------------------------------------------------------

    fn overview(&mut self, ui: &mut Ui, st: &HostStatus) -> Option<Act> {
        let mut act = self.status_card(ui, st);
        if st.session.is_some() && self.running() {
            ui.add_space(space::M);
            act = act.or(self.session_card(ui, st));
        }
        if !self.has_password && self.keys.is_empty() && !self.is_registered(st) && !self.by_service() {
            ui.add_space(space::M);
            ui::banner(
                ui,
                Tone::Warning,
                t!(
                    "Not registered and no password or key: the host cannot start. Register it or set a password.",
                    "Não cadastrado e sem senha ou chave: o host não pode iniciar. Cadastre-o ou defina uma senha."
                ),
                None,
            );
        }
        #[cfg(target_os = "macos")]
        mac_permissions_banner(ui);
        ui.add_space(space::M);
        act = act.or(self.checklist_card(ui, st));
        ui.add_space(space::M);
        act.or(self.recent_card(ui, st))
    }

    fn status_card(&mut self, ui: &mut Ui, st: &HostStatus) -> Option<Act> {
        let p = ui::pal(ui);
        let running = self.running();
        let by_service = self.by_service();
        let (tone, line, detail) = self.headline(st);
        let fp = self.fingerprint_text();
        let registered = self.is_registered(st);
        let direct = self.saved_cfg.direct.is_on(registered);
        let mut act = None;
        let mut copied = false;
        ui::card(ui, |ui| {
            tall_row(
                ui,
                |ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 6.5, tone.color(&p));
                    ui.add(egui::Label::new(RichText::new(&line).size(ui::size::SECTION + 1.5).strong()).wrap());
                },
                |ui| {
                    if by_service {
                        ui::badge(
                            ui,
                            &ui::with_icon(icons::WINDOWS_LOGO, t!("Windows service", "Serviço do Windows")),
                            Tone::Accent,
                        )
                        .on_hover_text(t!(
                            "The rdesk service runs this computer's host: it is always on.",
                            "O serviço do rdesk executa o host deste computador: ele fica sempre ligado."
                        ));
                    } else {
                        let mut on = running;
                        let hover = if running {
                            t!("Turn the host off", "Desligar o host")
                        } else {
                            t!("Turn the host on", "Ligar o host")
                        };
                        if ui::toggle(ui, &mut on).on_hover_text(hover).changed() {
                            act = Some(Act::Toggle);
                        }
                        ui.label(
                            RichText::new(if running { t!("On", "Ligado") } else { t!("Off", "Desligado") })
                                .color(p.muted),
                        );
                    }
                },
            );
            if let Some(d) = &detail {
                let c = if tone == Tone::Danger { p.danger } else { p.muted };
                ui.add(egui::Label::new(RichText::new(d).color(c)).wrap());
            }
            ui.add_space(space::XS);
            ui.separator();
            if let Some(fp) = &fp {
                ui::row(
                    ui,
                    |ui| {
                        ui.label(RichText::new(icons::FINGERPRINT).size(ui::size::ICON).color(p.muted));
                        ui.label(RichText::new(t!("Host ID", "ID do host")).color(p.muted));
                        ui.label(RichText::new(ui::short_id(fp)).monospace()).on_hover_text(fp);
                    },
                    |ui| {
                        if ui::secondary(ui, ui::with_icon(icons::COPY, t!("Copy", "Copiar")))
                            .on_hover_text(t!(
                                "Copy the full host ID: others add this computer with it",
                                "Copia o ID do host completo: outros adicionam este computador com ele"
                            ))
                            .clicked()
                        {
                            ui.ctx().copy_text(fp.clone());
                            copied = true;
                        }
                    },
                );
            }
            match &st.state {
                HostState::Listening(addrs) if running => {
                    let port = addrs.first().map(|a| a.port()).unwrap_or(self.saved_cfg.port);
                    match &st.relay {
                        Some(Ok(url)) => {
                            reach_line(
                                ui,
                                icons::CHECK_CIRCLE,
                                p.success,
                                t!(
                                    "Through the server: reachable from anywhere",
                                    "Pelo servidor: acessível de qualquer lugar"
                                ),
                            )
                            .on_hover_text(tf!("Relay {url}", "Relay {url}"));
                        }
                        Some(Err(e)) => {
                            reach_line(
                                ui,
                                icons::WARNING,
                                p.warning,
                                &tf!(
                                    "Through the server: relay unreachable ({e})",
                                    "Pelo servidor: relay inacessível ({e})"
                                ),
                            );
                        }
                        None => {
                            reach_line(
                                ui,
                                icons::CIRCLE,
                                p.muted,
                                t!(
                                    "No relay: others need this computer's address (local network or port forwarding)",
                                    "Sem relay: outros precisam do endereço deste computador (rede local ou \
                                     redirecionamento de porta)"
                                ),
                            );
                        }
                    }
                    if direct {
                        reach_line(
                            ui,
                            icons::CHECK_CIRCLE,
                            p.success,
                            t!("Direct on the local network: on", "Direto na rede local: ligado"),
                        );
                    } else {
                        reach_line(
                            ui,
                            icons::CIRCLE,
                            p.muted,
                            t!("Direct on the local network: off", "Direto na rede local: desligado"),
                        );
                    }
                    ui::caption(
                        ui,
                        &tf!("Direct connections use UDP port {port}.", "Conexões diretas usam a porta UDP {port}."),
                    );
                }
                _ if !running && !by_service => {
                    ui::caption(
                        ui,
                        t!(
                            "Turn it on to let others reach this computer.",
                            "Ligue-o para que outros possam acessar este computador."
                        ),
                    );
                }
                _ => {}
            }
        });
        if copied {
            self.notify(t!("Host ID copied.", "ID do host copiado."), false);
        }
        act
    }

    fn session_card(&mut self, ui: &mut Ui, st: &HostStatus) -> Option<Act> {
        let s = st.session.as_ref()?;
        let p = ui::pal(ui);
        let now = unix(SystemTime::now());
        // `ago` already says "5 min ago" / "há 5 min".
        let from = tf!(
            "from {} · {} · connected {}",
            "de {} · {} · conectado {}",
            s.peer,
            s.method,
            directory::ago(unix(s.since), now)
        );
        let extras = s.extras().trim_start_matches(" · ");
        let mut act = None;
        tinted_card(ui, Tone::Warning, |ui| {
            ui::caption(ui, t!("Connected now", "Conectado agora"));
            tall_row(
                ui,
                |ui| {
                    ui::avatar(ui, &s.client, 40.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui::truncate(ui, RichText::new(&s.client).size(ui::size::SECTION).strong());
                        ui.add(egui::Label::new(RichText::new(&from).size(ui::size::SMALL).color(p.muted)).wrap());
                        if !extras.is_empty() {
                            let tone = if s.admin_terminals > 0 { Tone::Warning } else { Tone::Neutral };
                            ui::badge(ui, &ui::with_icon(icons::TERMINAL_WINDOW, extras), tone);
                        }
                    });
                },
                |ui| {
                    if ui::danger(ui, t!("Disconnect", "Desconectar"))
                        .on_hover_text(t!(
                            "End it; the other side will not reconnect",
                            "Encerra a sessão; o outro lado não vai reconectar"
                        ))
                        .clicked()
                    {
                        act = Some(Act::Open(Dialog::Disconnect(s.client.clone())));
                    }
                },
            );
        });
        act
    }

    fn checklist_card(&mut self, ui: &mut Ui, st: &HostStatus) -> Option<Act> {
        let running = self.running();
        let reg_busy = self.reg_job.is_some();
        let registered = self.registered_as(st);
        let enrolling = self.enrolling(st);
        let by_service = self.by_service();
        let elevated = self.elevated;
        let service_busy = self.service_job.is_some();
        let has_pw = self.has_password;
        let n_keys = self.keys.len();
        let is_reg = self.is_registered(st);
        let direct = self.saved_cfg.direct.is_on(is_reg);
        let this = self.this_machine.clone();
        let access = self.access_of_this().cloned();
        // Unknown (not listed yet, another server): as before, the server decides.
        let can_unregister = this.as_ref().is_none_or(|m| m.is_owner || m.here.is_some());
        let gate_busy = self.gate.busy();
        let mut act = None;
        ui::card(ui, |ui| {
            ui::section(ui, t!("Access setup", "Configuração do acesso"));
            ui.add_space(space::XS);

            // Registration with the server.
            match (&registered, &enrolling) {
                (Some(name), _) => {
                    let (mark_, status) = match &st.registration {
                        Some(Ok(_)) if running => {
                            (Mark::Done, t!("In touch with the server.", "Em contato com o servidor.").to_string())
                        }
                        Some(Err(e)) if running => (Mark::Warn, e.clone()),
                        _ => (
                            Mark::Done,
                            t!(
                                "The host gets in touch with the server when it is on.",
                                "O host entra em contato com o servidor quando está ligado."
                            )
                            .to_string(),
                        ),
                    };
                    let left = tall_row(
                        ui,
                        |ui| {
                            mark(ui, mark_);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(tf!("Registered as {name}", "Cadastrado como {name}")).strong(),
                                    )
                                    .wrap(),
                                );
                                ui::caption(ui, &status);
                                who_here(ui, this.as_ref(), access.as_ref())
                            })
                            .inner
                        },
                        |ui| {
                            if reg_busy || gate_busy {
                                ui.spinner();
                            } else if can_unregister && ui::secondary(ui, t!("Unregister…", "Descadastrar…")).clicked()
                            {
                                act = Some(Act::AskUnregister);
                            }
                        },
                    );
                    match left {
                        WhoClick::Nothing => {}
                        WhoClick::Computers => act = Some(Act::GoComputers),
                        WhoClick::Manage => act = Some(Act::ManageAccess),
                    }
                }
                (None, Some(msg)) => {
                    tall_row(
                        ui,
                        |ui| {
                            mark(ui, Mark::Busy);
                            title_detail(
                                ui,
                                t!("Waiting for approval", "Aguardando aprovação"),
                                Some(&format!(
                                    "{msg} {}",
                                    t!(
                                        "Keep rdesk running (it can stay in the tray) until it is approved.",
                                        "Deixe o rdesk aberto (ele pode ficar na bandeja) até ser aprovado."
                                    )
                                )),
                            );
                        },
                        |ui| {
                            if ui::secondary(ui, t!("Cancel", "Cancelar")).clicked() {
                                act = Some(Act::CancelEnroll);
                            }
                        },
                    );
                }
                (None, None) => {
                    tall_row(
                        ui,
                        |ui| {
                            mark(ui, Mark::Todo);
                            title_detail(
                                ui,
                                t!("Not registered on the server", "Não cadastrado no servidor"),
                                Some(t!(
                                    "Others reach it only on the local network, by address. Register it to reach it \
                                     from anywhere, with access controlled by the server.",
                                    "Outros só o acessam na rede local, pelo endereço. Cadastre-o para acessá-lo de \
                                     qualquer lugar, com o acesso controlado pelo servidor."
                                )),
                            );
                        },
                        |ui| {
                            if reg_busy {
                                ui.spinner();
                            } else if ui::primary(ui, t!("Register…", "Cadastrar…")).clicked() {
                                act = Some(Act::Open(Dialog::Register { error: None }));
                            }
                        },
                    );
                }
            }

            // The Windows service.
            if cfg!(windows) {
                ui.separator();
                if by_service {
                    let detail = if elevated {
                        t!(
                            "Reachable before sign-in, on the lock screen and through UAC prompts.",
                            "Acessível antes do login, na tela de bloqueio e nos avisos do UAC."
                        )
                    } else {
                        t!(
                            "Reachable before sign-in, on the lock screen and through UAC prompts. Changing this \
                             computer's settings needs administrator rights.",
                            "Acessível antes do login, na tela de bloqueio e nos avisos do UAC. Alterar as \
                             configurações deste computador exige direitos de administrador."
                        )
                    };
                    let manage = tall_row(
                        ui,
                        |ui| {
                            mark(ui, Mark::Done);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                title_detail(
                                    ui,
                                    t!("Installed as a Windows service", "Instalado como serviço do Windows"),
                                    Some(detail),
                                );
                                !elevated
                                    && ui::secondary(
                                        ui,
                                        ui::with_icon(
                                            icons::SHIELD_CHECK,
                                            t!("Manage as administrator", "Gerenciar como administrador"),
                                        ),
                                    )
                                    .clicked()
                            })
                            .inner
                        },
                        |ui| {
                            if service_busy {
                                ui.spinner();
                            } else if ui::secondary(ui, t!("Uninstall…", "Desinstalar…")).clicked() {
                                act = Some(Act::Open(Dialog::UninstallService));
                            }
                        },
                    );
                    if manage {
                        act = Some(Act::ManageAsAdmin);
                    }
                } else {
                    tall_row(
                        ui,
                        |ui| {
                            mark(ui, Mark::Warn);
                            let detail = if service_busy {
                                t!(
                                    "Windows may ask for administrator permission…",
                                    "O Windows pode pedir permissão de administrador…"
                                )
                            } else {
                                t!(
                                    "Run by this app: not reachable before you sign in, and the lock screen and UAC \
                                     prompts cannot be controlled. Installing asks for administrator permission once.",
                                    "Executado por este app: inacessível antes do login, e a tela de bloqueio e os \
                                     avisos do UAC não podem ser controlados. Instalar pede permissão de \
                                     administrador uma vez."
                                )
                            };
                            title_detail(
                                ui,
                                t!("Not installed as a Windows service", "Não instalado como serviço do Windows"),
                                Some(detail),
                            );
                        },
                        |ui| {
                            if service_busy {
                                ui.spinner();
                            } else if ui::secondary(ui, ui::with_icon(icons::SHIELD_CHECK, t!("Install", "Instalar")))
                                .on_hover_text(t!(
                                    "Install rdesk on this computer (recommended)",
                                    "Instalar o rdesk neste computador (recomendado)"
                                ))
                                .clicked()
                            {
                                act = Some(Act::Service(true));
                            }
                        },
                    );
                }
            }

            // Own password.
            ui.separator();
            tall_row(
                ui,
                |ui| {
                    mark(ui, if has_pw { Mark::Done } else { Mark::Todo });
                    title_detail(
                        ui,
                        if has_pw {
                            t!("Own password: set", "Senha própria: definida")
                        } else {
                            t!("Own password (optional)", "Senha própria (opcional)")
                        },
                        Some(t!(
                            "Opens this computer on the local network without the server, or as the extra lock.",
                            "Abre este computador na rede local sem o servidor, ou como cadeado extra."
                        )),
                    );
                },
                |ui| {
                    let label = if has_pw { t!("Change…", "Alterar…") } else { t!("Set…", "Definir…") };
                    if ui::secondary(ui, label).clicked() {
                        act = Some(Act::Open(password_dialog()));
                    }
                },
            );

            // SSH keys.
            ui.separator();
            tall_row(
                ui,
                |ui| {
                    mark(ui, if n_keys > 0 { Mark::Done } else { Mark::Todo });
                    let title = match n_keys {
                        0 => t!("No SSH keys (optional)", "Nenhuma chave SSH (opcional)").to_string(),
                        1 => t!("1 SSH key authorized", "1 chave SSH autorizada").to_string(),
                        n => tf!("{n} SSH keys authorized", "{n} chaves SSH autorizadas"),
                    };
                    title_detail(
                        ui,
                        &title,
                        Some(t!(
                            "Whoever holds the private key can log in, like with the password.",
                            "Quem tiver a chave privada pode entrar, como com a senha."
                        )),
                    );
                },
                |ui| {
                    if n_keys > 0 {
                        if ui::secondary(ui, t!("Manage", "Gerenciar")).clicked() {
                            act = Some(Act::Tab(Tab::Security));
                        }
                    } else if ui::secondary(ui, t!("Add…", "Adicionar…")).clicked() {
                        act = Some(Act::AddKey);
                    }
                },
            );

            // Direct access.
            ui.separator();
            tall_row(
                ui,
                |ui| {
                    let (m, title, detail) = match (direct, is_reg) {
                        (true, true) => (
                            Mark::Warn,
                            t!("Direct access is on", "Acesso direto ligado"),
                            t!(
                                "A second way in besides the server's sharing: this computer's own password or key \
                                 open it on the local network.",
                                "Um segundo acesso além do compartilhamento do servidor: a senha ou chave própria \
                                 deste computador o abrem na rede local."
                            ),
                        ),
                        (true, false) => (
                            Mark::Done,
                            t!("Direct access on the local network", "Acesso direto na rede local"),
                            t!(
                                "This computer's own password or SSH key open it without the server.",
                                "A senha ou chave SSH própria deste computador o abrem sem o servidor."
                            ),
                        ),
                        (false, true) => (
                            Mark::Done,
                            t!("Direct access off", "Acesso direto desligado"),
                            t!(
                                "Only the server's sharing lets people in.",
                                "Só o compartilhamento do servidor dá acesso."
                            ),
                        ),
                        (false, false) => (
                            Mark::Warn,
                            t!("Direct access off", "Acesso direto desligado"),
                            t!(
                                "Not registered and direct access off: nobody can reach it.",
                                "Não cadastrado e acesso direto desligado: ninguém consegue acessá-lo."
                            ),
                        ),
                    };
                    mark(ui, m);
                    title_detail(ui, title, Some(detail));
                },
                |ui| {
                    if ui::secondary(ui, t!("Manage", "Gerenciar")).clicked() {
                        act = Some(Act::Tab(Tab::Security));
                    }
                },
            );
        });
        act
    }

    fn recent_card(&mut self, ui: &mut Ui, st: &HostStatus) -> Option<Act> {
        let mut act = None;
        let now = unix(SystemTime::now());
        ui::card(ui, |ui| {
            ui::row(
                ui,
                |ui| ui::section(ui, t!("Recent activity", "Atividade recente")),
                |ui| {
                    if !st.events.is_empty() && ui::link(ui, t!("See all", "Ver tudo")).clicked() {
                        act = Some(Act::Tab(Tab::Activity));
                    }
                },
            );
            ui.add_space(space::XS);
            if st.events.is_empty() {
                ui::caption(ui, t!("Nothing yet.", "Nada ainda."));
            }
            for e in st.events.iter().rev().take(5) {
                event_line(ui, e, now);
            }
        });
        act
    }

    // ---- Security ---------------------------------------------------------------

    /// When the service runs the host and this app is not elevated, settings cannot be saved.
    fn admin_banner(&self, ui: &mut Ui) -> Option<Act> {
        if !self.by_service() || self.elevated {
            return None;
        }
        let clicked = ui::banner(
            ui,
            Tone::Warning,
            t!(
                "Changing this computer's settings needs administrator rights.",
                "Alterar as configurações deste computador exige direitos de administrador."
            ),
            Some(t!("Manage as administrator", "Gerenciar como administrador")),
        );
        ui.add_space(space::M);
        clicked.then_some(Act::ManageAsAdmin)
    }

    fn security_tab(&mut self, ui: &mut Ui, st: &HostStatus) -> Option<Act> {
        let p = ui::pal(ui);
        let mut act = self.admin_banner(ui);
        let has_pw = self.has_password;

        // Password.
        ui::card(ui, |ui| {
            tall_row(
                ui,
                |ui| {
                    ui.label(RichText::new(icons::PASSWORD).size(ui::size::ICON + 4.0).color(p.muted));
                    title_detail(
                        ui,
                        t!("Password of this computer", "Senha deste computador"),
                        Some(if has_pw { t!("Set.", "Definida.") } else { t!("Not set.", "Não definida.") }),
                    );
                },
                |ui| {
                    if has_pw {
                        if ui::danger(ui, t!("Remove…", "Remover…")).clicked() {
                            act = Some(Act::Open(Dialog::RemovePassword));
                        }
                        if ui::secondary(ui, t!("Change…", "Alterar…")).clicked() {
                            act = Some(Act::Open(password_dialog()));
                        }
                    } else if ui::secondary(ui, t!("Set password…", "Definir senha…")).clicked() {
                        act = Some(Act::Open(password_dialog()));
                    }
                },
            );
            ui::caption(
                ui,
                t!(
                    "Used on the local network without the server, or as the extra lock after the server's permission.",
                    "Usada na rede local sem o servidor, ou como cadeado extra após a permissão do servidor."
                ),
            );
        });
        ui.add_space(space::M);

        // SSH keys.
        let keys = &self.keys;
        ui::card(ui, |ui| {
            ui::section(ui, t!("SSH keys", "Chaves SSH"));
            ui::caption(
                ui,
                t!(
                    "Whoever holds one of these private keys (e.g. in their 1Password) can log in to this computer.",
                    "Quem tiver uma destas chaves privadas (ex.: no 1Password) pode entrar neste computador."
                ),
            );
            ui.add_space(space::XS);
            if keys.is_empty() {
                ui::caption(ui, t!("No keys yet.", "Nenhuma chave ainda."));
            }
            for k in keys {
                let fp = agent::key_fingerprint(k.key_data());
                let name = key_name(k);
                ui.separator();
                tall_row(
                    ui,
                    |ui| {
                        ui.label(RichText::new(icons::KEY).size(ui::size::ICON + 2.0).color(p.muted));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui::truncate(ui, RichText::new(&name).strong());
                            ui.add(
                                egui::Label::new(RichText::new(&fp).monospace().size(ui::size::SMALL).color(p.muted))
                                    .truncate(),
                            );
                        });
                    },
                    |ui| {
                        if ui::icon_button_colored(
                            ui,
                            icons::TRASH,
                            t!("Remove this key", "Remover esta chave"),
                            p.danger,
                        )
                        .clicked()
                        {
                            act = Some(Act::Open(Dialog::RemoveKey { fp: fp.clone(), name: name.clone() }));
                        }
                    },
                );
            }
            ui.add_space(space::S);
            if ui::secondary(ui, ui::with_icon(icons::PLUS, t!("Add SSH key…", "Adicionar chave SSH…"))).clicked() {
                act = Some(Act::AddKey);
            }
        });
        ui.add_space(space::M);

        // Extra lock.
        let no_login = !self.has_password && self.keys.is_empty();
        let cfg = &mut self.cfg;
        ui::card(ui, |ui| {
            tall_row(
                ui,
                |ui| {
                    ui.label(RichText::new(icons::LOCK_KEY).size(ui::size::ICON + 4.0).color(p.muted));
                    title_detail(
                        ui,
                        t!("Extra lock", "Cadeado extra"),
                        Some(t!(
                            "Besides the server's permission, also ask for this computer's password or SSH key.",
                            "Além da permissão do servidor, pedir também a senha ou chave SSH deste computador."
                        )),
                    );
                },
                |ui| {
                    ui::toggle(ui, &mut cfg.require_local);
                },
            );
            if cfg.require_local && no_login {
                warn_line(
                    ui,
                    t!(
                        "Set a password or add an SSH key above, or nobody will get in.",
                        "Defina uma senha ou adicione uma chave SSH acima, ou ninguém vai conseguir entrar."
                    ),
                );
            }
        });
        ui.add_space(space::M);

        // Direct access.
        let registered = st.registration.is_some() || self.registration.is_some();
        let cfg = &mut self.cfg;
        ui::card(ui, |ui| {
            let mut on = cfg.direct.is_on(registered);
            let was = on;
            tall_row(
                ui,
                |ui| {
                    ui.label(RichText::new(icons::WIFI_HIGH).size(ui::size::ICON + 4.0).color(p.muted));
                    title_detail(
                        ui,
                        t!("Direct access", "Acesso direto"),
                        Some(t!(
                            "This computer's own password or SSH key open it without the server, only from nearby \
                             networks (never through the relay).",
                            "A senha ou chave SSH própria deste computador o abrem sem o servidor, só de redes \
                             próximas (nunca pelo relay)."
                        )),
                    );
                },
                |ui| {
                    ui::toggle(ui, &mut on);
                },
            );
            if on != was {
                // Back to the default when it matches it (on only while unregistered).
                cfg.direct.enabled = (on == registered).then_some(on);
            }
            if on {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.label(t!("Allow connections from", "Permitir conexões de"));
                    // The class ids go to the config; only the labels are translated.
                    let classes = [("lan", t!("local network", "rede local")), ("tailscale", "Tailscale")];
                    for (class, label) in classes {
                        let mut has = cfg.direct.from.iter().any(|c| c == class);
                        if ui.checkbox(&mut has, label).changed() {
                            cfg.direct.from.retain(|c| c != class);
                            if has {
                                cfg.direct.from.push(class.to_string());
                            }
                        }
                    }
                });
                ui.separator();
                tall_row(
                    ui,
                    |ui| {
                        title_detail(
                            ui,
                            t!("Announce it on the local network (mDNS)", "Anunciá-lo na rede local (mDNS)"),
                            Some(t!(
                                "Others connect with the host ID alone; anyone on the network sees it is here.",
                                "Outros se conectam só com o ID do host; qualquer um na rede vê que ele está aqui."
                            )),
                        );
                    },
                    |ui| {
                        ui::toggle(ui, &mut cfg.direct.mdns);
                    },
                );
                if registered {
                    ui.add_space(space::XS);
                    ui::banner(
                        ui,
                        Tone::Warning,
                        t!(
                            "Registered: this is a second way in, besides the server's sharing. The owner sees it is \
                             on, and these sessions appear in the history.",
                            "Cadastrado: este é um segundo acesso, além do compartilhamento do servidor. O dono vê \
                             que está ligado, e essas sessões aparecem no histórico."
                        ),
                        None,
                    );
                }
            } else if !registered {
                ui.add_space(space::XS);
                ui::banner(
                    ui,
                    Tone::Warning,
                    t!(
                        "Not registered and direct access off: nobody can reach it.",
                        "Não cadastrado e acesso direto desligado: ninguém consegue acessá-lo."
                    ),
                    None,
                );
            }
        });
        ui.add_space(space::M);

        // Asking the person at the computer.
        ui::card(ui, |ui| {
            tall_row(
                ui,
                |ui| {
                    title_detail(
                        ui,
                        t!("Ask before each session", "Pedir permissão antes de cada sessão"),
                        Some(t!(
                            "Whoever is at this computer allows or denies each connection. Not asked when it is                              locked or nobody is signed in, nor for a reconnect.",
                            "Quem estiver neste computador permite ou recusa cada conexão. Não pergunta com ele                              bloqueado ou sem ninguém conectado, nem numa reconexão."
                        )),
                    );
                },
                |ui| {
                    ui::toggle(ui, &mut cfg.consent.ask);
                },
            );
            if cfg.consent.ask {
                ui.separator();
                ui.add_space(space::XS);
                title_detail(ui, t!("Wait for an answer", "Esperar a resposta por"), None);
                ui.add(egui::Slider::new(&mut cfg.consent.timeout_s, 10..=120).suffix(" s"));
                ui.add_space(space::XS);
                ui.separator();
                tall_row(
                    ui,
                    |ui| {
                        title_detail(
                            ui,
                            t!("Without an answer, allow", "Sem resposta, permitir"),
                            Some(t!(
                                "Nobody there to answer: let the session in (off: refuse it).",
                                "Ninguém lá para responder: deixa a sessão entrar (desligado: recusa)."
                            )),
                        );
                    },
                    |ui| {
                        ui::toggle(ui, &mut cfg.consent.allow_on_timeout);
                    },
                );
            }
        });
        ui.add_space(space::M);

        // New identity.
        ui::card(ui, |ui| {
            tall_row(
                ui,
                |ui| {
                    ui.label(RichText::new(icons::FINGERPRINT).size(ui::size::ICON + 4.0).color(p.danger));
                    title_detail(
                        ui,
                        t!("New host identity", "Nova identidade do host"),
                        Some(t!(
                            "This computer gets a new host ID; others must be given the new one.",
                            "Este computador ganha um novo ID do host; os outros precisam receber o novo."
                        )),
                    );
                },
                |ui| {
                    if ui::danger(ui, t!("Create new identity…", "Criar nova identidade…")).clicked() {
                        act = Some(Act::Open(Dialog::NewIdentity));
                    }
                },
            );
        });
        act
    }

    // ---- Performance and files -----------------------------------------------------

    fn performance_tab(&mut self, ui: &mut Ui) -> Option<Act> {
        let p = ui::pal(ui);
        let mut act = self.admin_banner(ui);
        let inbox = self.cfg.inbox_dir().display().to_string();
        let cfg = &mut self.cfg;
        ui::card(ui, |ui| {
            tall_row(
                ui,
                |ui| {
                    title_detail(
                        ui,
                        t!("UDP port", "Porta UDP"),
                        Some(t!(
                            "For direct connections (local network, port forwarding).",
                            "Para conexões diretas (rede local, redirecionamento de porta)."
                        )),
                    );
                },
                |ui| {
                    ui.add(egui::DragValue::new(&mut cfg.port).range(1..=65535));
                },
            );
            ui.separator();
            tall_row(
                ui,
                |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.add(
                            egui::Label::new(RichText::new(t!("Received files", "Arquivos recebidos")).strong()).wrap(),
                        );
                        ui::truncate(ui, RichText::new(&inbox).size(ui::size::SMALL).color(p.muted));
                    });
                },
                |ui| {
                    if ui::secondary(ui, ui::with_icon(icons::FOLDER_OPEN, t!("Change…", "Alterar…"))).clicked() {
                        act = Some(Act::PickInbox);
                    }
                },
            );
            ui.separator();
            ui.add_space(space::XS);
            title_detail(
                ui,
                t!("Max frame rate", "Taxa de quadros máxima"),
                Some(t!(
                    "Higher is smoother, and uses more of the network.",
                    "Mais alta é mais fluida, e usa mais da rede."
                )),
            );
            ui.add(egui::Slider::new(&mut cfg.max_fps, 5..=60).suffix(" fps"));
            ui.add_space(space::XS);
            ui.separator();
            ui.add_space(space::XS);
            title_detail(
                ui,
                t!("Image quality in motion", "Qualidade da imagem em movimento"),
                Some(t!(
                    "Lower is faster on slow connections; a still screen is always sharp.",
                    "Mais baixa é mais rápida em conexões lentas; a tela parada fica sempre nítida."
                )),
            );
            ui.add(egui::Slider::new(&mut cfg.jpeg_quality, 30..=95));
            ui.add_space(space::XS);
            ui.separator();
            tall_row(
                ui,
                |ui| {
                    title_detail(
                        ui,
                        t!("Clipboard", "Área de transferência"),
                        Some(t!(
                            "Share text and images in both directions.",
                            "Compartilhar textos e imagens nos dois sentidos."
                        )),
                    );
                },
                |ui| {
                    ui::toggle(ui, &mut cfg.clipboard);
                },
            );
            // Administrator rights in a terminal exist only on Windows.
            if cfg!(windows) {
                ui.separator();
                tall_row(
                    ui,
                    |ui| {
                        title_detail(
                            ui,
                            t!("Terminal as administrator", "Terminal como administrador"),
                            Some(t!(
                                "Full control may open it with the signed-in person's administrator rights (with \
                                 rdesk installed).",
                                "Controle total pode abri-lo com os direitos de administrador de quem estiver logado \
                                 (com o rdesk instalado)."
                            )),
                        );
                    },
                    |ui| {
                        ui::toggle(ui, &mut cfg.admin_terminal);
                    },
                );
            }
        });
        act
    }

    // ---- Activity -------------------------------------------------------------------

    fn activity_tab(&mut self, ui: &mut Ui, st: &HostStatus) {
        if st.events.is_empty() {
            ui::empty_state(
                ui,
                icons::CLOCK_COUNTER_CLOCKWISE,
                t!("Nothing yet", "Nada ainda"),
                t!(
                    "Connections, logins and problems of this computer's host show up here.",
                    "Conexões, entradas e problemas do host deste computador aparecem aqui."
                ),
            );
            return;
        }
        let now = unix(SystemTime::now());
        ui::card(ui, |ui| {
            for (i, e) in st.events.iter().rev().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                event_line(ui, e, now);
            }
        });
    }

    // ---- Diagnostics -----------------------------------------------------------------

    fn diagnostics_tab(&mut self, ui: &mut Ui) -> Option<Act> {
        let p = ui::pal(ui);
        let busy = self.busy == Some("diagnose");
        let ran = self.diag.is_some();
        let mut act = None;
        ui::card(ui, |ui| {
            tall_row(
                ui,
                |ui| {
                    ui.label(RichText::new(icons::STETHOSCOPE).size(ui::size::ICON + 4.0).color(p.muted));
                    title_detail(
                        ui,
                        t!("Check this computer", "Verificar este computador"),
                        Some(t!(
                            "Looks at the host's files, how people log in, the server, the relay and the screens.",
                            "Confere os arquivos do host, as formas de entrar, o servidor, o relay e as telas."
                        )),
                    );
                },
                |ui| {
                    if busy {
                        ui.spinner();
                    } else {
                        let label =
                            if ran { t!("Check again", "Verificar de novo") } else { t!("Run check", "Verificar") };
                        if ui::secondary(ui, label).clicked() {
                            act = Some(Act::RunCheck);
                        }
                    }
                },
            );
            if let Some(lines) = &self.diag {
                ui.separator();
                egui::Grid::new("host-diag").num_columns(2).spacing([space::L, space::S]).show(ui, |ui| {
                    for l in lines {
                        ui.horizontal(|ui| {
                            let (icon, c) =
                                if l.ok { (icons::CHECK_CIRCLE, p.success) } else { (icons::WARNING, p.warning) };
                            ui.label(RichText::new(icon).size(ui::size::ICON).color(c));
                            ui.label(RichText::new(&l.label).strong());
                        });
                        let c = if l.ok { p.text } else { p.warning };
                        ui.add(egui::Label::new(RichText::new(&l.value).color(c)).wrap());
                        ui.end_row();
                    }
                });
            }
        });
        act
    }

    // ---- sign-in screen ---------------------------------------------------------------

    /// On the sign-in screen, inside a card: make this computer reachable without signing in
    /// (it asks the server to add it; its owner approves it, or an enrollment key adds it at once).
    pub(super) fn join_section(&mut self, ui: &mut Ui, server: &str) {
        let p = ui::pal(ui);
        let st = self.status();
        let running = self.running();
        let dot_color = if running { p.success } else { p.muted };
        if self.by_service() {
            ui::section(ui, t!("This computer", "Este computador"));
            ui.horizontal(|ui| {
                ui::dot(ui, dot_color);
                ui.add(
                    egui::Label::new(tf!(
                        "Run by the rdesk service · host {}.",
                        "Executado pelo serviço do rdesk · host {}.",
                        self.summary()
                    ))
                    .wrap(),
                );
            });
            // Asking a server to add it: the code its owner compares before approving.
            if let Some(msg) = st.enrollment.as_ref().filter(|_| running) {
                ui.add_space(space::S);
                ui::banner(ui, Tone::Warning, msg, None);
                ui::caption(
                    ui,
                    t!(
                        "Tell the owner this code: they approve this computer in their rdesk app.",
                        "Passe este código ao dono: ele aprova este computador no app rdesk dele."
                    ),
                );
            }
            return;
        }
        if let Some(r) = &self.registration {
            ui::section(ui, t!("This computer", "Este computador"));
            ui.horizontal(|ui| {
                ui::dot(ui, dot_color);
                ui.add(
                    egui::Label::new(tf!(
                        "Reachable as {} on {} · host {}.",
                        "Acessível como {} em {} · host {}.",
                        r.name,
                        r.server_name,
                        self.summary()
                    ))
                    .wrap(),
                );
            });
            ui.add_space(space::XS);
            let label = if running {
                t!("Turn the host off", "Desligar o host")
            } else {
                t!("Turn the host on", "Ligar o host")
            };
            if ui::secondary(ui, ui::with_icon(icons::POWER, label)).clicked() {
                self.toggle();
            }
            return;
        }
        ui::section(ui, t!("Only share this computer", "Só compartilhar este computador"));
        if let Some(msg) = self.enrolling(&st) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add(egui::Label::new(msg).wrap());
            });
            ui::caption(
                ui,
                t!(
                    "Keep rdesk running (it can stay in the tray) until it is approved.",
                    "Deixe o rdesk aberto (ele pode ficar na bandeja) até ser aprovado."
                ),
            );
            ui.add_space(space::XS);
            if ui::secondary(ui, t!("Cancel", "Cancelar")).clicked() {
                self.cancel_enroll();
            }
            return;
        }
        ui::caption(
            ui,
            t!(
                "For a computer that is only accessed, without signing in: its owner approves it in their rdesk app.",
                "Para um computador que só é acessado, sem entrar: o dono o aprova no app rdesk dele."
            ),
        );
        if let HostState::Failed(e) = &st.state {
            ui.add(egui::Label::new(RichText::new(e).size(ui::size::SMALL).color(p.danger)).wrap());
        }
        ui.add_space(space::S);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = space::L;
            ui.radio_value(&mut self.join_use_key, false, t!("Owner's e-mail", "E-mail do dono"));
            ui.radio_value(&mut self.join_use_key, true, t!("Enrollment key", "Chave de cadastro"));
        });
        let (label, value, hint) = if self.join_use_key {
            (
                t!("Enrollment key", "Chave de cadastro"),
                &mut self.join_key,
                t!("rdk_… (from Computers, Add a computer)", "rdk_… (em Computadores, Adicionar um computador)"),
            )
        } else {
            (
                t!("Owner's e-mail", "E-mail do dono"),
                &mut self.join_owner,
                t!("who will own it, e.g. ana@gmail.com", "quem será o dono, ex.: ana@gmail.com"),
            )
        };
        ui.label(RichText::new(label).size(ui::size::SMALL).color(p.muted));
        let r1 = ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY));
        let filled = !value.trim().is_empty();
        ui.label(
            RichText::new(t!("Name of this computer", "Nome deste computador")).size(ui::size::SMALL).color(p.muted),
        );
        let r2 = ui.add(egui::TextEdit::singleline(&mut self.reg_name).desired_width(f32::INFINITY));
        let ready = !server.trim().is_empty() && filled;
        let enter = (r1.lost_focus() || r2.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(space::XS);
        let ask = ui.add_enabled_ui(ready, |ui| ui::secondary(ui, t!("Ask to join", "Pedir para entrar"))).inner;
        if server.trim().is_empty() {
            ui::caption(ui, t!("Type the server address above first.", "Digite antes o endereço do servidor acima."));
        } else {
            ui::caption(
                ui,
                t!("The server address is the one typed above.", "O endereço do servidor é o digitado acima."),
            );
        }
        if ready && (ask.clicked() || enter) {
            self.ask_to_join(server);
        }
    }

    // ---- dialogs ---------------------------------------------------------------------

    fn load_agent_keys(&self) {
        let tx = self.jobs.0.clone();
        self.rt.spawn(async move {
            let r = async {
                let mut a = Agent::connect(None).await?;
                a.identities().await
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Job::AgentKeys(r));
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        let cancel = t!("Cancel", "Cancelar");
        match &mut dialog {
            Dialog::Password { a, b, op_ref, shown, error } => {
                let busy = self.busy == Some("password");
                let mut load = false;
                let d = ui::dialog_with_footer(
                    ctx,
                    "host-password",
                    t!("Password for this computer", "Senha deste computador"),
                    480.0,
                    |ui| {
                        let p = ui::pal(ui);
                        ui::paragraph(
                            ui,
                            t!(
                                "People who know it can control this computer.",
                                "Quem souber a senha pode controlar este computador."
                            ),
                        );
                        ui::caption(
                            ui,
                            t!(
                                "Only a derived record is stored (OPAQUE + Argon2id); the password itself never leaves the client.",
                                "Só um registro derivado é guardado (OPAQUE + Argon2id); a senha em si nunca sai do cliente."
                            ),
                        );
                        ui.add_space(space::S);
                        ui.label(t!("Password", "Senha"));
                        ui.add(
                            egui::TextEdit::singleline(&mut **a).password(shown.is_none()).desired_width(f32::INFINITY),
                        );
                        ui.label(t!("Repeat", "Repetir"));
                        ui.add(
                            egui::TextEdit::singleline(&mut **b).password(shown.is_none()).desired_width(f32::INFINITY),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if ui::secondary(ui, t!("Generate a strong one", "Gerar uma forte")).clicked() {
                                let pw = admin::generate_password(24);
                                **a = pw.clone();
                                **b = pw.clone();
                                *shown = Some(pw);
                            }
                            if let Some(pw) = shown {
                                if ui::secondary(ui, ui::with_icon(icons::COPY, t!("Copy", "Copiar"))).clicked() {
                                    ui.ctx().copy_text(pw.clone());
                                }
                                ui::caption(ui, t!("Save it in 1Password.", "Salve-a no 1Password."));
                            }
                        });
                        ui.add_space(space::XS);
                        ui.label(t!("Or read from 1Password:", "Ou leia do 1Password:"));
                        let can_load = !op_ref.trim().is_empty();
                        ui::row(
                            ui,
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(op_ref)
                                        .hint_text("op://Private/rdesk/password")
                                        .desired_width(ui.available_width()),
                                );
                            },
                            |ui| {
                                if ui
                                    .add_enabled_ui(can_load, |ui| ui::secondary(ui, t!("Load", "Carregar")))
                                    .inner
                                    .clicked()
                                {
                                    load = true;
                                }
                            },
                        );
                        if !a.is_empty() && a.chars().count() < 16 {
                            warn_line(
                                ui,
                                t!(
                                    "Short password: prefer 16+ characters (or Generate).",
                                    "Senha curta: prefira 16+ caracteres (ou gere uma forte)."
                                ),
                            );
                        }
                        if !a.is_empty() && a != b {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(t!("The two passwords differ.", "As duas senhas são diferentes."))
                                        .color(p.danger),
                                )
                                .wrap(),
                            );
                        }
                        if let Some(e) = error {
                            ui.add(egui::Label::new(RichText::new(e.as_str()).color(p.danger)).wrap());
                        }
                        if busy {
                            ui::busy(ui, t!("Deriving (Argon2id)…", "Derivando (Argon2id)…"));
                        }
                        ui.input(|i| i.key_pressed(egui::Key::Enter))
                    },
                    |ui| ui::dialog_buttons(ui, t!("Save password", "Salvar senha"), false, &[cancel]),
                );
                if load {
                    let (tx, r) = (self.jobs.0.clone(), op_ref.trim().to_string());
                    self.rt.spawn(async move {
                        let res = agent::op_read(&r).await.map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Job::OpRead(res));
                    });
                }
                let (enter, button) = d.inner;
                let ready = !a.is_empty() && a == b && self.busy.is_none();
                match button {
                    Some(ui::DialogButton::Primary) | None if ready && (enter || button.is_some()) => {
                        self.busy = Some("password");
                        let (tx, pw) = (self.jobs.0.clone(), a.clone());
                        let ctx = ctx.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(Job::PasswordSet(admin::set_password(&pw).map_err(|e| format!("{e:#}"))));
                            ctx.request_repaint();
                        });
                    }
                    Some(ui::DialogButton::Other(_)) if !busy => keep = false,
                    _ => {}
                }
                if d.closed && !busy {
                    keep = false;
                }
            }
            Dialog::AddKey { text, agent: agent_keys, error } => {
                let mut added: Option<PublicKey> = None;
                let keys = &self.keys;
                let d = ui::dialog_with_footer(
                    ctx,
                    "host-addkey",
                    t!("Add an SSH key", "Adicionar uma chave SSH"),
                    560.0,
                    |ui| {
                        let p = ui::pal(ui);
                        ui::paragraph(
                            ui,
                            t!(
                                "Whoever holds the private key (e.g. in their 1Password) can log in to this computer.",
                                "Quem tiver a chave privada (ex.: no 1Password) pode entrar neste computador."
                            ),
                        );
                        ui.add_space(space::S);
                        ui.add(
                            egui::Label::new(
                                RichText::new(t!(
                                    "From the SSH agent on this computer (1Password)",
                                    "Do agente SSH deste computador (1Password)"
                                ))
                                .strong(),
                            )
                            .wrap(),
                        );
                        match agent_keys {
                            None => ui::busy(ui, t!("Looking for keys…", "Procurando chaves…")),
                            Some(Err(e)) => {
                                ui::caption(ui, &tf!("Not available: {e}", "Indisponível: {e}"));
                            }
                            Some(Ok(list)) if list.is_empty() => {
                                ui::caption(ui, t!("The agent has no keys.", "O agente não tem chaves."));
                            }
                            Some(Ok(list)) => {
                                for k in list.iter() {
                                    let fp = agent::key_fingerprint(k.key_data());
                                    let already = keys.iter().any(|x| x.key_data() == k.key_data());
                                    ui::row(
                                        ui,
                                        |ui| {
                                            ui.vertical(|ui| {
                                                ui.spacing_mut().item_spacing.y = 2.0;
                                                ui::truncate(ui, key_name(k));
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(&fp)
                                                            .monospace()
                                                            .size(ui::size::SMALL)
                                                            .color(p.muted),
                                                    )
                                                    .truncate(),
                                                );
                                            });
                                        },
                                        |ui| {
                                            if already {
                                                ui::badge(ui, t!("added", "adicionada"), Tone::Success);
                                            } else if ui::secondary(ui, t!("Add", "Adicionar")).clicked() {
                                                added = Some(k.clone());
                                            }
                                        },
                                    );
                                }
                            }
                        }
                        ui.add_space(space::M);
                        ui.add(
                            egui::Label::new(
                                RichText::new(t!(
                                    "Or paste a public key (from `rdesk agent-keys` on the other computer)",
                                    "Ou cole uma chave pública (de `rdesk agent-keys` no outro computador)"
                                ))
                                .strong(),
                            )
                            .wrap(),
                        );
                        ui.add(
                            egui::TextEdit::multiline(text)
                                .hint_text(t!("ssh-ed25519 AAAA… comment", "ssh-ed25519 AAAA… comentário"))
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );
                        if let Some(e) = error {
                            ui.add(egui::Label::new(RichText::new(e.as_str()).color(p.danger)).wrap());
                        }
                    },
                    |ui| {
                        ui::dialog_buttons(
                            ui,
                            t!("Add pasted key", "Adicionar chave colada"),
                            false,
                            &[t!("Close", "Fechar")],
                        )
                    },
                );
                match d.inner.1 {
                    Some(ui::DialogButton::Primary) if !text.trim().is_empty() => match admin::parse_public_key(text) {
                        Ok(k) => added = Some(k),
                        Err(e) => *error = Some(format!("{e:#}")),
                    },
                    Some(ui::DialogButton::Other(_)) => keep = false,
                    _ => {}
                }
                if let Some(k) = added {
                    match admin::add_key(&k) {
                        Ok(true) => {
                            let fp = agent::key_fingerprint(k.key_data());
                            self.notify(tf!("Added {fp}.", "Chave {fp} adicionada."), false);
                            text.clear();
                            *error = None;
                        }
                        Ok(false) => self
                            .notify(t!("That key was already authorized.", "Essa chave já estava autorizada."), false),
                        Err(e) => self.notify(format!("{e:#}"), true),
                    }
                    self.keys = admin::load_keys().unwrap_or_default();
                    if self.service.is_some() {
                        if matches!(self.status().state, HostState::Failed(_)) {
                            self.restart_service_host();
                        }
                    } else if matches!(self.status().state, HostState::Failed(_)) && remembered_enabled() {
                        self.handle = None;
                        self.start();
                    }
                }
                if d.closed {
                    keep = false;
                }
            }
            Dialog::Register { error } => {
                let busy = self.reg_job.is_some();
                let d = ui::dialog_with_footer(
                    ctx,
                    "host-register",
                    t!("Register this computer", "Cadastrar este computador"),
                    480.0,
                    |ui| {
                        let p = ui::pal(ui);
                        ui::paragraph(
                            ui,
                            t!(
                                "It joins your rdesk server: you reach it from anywhere, and the server controls who else may.",
                                "Ele entra no seu servidor rdesk: você o acessa de qualquer lugar, e o servidor controla \
                             quem mais pode."
                            ),
                        );
                        ui.add_space(space::S);
                        ui.label(t!("Name", "Nome"));
                        ui.add(egui::TextEdit::singleline(&mut self.reg_name).desired_width(f32::INFINITY));
                        ui.add_space(space::XS);
                        ui.label(t!("Who can reach it", "Quem pode acessar"));
                        ui.radio_value(&mut self.reg_visibility, Visibility::Private, t!("Only me", "Só eu"));
                        ui.radio_value(
                            &mut self.reg_visibility,
                            Visibility::Shared,
                            t!("Shared (choose with whom later)", "Compartilhado (escolha com quem depois)"),
                        );
                        ui.radio_value(
                            &mut self.reg_visibility,
                            Visibility::Everyone,
                            t!("Everyone on the server", "Todos do servidor"),
                        );
                        let go = who_may_reach(ui);
                        if let Some(e) = error {
                            ui.add(egui::Label::new(RichText::new(e.as_str()).color(p.danger)).wrap());
                        }
                        if busy {
                            ui::busy(ui, t!("Registering…", "Cadastrando…"));
                        }
                        (ui.input(|i| i.key_pressed(egui::Key::Enter)), go)
                    },
                    |ui| ui::dialog_buttons(ui, t!("Register", "Cadastrar"), false, &[cancel]),
                );
                let ((enter, go), button) = d.inner;
                let ready = !busy && !self.reg_name.trim().is_empty();
                if ready && (matches!(button, Some(ui::DialogButton::Primary)) || enter) {
                    *error = None;
                    self.register(ctx);
                }
                if go {
                    self.go_to = Some(super::Page::Computers);
                    keep = false;
                }
                if matches!(button, Some(ui::DialogButton::Other(_))) || d.closed {
                    keep = false;
                }
            }
            Dialog::RemovePassword => {
                let r = ui::confirm(
                    ctx,
                    "host-remove-password",
                    t!("Remove the password?", "Remover a senha?"),
                    t!(
                        "This computer's password stops working: only SSH keys and the server will let people in.",
                        "A senha deste computador deixa de funcionar: só chaves SSH e o servidor vão dar acesso."
                    ),
                    t!("Remove password", "Remover senha"),
                    true,
                );
                if let Some(yes) = r {
                    if yes {
                        match admin::clear_password() {
                            Ok(()) => self.notify(t!("Password removed.", "Senha removida."), false),
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
                        self.has_password = admin::has_password();
                    }
                    keep = false;
                }
            }
            Dialog::RemoveKey { fp, name } => {
                let r = ui::confirm(
                    ctx,
                    "host-remove-key",
                    t!("Remove this key?", "Remover esta chave?"),
                    &tf!("{name} ({fp}) will no longer be able to log in.", "{name} ({fp}) não vai mais poder entrar."),
                    t!("Remove key", "Remover chave"),
                    true,
                );
                if let Some(yes) = r {
                    if yes {
                        if let Err(e) = admin::remove_key(fp) {
                            self.notify(format!("{e:#}"), true);
                        }
                        self.keys = admin::load_keys().unwrap_or_default();
                    }
                    keep = false;
                }
            }
            Dialog::NewIdentity => {
                let r = ui::confirm(
                    ctx,
                    "host-new-identity",
                    t!("Create a new host identity?", "Criar uma nova identidade do host?"),
                    t!(
                        "This computer gets a new host ID. Others can no longer reach it until they are given the new ID.",
                        "Este computador ganha um novo ID do host. Os outros não conseguem mais acessá-lo até receberem \
                         o novo ID."
                    ),
                    t!("Create new identity", "Criar nova identidade"),
                    true,
                );
                if let Some(yes) = r {
                    if yes {
                        match admin::regenerate_identity() {
                            Ok(fp) => {
                                self.fingerprint = Some(fp);
                                self.restart();
                                self.notify(tf!("New host ID: {}", "Novo ID do host: {}", format_host_id(&fp)), false);
                            }
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
                    }
                    keep = false;
                }
            }
            Dialog::Unregister(name) => {
                let r = ui::confirm(
                    ctx,
                    "host-unregister",
                    &tf!("Unregister {name}?", "Descadastrar {name}?"),
                    t!(
                        "Nobody will reach it through the server any more, and its sharing is removed.",
                        "Ninguém mais vai acessá-lo pelo servidor, e o compartilhamento dele é removido."
                    ),
                    t!("Unregister", "Descadastrar"),
                    true,
                );
                if let Some(yes) = r {
                    if yes {
                        self.unregister(ctx);
                    }
                    keep = false;
                }
            }
            Dialog::RestartAsAdmin => {
                let (title, body) = presence::restart_question();
                let r = ui::confirm(
                    ctx,
                    "host-admin",
                    title,
                    body,
                    t!("Open as administrator", "Abrir como administrador"),
                    false,
                );
                if let Some(yes) = r {
                    keep = false;
                    if yes {
                        match autostart::restart_as_admin(&self.exe) {
                            Ok(()) => self.quit_requested = true,
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
                    }
                }
            }
            Dialog::UninstallService => {
                let r = ui::confirm(
                    ctx,
                    "host-uninstall-service",
                    t!("Uninstall the rdesk service?", "Desinstalar o serviço do rdesk?"),
                    t!(
                        "This app runs the host again, only while it is open and you are signed in: no access before \
                         sign-in, on the lock screen or through UAC prompts. Windows asks for administrator permission.",
                        "Este app volta a executar o host, só enquanto estiver aberto e você estiver logado: sem acesso \
                         antes do login, na tela de bloqueio ou nos avisos do UAC. O Windows pede permissão de \
                         administrador."
                    ),
                    t!("Uninstall", "Desinstalar"),
                    true,
                );
                if let Some(yes) = r {
                    if yes {
                        self.service_action(ctx, false);
                    }
                    keep = false;
                }
            }
            Dialog::Disconnect(who) => {
                let r = ui::confirm(
                    ctx,
                    "host-disconnect",
                    &tf!("Disconnect {who}?", "Desconectar {who}?"),
                    t!(
                        "The session ends now, and the other side will not reconnect by itself.",
                        "A sessão termina agora, e o outro lado não vai reconectar sozinho."
                    ),
                    t!("Disconnect", "Desconectar"),
                    true,
                );
                if let Some(yes) = r {
                    if yes {
                        self.disconnect_session();
                    }
                    keep = false;
                }
            }
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }
}

// ---- small pieces ----------------------------------------------------------------------

/// A checklist mark.
#[derive(Clone, Copy)]
enum Mark {
    Done,
    Todo,
    Warn,
    Busy,
}

fn mark(ui: &mut Ui, m: Mark) {
    let p = ui::pal(ui);
    let (icon, c) = match m {
        Mark::Done => (icons::CHECK_CIRCLE, p.success),
        Mark::Todo => (icons::CIRCLE, p.muted),
        Mark::Warn => (icons::WARNING, p.warning),
        Mark::Busy => {
            ui.add(egui::Spinner::new().size(ui::size::ICON + 4.0));
            return;
        }
    };
    ui.label(RichText::new(icon).size(ui::size::ICON + 4.0).color(c));
}

/// Like [`ui::row`], at least [`ROW_H`] tall: the right side first, the left gets the rest.
fn tall_row<R>(ui: &mut Ui, left: impl FnOnce(&mut Ui) -> R, right: impl FnOnce(&mut Ui)) -> R {
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            right(ui);
            ui.with_layout(Layout::left_to_right(Align::Center), left).inner
        })
        .inner
    })
    .inner
}

/// A title and a wrapped, muted line under it.
fn title_detail(ui: &mut Ui, title: &str, detail: Option<&str>) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.add(egui::Label::new(RichText::new(title).strong()).wrap());
        if let Some(d) = detail {
            ui::caption(ui, d);
        }
    });
}

/// "Who may reach it is set on the Computers page", with the page as a link. `true` when
/// the link was clicked.
fn who_may_reach(ui: &mut Ui) -> bool {
    let p = ui::pal(ui);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::XS;
        ui.label(
            RichText::new(t!("Who may reach it is set on the page", "Quem pode acessá-lo é definido na página"))
                .size(ui::size::SMALL)
                .color(p.muted),
        );
        ui.add(egui::Link::new(RichText::new(t!("Computers", "Computadores")).size(ui::size::SMALL))).clicked()
    })
    .inner
}

/// What was clicked in [`who_here`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhoClick {
    Nothing,
    Computers,
    Manage,
}

/// Under "Registered as": whose it is, who manages it and who it is shared with, and
/// "Manage access" for who may change it (here: anyone signed in, as administrator).
/// Before the server listed it, the link to the Computers page.
fn who_here(ui: &mut Ui, m: Option<&Machine>, access: Option<&Result<(Shares, Vec<UserRef>), String>>) -> WhoClick {
    let Some(m) = m else {
        return if who_may_reach(ui) { WhoClick::Computers } else { WhoClick::Nothing };
    };
    let p = ui::pal(ui);
    let small = |ui: &mut Ui, text: &str| {
        ui.add(egui::Label::new(RichText::new(text).size(ui::size::SMALL).color(p.muted)).wrap());
    };
    let person = |u: &UserRef| super::access::person_label(u).to_string();
    small(
        ui,
        &if m.is_owner {
            t!("Owner: you", "Dono: você").to_string()
        } else {
            tf!("Owner: {}", "Dono: {}", m.owner.email)
        },
    );
    match access {
        None => small(ui, t!("Loading who may reach it…", "Carregando quem pode acessá-lo…")),
        Some(Err(e)) => {
            small(ui, &tf!("Cannot load who may reach it: {e}", "Não foi possível carregar quem o acessa: {e}"))
        }
        Some(Ok((shares, managers))) => {
            if !managers.is_empty() {
                let names: Vec<String> = managers.iter().map(person).collect();
                small(ui, &tf!("Managers: {}", "Gestores: {}", names.join(", ")));
            }
            match m.visibility {
                Visibility::Private => small(ui, t!("Not shared with anyone.", "Não compartilhado com ninguém.")),
                Visibility::Everyone => {
                    small(ui, t!("Everyone on the server may reach it.", "Todos do servidor podem acessá-lo."))
                }
                Visibility::Shared if shares.users.is_empty() && shares.groups.is_empty() => {
                    small(ui, t!("Not shared with anyone yet.", "Ainda não compartilhado com ninguém."))
                }
                Visibility::Shared => {
                    let role = |r| rdesk_common::i18n::role_label(r);
                    let mut who: Vec<String> =
                        shares.groups.iter().map(|(g, r)| format!("{} ({})", g.name, role(*r))).collect();
                    who.extend(shares.users.iter().map(|(u, r)| format!("{} ({})", person(u), role(*r))));
                    small(ui, &tf!("Shared with: {}", "Compartilhado com: {}", who.join(", ")));
                }
            }
        }
    }
    let manage = m.can_manage || m.here.is_some();
    let label =
        if manage { t!("Manage access", "Gerenciar acesso") } else { t!("See in Computers", "Ver em Computadores") };
    if ui.add(egui::Link::new(RichText::new(label).size(ui::size::SMALL))).clicked() {
        if manage { WhoClick::Manage } else { WhoClick::Computers }
    } else {
        WhoClick::Nothing
    }
}

/// How others reach it: an icon in a color and a wrapped line.
fn reach_line(ui: &mut Ui, icon: &str, color: egui::Color32, text: &str) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(ui::size::ICON).color(color));
        ui.add(egui::Label::new(text).wrap());
    })
    .response
}

/// A warning icon and a wrapped line in the warning color.
fn warn_line(ui: &mut Ui, text: &str) {
    let p = ui::pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(icons::WARNING).color(p.warning));
        ui.add(egui::Label::new(RichText::new(text).size(ui::size::SMALL).color(p.warning)).wrap());
    });
}

/// One event of the host: a warning icon when it is one, the text, and when on the right.
fn event_line(ui: &mut Ui, e: &HostEvent, now: i64) {
    let p = ui::pal(ui);
    ui::row(
        ui,
        |ui| {
            let (icon, c) = if e.warning { (icons::WARNING, p.warning) } else { (icons::CIRCLE, p.border) };
            ui.label(RichText::new(icon).size(if e.warning { ui::size::ICON } else { 8.0 }).color(c));
            ui.add(egui::Label::new(&e.text).wrap());
        },
        |ui| {
            ui.label(RichText::new(directory::ago(unix(e.at), now)).size(ui::size::SMALL).color(p.muted));
        },
    );
}

/// A card tinted with a tone (the session in progress).
fn tinted_card<R>(ui: &mut Ui, tone: Tone, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = ui::pal(ui);
    let c = tone.color(&p);
    egui::Frame::new()
        .fill(ui::tint(c, if p.dark { 26 } else { 18 }))
        .stroke(Stroke::new(1.0, ui::tint(c, 150)))
        .corner_radius(ui::RADIUS_CARD)
        .inner_margin(Margin::same(space::L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// "ssh-ed25519 ana@notebook" (or "(no comment)").
fn key_name(k: &PublicKey) -> String {
    let comment = if k.comment().is_empty() { t!("(no comment)", "(sem comentário)") } else { k.comment() };
    format!("{} {comment}", k.algorithm())
}

fn password_dialog() -> Dialog {
    Dialog::Password { a: Default::default(), b: Default::default(), op_ref: String::new(), shown: None, error: None }
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The host can run with nobody signed in: registered (its own credential), or asking a
/// server to add it.
pub(super) fn runs_signed_out() -> bool {
    Registration::load().is_some()
        || admin::load_config().ok().and_then(|c| rdesk_host::enroll::settings(&c.enroll)).is_some()
}

pub(super) fn remembered_enabled() -> bool {
    config::load_toml::<config::ClientConfig>(config::CLIENT_CONFIG).map(|c| c.app.host_enabled).unwrap_or(false)
}

fn remember_enabled(on: bool) {
    let mut cfg: config::ClientConfig = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
    if cfg.app.host_enabled != on {
        cfg.app.host_enabled = on;
        let _ = config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &cfg));
    }
}

// ---- gallery -------------------------------------------------------------------------------

/// Screens for the ui gallery (see `app/gallery.rs`): sample data, long texts.
#[cfg(all(test, feature = "host"))]
pub(super) fn gallery() -> Vec<super::gallery::Screen> {
    use super::Page;
    use super::gallery::{page, screen, with_sidebar};
    use rdesk_host::control::SessionInfo;
    use sample::*;
    use ssh_key::public::{Ed25519PublicKey, KeyData};

    /// The page as the window shows it.
    fn shown(make: impl Fn() -> HostPage + 'static) -> impl Fn() -> Box<dyn FnMut(&mut Ui)> {
        move || {
            let mut p = make();
            Box::new(move |ui: &mut Ui| with_sidebar(ui, Page::ThisComputer, |ui| p.show(ui)))
        }
    }
    fn s(name: &'static str, make: impl Fn() -> HostPage + 'static) -> super::gallery::Screen {
        let f = shown(make);
        screen(name, move || f())
    }
    /// `join_section` in a card, as on the sign-in screen.
    fn join(name: &'static str, server: &'static str, make: impl Fn() -> HostPage + 'static) -> super::gallery::Screen {
        screen(name, move || {
            let mut p = make();
            move |ui: &mut Ui| {
                page(ui, |ui| {
                    ui.scope(|ui| {
                        ui.set_max_width(440.0);
                        ui::card(ui, |ui| p.join_section(ui, server));
                    });
                })
            }
        })
    }
    fn off() -> HostPage {
        let mut p = HostPage::sample(HostStatus { events: events(3), ..Default::default() }, false, false);
        p.keys.clear();
        p
    }

    vec![
        s("host-off", off),
        s("host-off-nothing-set", || {
            let mut p = off();
            p.has_password = false;
            p.cfg.direct.enabled = Some(false);
            p.saved_cfg = p.cfg.clone();
            p
        }),
        s("host-on-registered", || HostPage::sample(on_registered(), true, false).registered()),
        // Signed in as someone else than who registered it: whose it is, who may reach it.
        s("host-registered-by-someone-else", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.listed(false, Visibility::Shared);
            p
        }),
        s("host-registered-mine-private", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.listed(true, Visibility::Private);
            p
        }),
        s("host-on-unregistered-norelay", || {
            HostPage::sample(HostStatus { state: listening(), events: events(2), ..Default::default() }, true, false)
        }),
        s("host-in-use", || {
            let st = HostStatus { session: Some(session()), events: events(12), ..on_registered() };
            HostPage::sample(st, true, false).registered()
        }),
        s("host-failed", || {
            let st = HostStatus {
                state: HostState::Failed(
                    "Could not bind UDP port 7420: another program is using it (os error 10048). Choose another \
                     port in Performance and files."
                        .into(),
                ),
                relay: Some(Err("connection timed out".into())),
                events: events(4),
                ..Default::default()
            };
            HostPage::sample(st, false, false)
        }),
        s("host-relay-error", || {
            let st = HostStatus {
                relay: Some(Err(
                    "connection timed out after 10 s while reaching relay.familia-santos.exemplo.com.br".into()
                )),
                registration: Some(Err(
                    "The server refused this computer: its registration was removed by an administrator".into(),
                )),
                ..on_registered()
            };
            HostPage::sample(st, true, false).registered()
        }),
        s("host-waiting", || {
            let st = HostStatus {
                state: HostState::Starting,
                enrollment: Some(
                    "Waiting for ana.beatriz.ferreira.dos.santos@exemplo.com.br to approve it (code 4F7-2KQ)".into(),
                ),
                events: events(2),
                ..Default::default()
            };
            HostPage::sample(st, true, false)
        }),
        s("host-service-not-elevated", || {
            let st = HostStatus {
                session: Some(SessionInfo { admin_terminals: 0, terminals: 1, ..session() }),
                ..on_registered()
            };
            let mut p = HostPage::sample(st, true, true);
            p.elevated = false;
            p
        }),
        s("host-tab-security", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Security;
            p.cfg.direct.enabled = Some(true);
            p.cfg.require_local = true;
            p.saved_cfg = p.cfg.clone();
            p
        }),
        s("host-tab-security-empty", || {
            let mut p = off();
            p.tab = Tab::Security;
            p.has_password = false;
            p.cfg.require_local = true;
            p.cfg.direct.enabled = Some(false);
            p.saved_cfg = p.cfg.clone();
            p
        }),
        s("host-tab-security-service", || {
            let mut p = HostPage::sample(on_registered(), true, true);
            p.elevated = false;
            p.tab = Tab::Security;
            p
        }),
        s("host-tab-performance", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Performance;
            p
        }),
        s("host-pending", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Performance;
            p.cfg.max_fps = 60;
            p.cfg.clipboard = false;
            p
        }),
        s("host-tab-activity", || {
            let mut p =
                HostPage::sample(HostStatus { events: events(40), ..on_registered() }, true, false).registered();
            p.tab = Tab::Activity;
            p
        }),
        s("host-tab-activity-empty", || {
            let mut p = off();
            p.tab = Tab::Activity;
            p
        }),
        s("host-tab-diagnostics", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Diagnostics;
            p.diag = Some(diag());
            p
        }),
        s("host-tab-diagnostics-busy", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Diagnostics;
            p.busy = Some("diagnose");
            p
        }),
        s("host-dialog-password", || {
            let mut p = HostPage::sample(on_registered(), true, false);
            p.dialog = Some(Dialog::Password {
                a: Zeroizing::new("curta".into()),
                b: Zeroizing::new("curto".into()),
                op_ref: String::new(),
                shown: None,
                error: Some("1Password: the item op://Private/rdesk/password was not found in any vault".into()),
            });
            p
        }),
        s("host-dialog-password-generated", || {
            let mut p = HostPage::sample(on_registered(), true, false);
            let pw = "k3F9-7a2C-Qm81-x0Lp-Zt44-Wb2n".to_string();
            p.dialog = Some(Dialog::Password {
                a: Zeroizing::new(pw.clone()),
                b: Zeroizing::new(pw.clone()),
                op_ref: "op://Private/rdesk/password".into(),
                shown: Some(pw),
                error: None,
            });
            p.busy = Some("password");
            p
        }),
        s("host-dialog-addkey", || {
            let mut p = HostPage::sample(on_registered(), true, false);
            let mut agent_keys = keys();
            agent_keys.push(PublicKey::new(
                KeyData::Ed25519(Ed25519PublicKey([9; 32])),
                "jeferson.santos.silva@exemplo.com.br — chave pessoal do 1Password",
            ));
            p.dialog = Some(Dialog::AddKey {
                text: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK".into(),
                agent: Some(Ok(agent_keys)),
                error: Some("This is not an OpenSSH public key: unexpected end of the base64 data".into()),
            });
            p
        }),
        s("host-dialog-addkey-noagent", || {
            let mut p = HostPage::sample(on_registered(), true, false);
            p.dialog = Some(Dialog::AddKey {
                text: String::new(),
                agent: Some(Err("no SSH agent: SSH_AUTH_SOCK is not set and the 1Password pipe was not found".into())),
                error: None,
            });
            p
        }),
        s("host-dialog-register", || {
            let mut p = off();
            p.reg_visibility = Visibility::Shared;
            p.dialog = Some(Dialog::Register {
                error: Some("The server refused it: a computer named like this already exists in your account".into()),
            });
            p
        }),
        s("host-confirm-unregister", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.dialog =
                Some(Dialog::Unregister("Notebook da Ana Beatriz no escritório em rdesk da Família Santos".into()));
            p
        }),
        s("host-confirm-new-identity", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Security;
            p.dialog = Some(Dialog::NewIdentity);
            p
        }),
        s("host-confirm-remove-key", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Security;
            let k = &keys()[0];
            p.dialog = Some(Dialog::RemoveKey { fp: agent::key_fingerprint(k.key_data()), name: key_name(k) });
            p
        }),
        s("host-confirm-remove-password", || {
            let mut p = HostPage::sample(on_registered(), true, false).registered();
            p.tab = Tab::Security;
            p.dialog = Some(Dialog::RemovePassword);
            p
        }),
        s("host-confirm-uninstall-service", || {
            let mut p = HostPage::sample(on_registered(), true, true);
            p.dialog = Some(Dialog::UninstallService);
            p
        }),
        s("host-confirm-disconnect", || {
            let st = HostStatus { session: Some(session()), ..on_registered() };
            let mut p = HostPage::sample(st, true, false).registered();
            p.dialog = Some(Dialog::Disconnect(session().client));
            p
        }),
        join("host-join-form", "rdesk.familia-santos.exemplo.com.br", || {
            let mut p = off();
            p.join_owner = "ana.beatriz.ferreira.dos.santos@exemplo.com.br".into();
            p
        }),
        join("host-join-key-noserver", "", || {
            let mut p = off();
            p.join_use_key = true;
            p
        }),
        join("host-join-failed", "rdesk.exemplo.com.br", || {
            let st = HostStatus {
                state: HostState::Failed("The server refused the request: unknown owner ana@exemplo.com".into()),
                ..Default::default()
            };
            HostPage::sample(st, false, false)
        }),
        join("host-join-waiting", "rdesk.exemplo.com.br", || {
            let st = HostStatus {
                state: HostState::Starting,
                enrollment: Some(
                    "Waiting for ana.beatriz.ferreira.dos.santos@exemplo.com.br to approve it (code 4F7-2KQ)".into(),
                ),
                ..Default::default()
            };
            HostPage::sample(st, true, false)
        }),
        join("host-join-registered", "rdesk.exemplo.com.br", || {
            HostPage::sample(on_registered(), true, false).registered()
        }),
        join("host-join-service", "rdesk.exemplo.com.br", || HostPage::sample(on_registered(), true, true)),
    ]
}

#[cfg(test)]
mod sample {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::time::Duration;

    use rdesk_host::control::SessionInfo;
    use ssh_key::public::{Ed25519PublicKey, KeyData};

    use super::*;

    impl HostPage {
        /// A page drawn from `status`, touching nothing on this machine: no host, no service
        /// watcher, no files.
        pub(super) fn sample(status: HostStatus, running: bool, service: bool) -> Self {
            let rt = Arc::new(tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime"));
            let reporter = Reporter::new(|| {});
            reporter.replace(status);
            let mut p = Self::with(PathBuf::new(), rt, reporter);
            p.sample = Some(Sample { service, running });
            p.elevated = true;
            p.reg_name = "Notebook da Ana Beatriz no escritório".into();
            p.fingerprint = Some(core::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11)));
            p.has_password = true;
            p.keys = keys();
            p.cfg.inbox_dir = Some(PathBuf::from(
                r"C:\Users\Jeferson Santos da Silva\Documentos\Arquivos recebidos pelo rdesk\Computador do escritório",
            ));
            p.saved_cfg = p.cfg.clone();
            p
        }

        /// Registered with a server (as read from disk).
        pub(super) fn registered(mut self) -> Self {
            self.registration = Some(Registration {
                server: "https://rdesk.familia-santos.exemplo.com.br".into(),
                server_name: "rdesk da Família Santos".into(),
                machine_id: 42,
                name: "Notebook da Ana Beatriz no escritório".into(),
                token: String::new(),
                server_key: String::new(),
                relay_url: "https://relay.familia-santos.exemplo.com.br".into(),
                endpoint_id: String::new(),
            });
            self
        }

        /// As the server lists it for whoever is signed in, with who may reach it.
        pub(super) fn listed(&mut self, mine: bool, visibility: Visibility) {
            use rdesk_proto::api::{GroupRef, Here, ShareRole};
            let user = |id: i64, email: &str, name: &str| UserRef { id, email: email.into(), name: name.into() };
            let owner = if mine {
                user(1, "jeferson@exemplo.com", "")
            } else {
                user(9, "ana.beatriz.ferreira.dos.santos@exemplo.com.br", "")
            };
            let m = Machine {
                id: 42,
                endpoint_id: format_host_id(&self.fingerprint.unwrap_or_default()),
                name: "Notebook da Ana Beatriz no escritório".into(),
                os: "windows".into(),
                owner,
                visibility,
                online: true,
                in_session: false,
                last_seen: None,
                addrs: Vec::new(),
                can_access: mine,
                access: if mine { rdesk_proto::Access::FULL } else { rdesk_proto::Access::NONE },
                is_owner: mine,
                can_manage: mine,
                direct: false,
                version: "0.5.0".into(),
                ring: Default::default(),
                here: Some(Here::View),
                can_wake: false,
            };
            let shares = Shares {
                users: vec![
                    (user(2, "jefersonsantos006@hotmail.com", ""), ShareRole::Control),
                    (user(3, "mariana.costa@exemplo.com", "Mariana Costa"), ShareRole::View),
                ],
                groups: vec![(GroupRef { id: 1, name: "Família".into() }, ShareRole::Files)],
            };
            let managers = vec![user(4, "carlos@exemplo.com", "Carlos")];
            self.access = Some(((m.id, Ok((shares, managers))), Instant::now()));
            self.this_machine = Some(m);
        }
    }

    pub(super) fn keys() -> Vec<PublicKey> {
        let k = |n: u8, c: &str| PublicKey::new(KeyData::Ed25519(Ed25519PublicKey([n; 32])), c);
        vec![
            k(1, "jeferson@notebook-do-escritorio-com-um-nome-de-maquina-bem-comprido"),
            k(2, "ana.beatriz@exemplo.com.br (1Password)"),
            k(3, ""),
        ]
    }

    pub(super) fn events(n: usize) -> VecDeque<HostEvent> {
        let texts = [
            ("ana.beatriz.ferreira@exemplo.com.br connected from 192.168.15.23 (key SHA256:Xk3f…)", false),
            ("Session ended: ana.beatriz.ferreira@exemplo.com.br disconnected after 1 h 12 min", false),
            (
                "Refused a login from 203.0.113.77: direct access is only allowed from the local network or Tailscale",
                true,
            ),
            ("Terminal opened as administrator", false),
            ("Relay unreachable: connection timed out after 10 s; trying again in 30 s", true),
            ("Listening on UDP 7420", false),
        ];
        let now = SystemTime::now();
        (0..n)
            .map(|i| {
                let (text, warning) = texts[i % texts.len()];
                let at = now - Duration::from_secs(((n - i) as u64) * 7 * 60);
                HostEvent { at, text: text.to_string(), warning }
            })
            .collect()
    }

    pub(super) fn listening() -> HostState {
        HostState::Listening(vec![SocketAddr::from(([0, 0, 0, 0], 7420))])
    }

    pub(super) fn on_registered() -> HostStatus {
        HostStatus {
            state: listening(),
            relay: Some(Ok("https://relay.familia-santos.exemplo.com.br".into())),
            registration: Some(Ok("Notebook da Ana Beatriz no escritório em rdesk da Família Santos".into())),
            events: events(8),
            ..Default::default()
        }
    }

    pub(super) fn session() -> SessionInfo {
        SessionInfo {
            client: "ana.beatriz.ferreira.dos.santos@exemplo.com.br".into(),
            peer: "relay (relay.familia-santos.exemplo.com.br)".into(),
            method: "key SHA256:Xk3fP0q9ZtW2bLm8".into(),
            since: SystemTime::now() - Duration::from_secs(47 * 60),
            terminals: 2,
            admin_terminals: 1,
        }
    }

    pub(super) fn diag() -> Vec<CheckLine> {
        let l = |label: &str, value: &str, ok: bool| CheckLine { label: label.into(), value: value.into(), ok };
        vec![
            l("Pasta do host", r"C:\ProgramData\rdesk\host", true),
            l("ID do host", "0b30557a9fc4e9136d88b2d7fc21460bea0f344b8bc6a7e0d71c2b4e7f86a1c3", true),
            l("Chaves SSH", "3 autorizadas", true),
            l("Senha", "definida", true),
            l("Cadastro", "Notebook da Ana Beatriz no escritório em rdesk da Família Santos", true),
            l(
                "Relay",
                "https://relay.familia-santos.exemplo.com.br: connection timed out after 10 s (is the firewall blocking UDP 443?)",
                false,
            ),
            l("Telas", "2 encontradas: 2560×1440 e 1920×1080", true),
        ]
    }
}

/// macOS: what the host still needs (Screen Recording, Accessibility), with buttons that
/// open the right pane of System Settings. Nothing when both are given.
#[cfg(target_os = "macos")]
fn mac_permissions_banner(ui: &mut Ui) {
    use rdesk_host::mac_permissions::{self as mp, Pane};
    use std::sync::Mutex;
    use std::time::Duration;
    // Asked every couple of seconds, not every frame.
    static CACHE: Mutex<Option<(Instant, bool, bool)>> = Mutex::new(None);
    let (screen, input) = {
        let mut c = CACHE.lock().unwrap();
        match *c {
            Some((at, s, i)) if at.elapsed() < Duration::from_secs(2) => (s, i),
            _ => {
                let (s, i) = (mp::screen_allowed(), mp::input_allowed());
                *c = Some((Instant::now(), s, i));
                (s, i)
            }
        }
    };
    if screen && input {
        return;
    }
    ui.add_space(space::M);
    let text = match (screen, input) {
        (false, false) => t!(
            "macOS has not given rdesk Screen Recording nor Accessibility: others see no screen and cannot click.",
            "O macOS não deu ao rdesk Gravação de Tela nem Acessibilidade: quem conecta não vê a tela nem consegue clicar."
        ),
        (false, true) => t!(
            "macOS has not given rdesk Screen Recording: others see no screen.",
            "O macOS não deu ao rdesk Gravação de Tela: quem conecta não vê a tela."
        ),
        _ => t!(
            "macOS has not given rdesk Accessibility: others see the screen but cannot click or type.",
            "O macOS não deu ao rdesk Acessibilidade: quem conecta vê a tela, mas não consegue clicar nem digitar."
        ),
    };
    ui::banner(ui, Tone::Warning, text, None);
    ui.horizontal_wrapped(|ui| {
        if !screen && ui::secondary(ui, t!("Open Screen Recording", "Abrir Gravação de Tela")).clicked() {
            mp::open_settings(Pane::ScreenRecording);
        }
        if !input && ui::secondary(ui, t!("Open Accessibility", "Abrir Acessibilidade")).clicked() {
            mp::open_settings(Pane::Accessibility);
        }
    });
    ui::caption(
        ui,
        t!(
            "Add rdesk there and turn it on, then quit rdesk (tray icon) and open it again.",
            "Adicione o rdesk lá e ligue a chave; depois saia do rdesk (ícone da barra de menus) e abra de novo."
        ),
    );
}
