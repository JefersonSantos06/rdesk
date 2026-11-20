//! "Manage access" for one computer: who may reach it, in one place.
//!
//! What the server does with each choice (see `crates/server/src/access.rs`):
//!
//! * **Only me** (`Private`): the owner and the managers. The list of people and groups
//!   stays saved on the server but grants nothing.
//! * **People and groups** (`Shared`): the owner, the managers, and each person and group
//!   in the list with its role (a member of a group gets the group's role; the widest wins).
//! * **Everyone on the server** (`Everyone`): every account on the server, with full
//!   control. The list stays saved but adds nothing.
//!
//! Managers (chosen by the owner only) always have full control, whatever the choice, and
//! may manage the computer too. Saving sends only what changed: the list (`set_shares`),
//! the choice (`update_machine`), the managers (`set_managers`).

use std::sync::mpsc::Receiver;

use egui::{RichText, Ui};
use rdesk_common::account::{Api, Session};
use rdesk_common::i18n::role_label;
use rdesk_common::{t, tf};
use rdesk_proto::api::{GroupRef, Machine, ShareRole, Shares, UpdateMachine, UserRef, Visibility};

use super::account::spawn;
use crate::ui::{self, icons, pal, size, space};

/// Everyone, every group, the current list, and (for the owner) the managers.
type Load = Result<(Vec<UserRef>, Vec<GroupRef>, Shares, Option<Vec<UserRef>>), String>;

struct Person {
    user: UserRef,
    role: ShareRole,
    /// A manager (the owner chooses them): full control, and may manage it.
    manage: bool,
    /// In the list on the server already (a manager keeps that line as it was).
    listed: bool,
}

struct GroupLine {
    group: GroupRef,
    role: ShareRole,
}

/// What was on the server when the dialog opened, to send only what changed.
#[derive(Default)]
struct Original {
    users: Vec<(i64, ShareRole)>,
    groups: Vec<(i64, ShareRole)>,
    managers: Vec<i64>,
}

pub(super) struct AccessDialog {
    machine: Machine,
    mode: Visibility,
    people: Vec<Person>,
    groups: Vec<GroupLine>,
    everyone: Vec<UserRef>,
    all_groups: Vec<GroupRef>,
    original: Original,
    /// `None` while loading.
    loaded: Option<Result<(), String>>,
    job: Option<Receiver<Load>>,
    query: String,
    /// The user editing (not offered in the list).
    me: Option<i64>,
}

/// The calls that save the dialog.
pub(super) struct Plan {
    id: i64,
    name: String,
    visibility: Option<Visibility>,
    shares: Option<(Lines, Lines)>,
    managers: Option<Vec<i64>>,
}

/// `(user or group id, role)`.
type Lines = Vec<(i64, ShareRole)>;

impl Plan {
    pub(super) fn is_empty(&self) -> bool {
        self.visibility.is_none() && self.shares.is_none() && self.managers.is_none()
    }

    /// Run the calls. The list goes first and the choice after it, so nobody is let in by a
    /// choice before the list it refers to is right.
    pub(super) async fn apply(self, api: Api) -> anyhow::Result<String> {
        if let Some((users, groups)) = self.shares {
            api.set_shares(self.id, users, groups).await?;
        }
        if let Some(v) = self.visibility {
            api.update_machine(self.id, &UpdateMachine { name: None, visibility: Some(v) }).await?;
        }
        if let Some(m) = self.managers {
            api.set_managers(self.id, m).await?;
        }
        let name = self.name;
        Ok(tf!("Saved who can reach {name}.", "Salvo quem acessa {name}."))
    }
}

pub(super) enum Outcome {
    Open,
    Closed,
    Save(Plan),
}

fn sorted<T: Ord + Clone>(v: &[T]) -> Vec<T> {
    let mut v = v.to_vec();
    v.sort();
    v
}

fn role_key(r: ShareRole) -> u8 {
    match r {
        ShareRole::Control => 0,
        ShareRole::View => 1,
        ShareRole::Files => 2,
        ShareRole::Terminal => 3,
    }
}

