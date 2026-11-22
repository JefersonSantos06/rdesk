//! Every screen drawn with sample data to PNG files, to look at them without a server, a
//! host or a session: in Portuguese and English, dark and light, at the smallest window
//! size and a large one. Long names and texts on purpose: nothing may be clipped or run
//! over the layout.
//!
//! ```sh
//! cargo test -p rdesk-client --features host --lib ui_gallery -- --ignored --nocapture
//! ```
//!
//! The images go to `target/ui-gallery/`. Narrow it with `RDESK_GALLERY=<part of a name>`,
//! and choose `RDESK_GALLERY_LANGS=pt,en` and `RDESK_GALLERY_THEMES=dark,light`.
//!
//! Each page module offers `gallery()`: its screens, each made fresh for every picture.

use std::path::PathBuf;

use egui::Vec2;
use rdesk_common::i18n::{self, Lang};

use crate::ui;

/// Draws one frame of a screen.
type Draw = Box<dyn FnMut(&mut egui::Ui)>;

/// One screen: a name (the file name) and how to make it.
pub(crate) struct Screen {
    pub name: &'static str,
    make: Box<dyn Fn() -> Draw>,
}

/// A screen from a function that builds its state and returns how to draw it.
pub(crate) fn screen<F: FnMut(&mut egui::Ui) + 'static>(name: &'static str, make: impl Fn() -> F + 'static) -> Screen {
    Screen { name, make: Box::new(move || Box::new(make())) }
}

/// The window sizes drawn: the smallest the window allows, and a large one.
pub(crate) const SIZES: [(&str, [f32; 2]); 2] = [("small", [900.0, 600.0]), ("large", [1440.0, 900.0])];

fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-gallery")
}

fn all() -> Vec<Screen> {
    let mut v = own();
    v.extend(super::computers::gallery());
    v.extend(super::add_computer::gallery());
    #[cfg(feature = "host")]
    v.extend(super::host_page::gallery());
    v.extend(super::account::gallery());
    v.extend(super::settings::gallery());
    v.extend(super::groups::gallery());
    v.extend(super::admin::gallery());
    v.extend(crate::viewer::gallery());
    v
}

/// Draw `draw` on a page as the window does (page background).
pub(crate) fn page(ui: &mut egui::Ui, draw: impl FnOnce(&mut egui::Ui)) {
    let p = ui::pal(ui);
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(ui::space::XL as i8, 20)))
        .show(ui, draw);
}

/// The sidebar next to `draw`, as in the window.
pub(crate) fn with_sidebar(ui: &mut egui::Ui, page_: super::Page, draw: impl FnOnce(&mut egui::Ui)) {
    sidebar_and(ui, page_, false, draw);
}

/// [`with_sidebar`], with the server menu open if `open_menu`.
fn sidebar_and(ui: &mut egui::Ui, page_: super::Page, open_menu: bool, draw: impl FnOnce(&mut egui::Ui)) {
    let p = ui::pal(ui);
    let saved = super::account::Account::sample_saved();
    let model = super::Sidebar {
        page: page_,
        signed_in: true,
        admin: true,
        pending: 2,
        host: Some((p.warning, "em uso por Ana Beatriz".into())),
        account: Some(("jeferson.santos.silva@exemplo.com.br".into(), "rdesk da Família Santos".into())),
        saved: saved.into_iter().enumerate().map(|(i, s)| (s, i == 0)).collect(),
        open_menu,
        sessions: 1,
    };
    egui::Panel::left("nav")
        .exact_size(super::SIDEBAR_WIDTH)
        .resizable(false)
        .frame(egui::Frame::new().fill(p.bg_alt).inner_margin(egui::Margin::same(ui::space::S as i8)))
        .show(ui, |ui| super::sidebar(ui, &model));
    page(ui, draw);
}

/// A dialog with `lines` lines of text and buttons, over a page.
fn tall_dialog(lines: usize) -> impl FnMut(&mut egui::Ui) {
    move |ui: &mut egui::Ui| {
        page(ui, |ui| ui::page_title(ui, "Página por trás", None));
        ui::dialog_with_footer(
            ui.ctx(),
            "tall",
            "Um diálogo alto com um título que pode quebrar a linha",
            480.0,
            |ui| {
                for i in 0..lines {
                    ui::paragraph(ui, &format!("Linha {i}: um texto que ocupa espaço para o diálogo crescer."));
                }
            },
            |ui| ui::dialog_buttons(ui, "Salvar alterações", false, &["Cancelar"]),
        );
    }
}

