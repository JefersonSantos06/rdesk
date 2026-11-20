//! "Settings": the account, app behaviour (language, start with the system, window),
//! remote sessions, your SSH keys, your enrollment keys, and where the files are.
//! (The account card is drawn by `account.rs`, the enrollment keys by `add_computer.rs`.)

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use egui::{RichText, Ui};
use rdesk_common::agent::{self, Agent};
use rdesk_common::config::{self, ClientConfig};
use rdesk_common::i18n::{self, LanguagePref};
use rdesk_common::{t, tf};

use super::add_computer::KeysPanel;
use super::autostart::{self, Autostart};
use crate::ui::{self, Tone, icons, size, space};

type AgentKeys = Result<Vec<(String, String, String)>, String>;

/// The parts of the page.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Section {
    Account,
    General,
    Sessions,
    SshKeys,
    EnrollKeys,
    Advanced,
}

pub(super) struct SettingsPage {
    exe: PathBuf,
    cfg: ClientConfig,
    autostart: Autostart,
    autostart_job: Option<Receiver<Result<Autostart, String>>>,
    keys: Option<AgentKeys>,
    keys_job: Option<Receiver<AgentKeys>>,
    section: Section,
    /// The enrollment keys, loaded when that section is first shown.
    enroll: Option<KeysPanel>,
    /// Where the settings and logs are.
    config_dir: Option<PathBuf>,
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
            section: Section::Account,
            enroll: None,
            config_dir: config::config_dir().ok(),
            toast: None,
        }
    }

    /// Re-read what may have changed elsewhere (when the page is opened).
    pub(super) fn refresh(&mut self) {
        self.cfg = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
        if self.autostart_job.is_none() {
            self.autostart = autostart::current();
        }
        // The enrollment keys load again when shown.
        self.enroll = None;
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

    /// The page; `true` when the user asked to sign in to a server (from the local mode).
    pub(super) fn show(&mut self, ui: &mut Ui, account: &mut super::account::Account, local_mode: bool) -> bool {
        let mut sign_in = false;
        let signed_in = account.signed_in();
        ui::page_title(ui, t!("Settings", "Configurações"), None);
        ui.add_space(space::S);
        let mut items = vec![
            (Section::Account, t!("Account", "Conta")),
            (Section::General, t!("General", "Geral")),
            (Section::Sessions, t!("Remote sessions", "Sessões remotas")),
            (Section::SshKeys, t!("SSH keys", "Chaves SSH")),
        ];
        if signed_in {
            items.push((Section::EnrollKeys, t!("Enrollment keys", "Chaves de cadastro")));
        }
        items.push((Section::Advanced, t!("Advanced", "Avançado")));
        if self.section == Section::EnrollKeys && !signed_in {
            self.section = Section::Account;
        }
        ui::tabs(ui, &mut self.section, &items);
        ui.add_space(space::M);
        egui::ScrollArea::vertical().id_salt(("settings", self.section)).auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(760.0));
            match self.section {
                Section::Account => {
                    if local_mode && !signed_in {
                        sign_in = local_account(ui);
                    } else {
                        account.settings_section(ui);
                    }
                }
                Section::General => self.general_ui(ui),
                Section::Sessions => self.sessions_ui(ui),
                Section::SshKeys => self.ssh_keys_ui(ui),
                Section::EnrollKeys => {
                    let server = account.session().map(|s| s.server.clone()).unwrap_or_default();
                    ui::paragraph(
                        ui,
                        t!(
                            "An enrollment key adds computers at once, with no approval: whoever has the key can \
                             add computers to your account until it expires or you revoke it.",
                            "Uma chave de cadastro adiciona computadores na hora, sem aprovação: quem tiver a chave \
                             pode adicionar computadores à sua conta até ela expirar ou você revogá-la."
                        ),
                    );
                    ui.add_space(space::M);
                    self.enroll.get_or_insert_with(KeysPanel::new).show(ui, &server);
                }
                Section::Advanced => self.advanced_ui(ui),
            }
            ui.add_space(space::L);
        });
        sign_in
    }

    fn general_ui(&mut self, ui: &mut Ui) {
        ui::card(ui, |ui| self.language_ui(ui));
        ui.add_space(space::L);

        ui::section(ui, t!("Start with the system", "Iniciar com o sistema"));
        ui.add_space(space::XS);
        let mut mode = self.autostart;
        ui::card(ui, |ui| {
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
                ui::busy(
                    ui,
                    t!(
                        "Applying… (Windows may ask for administrator permission)",
                        "Aplicando… (o Windows pode pedir permissão de administrador)"
                    ),
                );
            }
        });
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

        ui.add_space(space::L);
        ui::section(ui, t!("Window", "Janela"));
        ui.add_space(space::XS);
        let mut changed = false;
        ui::card(ui, |ui| {
            changed |= toggle_row(
                ui,
                t!("Keep running in the tray", "Continuar rodando na bandeja"),
                t!(
                    "Closing the window keeps rdesk running in the tray",
                    "Fechar a janela mantém o rdesk rodando na bandeja"
                ),
                &mut self.cfg.app.close_to_tray,
            );
            ui.separator();
            changed |= toggle_row(
                ui,
                t!("Start hidden in the tray", "Iniciar escondido na bandeja"),
                t!(
                    "Only the tray icon shows when rdesk starts.",
                    "Só o ícone da bandeja aparece quando o rdesk inicia."
                ),
                &mut self.cfg.app.start_minimized,
            );
        });
        if changed {
            self.save();
        }
    }

    fn sessions_ui(&mut self, ui: &mut Ui) {
        let mut changed = false;
        ui::card(ui, |ui| {
            changed = toggle_row(
                ui,
                t!(
                    "Mac keys (for a Mac controlling Windows/Linux)",
                    "Teclas de Mac (para um Mac controlando Windows/Linux)"
                ),
                t!(
                    "Cmd \u{E06C} Ctrl, left Option \u{E06C} Windows key, Control \u{E06C} Alt; the right Option stays AltGr. \
                     On macOS this is on by default for each session window.",
                    "Cmd \u{E06C} Ctrl, Option esquerdo \u{E06C} tecla Windows, Control \u{E06C} Alt; o Option direito continua AltGr. \
                     No macOS isto vem ligado em cada janela de sessão."
                ),
                &mut self.cfg.swap_cmd_ctrl,
            );
        });
        if changed {
            self.save();
        }
    }

    fn ssh_keys_ui(&mut self, ui: &mut Ui) {
        ui::paragraph(
            ui,
            t!(
                "Keys your SSH agent (1Password) offers. Add the public key on a computer to log in there with it.",
                "Chaves que o seu agente SSH (1Password) oferece. Adicione a chave pública em um computador para entrar nele com ela."
            ),
        );
        ui.add_space(space::S);
        ui.horizontal_wrapped(|ui| {
            let label = if self.keys.is_some() {
                t!("Load again", "Carregar de novo")
            } else {
                t!("Load from the agent", "Carregar do agente")
            };
            if ui
                .add_enabled(
                    self.keys_job.is_none(),
                    egui::Button::new(ui::with_icon(icons::ARROW_CLOCKWISE, label)).min_size(egui::vec2(0.0, 32.0)),
                )
                .clicked()
            {
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
        ui.add_space(space::S);
        let mut copied = false;
        match &self.keys {
            None => {}
            Some(Err(e)) => {
                ui::banner(ui, Tone::Warning, e, None);
            }
            Some(Ok(keys)) => {
                let p = ui::pal(ui);
                super::account::rows_card(ui, keys, |ui, (fp, label, line)| {
                    ui::row(
                        ui,
                        |ui| {
                            ui.label(RichText::new(icons::KEY).size(size::ICON + 2.0).color(p.accent));
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui::truncate(ui, RichText::new(label).strong());
                                ui::truncate(ui, RichText::new(fp).monospace().size(size::SMALL).color(p.muted));
                            });
                        },
                        |ui| {
                            if ui::secondary(
                                ui,
                                ui::with_icon(icons::COPY, t!("Copy public key", "Copiar chave pública")),
                            )
                            .clicked()
                            {
                                ui.ctx().copy_text(line.clone());
                                copied = true;
                            }
                        },
                    );
                });
            }
        }
        if copied {
            self.toast = Some((t!("Public key copied.", "Chave pública copiada.").into(), false, Instant::now()));
        }
    }

    fn advanced_ui(&mut self, ui: &mut Ui) {
        let p = ui::pal(ui);
        ui::card(ui, |ui| {
            ui::row(
                ui,
                |ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(t!("Settings and logs", "Configurações e logs")).strong());
                        match &self.config_dir {
                            Some(dir) => {
                                ui::truncate(
                                    ui,
                                    RichText::new(dir.display().to_string())
                                        .monospace()
                                        .size(size::SMALL)
                                        .color(p.muted),
                                );
                            }
                            None => {
                                ui::caption(ui, t!("Folder not found.", "Pasta não encontrada."));
                            }
                        }
                    });
                },
                |ui| {
                    if let Some(dir) = &self.config_dir
                        && ui::secondary(ui, ui::with_icon(icons::FOLDER_OPEN, t!("Open", "Abrir"))).clicked()
                    {
                        open_folder(dir);
                    }
                },
            );
        });
        ui.add_space(space::M);
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(icons::TERMINAL_WINDOW).size(size::ICON).color(p.muted));
            ui::caption(
                ui,
                t!(
                    "Everything here is also available from the command line: run `rdesk --help`.",
                    "Tudo aqui também existe na linha de comando (em inglês): rode `rdesk --help`."
                ),
            );
        });
    }
}

