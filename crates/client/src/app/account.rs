//! Signing in to the rdesk server. Without a session the app shows only the sign-in
//! screen; the session (10 days) is checked with the server at start.
//!
//! Also a few small pieces shared by the sign-in screen, Add a computer, Settings, Groups
//! and Admin (option cards, clickable rows, the "more" menu).

use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use egui::{Align, CornerRadius, CursorIcon, Layout, Margin, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2};
use rdesk_common::account::{self, NotSignedIn, Session};
use rdesk_common::config::{self, ClientConfig};
use rdesk_common::{t, tf};

use crate::ui::{self, Tone, icons, size, space};

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
    /// The sign-in screen's "Only share this computer" card is open.
    share_open: bool,
    /// "Sign out?" is asked.
    confirm_sign_out: bool,
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
            // Downloaded from the server's page as rdesk@server.exe.
            .or_else(config::server_from_exe_name)
            .unwrap_or_default();
        let mut a = Self::with(session, server);
        a.check(ctx);
        a
    }

    fn with(session: Option<Session>, server: String) -> Self {
        Self {
            session,
            server,
            login_job: None,
            check_job: None,
            logout_job: None,
            error: None,
            share_open: false,
            confirm_sign_out: false,
            changed: false,
            toast: None,
        }
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
                    self.toast =
                        Some((tf!("Signed in as {}.", "Você entrou como {}.", s.user.email), false, Instant::now()));
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
                self.signed_out(&tf!("Signed out by the server: {why}", "O servidor encerrou sua sessão: {why}"));
            }
        }
        if let Some(rx) = &self.logout_job
            && rx.try_recv().is_ok()
        {
            self.logout_job = None;
        }
        // The 10 days are up.
        if self.session.as_ref().is_some_and(Session::expired) {
            self.signed_out(t!(
                "Your sign-in expired (10 days). Sign in again.",
                "Seu acesso expirou (10 dias). Entre de novo."
            ));
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
        self.toast = Some((t!("Signed out.", "Você saiu.").into(), false, Instant::now()));
    }

    /// The whole window when nobody is signed in: a card to sign in, and below it the two
    /// other ways to use the app. `extra` draws the "Only share this computer" part (with
    /// the server address typed so far). Returns `true` when the user chose to use the app
    /// without a server (local network only).
    pub(super) fn login_screen(&mut self, ui: &mut Ui, extra: impl FnOnce(&mut Ui, &str)) -> bool {
        let mut go_local = false;
        let mut just_opened = false;
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().id_salt("login").auto_shrink([false, false]).show(ui, |ui| {
            // Centered: the content's height is known from the last frame.
            let height_id = ui.make_persistent_id("login-height");
            let last_h: f32 = ui.data(|d| d.get_temp(height_id)).unwrap_or(0.0);
            let top = ((ui.available_height() - last_h) / 2.0).max(space::S);
            let avail = ui.available_width();
            let w = avail.min(440.0);
            let start = ui.cursor().top();
            ui.add_space(top);
            ui.horizontal(|ui| {
                ui.add_space(((avail - w) / 2.0).max(0.0));
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui::card(ui, |ui| self.sign_in_card(ui, &ctx));
                    ui.add_space(space::L);
                    ui::caption(ui, t!("Or, without signing in:", "Ou, sem entrar:"));
                    ui.add_space(space::XS);
                    if option_card(
                        ui,
                        icons::WIFI_HIGH,
                        t!("Use on this network, without a server", "Usar nesta rede, sem servidor"),
                        t!(
                            "Only computers on the local network or Tailscale that let you in with their own \
                             password or SSH key.",
                            "Só computadores da rede local ou do Tailscale que deixam você entrar com a senha ou \
                             chave SSH deles."
                        ),
                        icons::CARET_RIGHT,
                        true,
                    )
                    .clicked()
                    {
                        go_local = true;
                    }
                    ui.add_space(space::S);
                    let caret = if self.share_open { icons::CARET_UP } else { icons::CARET_DOWN };
                    if option_card(
                        ui,
                        icons::SHARE_NETWORK,
                        t!("Only share this computer", "Só compartilhar este computador"),
                        t!(
                            "Let someone reach this computer, without using the app here.",
                            "Deixe alguém acessar este computador, sem usar o app aqui."
                        ),
                        caret,
                        true,
                    )
                    .clicked()
                    {
                        self.share_open = !self.share_open;
                        just_opened = self.share_open;
                    }
                    if self.share_open {
                        ui.add_space(space::XS);
                        let card = ui::card(ui, |ui| extra(ui, &self.server)).response;
                        if just_opened {
                            card.scroll_to_me(None);
                        }
                    }
                });
            });
            ui.add_space(space::S);
            let h = ui.cursor().top() - start - top;
            if (h - last_h).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(height_id, h));
                ui.ctx().request_repaint();
            }
        });
        go_local
    }

    /// The sign-in card: the server and "Sign in with the browser", or the wait for it.
    fn sign_in_card(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let p = ui::pal(ui);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(icons::DESKTOP).size(34.0).color(p.accent));
            ui.label(RichText::new("rdesk").size(26.0).strong());
            ui.add(
                egui::Label::new(
                    RichText::new(t!(
                        "Sign in to your rdesk server to use the app",
                        "Entre no seu servidor rdesk para usar o app"
                    ))
                    .color(p.muted),
                )
                .wrap(),
            );
        });
        ui.add_space(space::L);
        let busy = self.login_job.is_some();
        ui.label(RichText::new(t!("Server", "Servidor")).size(size::SMALL).color(p.muted));
        let resp = ui.add_enabled(
            !busy,
            egui::TextEdit::singleline(&mut self.server)
                .hint_text(t!("e.g. rdesk.example.com", "ex.: rdesk.exemplo.com.br"))
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8, 8)),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(space::S);
        if busy {
            egui::Frame::new()
                .fill(ui::tint(p.accent, if p.dark { 30 } else { 18 }))
                .corner_radius(CornerRadius::same(ui::RADIUS))
                .inner_margin(Margin::same(space::M as i8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.add(
                            egui::Label::new(t!(
                                "Finish the sign-in in your browser (Google or GitHub, then the second factor)…",
                                "Termine de entrar no navegador (Google ou GitHub, depois o segundo fator)…"
                            ))
                            .wrap(),
                        );
                    });
                    ui.add_space(space::XS);
                    if ui::secondary(ui, t!("Cancel", "Cancelar")).clicked() {
                        // The browser's answer, if it still comes, is ignored.
                        self.login_job = None;
                    }
                });
        } else if wide_primary(
            ui,
            &ui::with_icon(icons::SIGN_IN, t!("Sign in with the browser", "Entrar pelo navegador")),
            true,
        )
        .clicked()
            || enter
        {
            self.start_login(ctx);
        }
        if let Some(e) = &self.error {
            ui.add_space(space::S);
            ui::banner(ui, Tone::Danger, e, None);
        }
        ui.add_space(space::M);
        ui::caption(
            ui,
            t!(
                "Only e-mails allowed by an admin of the server can sign in. The sign-in lasts 10 days.",
                "Só e-mails liberados por um admin do servidor podem entrar. O acesso dura 10 dias."
            ),
        );
    }

    /// The "Account" part of Settings: who is signed in, where, and Sign out.
    pub(super) fn settings_section(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let Some(s) = self.session.clone() else { return };
        ui::card(ui, |ui| {
            ui::row(
                ui,
                |ui| {
                    ui::avatar(ui, if s.user.name.is_empty() { &s.user.email } else { &s.user.name }, 40.0);
                    ui.vertical(|ui| {
                        let main = if s.user.name.is_empty() { &s.user.email } else { &s.user.name };
                        ui::truncate(ui, RichText::new(main).strong());
                        if !s.user.name.is_empty() {
                            ui::truncate(ui, ui::muted(&s.user.email));
                        }
                        if s.user.is_admin {
                            ui::badge(ui, t!("system admin", "admin do sistema"), Tone::Accent);
                        }
                    });
                },
                |ui| {
                    if ui::secondary(ui, ui::with_icon(icons::SIGN_OUT, t!("Sign out", "Sair"))).clicked() {
                        self.confirm_sign_out = true;
                    }
                },
            );
            ui.add_space(space::S);
            ui.separator();
            ui.add_space(space::XS);
            ui::facts(
                ui,
                "account-facts",
                &[
                    (t!("Server", "Servidor"), format!("{} ({})", s.server_name, s.server)),
                    (
                        t!("Sign-in", "Acesso"),
                        tf!(
                            "Ends in {} day(s); then you sign in again.",
                            "Termina em {} dia(s); depois você entra de novo.",
                            s.days_left()
                        ),
                    ),
                ],
            );
        });
        if self.confirm_sign_out {
            match ui::confirm(
                &ctx,
                "sign-out",
                &tf!("Sign out of {}?", "Sair de {}?", s.server_name),
                t!(
                    "To use the server's computers again you sign in again with the browser.",
                    "Para usar os computadores do servidor de novo, você entra outra vez pelo navegador."
                ),
                t!("Sign out", "Sair"),
                false,
            ) {
                Some(true) => {
                    self.confirm_sign_out = false;
                    self.sign_out(&ctx);
                }
                Some(false) => self.confirm_sign_out = false,
                None => {}
            }
        }
    }
}

