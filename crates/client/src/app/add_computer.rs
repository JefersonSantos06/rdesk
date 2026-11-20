//! "Add a computer": a short wizard (first how, then only that case's steps), and the
//! enrollment keys (created, used, revoked), which Settings shows too.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use egui::{CornerRadius, Margin, RichText, Stroke, Ui, Vec2};
use rdesk_common::account::Session;
use rdesk_common::i18n::{role_label, visibility_label};
use rdesk_common::{t, tf};
use rdesk_proto::api::{CreatedEnrollKey, EnrollKey, GroupRef, NewEnrollKey, SetShares, ShareRole, Visibility};

use super::account::{combo_width, now, option_card, rows_card, spawn};
use crate::ui::{self, Tone, icons, size, space};

/// A share's role, as a small drop-down.
fn role_picker(ui: &mut Ui, salt: impl std::hash::Hash + std::fmt::Debug, role: &mut ShareRole) {
    let w = combo_width(ui, ShareRole::ALL.iter().map(|r| role_label(*r)));
    egui::ComboBox::from_id_salt(salt).selected_text(role_label(*role)).width(w).show_ui(ui, |ui| {
        for r in ShareRole::ALL {
            ui.selectable_value(role, r, role_label(r));
        }
    });
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

impl NewKeyForm {
    fn new() -> Self {
        Self {
            name: String::new(),
            reusable: true,
            days: 7,
            visibility: Visibility::Private,
            groups: HashMap::new(),
            error: None,
        }
    }
}

/// The enrollment keys: the list (revoke asks first), the new-key form, and the key just
/// created (shown only once). Used by the wizard and by Settings.
pub(super) struct KeysPanel {
    keys: Option<KeysLoad>,
    job: Option<Receiver<KeysLoad>>,
    form: Option<NewKeyForm>,
    create_job: Option<Receiver<Result<CreatedEnrollKey, String>>>,
    /// The key just created: shown once.
    created: Option<String>,
    /// Revoking this key (id, name) is asked.
    confirm_revoke: Option<(i64, String)>,
    /// The first load was started.
    started: bool,
    /// Bring the keys into view (a key was just created, or the form opened).
    scroll: bool,
}

impl KeysPanel {
    /// Nothing loaded yet: the keys load when the panel is first shown.
    pub(super) fn new() -> Self {
        Self {
            keys: None,
            job: None,
            form: None,
            create_job: None,
            created: None,
            confirm_revoke: None,
            started: false,
            scroll: false,
        }
    }

    /// Load the keys again (when the page that shows them is opened).
    pub(super) fn reload(&mut self, ctx: &egui::Context) {
        self.started = true;
        self.job = Some(load_keys(ctx, None));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if !self.started {
            self.reload(ctx);
        }
        if let Some(rx) = &self.job
            && let Ok(r) = rx.try_recv()
        {
            self.job = None;
            self.keys = Some(r);
        }
        if let Some(rx) = &self.create_job
            && let Ok(r) = rx.try_recv()
        {
            self.create_job = None;
            match r {
                Ok(c) => {
                    self.created = Some(c.key);
                    self.scroll = true;
                    self.form = None;
                    self.job = Some(load_keys(ctx, None));
                }
                Err(e) => {
                    if let Some(f) = &mut self.form {
                        f.error = Some(e);
                    }
                }
            }
        }
    }

    /// The key just created, the "Your keys" header with "New key…", the form and the list.
    /// `server` goes into the commands shown with a new key.
    pub(super) fn show(&mut self, ui: &mut Ui, server: &str) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        let start = ui.cursor().min;
        if let Some(k) = &self.created {
            created_key_box(ui, server, k);
            ui.add_space(space::M);
        }
        ui::row(
            ui,
            |ui| ui::section(ui, t!("Your keys", "Suas chaves")),
            |ui| {
                if self.form.is_none()
                    && ui::secondary(ui, ui::with_icon(icons::PLUS, t!("New key…", "Nova chave…"))).clicked()
                {
                    self.created = None;
                    self.form = Some(NewKeyForm::new());
                    self.scroll = true;
                }
                if self.job.is_some() {
                    ui.spinner();
                }
            },
        );
        ui.add_space(space::XS);
        let groups = match &self.keys {
            Some(Ok((_, g))) => g.clone(),
            _ => Vec::new(),
        };
        let busy = self.create_job.is_some();
        if let Some(f) = &mut self.form {
            match new_key_form(ui, f, &groups, busy) {
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
                    self.create_job = Some(spawn(&ctx, move || async move {
                        let s = Session::load()
                            .ok_or_else(|| t!("not signed in", "você não entrou em um servidor").to_string())?;
                        let api = s.api().map_err(|e| format!("{e:#}"))?;
                        api.create_enroll_key(&req).await.map_err(|e| format!("{e:#}"))
                    }));
                }
                Some(false) => self.form = None,
                None => {}
            }
            ui.add_space(space::M);
        }
        if std::mem::take(&mut self.scroll) {
            let rect = egui::Rect::from_min_size(start, Vec2::new(1.0, 1.0));
            ui.scroll_to_rect(rect, Some(egui::Align::Min));
        }
        match &self.keys {
            None => ui::busy(ui, t!("Loading the keys…", "Carregando as chaves…")),
            Some(Err(e)) => {
                if ui::banner(ui, Tone::Danger, e, Some(t!("Try again", "Tentar de novo"))) {
                    self.job = Some(load_keys(&ctx, None));
                }
            }
            Some(Ok((keys, _))) if keys.is_empty() => {
                ui::caption(ui, t!("No keys yet.", "Nenhuma chave ainda."));
            }
            Some(Ok((keys, _))) => {
                let can_revoke = self.job.is_none();
                rows_card(ui, keys, |ui, k| {
                    if let Some(r) = key_row(ui, k, can_revoke) {
                        self.confirm_revoke = Some(r);
                    }
                });
            }
        }
        if let Some((id, name)) = self.confirm_revoke.clone() {
            match ui::confirm(
                &ctx,
                "revoke-key",
                &tf!("Revoke the key {}?", "Revogar a chave {}?", name),
                t!(
                    "It stops adding computers at once. Computers already added with it stay.",
                    "Ela deixa de adicionar computadores na hora. Os computadores já adicionados com ela continuam."
                ),
                t!("Revoke", "Revogar"),
                true,
            ) {
                Some(true) => {
                    self.confirm_revoke = None;
                    self.job = Some(load_keys(&ctx, Some(id)));
                }
                Some(false) => self.confirm_revoke = None,
                None => {}
            }
        }
    }
}

