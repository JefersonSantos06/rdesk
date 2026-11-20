//! Colors, sizes and fonts: one place for the whole look, light and dark (following the
//! system).

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle, Theme};

/// The icon font (Phosphor, regular), a fallback after the text font.
const PHOSPHOR: &[u8] = include_bytes!("../../assets/fonts/Phosphor.ttf");

/// Text sizes, in points.
pub mod size {
    /// A page's title.
    pub const TITLE: f32 = 20.0;
    /// A section inside a page, a card's title, a dialog's title.
    pub const SECTION: f32 = 15.5;
    pub const BODY: f32 = 14.0;
    /// Captions, metadata, badges.
    pub const SMALL: f32 = 12.0;
    pub const MONO: f32 = 13.0;
    /// Icons next to body text.
    pub const ICON: f32 = 16.0;
}

/// Spacing, in points: use these (and only these) between things.
pub mod space {
    pub const XS: f32 = 4.0;
    pub const S: f32 = 8.0;
    pub const M: f32 = 12.0;
    pub const L: f32 = 16.0;
    pub const XL: f32 = 24.0;
}

/// Corner radius of controls; cards and dialogs use [`RADIUS_CARD`].
pub const RADIUS: u8 = 6;
pub const RADIUS_CARD: u8 = 10;

/// The colors of one theme.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// The page.
    pub bg: Color32,
    /// The sidebar and other secondary areas.
    pub bg_alt: Color32,
    /// Cards, dialogs, menus.
    pub surface: Color32,
    /// A row under the mouse, a selected row.
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    /// Text on an accent (or danger) fill.
    pub on_accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            dark,
            bg: Color32::from_rgb(24, 25, 28),
            bg_alt: Color32::from_rgb(30, 32, 36),
            surface: Color32::from_rgb(36, 38, 43),
            hover: Color32::from_rgb(45, 48, 55),
            border: Color32::from_rgb(58, 61, 69),
            text: Color32::from_rgb(230, 232, 236),
            muted: Color32::from_rgb(150, 156, 166),
            accent: Color32::from_rgb(82, 145, 255),
            on_accent: Color32::WHITE,
            success: Color32::from_rgb(70, 190, 115),
            warning: Color32::from_rgb(235, 170, 60),
            danger: Color32::from_rgb(235, 87, 87),
        }
    } else {
        Palette {
            dark,
            bg: Color32::from_rgb(247, 248, 250),
            bg_alt: Color32::from_rgb(238, 240, 244),
            surface: Color32::WHITE,
            hover: Color32::from_rgb(232, 236, 243),
            border: Color32::from_rgb(218, 222, 229),
            text: Color32::from_rgb(24, 27, 32),
            muted: Color32::from_rgb(98, 104, 115),
            accent: Color32::from_rgb(37, 99, 235),
            on_accent: Color32::WHITE,
            success: Color32::from_rgb(22, 140, 75),
            warning: Color32::from_rgb(176, 110, 0),
            danger: Color32::from_rgb(200, 40, 40),
        }
    }
}

/// The colors of the theme `ui` is drawn with.
pub fn pal(ui: &egui::Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

/// `c` with the given opacity (for tinted backgrounds).
pub fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// Fonts and both themes; the system decides light or dark.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("phosphor".into(), Arc::new(FontData::from_static(PHOSPHOR)));
    // Second: the text font draws letters (Phosphor maps a–z to blank glyphs), icons fall
    // through to Phosphor.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        if let Some(keys) = fonts.families.get_mut(&family) {
            let at = keys.len().min(1);
            keys.insert(at, "phosphor".into());
        }
    }
    ctx.set_fonts(fonts);
    ctx.set_style_of(Theme::Dark, style(true));
    ctx.set_style_of(Theme::Light, style(false));
}

/// The egui style for one theme.
pub fn style(dark: bool) -> egui::Style {
    let p = palette(dark);
    let mut s = egui::Style {
        visuals: if dark { egui::Visuals::dark() } else { egui::Visuals::light() },
        ..Default::default()
    };

    s.text_styles = [
        (TextStyle::Heading, FontId::proportional(size::TITLE)),
        (TextStyle::Body, FontId::proportional(size::BODY)),
        (TextStyle::Button, FontId::proportional(size::BODY)),
        (TextStyle::Small, FontId::proportional(size::SMALL)),
        (TextStyle::Monospace, FontId::monospace(size::MONO)),
    ]
    .into();

    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(space::S, space::S);
    sp.button_padding = egui::vec2(12.0, 6.0);
    sp.interact_size = egui::vec2(32.0, 30.0);
    sp.window_margin = Margin::same(16);
    sp.menu_margin = Margin::same(6);
    sp.indent = 18.0;
    sp.combo_width = 180.0;
    sp.text_edit_width = 260.0;
    sp.icon_width = 16.0;
    sp.icon_width_inner = 9.0;
    sp.icon_spacing = 6.0;
    sp.tooltip_width = 420.0;
    sp.menu_width = 260.0;
    // Scroll bars that show there is more (not thin ones that appear on hover).
    sp.scroll = egui::style::ScrollStyle::solid();

    let v = &mut s.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(RADIUS_CARD);
    v.menu_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = if dark { Color32::from_rgb(20, 21, 24) } else { Color32::WHITE };
    v.faint_bg_color = p.bg_alt;
    v.code_bg_color = p.bg_alt;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = tint(p.accent, if dark { 120 } else { 70 });
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.striped = false;
    v.button_frame = true;
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.slider_trailing_fill = true;

    let radius = CornerRadius::same(RADIUS);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.corner_radius = radius;

    let button = if dark { Color32::from_rgb(44, 47, 54) } else { Color32::from_rgb(241, 243, 247) };
    let button_hover = if dark { Color32::from_rgb(54, 58, 66) } else { Color32::from_rgb(230, 234, 241) };
    w.inactive.weak_bg_fill = button;
    w.inactive.bg_fill = if dark { Color32::from_rgb(52, 55, 63) } else { Color32::from_rgb(222, 226, 233) };
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = radius;

    w.hovered.weak_bg_fill = button_hover;
    w.hovered.bg_fill = button_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, tint(p.accent, 160));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;

    w.active.weak_bg_fill = p.hover;
    w.active.bg_fill = p.hover;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;

    w.open.weak_bg_fill = p.hover;
    w.open.bg_fill = p.hover;
    w.open.bg_stroke = Stroke::new(1.0, p.border);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = radius;
    s
}
