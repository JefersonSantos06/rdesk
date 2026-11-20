//! "Settings": app behaviour, start with the system, viewer options, your SSH keys.
//! (The "Account" part above it is drawn by `account.rs`.)

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use egui::{Color32, RichText};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, ClientConfig};
use rdesk_common::i18n::{self, LanguagePref};
use rdesk_common::{t, tf};

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
        on_disk.app.language = self.cfg.app.language;
        on_disk.swap_cmd_ctrl = self.cfg.swap_cmd_ctrl;
        if let Err(e) = config::ensure_config_dir().and_then(|_| config::save_toml(config::CLIENT_CONFIG, &on_disk)) {
            self.toast =
                Some((tf!("Saving settings: {e:#}", "Salvando as configurações: {e:#}"), true, Instant::now()));
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
                    self.toast =
                        Some((t!("Start-up setting saved.", "Início automático salvo.").into(), false, Instant::now()));
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
            self.language_ui(ui);

            ui.add_space(14.0);
            ui.label(RichText::new(t!("Start with the system", "Iniciar com o sistema")).strong().size(16.0));
            let mut mode = self.autostart;
            ui.add_enabled_ui(self.autostart_job.is_none(), |ui| {
                ui.radio_value(&mut mode, Autostart::Off, t!("Don't start automatically", "Não iniciar automaticamente"));
                ui.radio_value(
                    &mut mode,
                    Autostart::Login,
                    t!("Start when I log in (in the tray)", "Iniciar quando eu entrar no computador (na bandeja)"),
                );
                // Replaced by the rdesk service (This computer); kept only to be turned off.
                if autostart::supports_admin() && self.autostart == Autostart::LoginAdmin {
                    ui.radio_value(
                        &mut mode,
                        Autostart::LoginAdmin,
                        t!(
                            "Start when I log in, as administrator (recommended to control admin windows; asks for permission once)",
                            "Iniciar quando eu entrar no computador, como administrador (recomendado para controlar janelas de administrador; pede permissão uma vez)"
                        ),
                    );
                }
            });
            if self.autostart_job.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!(
                        "Applying… (Windows may ask for administrator permission)",
                        "Aplicando… (o Windows pode pedir permissão de administrador)"
                    ));
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
            ui.label(RichText::new(t!("Window", "Janela")).strong().size(16.0));
            let mut changed = false;
            changed |= ui
                .checkbox(
                    &mut self.cfg.app.close_to_tray,
                    t!("Closing the window keeps rdesk running in the tray", "Fechar a janela mantém o rdesk rodando na bandeja"),
                )
                .changed();
            changed |= ui
                .checkbox(&mut self.cfg.app.start_minimized, t!("Start hidden in the tray", "Iniciar escondido na bandeja"))
                .changed();

            ui.add_space(14.0);
            ui.label(RichText::new(t!("Remote sessions", "Sessões remotas")).strong().size(16.0));
            changed |= ui
                .checkbox(
                    &mut self.cfg.swap_cmd_ctrl,
                    t!("Mac keys (for a Mac controlling Windows/Linux)", "Teclas de Mac (para um Mac controlando Windows/Linux)"),
                )
                .on_hover_text(t!(
                    "Cmd → Ctrl, left Option → Windows key, Control → Alt; the right Option stays AltGr. \
                     On macOS this is on by default for each session window.",
                    "Cmd → Ctrl, Option esquerdo → tecla Windows, Control → Alt; o Option direito continua AltGr. \
                     No macOS isto vem ligado em cada janela de sessão."
                ))
                .changed();
            if changed {
                self.save();
            }

            ui.add_space(14.0);
            ui.label(RichText::new(t!("My SSH keys", "Minhas chaves SSH")).strong().size(16.0));
            ui.label(t!(
                "Keys your SSH agent (1Password) offers. Add the public key on a computer to log in there with it.",
                "Chaves que o seu agente SSH (1Password) oferece. Adicione a chave pública em um computador para entrar nele com ela."
            ));
            ui.horizontal(|ui| {
                if ui.add_enabled(self.keys_job.is_none(), egui::Button::new(t!("Load from the agent", "Carregar do agente"))).clicked() {
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
                            if ui.small_button(t!("Copy public key", "Copiar chave pública")).clicked() {
                                ui.ctx().copy_text(line.clone());
                            }
                        });
                    }
                }
            }

            ui.add_space(14.0);
            ui.label(RichText::new(t!("Files", "Arquivos")).strong().size(16.0));
            if let Ok(dir) = config::config_dir() {
                ui.horizontal(|ui| {
                    ui.label(t!("Settings and logs:", "Configurações e logs:"));
                    ui.label(RichText::new(dir.display().to_string()).monospace().small());
                    if ui.small_button(t!("Open", "Abrir")).clicked() {
                        open_folder(&dir);
                    }
                });
            }
            ui.add_space(14.0);
            ui.weak(t!(
                "Everything here is also available from the command line: run `rdesk --help`.",
                "Tudo aqui também existe na linha de comando (em inglês): rode `rdesk --help`."
            ));
        });
    }
}

impl SettingsPage {
    /// The app's language: applies at once (this window and the tray; session windows
    /// opened from now on too).
    fn language_ui(&mut self, ui: &mut egui::Ui) {
        // In both languages, so it can be found in either.
        ui.label(RichText::new("Language · Idioma").strong().size(16.0));
        let system = match i18n::system() {
            i18n::Lang::Pt => "Português (Brasil)",
            i18n::Lang::En => "English",
        };
        let name = |p: LanguagePref| match p {
            LanguagePref::System => tf!("System ({system})", "Do sistema ({system})"),
            LanguagePref::English => "English".to_string(),
            LanguagePref::Portuguese => "Português (Brasil)".to_string(),
        };
        let mut pref = self.cfg.app.language;
        egui::ComboBox::from_id_salt("language").selected_text(name(pref)).width(220.0).show_ui(ui, |ui| {
            for p in [LanguagePref::System, LanguagePref::English, LanguagePref::Portuguese] {
                ui.selectable_value(&mut pref, p, name(p));
            }
        });
        if pref != self.cfg.app.language {
            self.cfg.app.language = pref;
            i18n::set(i18n::resolve(pref));
            self.save();
        }
    }
}

async fn load_agent_keys() -> AgentKeys {
    let mut a = Agent::connect(None).await.map_err(|e| format!("{e:#}"))?;
    let keys = a.identities().await.map_err(|e| format!("{e:#}"))?;
    if keys.is_empty() {
        return Err(tf!("the SSH agent at {} offers no keys", "o agente SSH em {} não oferece chaves", a.path));
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
