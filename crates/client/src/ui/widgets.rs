//! The building blocks of every screen. They size to their text and wrap or truncate
//! instead of overflowing (see the rules in [`crate::ui`]).

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Response, RichText, Sense, Stroke, Ui, Vec2,
    WidgetText,
};

use super::icons;
use super::theme::{Palette, RADIUS, RADIUS_CARD, pal, palette, size, space, tint};

/// What a badge, banner or message is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
}

impl Tone {
    pub fn color(self, p: &Palette) -> Color32 {
        match self {
            Tone::Neutral => p.muted,
            Tone::Accent => p.accent,
            Tone::Success => p.success,
            Tone::Warning => p.warning,
            Tone::Danger => p.danger,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Tone::Neutral | Tone::Accent => icons::INFO,
            Tone::Success => icons::CHECK_CIRCLE,
            Tone::Warning => icons::WARNING,
            Tone::Danger => icons::WARNING_CIRCLE,
        }
    }
}

/// `icon` then `text`, for buttons and labels.
pub fn with_icon(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// The main action of a view (at most one per view): filled with the accent color.
pub fn primary(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = pal(ui);
    ui.add(
        egui::Button::new(RichText::new(text.into()).color(p.on_accent).strong())
            .fill(p.accent)
            .stroke(Stroke::NONE)
            .min_size(Vec2::new(0.0, 32.0)),
    )
}

/// An action that removes or ends something: red.
pub fn danger(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = pal(ui);
    ui.add(
        egui::Button::new(RichText::new(text.into()).color(p.on_accent).strong())
            .fill(p.danger)
            .stroke(Stroke::NONE)
            .min_size(Vec2::new(0.0, 32.0)),
    )
}

/// An ordinary action.
pub fn secondary(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Button::new(text.into()).min_size(Vec2::new(0.0, 32.0)))
}

/// A quiet action (no frame until hovered), e.g. "Cancel" next to a primary button.
pub fn ghost(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Button::new(text.into()).frame_when_inactive(false).min_size(Vec2::new(0.0, 32.0)))
}

/// A frameless icon button; `hover` names it (tooltip and screen readers).
pub fn icon_button(ui: &mut Ui, icon: &str, hover: &str) -> Response {
    let p = pal(ui);
    ui.add(
        egui::Button::new(RichText::new(icon).size(size::ICON + 1.0).color(p.text))
            .frame_when_inactive(false)
            .min_size(Vec2::splat(30.0)),
    )
    .on_hover_text(hover)
}

/// An icon button in a color (e.g. danger).
pub fn icon_button_colored(ui: &mut Ui, icon: &str, hover: &str, color: Color32) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon).size(size::ICON + 1.0).color(color))
            .frame_when_inactive(false)
            .min_size(Vec2::splat(30.0)),
    )
    .on_hover_text(hover)
}

/// A link-looking action inside text or a card.
pub fn link(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Link::new(RichText::new(text.into())))
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// A page's title, with an optional line under it.
pub fn page_title(ui: &mut Ui, title: &str, subtitle: Option<&str>) {
    ui.add(egui::Label::new(RichText::new(title).size(size::TITLE).strong()).wrap());
    if let Some(s) = subtitle {
        caption(ui, s);
    }
}

/// A section title inside a page or card.
pub fn section(ui: &mut Ui, title: &str) {
    ui.add(egui::Label::new(RichText::new(title).size(size::SECTION).strong()).wrap());
}

/// Muted text for metadata.
pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(Color32::PLACEHOLDER)
}

/// A small, muted, wrapped line.
pub fn caption(ui: &mut Ui, text: &str) -> Response {
    let c = pal(ui).muted;
    ui.add(egui::Label::new(RichText::new(text).size(size::SMALL).color(c)).wrap())
}

/// A sentence or two, wrapped, at a readable width.
pub fn paragraph(ui: &mut Ui, text: &str) -> Response {
    let w = ui.available_width().min(640.0);
    ui.scope(|ui| {
        ui.set_max_width(w);
        ui.add(egui::Label::new(text).wrap())
    })
    .inner
}

/// One line that never pushes the layout wider: cut with "…", the whole text on hover.
pub fn truncate(ui: &mut Ui, text: impl Into<WidgetText>) -> Response {
    ui.add(egui::Label::new(text).truncate())
}

/// Monospace text that can be selected and copied.
pub fn mono(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(text).monospace()).truncate())
}

/// A long ID shortened for display ("k3f9 7a2c … 81de").
pub fn short_id(id: &str) -> String {
    let clean: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if clean.len() <= 16 {
        return clean;
    }
    format!("{} {} … {}", &clean[..4], &clean[4..8], &clean[clean.len() - 4..])
}

// ---------------------------------------------------------------------------
// Small pieces
// ---------------------------------------------------------------------------