fn pairs_sorted(v: &[(i64, ShareRole)]) -> Vec<(i64, u8)> {
    sorted(&v.iter().map(|(i, r)| (*i, role_key(*r))).collect::<Vec<_>>())
}

/// What a person is called in a line.
pub(super) fn person_label(u: &UserRef) -> &str {
    if u.name.trim().is_empty() { &u.email } else { &u.name }
}

/// What the three choices mean, for people.
pub(super) fn mode_label(v: Visibility) -> &'static str {
    match v {
        Visibility::Private => t!("Only me", "Só eu"),
        Visibility::Shared => t!("People and groups", "Pessoas e grupos"),
        Visibility::Everyone => t!("Everyone on the server", "Todos do servidor"),
    }
}

impl AccessDialog {
    /// Opened: everyone, the groups, the current list and the managers load in the background.
    pub(super) fn new(ctx: &egui::Context, m: &Machine) -> Self {
        let mut d = Self::empty(m);
        if let Some(s) = Session::load() {
            d.me = Some(s.user.id);
            let (id, owner) = (m.id, m.is_owner);
            d.job = Some(spawn(ctx, move || async move {
                let api = s.api().map_err(|e| format!("{e:#}"))?;
                let f = async {
                    let users = api.directory().await?;
                    let groups = api.all_groups().await?;
                    let shares = api.shares(id).await?;
                    let managers = if owner { Some(api.managers(id).await?.users) } else { None };
                    anyhow::Ok((users, groups, shares, managers))
                };
                f.await.map_err(|e| format!("{e:#}"))
            }));
        } else {
            d.loaded = Some(Err(t!("Not signed in.", "Você não entrou em um servidor.").into()));
        }
        d
    }

    fn empty(m: &Machine) -> Self {
        Self {
            machine: m.clone(),
            mode: m.visibility,
            people: Vec::new(),
            groups: Vec::new(),
            everyone: Vec::new(),
            all_groups: Vec::new(),
            original: Original::default(),
            loaded: None,
            job: None,
            query: String::new(),
            me: None,
        }
    }

    pub(super) fn machine_id(&self) -> i64 {
        self.machine.id
    }

    fn take(&mut self, (users, groups, shares, managers): (Vec<UserRef>, Vec<GroupRef>, Shares, Option<Vec<UserRef>>)) {
        self.original = Original {
            users: shares.users.iter().map(|(u, r)| (u.id, *r)).collect(),
            groups: shares.groups.iter().map(|(g, r)| (g.id, *r)).collect(),
            managers: managers.iter().flatten().map(|u| u.id).collect(),
        };
        self.people = shares
            .users
            .iter()
            .map(|(u, r)| Person { user: u.clone(), role: *r, manage: false, listed: true })
            .collect();
        for u in managers.into_iter().flatten() {
            match self.people.iter_mut().find(|p| p.user.id == u.id) {
                Some(p) => p.manage = true,
                None => self.people.push(Person { user: u, role: ShareRole::Control, manage: true, listed: false }),
            }
        }
        self.groups = shares.groups.into_iter().map(|(g, r)| GroupLine { group: g, role: r }).collect();
        self.everyone = users;
        self.all_groups = groups;
        self.loaded = Some(Ok(()));
    }

    /// The calls to make (only what changed).
    fn plan(&self) -> Plan {
        let users: Vec<(i64, ShareRole)> = self
            .people
            .iter()
            .filter(|p| !p.manage || p.listed)
            .map(|p| {
                // A manager's own line stays as it was (they have full control anyway).
                let role = if p.manage {
                    self.original.users.iter().find(|(i, _)| *i == p.user.id).map(|(_, r)| *r).unwrap_or(p.role)
                } else {
                    p.role
                };
                (p.user.id, role)
            })
            .collect();
        let groups: Vec<(i64, ShareRole)> = self.groups.iter().map(|g| (g.group.id, g.role)).collect();
        let shares_changed = pairs_sorted(&users) != pairs_sorted(&self.original.users)
            || pairs_sorted(&groups) != pairs_sorted(&self.original.groups);
        let managers: Vec<i64> = self.people.iter().filter(|p| p.manage).map(|p| p.user.id).collect();
        let managers_changed = self.machine.is_owner && sorted(&managers) != sorted(&self.original.managers);
        Plan {
            id: self.machine.id,
            name: self.machine.name.clone(),
            visibility: (self.mode != self.machine.visibility).then_some(self.mode),
            shares: shares_changed.then_some((users, groups)),
            managers: managers_changed.then_some(managers),
        }
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.job
            && let Ok(r) = rx.try_recv()
        {
            self.job = None;
            match r {
                Ok(data) => self.take(data),
                Err(e) => self.loaded = Some(Err(e)),
            }
        }
    }
}

