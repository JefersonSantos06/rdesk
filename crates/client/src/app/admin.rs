//! "Admin" (system admins only): who may sign in to the server, every computer, and which
//! rdesk version the computers get (first the test computers, then everyone).

use std::sync::mpsc::Receiver;
use std::time::Instant;

use egui::{RichText, Ui};
use rdesk_common::account::Session;
use rdesk_common::directory::ago;
use rdesk_common::{t, tf};
use rdesk_proto::api::{AdminUser, Machine, ReleaseInfo, Releases, Ring, UpdateUser, Visibility};

use super::account::{menu_item, menu_item_danger, more_menu, now, rows_card, spawn};
use crate::ui::{self, Tone, icons, size, space};

enum Op {
    Add { email: String, admin: bool },
    Update { id: i64, email: String, change: UpdateUser },
    Remove { id: i64, email: String },
    Release { version: String, ring: Ring },
    UpdateServer { version: String },
    Ring { id: i64, name: String, ring: Ring },
    Check,
}

type Loaded = Result<(Vec<AdminUser>, Vec<Machine>, Releases), String>;

/// The tabs of the page.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Tab {
    Users,
    Computers,
    Versions,
}

pub(super) struct AdminPage {
    tab: Tab,
    users: Option<Result<Vec<AdminUser>, String>>,
    machines: Vec<Machine>,
    releases: Option<Releases>,
    /// Releasing to everyone asks first.
    confirm_release: Option<String>,
    /// Updating the server asks first.
    confirm_server: Option<String>,
    load_job: Option<Receiver<Loaded>>,
    op_job: Option<Receiver<Result<String, String>>>,
    new_email: String,
    new_admin: bool,
    user_search: String,
    machine_search: String,
    confirm_remove: Option<AdminUser>,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl AdminPage {
    pub(super) fn new() -> Self {
        Self {
            tab: Tab::Users,
            users: None,
            machines: Vec::new(),
            releases: None,
            confirm_release: None,
            confirm_server: None,
            load_job: None,
            op_job: None,
            new_email: String::new(),
            new_admin: false,
            user_search: String::new(),
            machine_search: String::new(),
            confirm_remove: None,
            toast: None,
        }
    }

    pub(super) fn reload(&mut self, ctx: &egui::Context, session: Option<&Session>) {
        let Some(s) = session.cloned() else { return };
        self.load_job = Some(spawn(ctx, move || async move {
            let api = s.api().map_err(|e| format!("{e:#}"))?;
            let f = async { anyhow::Ok((api.users().await?, api.admin_machines().await?, api.releases().await?)) };
            f.await.map_err(|e| format!("{e:#}"))
        }));
    }

    fn run(&mut self, ctx: &egui::Context, session: Option<&Session>, op: Op) {
        let Some(s) = session.cloned() else { return };
        self.op_job = Some(spawn(ctx, move || async move {
            let api = s.api().map_err(|e| format!("{e:#}"))?;
            let r = match op {
                Op::Add { email, admin } => {
                    api.add_user(&email, admin).await.map(|u| tf!("{} can now sign in.", "{} já pode entrar.", u.email))
                }
                Op::Update { id, email, change } => {
                    api.update_user(id, &change).await.map(|_| tf!("Updated {email}.", "{email} atualizado."))
                }
                Op::Remove { id, email } => {
                    api.remove_user(id).await.map(|_| tf!("Removed {email}.", "{email} removido."))
                }
                Op::Release { version, ring } => api.release_version(&version, ring).await.map(|_| match ring {
                    Ring::Test => {
                        tf!(
                            "{version} released to the test computers.",
                            "{version} liberada para os computadores de teste."
                        )
                    }
                    Ring::All => tf!("{version} released to everyone.", "{version} liberada para todos."),
                }),
                Op::Ring { id, name, ring } => api.set_ring(id, ring).await.map(|_| match ring {
                    Ring::Test => {
                        tf!("{name} now gets new versions first.", "{name} agora recebe as versões novas antes.")
                    }
                    Ring::All => tf!(
                        "{name} gets new versions with everyone.",
                        "{name} recebe as versões novas junto com todos."
                    ),
                }),
                Op::UpdateServer { version } => api.update_server(&version).await.map(|_| {
                    tf!(
                        "The server is installing {version}; it restarts in a moment.",
                        "O servidor está instalando a {version}; ele reinicia em instantes."
                    )
                }),
                Op::Check => {
                    let r = api
                        .check_releases()
                        .await
                        .map(|_| t!("Looking for new versions…", "Procurando versões novas…").to_string());
                    // Give the server a moment before the list is read again.
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    r
                }
            };
            r.map_err(|e| format!("{e:#}"))
        }));
    }

    pub(super) fn tick(&mut self, ctx: &egui::Context, session: Option<&Session>) {
        if let Some(rx) = &self.load_job
            && let Ok(r) = rx.try_recv()
        {
            self.load_job = None;
            match r {
                Ok((users, machines, releases)) => {
                    self.users = Some(Ok(users));
                    self.machines = machines;
                    self.releases = Some(releases);
                }
                Err(e) => self.users = Some(Err(e)),
            }
        }
        if let Some(rx) = &self.op_job
            && let Ok(r) = rx.try_recv()
        {
            self.op_job = None;
            match r {
                Ok(msg) => {
                    self.toast = Some((msg, false, Instant::now()));
                    self.new_email.clear();
                    self.new_admin = false;
                }
                Err(e) => self.toast = Some((e, true, Instant::now())),
            }
            self.reload(ctx, session);
        }
    }

    pub(super) fn show(&mut self, ui: &mut Ui, session: Option<&Session>) {
        let ctx = ui.ctx().clone();
        ui::row(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui::page_title(
                        ui,
                        "Admin",
                        Some(t!(
                            "Who can sign in, every computer, and the rdesk versions",
                            "Quem pode entrar, todos os computadores e as versões do rdesk"
                        )),
                    );
                });
            },
            |ui| {
                if ui::icon_button(ui, icons::ARROW_CLOCKWISE, t!("Refresh", "Atualizar")).clicked() {
                    self.reload(&ctx, session);
                }
                if self.load_job.is_some() || self.op_job.is_some() {
                    ui.spinner();
                }
            },
        );
        ui.add_space(space::S);
        ui::tabs(
            ui,
            &mut self.tab,
            &[
                (Tab::Users, t!("Users", "Usuários")),
                (Tab::Computers, t!("Computers", "Computadores")),
                (Tab::Versions, t!("Versions", "Versões")),
            ],
        );
        ui.add_space(space::M);
        if let Some(Err(e)) = &self.users {
            let e = e.clone();
            if ui::banner(ui, Tone::Danger, &e, Some(t!("Try again", "Tentar de novo"))) {
                self.reload(&ctx, session);
            }
            return;
        }
        egui::ScrollArea::vertical().id_salt(("admin", self.tab)).auto_shrink([false, false]).show(ui, |ui| {
            match self.tab {
                Tab::Users => self.users_ui(ui, session),
                Tab::Computers => self.machines_ui(ui, session),
                Tab::Versions => self.versions_ui(ui, session),
            }
            ui.add_space(space::L);
        });
        self.dialogs(&ctx, session);
    }

    fn users_ui(&mut self, ui: &mut Ui, session: Option<&Session>) {
        let ctx = ui.ctx().clone();
        let me = session.map(|s| s.user.id);
        let p = ui::pal(ui);
        let busy = self.op_job.is_some();

        // ---- allowing an e-mail ----
        ui::card(ui, |ui| {
            ui::section(ui, t!("Allow an e-mail", "Liberar e-mail"));
            ui::caption(
                ui,
                t!(
                    "Only these e-mails can sign in (with Google or GitHub, plus a second factor).",
                    "Só estes e-mails podem entrar (com Google ou GitHub, mais um segundo fator)."
                ),
            );
            ui.add_space(space::XS);
            ui.horizontal_wrapped(|ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.new_email)
                        .hint_text(t!("e-mail to allow", "e-mail a liberar"))
                        .desired_width(ui.available_width().min(320.0))
                        .margin(egui::Margin::symmetric(8, 6)),
                );
                ui.checkbox(&mut self.new_admin, t!("system admin", "admin do sistema"));
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ok = !self.new_email.trim().is_empty() && !busy;
                let clicked = ui
                    .add_enabled(
                        ok,
                        egui::Button::new(
                            RichText::new(ui::with_icon(icons::PLUS, t!("Allow", "Liberar")))
                                .color(p.on_accent)
                                .strong(),
                        )
                        .fill(p.accent)
                        .stroke(egui::Stroke::NONE)
                        .min_size(egui::vec2(0.0, 32.0)),
                    )
                    .clicked();
                if ok && (clicked || enter) {
                    let op = Op::Add { email: self.new_email.trim().to_string(), admin: self.new_admin };
                    self.run(&ctx, session, op);
                }
            });
        });
        ui.add_space(space::M);

        let users = match &self.users {
            None => {
                ui::busy(ui, t!("Loading…", "Carregando…"));
                return;
            }
            Some(Err(_)) => return,
            Some(Ok(u)) => u.clone(),
        };
        ui::search(ui, &mut self.user_search, t!("Search by e-mail or name", "Buscar por e-mail ou nome"));
        ui.add_space(space::XS);
        let q = self.user_search.trim().to_lowercase();
        let shown: Vec<&AdminUser> = users
            .iter()
            .filter(|u| q.is_empty() || u.email.to_lowercase().contains(&q) || u.name.to_lowercase().contains(&q))
            .collect();
        ui::caption(ui, &tf!("{} of {} user(s)", "{} de {} usuário(s)", shown.len(), users.len()));
        ui.add_space(space::XS);
        if shown.is_empty() {
            ui::caption(ui, t!("Nobody matches.", "Ninguém encontrado."));
            return;
        }
        let mut op = None;
        rows_card(ui, &shown, |ui, u| {
            ui::row(
                ui,
                |ui| {
                    ui::avatar(ui, &u.email, 32.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let email = RichText::new(&u.email).strong();
                        ui::truncate(ui, if u.disabled { email.color(p.muted) } else { email });
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = space::XS;
                            if u.is_admin {
                                ui::badge(ui, t!("system admin", "admin do sistema"), Tone::Accent);
                            }
                            if u.disabled {
                                ui::badge(ui, t!("disabled", "desativado"), Tone::Danger);
                            }
                            match (u.has_totp, u.passkeys) {
                                (false, 0) => {
                                    ui::badge(ui, t!("no second factor yet", "sem segundo fator ainda"), Tone::Warning);
                                }
                                (totp, n) => {
                                    if totp {
                                        ui::badge(ui, "TOTP", Tone::Success);
                                    }
                                    if n > 0 {
                                        ui::badge(ui, &tf!("{n} passkey(s)", "{n} passkey(s)"), Tone::Success);
                                    }
                                }
                            }
                            let last = match u.last_login {
                                Some(t) => tf!("last sign-in {}", "último acesso {}", ago(t, now())),
                                None => t!("never signed in", "nunca entrou").into(),
                            };
                            let name = if u.name.is_empty() { last } else { format!("{} · {last}", u.name) };
                            ui.add(egui::Label::new(RichText::new(name).size(size::SMALL).color(p.muted)).wrap());
                        });
                    });
                },
                |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        more_menu(ui, t!("More", "Mais"), |ui| {
                            let (icon, label) = if u.is_admin {
                                (icons::USER, t!("Make user", "Tornar usuário"))
                            } else {
                                (icons::CROWN, t!("Make admin", "Tornar admin"))
                            };
                            if menu_item(ui, icon, label).clicked() {
                                let change = UpdateUser { is_admin: Some(!u.is_admin), disabled: None };
                                op = Some(Op::Update { id: u.id, email: u.email.clone(), change });
                            }
                            let (icon, label) = if u.disabled {
                                (icons::CHECK_CIRCLE, t!("Enable", "Ativar"))
                            } else {
                                (icons::PROHIBIT, t!("Disable", "Desativar"))
                            };
                            if menu_item(ui, icon, label)
                                .on_hover_text(t!(
                                    "A disabled user cannot sign in and is signed out everywhere",
                                    "Um usuário desativado não consegue entrar e sai de todos os lugares"
                                ))
                                .clicked()
                            {
                                let change = UpdateUser { is_admin: None, disabled: Some(!u.disabled) };
                                op = Some(Op::Update { id: u.id, email: u.email.clone(), change });
                            }
                            if Some(u.id) != me {
                                ui.separator();
                                if menu_item_danger(ui, icons::TRASH, t!("Remove…", "Remover…")).clicked() {
                                    self.confirm_remove = Some((*u).clone());
                                }
                            }
                        });
                    });
                },
            );
        });
        if let Some(op) = op {
            self.run(&ctx, session, op);
        }
    }

    fn machines_ui(&mut self, ui: &mut Ui, session: Option<&Session>) {
        let ctx = ui.ctx().clone();
        let p = ui::pal(ui);
        ui::caption(
            ui,
            t!(
                "Admins see every computer, but connect only to those shared with them.",
                "Admins veem todos os computadores, mas só se conectam aos compartilhados com eles."
            ),
        );
        ui.add_space(space::S);
        if self.users.is_none() {
            ui::busy(ui, t!("Loading…", "Carregando…"));
            return;
        }
        ui::search(
            ui,
            &mut self.machine_search,
            t!("Search by name, owner or version", "Buscar por nome, dono ou versão"),
        );
        ui.add_space(space::XS);
        let q = self.machine_search.trim().to_lowercase();
        let shown: Vec<&Machine> = self
            .machines
            .iter()
            .filter(|m| {
                q.is_empty()
                    || m.name.to_lowercase().contains(&q)
                    || m.owner.email.to_lowercase().contains(&q)
                    || m.version.contains(&q)
            })
            .collect();
        ui::caption(ui, &tf!("{} of {} computer(s)", "{} de {} computador(es)", shown.len(), self.machines.len()));
        ui.add_space(space::XS);
        if shown.is_empty() {
            ui::caption(ui, t!("No computer matches.", "Nenhum computador encontrado."));
            return;
        }
        let busy = self.op_job.is_some();
        let mut ring_op = None;
        rows_card(ui, &shown, |ui, m| {
            ui::row(
                ui,
                |ui| {
                    ui.label(RichText::new(os_icon(&m.os)).size(size::ICON + 4.0).color(p.muted));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui::truncate(ui, RichText::new(&m.name).strong());
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = space::XS;
                            let (text, tone) = if m.in_session {
                                (t!("in use", "em uso").to_string(), Tone::Warning)
                            } else if m.online {
                                ("online".to_string(), Tone::Success)
                            } else {
                                match m.last_seen {
                                    Some(at) => (format!("offline · {}", ago(at, now())), Tone::Neutral),
                                    None => (t!("never connected", "nunca se conectou").to_string(), Tone::Neutral),
                                }
                            };
                            ui::badge(ui, &text, tone);
                            let vis = match m.visibility {
                                Visibility::Private => t!("only the owner", "só o dono"),
                                Visibility::Shared => t!("shared", "compartilhado"),
                                Visibility::Everyone => t!("everyone", "todos"),
                            };
                            let version = if m.version.is_empty() { "?" } else { &m.version };
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("{} · {vis} · {version}", m.owner.email))
                                        .size(size::SMALL)
                                        .color(p.muted),
                                )
                                .wrap(),
                            );
                        });
                    });
                },
                |ui| {
                    let mut test = m.ring == Ring::Test;
                    let hover =
                        t!("Gets new versions first, before everyone", "Recebe as versões novas antes de todos");
                    let changed =
                        ui.add_enabled_ui(!busy, |ui| ui::toggle(ui, &mut test).on_hover_text(hover)).inner.changed();
                    if changed {
                        let ring = if test { Ring::Test } else { Ring::All };
                        ring_op = Some(Op::Ring { id: m.id, name: m.name.clone(), ring });
                    }
                    ui.add(
                        egui::Label::new(RichText::new(t!("Test computer", "Computador de teste")).size(size::SMALL))
                            .selectable(false),
                    )
                    .on_hover_text(hover);
                },
            );
        });
        if let Some(op) = ring_op {
            self.run(&ctx, session, op);
        }
    }

    /// New versions found on GitHub, and the two steps of releasing one.
    fn versions_ui(&mut self, ui: &mut Ui, session: Option<&Session>) {
        let ctx = ui.ctx().clone();
        let busy = self.op_job.is_some();
        let Some(r) = self.releases.clone() else {
            ui::busy(ui, t!("Loading…", "Carregando…"));
            return;
        };
        let none = t!("none", "nenhuma");
        ui::card(ui, |ui| {
            ui::row(
                ui,
                |ui| ui::section(ui, t!("rdesk versions", "Versões do rdesk")),
                |ui| {
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(ui::with_icon(
                                icons::ARROWS_CLOCKWISE,
                                t!("Check now", "Verificar agora"),
                            ))
                            .min_size(egui::vec2(0.0, 32.0)),
                        )
                        .clicked()
                    {
                        self.run(&ctx, session, Op::Check);
                    }
                },
            );
            ui.add_space(space::XS);
            let checked = match (&r.check_error, r.last_check) {
                (_, Some(t)) => tf!("on GitHub {}", "no GitHub {}", ago(t, now())),
                (_, None) => t!("not yet", "ainda não").into(),
            };
            ui::facts(
                ui,
                "admin-versions",
                &[
                    (t!("Server", "Servidor"), r.server_version.clone()),
                    (t!("Released to everyone", "Liberada para todos"), r.all.clone().unwrap_or_else(|| none.into())),
                    (
                        t!("Released to the test computers", "Liberada para os de teste"),
                        r.test.clone().unwrap_or_else(|| none.into()),
                    ),
                    (t!("Last check", "Última verificação"), checked),
                ],
            );
            if let Some(u) = &r.server_update {
                ui.add_space(space::XS);
                ui::caption(ui, &tf!("Server update: {u}", "Atualização do servidor: {u}"));
            }
            if let Some(e) = &r.check_error {
                ui.add_space(space::S);
                ui::banner(
                    ui,
                    Tone::Danger,
                    &tf!("Could not look for new versions: {e}", "Não deu para procurar versões novas: {e}"),
                    None,
                );
            }
        });
        ui.add_space(space::L);
        ui::section(ui, t!("Releases", "Versões publicadas"));
        ui.add_space(space::XS);
        if r.releases.is_empty() {
            ui::caption(ui, t!("No versions found yet.", "Nenhuma versão encontrada ainda."));
            return;
        }
        let newer = |v: &str, than: &Option<String>| {
            than.as_deref()
                .is_none_or(|t| rdesk_proto::release::parse_version(v) > rdesk_proto::release::parse_version(t))
        };
        let p = ui::pal(ui);
        let shown: Vec<&ReleaseInfo> = r.releases.iter().take(6).collect();
        let mut op = None;
        rows_card(ui, &shown, |ui, rel| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = space::S;
                ui.label(RichText::new(icons::PACKAGE).size(size::ICON + 2.0).color(p.muted));
                ui.label(RichText::new(&rel.version).strong()).on_hover_text(if rel.notes.is_empty() {
                    t!("No notes", "Sem notas")
                } else {
                    &rel.notes
                });
                if r.all.as_deref() == Some(&rel.version) {
                    ui::badge(ui, t!("everyone", "todos"), Tone::Success);
                } else if r.test.as_deref() == Some(&rel.version) {
                    ui::badge(ui, t!("test", "teste"), Tone::Accent);
                }
                if rel.version == r.server_version {
                    ui::badge(ui, t!("on the server", "no servidor"), Tone::Neutral);
                }
            });
            ui.add(
                egui::Label::new(
                    RichText::new(tf!(
                        "Published {} · running on {} computer(s) · {}",
                        "Publicada {} · rodando em {} computador(es) · {}",
                        ago(rel.published_at, now()),
                        rel.running,
                        rel.platforms.join(", ")
                    ))
                    .size(size::SMALL)
                    .color(p.muted),
                )
                .wrap(),
            );
            let to_test = newer(&rel.version, &r.test) && newer(&rel.version, &r.all);
            let to_all = newer(&rel.version, &r.all);
            let server_newer = rdesk_proto::release::parse_version(&rel.version)
                > rdesk_proto::release::parse_version(&r.server_version);
            let server = r.server_can_update && server_newer && rel.platforms.iter().any(|p| p.starts_with("linux"));
            if to_test || to_all || server {
                ui.add_space(space::XS);
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        if to_test
                            && ui::secondary(ui, t!("Release to test computers", "Liberar para os de teste"))
                                .on_hover_text(t!(
                                    "Only the computers marked as test computers (Computers tab)",
                                    "Só os computadores marcados como de teste (aba Computadores)"
                                ))
                                .clicked()
                        {
                            op = Some(Op::Release { version: rel.version.clone(), ring: Ring::Test });
                        }
                        if to_all && ui::secondary(ui, t!("Release to everyone…", "Liberar para todos…")).clicked()
                        {
                            self.confirm_release = Some(rel.version.clone());
                        }
                        if server
                            && ui::secondary(ui, t!("Update the server…", "Atualizar o servidor…"))
                                .on_hover_text(t!(
                                    "Backs up the database, installs it and restarts; goes back if it fails",
                                    "Faz backup do banco, instala e reinicia; volta atrás se falhar"
                                ))
                                .clicked()
                        {
                            self.confirm_server = Some(rel.version.clone());
                        }
                    });
                });
            }
        });
        if let Some(op) = op {
            self.run(&ctx, session, op);
        }
    }

    /// The questions asked before releasing to everyone, updating the server, removing.
    fn dialogs(&mut self, ctx: &egui::Context, session: Option<&Session>) {
        if let Some(version) = self.confirm_release.clone() {
            match ui::confirm(
                ctx,
                "admin-release",
                &tf!("Release {version} to everyone?", "Liberar a {version} para todos?"),
                t!(
                    "Every computer downloads it and installs it when nobody is connected to it. \
                     Computers do not go back to an older version by themselves.",
                    "Cada computador baixa e instala quando ninguém está conectado a ele. \
                     Os computadores não voltam sozinhos para uma versão mais antiga."
                ),
                t!("Release to everyone", "Liberar para todos"),
                false,
            ) {
                Some(true) => {
                    self.confirm_release = None;
                    self.run(ctx, session, Op::Release { version, ring: Ring::All });
                }
                Some(false) => self.confirm_release = None,
                None => {}
            }
        }
        if let Some(version) = self.confirm_server.clone() {
            match ui::confirm(
                ctx,
                "admin-update-server",
                &tf!("Update the server to {version}?", "Atualizar o servidor para a {version}?"),
                t!(
                    "It backs up the database, installs the new version and restarts (everyone is disconnected \
                     for a moment). If it fails, it goes back to the current version.",
                    "Ele faz backup do banco, instala a versão nova e reinicia (todos ficam desconectados por um \
                     instante). Se falhar, volta para a versão atual."
                ),
                t!("Update the server", "Atualizar o servidor"),
                false,
            ) {
                Some(true) => {
                    self.confirm_server = None;
                    self.run(ctx, session, Op::UpdateServer { version });
                }
                Some(false) => self.confirm_server = None,
                None => {}
            }
        }
        if let Some(u) = self.confirm_remove.clone() {
            match ui::confirm(
                ctx,
                "admin-remove",
                &tf!("Remove {}?", "Remover {}?", u.email),
                t!(
                    "They can no longer sign in, and their sessions end. Add the e-mail again to undo.",
                    "A pessoa não consegue mais entrar e as sessões dela terminam. Libere o e-mail de novo para desfazer."
                ),
                t!("Remove", "Remover"),
                true,
            ) {
                Some(true) => {
                    self.confirm_remove = None;
                    self.run(ctx, session, Op::Remove { id: u.id, email: u.email.clone() });
                }
                Some(false) => self.confirm_remove = None,
                None => {}
            }
        }
    }
}

