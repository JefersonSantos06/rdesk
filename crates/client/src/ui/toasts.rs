//! Messages in the bottom-right corner: news fades after a few seconds, errors stay until
//! closed.

use std::time::{Duration, Instant};

use egui::{Align2, CornerRadius, Margin, RichText, Stroke};

use super::icons;
use super::theme::{RADIUS_CARD, pal, size, space, tint};
use super::widgets::{Tone, icon_button, row};

const SHOW_FOR: Duration = Duration::from_secs(6);
const MAX_SHOWN: usize = 4;
const WIDTH: f32 = 380.0;

struct Toast {
    id: u64,
    text: String,
    tone: Tone,
    at: Instant,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
    next: u64,
}

impl Toasts {
    /// A message; `error` ones stay until closed.
    pub fn push(&mut self, text: impl Into<String>, error: bool) {
        self.push_tone(text, if error { Tone::Danger } else { Tone::Success });
    }

    pub fn push_tone(&mut self, text: impl Into<String>, tone: Tone) {
        let text = text.into();
        // The same message again just refreshes it.
        self.items.retain(|t| t.text != text);
        self.next += 1;
        self.items.push(Toast { id: self.next, text, tone, at: Instant::now() });
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        self.items.retain(|t| t.tone == Tone::Danger || t.at.elapsed() < SHOW_FOR);
        if self.items.len() > MAX_SHOWN {
            let drop = self.items.len() - MAX_SHOWN;
            self.items.drain(..drop);
        }
        if self.items.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let width = WIDTH.min(screen.width() - 2.0 * space::L);
        let mut close = None;
        egui::Area::new(egui::Id::unique("rdesk-toasts"))
            .anchor(Align2::RIGHT_BOTTOM, [-space::L, -space::L])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(width);
                for t in self.items.iter().rev() {
                    let p = pal(ui);
                    let c = t.tone.color(&p);
                    egui::Frame::new()
                        .fill(p.surface)
                        .stroke(Stroke::new(1.0, tint(c, 150)))
                        .corner_radius(CornerRadius::same(RADIUS_CARD))
                        .inner_margin(Margin::symmetric(12, 8))
                        .shadow(ui.visuals().popup_shadow)
                        .show(ui, |ui| {
                            ui.set_width(width - 24.0);
                            row(
                                ui,
                                |ui| {
                                    ui.label(RichText::new(t.tone.icon()).size(size::ICON).color(c));
                                    ui.add(egui::Label::new(t.text.as_str()).wrap());
                                },
                                |ui| {
                                    if icon_button(ui, icons::X, rdesk_common::t!("Close", "Fechar")).clicked() {
                                        close = Some(t.id);
                                    }
                                },
                            );
                        });
                    ui.add_space(space::S);
                }
            });
        if let Some(id) = close {
            self.items.retain(|t| t.id != id);
        }
        if let Some(oldest) =
            self.items.iter().filter(|t| t.tone != Tone::Danger).map(|t| SHOW_FOR.saturating_sub(t.at.elapsed())).min()
        {
            ctx.request_repaint_after(oldest);
        }
    }
}
