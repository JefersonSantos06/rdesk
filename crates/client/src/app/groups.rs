//! "Groups": groups of people to share computers with. Anyone can create one and becomes
//! its admin; admins add and remove members and make other admins (there is no owner).

use std::sync::mpsc::Receiver;
use std::time::Instant;

use egui::{RichText, Ui};
use rdesk_common::account::{Api, Session};
use rdesk_common::{t, tf};
use rdesk_proto::api::{Group, GroupRole, UserRef};

use super::account::{list_row, menu_item, menu_item_danger, more_menu, rows_card, spawn};
use crate::ui::{self, Tone, icons, size, space};

type Loaded = Result<(Vec<Group>, Vec<UserRef>), String>;

/// Something that asks first.
#[derive(Clone, Debug)]
enum Confirm {
    RemoveMember { gid: i64, uid: i64, email: String },
    Leave { gid: i64, name: String },
    Delete { gid: i64, name: String },
}

pub(super) struct GroupsPage {
    data: Option<Loaded>,
    load_job: Option<Receiver<Loaded>>,
    op_job: Option<Receiver<Result<String, String>>>,
    selected: Option<i64>,
    /// Filters the list of groups.
    search: String,
    /// The "New group" dialog (the name typed so far).
    new_group: Option<String>,
    /// Finds a person to add.
    member_search: String,
    new_member_admin: bool,
    rename: Option<String>,
    confirm: Option<Confirm>,
    /// Who is signed in (their id), and whether they are a system admin.
    me: i64,
    sys_admin: bool,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl GroupsPage {
    pub(super) fn new() -> Self {
        Self {
            data: None,
            load_job: None,
            op_job: None,
            selected: None,
            search: String::new(),
            new_group: None,
            member_search: String::new(),
            new_member_admin: false,
            rename: None,
            confirm: None,
            me: -1,
            sys_admin: false,
            toast: None,
        }
    }

    pub(super) fn reload(&mut self, ctx: &egui::Context) {
        let Some(s) = Session::load() else { return };
        self.me = s.user.id;
        self.sys_admin = s.user.is_admin;
        self.load_job = Some(spawn(ctx, move || async move {
            let api = s.api().map_err(|e| format!("{e:#}"))?;
            let f = async { anyhow::Ok((api.groups().await?, api.directory().await?)) };
            f.await.map_err(|e| format!("{e:#}"))
        }));
    }

    fn run<F, Fut>(&mut self, ctx: &egui::Context, f: F)
    where
        F: FnOnce(Api) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<String>>,
    {
        let Some(s) = Session::load() else { return };
        self.op_job = Some(spawn(ctx, move || async move {
            match s.api() {
                Ok(api) => f(api).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            }
        }));
    }

    pub(super) fn tick(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.load_job
            && let Ok(r) = rx.try_recv()
        {
            self.load_job = None;
            if let Ok((groups, _)) = &r
                && self.selected.is_none_or(|s| !groups.iter().any(|g| g.id == s))
            {
                self.selected = groups.first().map(|g| g.id);
            }
            self.data = Some(r);
        }
        if let Some(rx) = &self.op_job
            && let Ok(r) = rx.try_recv()
        {
            self.op_job = None;
            match r {
                Ok(msg) => {
                    self.toast = Some((msg, false, Instant::now()));
                    self.member_search.clear();
                    self.new_group = None;
                }
                Err(e) => self.toast = Some((e, true, Instant::now())),
            }
            self.reload(ctx);
        }
    }

    pub(super) fn show(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui::row(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui::page_title(
                        ui,
                        t!("Groups", "Grupos"),
                        Some(t!(
                            "People you share computers with, all at once",
                            "Pessoas com quem você compartilha computadores de uma vez"
                        )),
                    );
                });
            },
            |ui| {
                if self.load_job.is_some() || self.op_job.is_some() {
                    ui.spinner();
                }
            },
        );
        ui.add_space(space::M);
        let (groups, people) = match &self.data {
            None => {
                ui::busy(ui, t!("Loading the groups…", "Carregando os grupos…"));
                return;
            }
            Some(Err(e)) => {
                if ui::banner(ui, Tone::Danger, e, Some(t!("Try again", "Tentar de novo"))) {
                    self.reload(&ctx);
                }
                return;
            }
            Some(Ok((g, p))) => (g.clone(), p.clone()),
        };
        if groups.is_empty() {
            ui::empty_state(
                ui,
                icons::USERS_THREE,
                t!("You are in no group yet", "Você ainda não está em nenhum grupo"),
                t!(
                    "Create a group to share computers with several people at once.",
                    "Crie um grupo para compartilhar computadores com várias pessoas de uma vez."
                ),
            );
            ui.vertical_centered(|ui| {
                if ui::primary(ui, ui::with_icon(icons::PLUS, t!("New group", "Novo grupo"))).clicked() {
                    self.new_group = Some(String::new());
                }
            });
        } else if ui.available_width() >= 700.0 {
            ui.horizontal_top(|ui| {
                let h = ui.available_height();
                ui.vertical(|ui| {
                    ui.set_width(270.0);
                    ui.set_min_height(h);
                    self.list_ui(ui, &groups, None);
                });
                ui.add_space(space::S);
                ui.separator();
                ui.add_space(space::S);
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical().id_salt("group-detail").auto_shrink([false, false]).show(ui, |ui| {
                        self.detail_ui(ui, &groups, &people);
                    });
                });
            });
        } else {
            egui::ScrollArea::vertical().id_salt("groups-narrow").auto_shrink([false, false]).show(ui, |ui| {
                self.list_ui(ui, &groups, Some(200.0));
                ui.add_space(space::M);
                ui.separator();
                ui.add_space(space::M);
                self.detail_ui(ui, &groups, &people);
            });
        }
        self.dialogs(&ctx);
    }

    /// The list of groups: "New group", a search, and the groups. `max_h` limits the list
    /// (when the page is narrow and the selected group shows below it).
    fn list_ui(&mut self, ui: &mut Ui, groups: &[Group], max_h: Option<f32>) {
        let p = ui::pal(ui);
        ui.horizontal_wrapped(|ui| {
            if ui::primary(ui, ui::with_icon(icons::PLUS, t!("New group", "Novo grupo"))).clicked() {
                self.new_group = Some(String::new());
            }
        });
        ui.add_space(space::XS);
        if groups.len() > 4 {
            ui::search(ui, &mut self.search, t!("Search groups", "Buscar grupos"));
            ui.add_space(space::XS);
        }
        let q = self.search.trim().to_lowercase();
        let shown: Vec<&Group> = groups.iter().filter(|g| q.is_empty() || g.name.to_lowercase().contains(&q)).collect();
        let mut scroll = egui::ScrollArea::vertical().id_salt("group-list").auto_shrink([false, true]);
        if let Some(h) = max_h {
            scroll = scroll.max_height(h);
        }
        scroll.show(ui, |ui| {
            if shown.is_empty() {
                ui::caption(ui, t!("No group matches.", "Nenhum grupo encontrado."));
            }
            for g in shown {
                let selected = self.selected == Some(g.id);
                let (resp, _) = list_row(ui, 50.0, selected, |ui| {
                    ui::row(
                        ui,
                        |ui| {
                            ui.label(RichText::new(icons::USERS_THREE).size(size::ICON + 2.0).color(p.accent));
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui::truncate(ui, RichText::new(&g.name).strong());
                                let what = match g.my_role {
                                    Some(_) => tf!("{} member(s)", "{} membro(s)", g.members.len()),
                                    None => t!("not a member", "não é membro").into(),
                                };
                                ui::truncate(ui, RichText::new(what).size(size::SMALL).color(p.muted));
                            });
                        },
                        |ui| {
                            if g.my_role == Some(GroupRole::Admin) {
                                ui::badge(ui, "admin", Tone::Accent);
                            }
                        },
                    );
                });
                if resp.clicked() && !selected {
                    self.selected = Some(g.id);
                    self.rename = None;
                    self.member_search.clear();
                }
            }
        });
    }

    /// The selected group: its name, members, adding a person, leaving and deleting.
    fn detail_ui(&mut self, ui: &mut Ui, groups: &[Group], people: &[UserRef]) {
        let ctx = ui.ctx().clone();
        let p = ui::pal(ui);
        let Some(g) = groups.iter().find(|g| Some(g.id) == self.selected).cloned() else {
            ui::caption(ui, t!("Select or create a group.", "Escolha ou crie um grupo."));
            return;
        };
        let me = self.me;
        let admin = g.my_role == Some(GroupRole::Admin);
        let manage = admin || self.sys_admin;
        let busy = self.op_job.is_some() || self.load_job.is_some();

        // ---- the name (renamed in place) ----
        match &mut self.rename {
            Some(name) => {
                let mut save = false;
                let mut cancel = false;
                ui.horizontal_wrapped(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(name)
                            .desired_width(ui.available_width().min(320.0))
                            .margin(egui::Margin::symmetric(8, 6)),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        save = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancel = true;
                    }
                    if ui::primary(ui, t!("Save", "Salvar")).clicked() {
                        save = true;
                    }
                    if ui::ghost(ui, t!("Cancel", "Cancelar")).clicked() {
                        cancel = true;
                    }
                });
                if save && !name.trim().is_empty() {
                    let (id, n) = (g.id, name.trim().to_string());
                    self.rename = None;
                    self.run(&ctx, move |api| async move {
                        Ok(tf!("Renamed to {}.", "Renomeado para {}.", api.rename_group(id, &n).await?.name))
                    });
                } else if cancel {
                    self.rename = None;
                }
            }
            None => {
                ui::row(
                    ui,
                    |ui| {
                        ui::truncate(ui, RichText::new(&g.name).size(size::TITLE - 2.0).strong());
                    },
                    |ui| {
                        if manage && ui::icon_button(ui, icons::PENCIL_SIMPLE, t!("Rename", "Renomear")).clicked() {
                            self.rename = Some(g.name.clone());
                        }
                    },
                );
            }
        }
        let role = match g.my_role {
            Some(GroupRole::Admin) => t!("you are an admin", "você é admin"),
            Some(GroupRole::Member) => t!("you are a member", "você é membro"),
            None => t!("you are not a member", "você não é membro"),
        };
        ui::caption(ui, &tf!("{} member(s) · {role}", "{} membro(s) · {role}", g.members.len()));
        ui.add_space(space::M);

        // ---- members ----
        ui::section(ui, t!("Members", "Membros"));
        ui.add_space(space::XS);
        rows_card(ui, &g.members, |ui, m| {
            ui::row(
                ui,
                |ui| {
                    let name = if m.user.name.is_empty() { &m.user.email } else { &m.user.name };
                    ui::avatar(ui, name, 32.0);
                    let first = if m.user.id == me { tf!("{name} · you", "{name} · você") } else { name.clone() };
                    if m.user.name.is_empty() {
                        // One line: centered like the avatar.
                        ui::truncate(ui, first);
                    } else {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui::truncate(ui, first);
                            ui::truncate(ui, RichText::new(&m.user.email).size(size::SMALL).color(p.muted));
                        });
                    }
                },
                |ui| {
                    if manage {
                        ui.add_enabled_ui(!busy, |ui| {
                            more_menu(ui, t!("More", "Mais"), |ui| {
                                let (gid, uid, email) = (g.id, m.user.id, m.user.email.clone());
                                let to = if m.role == GroupRole::Admin { GroupRole::Member } else { GroupRole::Admin };
                                let (icon, label) = if to == GroupRole::Admin {
                                    (icons::CROWN, t!("Make admin", "Tornar admin"))
                                } else {
                                    (icons::USER, t!("Make member", "Tornar membro"))
                                };
                                if menu_item(ui, icon, label).clicked() {
                                    self.run(&ctx, move |api| async move {
                                        api.set_member_role(gid, uid, to).await?;
                                        Ok(tf!("Updated {email}.", "{email} atualizado."))
                                    });
                                }
                                if m.user.id != me
                                    && menu_item_danger(
                                        ui,
                                        icons::USER_MINUS,
                                        t!("Remove from the group…", "Remover do grupo…"),
                                    )
                                    .clicked()
                                {
                                    self.confirm =
                                        Some(Confirm::RemoveMember { gid, uid, email: m.user.email.clone() });
                                }
                            });
                        });
                    }
                    if m.role == GroupRole::Admin {
                        ui::badge(ui, "admin", Tone::Accent);
                    } else {
                        ui::badge(ui, t!("member", "membro"), Tone::Neutral);
                    }
                },
            );
        });

        // ---- adding a person ----
        if manage {
            ui.add_space(space::L);
            ui::section(ui, t!("Add a person", "Adicionar uma pessoa"));
            ui.add_space(space::XS);
            ui::search(ui, &mut self.member_search, t!("Name or e-mail", "Nome ou e-mail"));
            ui.checkbox(&mut self.new_member_admin, t!("as an admin of the group", "como admin do grupo"));
            let q = self.member_search.trim().to_lowercase();
            if !q.is_empty() {
                let found: Vec<&UserRef> = people
                    .iter()
                    .filter(|u| !g.members.iter().any(|m| m.user.id == u.id))
                    .filter(|u| u.email.to_lowercase().contains(&q) || u.name.to_lowercase().contains(&q))
                    .take(6)
                    .collect();
                if found.is_empty() {
                    ui::caption(
                        ui,
                        t!(
                            "Nobody found (only people who can sign in to this server).",
                            "Ninguém encontrado (só pessoas que podem entrar neste servidor)."
                        ),
                    );
                } else {
                    rows_card(ui, &found, |ui, u| {
                        ui::row(
                            ui,
                            |ui| {
                                ui::avatar(ui, if u.name.is_empty() { &u.email } else { &u.name }, 28.0);
                                if u.name.is_empty() {
                                    ui::truncate(ui, &u.email);
                                } else {
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 0.0;
                                        ui::truncate(ui, &u.name);
                                        ui::truncate(ui, RichText::new(&u.email).size(size::SMALL).color(p.muted));
                                    });
                                }
                            },
                            |ui| {
                                if ui
                                    .add_enabled(
                                        !busy,
                                        egui::Button::new(ui::with_icon(icons::USER_PLUS, t!("Add", "Adicionar")))
                                            .min_size(egui::vec2(0.0, 32.0)),
                                    )
                                    .clicked()
                                {
                                    let (gid, email) = (g.id, u.email.clone());
                                    let role = if self.new_member_admin { GroupRole::Admin } else { GroupRole::Member };
                                    self.run(&ctx, move |api| async move {
                                        api.add_member(gid, &email, role).await?;
                                        Ok(tf!("Added {email}.", "{email} adicionado."))
                                    });
                                }
                            },
                        );
                    });
                }
            }
        }

        // ---- leaving, deleting ----
        ui.add_space(space::L);
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                if g.my_role.is_some()
                    && ui::secondary(ui, ui::with_icon(icons::SIGN_OUT, t!("Leave the group", "Sair do grupo")))
                        .clicked()
                {
                    self.confirm = Some(Confirm::Leave { gid: g.id, name: g.name.clone() });
                }
                if manage
                    && ui::danger(ui, ui::with_icon(icons::TRASH, t!("Delete group…", "Excluir grupo…"))).clicked()
                {
                    self.confirm = Some(Confirm::Delete { gid: g.id, name: g.name.clone() });
                }
            });
        });
    }

    /// "New group" and the questions asked before removing, leaving or deleting.
    fn dialogs(&mut self, ctx: &egui::Context) {
        let busy = self.op_job.is_some();
        if let Some(name) = &mut self.new_group {
            let mut create = false;
            let d = ui::dialog_with_footer(
                ctx,
                "new-group",
                t!("New group", "Novo grupo"),
                420.0,
                |ui| {
                    ui::caption(
                        ui,
                        t!(
                            "You become its admin; then add people and share computers with it.",
                            "Você vira admin dele; depois adicione pessoas e compartilhe computadores com ele."
                        ),
                    );
                    ui.add_space(space::XS);
                    let resp = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text(t!("Group name", "Nome do grupo"))
                            .desired_width(f32::INFINITY)
                            .margin(egui::Margin::symmetric(8, 6)),
                    );
                    if !resp.has_focus() && name.is_empty() {
                        resp.request_focus();
                    }
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                },
                |ui| ui::dialog_buttons(ui, t!("Create", "Criar"), false, &[t!("Cancel", "Cancelar")]),
            );
            let (enter, button) = d.inner;
            let button = if enter { Some(ui::DialogButton::Primary) } else { button };
            let name = name.trim().to_string();
            match button {
                Some(ui::DialogButton::Primary) if !name.is_empty() && !busy => create = true,
                Some(ui::DialogButton::Other(_)) => self.new_group = None,
                _ if d.closed => self.new_group = None,
                _ => {}
            }
            if create {
                self.run(ctx, move |api| async move {
                    let g = api.create_group(&name).await?;
                    Ok(tf!("Created {}.", "{} criado.", g.name))
                });
            }
        }
        let Some(c) = self.confirm.clone() else { return };
        let me = self.me;
        let answer = match &c {
            Confirm::RemoveMember { email, .. } => ui::confirm(
                ctx,
                "group-remove-member",
                &tf!("Remove {} from the group?", "Remover {} do grupo?", email),
                t!(
                    "They stop reaching the computers shared with the group. You can add them again.",
                    "A pessoa deixa de acessar os computadores compartilhados com o grupo. Dá para adicioná-la de novo."
                ),
                t!("Remove", "Remover"),
                true,
            ),
            Confirm::Leave { name, .. } => ui::confirm(
                ctx,
                "group-leave",
                &tf!("Leave {}?", "Sair de {}?", name),
                t!(
                    "You stop reaching the computers shared with the group. An admin of it can add you again.",
                    "Você deixa de acessar os computadores compartilhados com o grupo. Um admin dele pode adicionar você de novo."
                ),
                t!("Leave", "Sair"),
                true,
            ),
            Confirm::Delete { name, .. } => ui::confirm(
                ctx,
                "group-delete",
                &tf!("Delete {}?", "Excluir {}?", name),
                t!(
                    "Computers shared with it stop being shared with its members.",
                    "Os computadores compartilhados com o grupo deixam de estar acessíveis aos membros dele."
                ),
                t!("Delete", "Excluir"),
                true,
            ),
        };
        match answer {
            Some(true) => {
                self.confirm = None;
                match c {
                    Confirm::RemoveMember { gid, uid, email } => self.run(ctx, move |api| async move {
                        api.remove_member(gid, uid).await?;
                        Ok(tf!("Removed {email}.", "{email} removido."))
                    }),
                    Confirm::Leave { gid, .. } => self.run(ctx, move |api| async move {
                        api.remove_member(gid, me).await?;
                        Ok(t!("You left the group.", "Você saiu do grupo.").into())
                    }),
                    Confirm::Delete { gid, name } => self.run(ctx, move |api| async move {
                        api.delete_group(gid).await?;
                        Ok(tf!("Deleted {name}.", "{name} excluído."))
                    }),
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

#[cfg(test)]
impl GroupsPage {
    /// A page with these groups loaded (no network).
    fn sample(groups: Vec<Group>, people: Vec<UserRef>) -> Self {
        let mut g = Self::new();
        g.me = 1;
        g.selected = groups.first().map(|g| g.id);
        g.data = Some(Ok((groups, people)));
        g
    }

    fn sample_people(n: usize) -> Vec<UserRef> {
        let names = [
            ("Jeferson Santos da Silva", "jeferson.santos.silva@exemplo.com.br"),
            ("", "maria.aparecida.de.oliveira.figueiredo@empresa-de-tecnologia.com.br"),
            ("Ana Beatriz", "ana.b@exemplo.com"),
            ("Carlos Eduardo Nascimento Pereira Júnior", "carlos.eduardo.nascimento.pereira.junior@gmail.com"),
            ("Lia", "lia@x.io"),
        ];
        (0..n)
            .map(|i| {
                let (name, email) = names[i % names.len()];
                UserRef {
                    id: i as i64 + 1,
                    name: name.into(),
                    email: if i < names.len() { email.into() } else { format!("{i}.{email}") },
                }
            })
            .collect()
    }

    fn sample_groups() -> Vec<Group> {
        use rdesk_proto::api::GroupMember;
        let people = Self::sample_people(14);
        let members = |n: usize| -> Vec<GroupMember> {
            people
                .iter()
                .take(n)
                .enumerate()
                .map(|(i, u)| GroupMember {
                    user: u.clone(),
                    role: if i % 4 == 0 { GroupRole::Admin } else { GroupRole::Member },
                })
                .collect()
        };
        vec![
            Group {
                id: 1,
                name: "Equipe de suporte técnico do escritório central de São Paulo".into(),
                my_role: Some(GroupRole::Admin),
                members: members(12),
            },
            Group { id: 2, name: "Família".into(), my_role: Some(GroupRole::Member), members: members(3) },
            Group { id: 3, name: "Servidores".into(), my_role: None, members: members(2) },
            Group { id: 4, name: "Oficina de robótica".into(), my_role: Some(GroupRole::Admin), members: members(5) },
            Group { id: 5, name: "Estagiários".into(), my_role: Some(GroupRole::Member), members: members(4) },
        ]
    }
}

/// Screens for the ui gallery (see `gallery.rs`): sample data, long texts.
#[cfg(test)]
pub(super) fn gallery() -> Vec<super::gallery::Screen> {
    use super::gallery::{screen, with_sidebar};
    fn page(mut g: GroupsPage) -> impl FnMut(&mut Ui) {
        move |ui: &mut Ui| with_sidebar(ui, super::Page::Groups, |ui| g.show(ui))
    }
    vec![
        screen("groups-loading", || page(GroupsPage::new())),
        screen("groups-error", || {
            let mut g = GroupsPage::new();
            g.data =
                Some(Err("não deu para falar com o servidor: tempo esgotado (https://rdesk.exemplo.com.br)".into()));
            page(g)
        }),
        screen("groups-empty", || page(GroupsPage::sample(Vec::new(), GroupsPage::sample_people(3)))),
        screen("groups-many", || page(GroupsPage::sample(GroupsPage::sample_groups(), GroupsPage::sample_people(14)))),
        screen("groups-add-person", || {
            let mut g = GroupsPage::sample(GroupsPage::sample_groups(), GroupsPage::sample_people(30));
            g.selected = Some(2);
            g.sys_admin = true;
            g.member_search = "a".into();
            page(g)
        }),
        screen("groups-rename", || {
            let mut g = GroupsPage::sample(GroupsPage::sample_groups(), GroupsPage::sample_people(14));
            g.rename = Some("Equipe de suporte técnico do escritório central".into());
            page(g)
        }),
        screen("groups-new", || {
            let mut g = GroupsPage::sample(GroupsPage::sample_groups(), GroupsPage::sample_people(14));
            g.new_group = Some("Oficina de robótica da escola".into());
            page(g)
        }),
        screen("groups-confirm-remove", || {
            let mut g = GroupsPage::sample(GroupsPage::sample_groups(), GroupsPage::sample_people(14));
            g.confirm = Some(Confirm::RemoveMember {
                gid: 1,
                uid: 2,
                email: "maria.aparecida.de.oliveira.figueiredo@empresa-de-tecnologia.com.br".into(),
            });
            page(g)
        }),
        screen("groups-confirm-delete", || {
            let mut g = GroupsPage::sample(GroupsPage::sample_groups(), GroupsPage::sample_people(14));
            g.confirm = Some(Confirm::Delete {
                gid: 1,
                name: "Equipe de suporte técnico do escritório central de São Paulo".into(),
            });
            page(g)
        }),
    ]
}