/// A pill with text, sized to the text (never clipped).
pub fn badge(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let p = pal(ui);
    let c = tone.color(&p);
    let fill = tint(c, if p.dark { 46 } else { 30 });
    // Drawn, not a frame around a label: exactly the text's size plus padding, whatever
    // the height of the row it sits in.
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(size::SMALL), c);
    let pad = Vec2::new(8.0, 3.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + 2.0 * pad, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(255), fill);
    ui.painter().galley(rect.min + pad, galley, c);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
    resp
}

/// A round status dot.
pub fn dot(ui: &mut Ui, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
    resp
}

/// A selectable pill (filters).
pub fn chip(ui: &mut Ui, selected: bool, text: &str) -> Response {
    let p = pal(ui);
    let (fill, stroke, color) = if selected {
        (tint(p.accent, if p.dark { 60 } else { 34 }), Stroke::new(1.0, tint(p.accent, 180)), p.text)
    } else {
        (Color32::TRANSPARENT, Stroke::new(1.0, p.border), p.muted)
    };
    ui.add(
        egui::Button::new(RichText::new(text).size(size::SMALL + 0.5).color(color))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(255))
            .min_size(Vec2::new(0.0, 26.0)),
    )
}

/// An on/off switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let p = pal(ui);
    let size = Vec2::new(38.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = if *on { p.accent } else { p.border };
    ui.painter().rect_filled(rect, CornerRadius::same(255), track);
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 8.0, Color32::WHITE);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, true, *on, "toggle"));
    resp
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

/// A card: a surface with a border and padding.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let p = pal(ui);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(Margin::same(space::L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
}

/// A message across the page (e.g. "2 computers are waiting for your approval"), with an
/// optional action. Returns `true` when the action was clicked.
pub fn banner(ui: &mut Ui, tone: Tone, text: &str, action: Option<&str>) -> bool {
    let p = pal(ui);
    let c = tone.color(&p);
    let mut clicked = false;
    egui::Frame::new()
        .fill(tint(c, if p.dark { 34 } else { 22 }))
        .stroke(Stroke::new(1.0, tint(c, 110)))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            row(
                ui,
                |ui| {
                    ui.label(RichText::new(tone.icon()).size(size::ICON).color(c));
                    ui.add(egui::Label::new(RichText::new(text).color(p.text)).wrap());
                },
                |ui| {
                    if let Some(a) = action
                        && ui.button(a).clicked()
                    {
                        clicked = true;
                    }
                },
            );
        });
    clicked
}

/// A friendly empty area: what goes here, and what to do.
pub fn empty_state(ui: &mut Ui, icon: &str, title: &str, body: &str) {
    let p = pal(ui);
    ui.vertical_centered(|ui| {
        ui.set_max_width(ui.available_width().min(460.0));
        ui.add_space(space::XL);
        ui.label(RichText::new(icon).size(34.0).color(p.muted));
        ui.add_space(space::S);
        ui.add(egui::Label::new(RichText::new(title).size(size::SECTION).strong()).wrap());
        ui.add(egui::Label::new(RichText::new(body).color(p.muted)).wrap());
        ui.add_space(space::S);
    });
}

/// A line with content on the left and controls on the right. The right side is laid
/// out first; the left side gets the rest (use [`truncate`] there for names).
pub fn row<R>(ui: &mut Ui, left: impl FnOnce(&mut Ui) -> R, right: impl FnOnce(&mut Ui)) -> R {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            right(ui);
            ui.with_layout(Layout::left_to_right(Align::Center), left).inner
        })
        .inner
    })
    .inner
}

/// Label/value lines (a two-column grid); values wrap.
pub fn facts(ui: &mut Ui, id: &str, items: &[(&str, String)]) {
    let p = pal(ui);
    egui::Grid::new(id).num_columns(2).spacing([space::L, space::S]).show(ui, |ui| {
        for (k, v) in items {
            ui.label(RichText::new(*k).color(p.muted));
            ui.add(egui::Label::new(v.as_str()).wrap());
            ui.end_row();
        }
    });
}

/// Underlined tabs; they wrap to a second line when the window is narrow. Returns `true`
/// when the tab changed.
pub fn tabs<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, &str)]) -> bool {
    let p = pal(ui);
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::L;
        for (value, label) in items {
            let on = *current == *value;
            let text = RichText::new(*label).color(if on { p.text } else { p.muted });
            let resp = ui.add(egui::Button::new(if on { text.strong() } else { text }).frame(false));
            let r = resp.rect;
            if on {
                ui.painter().line_segment(
                    [egui::pos2(r.left() + 4.0, r.bottom() + 2.0), egui::pos2(r.right() - 4.0, r.bottom() + 2.0)],
                    Stroke::new(2.0, p.accent),
                );
            }
            if resp.clicked() && !on {
                *current = *value;
                changed = true;
            }
        }
    });
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.border));
    changed
}