/// One key: name, prefix and use, how long it lasts, and Revoke. Returns the key to revoke.
fn key_row(ui: &mut Ui, k: &EnrollKey, can_revoke: bool) -> Option<(i64, String)> {
    let p = ui::pal(ui);
    let mut revoke = None;
    let live = !k.revoked && k.expires_at > now();
    ui::row(
        ui,
        |ui| {
            ui.label(RichText::new(icons::KEY).size(size::ICON + 2.0).color(if live { p.accent } else { p.muted }));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui::truncate(ui, RichText::new(&k.name).strong());
                let used = if k.reusable {
                    tf!("{} computer(s)", "{} computador(es)", k.uses)
                } else if k.uses > 0 {
                    t!("used", "usada").into()
                } else {
                    t!("single use", "uso único").into()
                };
                ui::truncate(
                    ui,
                    RichText::new(format!("{}… · {} · {used}", k.prefix, visibility_label(k.visibility)))
                        .size(size::SMALL)
                        .color(p.muted),
                );
            });
        },
        |ui| {
            if live
                && ui
                    .add_enabled(can_revoke, egui::Button::new(t!("Revoke", "Revogar")).min_size(Vec2::new(0.0, 32.0)))
                    .clicked()
            {
                revoke = Some((k.id, k.name.clone()));
            }
            if k.revoked {
                ui::badge(ui, t!("revoked", "revogada"), Tone::Neutral);
            } else if !live {
                ui::badge(ui, t!("expired", "expirada"), Tone::Neutral);
            } else {
                ui::badge(
                    ui,
                    &tf!("{} day(s) left", "{} dia(s) restante(s)", (k.expires_at - now() + 86_399) / 86_400),
                    Tone::Success,
                );
            }
        },
    );
    revoke
}