/// An icon for a computer's operating system.
fn os_icon(os: &str) -> &'static str {
    let os = os.to_lowercase();
    if os.contains("windows") {
        icons::WINDOWS_LOGO
    } else if os.contains("mac") || os.contains("darwin") {
        icons::APPLE_LOGO
    } else if os.contains("android") {
        icons::ANDROID_LOGO
    } else if os.contains("linux") {
        icons::LINUX_LOGO
    } else {
        icons::DESKTOP
    }
}

#[cfg(test)]
impl AdminPage {
    /// A page with sample data loaded (no network).
    fn sample(tab: Tab) -> Self {
        use rdesk_proto::api::UserRef;
        let mut a = Self::new();
        a.tab = tab;
        let emails = [
            "jeferson.santos.silva@exemplo.com.br",
            "maria.aparecida.de.oliveira.figueiredo@empresa-de-tecnologia.com.br",
            "ana.b@exemplo.com",
            "carlos.eduardo.nascimento.pereira.junior@gmail.com",
            "lia@x.io",
        ];
        a.users = Some(Ok((0..18)
            .map(|i| AdminUser {
                id: i as i64 + 1,
                email: if i < emails.len() { emails[i].into() } else { format!("pessoa.{i}.{}", emails[i % 5]) },
                name: if i % 3 == 0 { "Nome Completo de Alguém com Sobrenomes".into() } else { String::new() },
                is_admin: i % 4 == 0,
                disabled: i % 6 == 5,
                has_totp: i % 2 == 0,
                passkeys: (i % 3) as u32,
                created_at: now() - 86_400 * 40,
                last_login: if i % 5 == 4 { None } else { Some(now() - 3600 * (i as i64 * 7 + 1)) },
            })
            .collect()));
        let oses = ["Windows 11 Pro", "macOS 15", "Linux (Debian 12)", "Android", "FreeBSD"];
        a.machines = (0..16)
            .map(|i| Machine {
                id: i as i64 + 1,
                endpoint_id: format!("{i:064x}"),
                name: if i % 3 == 0 {
                    format!("Computador da recepção do escritório central número {i}")
                } else {
                    format!("pi-{i}")
                },
                os: oses[i % oses.len()].into(),
                owner: UserRef { id: 1, email: emails[i % emails.len()].into(), name: String::new() },
                visibility: [Visibility::Private, Visibility::Shared, Visibility::Everyone][i % 3],
                online: i % 2 == 0,
                in_session: i % 5 == 0,
                last_seen: if i % 7 == 6 { None } else { Some(now() - 86_400 * i as i64) },
                addrs: Vec::new(),
                can_access: true,
                access: rdesk_proto::Access { screen: true, input: true, files: true, terminal: true },
                is_owner: false,
                can_manage: false,
                direct: false,
                version: if i % 4 == 3 { String::new() } else { format!("0.{}.{}", 3 + i % 2, i % 3) },
                ring: if i % 3 == 1 { Ring::Test } else { Ring::All },
            })
            .collect();
        a.releases = Some(Releases {
            releases: (0..8)
                .map(|i| ReleaseInfo {
                    version: format!("0.{}.0", 9 - i),
                    notes: "Terminal remoto, instaladores, atualizações e português.".into(),
                    published_at: now() - 86_400 * (i as i64 * 9 + 2),
                    protocol: 3,
                    platforms: vec![
                        "windows-x86_64".into(),
                        "linux-x86_64".into(),
                        "linux-aarch64".into(),
                        "macos-aarch64".into(),
                    ],
                    running: 12 - i,
                })
                .collect(),
            test: Some("0.7.0".into()),
            all: Some("0.6.0".into()),
            server_version: "0.6.0".into(),
            last_check: Some(now() - 7200),
            check_error: Some(
                "GitHub respondeu 403: limite de requisições excedido para este endereço; tente de novo em uma hora"
                    .into(),
            ),
            server_can_update: true,
            server_update: Some("instalando a 0.7.0 (backup do banco feito)".into()),
        });
        a
    }
}