fn own() -> Vec<Screen> {
    vec![
        screen("widgets-dialog-tall", || tall_dialog(18)),
        screen("widgets-dialog-huge", || tall_dialog(60)),
        screen("shell-sidebar", || {
            |ui: &mut egui::Ui| {
                with_sidebar(ui, super::Page::Computers, |ui| {
                    ui::page_title(ui, "Computadores", Some("Os computadores que você pode acessar"));
                })
            }
        }),
        screen("shell-sidebar-servers", || {
            |ui: &mut egui::Ui| {
                sidebar_and(ui, super::Page::Computers, true, |ui| {
                    ui::page_title(ui, "Computadores", Some("Os computadores que você pode acessar"));
                })
            }
        }),
        screen("widgets", || {
            let mut on = true;
            let mut search = String::new();
            let mut tab = 1;
            move |ui: &mut egui::Ui| {
                page(ui, |ui| {
                    ui::page_title(ui, "Componentes", Some("Uma amostra de cada peça, com textos longos"));
                    ui.horizontal_wrapped(|ui| {
                        ui::primary(ui, "Conectar ao computador");
                        ui::secondary(ui, "Arquivos");
                        ui::danger(ui, "Descadastrar do servidor");
                        ui::ghost(ui, "Cancelar");
                        ui::icon_button(ui, ui::icons::DOTS_THREE, "Mais");
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui::badge(ui, "controle total", ui::Tone::Accent);
                        ui::badge(ui, "terminal como administrador", ui::Tone::Warning);
                        ui::badge(ui, "Online", ui::Tone::Success);
                        ui::badge(ui, "Offline · visto há 3 dias", ui::Tone::Neutral);
                        ui::badge(ui, "Falhou", ui::Tone::Danger);
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui::chip(ui, true, "Todos 12");
                        ui::chip(ui, false, "Compartilhados comigo 3");
                        ui::chip(ui, false, "Nesta rede 2");
                        ui::toggle(ui, &mut on);
                    });
                    ui::search(ui, &mut search, "Buscar ou conectar: nome, ID ou ID@endereço");
                    ui::tabs(
                        ui,
                        &mut tab,
                        &[(0, "Visão geral"), (1, "Segurança"), (2, "Desempenho e arquivos"), (3, "Atividade")],
                    );
                    ui::banner(
                        ui,
                        ui::Tone::Warning,
                        "2 computadores aguardam sua aprovação: confira o código que cada um mostra antes de aprovar.",
                        Some("Revisar"),
                    );
                    ui::card(ui, |ui| {
                        ui::row(
                            ui,
                            |ui| {
                                ui::dot(ui, ui::pal(ui).success);
                                ui::truncate(
                                    ui,
                                    "Um nome de computador muito, muito comprido que não cabe na linha de jeito nenhum",
                                );
                            },
                            |ui| {
                                ui::primary(ui, "Conectar");
                            },
                        );
                        ui::caption(
                            ui,
                            "Uma legenda longa, que precisa quebrar a linha quando a janela é estreita, em vez de passar da borda do cartão.",
                        );
                    });
                    ui::empty_state(
                        ui,
                        ui::icons::DESKTOP,
                        "Nenhum computador ainda",
                        "Adicione um computador para acessá-lo de qualquer lugar.",
                    );
                });
            }
        }),
    ]
}

fn render(name: &str, size: [f32; 2], dark: bool, draw: Box<dyn FnMut(&mut egui::Ui)>) -> Result<PathBuf, String> {
    let mut draw = draw;
    let mut installed = false;
    let mut harness = egui_kittest::Harness::builder()
        .with_size(Vec2::from(size))
        .with_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light })
        .with_accessibility_check(false)
        .wgpu()
        .build_ui(move |ui| {
            if !installed {
                ui::install(ui.ctx());
                installed = true;
            }
            draw(ui);
        });
    // A few frames: fonts and layouts settle (spinners never do, so no `run`).
    harness.run_steps(6);
    let image = harness.render()?;
    let path = out_dir().join(format!("{name}.png"));
    image.save(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

#[test]
#[ignore = "draws every screen to target/ui-gallery (needs a GPU or a software adapter)"]
fn ui_gallery() {
    let filter = std::env::var("RDESK_GALLERY").unwrap_or_default();
    let langs = std::env::var("RDESK_GALLERY_LANGS").unwrap_or_else(|_| "pt,en".into());
    let themes = std::env::var("RDESK_GALLERY_THEMES").unwrap_or_else(|_| "dark".into());
    std::fs::create_dir_all(out_dir()).unwrap();
    let mut count = 0;
    for lang in langs.split(',') {
        i18n::set(if lang.trim() == "en" { Lang::En } else { Lang::Pt });
        for s in all().into_iter().filter(|s| s.name.contains(&filter)) {
            for theme in themes.split(',') {
                let dark = theme.trim() != "light";
                for (size_name, size) in SIZES {
                    let name = format!("{}-{}-{}-{}", s.name, lang.trim(), theme.trim(), size_name);
                    match render(&name, size, dark, (s.make)()) {
                        Ok(p) => {
                            println!("{}", p.display());
                            count += 1;
                        }
                        Err(e) => panic!("{name}: {e}"),
                    }
                }
            }
        }
    }
    i18n::set(Lang::En);
    println!("{count} picture(s)");
}