/// The key just created, with what to do with it: shown only now.
fn created_key_box(ui: &mut Ui, server: &str, key: &str) {
    let p = ui::pal(ui);
    egui::Frame::new()
        .fill(ui::tint(p.success, if p.dark { 30 } else { 20 }))
        .stroke(Stroke::new(1.0, ui::tint(p.success, 120)))
        .corner_radius(CornerRadius::same(ui::RADIUS_CARD))
        .inner_margin(Margin::same(space::M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::CHECK_CIRCLE).size(size::ICON).color(p.success));
                ui.add(
                    egui::Label::new(
                        RichText::new(t!(
                            "New key (shown only now, copy it):",
                            "Nova chave (mostrada só agora, copie-a):"
                        ))
                        .strong(),
                    )
                    .wrap(),
                );
            });
            copy_box(ui, &format!("RDESK_SERVER={server}\nRDESK_ENROLL_KEY={key}"));
            ui::caption(
                ui,
                t!(
                    "Put these in the host service's environment, or run on each computer:",
                    "Coloque isto no ambiente do serviço do host, ou rode em cada computador:"
                ),
            );
            copy_box(ui, &format!("rdesk host enroll --server {server} --key {key}"));
        });
}

/// What the "Add a computer" window wants after a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Still open.
    Open,
    Closed,
    /// "This computer": show the This computer page (where it is registered).
    GoToThisComputer,
    /// "A computer on this network": open the form for a computer saved on this device.
    AddLocal,
}

/// Where the wizard is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The ways to add one.
    Choose,
    /// Another computer the user will sign in on.
    SignIn,
    /// Another computer, without signing in on it: it asks, the user approves.
    Approve,
    /// Many computers at once, with an enrollment key.
    Keys,
}

/// "Add a computer": the ways to add one, then only that way's steps.
pub(super) struct AddComputer {
    step: Step,
    /// The server and e-mail of whoever is signed in (empty without a server).
    server: String,
    email: String,
    keys: KeysPanel,
}

impl AddComputer {
    /// Opened: the keys load when that step is shown.
    pub(super) fn new(_ctx: &egui::Context) -> Self {
        let (server, email) = Session::load().map(|s| (s.server.clone(), s.user.email.clone())).unwrap_or_default();
        Self { step: Step::Choose, server, email, keys: KeysPanel::new() }
    }
}

/// The "Add a computer" window, for one frame.
pub(super) fn show(ctx: &egui::Context, d: &mut AddComputer) -> Outcome {
    let title = t!("Add a computer", "Adicionar um computador");
    if d.step == Step::Choose {
        let r = ui::dialog(ctx, "add-computer", title, 600.0, |ui| d.choose_ui(ui));
        return match r.inner {
            Some(o) => o,
            None if r.closed => Outcome::Closed,
            None => Outcome::Open,
        };
    }
    // A step: its instructions scroll, Back and Done stay at the bottom.
    let r = ui::dialog_with_footer(
        ctx,
        "add-computer-step",
        title,
        600.0,
        |ui| match d.step {
            Step::SignIn => d.sign_in_ui(ui),
            Step::Approve => d.approve_ui(ui),
            _ => d.keys_ui(ui),
        },
        |ui| ui::dialog_buttons(ui, t!("Done", "Concluir"), false, &[t!("Back", "Voltar")]),
    );
    match r.inner.1 {
        Some(ui::DialogButton::Primary) => Outcome::Closed,
        Some(ui::DialogButton::Other(_)) => {
            d.step = Step::Choose;
            Outcome::Open
        }
        None if r.closed => Outcome::Closed,
        None => Outcome::Open,
    }
}

