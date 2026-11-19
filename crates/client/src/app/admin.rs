//! "Admin" (system admins only): who may sign in to the server, every computer, and which
//! rdesk version the computers get (first the test computers, then everyone).

use std::sync::mpsc::Receiver;
use std::time::Instant;

use egui::{Color32, RichText};
use rdesk_common::account::Session;
use rdesk_proto::api::{AdminUser, Machine, Releases, Ring, UpdateUser, Visibility};

use super::account::spawn;

const ERROR: Color32 = Color32::from_rgb(230, 70, 70);
const GREEN: Color32 = Color32::from_rgb(46, 184, 92);

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

pub(super) struct AdminPage {
    users: Option<Result<Vec<AdminUser>, String>>,
    machines: Vec<Machine>,
    releases: Option<Releases>,
    /// Releasing to everyone asks first.
    confirm_release: Option<String>,
    load_job: Option<Receiver<Loaded>>,
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
            machines: Vec::new(),
            releases: None,
            confirm_release: None,
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
                    api.add_user(&email, admin).await.map(|u| format!("{} can now sign in.", u.email))
                }
                Op::Update { id, email, change } => {
                    api.update_user(id, &change).await.map(|_| format!("Updated {email}."))
                }
                Op::Remove { id, email } => api.remove_user(id).await.map(|_| format!("Removed {email}.")),
                Op::Release { version, ring } => api.release_version(&version, ring).await.map(|_| match ring {
                    Ring::Test => format!("{version} released to the test computers."),
                    Ring::All => format!("{version} released to everyone."),
                }),
                Op::Ring { id, name, ring } => api.set_ring(id, ring).await.map(|_| match ring {
                    Ring::Test => format!("{name} now gets new versions first."),
                    Ring::All => format!("{name} gets new versions with everyone."),
                }),
                Op::UpdateServer { version } => api
                    .update_server(&version)
                    .await
                    .map(|_| format!("The server is installing {version}; it restarts in a moment.")),
                Op::Check => {
                    let r = api.check_releases().await.map(|_| "Looking for new versions…".to_string());
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
        ui.add_space(16.0);
        self.versions_ui(ui, session);
        ui.add_space(16.0);
        ui.label(RichText::new("All computers").strong().size(16.0));
        ui.label("Admins see every computer, but connect only to those shared with them.");
        let busy = self.op_job.is_some();
        let mut ring_op = None;
        egui::Grid::new("admin-machines").num_columns(6).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
            ui.strong("Computer");
            ui.strong("Registered by");
            ui.strong("Visible to");
            ui.strong("Status");
            ui.strong("Version");
            ui.strong("Test computer");
            ui.end_row();
            for m in &self.machines {
                ui.label(&m.name);
                ui.label(&m.owner.email);
                ui.label(match m.visibility {
                    Visibility::Private => "only the owner",
                    Visibility::Shared => "shared",
                    Visibility::Everyone => "everyone",
                });
                ui.label(if m.in_session {
                    "in use".to_string()
                } else if m.online {
                    "online".to_string()
                } else {
                    match m.last_seen {
                        Some(t) => format!("offline · {}", rdesk_common::directory::ago(t, now())),
                        None => "never connected".to_string(),
                    }
                });
                ui.label(if m.version.is_empty() { "?".to_string() } else { m.version.clone() });
                let mut test = m.ring == Ring::Test;
                if ui
                    .add_enabled(!busy, egui::Checkbox::new(&mut test, ""))
                    .on_hover_text("Gets new versions first, before everyone")
                    .changed()
                {
                    let ring = if test { Ring::Test } else { Ring::All };
                    ring_op = Some(Op::Ring { id: m.id, name: m.name.clone(), ring });
                }
                ui.end_row();
            }
        });
        if let Some(op) = ring_op {
            self.run(&ctx, session, op);
        }
        if let Some(version) = self.confirm_release.clone() {
            let m = egui::Modal::new(egui::Id::unique("admin-release")).show(&ctx, |ui| {
                ui.heading(format!("Release {version} to everyone?"));
                ui.label(
                    "Every computer downloads it and installs it when nobody is connected to it. \
                     Computers do not go back to an older version by themselves.",
                );
                ui.add_space(6.0);
                let mut done = None;
                ui.horizontal(|ui| {
                    if ui.button("Release to everyone").clicked() {
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
                    self.confirm_release = None;
                    self.run(&ctx, session, Op::Release { version, ring: Ring::All });
                }
                Some(false) => self.confirm_release = None,
                None if m.should_close() => self.confirm_release = None,
                None => {}
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

impl AdminPage {
    /// New versions found on GitHub, and the two steps of releasing one.
    fn versions_ui(&mut self, ui: &mut egui::Ui, session: Option<&Session>) {
        let ctx = ui.ctx().clone();
        let busy = self.op_job.is_some();
        ui.horizontal(|ui| {
            ui.label(RichText::new("rdesk versions").strong().size(16.0));
            if ui.add_enabled(!busy, egui::Button::new("Check now")).clicked() {
                self.run(&ctx, session, Op::Check);
            }
        });
        let Some(r) = self.releases.clone() else {
            ui.spinner();
            return;
        };
        ui.label(format!(
            "Server {} · released to everyone: {} · to the test computers: {}",
            r.server_version,
            r.all.as_deref().unwrap_or("none"),
            r.test.as_deref().unwrap_or("none"),
        ));
        if let Some(u) = &r.server_update {
            ui.weak(format!("Server update: {u}"));
        }
        match (&r.check_error, r.last_check) {
            (Some(e), _) => {
                ui.colored_label(ERROR, format!("Could not look for new versions: {e}"));
            }
            (None, Some(t)) => {
                ui.weak(format!("Looked on GitHub {}.", rdesk_common::directory::ago(t, now())));
            }
            (None, None) => {
                ui.weak("Not looked for new versions yet.");
            }
        }
        if r.releases.is_empty() {
            return;
        }
        let newer = |v: &str, than: &Option<String>| {
            than.as_deref()
                .is_none_or(|t| rdesk_proto::release::parse_version(v) > rdesk_proto::release::parse_version(t))
        };
        egui::Grid::new("admin-releases").num_columns(5).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
            ui.strong("Version");
            ui.strong("Published");
            ui.strong("Running on");
            ui.strong("Platforms");
            ui.strong("");
            ui.end_row();
            for rel in r.releases.iter().take(6) {
                let mut label = rel.version.clone();
                if r.all.as_deref() == Some(&rel.version) {
                    label.push_str(" · everyone");
                } else if r.test.as_deref() == Some(&rel.version) {
                    label.push_str(" · test");
                }
                ui.label(label).on_hover_text(if rel.notes.is_empty() { "No notes" } else { &rel.notes });
                ui.label(rdesk_common::directory::ago(rel.published_at, now()));
                ui.label(format!("{} computer(s)", rel.running));
                ui.label(rel.platforms.join(", "));
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        if newer(&rel.version, &r.test)
                            && newer(&rel.version, &r.all)
                            && ui
                                .small_button("Release to test computers")
                                .on_hover_text("Only the computers marked as test computers below")
                                .clicked()
                        {
                            self.run(&ctx, session, Op::Release { version: rel.version.clone(), ring: Ring::Test });
                        }
                        if newer(&rel.version, &r.all) && ui.small_button("Release to everyone…").clicked() {
                            self.confirm_release = Some(rel.version.clone());
                        }
                        let server_newer = rdesk_proto::release::parse_version(&rel.version)
                            > rdesk_proto::release::parse_version(&r.server_version);
                        if r.server_can_update
                            && server_newer
                            && rel.platforms.iter().any(|p| p.starts_with("linux"))
                            && ui
                                .small_button("Update the server")
                                .on_hover_text("Backs up the database, installs it and restarts; goes back if it fails")
                                .clicked()
                        {
                            self.run(&ctx, session, Op::UpdateServer { version: rel.version.clone() });
                        }
                    });
                });
                ui.end_row();
            }
        });
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
