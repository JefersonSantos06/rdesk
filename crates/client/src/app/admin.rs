//! "Admin" (system admins only): who may sign in to the server.

use std::sync::mpsc::Receiver;
use std::time::Instant;

use egui::{Color32, RichText};
use rdesk_common::account::Session;
use rdesk_proto::api::{AdminUser, UpdateUser};

use super::account::spawn;

const ERROR: Color32 = Color32::from_rgb(230, 70, 70);
const GREEN: Color32 = Color32::from_rgb(46, 184, 92);

enum Op {
    Add { email: String, admin: bool },
    Update { id: i64, email: String, change: UpdateUser },
    Remove { id: i64, email: String },
}

pub(super) struct AdminPage {
    users: Option<Result<Vec<AdminUser>, String>>,
    load_job: Option<Receiver<Result<Vec<AdminUser>, String>>>,
    op_job: Option<Receiver<Result<String, String>>>,
    new_email: String,
    new_admin: bool,
    confirm_remove: Option<AdminUser>,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl AdminPage {
    pub(super) fn new() -> Self {
        Self {
            users: None,
            load_job: None,
            op_job: None,
            new_email: String::new(),
            new_admin: false,
            confirm_remove: None,
            toast: None,
        }
    }

    pub(super) fn reload(&mut self, ctx: &egui::Context, session: Option<&Session>) {
        let Some(s) = session.cloned() else { return };
        self.load_job = Some(spawn(ctx, move || async move {
            match s.api() {
                Ok(api) => api.users().await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            }
        }));
    }

    fn run(&mut self, ctx: &egui::Context, session: Option<&Session>, op: Op) {
        let Some(s) = session.cloned() else { return };
        self.op_job = Some(spawn(ctx, move || async move {
            let api = s.api().map_err(|e| format!("{e:#}"))?;
            let r = match op {
                Op::Add { email, admin } => {
                    api.add_user(&email, admin).await.map(|u| format!("{} can now sign in.", u.email))
                }
                Op::Update { id, email, change } => {
                    api.update_user(id, &change).await.map(|_| format!("Updated {email}."))
                }
                Op::Remove { id, email } => api.remove_user(id).await.map(|_| format!("Removed {email}.")),
            };
            r.map_err(|e| format!("{e:#}"))
        }));
    }

    pub(super) fn tick(&mut self, ctx: &egui::Context, session: Option<&Session>) {
        if let Some(rx) = &self.load_job
            && let Ok(r) = rx.try_recv()
        {
            self.load_job = None;
            self.users = Some(r);
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

    pub(super) fn show(&mut self, ui: &mut egui::Ui, session: Option<&Session>) {
        let ctx = ui.ctx().clone();
        let me = session.map(|s| s.user.id);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Who can sign in").strong().size(16.0));
            if ui.button("⟳").on_hover_text("Refresh").clicked() {
                self.reload(&ctx, session);
            }
            if self.load_job.is_some() || self.op_job.is_some() {
                ui.spinner();
            }
        });
        ui.label("Only these e-mails can sign in (with Google or GitHub, plus a second factor).");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let resp = ui
                .add(egui::TextEdit::singleline(&mut self.new_email).hint_text("e-mail to allow").desired_width(260.0));
            ui.checkbox(&mut self.new_admin, "system admin");
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let ok = !self.new_email.trim().is_empty() && self.op_job.is_none();
            let clicked = ui.add_enabled(ok, egui::Button::new("＋ Allow")).clicked();
            if ok && (clicked || enter) {
                let op = Op::Add { email: self.new_email.trim().to_string(), admin: self.new_admin };
                self.run(&ctx, session, op);
            }
        });
        ui.add_space(10.0);
        match &self.users {
            None => {
                ui.spinner();
            }
            Some(Err(e)) => {
                ui.colored_label(ERROR, e);
            }
            Some(Ok(users)) => {
                let users = users.clone();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("admin-users").num_columns(5).striped(true).spacing([14.0, 8.0]).show(ui, |ui| {
                        ui.strong("E-mail");
                        ui.strong("Second factor");
                        ui.strong("Last sign-in");
                        ui.strong("Role");
                        ui.strong("");
                        ui.end_row();
                        for u in users {
                            let label =
                                if u.name.is_empty() { u.email.clone() } else { format!("{} ({})", u.email, u.name) };
                            if u.disabled {
                                ui.label(RichText::new(format!("{label} · disabled")).weak());
                            } else {
                                ui.label(label);
                            }
                            match (u.has_totp, u.passkeys) {
                                (false, 0) => ui.weak("not set up yet"),
                                (t, n) => ui.colored_label(
                                    GREEN,
                                    format!(
                                        "{}{}",
                                        if t { "TOTP " } else { "" },
                                        if n > 0 { format!("{n} passkey(s)") } else { String::new() }
                                    ),
                                ),
                            };
                            ui.label(match u.last_login {
                                Some(t) => rdesk_common::directory::ago(t, now()),
                                None => "never".into(),
                            });
                            ui.label(if u.is_admin { "system admin" } else { "user" });
                            ui.horizontal(|ui| {
                                let busy = self.op_job.is_some();
                                ui.add_enabled_ui(!busy, |ui| {
                                    let admin_label = if u.is_admin { "Make user" } else { "Make admin" };
                                    if ui.small_button(admin_label).clicked() {
                                        let change = UpdateUser { is_admin: Some(!u.is_admin), disabled: None };
                                        self.run(
                                            &ctx,
                                            session,
                                            Op::Update { id: u.id, email: u.email.clone(), change },
                                        );
                                    }
                                    let dis_label = if u.disabled { "Enable" } else { "Disable" };
                                    if ui
                                        .small_button(dis_label)
                                        .on_hover_text("A disabled user cannot sign in and is signed out everywhere")
                                        .clicked()
                                    {
                                        let change = UpdateUser { is_admin: None, disabled: Some(!u.disabled) };
                                        self.run(
                                            &ctx,
                                            session,
                                            Op::Update { id: u.id, email: u.email.clone(), change },
                                        );
                                    }
                                    if Some(u.id) != me && ui.small_button("Remove…").clicked() {
                                        self.confirm_remove = Some(u.clone());
                                    }
                                });
                            });
                            ui.end_row();
                        }
                    });
                });
            }
        }
        if let Some(u) = self.confirm_remove.clone() {
            let m = egui::Modal::new(egui::Id::unique("admin-remove")).show(&ctx, |ui| {
                ui.heading(format!("Remove {}?", u.email));
                ui.label("They can no longer sign in, and their sessions end. Add the e-mail again to undo.");
                ui.add_space(6.0);
                let mut done = None;
                ui.horizontal(|ui| {
                    if ui.button("Remove").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
                done
            });
            match m.inner {
                Some(true) => {
                    self.confirm_remove = None;
                    self.run(&ctx, session, Op::Remove { id: u.id, email: u.email.clone() });
                }
                Some(false) => self.confirm_remove = None,
                None if m.should_close() => self.confirm_remove = None,
                None => {}
            }
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