/// The Account section without a server: what that means, and "Sign in to a server…".
fn local_account(ui: &mut Ui) -> bool {
    let p = ui::pal(ui);
    let mut sign_in = false;
    ui::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(icons::WIFI_SLASH).size(24.0).color(p.muted));
            ui.label(RichText::new(t!("Without a server", "Sem servidor")).size(size::SECTION).strong());
        });
        ui.add_space(space::XS);
        ui::paragraph(
            ui,
            t!(
                "Used without a server: only computers on this network.",
                "Usado sem servidor: só computadores desta rede."
            ),
        );
        ui.add_space(space::S);
        if ui::primary(ui, ui::with_icon(icons::SIGN_IN, t!("Sign in to a server…", "Entrar em um servidor…")))
            .clicked()
        {
            sign_in = true;
        }
    });
    sign_in
}

/// A setting with a title, a line about it and a switch on the right. Returns `true` when
/// it changed.
fn toggle_row(ui: &mut Ui, title: &str, body: &str, on: &mut bool) -> bool {
    let mut changed = false;
    ui::row(
        ui,
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add(egui::Label::new(title).wrap());
                ui::caption(ui, body);
            });
        },
        |ui| {
            changed = ui::toggle(ui, on).on_hover_text(title).changed();
        },
    );
    changed
}

