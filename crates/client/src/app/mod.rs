//! The desktop app: one window with its pages (Computers, This computer, Settings, and
//! Admin for system admins), a tray icon, and the built-in host. Nothing but the sign-in
//! screen is available until the user signs in to the rdesk server. Closing the window
//! keeps it in the tray, like AnyDesk; a second launch just shows the running window.

mod account;
mod admin;
mod autostart;
mod computers;
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
use egui::{Color32, RichText};

pub use autostart::{Autostart, apply as apply_autostart};

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

#[derive(Clone, Copy, PartialEq)]
enum Page {
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
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([600.0, 420.0])
            .with_visible(!hidden),
        ..Default::default()
    };
    let exe = opts.exe.clone();
    eframe::run_native(
        "rdesk",
        native,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
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
    cmds: Receiver<AppCmd>,
    quitting: bool,
    visible: bool,
    _instance: instance::Guard,
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
        #[cfg(feature = "host")]
        let host = {
            let mut h = host_page::HostPage::new(exe.clone(), ctx.clone());
            // A registered computer has its own credential: it stays reachable with nobody signed in
            // (and one asking a server to add it keeps asking).
            if (account.signed_in() || host_page::runs_signed_out()) && host_page::remembered_enabled() {
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
            settings: settings::SettingsPage::new(exe),
            visible: !hidden || tray.is_none(),
            tray,
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
                if self.account.signed_in() || host_page::runs_signed_out() {
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

    fn toast(&self) -> Option<&(String, bool, Instant)> {
        let mut all: Vec<&(String, bool, Instant)> = Vec::new();
        all.extend(self.computers.toast.as_ref());
        #[cfg(feature = "host")]
        all.extend(self.host.toast.as_ref());
        all.extend(self.settings.toast.as_ref());
        all.extend(self.account.toast.as_ref());
        all.extend(self.admin.toast.as_ref());
        all.extend(self.groups.toast.as_ref());
        all.into_iter().filter(|t| t.2.elapsed() < Duration::from_secs(12)).max_by_key(|t| t.2)
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("rdesk").strong().size(22.0));
            ui.add_space(16.0);
            let ctx = ui.ctx().clone();
            let mut clicked = None;
            let mut tab = |ui: &mut egui::Ui, page: Page, label: &str| {
                if ui.selectable_label(self.page == page, RichText::new(label).size(15.0)).clicked() {
                    clicked = Some(page);
                }
            };
            tab(ui, Page::Computers, "🖥  Computers");
            #[cfg(feature = "host")]
            tab(ui, Page::ThisComputer, "🏠  This computer");
            tab(ui, Page::Groups, "👥  Groups");
            tab(ui, Page::Settings, "⚙  Settings");
            if self.account.is_admin() {
                tab(ui, Page::Admin, "🛡  Admin");
            }
            if let Some(page) = clicked {
                self.page = page;
                match page {
                    Page::Settings => self.settings.refresh(),
                    Page::Admin => self.admin.reload(&ctx, self.account.session()),
                    Page::Groups => self.groups.reload(&ctx),
                    _ => {}
                }
            }
            #[cfg(feature = "host")]
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let on = self.host.running();
                let (color, text) = if on {
                    (Color32::from_rgb(46, 184, 92), format!("● Host {}", self.host.summary()))
                } else {
                    (Color32::GRAY, "○ Host off".to_string())
                };
                if ui
                    .add(egui::Button::new(RichText::new(text).color(color)).frame(false))
                    .on_hover_text("Others can connect to this computer while the host is on")
                    .clicked()
                {
                    self.page = Page::ThisComputer;
                }
            });
        });
    }
}

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
            if !self.account.is_admin() && self.page == Page::Admin {
                self.page = Page::Computers;
            }
            // An unregistered host depends on someone being signed in; a registered one does not.
            #[cfg(feature = "host")]
            {
                if self.account.signed_in() {
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
        if !self.account.signed_in() {
            #[cfg(feature = "host")]
            let host = &mut self.host;
            egui::CentralPanel::default().show(ui, |ui| {
                self.account.login_screen(ui, |_ui, _server| {
                    #[cfg(feature = "host")]
                    host.join_section(_ui, _server);
                })
            });
            #[cfg(feature = "host")]
            if let Some((text, true, at)) = &self.host.toast
                && at.elapsed() < Duration::from_secs(12)
            {
                egui::Panel::bottom("signed-out-status").show(ui, |ui| {
                    ui.colored_label(Color32::from_rgb(230, 70, 70), text);
                });
            }
            return;
        }
        egui::Panel::top("nav").show(ui, |ui| {
            ui.add_space(6.0);
            self.nav(ui);
            ui.add_space(4.0);
        });
        let toast = self.toast().cloned();
        let sessions = self.computers.open_sessions();
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                match toast {
                    Some((text, err, _)) => {
                        let color = if err { Color32::from_rgb(230, 70, 70) } else { ui.visuals().text_color() };
                        ui.colored_label(color, text);
                    }
                    None => {
                        ui.weak(match self.page {
                            Page::Computers => "Double-click a computer to connect · right-click for more",
                            #[cfg(feature = "host")]
                            Page::ThisComputer => "Others can connect to this computer while the host is on",
                            Page::Settings => "Changes are saved immediately",
                            Page::Admin => "Only these e-mails can sign in to your server",
                            Page::Groups => "Share computers with a group from the Computers page",
                        });
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if sessions > 0 {
                        ui.label(format!("{sessions} open session(s)"));
                    }
                });
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.page {
            Page::Computers => self.computers.show(ui),
            #[cfg(feature = "host")]
            Page::ThisComputer => self.host.show(ui),
            Page::Settings => {
                self.account.settings_section(ui);
                self.settings.show(ui);
            }
            Page::Admin => self.admin.show(ui, self.account.session()),
            Page::Groups => self.groups.show(ui),
        });
    }

    fn on_exit(&mut self) {
        #[cfg(feature = "host")]
        self.host.shutdown();
    }
}