/// The dialog, one frame.
pub(super) fn show(ctx: &egui::Context, d: &mut AccessDialog) -> Outcome {
    d.poll();
    let title = tf!("Manage access · {}", "Gerenciar acesso · {}", d.machine.name);
    let ready = matches!(d.loaded, Some(Ok(())));
    let r = ui::dialog_with_footer(ctx, "computers-access", &title, 600.0, |ui| body(ui, d), |ui| footer(ui, ready));
    match r.inner.1 {
        Some(true) => {
            let plan = d.plan();
            if plan.is_empty() { Outcome::Closed } else { Outcome::Save(plan) }
        }
        Some(false) => Outcome::Closed,
        None if r.closed => Outcome::Closed,
        None => Outcome::Open,
    }
}

/// Save and Cancel (or Close while it cannot be edited). `Some(true)`: save.
fn footer(ui: &mut Ui, ready: bool) -> Option<bool> {
    if !ready {
        return close_only(ui);
    }
    match ui::dialog_buttons(ui, t!("Save", "Salvar"), false, &[t!("Cancel", "Cancelar")]) {
        Some(ui::DialogButton::Primary) => Some(true),
        Some(ui::DialogButton::Other(_)) => Some(false),
        None => None,
    }
}

fn body(ui: &mut Ui, d: &mut AccessDialog) {
    match &d.loaded {
        None => {
            ui::busy(ui, t!("Loading who can reach it…", "Carregando quem acessa…"));
            return;
        }
        Some(Err(e)) => {
            let e = e.clone();
            ui::banner(
                ui,
                ui::Tone::Danger,
                &tf!("Cannot load the access list: {e}", "Não foi possível carregar a lista de acesso: {e}"),
                None,
            );
            return;
        }
        Some(Ok(())) => {}
    }
    ui::caption(ui, t!("Who can see and reach this computer", "Quem pode ver e acessar este computador"));
    ui.add_space(space::XS);
    segmented(
        ui,
        &mut d.mode,
        &[
            (Visibility::Private, mode_label(Visibility::Private)),
            (Visibility::Shared, mode_label(Visibility::Shared)),
            (Visibility::Everyone, mode_label(Visibility::Everyone)),
        ],
    );
    ui.add_space(space::S);
    let owner_email = d.machine.owner.email.clone();
    let explain = match d.mode {
        Visibility::Private if d.machine.is_owner && d.people.iter().any(|p| p.manage) => {
            t!("Only you and its managers can see and reach it.", "Só você e os gestores podem vê-lo e acessá-lo.")
                .to_string()
        }
        Visibility::Private if d.machine.is_owner => {
            t!("Only you can see and reach it.", "Só você pode vê-lo e acessá-lo.").to_string()
        }
        Visibility::Private => tf!(
            "Only its owner ({owner_email}) and its managers can see and reach it.",
            "Só o dono ({owner_email}) e os gestores podem vê-lo e acessá-lo."
        ),
        Visibility::Shared => t!(
            "You and the people and groups below, each with the access chosen. Members of a group get \
             the group's access.",
            "Você e as pessoas e grupos abaixo, cada um com o acesso escolhido. Quem está num grupo recebe \
             o acesso do grupo."
        )
        .to_string(),
        Visibility::Everyone => t!(
            "Everyone with an account on this server can see it and has full control.",
            "Todas as pessoas com conta neste servidor podem vê-lo e têm controle total."
        )
        .to_string(),
    };
    ui::paragraph(ui, &explain);
    let narrows = (d.mode == Visibility::Private && d.machine.visibility != Visibility::Private)
        || (d.mode == Visibility::Shared && d.machine.visibility == Visibility::Everyone);
    if narrows {
        ui.add_space(space::XS);
        ui::banner(
            ui,
            ui::Tone::Warning,
            t!(
                "Whoever reaches it now and is not covered by this choice loses access when you save.",
                "Quem acessa agora e não fica coberto por esta escolha perde o acesso ao salvar."
            ),
            None,
        );
    }
    ui.add_space(space::S);
    if d.mode == Visibility::Shared {
        shared_ui(ui, d);
    } else {
        let managers: Vec<String> = d.people.iter().filter(|p| p.manage).map(|p| p.user.email.clone()).collect();
        if !managers.is_empty() {
            ui::caption(
                ui,
                &tf!(
                    "Managers also keep full control: {}. Change them under People and groups.",
                    "Os gestores também mantêm controle total: {}. Mude-os em Pessoas e grupos.",
                    managers.join(", ")
                ),
            );
        }
        let listed = d.people.iter().filter(|p| !p.manage).count() + d.groups.len();
        if listed > 0 {
            ui::caption(
                ui,
                &tf!(
                    "The list of {listed} person(s) and group(s) stays saved, but only counts with People and groups.",
                    "A lista de {listed} pessoa(s) e grupo(s) fica guardada, mas só vale com Pessoas e grupos."
                ),
            );
        }
    }
}