/// Seconds since 1970.
pub(super) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Small pieces shared by the pages of this area (candidates for `ui`).
// ---------------------------------------------------------------------------

/// The main action as wide as the card it is in.
pub(super) fn wide_primary(ui: &mut Ui, text: &str, enabled: bool) -> egui::Response {
    let p = ui::pal(ui);
    let w = ui.available_width();
    // Centered: the label sits in the middle of the wide button.
    ui.vertical_centered(|ui| {
        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(text).color(p.on_accent).strong())
                .fill(p.accent)
                .stroke(Stroke::NONE)
                .min_size(Vec2::new(w, 38.0)),
        )
    })
    .inner
}

/// A card to click: an icon, a title and a line about it, and `trailing` (a caret) on the
/// right. Disabled cards are dimmed and do not react.
pub(super) fn option_card(
    ui: &mut Ui,
    icon: &str,
    title: &str,
    body: &str,
    trailing: &str,
    enabled: bool,
) -> egui::Response {
    let p = ui::pal(ui);
    let bg = ui.painter().add(egui::Shape::Noop);
    let fg = if enabled { p.text } else { p.muted };
    let inner = egui::Frame::new().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui::row(
            ui,
            |ui| {
                ui.add(
                    egui::Label::new(RichText::new(icon).size(24.0).color(if enabled { p.accent } else { p.muted }))
                        .selectable(false),
                );
                ui.add_space(space::XS);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add(egui::Label::new(RichText::new(title).strong().color(fg)).wrap().selectable(false));
                    ui.add(
                        egui::Label::new(RichText::new(body).size(size::SMALL).color(p.muted)).wrap().selectable(false),
                    );
                });
            },
            |ui| {
                ui.add(egui::Label::new(RichText::new(trailing).size(size::ICON).color(p.muted)).selectable(false));
            },
        );
    });
    let rect = inner.response.rect;
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, ui.make_persistent_id(("option-card", title)), sense);
    let hovered = enabled && resp.hovered();
    let fill = if hovered { p.hover } else { p.surface };
    let stroke = Stroke::new(1.0, if hovered { ui::tint(p.accent, 170) } else { p.border });
    ui.painter().set(
        bg,
        egui::epaint::RectShape::new(rect, CornerRadius::same(ui::RADIUS_CARD), fill, stroke, StrokeKind::Inside),
    );
    if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp }
}