/// A search field with a magnifier, as wide as allowed.
pub fn search(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    let p = pal(ui);
    let w = ui.available_width();
    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.set_width(w - 18.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::MAGNIFYING_GLASS).color(p.muted));
                ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text(hint)
                        .frame(egui::Frame::NONE)
                        .desired_width(ui.available_width())
                        .align(Align2::LEFT_CENTER)
                        .min_size(Vec2::new(0.0, 30.0))
                        .margin(Margin::symmetric(0, 4)),
                )
            })
            .inner
        })
        .inner
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

/// What a dialog's frame reports.
pub struct DialogResponse<R> {
    pub inner: R,
    /// Escape, a click outside, or the close button.
    pub closed: bool,
}

/// A modal dialog with a title and a close button, at most `width` wide (and never wider
/// than the window), scrolling when taller than the window. Prefer [`dialog_with_footer`]
/// when it ends with buttons: they then stay visible while the body scrolls.
pub fn dialog<R>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    width: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> DialogResponse<R> {
    let d = dialog_impl(ctx, id, title, width, add, None::<fn(&mut Ui)>);
    DialogResponse { inner: d.inner.0, closed: d.closed }
}

/// Like [`dialog`], with `footer` (the buttons, see [`dialog_buttons`]) kept below the
/// scrolling body, always visible.
pub fn dialog_with_footer<R, F>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    width: f32,
    body: impl FnOnce(&mut Ui) -> R,
    footer: impl FnOnce(&mut Ui) -> F,
) -> DialogResponse<(R, F)> {
    let d = dialog_impl(ctx, id, title, width, body, Some(footer));
    let (r, f) = d.inner;
    DialogResponse { inner: (r, f.expect("footer drawn")), closed: d.closed }
}

fn dialog_impl<R, F>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    width: f32,
    body: impl FnOnce(&mut Ui) -> R,
    footer: Option<impl FnOnce(&mut Ui) -> F>,
) -> DialogResponse<(R, Option<F>)> {
    let screen = ctx.content_rect();
    let width = width.min(screen.width() - 88.0).max(260.0);
    let max_h = (screen.height() - 150.0).max(200.0);
    let mut close = false;
    let frame = egui::Frame::window(&ctx.global_style())
        .inner_margin(Margin::same(20))
        .corner_radius(CornerRadius::same(RADIUS_CARD));
    let m = egui::Modal::new(egui::Id::unique(("rdesk-dialog", id))).frame(frame).show(ctx, |ui| {
        ui.set_width(width);
        row(
            ui,
            |ui| {
                ui.add(egui::Label::new(RichText::new(title).size(size::SECTION).strong()).wrap());
            },
            |ui| {
                if icon_button(ui, icons::X, rdesk_common::t!("Close", "Fechar")).clicked() {
                    close = true;
                }
            },
        );
        ui.add_space(space::XS);
        // A modal's area is only as tall as it was last frame (400 points at first), and a
        // scroll area never asks for more than that: ask for the content's height (up to the
        // window) so a tall dialog grows instead of scrolling inside a short box.
        let key = egui::Id::unique(("rdesk-dialog-height", id));
        let footer_key = egui::Id::unique(("rdesk-dialog-footer", id));
        let footer_h =
            if footer.is_some() { ui.ctx().data(|d| d.get_temp::<f32>(footer_key)).unwrap_or(48.0) } else { 0.0 };
        let body_max = (max_h - footer_h).max(120.0);
        let last = ui.ctx().data(|d| d.get_temp::<f32>(key)).unwrap_or(body_max);
        let out = egui::ScrollArea::vertical()
            .max_height(body_max)
            .min_scrolled_height(last.min(body_max))
            .auto_shrink([false, true])
            .show(ui, body);
        ui.ctx().data_mut(|d| d.insert_temp(key, out.content_size.y));
        let f = footer.map(|footer| {
            let r = ui.scope(footer);
            let h = r.response.rect.height();
            ui.ctx().data_mut(|d| d.insert_temp(footer_key, h));
            r.inner
        });
        (out.inner, f)
    });
    let closed = close || m.should_close();
    DialogResponse { inner: m.inner, closed }
}

/// The buttons at the bottom of a dialog: `primary` on the right, then the others.
/// Returns which was clicked (0 = the first in `others`).
pub enum DialogButton {
    Primary,
    Other(usize),
}

pub fn dialog_buttons(ui: &mut Ui, primary_label: &str, danger_style: bool, others: &[&str]) -> Option<DialogButton> {
    ui.add_space(space::S);
    let mut out = None;
    ui.with_layout(Layout::right_to_left(Align::Center).with_main_wrap(true), |ui| {
        let r = if danger_style { danger(ui, primary_label) } else { primary(ui, primary_label) };
        if r.clicked() {
            out = Some(DialogButton::Primary);
        }
        for (i, o) in others.iter().enumerate() {
            if secondary(ui, *o).clicked() {
                out = Some(DialogButton::Other(i));
            }
        }
    });
    out
}