fn close_only(ui: &mut Ui) -> Option<bool> {
    ui.add_space(space::S);
    let mut out = None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui::secondary(ui, t!("Close", "Fechar")).clicked() {
            out = Some(false);
        }
    });
    out
}

/// The list, and the field to add to it.
fn shared_ui(ui: &mut Ui, d: &mut AccessDialog) {
    let resp = ui::search(
        ui,
        &mut d.query,
        t!("Add a person or group: name or e-mail", "Adicionar pessoa ou grupo: nome ou e-mail"),
    );
    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let q = d.query.trim().to_lowercase();
    if !q.is_empty() {
        let me = d.me;
        let groups: Vec<GroupRef> = d
            .all_groups
            .iter()
            .filter(|g| g.name.to_lowercase().contains(&q) && !d.groups.iter().any(|l| l.group.id == g.id))
            .take(4)
            .cloned()
            .collect();
        let people: Vec<UserRef> = d
            .everyone
            .iter()
            .filter(|u| {
                (u.email.to_lowercase().contains(&q) || u.name.to_lowercase().contains(&q))
                    && u.id != d.machine.owner.id
                    && Some(u.id) != me
                    && !d.people.iter().any(|p| p.user.id == u.id)
            })
            .take(6)
            .cloned()
            .collect();
        let mut add_group = None;
        let mut add_person = None;
        if enter {
            add_group = groups.first().cloned();
            if add_group.is_none() {
                add_person = people.first().cloned();
            }
        }
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, pal(ui).border))
            .corner_radius(egui::CornerRadius::same(ui::RADIUS))
            .inner_margin(egui::Margin::same(space::XS as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if groups.is_empty() && people.is_empty() {
                    ui::caption(ui, &tf!("Nobody called “{}”.", "Ninguém chamado “{}”.", d.query.trim()));
                }
                for g in &groups {
                    if suggestion(ui, icons::USERS_THREE, &g.name, t!("group", "grupo")).clicked() {
                        add_group = Some(g.clone());
                    }
                }
                for u in &people {
                    let sub = if u.name.trim().is_empty() { String::new() } else { u.email.clone() };
                    if suggestion(ui, icons::USER, person_label(u), &sub).clicked() {
                        add_person = Some(u.clone());
                    }
                }
            });
        if let Some(g) = add_group {
            d.groups.push(GroupLine { group: g, role: ShareRole::Control });
            d.query.clear();
        } else if let Some(u) = add_person {
            d.people.push(Person { user: u, role: ShareRole::Control, manage: false, listed: false });
            d.query.clear();
        }
        if enter {
            resp.request_focus();
        }
    }
    ui.add_space(space::S);

    // The owner first: always there.
    entry_frame(ui, |ui| {
        ui::row(
            ui,
            |ui| {
                ui::avatar(ui, &d.machine.owner.email, 28.0);
                two_lines(ui, &d.machine.owner);
            },
            |ui| {
                ui::badge(ui, t!("owner · full control", "dono · controle total"), ui::Tone::Neutral);
            },
        );
    });
    let mut remove_group = None;
    for (i, g) in d.groups.iter_mut().enumerate() {
        entry_frame(ui, |ui| {
            ui::row(
                ui,
                |ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(28.0), egui::Sense::hover());
                    ui::paint_centered(ui, rect, icons::USERS_THREE, ui::icon_font(size::ICON + 2.0), pal(ui).muted);
                    ui.vertical(|ui| {
                        ui::truncate(ui, g.group.name.as_str());
                        ui::truncate(ui, ui::muted(t!("group", "grupo")).size(size::SMALL));
                    });
                },
                |ui| {
                    if ui::icon_button(ui, icons::X, &tf!("Remove {}", "Remover {}", g.group.name)).clicked() {
                        remove_group = Some(i);
                    }
                    role_menu(ui, ("g", g.group.id), &mut g.role, None);
                },
            );
        });
    }
    if let Some(i) = remove_group {
        d.groups.remove(i);
    }
    let owner = d.machine.is_owner;
    let mut remove_person = None;
    for (i, p) in d.people.iter_mut().enumerate() {
        entry_frame(ui, |ui| {
            ui::row(
                ui,
                |ui| {
                    ui::avatar(ui, &p.user.email, 28.0);
                    two_lines(ui, &p.user);
                },
                |ui| {
                    let who = person_label(&p.user).to_string();
                    if ui::icon_button(ui, icons::X, &tf!("Remove {}", "Remover {}", who)).clicked() {
                        remove_person = Some(i);
                    }
                    // Only the owner chooses managers; others see a manager as one, untouched.
                    if !owner && p.manage {
                        ui::badge(ui, t!("manager", "gestor"), ui::Tone::Accent);
                    } else {
                        role_menu(ui, ("u", p.user.id), &mut p.role, owner.then_some(&mut p.manage));
                    }
                },
            );
        });
    }
    if let Some(i) = remove_person {
        d.people.remove(i);
    }
    if d.people.is_empty() && d.groups.is_empty() {
        ui.add_space(space::XS);
        ui::caption(
            ui,
            t!("Nobody else yet: add people or groups above.", "Ninguém mais ainda: adicione pessoas ou grupos acima."),
        );
    }
    ui.add_space(space::XS);
    ui::caption(
        ui,
        t!(
            "Full control: screen, keyboard, mouse, clipboard, files and terminal. View only: just the screen.",
            "Controle total: tela, teclado, mouse, área de transferência, arquivos e terminal. Só visualizar: só a tela."
        ),
    );
    if owner {
        ui::caption(
            ui,
            t!(
                "Managers have full control and may share it, rename it, change who sees it and see its history.",
                "Gestores têm controle total e podem compartilhá-lo, renomeá-lo, mudar quem o vê e ver o histórico dele."
            ),
        );
    }
}