impl SettingsPage {
    /// The app's language: applies at once (this window and the tray; session windows
    /// opened from now on too).
    fn language_ui(&mut self, ui: &mut Ui) {
        let system = match i18n::system() {
            i18n::Lang::Pt => "Português (Brasil)",
            i18n::Lang::En => "English",
        };
        let name = |p: LanguagePref| match p {
            LanguagePref::System => tf!("System ({system})", "Do sistema ({system})"),
            LanguagePref::English => "English".to_string(),
            LanguagePref::Portuguese => "Português (Brasil)".to_string(),
        };
        let all = [LanguagePref::System, LanguagePref::English, LanguagePref::Portuguese];
        let names: Vec<String> = all.iter().map(|p| name(*p)).collect();
        let mut pref = self.cfg.app.language;
        ui::row(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    // In both languages, so it can be found in either.
                    ui.add(
                        egui::Label::new(RichText::new(ui::with_icon(icons::TRANSLATE, "Language · Idioma")).strong())
                            .wrap(),
                    );
                    ui::caption(ui, t!("Applies at once.", "Vale na hora."));
                });
            },
            |ui| {
                let w = super::account::combo_width(ui, names.iter().map(String::as_str));
                egui::ComboBox::from_id_salt("language").selected_text(name(pref)).width(w).show_ui(ui, |ui| {
                    for p in all {
                        ui.selectable_value(&mut pref, p, name(p));
                    }
                });
            },
        );
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

