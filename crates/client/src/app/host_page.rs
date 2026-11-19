//! "This computer": the built-in host. Turn it on/off, see who is connected and what
//! happened, manage how people log in (password, SSH keys), options and diagnostics.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use egui::{Color32, RichText};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, HostConfig, format_host_id};
use rdesk_common::directory;
use rdesk_host::admin::{self, CheckLine};
use rdesk_host::{HostHandle, HostState, HostStatus, Reporter, RunOpts};
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
    rt: Arc<tokio::runtime::Runtime>,
    reporter: Reporter,
    handle: Option<HostHandle>,
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
            rt,
            reporter,
            handle: None,
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
        p.reload();
        p
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
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// One word for the tray: "on", "off", "in use", "waiting", "error".
    pub(super) fn summary(&self) -> String {
        let st = self.status();
        match (&st.state, &st.session) {
            (_, Some(s)) if self.running() => format!("in use by {}", s.client),
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
        if self.running() {
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

    pub(super) fn tick(&mut self) {
        while let Ok(job) = self.jobs.1.try_recv() {
            self.busy = None;
            match job {
                Job::PasswordSet(Ok(())) => {
                    self.has_password = true;
                    self.dialog = None;
                    self.notify("Password saved.", false);
                    // A host waiting for a first login method can start now.
                    if matches!(self.status().state, HostState::Failed(_)) && remembered_enabled() {
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if running { "Turn off" } else { "Turn on" };
                    if ui.button(RichText::new(label).size(15.0)).clicked() {
                        self.toggle();
                    }
                });
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
                ui.horizontal(|ui| {
                    if rdesk_host::elevation::is_elevated() {
                        ui.colored_label(GREEN, "✔ Running as administrator: admin windows can be controlled.");
                    } else {
                        ui.colored_label(
                            ORANGE,
                            "⚠ Not running as administrator: Task Manager, installers and admin windows ignore remote input.",
                        );
                        if ui.button("Restart as administrator").clicked() {
                            match autostart::restart_as_admin(&self.exe) {
                                Ok(()) => self.quit_requested = true,
                                Err(e) => self.notify(format!("{e:#}"), true),
                            }
                        }
                    }
                });
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
                    if ui.button("Disconnect").on_hover_text("End it; the other side will not reconnect").clicked()
                        && let Some(h) = &self.handle
                    {
                        h.disconnect();
                    }
                });
            }
            _ => {
                ui.weak("Nobody is connected.");
            }
        }
    }

    fn login_section(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Who can log in").strong().size(16.0));
        if !self.has_password && self.keys.is_empty() {
            ui.colored_label(ORANGE, "Nobody yet: set a password or add an SSH key, or the host will not start.");
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
                    if matches!(self.status().state, HostState::Failed(_)) && remembered_enabled() {
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