impl AddComputer {
    fn choose_ui(&mut self, ui: &mut Ui) -> Option<Outcome> {
        let mut out = None;
        ui::paragraph(ui, t!("How do you want to add it?", "Como você quer adicioná-lo?"));
        ui.add_space(space::XS);
        let server = !self.server.is_empty();
        let needs_server = t!("Sign in to a server to use this.", "Entre em um servidor para usar esta opção.");
        #[cfg(feature = "host")]
        {
            if option_card(
                ui,
                icons::HOUSE,
                t!("This computer", "Este computador"),
                t!(
                    "Register this computer on your account, on its This computer page.",
                    "Cadastre este computador na sua conta, na página Este computador."
                ),
                icons::CARET_RIGHT,
                true,
            )
            .clicked()
            {
                out = Some(Outcome::GoToThisComputer);
            }
        }
        let card = |ui: &mut Ui, icon: &str, title: &str, body: &str| {
            option_card(ui, icon, title, if server { body } else { needs_server }, icons::CARET_RIGHT, server)
        };
        if card(
            ui,
            icons::SIGN_IN,
            t!("Another computer I will sign in on", "Outro computador em que vou entrar"),
            t!(
                "Install rdesk there, sign in with your account and register it.",
                "Instale o rdesk lá, entre com sua conta e cadastre-o."
            ),
        )
        .clicked()
        {
            self.step = Step::SignIn;
        }
        if card(
            ui,
            icons::ENVELOPE_SIMPLE,
            t!("Another computer, without signing in on it", "Outro computador sem entrar nele"),
            t!(
                "It asks to be added with your e-mail; you approve it here.",
                "Ele pede para ser adicionado com o seu e-mail; você aprova aqui."
            ),
        )
        .clicked()
        {
            self.step = Step::Approve;
        }
        if card(
            ui,
            icons::KEY,
            t!("Many computers (enrollment key)", "Vários computadores (chave de cadastro)"),
            t!(
                "Added at once, no approval: for ready-made images like Raspberry Pi.",
                "Adicionados na hora, sem aprovação: para imagens prontas como Raspberry Pi."
            ),
        )
        .clicked()
        {
            self.step = Step::Keys;
        }
        if option_card(
            ui,
            icons::WIFI_HIGH,
            t!("A computer on this network, without a server", "Um computador nesta rede, sem servidor"),
            t!(
                "Saved only on this device; you get in with its own password or SSH key.",
                "Salvo só neste aparelho; você entra com a senha ou chave SSH dele."
            ),
            icons::CARET_RIGHT,
            true,
        )
        .clicked()
        {
            out = Some(Outcome::AddLocal);
        }
        out
    }

    fn sign_in_ui(&mut self, ui: &mut Ui) {
        ui::section(ui, t!("Another computer I will sign in on", "Outro computador em que vou entrar"));
        ui.add_space(space::S);
        step(
            ui,
            1,
            &tf!("Install rdesk on it (download it from {}).", "Instale o rdesk nele (baixe em {}).", self.server),
        );
        step(
            ui,
            2,
            &tf!(
                "Open rdesk there and sign in to this server with your account ({}).",
                "Abra o rdesk lá e entre neste servidor com a sua conta ({}).",
                self.email
            ),
        );
        step(ui, 3, t!("On its This computer page, register it.", "Na página Este computador dele, cadastre-o."));
        step(
            ui,
            4,
            t!("Done: it shows up in your list of computers.", "Pronto: ele aparece na sua lista de computadores."),
        );
    }

    fn approve_ui(&mut self, ui: &mut Ui) {
        ui::section(ui, t!("Another computer, without signing in on it", "Outro computador sem entrar nele"));
        ui.add_space(space::S);
        step(
            ui,
            1,
            &tf!("Install rdesk on it (download it from {}).", "Instale o rdesk nele (baixe em {}).", self.server),
        );
        step(
            ui,
            2,
            &tf!(
                "On its sign-in screen, choose \"Only share this computer\" and enter this server and your \
                 e-mail ({}).",
                "Na tela de entrada dele, escolha \"Só compartilhar este computador\" e digite este servidor e o \
                 seu e-mail ({}).",
                self.email
            ),
        );
        step_body(ui, |ui| {
            ui::caption(ui, t!("Or, on the command line:", "Ou, na linha de comando:"));
            copy_box(ui, &format!("rdesk host enroll --server {} --owner {}\nrdesk host run", self.server, self.email));
        });
        step(
            ui,
            3,
            t!(
                "It then shows up in your computers, under Waiting for your approval, with a code: approve it \
                 only if that computer shows the same code.",
                "Ele então aparece nos seus computadores, em Aguardando sua aprovação, com um código: aprove só \
                 se aquele computador mostrar o mesmo código."
            ),
        );
    }

