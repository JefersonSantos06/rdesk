//! "This computer": the built-in host. Turn it on/off, see who is connected and what
//! happened, manage how people log in (password, SSH keys), options and diagnostics.
//!
//! On Windows the host can instead be run by the rdesk service (as SYSTEM, so it also
//! works on the sign-in and lock screens and through UAC). Then this page shows that host's
//! status through its control pipe, and changing its settings needs administrator rights.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use egui::{Color32, RichText};
use rdesk_common::account::{Registration, Session};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, HostConfig, format_host_id};
use rdesk_common::directory;
use rdesk_host::admin::{self, CheckLine};
use rdesk_host::ipc::RemoteHost;
use rdesk_host::{HostHandle, HostState, HostStatus, Reporter, RunOpts};
use rdesk_proto::api::Visibility;
use ssh_key::PublicKey;
use zeroize::Zeroizing;

use super::autostart;

const GREEN: Color32 = Color32::from_rgb(46, 184, 92);
const ORANGE: Color32 = Color32::from_rgb(240, 160, 40);
const RED: Color32 = Color32::from_rgb(230, 70, 70);

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
    RemovePassword,
    RemoveKey(String),
    NewIdentity,
}

pub(super) struct HostPage {
    exe: PathBuf,
    /// Registration with the server: the name and visibility typed for it, and the job.
    reg_name: String,
    reg_visibility: Visibility,
    reg_job: Option<Receiver<Result<String, String>>>,
    confirm_unregister: bool,
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
    cfg: HostConfig,
    keys: Vec<PublicKey>,
    has_password: bool,
    fingerprint: Option<[u8; 32]>,
    dialog: Option<Dialog>,
    jobs: (Sender<Job>, Receiver<Job>),
    busy: Option<&'static str>,
    diag: Option<Vec<CheckLine>>,
    pub(super) toast: Option<(String, bool, Instant)>,
    /// Ask the shell to quit (after starting an elevated copy).
    pub(super) quit_requested: bool,
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
        let mut p = Self {
            exe,
            reg_name: rdesk_common::local_hostname(),
            reg_visibility: Visibility::Private,
            reg_job: None,
            confirm_unregister: false,
            join_use_key: false,
            join_owner: String::new(),
            join_key: String::new(),
            rt,
            reporter,
            handle: None,
            service: None,
            service_job: None,
            service_checked: Instant::now(),
            cfg: HostConfig::default(),
            keys: Vec::new(),
            has_password: false,
            fingerprint: None,
            dialog: None,
            jobs: channel(),
            busy: None,
            diag: None,
            toast: None,
            quit_requested: false,
        };
        if config::service_mode() {
            p.service = Some(RemoteHost::start(p.rt.handle(), p.reporter.clone()));
        }
        p.reload();
        p
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
                Ok(()) => self.notify("Applied: the service's host is restarting.", false),
                Err(e) => self.notify(format!("Saved, but the host could not be restarted: {e:#}"), true),
            }
        }
    }

    fn reload(&mut self) {
        self.cfg = admin::load_config().unwrap_or_default();
        self.keys = admin::load_keys().unwrap_or_default();
        self.has_password = admin::has_password();
        self.fingerprint = admin::fingerprint().ok();
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some((text.into(), error, Instant::now()));
    }

    pub(super) fn status(&self) -> HostStatus {
        self.reporter.snapshot()
    }

    pub(super) fn running(&self) -> bool {
        if self.service.is_some() {
            return matches!(self.status().state, HostState::Listening(_) | HostState::Starting);
        }
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// One word for the tray: "on", "off", "in use", "waiting", "error".
    pub(super) fn summary(&self) -> String {
        let st = self.status();
        match (&st.state, &st.session) {
            (_, Some(s)) if self.running() => format!("in use by {}", s.client),
            (HostState::Starting, _) if st.enrollment.is_some() => "waiting for approval".into(),
            (HostState::Listening(_), _) => "on".into(),
            (HostState::Starting, _) => "starting".into(),
            (HostState::Failed(_), _) => "error".into(),
            (HostState::Stopped, _) => "off".into(),
        }
    }

    /// The host ID, as clients enter it.
    pub(super) fn fingerprint_text(&self) -> Option<String> {
        self.fingerprint.or(self.status().fingerprint).map(|f| format_host_id(&f))
    }

    pub(super) fn start(&mut self) {
        if self.service.is_some() || self.running() {
            return;
        }
        match admin::ensure_initialized() {
            Ok(fp) => self.fingerprint = Some(fp),
            Err(e) => {
                self.notify(format!("Cannot set up the host: {e:#}"), true);
                return;
            }
        }
        self.handle = Some(HostHandle::start(RunOpts::default(), self.rt.handle(), self.reporter.clone()));
        remember_enabled(true);
    }

    pub(super) fn stop(&mut self) {
        if self.service.is_some() {
            self.notify("The rdesk service runs this computer's host: it is always on.", false);
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
            self.notify("Sign in first.", true);
            return;
        };
        let (name, vis) = (self.reg_name.trim().to_string(), self.reg_visibility);
        self.reg_job = Some(super::account::spawn(ctx, move || async move {
            admin::register(&s, &name, vis)
                .await
                .map(|r| format!("Registered as {} on {}.", r.name, r.server_name))
                .map_err(|e| format!("{e:#}"))
        }));
    }

    fn unregister(&mut self, ctx: &egui::Context) {
        let Some(s) = Session::load() else {
            self.notify("Sign in first.", true);
            return;
        };
        self.reg_job = Some(super::account::spawn(ctx, move || async move {
            admin::unregister(&s).await.map(|_| "Unregistered.".to_string()).map_err(|e| format!("{e:#}"))
        }));
    }

    pub(super) fn tick(&mut self) {
        if self.service_checked.elapsed().as_secs() >= 2 {
            self.service_checked = Instant::now();
            self.check_service_mode();
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
                    // The host picks the registration (relay, tickets) up when it starts.
                    self.restart();
                }
                Err(e) => self.notify(e, true),
            }
        }
        while let Ok(job) = self.jobs.1.try_recv() {
            self.busy = None;
            match job {
                Job::PasswordSet(Ok(())) => {
                    self.has_password = true;
                    self.dialog = None;
                    self.notify("Password saved.", false);
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

    // ---- drawing ------------------------------------------------------------

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        let st = self.status();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            self.status_section(ui, &st);
            ui.add_space(14.0);
            self.server_section(ui, &st);
            ui.add_space(14.0);
            self.session_section(ui, &st);
            ui.add_space(14.0);
            self.login_section(ui);
            ui.add_space(14.0);
            self.options_section(ui);
            ui.add_space(14.0);
            self.activity_section(ui, &st);
            ui.add_space(14.0);
            self.diagnostics_section(ui);
        });
        self.dialogs(ui.ctx());
    }

    fn server_section(&mut self, ui: &mut egui::Ui, st: &HostStatus) {
        let ctx = ui.ctx().clone();
        ui.label(RichText::new("On your rdesk server").strong().size(16.0));
        let busy = self.reg_job.is_some();
        // The service's files are for administrators only: its status says it instead.
        let registered = match (&self.service, &st.registration) {
            (Some(_), Some(Ok(name))) => Some(name.clone()),
            (Some(_), _) => None,
            (None, _) => Registration::load().map(|r| format!("{} on {}", r.name, r.server_name)),
        };
        match registered {
            Some(name) => {
                ui.horizontal(|ui| {
                    ui.label(format!("Registered as {name}."));
                    match &st.registration {
                        Some(Ok(_)) if self.running() => {
                            ui.colored_label(GREEN, "✔ in touch with the server");
                        }
                        Some(Err(e)) if self.running() => {
                            ui.colored_label(ORANGE, e);
                        }
                        _ => {}
                    }
                });
                ui.weak("Who may reach it (only you, shared, everyone) is set on the Computers page.");
                ui.horizontal(|ui| {
                    if self.confirm_unregister {
                        ui.label("Unregister? Nobody will reach it through the server any more.");
                        if ui.add_enabled(!busy, egui::Button::new("Unregister")).clicked() {
                            self.confirm_unregister = false;
                            self.unregister(&ctx);
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_unregister = false;
                        }
                    } else if ui.add_enabled(!busy, egui::Button::new("Unregister…")).clicked() {
                        self.confirm_unregister = true;
                    }
                    if busy {
                        ui.spinner();
                    }
                });
            }
            None => {
                if let Some(msg) = st.enrollment.as_ref().filter(|_| self.running()) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(msg);
                    });
                    ui.weak("Or register it yourself below (it becomes yours at once).");
                    ui.add_space(4.0);
                }
                ui.label(
                    "Not registered: others reach it only on the LAN by address. Register it to reach it \
                     from anywhere, with access controlled by the server.",
                );
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut self.reg_name).desired_width(200.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Who can reach it");
                    ui.radio_value(&mut self.reg_visibility, Visibility::Private, "only me");
                    ui.radio_value(&mut self.reg_visibility, Visibility::Shared, "shared (choose later)");
                    ui.radio_value(&mut self.reg_visibility, Visibility::Everyone, "everyone on the server");
                });
                ui.horizontal(|ui| {
                    let ok = !busy && !self.reg_name.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new("Register this computer")).clicked() {
                        self.register(&ctx);
                    }
                    if busy {
                        ui.spinner();
                    }
                });
            }
        }
    }

    /// On the sign-in screen: make this computer reachable without signing in (it asks the
    /// server to add it; its owner approves it, or an enrollment key adds it at once).
    pub(super) fn join_section(&mut self, ui: &mut egui::Ui, server: &str) {
        ui.add_space(26.0);
        ui.separator();
        ui.add_space(8.0);
        let st = self.status();
        if self.service.is_some() {
            ui.label(RichText::new("This computer").strong());
            ui.label(format!("Run by the rdesk service · host {}.", self.summary()));
            return;
        }
        if let Some(r) = Registration::load() {
            ui.label(RichText::new("This computer").strong());
            ui.label(format!("Reachable as {} on {} · host {}.", r.name, r.server_name, self.summary()));
            let label = if self.running() { "Turn the host off" } else { "Turn the host on" };
            if ui.button(label).clicked() {
                self.toggle();
            }
            return;
        }
        if let Some(msg) = st.enrollment.as_ref().filter(|_| self.running()) {
            ui.label(RichText::new("This computer").strong());
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 380.0).max(0.0) / 2.0);
                ui.spinner();
                ui.label(msg);
            });
            ui.weak("Keep rdesk running (it can stay in the tray) until it is approved.");
            if ui.button("Cancel").clicked() {
                self.stop();
                if let Err(e) = rdesk_host::enroll::save(Default::default()) {
                    self.notify(format!("{e:#}"), true);
                }
            }
            return;
        }
        if let HostState::Failed(e) = &st.state {
            ui.colored_label(ORANGE, e);
        }
        egui::CollapsingHeader::new("Only share this computer (no sign-in)").show(ui, |ui| {
            ui.label(
                "For a computer that is only accessed, never used to reach others: it joins the server \
                 above, and its owner approves it in their rdesk app.",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.join_use_key, false, "Owner's e-mail");
                ui.radio_value(&mut self.join_use_key, true, "Enrollment key");
            });
            let (value, hint) = if self.join_use_key {
                (&mut self.join_key, "rdk_… (from Computers → Add a computer)")
            } else {
                (&mut self.join_owner, "who will own it, e.g. ana@gmail.com")
            };
            ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(360.0));
            ui.add(
                egui::TextEdit::singleline(&mut self.reg_name).hint_text("Name of this computer").desired_width(360.0),
            );
            let ready = !server.trim().is_empty() && !value.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new("Ask to join")).clicked() {
                let value = value.trim().to_string();
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
            if !server.trim().is_empty() {
                ui.weak("The server address is the one typed above.");
            }
        });
    }

    fn status_section(&mut self, ui: &mut egui::Ui, st: &HostStatus) {
        let running = self.running();
        let (color, text) = match &st.state {
            _ if !running && !matches!(st.state, HostState::Failed(_)) => {
                (Color32::GRAY, "Off: nobody can connect to this computer".to_string())
            }
            HostState::Listening(_) => (GREEN, "On: ready for connections".to_string()),
            HostState::Starting => (ORANGE, "Starting…".to_string()),
            HostState::Failed(e) => (RED, format!("Not running: {e}")),
            HostState::Stopped => (Color32::GRAY, "Off".to_string()),
        };
        egui::Frame::group(ui.style()).corner_radius(10.0).inner_margin(14.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 8.0, color);
                ui.label(RichText::new(text).size(17.0).strong());
                if self.service.is_none() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if running { "Turn off" } else { "Turn on" };
                        if ui.button(RichText::new(label).size(15.0)).clicked() {
                            self.toggle();
                        }
                    });
                }
            });
            if let Some(fp) = self.fingerprint_text() {
                ui.horizontal(|ui| {
                    ui.label("Host ID:");
                    ui.label(RichText::new(&fp).monospace().small());
                    if ui.small_button("Copy").on_hover_text("Others add this computer with this ID").clicked() {
                        ui.ctx().copy_text(fp.clone());
                    }
                });
            }
            if let HostState::Listening(addrs) = &st.state
                && running
            {
                let port = addrs.first().map(|a| a.port()).unwrap_or_default();
                match &st.relay {
                    Some(Ok(url)) => {
                        ui.label(format!("Reachable through the relay {url}; direct connections on UDP {port}."));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ORANGE, format!("Relay unreachable ({e}); direct connections on UDP {port}."));
                    }
                    None => {
                        ui.label(format!(
                            "No relay: others need this computer's address (LAN or port forwarding, UDP {port})."
                        ));
                    }
                }
            }
            if cfg!(windows) {
                self.service_row(ui);
            }
        });
    }

    /// Windows: the rdesk service (recommended), or how this app's host runs.
    fn service_row(&mut self, ui: &mut egui::Ui) {
        let elevated = rdesk_host::elevation::is_elevated();
        let busy = self.service_job.is_some();
        if self.service.is_some() {
            ui.colored_label(
                GREEN,
                "✔ Run by the rdesk service: reachable before sign-in, on the lock screen and through UAC prompts.",
            );
            ui.horizontal(|ui| {
                if elevated {
                    ui.weak("You can change this computer's settings below.");
                } else {
                    ui.weak("Changing this computer's settings needs administrator rights.");
                    if ui.button("Manage as administrator").clicked() {
                        match autostart::restart_as_admin(&self.exe) {
                            Ok(()) => self.quit_requested = true,
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
                    }
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Uninstall…"))
                    .on_hover_text("This app runs the host again (only while it is open and you are signed in)")
                    .clicked()
                {
                    self.service_action(ui.ctx(), false);
                }
                if busy {
                    ui.spinner();
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.colored_label(
                ORANGE,
                "⚠ Run by this app: not reachable before you sign in, and the lock screen and UAC prompts \
                 cannot be controlled.",
            );
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Install rdesk on this computer (recommended)"))
                .on_hover_text("Windows asks for administrator permission once")
                .clicked()
            {
                self.service_action(ui.ctx(), true);
            }
            if busy {
                ui.spinner();
                ui.label("Windows may ask for administrator permission…");
            }
        });
    }

    fn session_section(&mut self, ui: &mut egui::Ui, st: &HostStatus) {
        ui.label(RichText::new("Current session").strong().size(16.0));
        match &st.session {
            Some(s) if self.running() => {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} from {} · {} · since {}",
                        s.client,
                        s.peer,
                        s.method,
                        directory::ago(unix(s.since), unix(SystemTime::now()))
                    ));
                    if ui.button("Disconnect").on_hover_text("End it; the other side will not reconnect").clicked() {
                        if let Some(remote) = &self.service {
                            if let Err(e) = self.rt.block_on(remote.disconnect()) {
                                self.notify(format!("{e:#}"), true);
                            }
                        } else if let Some(h) = &self.handle {
                            h.disconnect();
                        }
                    }
                });
            }
            _ => {
                ui.weak("Nobody is connected.");
            }
        }
    }

    fn login_section(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("This computer's own password and keys").strong().size(16.0));
        ui.weak("Used on the LAN without the server, or as the extra lock (Options) after the server's permission.");
        if !self.has_password && self.keys.is_empty() && Registration::load().is_none() {
            ui.colored_label(ORANGE, "Not registered and no password or key: the host cannot start.");
        }
        ui.horizontal(|ui| {
            ui.label("Password:");
            if self.has_password {
                ui.colored_label(GREEN, "set");
                if ui.button("Change…").clicked() {
                    self.dialog = Some(password_dialog());
                }
                if ui.button("Remove").clicked() {
                    self.dialog = Some(Dialog::RemovePassword);
                }
            } else {
                ui.weak("not set");
                if ui.button("Set password…").clicked() {
                    self.dialog = Some(password_dialog());
                }
            }
        });
        ui.label("SSH keys:");
        if self.keys.is_empty() {
            ui.weak("   none");
        }
        let mut remove = None;
        for k in &self.keys {
            let fp = agent::key_fingerprint(k.key_data());
            ui.horizontal(|ui| {
                ui.label(format!(
                    "   {} {}",
                    k.algorithm(),
                    if k.comment().is_empty() { "(no comment)" } else { k.comment() }
                ));
                ui.label(RichText::new(&fp).monospace().small().weak());
                if ui.small_button("Remove").clicked() {
                    remove = Some(fp.clone());
                }
            });
        }
        if let Some(fp) = remove {
            self.dialog = Some(Dialog::RemoveKey(fp));
        }
        if ui.button("＋ Add SSH key…").clicked() {
            self.dialog = Some(Dialog::AddKey { text: String::new(), agent: None, error: None });
            self.load_agent_keys();
        }
    }

    fn options_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Options").strong().size(16.0)).default_open(false).show(ui, |ui| {
            egui::Grid::new("host-opts").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Port");
                ui.add(egui::DragValue::new(&mut self.cfg.port).range(1..=65535))
                    .on_hover_text("UDP port for direct connections (LAN, port forwarding)");
                ui.end_row();
                ui.label("Received files");
                ui.horizontal(|ui| {
                    ui.label(self.cfg.inbox_dir().display().to_string());
                    if ui.small_button("Change…").clicked()
                        && let Some(d) = rfd::FileDialog::new().set_directory(self.cfg.inbox_dir()).pick_folder()
                    {
                        self.cfg.inbox_dir = Some(d);
                    }
                });
                ui.end_row();
                ui.label("Max frame rate");
                ui.add(egui::Slider::new(&mut self.cfg.max_fps, 5..=60).suffix(" fps"));
                ui.end_row();
                ui.label("Image quality in motion");
                ui.add(egui::Slider::new(&mut self.cfg.jpeg_quality, 30..=95));
                ui.end_row();
                ui.label("Clipboard");
                ui.checkbox(&mut self.cfg.clipboard, "share text in both directions");
                ui.end_row();
                ui.label("Sound");
                ui.checkbox(&mut self.cfg.audio, "send what this computer plays (with the picture)");
                ui.end_row();
                ui.label("Terminal");
                ui.checkbox(&mut self.cfg.admin_terminal, "full control may open it as administrator")
                    .on_hover_text("Windows, with rdesk installed: the signed-in person's administrator rights");
                ui.end_row();
                ui.label("Extra lock");
                ui.checkbox(&mut self.cfg.require_local, "also ask for this computer's password or SSH key")
                    .on_hover_text("Besides the permission from the server (needs a password or key set above)");
                ui.end_row();
                ui.label("Direct access");
                ui.vertical(|ui| {
                    let registered = self.status().registration.is_some() || Registration::load().is_some();
                    let mut on = self.cfg.direct.is_on(registered);
                    let text = "this computer's own password or SSH key open it without the server";
                    if ui.checkbox(&mut on, text).changed() {
                        // Back to the default when it matches it (on only while unregistered).
                        self.cfg.direct.enabled = (on == registered).then_some(on);
                    }
                    if on {
                        ui.horizontal(|ui| {
                            ui.label("from");
                            for (class, label) in [("lan", "local network"), ("tailscale", "Tailscale")] {
                                let mut has = self.cfg.direct.from.iter().any(|c| c == class);
                                if ui.checkbox(&mut has, label).changed() {
                                    self.cfg.direct.from.retain(|c| c != class);
                                    if has {
                                        self.cfg.direct.from.push(class.to_string());
                                    }
                                }
                            }
                        });
                        ui.checkbox(&mut self.cfg.direct.mdns, "announce it on the local network (mDNS)")
                            .on_hover_text(
                                "Others connect with the host ID alone; anyone on the network sees it is here",
                            );
                        if registered {
                            ui.colored_label(
                                ORANGE,
                                "Registered: this is a second way in, besides the server's sharing. The owner sees it \
                                 is on, and these sessions appear in the history.",
                            );
                        }
                    } else if !registered {
                        ui.colored_label(ORANGE, "Not registered and direct access off: nobody can reach it.");
                    }
                });
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button("Save options").clicked() {
                    match admin::save_config(&self.cfg) {
                        Ok(()) => {
                            let was = self.running();
                            self.restart();
                            self.notify(if was { "Saved; host restarted." } else { "Saved." }, false);
                        }
                        Err(e) => self.notify(format!("Saving: {e:#}"), true),
                    }
                }
                if ui.button("Revert").clicked() {
                    self.reload();
                }
            });
            ui.add_space(8.0);
            if ui
                .button("New host identity…")
                .on_hover_text("This computer gets a new host ID; others must be given the new one")
                .clicked()
            {
                self.dialog = Some(Dialog::NewIdentity);
            }
        });
    }

    fn activity_section(&mut self, ui: &mut egui::Ui, st: &HostStatus) {
        egui::CollapsingHeader::new(RichText::new("Activity").strong().size(16.0)).default_open(true).show(ui, |ui| {
            if st.events.is_empty() {
                ui.weak("Nothing yet.");
            }
            let now = unix(SystemTime::now());
            for e in st.events.iter().rev().take(30) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(directory::ago(unix(e.at), now)).weak().small());
                    if e.warning {
                        ui.colored_label(ORANGE, &e.text);
                    } else {
                        ui.label(&e.text);
                    }
                });
            }
        });
    }

    fn diagnostics_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Diagnostics").strong().size(16.0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(self.busy.is_none(), egui::Button::new("Run check")).clicked() {
                    self.busy = Some("diagnose");
                    let tx = self.jobs.0.clone();
                    let ctx = ui.ctx().clone();
                    self.rt.spawn(async move {
                        let _ = tx.send(Job::Diagnose(admin::diagnose().await));
                        ctx.request_repaint();
                    });
                }
                if self.busy == Some("diagnose") {
                    ui.spinner();
                }
            });
            if let Some(lines) = &self.diag {
                egui::Grid::new("diag").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                    for l in lines {
                        ui.label(&l.label);
                        if l.ok {
                            ui.label(&l.value);
                        } else {
                            ui.colored_label(ORANGE, format!("⚠ {}", l.value));
                        }
                        ui.end_row();
                    }
                });
            }
        });
    }

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
        match &mut dialog {
            Dialog::Password { a, b, op_ref, shown, error } => {
                let m = egui::Modal::new(egui::Id::unique("host-password")).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.heading("Password for this computer");
                    ui.label("People who know it can control this computer.");
                    ui.label(
                        "Only a derived record is stored (OPAQUE + Argon2id); the password itself never leaves the client.",
                    );
                    ui.add_space(6.0);
                    egui::Grid::new("pw").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut **a).password(shown.is_none()).desired_width(280.0));
                        ui.end_row();
                        ui.label("Repeat");
                        ui.add(egui::TextEdit::singleline(&mut **b).password(shown.is_none()).desired_width(280.0));
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Generate a strong one").clicked() {
                            let p = admin::generate_password(24);
                            **a = p.clone();
                            **b = p.clone();
                            *shown = Some(p);
                        }
                        if let Some(p) = shown {
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(p.clone());
                            }
                            ui.weak("Save it in 1Password.");
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Or read from 1Password:");
                        ui.add(egui::TextEdit::singleline(op_ref).hint_text("op://Private/rdesk/password").desired_width(200.0));
                        if ui.add_enabled(!op_ref.trim().is_empty(), egui::Button::new("Load")).clicked() {
                            let (tx, r) = (self.jobs.0.clone(), op_ref.trim().to_string());
                            self.rt.spawn(async move {
                                let res = agent::op_read(&r).await.map_err(|e| format!("{e:#}"));
                                let _ = tx.send(Job::OpRead(res));
                            });
                        }
                    });
                    if !a.is_empty() && a.chars().count() < 16 {
                        ui.colored_label(ORANGE, "Short password: prefer 16+ characters (or Generate).");
                    }
                    if let Some(e) = error {
                        ui.colored_label(RED, e.as_str());
                    }
                    ui.add_space(6.0);
                    let mut close = false;
                    ui.horizontal(|ui| {
                        let ready = !a.is_empty() && a == b && self.busy.is_none();
                        if ui.add_enabled(ready, egui::Button::new("Save password")).clicked() {
                            self.busy = Some("password");
                            let (tx, pw) = (self.jobs.0.clone(), a.clone());
                            let ctx = ui.ctx().clone();
                            std::thread::spawn(move || {
                                let _ = tx.send(Job::PasswordSet(admin::set_password(&pw).map_err(|e| format!("{e:#}"))));
                                ctx.request_repaint();
                            });
                        }
                        if self.busy == Some("password") {
                            ui.spinner();
                            ui.label("Deriving (Argon2id)…");
                        }
                        if !a.is_empty() && a != b {
                            ui.colored_label(RED, "The two passwords differ.");
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                    close
                });
                if m.inner || (m.should_close() && self.busy.is_none()) {
                    keep = false;
                }
            }
            Dialog::AddKey { text, agent: agent_keys, error } => {
                let mut added: Option<PublicKey> = None;
                let m = egui::Modal::new(egui::Id::unique("host-addkey")).show(ctx, |ui| {
                    ui.set_width(560.0);
                    ui.heading("Add an SSH key");
                    ui.label("Whoever holds the private key (e.g. in their 1Password) can log in to this computer.");
                    ui.add_space(6.0);
                    ui.label(RichText::new("From the SSH agent on this computer (1Password):").strong());
                    match agent_keys {
                        None => {
                            ui.spinner();
                        }
                        Some(Err(e)) => {
                            ui.weak(format!("Not available: {e}"));
                        }
                        Some(Ok(keys)) => {
                            for k in keys.iter() {
                                let fp = agent::key_fingerprint(k.key_data());
                                let already = self.keys.iter().any(|x| x.key_data() == k.key_data());
                                ui.horizontal(|ui| {
                                    ui.label(format!("{} {}", k.algorithm(), k.comment()));
                                    ui.label(RichText::new(&fp).monospace().small().weak());
                                    if already {
                                        ui.weak("added");
                                    } else if ui.small_button("Add").clicked() {
                                        added = Some(k.clone());
                                    }
                                });
                            }
                        }
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Or paste a public key (from `rdesk agent-keys` on the other computer):")
                            .strong(),
                    );
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .hint_text("ssh-ed25519 AAAA… comment")
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    if let Some(e) = error {
                        ui.colored_label(RED, e.as_str());
                    }
                    let mut close = false;
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!text.trim().is_empty(), egui::Button::new("Add pasted key")).clicked() {
                            match admin::parse_public_key(text) {
                                Ok(k) => added = Some(k),
                                Err(e) => *error = Some(format!("{e:#}")),
                            }
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                    close
                });
                if let Some(k) = added {
                    match admin::add_key(&k) {
                        Ok(true) => {
                            self.notify(format!("Added {}.", agent::key_fingerprint(k.key_data())), false);
                            text.clear();
                        }
                        Ok(false) => self.notify("That key was already authorized.", false),
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
                if m.inner || m.should_close() {
                    keep = false;
                }
            }
            Dialog::RemovePassword => {
                let r = confirm(ctx, "Remove the password?", "Only SSH keys will be able to log in.", "Remove");
                if let Some(yes) = r {
                    if yes {
                        match admin::clear_password() {
                            Ok(()) => self.notify("Password removed.", false),
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
                        self.has_password = admin::has_password();
                    }
                    keep = false;
                }
            }
            Dialog::RemoveKey(fp) => {
                let r = confirm(ctx, "Remove this key?", &format!("{fp} will no longer be able to log in."), "Remove");
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
                let r = confirm(
                    ctx,
                    "Create a new host identity?",
                    "This computer gets a new host ID. Others can no longer reach it until they are given the new ID.",
                    "Create new identity",
                );
                if let Some(yes) = r {
                    if yes {
                        match admin::regenerate_identity() {
                            Ok(fp) => {
                                self.fingerprint = Some(fp);
                                self.restart();
                                self.notify(format!("New host ID: {}", format_host_id(&fp)), false);
                            }
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
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

fn password_dialog() -> Dialog {
    Dialog::Password { a: Default::default(), b: Default::default(), op_ref: String::new(), shown: None, error: None }
}

/// `Some(true)` confirmed, `Some(false)` cancelled, `None` still open.
fn confirm(ctx: &egui::Context, title: &str, text: &str, ok: &str) -> Option<bool> {
    let m = egui::Modal::new(egui::Id::unique(title)).show(ctx, |ui| {
        ui.set_max_width(420.0);
        ui.heading(title);
        ui.label(text);
        ui.add_space(6.0);
        let mut r = None;
        ui.horizontal(|ui| {
            if ui.button(ok).clicked() {
                r = Some(true);
            }
            if ui.button("Cancel").clicked() {
                r = Some(false);
            }
        });
        r
    });
    m.inner.or(m.should_close().then_some(false))
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