/// "Are you sure?" before something that cannot be undone. `Some(true)` confirmed,
/// `Some(false)` cancelled, `None` still open.
pub fn confirm(ctx: &egui::Context, id: &str, title: &str, body: &str, ok: &str, danger_style: bool) -> Option<bool> {
    let cancel = rdesk_common::t!("Cancel", "Cancelar");
    let d = dialog_with_footer(
        ctx,
        id,
        title,
        440.0,
        |ui| {
            paragraph(ui, body);
        },
        |ui| dialog_buttons(ui, ok, danger_style, &[cancel]),
    );
    match d.inner.1 {
        Some(DialogButton::Primary) => Some(true),
        Some(DialogButton::Other(_)) => Some(false),
        None if d.closed => Some(false),
        None => None,
    }
}

/// A spinner with what is happening.
pub fn busy(ui: &mut Ui, text: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spinner();
        caption(ui, text);
    });
}

/// The font for an icon at `size`.
pub fn icon_font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Paint `text` centered in `rect` (for custom widgets).
pub fn paint_centered(ui: &Ui, rect: egui::Rect, text: &str, font: FontId, color: Color32) {
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, color);
}

/// The palette for a theme flag (for code without a `Ui`).
pub fn palette_for(dark: bool) -> Palette {
    palette(dark)
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

/// What a navigation item shows on its right.
pub enum Trailing {
    None,
    /// A count to act on (e.g. computers waiting for approval).
    Count(usize),
    /// A status dot.
    Dot(Color32),
}

/// One line of the sidebar: icon, label (truncated if the sidebar is too narrow) and a
/// count or dot on the right.
pub fn nav_item(ui: &mut Ui, icon: &str, label: &str, selected: bool, trailing: Trailing) -> Response {
    let p = pal(ui);
    let h = 36.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click());
    let fill = if selected {
        tint(p.accent, if p.dark { 52 } else { 30 })
    } else if resp.hovered() {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), fill);
    let color = if selected { p.text } else { p.muted };
    let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), Vec2::splat(20.0));
    paint_centered(ui, icon_rect, icon, icon_font(size::ICON + 1.0), if selected { p.accent } else { color });

    // The right side first, so the label knows how much room it has.
    let mut right = rect.right() - 10.0;
    match trailing {
        Trailing::None => {}
        Trailing::Dot(c) => {
            ui.painter().circle_filled(egui::pos2(right - 4.0, rect.center().y), 4.5, c);
            right -= 16.0;
        }
        Trailing::Count(0) => {}
        Trailing::Count(n) => {
            let text = n.to_string();
            let galley = ui.painter().layout_no_wrap(text, egui::FontId::proportional(size::SMALL), p.on_accent);
            let w = (galley.size().x + 12.0).max(20.0);
            let pill = egui::Rect::from_center_size(egui::pos2(right - w / 2.0, rect.center().y), Vec2::new(w, 18.0));
            ui.painter().rect_filled(pill, CornerRadius::same(255), p.warning);
            ui.painter().galley(pill.center() - galley.size() / 2.0, galley, p.on_accent);
            right = pill.left() - 6.0;
        }
    }
    let text_left = rect.left() + 38.0;
    let max_w = (right - text_left).max(10.0);
    let mut job = egui::text::LayoutJob::single_section(
        label.to_owned(),
        egui::TextFormat::simple(egui::FontId::proportional(size::BODY), color),
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    let galley = ui.painter().layout_job(job);
    let elided = galley.elided;
    ui.painter().galley(egui::pos2(text_left, rect.center().y - galley.size().y / 2.0), galley, color);
    let resp = if elided { resp.on_hover_text(label) } else { resp };
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, selected, label));
    resp
}

/// A round avatar with someone's initials.
pub fn avatar(ui: &mut Ui, name_or_email: &str, diameter: f32) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::hover());
    ui.painter().circle_filled(rect.center(), diameter / 2.0, tint(p.accent, if p.dark { 90 } else { 50 }));
    let initials: String = name_or_email
        .split([' ', '.', '@', '_', '-'])
        .filter(|w| !w.is_empty())
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    paint_centered(ui, rect, &initials, egui::FontId::proportional(diameter * 0.4), p.text);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids() {
        assert_eq!(short_id("abcd"), "abcd");
        let id = "k3f97a2c11112222333344445555e81d";
        assert_eq!(short_id(id), "k3f9 7a2c … e81d");
        assert_eq!(short_id("k3f9-7a2c-1111-2222-3333-4444-5555-e81d"), "k3f9 7a2c … e81d");
    }
}