/// A person's name and, when it is not the e-mail already, the e-mail under it.
fn two_lines(ui: &mut Ui, u: &UserRef) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui::truncate(ui, person_label(u));
        if !u.name.trim().is_empty() {
            ui::truncate(ui, ui::muted(&u.email).size(size::SMALL));
        }
    });
}

/// One line of the list, at least 44 px tall.
fn entry_frame(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    egui::Frame::new().inner_margin(egui::Margin::symmetric(space::XS as i8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(40.0);
        add(ui);
    });
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(w, 1.0), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, egui::Stroke::new(1.0, p.border));
}

/// A clickable suggestion under the field.
fn suggestion(ui: &mut Ui, icon: &str, name: &str, sub: &str) -> egui::Response {
    let p = pal(ui);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::Vec2::new(w, 36.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, egui::CornerRadius::same(ui::RADIUS), p.hover);
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(space::S, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.label(RichText::new(icon).size(size::ICON).color(p.muted));
    let mut job = egui::text::LayoutJob::default();
    job.append(name, 0.0, egui::TextFormat::simple(egui::FontId::proportional(size::BODY), p.text));
    if !sub.is_empty() {
        job.append(sub, space::S, egui::TextFormat::simple(egui::FontId::proportional(size::SMALL), p.muted));
    }
    child.add(egui::Label::new(job).truncate().selectable(false));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A role as a button sized to its text, with a menu; `manage` adds "can manage" (owner).
fn role_menu(
    ui: &mut Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    role: &mut ShareRole,
    manage: Option<&mut bool>,
) {
    let managing = manage.as_ref().is_some_and(|m| **m);
    let text =
        if managing { t!("full control, can manage", "controle total e pode gerenciar") } else { role_label(*role) };
    ui.push_id(salt, |ui| {
        let mut manage = manage;
        ui.menu_button(format!("{text}  {}", icons::CARET_DOWN), |ui| {
            for r in ShareRole::ALL {
                if ui.radio(!managing && *role == r, role_label(r)).clicked() {
                    *role = r;
                    if let Some(m) = manage.as_deref_mut() {
                        *m = false;
                    }
                    ui.close();
                }
            }
            if let Some(m) = manage {
                ui.separator();
                if ui
                    .radio(*m, t!("full control, can manage", "controle total e pode gerenciar"))
                    .on_hover_text(t!(
                        "A manager may share it, rename it, change who sees it and see its history.",
                        "Um gestor pode compartilhá-lo, renomeá-lo, mudar quem o vê e ver o histórico dele."
                    ))
                    .clicked()
                {
                    *m = true;
                    ui.close();
                }
            }
        });
    });
}

/// Buttons side by side, one chosen (they wrap when narrow). Returns `true` when changed.
pub(super) fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, &str)]) -> bool {
    let p = pal(ui);
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::XS;
        for (v, label) in items {
            let on = *current == *v;
            let (fill, stroke, color) = if on {
                (ui::tint(p.accent, if p.dark { 70 } else { 40 }), egui::Stroke::new(1.0, p.accent), p.text)
            } else {
                (egui::Color32::TRANSPARENT, egui::Stroke::new(1.0, p.border), p.muted)
            };
            let text = RichText::new(*label).color(color);
            let resp = ui.add(
                egui::Button::new(if on { text.strong() } else { text })
                    .fill(fill)
                    .stroke(stroke)
                    .min_size(egui::Vec2::new(0.0, 32.0)),
            );
            if resp.clicked() && !on {
                *current = *v;
                changed = true;
            }
        }
    });
    changed
}

