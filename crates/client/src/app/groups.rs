//! "Groups": groups of people to share computers with. Anyone can create one and becomes
//! its admin; admins add and remove members and make other admins (there is no owner).

use std::sync::mpsc::Receiver;
use std::time::Instant;

use egui::{Color32, RichText};
use rdesk_common::account::{Api, Session};
use rdesk_common::{t, tf};
use rdesk_proto::api::{Group, GroupRole, UserRef};

use super::account::spawn;

const ERROR: Color32 = Color32::from_rgb(230, 70, 70);

type Loaded = Result<(Vec<Group>, Vec<UserRef>), String>;

pub(super) struct GroupsPage {
    data: Option<Loaded>,
    load_job: Option<Receiver<Loaded>>,
    op_job: Option<Receiver<Result<String, String>>>,
    selected: Option<i64>,
    new_group: String,
    new_member: String,
    new_member_admin: bool,
    rename: Option<String>,
    confirm_delete: bool,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl GroupsPage {
    pub(super) fn new() -> Self {
        Self {
            data: None,
            load_job: None,
            op_job: None,
            selected: None,
            new_group: String::new(),
            new_member: String::new(),
            new_member_admin: false,
            rename: None,
            confirm_delete: false,
            toast: None,
        }
    }

    pub(super) fn reload(&mut self, ctx: &egui::Context) {
        let Some(s) = Session::load() else { return };
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
                    self.new_member.clear();
                    self.new_group.clear();
                }
                Err(e) => self.toast = Some((e, true, Instant::now())),
            }
            self.reload(ctx);
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let me = Session::load().map(|s| s.user.id).unwrap_or(-1);
        let (groups, people) = match &self.data {
            None => {
                ui.spinner();
                return;
            }
            Some(Err(e)) => {
                ui.colored_label(ERROR, e);
                if ui.button(t!("Try again", "Tentar de novo")).clicked() {
                    self.reload(&ctx);
                }
                return;
            }
            Some(Ok((g, p))) => (g.clone(), p.clone()),
        };
        let busy = self.op_job.is_some() || self.load_job.is_some();
        ui.horizontal_top(|ui| {
            // ---- the list ----
            ui.vertical(|ui| {
                ui.set_width(220.0);
                ui.label(RichText::new(t!("My groups", "Meus grupos")).strong().size(16.0));
                for g in &groups {
                    let role = match g.my_role {
                        Some(GroupRole::Admin) => " · admin",
                        Some(GroupRole::Member) => "",
                        None => t!(" · (not a member)", " · (não é membro)"),
                    };
                    if ui.selectable_label(self.selected == Some(g.id), format!("{}{role}", g.name)).clicked() {
                        self.selected = Some(g.id);
                        self.rename = None;
                        self.confirm_delete = false;
                    }
                }
                if groups.is_empty() {
                    ui.weak(t!("You are in no group yet.", "Você ainda não está em nenhum grupo."));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_group).hint_text(t!("New group", "Novo grupo")).desired_width(130.0));
                    let ok = !busy && !self.new_group.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new(t!("Create", "Criar"))).clicked() {
                        let name = self.new_group.trim().to_string();
                        self.run(&ctx, move |api| async move {
                            let g = api.create_group(&name).await?;
                            Ok(tf!("Created {}.", "{} criado.", g.name))
                        });
                    }
                });
                if busy {
                    ui.spinner();
                }
            });
            ui.separator();
            // ---- the selected group ----
            ui.vertical(|ui| {
                let Some(g) = groups.iter().find(|g| Some(g.id) == self.selected).cloned() else {
                    ui.weak(t!("Select or create a group.", "Escolha ou crie um grupo."));
                    return;
                };
                let admin = g.my_role == Some(GroupRole::Admin);
                let sys_admin = Session::load().is_some_and(|s| s.user.is_admin);
                let manage = admin || sys_admin;
                ui.horizontal(|ui| match &mut self.rename {
                    Some(name) => {
                        ui.add(egui::TextEdit::singleline(name).desired_width(200.0));
                        if ui.button(t!("Save", "Salvar")).clicked() {
                            let (id, n) = (g.id, name.trim().to_string());
                            self.rename = None;
                            self.run(&ctx, move |api| async move {
                                Ok(tf!("Renamed to {}.", "Renomeado para {}.", api.rename_group(id, &n).await?.name))
                            });
                        }
                        if ui.button(t!("Cancel", "Cancelar")).clicked() {
                            self.rename = None;
                        }
                    }
                    None => {
                        ui.label(RichText::new(&g.name).strong().size(18.0));
                        if manage && ui.small_button(t!("Rename", "Renomear")).clicked() {
                            self.rename = Some(g.name.clone());
                        }
                    }
                });
                ui.add_space(6.0);
                egui::Grid::new("members").num_columns(3).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
                    for m in &g.members {
                        let label = if m.user.name.is_empty() {
                            m.user.email.clone()
                        } else {
                            format!("{} ({})", m.user.name, m.user.email)
                        };
                        ui.label(if m.user.id == me { tf!("{label} · you", "{label} · você") } else { label });
                        ui.label(if m.role == GroupRole::Admin { "admin" } else { t!("member", "membro") });
                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(manage && !busy, |ui| {
                                let (gid, uid, email) = (g.id, m.user.id, m.user.email.clone());
                                let to = if m.role == GroupRole::Admin { GroupRole::Member } else { GroupRole::Admin };
                                let label = if to == GroupRole::Admin {
                                    t!("Make admin", "Tornar admin")
                                } else {
                                    t!("Make member", "Tornar membro")
                                };
                                if ui.small_button(label).clicked() {
                                    self.run(&ctx, move |api| async move {
                                        api.set_member_role(gid, uid, to).await?;
                                        Ok(tf!("Updated {email}.", "{email} atualizado."))
                                    });
                                }
                                if m.user.id != me && ui.small_button(t!("Remove", "Remover")).clicked() {
                                    let email = m.user.email.clone();
                                    self.run(&ctx, move |api| async move {
                                        api.remove_member(gid, uid).await?;
                                        Ok(tf!("Removed {email}.", "{email} removido."))
                                    });
                                }
                            });
                        });
                        ui.end_row();
                    }
                });
                if manage {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let candidates: Vec<&UserRef> =
                            people.iter().filter(|p| !g.members.iter().any(|m| m.user.id == p.id)).collect();
                        egui::ComboBox::from_id_salt("add-member")
                            .selected_text(if self.new_member.is_empty() {
                                t!("Add a person…", "Adicionar uma pessoa…")
                            } else {
                                &self.new_member
                            })
                            .width(240.0)
                            .show_ui(ui, |ui| {
                                for p in candidates {
                                    ui.selectable_value(&mut self.new_member, p.email.clone(), &p.email);
                                }
                            });
                        ui.checkbox(&mut self.new_member_admin, "admin");
                        let ok = !busy && !self.new_member.is_empty();
                        if ui.add_enabled(ok, egui::Button::new(t!("Add", "Adicionar"))).clicked() {
                            let (gid, email) = (g.id, self.new_member.clone());
                            let role = if self.new_member_admin { GroupRole::Admin } else { GroupRole::Member };
                            self.run(&ctx, move |api| async move {
                                api.add_member(gid, &email, role).await?;
                                Ok(tf!("Added {email}.", "{email} adicionado."))
                            });
                        }
                    });
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if g.my_role.is_some() && ui.add_enabled(!busy, egui::Button::new(t!("Leave the group", "Sair do grupo"))).clicked() {
                        let gid = g.id;
                        self.run(&ctx, move |api| async move {
                            api.remove_member(gid, me).await?;
                            Ok(t!("You left the group.", "Você saiu do grupo.").into())
                        });
                    }
                    if manage {
                        if self.confirm_delete {
                            ui.label(t!(
                                "Delete it? Computers shared with it stop being shared with its members.",
                                "Excluir? Os computadores compartilhados com o grupo deixam de estar acessíveis aos membros dele."
                            ));
                            if ui.button(t!("Delete", "Excluir")).clicked() {
                                self.confirm_delete = false;
                                let (gid, name) = (g.id, g.name.clone());
                                self.run(&ctx, move |api| async move {
                                    api.delete_group(gid).await?;
                                    Ok(tf!("Deleted {name}.", "{name} excluído."))
                                });
                            }
                            if ui.button(t!("Cancel", "Cancelar")).clicked() {
                                self.confirm_delete = false;
                            }
                        } else if ui.add_enabled(!busy, egui::Button::new(t!("Delete group…", "Excluir grupo…"))).clicked() {
                            self.confirm_delete = true;
                        }
                    }
                });
            });
        });
    }
}