    fn keys_ui(&mut self, ui: &mut Ui) {
        ui::section(ui, t!("Many computers (enrollment key)", "Vários computadores (chave de cadastro)"));
        ui.add_space(space::XS);
        ui::paragraph(
            ui,
            t!(
                "An enrollment key adds computers at once, with no approval: whoever has the key can add \
                 computers to your account until it expires or you revoke it. Good for ready-made images \
                 (Raspberry Pi, Orange Pi…).",
                "Uma chave de cadastro adiciona computadores na hora, sem aprovação: quem tiver a chave pode \
                 adicionar computadores à sua conta até ela expirar ou você revogá-la. Boa para imagens prontas \
                 (Raspberry Pi, Orange Pi…).",
            ),
        );
        ui.add_space(space::S);
        step(ui, 1, t!("Create a key below (New key…).", "Crie uma chave abaixo (Nova chave…)."));
        step(
            ui,
            2,
            t!(
                "On each computer, run the command shown with the key, or put the key in the host service's \
                 environment.",
                "Em cada computador, rode o comando que aparece com a chave, ou coloque a chave no ambiente do \
                 serviço do host."
            ),
        );
        step(ui, 3, t!("They show up in your computers at once.", "Eles aparecem nos seus computadores na hora."));
        ui.add_space(space::S);
        ui.separator();
        ui.add_space(space::S);
        self.keys.show(ui, &self.server);
    }
}

/// The width of a step's number column.
const STEP_INDENT: f32 = 34.0;

/// A numbered step: a round number, then the text (wrapped).
fn step(ui: &mut Ui, n: u32, text: &str) {
    let p = ui::pal(ui);
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(STEP_INDENT - space::S, 24.0), egui::Sense::hover());
        let c = egui::pos2(rect.left() + 12.0, rect.center().y);
        ui.painter().circle_filled(c, 12.0, ui::tint(p.accent, if p.dark { 70 } else { 40 }));
        ui.painter().text(
            c,
            egui::Align2::CENTER_CENTER,
            n.to_string(),
            egui::FontId::proportional(size::SMALL + 1.0),
            p.text,
        );
        ui.vertical(|ui| {
            ui.add_space(2.0);
            ui.add(egui::Label::new(text).wrap());
        });
    });
    ui.add_space(space::XS);
}

/// More of a step, under its text.
fn step_body(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.horizontal_top(|ui| {
        ui.add_space(STEP_INDENT);
        ui.vertical(add);
    });
    ui.add_space(space::XS);
}

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

/// Text to copy (a command, a key): monospace, wrapped, with a Copy button that turns into
/// a check for a moment.
pub(super) fn copy_box(ui: &mut Ui, text: &str) {
    let p = ui::pal(ui);
    let id = ui.make_persistent_id(("copy-box", text));
    let copied_at: Option<f64> = ui.data(|d| d.get_temp(id));
    let time = ui.input(|i| i.time);
    let recent = copied_at.is_some_and(|t| time - t < 2.0);
    egui::Frame::new()
        .fill(ui.visuals().code_bg_color)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(ui::RADIUS))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui::row(
                ui,
                |ui| {
                    ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
                },
                |ui| {
                    let (icon, hover) = if recent {
                        (icons::CHECK, t!("Copied", "Copiado"))
                    } else {
                        (icons::COPY, t!("Copy", "Copiar"))
                    };
                    if ui::icon_button(ui, icon, hover).clicked() {
                        ui.ctx().copy_text(text.to_string());
                        ui.data_mut(|d| d.insert_temp(id, time));
                    }
                    if recent {
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                    }
                },
            );
        });
}