// ---------------------------------------------------------------------------
// Gallery
// ---------------------------------------------------------------------------

#[cfg(test)]
impl AccessDialog {
    /// A dialog with its data already loaded (no server).
    pub(super) fn sample(m: &Machine, mode: Visibility, query: &str) -> Self {
        let u = |id: i64, email: &str, name: &str| UserRef { id, email: email.into(), name: name.into() };
        let everyone = vec![
            u(1, "jeferson.santos.silva@exemplo.com.br", "Jeferson Santos"),
            u(
                2,
                "ana.beatriz.figueiredo.de.albuquerque@consultoria-exemplo.com.br",
                "Ana Beatriz Figueiredo de Albuquerque",
            ),
            u(3, "carlos@exemplo.com", ""),
            u(4, "mariana.costa@exemplo.com", "Mariana Costa"),
            u(5, "marcos.pereira@exemplo.com", "Marcos Pereira"),
        ];
        let groups = vec![
            GroupRef { id: 10, name: "Suporte técnico do escritório de São Paulo".into() },
            GroupRef { id: 11, name: "Família".into() },
            GroupRef { id: 12, name: "Marketing".into() },
        ];
        let shares = Shares {
            users: vec![(everyone[1].clone(), ShareRole::View), (everyone[2].clone(), ShareRole::Files)],
            groups: vec![(groups[0].clone(), ShareRole::Control)],
        };
        let managers = m.is_owner.then(|| vec![everyone[3].clone()]);
        let mut d = Self::empty(m);
        d.take((everyone, groups, shares, managers));
        d.mode = mode;
        d.query = query.into();
        d.me = Some(1);
        d
    }

    pub(super) fn sample_loading(m: &Machine) -> Self {
        Self::empty(m)
    }
}