/// Screens for the ui gallery (see `gallery.rs`): sample data, long texts.
#[cfg(test)]
pub(super) fn gallery() -> Vec<super::gallery::Screen> {
    use super::account::Account;
    use super::gallery::{screen, with_sidebar};
    fn page(mut a: AdminPage) -> impl FnMut(&mut Ui) {
        let session = Account::sample_session(true);
        move |ui: &mut Ui| with_sidebar(ui, super::Page::Admin, |ui| a.show(ui, Some(&session)))
    }
    vec![
        screen("admin-loading", || page(AdminPage::new())),
        screen("admin-error", || {
            let mut a = AdminPage::new();
            a.users =
                Some(Err("não deu para falar com o servidor: tempo esgotado (https://rdesk.exemplo.com.br)".into()));
            page(a)
        }),
        screen("admin-users", || page(AdminPage::sample(Tab::Users))),
        screen("admin-users-confirm", || {
            let mut a = AdminPage::sample(Tab::Users);
            if let Some(Ok(u)) = &a.users {
                a.confirm_remove = Some(u[1].clone());
            }
            page(a)
        }),
        screen("admin-computers", || page(AdminPage::sample(Tab::Computers))),
        screen("admin-versions", || page(AdminPage::sample(Tab::Versions))),
        screen("admin-versions-confirm", || {
            let mut a = AdminPage::sample(Tab::Versions);
            a.confirm_server = Some("0.9.0".into());
            page(a)
        }),
    ]
}