/// A clickable row of a list, `height` tall and as wide as allowed, highlighted when
/// selected or hovered. `add` draws its contents (left to right, centered); controls in
/// it still get their own clicks.
pub(super) fn list_row<R>(
    ui: &mut Ui,
    height: f32,
    selected: bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> (egui::Response, R) {
    let p = ui::pal(ui);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height), Sense::click());
    let fill = if selected {
        ui::tint(p.accent, if p.dark { 52 } else { 30 })
    } else if resp.hovered() {
        p.hover
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(ui::RADIUS), fill);
    let mut child = ui.new_child(
        UiBuilder::new().max_rect(rect.shrink2(Vec2::new(10.0, 4.0))).layout(Layout::left_to_right(Align::Center)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    let r = add(&mut child);
    (resp.on_hover_cursor(CursorIcon::PointingHand), r)
}

/// The "⋯" button that opens a menu of actions for one item.
pub(super) fn more_menu(ui: &mut Ui, hover: &str, add: impl FnOnce(&mut Ui)) {
    let p = ui::pal(ui);
    let button = egui::Button::new(RichText::new(icons::DOTS_THREE).size(size::ICON + 2.0).color(p.text))
        .frame_when_inactive(false)
        .min_size(Vec2::splat(32.0));
    let (resp, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
        ui.set_min_width(200.0);
        add(ui)
    });
    resp.on_hover_text(hover);
}