/// The new-key form: `Some(true)` create, `Some(false)` cancel.
fn new_key_form(ui: &mut Ui, f: &mut NewKeyForm, groups: &[GroupRef], busy: bool) -> Option<bool> {
    let mut out = None;
    let p = ui::pal(ui);
    let label = |ui: &mut Ui, text: &str| {
        ui.label(RichText::new(text).size(size::SMALL).color(p.muted));
    };
    ui::card(ui, |ui| {
        ui::section(ui, t!("New key", "Nova chave"));
        ui.add_space(space::XS);
        label(ui, t!("Name", "Nome"));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut f.name)
                .hint_text(t!("e.g. Raspberry Pis", "ex.: Raspberry Pis"))
                .desired_width(ui.available_width().min(360.0))
                .margin(Margin::symmetric(8, 6)),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(space::XS);
        label(ui, t!("Use", "Uso"));
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut f.reusable, true, t!("many computers", "vários computadores"));
            ui.radio_value(&mut f.reusable, false, t!("one computer", "um computador"));
        });
        ui.add_space(space::XS);
        label(ui, t!("Valid for", "Válida por"));
        ui.add(egui::DragValue::new(&mut f.days).range(1..=365).suffix(t!(" day(s)", " dia(s)")));
        ui.add_space(space::XS);
        label(ui, t!("Who can see the computers it adds", "Quem pode ver os computadores que ela adiciona"));
        ui.horizontal_wrapped(|ui| {
            for v in [Visibility::Private, Visibility::Shared, Visibility::Everyone] {
                ui.radio_value(&mut f.visibility, v, visibility_label(v));
            }
        });
        if f.visibility == Visibility::Shared {
            ui.add_space(space::XS);
            label(ui, t!("Shared with", "Compartilhados com"));
            if groups.is_empty() {
                ui::caption(
                    ui,
                    t!(
                        "No groups yet (create one on the Groups page).",
                        "Nenhum grupo ainda (crie um na página Grupos)."
                    ),
                );
            }
            for g in groups {
                let mut on = f.groups.contains_key(&g.id);
                let mut role = f.groups.get(&g.id).copied();
                let mut toggled = false;
                ui::row(
                    ui,
                    |ui| {
                        toggled = ui.checkbox(&mut on, "").changed();
                        ui::truncate(ui, &g.name);
                    },
                    |ui| {
                        if let Some(r) = &mut role {
                            role_picker(ui, ("key-role", g.id), r);
                        }
                    },
                );
                if toggled {
                    if on {
                        f.groups.insert(g.id, ShareRole::Control);
                    } else {
                        f.groups.remove(&g.id);
                    }
                } else if let Some(r) = role {
                    f.groups.insert(g.id, r);
                }
            }
        }
        if let Some(e) = &f.error {
            ui.add_space(space::S);
            ui::banner(ui, Tone::Danger, e, None);
        }
        ui.add_space(space::S);
        ui.horizontal_wrapped(|ui| {
            let ok = !busy && !f.name.trim().is_empty();
            let create = ui.add_enabled(
                ok,
                egui::Button::new(RichText::new(t!("Create key", "Criar chave")).color(p.on_accent).strong())
                    .fill(p.accent)
                    .stroke(Stroke::NONE)
                    .min_size(Vec2::new(0.0, 32.0)),
            );
            if create.clicked() || (ok && enter) {
                out = Some(true);
            }
            if ui::ghost(ui, t!("Cancel", "Cancelar")).clicked() {
                out = Some(false);
            }
            if busy {
                ui.spinner();
            }
        });
    });
    out
}

#[cfg(test)]
impl KeysPanel {
    /// Keys already loaded (no network).
    pub(super) fn sample(keys: Vec<EnrollKey>, groups: Vec<GroupRef>) -> Self {
        let mut k = Self::new();
        k.started = true;
        k.scroll = true;
        k.keys = Some(Ok((keys, groups)));
        k
    }

