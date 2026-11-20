//! The desktop app: one window with its pages (Computers, This computer, Settings, and
//! Admin for system admins), a tray icon, and the built-in host. Nothing but the sign-in
//! screen is available until the user signs in to the rdesk server. Closing the window
//! keeps it in the tray, like AnyDesk; a second launch just shows the running window.

mod access;
mod account;
mod add_computer;
mod admin;
mod autostart;
mod computers;
#[cfg(test)]
pub(crate) mod gallery;
mod groups;
#[cfg(feature = "host")]
mod host_page;
mod icon;
mod instance;
mod settings;
mod tray;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use rdesk_common::t;

use crate::ui::{self, Toasts, Trailing, icons, space};

pub use autostart::{Autostart, apply as apply_autostart, current as current_autostart};

pub struct AppOpts {
    /// The rdesk binary, re-run as `rdesk connect …` for each session window.
    pub exe: PathBuf,
    /// Start hidden in the tray (autostart).
    pub minimized: bool,
    /// Replace a running instance instead of handing over to it (restart as admin).
    pub takeover: bool,
}

pub(crate) enum AppCmd {
    Show,
    Quit,
    ToggleHost,
    CopyFingerprint,
}

/// The pages of the window. A page asks to show another by setting its `go_to`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Page {
    Computers,
    #[cfg(feature = "host")]
    ThisComputer,
    Groups,
    Settings,
    Admin,
}

pub fn run(opts: AppOpts) -> Result<()> {
    let (tx, rx) = channel::<AppCmd>();
    let ctx_slot: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    let guard = {
        let (tx, ctx_slot) = (tx.clone(), ctx_slot.clone());
        instance::acquire(opts.takeover, move |m| {
            let _ = tx.send(match m {
                instance::Msg::Show => AppCmd::Show,
                instance::Msg::Quit => AppCmd::Quit,
            });
            if let Some(c) = ctx_slot.get() {
                c.request_repaint();
            }
        })?
    };
    let Some(guard) = guard else {
        // Another copy is running; it has been asked to show its window.
        return Ok(());
    };

    let settings = computers::load_client_config().app;
    let hidden = opts.minimized || settings.start_minimized;
    let size = 64;
    let icon = egui::IconData { rgba: icon::rgba(size), width: size, height: size };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rdesk")
            .with_app_id("rdesk")
            .with_icon(Arc::new(icon))
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 600.0])
            .with_visible(!hidden),
        ..Default::default()
    };
    let exe = opts.exe.clone();
    eframe::run_native(
        "rdesk",
        native,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            ui::install(&ctx);
            let _ = ctx_slot.set(ctx.clone());
            let tray = match tray::Tray::create(tx.clone(), ctx.clone(), cfg!(feature = "host")) {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!("no tray icon: {e:#}");
                    None
                }
            };
            if tray.is_none() && hidden {
                // Without a tray there would be no way to bring the window back.
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            }
            Ok(Box::new(App::new(exe, ctx, tray, rx, guard, hidden)))
        }),
    )
    .map_err(|e| anyhow!("app window: {e}"))
}

struct App {
    page: Page,
    account: account::Account,
    admin: admin::AdminPage,
    groups: groups::GroupsPage,
    computers: computers::Computers,
    #[cfg(feature = "host")]
    host: host_page::HostPage,
    settings: settings::SettingsPage,
    tray: Option<tray::Tray>,
    toasts: Toasts,
    cmds: Receiver<AppCmd>,
    quitting: bool,
    visible: bool,
    /// Used without a server (only computers on this network), until someone signs in.
    local_mode: bool,
    /// This program file and when it was written: the Windows service replaces it with a
    /// new version, and the app then starts again from it.
    exe: PathBuf,
    exe_written: Option<std::time::SystemTime>,
    exe_checked: Instant,
    _instance: instance::Guard,
}

