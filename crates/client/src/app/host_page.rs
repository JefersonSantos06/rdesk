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
use rdesk_common::{t, tf};
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
        ui.label(RichText::new(t!("On your rdesk server", "No seu servidor rdesk")).strong().size(16.0));
        let busy = self.reg_job.is_some();
        // The service's files are for administrators only: its status says it instead.
        let registered = match (&self.service, &st.registration) {
            (Some(_), Some(Ok(name))) => Some(name.clone()),
            (Some(_), _) => None,
            (None, _) => Registration::load().map(|r| tf!("{} on {}", "{} em {}", r.name, r.server_name)),
        };
        match registered {
            Some(name) => {
                ui.horizontal(|ui| {
                    ui.label(tf!("Registered as {name}.", "Cadastrado como {name}."));
                    match &st.registration {
                        Some(Ok(_)) if self.running() => {
                            ui.colored_label(GREEN, t!("✔ in touch with the server", "✔ em contato com o servidor"));
                        }
                        Some(Err(e)) if self.running() => {
                            ui.colored_label(ORANGE, e);
                        }
                        _ => {}
                    }
                });
                ui.weak(t!(
                    "Who may reach it (only you, shared, everyone) is set on the Computers page.",
                    "Quem pode acessá-lo (só você, compartilhado, todos) é definido na página Computadores."
                ));
                ui.horizontal(|ui| {
                    if self.confirm_unregister {
                        ui.label(t!(
                            "Unregister? Nobody will reach it through the server any more.",
                            "Descadastrar? Ninguém mais vai acessá-lo pelo servidor."
                        ));
                        if ui.add_enabled(!busy, egui::Button::new(t!("Unregister", "Descadastrar"))).clicked() {
                            self.confirm_unregister = false;
                            self.unregister(&ctx);
                        }
                        if ui.button(t!("Cancel", "Cancelar")).clicked() {
                            self.confirm_unregister = false;
                        }
                    } else if ui.add_enabled(!busy, egui::Button::new(t!("Unregister…", "Descadastrar…"))).clicked()
                    {
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
                    ui.weak(t!(
                        "Or register it yourself below (it becomes yours at once).",
                        "Ou cadastre-o você mesmo abaixo (ele passa a ser seu na hora)."
                    ));
                    ui.add_space(4.0);
                }
                ui.label(t!(
                    "Not registered: others reach it only on the LAN by address. Register it to reach it \
                     from anywhere, with access controlled by the server.",
                    "Não cadastrado: outros só o acessam na rede local, pelo endereço. Cadastre-o para \
                     acessá-lo de qualquer lugar, com o acesso controlado pelo servidor."
                ));
                ui.horizontal(|ui| {
                    ui.label(t!("Name", "Nome"));
                    ui.add(egui::TextEdit::singleline(&mut self.reg_name).desired_width(200.0));
                });
                ui.horizontal(|ui| {
                    ui.label(t!("Who can reach it", "Quem pode acessar"));
                    ui.radio_value(&mut self.reg_visibility, Visibility::Private, t!("only me", "só eu"));
                    ui.radio_value(
                        &mut self.reg_visibility,
                        Visibility::Shared,
                        t!("shared (choose later)", "compartilhado (escolha depois)"),
                    );
                    ui.radio_value(
                        &mut self.reg_visibility,
                        Visibility::Everyone,
                        t!("everyone on the server", "todos do servidor"),
                    );
                });
                ui.horizontal(|ui| {
                    let ok = !busy && !self.reg_name.trim().is_empty();
                    if ui
                        .add_enabled(ok, egui::Button::new(t!("Register this computer", "Cadastrar este computador")))
                        .clicked()
                    {
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
            ui.label(RichText::new(t!("This computer", "Este computador")).strong());
            ui.label(tf!(
                "Run by the rdesk service · host {}.",
                "Executado pelo serviço do rdesk · host {}.",
                self.summary()
            ));
            return;
        }
        if let Some(r) = Registration::load() {
            ui.label(RichText::new(t!("This computer", "Este computador")).strong());
            ui.label(tf!(
                "Reachable as {} on {} · host {}.",
                "Acessível como {} em {} · host {}.",
                r.name,
                r.server_name,
                self.summary()
            ));
            let label = if self.running() {
                t!("Turn the host off", "Desligar o host")
            } else {
                t!("Turn the host on", "Ligar o host")
            };
            if ui.button(label).clicked() {
                self.toggle();
            }
            return;
        }
        if let Some(msg) = st.enrollment.as_ref().filter(|_| self.running()) {
            ui.label(RichText::new(t!("This computer", "Este computador")).strong());
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 380.0).max(0.0) / 2.0);
                ui.spinner();
                ui.label(msg);
            });
            ui.weak(t!(
                "Keep rdesk running (it can stay in the tray) until it is approved.",
                "Deixe o rdesk aberto (ele pode ficar na bandeja) até ser aprovado."
            ));
            if ui.button(t!("Cancel", "Cancelar")).clicked() {
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
        let header = t!("Only share this computer (no sign-in)", "Só compartilhar este computador (sem entrar)");
        egui::CollapsingHeader::new(header).id_salt("host-join").show(ui, |ui| {
            ui.label(t!(
                "For a computer that is only accessed, never used to reach others: it joins the server \
                 above, and its owner approves it in their rdesk app.",
                "Para um computador que só é acessado, nunca usado para acessar outros: ele entra no \
                 servidor acima, e o dono o aprova no app rdesk dele."
            ));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.join_use_key, false, t!("Owner's e-mail", "E-mail do dono"));
                ui.radio_value(&mut self.join_use_key, true, t!("Enrollment key", "Chave de cadastro"));
            });
            let (value, hint) = if self.join_use_key {
                (
                    &mut self.join_key,
                    t!("rdk_… (from Computers → Add a computer)", "rdk_… (em Computadores → Adicionar um computador)"),
                )
            } else {
                (
                    &mut self.join_owner,
                    t!("who will own it, e.g. ana@gmail.com", "quem será o dono, ex.: ana@gmail.com"),
                )
            };
            ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(360.0));
            ui.add(
                egui::TextEdit::singleline(&mut self.reg_name)
                    .hint_text(t!("Name of this computer", "Nome deste computador"))
                    .desired_width(360.0),
            );
            let ready = !server.trim().is_empty() && !value.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new(t!("Ask to join", "Pedir para entrar"))).clicked() {
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
                ui.weak(t!("The server address is the one typed above.", "O endereço do servidor é o digitado acima."));
            }
        });
    }

    fn status_section(&mut self, ui: &mut egui::Ui, st: &HostStatus) {
        let running = self.running();
        let (color, text) = match &st.state {
            _ if !running && !matches!(st.state, HostState::Failed(_)) => (
                Color32::GRAY,
                t!("Off: nobody can connect to this computer", "Desligado: ninguém pode se conectar a este computador")
                    .to_string(),
            ),
            HostState::Listening(_) => {
                (GREEN, t!("On: ready for connections", "Ligado: pronto para conexões").to_string())
            }
            HostState::Starting => (ORANGE, t!("Starting…", "Iniciando…").to_string()),
            HostState::Failed(e) => (RED, tf!("Not running: {e}", "Não está em execução: {e}")),
            HostState::Stopped => (Color32::GRAY, t!("Off", "Desligado").to_string()),
        };
        egui::Frame::group(ui.style()).corner_radius(10.0).inner_margin(14.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 8.0, color);
                ui.label(RichText::new(text).size(17.0).strong());
                if self.service.is_none() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if running { t!("Turn off", "Desligar") } else { t!("Turn on", "Ligar") };
                        if ui.button(RichText::new(label).size(15.0)).clicked() {
                            self.toggle();
                        }
                    });
                }
            });
            if let Some(fp) = self.fingerprint_text() {
                ui.horizontal(|ui| {
                    ui.label(t!("Host ID:", "ID do host:"));
                    ui.label(RichText::new(&fp).monospace().small());
                    if ui
                        .small_button(t!("Copy", "Copiar"))
                        .on_hover_text(t!(
                            "Others add this computer with this ID",
                            "Outros adicionam este computador com este ID"
                        ))
                        .clicked()
                    {
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
                        ui.label(tf!(
                            "Reachable through the relay {url}; direct connections on UDP {port}.",
                            "Acessível pelo relay {url}; conexões diretas na porta UDP {port}."
                        ));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(
                            ORANGE,
                            tf!(
                                "Relay unreachable ({e}); direct connections on UDP {port}.",
                                "Relay inacessível ({e}); conexões diretas na porta UDP {port}."
                            ),
                        );
                    }
                    None => {
                        ui.label(tf!(
                            "No relay: others need this computer's address (LAN or port forwarding, UDP {port}).",
                            "Sem relay: outros precisam do endereço deste computador (rede local ou \
                             redirecionamento de porta, UDP {port})."
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
                t!(
                    "✔ Run by the rdesk service: reachable before sign-in, on the lock screen and through UAC prompts.",
                    "✔ Executado pelo serviço do rdesk: acessível antes do login, na tela de bloqueio e nos avisos \
                     do UAC."
                ),
            );
            ui.horizontal(|ui| {
                if elevated {
                    ui.weak(t!(
                        "You can change this computer's settings below.",
                        "Você pode alterar as configurações deste computador abaixo."
                    ));
                } else {
                    ui.weak(t!(
                        "Changing this computer's settings needs administrator rights.",
                        "Alterar as configurações deste computador exige direitos de administrador."
                    ));
                    if ui.button(t!("Manage as administrator", "Gerenciar como administrador")).clicked() {
                        match autostart::restart_as_admin(&self.exe) {
                            Ok(()) => self.quit_requested = true,
                            Err(e) => self.notify(format!("{e:#}"), true),
                        }
                    }
                }
                if ui
                    .add_enabled(!busy, egui::Button::new(t!("Uninstall…", "Desinstalar…")))
                    .on_hover_text(t!(
                        "This app runs the host again (only while it is open and you are signed in)",
                        "Este app volta a executar o host (só enquanto estiver aberto e você estiver logado)"
                    ))
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
                t!(
                    "⚠ Run by this app: not reachable before you sign in, and the lock screen and UAC prompts \
                     cannot be controlled.",
                    "⚠ Executado por este app: inacessível antes do login, e a tela de bloqueio e os avisos do \
                     UAC não podem ser controlados."
                ),
            );
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "Install rdesk on this computer (recommended)",
                        "Instalar o rdesk neste computador (recomendado)"
                    )),
                )
                .on_hover_text(t!(
                    "Windows asks for administrator permission once",
                    "O Windows pede permissão de administrador uma vez"
                ))
                .clicked()
            {
                self.service_action(ui.ctx(), true);
            }
            if busy {
                ui.spinner();
                ui.label(t!(
                    "Windows may ask for administrator permission…",
                    "O Windows pode pedir permissão de administrador…"
                ));
            }
        });
    }

    fn session_section(&mut self, ui: &mut egui::Ui, st: &HostStatus) {
        ui.label(RichText::new(t!("Current session", "Sessão atual")).strong().size(16.0));
        match &st.session {
            Some(s) if self.running() => {
                ui.horizontal(|ui| {
                    // `ago` already says "5 min ago" / "há 5 min".
                    ui.label(tf!(
                        "{} from {} · {} · since {}{}",
                        "{} de {} · {} · conectado {}{}",
                        s.client,
                        s.peer,
                        s.method,
                        directory::ago(unix(s.since), unix(SystemTime::now())),
                        s.extras()
                    ));
                    if ui
                        .button(t!("Disconnect", "Desconectar"))
                        .on_hover_text(t!(
                            "End it; the other side will not reconnect",
                            "Encerra a sessão; o outro lado não vai reconectar"
                        ))
                        .clicked()
                    {
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
                ui.weak(t!("Nobody is connected.", "Ninguém está conectado."));
            }
        }
    }

    fn login_section(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new(t!("This computer's own password and keys", "Senha e chaves próprias deste computador"))
                .strong()
                .size(16.0),
        );
        ui.weak(t!(
            "Used on the LAN without the server, or as the extra lock (Options) after the server's permission.",
            "Usadas na rede local sem o servidor, ou como cadeado extra (Opções) após a permissão do servidor."
        ));
        if !self.has_password && self.keys.is_empty() && Registration::load().is_none() {
            ui.colored_label(
                ORANGE,
                t!(
                    "Not registered and no password or key: the host cannot start.",
                    "Não cadastrado e sem senha ou chave: o host não pode iniciar."
                ),
            );
        }
        ui.horizontal(|ui| {
            ui.label(t!("Password:", "Senha:"));
            if self.has_password {
                ui.colored_label(GREEN, t!("set", "definida"));
                if ui.button(t!("Change…", "Alterar…")).clicked() {
                    self.dialog = Some(password_dialog());
                }
                if ui.button(t!("Remove", "Remover")).clicked() {
                    self.dialog = Some(Dialog::RemovePassword);
                }
            } else {
                ui.weak(t!("not set", "não definida"));
                if ui.button(t!("Set password…", "Definir senha…")).clicked() {
                    self.dialog = Some(password_dialog());
                }
            }
        });
        ui.label(t!("SSH keys:", "Chaves SSH:"));
        if self.keys.is_empty() {
            ui.weak(t!("   none", "   nenhuma"));
        }
        let mut remove = None;
        for k in &self.keys {
            let fp = agent::key_fingerprint(k.key_data());
            ui.horizontal(|ui| {
                ui.label(format!(
                    "   {} {}",
                    k.algorithm(),
                    if k.comment().is_empty() { t!("(no comment)", "(sem comentário)") } else { k.comment() }
                ));
                ui.label(RichText::new(&fp).monospace().small().weak());
                if ui.small_button(t!("Remove", "Remover")).clicked() {
                    remove = Some(fp.clone());
                }
            });
        }
        if let Some(fp) = remove {
            self.dialog = Some(Dialog::RemoveKey(fp));
        }
        if ui.button(t!("＋ Add SSH key…", "＋ Adicionar chave SSH…")).clicked() {
            self.dialog = Some(Dialog::AddKey { text: String::new(), agent: None, error: None });
            self.load_agent_keys();
        }
    }

    fn options_section(&mut self, ui: &mut egui::Ui) {
        let header = RichText::new(t!("Options", "Opções")).strong().size(16.0);
        egui::CollapsingHeader::new(header).id_salt("host-options").default_open(false).show(ui, |ui| {
            egui::Grid::new("host-opts").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label(t!("Port", "Porta"));
                ui.add(egui::DragValue::new(&mut self.cfg.port).range(1..=65535)).on_hover_text(t!(
                    "UDP port for direct connections (LAN, port forwarding)",
                    "Porta UDP para conexões diretas (rede local, redirecionamento de porta)"
                ));
                ui.end_row();
                ui.label(t!("Received files", "Arquivos recebidos"));
                ui.horizontal(|ui| {
                    ui.label(self.cfg.inbox_dir().display().to_string());
                    if ui.small_button(t!("Change…", "Alterar…")).clicked()
                        && let Some(d) = rfd::FileDialog::new().set_directory(self.cfg.inbox_dir()).pick_folder()
                    {
                        self.cfg.inbox_dir = Some(d);
                    }
                });
                ui.end_row();
                ui.label(t!("Max frame rate", "Taxa de quadros máxima"));
                ui.add(egui::Slider::new(&mut self.cfg.max_fps, 5..=60).suffix(" fps"));
                ui.end_row();
                ui.label(t!("Image quality in motion", "Qualidade da imagem em movimento"));
                ui.add(egui::Slider::new(&mut self.cfg.jpeg_quality, 30..=95));
                ui.end_row();
                ui.label(t!("Clipboard", "Área de transferência"));
                ui.checkbox(
                    &mut self.cfg.clipboard,
                    t!("share text in both directions", "compartilhar texto nos dois sentidos"),
                );
                ui.end_row();
                // Administrator rights in a terminal exist only on Windows.
                if cfg!(windows) {
                    ui.label(t!("Terminal", "Terminal"));
                    ui.checkbox(
                        &mut self.cfg.admin_terminal,
                        t!(
                            "full control may open it as administrator",
                            "controle total pode abri-lo como administrador"
                        ),
                    )
                    .on_hover_text(t!(
                        "With rdesk installed: the signed-in person's administrator rights",
                        "Com o rdesk instalado: com os direitos de administrador de quem estiver logado"
                    ));
                    ui.end_row();
                }
                ui.label(t!("Extra lock", "Cadeado extra"));
                ui.checkbox(
                    &mut self.cfg.require_local,
                    t!(
                        "also ask for this computer's password or SSH key",
                        "pedir também a senha ou chave SSH deste computador"
                    ),
                )
                .on_hover_text(t!(
                    "Besides the permission from the server (needs a password or key set above)",
                    "Além da permissão do servidor (precisa de uma senha ou chave definida acima)"
                ));
                ui.end_row();
                ui.label(t!("Direct access", "Acesso direto"));
                ui.vertical(|ui| {
                    let registered = self.status().registration.is_some() || Registration::load().is_some();
                    let mut on = self.cfg.direct.is_on(registered);
                    let text = t!(
                        "this computer's own password or SSH key open it without the server",
                        "a senha ou chave SSH própria deste computador o abrem sem o servidor"
                    );
                    if ui.checkbox(&mut on, text).changed() {
                        // Back to the default when it matches it (on only while unregistered).
                        self.cfg.direct.enabled = (on == registered).then_some(on);
                    }
                    if on {
                        ui.horizontal(|ui| {
                            ui.label(t!("from", "de"));
                            // The class ids go to the config; only the labels are translated.
                            let classes = [("lan", t!("local network", "rede local")), ("tailscale", "Tailscale")];
                            for (class, label) in classes {
                                let mut has = self.cfg.direct.from.iter().any(|c| c == class);
                                if ui.checkbox(&mut has, label).changed() {
                                    self.cfg.direct.from.retain(|c| c != class);
                                    if has {
                                        self.cfg.direct.from.push(class.to_string());
                                    }
                                }
                            }
                        });
                        ui.checkbox(
                            &mut self.cfg.direct.mdns,
                            t!("announce it on the local network (mDNS)", "anunciá-lo na rede local (mDNS)"),
                        )
                        .on_hover_text(t!(
                            "Others connect with the host ID alone; anyone on the network sees it is here",
                            "Outros se conectam só com o ID do host; qualquer um na rede vê que ele está aqui"
                        ));
                        if registered {
                            ui.colored_label(
                                ORANGE,
                                t!(
                                    "Registered: this is a second way in, besides the server's sharing. The owner \
                                     sees it is on, and these sessions appear in the history.",
                                    "Cadastrado: este é um segundo acesso, além do compartilhamento do servidor. O \
                                     dono vê que está ligado, e essas sessões aparecem no histórico."
                                ),
                            );
                        }
                    } else if !registered {
                        ui.colored_label(
                            ORANGE,
                            t!(
                                "Not registered and direct access off: nobody can reach it.",
                                "Não cadastrado e acesso direto desligado: ninguém consegue acessá-lo."
                            ),
                        );
                    }
                });
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button(t!("Save options", "Salvar opções")).clicked() {
                    match admin::save_config(&self.cfg) {
                        Ok(()) => {
                            let was = self.running();
                            self.restart();
                            let text = if was {
                                t!("Saved; host restarted.", "Salvo; host reiniciado.")
                            } else {
                                t!("Saved.", "Salvo.")
                            };
                            self.notify(text, false);
                        }
                        Err(e) => self.notify(tf!("Saving: {e:#}", "Erro ao salvar: {e:#}"), true),
                    }
                }
                if ui.button(t!("Revert", "Desfazer")).clicked() {
                    self.reload();
                }
            });
            ui.add_space(8.0);
            if ui
                .button(t!("New host identity…", "Nova identidade do host…"))
                .on_hover_text(t!(
                    "This computer gets a new host ID; others must be given the new one",
                    "Este computador ganha um novo ID do host; os outros precisam receber o novo"
                ))
                .clicked()
            {
                self.dialog = Some(Dialog::NewIdentity);
            }
        });
    }

    fn activity_section(&mut self, ui: &mut egui::Ui, st: &HostStatus) {
        let header = RichText::new(t!("Activity", "Atividade")).strong().size(16.0);
        egui::CollapsingHeader::new(header).id_salt("host-activity").default_open(true).show(ui, |ui| {
            if st.events.is_empty() {
                ui.weak(t!("Nothing yet.", "Nada ainda."));
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
        let header = RichText::new(t!("Diagnostics", "Diagnóstico")).strong().size(16.0);
        egui::CollapsingHeader::new(header).id_salt("host-diagnostics").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(self.busy.is_none(), egui::Button::new(t!("Run check", "Verificar"))).clicked() {
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
                    ui.heading(t!("Password for this computer", "Senha deste computador"));
                    ui.label(t!(
                        "People who know it can control this computer.",
                        "Quem souber a senha pode controlar este computador."
                    ));
                    ui.label(t!(
                        "Only a derived record is stored (OPAQUE + Argon2id); the password itself never leaves the client.",
                        "Só um registro derivado é guardado (OPAQUE + Argon2id); a senha em si nunca sai do cliente."
                    ));
                    ui.add_space(6.0);
                    egui::Grid::new("pw").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label(t!("Password", "Senha"));
                        ui.add(egui::TextEdit::singleline(&mut **a).password(shown.is_none()).desired_width(280.0));
                        ui.end_row();
                        ui.label(t!("Repeat", "Repetir"));
                        ui.add(egui::TextEdit::singleline(&mut **b).password(shown.is_none()).desired_width(280.0));
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        if ui.button(t!("Generate a strong one", "Gerar uma forte")).clicked() {
                            let p = admin::generate_password(24);
                            **a = p.clone();
                            **b = p.clone();
                            *shown = Some(p);
                        }
                        if let Some(p) = shown {
                            if ui.small_button(t!("Copy", "Copiar")).clicked() {
                                ui.ctx().copy_text(p.clone());
                            }
                            ui.weak(t!("Save it in 1Password.", "Salve-a no 1Password."));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(t!("Or read from 1Password:", "Ou leia do 1Password:"));
                        ui.add(egui::TextEdit::singleline(op_ref).hint_text("op://Private/rdesk/password").desired_width(200.0));
                        if ui.add_enabled(!op_ref.trim().is_empty(), egui::Button::new(t!("Load", "Carregar"))).clicked() {
                            let (tx, r) = (self.jobs.0.clone(), op_ref.trim().to_string());
                            self.rt.spawn(async move {
                                let res = agent::op_read(&r).await.map_err(|e| format!("{e:#}"));
                                let _ = tx.send(Job::OpRead(res));
                            });
                        }
                    });
                    if !a.is_empty() && a.chars().count() < 16 {
                        ui.colored_label(
                            ORANGE,
                            t!(
                                "Short password: prefer 16+ characters (or Generate).",
                                "Senha curta: prefira 16+ caracteres (ou gere uma forte)."
                            ),
                        );
                    }
                    if let Some(e) = error {
                        ui.colored_label(RED, e.as_str());
                    }
                    ui.add_space(6.0);
                    let mut close = false;
                    ui.horizontal(|ui| {
                        let ready = !a.is_empty() && a == b && self.busy.is_none();
                        if ui.add_enabled(ready, egui::Button::new(t!("Save password", "Salvar senha"))).clicked() {
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
                            ui.label(t!("Deriving (Argon2id)…", "Derivando (Argon2id)…"));
                        }
                        if !a.is_empty() && a != b {
                            ui.colored_label(RED, t!("The two passwords differ.", "As duas senhas são diferentes."));
                        }
                        if ui.button(t!("Cancel", "Cancelar")).clicked() {
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
                    ui.heading(t!("Add an SSH key", "Adicionar uma chave SSH"));
                    ui.label(t!(
                        "Whoever holds the private key (e.g. in their 1Password) can log in to this computer.",
                        "Quem tiver a chave privada (ex.: no 1Password) pode entrar neste computador."
                    ));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(t!(
                            "From the SSH agent on this computer (1Password):",
                            "Do agente SSH deste computador (1Password):"
                        ))
                        .strong(),
                    );
                    match agent_keys {
                        None => {
                            ui.spinner();
                        }
                        Some(Err(e)) => {
                            ui.weak(tf!("Not available: {e}", "Indisponível: {e}"));
                        }
                        Some(Ok(keys)) => {
                            for k in keys.iter() {
                                let fp = agent::key_fingerprint(k.key_data());
                                let already = self.keys.iter().any(|x| x.key_data() == k.key_data());
                                ui.horizontal(|ui| {
                                    ui.label(format!("{} {}", k.algorithm(), k.comment()));
                                    ui.label(RichText::new(&fp).monospace().small().weak());
                                    if already {
                                        ui.weak(t!("added", "adicionada"));
                                    } else if ui.small_button(t!("Add", "Adicionar")).clicked() {
                                        added = Some(k.clone());
                                    }
                                });
                            }
                        }
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(t!(
                            "Or paste a public key (from `rdesk agent-keys` on the other computer):",
                            "Ou cole uma chave pública (de `rdesk agent-keys` no outro computador):"
                        ))
                        .strong(),
                    );
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .hint_text(t!("ssh-ed25519 AAAA… comment", "ssh-ed25519 AAAA… comentário"))
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    if let Some(e) = error {
                        ui.colored_label(RED, e.as_str());
                    }
                    let mut close = false;
                    ui.horizontal(|ui| {
                        let add = egui::Button::new(t!("Add pasted key", "Adicionar chave colada"));
                        if ui.add_enabled(!text.trim().is_empty(), add).clicked() {
                            match admin::parse_public_key(text) {
                                Ok(k) => added = Some(k),
                                Err(e) => *error = Some(format!("{e:#}")),
                            }
                        }
                        if ui.button(t!("Close", "Fechar")).clicked() {
                            close = true;
                        }
                    });
                    close
                });
                if let Some(k) = added {
                    match admin::add_key(&k) {
                        Ok(true) => {
                            let fp = agent::key_fingerprint(k.key_data());
                            self.notify(tf!("Added {fp}.", "Chave {fp} adicionada."), false);
                            text.clear();
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
                if m.inner || m.should_close() {
                    keep = false;
                }
            }
            Dialog::RemovePassword => {
                let r = confirm(
                    ctx,
                    "remove-password",
                    t!("Remove the password?", "Remover a senha?"),
                    t!("Only SSH keys will be able to log in.", "Só chaves SSH vão poder entrar."),
                    t!("Remove", "Remover"),
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
            Dialog::RemoveKey(fp) => {
                let r = confirm(
                    ctx,
                    "remove-key",
                    t!("Remove this key?", "Remover esta chave?"),
                    &tf!("{fp} will no longer be able to log in.", "{fp} não vai mais poder entrar."),
                    t!("Remove", "Remover"),
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
                let r = confirm(
                    ctx,
                    "new-identity",
                    t!("Create a new host identity?", "Criar uma nova identidade do host?"),
                    t!(
                        "This computer gets a new host ID. Others can no longer reach it until they are given the new ID.",
                        "Este computador ganha um novo ID do host. Os outros não conseguem mais acessá-lo até receberem \
                         o novo ID."
                    ),
                    t!("Create new identity", "Criar nova identidade"),
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
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }
}

fn password_dialog() -> Dialog {
    Dialog::Password { a: Default::default(), b: Default::default(), op_ref: String::new(), shown: None, error: None }
}

/// `Some(true)` confirmed, `Some(false)` cancelled, `None` still open. `id` names the dialog
/// (the texts change with the language).
fn confirm(ctx: &egui::Context, id: &str, title: &str, text: &str, ok: &str) -> Option<bool> {
    let m = egui::Modal::new(egui::Id::unique(("host-confirm", id))).show(ctx, |ui| {
        ui.set_max_width(420.0);
        ui.heading(title);
        ui.label(text);
        ui.add_space(6.0);
        let mut r = None;
        ui.horizontal(|ui| {
            if ui.button(ok).clicked() {
                r = Some(true);
            }
            if ui.button(t!("Cancel", "Cancelar")).clicked() {
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