    /// Shown from the top (not scrolled to the keys).
    pub(super) fn no_scroll(&mut self) {
        self.scroll = false;
    }

    pub(super) fn sample_keys(n: usize) -> Vec<EnrollKey> {
        let names = [
            "Raspberry Pis da oficina de robótica da escola municipal",
            "Orange Pi",
            "Servidores do escritório",
            "Notebooks dos estagiários do segundo semestre",
        ];
        (0..n)
            .map(|i| EnrollKey {
                id: i as i64 + 1,
                name: names[i % names.len()].into(),
                prefix: format!("rdk_Ab{i}x"),
                reusable: i % 3 != 1,
                visibility: [Visibility::Private, Visibility::Shared, Visibility::Everyone][i % 3],
                shares: Default::default(),
                created_at: now() - 86_400 * 3,
                expires_at: if i % 4 == 3 { now() - 3600 } else { now() + 86_400 * (i as i64 % 30 + 1) },
                uses: (i * 7 % 13) as i64,
                revoked: i % 5 == 4,
            })
            .collect()
    }

    pub(super) fn sample_groups() -> Vec<GroupRef> {
        vec![
            GroupRef { id: 1, name: "Família".into() },
            GroupRef { id: 2, name: "Equipe de suporte técnico do escritório central de São Paulo".into() },
        ]
    }
}

/// Screens for the ui gallery (see `gallery.rs`): sample data, long texts.
#[cfg(test)]
pub(super) fn gallery() -> Vec<super::gallery::Screen> {
    use super::gallery::{screen, with_sidebar};
    fn wizard(step: Step, keys: KeysPanel, server: &str) -> impl FnMut(&mut Ui) {
        let mut d =
            AddComputer { step, server: server.into(), email: "jeferson.santos.silva@exemplo.com.br".into(), keys };
        move |ui: &mut Ui| {
            with_sidebar(ui, super::Page::Computers, |ui| {
                ui::page_title(ui, "Computadores", None);
            });
            show(ui.ctx(), &mut d);
        }
    }
    const SERVER: &str = "https://rdesk.familia-santos-silva.exemplo.com.br";
    vec![
        screen("add-choose", || wizard(Step::Choose, KeysPanel::new(), SERVER)),
        screen("add-choose-no-server", || wizard(Step::Choose, KeysPanel::new(), "")),
        screen("add-sign-in", || wizard(Step::SignIn, KeysPanel::new(), SERVER)),
        screen("add-approve", || wizard(Step::Approve, KeysPanel::new(), SERVER)),
        screen("add-keys-empty", || wizard(Step::Keys, KeysPanel::sample(Vec::new(), Vec::new()), SERVER)),
        screen("add-keys-created", || {
            let mut k = KeysPanel::sample(KeysPanel::sample_keys(2), KeysPanel::sample_groups());
            k.created = Some("rdk_AbC1xY9zQ7mN3pL5kJ8hG2fD4sA6wE0rT".into());
            wizard(Step::Keys, k, SERVER)
        }),
        screen("add-keys-long", || {
            wizard(Step::Keys, KeysPanel::sample(KeysPanel::sample_keys(14), KeysPanel::sample_groups()), SERVER)
        }),
        screen("add-keys-form", || {
            let mut k = KeysPanel::sample(KeysPanel::sample_keys(3), KeysPanel::sample_groups());
            let mut f = NewKeyForm::new();
            f.name = "Raspberry Pis da oficina".into();
            f.visibility = Visibility::Shared;
            f.groups.insert(2, ShareRole::View);
            f.error = Some("o servidor recusou: já existe uma chave com esse nome".into());
            k.form = Some(f);
            wizard(Step::Keys, k, SERVER)
        }),
        screen("add-keys-revoke", || {
            let mut k = KeysPanel::sample(KeysPanel::sample_keys(3), KeysPanel::sample_groups());
            k.confirm_revoke = Some((1, "Raspberry Pis da oficina de robótica da escola municipal".into()));
            wizard(Step::Keys, k, SERVER)
        }),
    ]
}