fn written(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Remember the choice of using the app without a server.
fn set_local_only(on: bool) {
    let mut cfg: rdesk_common::config::ClientConfig =
        rdesk_common::config::load_toml(rdesk_common::config::CLIENT_CONFIG).unwrap_or_default();
    if cfg.app.local_only != on {
        cfg.app.local_only = on;
        let _ = rdesk_common::config::ensure_config_dir()
            .and_then(|_| rdesk_common::config::save_toml(rdesk_common::config::CLIENT_CONFIG, &cfg));
    }
}

impl App {
    fn new(
        exe: PathBuf,
        ctx: egui::Context,
        tray: Option<tray::Tray>,
        cmds: Receiver<AppCmd>,
        guard: instance::Guard,
        hidden: bool,
    ) -> Self {
        let shared = Arc::new(Mutex::new(computers::Snapshot::default()));
        let (wtx, wrx) = tokio::sync::mpsc::unbounded_channel();
        computers::spawn_worker(shared.clone(), wrx, ctx.clone());
        let account = account::Account::new(&ctx);
        let local_mode = !account.signed_in()
            && rdesk_common::config::load_toml::<rdesk_common::config::ClientConfig>(
                rdesk_common::config::CLIENT_CONFIG,
            )
            .map(|c| c.app.local_only)
            .unwrap_or(false);
        #[cfg(feature = "host")]
        let host = {
            let mut h = host_page::HostPage::new(exe.clone(), ctx.clone());
            // A registered computer has its own credential: it stays reachable with nobody signed in
            // (and one asking a server to add it keeps asking). Without a server, the host
            // serves the local network.
            if (account.signed_in() || local_mode || host_page::runs_signed_out()) && host_page::remembered_enabled() {
                h.start();
            }
            h
        };
        Self {
            page: Page::Computers,
            account,
            admin: admin::AdminPage::new(),
            groups: groups::GroupsPage::new(),
            computers: computers::Computers::new(exe.clone(), shared, wtx),
            #[cfg(feature = "host")]
            host,
            settings: settings::SettingsPage::new(exe.clone()),
            visible: !hidden || tray.is_none(),
            local_mode,
            exe_written: written(&exe),
            exe_checked: Instant::now(),
            exe: exe.clone(),
            tray,
            toasts: Toasts::default(),
            cmds,
            quitting: false,
            _instance: guard,
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        #[cfg(feature = "host")]
        self.host.shutdown();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn handle(&mut self, cmd: AppCmd, ctx: &egui::Context) {
        match cmd {
            AppCmd::Show => self.show_window(ctx),
            AppCmd::Quit => self.quit(ctx),
            AppCmd::ToggleHost =>
            {
                #[cfg(feature = "host")]
                if self.account.signed_in() || self.local_mode || host_page::runs_signed_out() {
                    self.host.toggle();
                } else {
                    self.show_window(ctx);
                }
            }
            AppCmd::CopyFingerprint => {
                #[cfg(feature = "host")]
                if let Some(fp) = self.host.fingerprint_text() {
                    // The window may be hidden: go straight to the system clipboard.
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(fp)) {
                        Ok(()) => {}
                        Err(e) => tracing::warn!("clipboard: {e}"),
                    }
                }
            }
        }
    }

    /// Messages the pages left since the last frame go to the corner.
    fn collect_toasts(&mut self) {
        let mut all = Vec::new();
        all.extend(self.computers.toast.take());
        #[cfg(feature = "host")]
        all.extend(self.host.toast.take());
        all.extend(self.settings.toast.take());
        all.extend(self.account.toast.take());
        all.extend(self.admin.toast.take());
        all.extend(self.groups.toast.take());
        all.sort_by_key(|t| t.2);
        for (text, error, _) in all {
            self.toasts.push(text, error);
        }
    }

    fn go(&mut self, page: Page, ctx: &egui::Context) {
        self.page = page;
        match page {
            Page::Settings => self.settings.refresh(),
            Page::Admin => self.admin.reload(ctx, self.account.session()),
            Page::Groups => self.groups.reload(ctx),
            _ => {}
        }
    }

    /// What the sidebar shows now.
    fn sidebar_model(&self) -> Sidebar {
        #[cfg(feature = "host")]
        let host = {
            let p = ui::palette_for(true);
            let st = self.host.status();
            let color = if !self.host.running() {
                p.muted
            } else if st.session.is_some() {
                p.warning
            } else {
                p.success
            };
            Some((color, self.host.summary()))
        };
        #[cfg(not(feature = "host"))]
        let host = None;
        Sidebar {
            page: self.page,
            signed_in: self.account.signed_in(),
            admin: self.account.is_admin(),
            pending: self.computers.pending_requests(),
            host,
            account: self.account.session().map(|s| (s.user.email.clone(), s.server_name.clone())),
            sessions: self.computers.open_sessions(),
        }
    }
}

/// The sidebar's contents (kept apart from the pages so it can be drawn on its own).
pub(super) struct Sidebar {
    pub page: Page,
    pub signed_in: bool,
    pub admin: bool,
    /// Computers waiting for this user's approval.
    pub pending: usize,
    /// The host's dot color and summary ("on", "in use by ana"…).
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub host: Option<(egui::Color32, String)>,
    /// Signed in as (e-mail, server name).
    pub account: Option<(String, String)>,
    /// Session windows open from this app.
    pub sessions: usize,
}

/// The sidebar; returns the page clicked.
pub(super) fn sidebar(ui: &mut egui::Ui, m: &Sidebar) -> Option<Page> {
    let p = ui::pal(ui);
    let mut go = None;
    ui.add_space(space::S);
    ui.horizontal(|ui| {
        ui.add_space(space::S);
        ui.label(egui::RichText::new("rdesk").size(22.0).strong());
    });
    ui.add_space(space::M);
    let mut item = |ui: &mut egui::Ui, page: Page, icon: &str, label: &str, trailing: Trailing| {
        if ui::nav_item(ui, icon, label, m.page == page, trailing).clicked() {
            go = Some(page);
        }
    };
    item(ui, Page::Computers, icons::DESKTOP, t!("Computers", "Computadores"), Trailing::Count(m.pending));
    #[cfg(feature = "host")]
    {
        let (dot, _) = m.host.clone().unwrap_or((p.muted, String::new()));
        item(ui, Page::ThisComputer, icons::HOUSE, t!("This computer", "Este computador"), Trailing::Dot(dot));
    }
    if m.signed_in {
        item(ui, Page::Groups, icons::USERS_THREE, t!("Groups", "Grupos"), Trailing::None);
    }
    if m.admin {
        item(ui, Page::Admin, icons::SHIELD_CHECK, "Admin", Trailing::None);
    }

    // The bottom: settings, then who is signed in.
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        ui.add_space(space::S);
        let account = egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| match &m.account {
                Some((email, server)) => {
                    ui::avatar(ui, email, 28.0);
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(email.as_str()).truncate());
                        ui.add(egui::Label::new(ui::muted(server.as_str()).size(ui::size::SMALL)).truncate());
                    });
                }
                None => {
                    ui.label(egui::RichText::new(icons::WIFI_SLASH).size(ui::size::ICON).color(p.muted));
                    ui.add(
                        egui::Label::new(ui::muted(t!("Without a server", "Sem servidor")).size(ui::size::SMALL))
                            .truncate(),
                    );
                }
            });
        });
        if account.response.interact(egui::Sense::click()).on_hover_text(t!("Account", "Conta")).clicked() {
            go = Some(Page::Settings);
        }
        ui.separator();
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            // Top-down again, at the end of the bottom-up space.
            let h = 36.0 * if m.sessions > 0 { 2.0 } else { 1.0 } + space::S;
            ui.add_space((ui.available_height() - h).max(0.0));
            if m.sessions > 0 {
                let text = rdesk_common::tf!("{} open session(s)", "{} sessão(ões) aberta(s)", m.sessions);
                ui.horizontal(|ui| {
                    ui.add_space(space::S);
                    ui.label(egui::RichText::new(icons::MONITOR).color(p.accent));
                    ui.add(egui::Label::new(ui::muted(text).size(ui::size::SMALL)).truncate());
                });
            }
            if ui::nav_item(
                ui,
                icons::GEAR_SIX,
                t!("Settings", "Configurações"),
                m.page == Page::Settings,
                Trailing::None,
            )
            .clicked()
            {
                go = Some(Page::Settings);
            }
        });
    });
    go
}