#[cfg(test)]
impl SettingsPage {
    /// A page for the gallery: default settings, nothing read from this machine.
    fn sample(section: Section) -> Self {
        Self {
            exe: PathBuf::new(),
            cfg: ClientConfig::default(),
            autostart: Autostart::Login,
            autostart_job: None,
            keys: None,
            keys_job: None,
            section,
            enroll: None,
            config_dir: Some(PathBuf::from(
                r"C:\Users\Jeferson Santos da Silva\AppData\Roaming\rdesk\config\com um caminho bem comprido",
            )),
            toast: None,
        }
    }
}

/// Screens for the ui gallery (see `gallery.rs`): sample data, long texts.
#[cfg(test)]
pub(super) fn gallery() -> Vec<super::gallery::Screen> {
    use super::account::Account;
    use super::gallery::{screen, with_sidebar};
    fn page(mut s: SettingsPage, signed_in: bool) -> impl FnMut(&mut Ui) {
        let session = signed_in.then(|| Account::sample_session(true));
        let mut a = Account::sample(session, "");
        move |ui: &mut Ui| {
            with_sidebar(ui, super::Page::Settings, |ui| {
                s.show(ui, &mut a, !signed_in);
            });
        }
    }
    vec![
        screen("settings-account", || page(SettingsPage::sample(Section::Account), true)),
        screen("settings-account-local", || page(SettingsPage::sample(Section::Account), false)),
        screen("settings-general", || page(SettingsPage::sample(Section::General), true)),
        screen("settings-general-applying", || {
            let mut s = SettingsPage::sample(Section::General);
            let (tx, rx) = channel();
            std::mem::forget(tx);
            s.autostart_job = Some(rx);
            page(s, false)
        }),
        screen("settings-sessions", || page(SettingsPage::sample(Section::Sessions), true)),
        screen("settings-ssh-empty", || page(SettingsPage::sample(Section::SshKeys), false)),
        screen("settings-ssh-keys", || {
            let mut s = SettingsPage::sample(Section::SshKeys);
            s.keys = Some(Ok((0..4)
                .map(|i| {
                    (
                        format!("SHA256:k3f97a2c11112222333344445555e81d{i}AbCdEfGh"),
                        format!("ssh-ed25519 Chave pessoal do notebook de trabalho do Jeferson {i}"),
                        "ssh-ed25519 AAAA…".into(),
                    )
                })
                .collect()));
            page(s, true)
        }),
        screen("settings-ssh-error", || {
            let mut s = SettingsPage::sample(Section::SshKeys);
            s.keys = Some(Err(
                "o agente SSH em \\\\.\\pipe\\openssh-ssh-agent não oferece chaves (o 1Password está aberto e destravado?)"
                    .into(),
            ));
            page(s, true)
        }),
        screen("settings-enroll-keys", || {
            let mut s = SettingsPage::sample(Section::EnrollKeys);
            let mut k = KeysPanel::sample(KeysPanel::sample_keys(8), KeysPanel::sample_groups());
            k.no_scroll();
            s.enroll = Some(k);
            page(s, true)
        }),
        screen("settings-advanced", || page(SettingsPage::sample(Section::Advanced), true)),
    ]
}
