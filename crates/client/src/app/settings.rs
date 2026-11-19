//! "Settings": app behaviour, start with the system, viewer options, your SSH keys.
//! (The "Account" part above it is drawn by `account.rs`.)

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use egui::{Color32, RichText};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, ClientConfig};

use super::autostart::{self, Autostart};

type AgentKeys = Result<Vec<(String, String, String)>, String>;

pub(super) struct SettingsPage {
    exe: PathBuf,
    cfg: ClientConfig,
    autostart: Autostart,
    autostart_job: Option<Receiver<Result<Autostart, String>>>,
    keys: Option<AgentKeys>,
    keys_job: Option<Receiver<AgentKeys>>,
    pub(super) toast: Option<(String, bool, Instant)>,
}

impl SettingsPage {
    pub(super) fn new(exe: PathBuf) -> Self {
        Self {
            exe,
            cfg: config::load_toml(config::CLIENT_CONFIG).unwrap_or_default(),
            autostart: autostart::current(),
            autostart_job: None,
            keys: None,
            keys_job: None,
            toast: None,
        }
    }

    /// Re-read what may have changed elsewhere (when the page is opened).
    pub(super) fn refresh(&mut self) {
        self.cfg = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
        if self.autostart_job.is_none() {
            self.autostart = autostart::current();
        }
    }

    pub(super) fn app_settings(&self) -> &config::AppSettings {
        &self.cfg.app
    }

    fn save(&mut self) {
        // Merge into the file so hosts edited elsewhere are kept.
        let mut on_disk: ClientConfig = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
        on_disk.app.close_to_tray = self.cfg.app.close_to_tray;
        on_disk.app.start_minimized = self.cfg.app.start_minimized;
        on_disk.swap_cmd_ctrl = self.cfg.swap_cmd_ctrl;
        if let Err(e) = config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &on_disk)) {
            self.toast = Some((format!("Saving settings: {e:#}"), true, Instant::now()));
        }
        self.cfg = on_disk;
    }

    pub(super) fn tick(&mut self) {
        if let Some(rx) = &self.autostart_job
            && let Ok(r) = rx.try_recv()
        {
            self.autostart_job = None;
            match r {
                Ok(m) => {
                    self.autostart = m;
                    self.toast = Some(("Start-up setting saved.".into(), false, Instant::now()));
                }
                Err(e) => {
                    self.autostart = autostart::current();
                    self.toast = Some((e, true, Instant::now()));
                }
            }
        }
        if let Some(rx) = &self.keys_job
            && let Ok(r) = rx.try_recv()
        {
            self.keys_job = None;
            self.keys = Some(r);
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("Start with the system").strong().size(16.0));
            let mut mode = self.autostart;
            ui.add_enabled_ui(self.autostart_job.is_none(), |ui| {
                ui.radio_value(&mut mode, Autostart::Off, "Don't start automatically");
                ui.radio_value(&mut mode, Autostart::Login, "Start when I log in (in the tray)");
                if autostart::supports_admin() {
                    ui.radio_value(
                        &mut mode,
                        Autostart::LoginAdmin,
                        "Start when I log in, as administrator (recommended to control admin windows; asks for permission once)",
                    );
                }
            });
            if self.autostart_job.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Applying… (Windows may ask for administrator permission)");
                });
            }
            if mode != self.autostart && self.autostart_job.is_none() {
                let (tx, rx) = channel();
                let exe = self.exe.clone();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let _ = tx.send(autostart::apply_with_elevation(mode, &exe).map_err(|e| format!("{e:#}")));
                    ctx.request_repaint();
                });
                self.autostart = mode;
                self.autostart_job = Some(rx);
            }

            ui.add_space(14.0);
            ui.label(RichText::new("Window").strong().size(16.0));
            let mut changed = false;
            changed |= ui
                .checkbox(&mut self.cfg.app.close_to_tray, "Closing the window keeps rdesk running in the tray")
                .changed();
            changed |= ui.checkbox(&mut self.cfg.app.start_minimized, "Start hidden in the tray").changed();

            ui.add_space(14.0);
            ui.label(RichText::new("Remote sessions").strong().size(16.0));
            changed |= ui
                .checkbox(&mut self.cfg.swap_cmd_ctrl, "Send Cmd as Ctrl (for a Mac controlling Windows/Linux)")
                .on_hover_text("On macOS this is on by default for each session window.")
                .changed();
            if changed {
                self.save();
            }

            ui.add_space(14.0);
            ui.label(RichText::new("My SSH keys").strong().size(16.0));
            ui.label("Keys your SSH agent (1Password) offers. Add the public key on a computer to log in there with it.");
            ui.horizontal(|ui| {
                if ui.add_enabled(self.keys_job.is_none(), egui::Button::new("Load from the agent")).clicked() {
                    let (tx, rx) = channel();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let r = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| e.to_string())
                            .and_then(|rt| rt.block_on(load_agent_keys()));
                        let _ = tx.send(r);
                        ctx.request_repaint();
                    });
                    self.keys_job = Some(rx);
                }
                if self.keys_job.is_some() {
                    ui.spinner();
                }
            });
            match &self.keys {
                None => {}
                Some(Err(e)) => {
                    ui.colored_label(Color32::from_rgb(240, 160, 40), e);
                }
                Some(Ok(keys)) => {
                    for (fp, label, line) in keys {
                        ui.horizontal(|ui| {
                            ui.label(label);
                            ui.label(RichText::new(fp).monospace().small().weak());
                            if ui.small_button("Copy public key").clicked() {
                                ui.ctx().copy_text(line.clone());
                            }
                        });
                    }
                }
            }

            ui.add_space(14.0);
            ui.label(RichText::new("Files").strong().size(16.0));
            if let Ok(dir) = config::config_dir() {
                ui.horizontal(|ui| {
                    ui.label("Settings and logs:");
                    ui.label(RichText::new(dir.display().to_string()).monospace().small());
                    if ui.small_button("Open").clicked() {
                        open_folder(&dir);
                    }
                });
            }
            ui.add_space(14.0);
            ui.weak("Everything here is also available from the command line: run `rdesk --help`.");
        });
    }
}

async fn load_agent_keys() -> AgentKeys {
    let mut a = Agent::connect(None).await.map_err(|e| format!("{e:#}"))?;
    let keys = a.identities().await.map_err(|e| format!("{e:#}"))?;
    if keys.is_empty() {
        return Err(format!("the SSH agent at {} offers no keys", a.path));
    }
    Ok(keys
        .iter()
        .map(|k| {
            (
                agent::key_fingerprint(k.key_data()),
                format!("{} {}", k.algorithm(), k.comment()),
                k.to_openssh().unwrap_or_default().trim().to_string(),
            )
        })
        .collect())
}

pub(super) fn open_folder(dir: &std::path::Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(dir).spawn();
}