/// The sidebar's width: room for the longest Portuguese label plus a count.
pub(super) const SIDEBAR_WIDTH: f32 = 220.0;

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(cmd) = self.cmds.try_recv() {
            self.handle(cmd, ctx);
        }
        self.account.tick();
        self.computers.tick();
        self.settings.tick();
        self.admin.tick(ctx, self.account.session());
        self.groups.tick(ctx);
        if std::mem::take(&mut self.account.changed) {
            // A different user (or none): their list, not the previous one's.
            self.computers.refresh();
            // Signing in ends the local-network-only mode (signing out shows the sign-in page).
            if self.account.signed_in() && self.local_mode {
                self.local_mode = false;
                set_local_only(false);
            }
            if !self.account.signed_in() && self.page == Page::Groups {
                self.page = Page::Computers;
            }
            if !self.account.is_admin() && self.page == Page::Admin {
                self.page = Page::Computers;
            }
            // An unregistered host depends on someone being signed in; a registered one does not.
            #[cfg(feature = "host")]
            {
                if self.account.signed_in() || self.local_mode {
                    if host_page::remembered_enabled() && !self.host.running() {
                        self.host.start();
                    }
                } else if !host_page::runs_signed_out() {
                    self.host.suspend();
                }
            }
        }
        #[cfg(feature = "host")]
        {
            self.host.tick();
            if self.host.quit_requested {
                self.host.quit_requested = false;
                self.quit(ctx);
            }
            let (summary, on) = (self.host.summary(), self.host.running());
            if let Some(t) = &mut self.tray {
                t.update(&summary, on);
            }
        }

        // Updated underneath (the service installed a new version): start again from it, once no
        // session window of this copy is open.
        if self.exe_checked.elapsed() >= Duration::from_secs(60) {
            self.exe_checked = Instant::now();
            let now = written(&self.exe);
            if now.is_some() && now != self.exe_written && self.computers.open_sessions() == 0 {
                let mut cmd = std::process::Command::new(&self.exe);
                cmd.args(["ui", "--takeover"]);
                if !self.visible {
                    cmd.arg("--minimized");
                }
                match cmd.spawn() {
                    Ok(_) => {
                        tracing::info!("this program was updated: starting the new version");
                        self.quit(ctx);
                    }
                    Err(e) => {
                        tracing::warn!("starting the updated program: {e}");
                        self.exe_written = now;
                    }
                }
            }
        }

        // The close button hides to the tray (unless quitting or there is no tray).
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting
            && self.tray.is_some()
            && self.settings.app_settings().close_to_tray
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.visible = false;
        }
        // Keep polling sessions/jobs even while hidden.
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.collect_toasts();
        let page_frame = |p: &ui::Palette| {
            egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(space::XL as i8, space::L as i8 + 4))
        };
        let p = ui::pal(ui);
        if !self.account.signed_in() && !self.local_mode {
            #[cfg(feature = "host")]
            let host = &mut self.host;
            let mut go_local = false;
            egui::CentralPanel::default().frame(page_frame(&p)).show(ui, |ui| {
                go_local = self.account.login_screen(ui, |_ui, _server| {
                    #[cfg(feature = "host")]
                    host.join_section(_ui, _server);
                });
            });
            if go_local {
                self.local_mode = true;
                set_local_only(true);
                self.page = Page::Computers;
                #[cfg(feature = "host")]
                if host_page::remembered_enabled() && !self.host.running() {
                    self.host.start();
                }
            }
            self.toasts.show(&ctx);
            return;
        }
        let model = self.sidebar_model();
        let clicked = egui::Panel::left("nav")
            .exact_size(SIDEBAR_WIDTH)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.bg_alt).inner_margin(egui::Margin::same(space::S as i8)))
            .show(ui, |ui| sidebar(ui, &model))
            .inner;
        if let Some(page) = clicked {
            self.go(page, &ctx);
        }
        let mut sign_in = false;
        egui::CentralPanel::default().frame(page_frame(&p)).show(ui, |ui| match self.page {
            Page::Computers => self.computers.show(ui, self.account.signed_in()),
            #[cfg(feature = "host")]
            Page::ThisComputer => self.host.show(ui),
            Page::Settings => sign_in = self.settings.show(ui, &mut self.account, self.local_mode),
            Page::Admin => self.admin.show(ui, self.account.session()),
            Page::Groups => self.groups.show(ui),
        });
        if sign_in {
            // Leave the local-network mode: the sign-in screen shows.
            self.local_mode = false;
            set_local_only(false);
        }
        // A page asked for another one.
        let wanted = self.computers.go_to.take();
        #[cfg(feature = "host")]
        let wanted = wanted.or(self.host.go_to.take());
        if let Some(page) = wanted {
            self.go(page, &ctx);
        }
        self.toasts.show(&ctx);
    }

    fn on_exit(&mut self) {
        #[cfg(feature = "host")]
        self.host.shutdown();
    }
}
