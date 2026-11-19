//! Signing in to the rdesk server. Without a session the app shows only the sign-in
//! screen; the session (10 days) is checked with the server at start.

use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use egui::{Color32, RichText};
use rdesk_common::account::{self, NotSignedIn, Session};
use rdesk_common::config::{self, ClientConfig};

const ERROR: Color32 = Color32::from_rgb(230, 70, 70);

enum Check {
    Ok,
    /// The server ended the session (expired, revoked, user disabled or removed).
    SignedOut(String),
    /// Could not ask (offline...): keep the local session.
    Unknown,
}

pub(super) struct Account {
    session: Option<Session>,
    server: String,
    login_job: Option<Receiver<Result<Session, String>>>,
    check_job: Option<Receiver<Check>>,
    logout_job: Option<Receiver<()>>,
    error: Option<String>,
    /// Set when the user signed in or out (the shell starts/stops the host).
    pub(super) changed: bool,
    pub(super) toast: Option<(String, bool, Instant)>,
}

/// Run `f` on a small runtime of its own; the result arrives on the returned channel.
pub(super) fn spawn<T, F, Fut>(ctx: &egui::Context, f: F) -> Receiver<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
{
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let _ = tx.send(rt.block_on(f()));
        ctx.request_repaint();
    });
    rx
}

impl Account {
    pub(super) fn new(ctx: &egui::Context) -> Self {
        let session = Session::load();
        let server = session
            .as_ref()
            .map(|s| s.server.clone())
            .or_else(|| config::load_toml::<ClientConfig>(config::CLIENT_CONFIG).ok().and_then(|c| c.server))
            .unwrap_or_default();
        let mut a = Self {
            session,
            server,
            login_job: None,
            check_job: None,
            logout_job: None,
            error: None,
            changed: false,
            toast: None,
        };
        a.check(ctx);
        a
    }

    pub(super) fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    pub(super) fn signed_in(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.expired())
    }

    pub(super) fn is_admin(&self) -> bool {
        self.signed_in() && self.session.as_ref().is_some_and(|s| s.user.is_admin)
    }

    /// Ask the server whether the session is still good.
    fn check(&mut self, ctx: &egui::Context) {
        let Some(s) = self.session.clone() else { return };
        self.check_job = Some(spawn(ctx, move || async move {
            match s.api() {
                Ok(api) => match api.me().await {
                    Ok(_) => Check::Ok,
                    Err(e) if e.downcast_ref::<NotSignedIn>().is_some() => Check::SignedOut(format!("{e:#}")),
                    Err(e) => {
                        tracing::info!("cannot check the session: {e:#}");
                        Check::Unknown
                    }
                },
                Err(_) => Check::Unknown,
            }
        }));
    }

    fn signed_out(&mut self, why: &str) {
        let _ = Session::clear();
        self.session = None;
        self.changed = true;
        self.error = Some(why.to_string());
    }

    pub(super) fn tick(&mut self) {
        if let Some(rx) = &self.login_job
            && let Ok(r) = rx.try_recv()
        {
            self.login_job = None;
            match r {
                Ok(s) => {
                    let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
                    cfg.server = Some(s.server.clone());
                    let _ = config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &cfg));
                    self.toast = Some((format!("Signed in as {}.", s.user.email), false, Instant::now()));
                    self.session = Some(s);
                    self.error = None;
                    self.changed = true;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(rx) = &self.check_job
            && let Ok(r) = rx.try_recv()
        {
            self.check_job = None;
            if let Check::SignedOut(why) = r {
                self.signed_out(&format!("Signed out by the server: {why}"));
            }
        }
        if let Some(rx) = &self.logout_job
            && rx.try_recv().is_ok()
        {
            self.logout_job = None;
        }
        // The 10 days are up.
        if self.session.as_ref().is_some_and(Session::expired) {
            self.signed_out("Your sign-in expired (10 days). Sign in again.");
        }
    }

    fn start_login(&mut self, ctx: &egui::Context) {
        let server = match account::normalize_server(&self.server) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
        self.error = None;
        self.login_job = Some(spawn(ctx, move || async move {
            account::login(&server, &rdesk_common::local_hostname(), account::open_browser)
                .await
                .map_err(|e| format!("{e:#}"))
        }));
    }

    pub(super) fn sign_out(&mut self, ctx: &egui::Context) {
        if let Some(s) = self.session.take() {
            self.logout_job = Some(spawn(ctx, move || async move {
                if let Ok(api) = s.api() {
                    let _ = api.logout().await;
                }
            }));
        }
        let _ = Session::clear();
        self.changed = true;
        self.toast = Some(("Signed out.".into(), false, Instant::now()));
    }

    /// The whole window when nobody is signed in.
    pub(super) fn login_screen(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.18).max(20.0));
            ui.set_max_width(420.0);
            ui.label(RichText::new("rdesk").strong().size(34.0));
            ui.label(RichText::new("Sign in to your rdesk server to use the app").size(15.0));
            ui.add_space(18.0);
            let busy = self.login_job.is_some();
            ui.add_enabled_ui(!busy, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.server)
                        .hint_text("Server, e.g. rdesk.example.com")
                        .desired_width(360.0),
                );
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.add_space(8.0);
                let go = ui
                    .add_sized([360.0, 34.0], egui::Button::new(RichText::new("Sign in with the browser").size(15.0)));
                if (go.clicked() || enter) && !busy {
                    self.start_login(&ctx);
                }
            });
            ui.add_space(10.0);
            if busy {
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 330.0).max(0.0) / 2.0);
                    ui.spinner();
                    ui.label("Finish the sign-in in your browser (Google or GitHub, then the second factor)…");
                });
                if ui.button("Cancel").clicked() {
                    // The browser's answer, if it still comes, is ignored.
                    self.login_job = None;
                }
            }
            if let Some(e) = &self.error {
                ui.add_space(6.0);
                ui.colored_label(ERROR, e);
            }
            ui.add_space(18.0);
            ui.weak("Only e-mails allowed by an admin of the server can sign in. The sign-in lasts 10 days.");
        });
    }

    /// The "Account" part of Settings.
    pub(super) fn settings_section(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.label(RichText::new("Account").strong().size(16.0));
        if let Some(s) = self.session.clone() {
            ui.label(format!(
                "Signed in as {}{} on {} ({}).",
                s.user.email,
                if s.user.is_admin { " (system admin)" } else { "" },
                s.server_name,
                s.server
            ));
            ui.horizontal(|ui| {
                ui.weak(format!("The sign-in ends in {} day(s); then you sign in again.", s.days_left()));
                if ui.button("Sign out").clicked() {
                    self.sign_out(&ctx);
                }
            });
        }
        ui.add_space(14.0);
    }
}