/// An item of a [`more_menu`].
pub(super) fn menu_item(ui: &mut Ui, icon: &str, text: &str) -> egui::Response {
    ui.add(egui::Button::new(ui::with_icon(icon, text)).min_size(Vec2::new(0.0, 30.0)))
}

/// A menu item for something that removes or ends: red.
pub(super) fn menu_item_danger(ui: &mut Ui, icon: &str, text: &str) -> egui::Response {
    let c = ui::pal(ui).danger;
    ui.add(egui::Button::new(RichText::new(ui::with_icon(icon, text)).color(c)).min_size(Vec2::new(0.0, 30.0)))
}

/// The width a combo box needs to show the longest of `texts` without cutting it.
pub(super) fn combo_width<'a>(ui: &Ui, texts: impl IntoIterator<Item = &'a str>) -> f32 {
    let widest = texts
        .into_iter()
        .map(|t| {
            ui.painter()
                .layout_no_wrap(t.to_owned(), egui::FontId::proportional(size::BODY), egui::Color32::WHITE)
                .size()
                .x
        })
        .fold(0.0, f32::max);
    widest + 2.0 * ui.spacing().button_padding.x + ui.spacing().icon_width + ui.spacing().item_spacing.x + 8.0
}

/// A card's list of rows with lines between them.
pub(super) fn rows_card<T>(ui: &mut Ui, items: &[T], mut add: impl FnMut(&mut Ui, &T)) {
    let p = ui::pal(ui);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(ui::RADIUS_CARD))
        .inner_margin(Margin::symmetric(space::M as i8, space::XS as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    let w = ui.available_width();
                    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
                    ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.0, p.border));
                }
                ui.scope(|ui| {
                    ui.add_space(2.0);
                    add(ui, item);
                    ui.add_space(2.0);
                });
            }
        });
}

#[cfg(test)]
impl Account {
    /// An account for the gallery: no network, no files.
    pub(super) fn sample(session: Option<Session>, server: &str) -> Self {
        Self::with(session, server.into())
    }

    /// A session with long names.
    pub(super) fn sample_session(admin: bool) -> Session {
        Session {
            server: "https://rdesk.familia-santos-silva.exemplo.com.br".into(),
            token: String::new(),
            expires_at: now() + 7 * 86_400 + 3600,
            user: rdesk_proto::api::UserInfo {
                id: 1,
                email: "jeferson.santos.silva@exemplo.com.br".into(),
                name: "Jeferson Santos da Silva".into(),
                is_admin: admin,
            },
            relay_url: String::new(),
            server_name: "rdesk da Família Santos".into(),
        }
    }
}

/// Screens for the ui gallery (see `gallery.rs`): sample data, long texts.
#[cfg(test)]
pub(super) fn gallery() -> Vec<super::gallery::Screen> {
    use super::gallery::{page, screen};
    fn login(a: Account, placeholder: bool) -> impl FnMut(&mut Ui) {
        let mut a = a;
        move |ui: &mut Ui| {
            page(ui, |ui| {
                a.login_screen(ui, |ui, server| {
                    if placeholder {
                        ui::section(ui, "Só compartilhar este computador (amostra)");
                        ui::paragraph(
                            ui,
                            &format!(
                                "Aqui fica a parte do host: o e-mail do dono e o botão para pedir acesso a {server}. \
                                 Um texto longo de amostra para ver a quebra de linha dentro do cartão."
                            ),
                        );
                        ui::secondary(ui, "Pedir para ser adicionado");
                    }
                });
            });
        }
    }
    vec![
        screen("login-idle", || login(Account::sample(None, ""), false)),
        screen("login-waiting", || {
            let mut a = Account::sample(None, "rdesk.familia-santos-silva.exemplo.com.br");
            let (tx, rx) = channel();
            std::mem::forget(tx);
            a.login_job = Some(rx);
            login(a, false)
        }),
        screen("login-error", || {
            let mut a = Account::sample(None, "rdesk.exemplo");
            a.error = Some(
                "não deu para falar com https://rdesk.exemplo: erro de conexão (o servidor não respondeu a tempo); \
                 confira o endereço e a sua internet"
                    .into(),
            );
            login(a, false)
        }),
        screen("login-share-open", || {
            let mut a = Account::sample(None, "rdesk.familia-santos-silva.exemplo.com.br");
            a.share_open = true;
            login(a, true)
        }),
    ]
}
