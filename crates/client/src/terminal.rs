//! The remote terminal on the client: the stream to the host's shell ([`open`], from
//! `rdesk-core`) and an egui widget that draws it ([`TermView`]: a VT screen kept by `vt100`) and turns keys
//! into what a terminal sends.

use std::num::NonZeroU16;

use egui::{Color32, FontId, Key, Modifiers, text::LayoutJob};
use rdesk_common::{t, tf};
use rdesk_proto::{TermClient, TermHost};
use tokio::sync::mpsc;

pub use rdesk_core::terminal::{TermSlot, TerminalLink, answers, open};

/// What a key sends to a terminal (xterm). `app_cursor`: the program asked for the
/// "application" arrow keys (full-screen programs like vim, less).
pub fn key_bytes(key: Key, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let csi = |s: &str| format!("\x1b[{s}").into_bytes();
    let arrow = |c: char| {
        if mods.shift || mods.ctrl || mods.alt {
            let m = 1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.ctrl);
            format!("\x1b[1;{m}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    if mods.ctrl && !mods.alt {
        let name = key.name();
        if name.len() == 1 {
            let c = name.as_bytes()[0].to_ascii_uppercase();
            if c.is_ascii_uppercase() {
                return Some(vec![c - b'@']);
            }
        }
        match key {
            Key::OpenBracket => return Some(vec![0x1b]),
            Key::Backslash => return Some(vec![0x1c]),
            Key::CloseBracket => return Some(vec![0x1d]),
            Key::Space => return Some(vec![0]),
            _ => {}
        }
    }
    Some(match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => vec![0x7f],
        Key::Tab if mods.shift => csi("Z"),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => vec![0x1b],
        Key::ArrowUp => arrow('A'),
        Key::ArrowDown => arrow('B'),
        Key::ArrowRight => arrow('C'),
        Key::ArrowLeft => arrow('D'),
        Key::Home => arrow('H'),
        Key::End => arrow('F'),
        Key::Insert => csi("2~"),
        Key::Delete => csi("3~"),
        Key::PageUp => csi("5~"),
        Key::PageDown => csi("6~"),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => csi("15~"),
        Key::F6 => csi("17~"),
        Key::F7 => csi("18~"),
        Key::F8 => csi("19~"),
        Key::F9 => csi("20~"),
        Key::F10 => csi("21~"),
        Key::F11 => csi("23~"),
        Key::F12 => csi("24~"),
        _ => return None,
    })
}

/// The 16 basic colors (xterm's), then the 6×6×6 cube and the grays.
fn indexed(i: u8) -> Color32 {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    match i {
        0..=15 => {
            let (r, g, b) = BASE[i as usize];
            Color32::from_rgb(r, g, b)
        }
        16..=231 => {
            let i = i - 16;
            let v = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            Color32::from_rgb(v(i / 36), v((i / 6) % 6), v(i % 6))
        }
        _ => {
            let g = 8 + (i - 232) * 10;
            Color32::from_rgb(g, g, g)
        }
    }
}

const FG: Color32 = Color32::from_rgb(220, 220, 220);
const BG: Color32 = Color32::from_rgb(16, 16, 16);

fn color(c: vt100::Color, default: Color32) -> Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => indexed(i),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

/// A cell of the visible grid: (row, column).
type Cell = (u16, u16);

/// A terminal's contents and state, drawn by [`TermView::ui`].
pub struct TermView {
    parser: vt100::Parser,
    link: Option<TerminalLink>,
    /// "PowerShell as ana", "Exited (code 0)", an error…
    pub status: String,
    pub ended: bool,
    size: (u16, u16),
    font: FontId,
    /// The keyboard goes here (it had the focus when last drawn).
    pub focused: bool,
    /// Take the keyboard when next drawn.
    want_focus: bool,
    /// Selected text on the visible grid: where the drag started and where it is now.
    selection: Option<(Cell, Cell)>,
}

impl TermView {
    pub fn new(link: TerminalLink, cols: u16, rows: u16) -> Self {
        let nz = |v: u16| NonZeroU16::new(v.max(2)).unwrap();
        Self {
            parser: vt100::Parser::new(nz(rows), nz(cols), 5000),
            link: Some(link),
            status: t!("Starting…", "Iniciando…").into(),
            ended: false,
            size: (cols, rows),
            font: FontId::monospace(14.0),
            focused: false,
            want_focus: true,
            selection: None,
        }
    }

    /// A terminal showing `output`, without a host (the ui gallery).
    #[cfg(test)]
    pub(crate) fn sample(status: &str, output: &[u8], cols: u16, rows: u16) -> Self {
        let nz = |v: u16| NonZeroU16::new(v.max(2)).unwrap();
        let mut parser = vt100::Parser::new(nz(rows), nz(cols), 5000);
        parser.process(output);
        Self {
            parser,
            link: None,
            status: status.into(),
            ended: false,
            size: (cols, rows),
            font: FontId::monospace(14.0),
            focused: false,
            want_focus: false,
            selection: None,
        }
    }

    /// Select from `a` to `b`, (row, column) on the visible grid (the ui gallery).
    #[cfg(test)]
    pub(crate) fn select(&mut self, a: Cell, b: Cell) {
        self.selection = Some((a, b));
    }

    /// Look back `rows` lines (the ui gallery).
    #[cfg(test)]
    pub(crate) fn scroll_back(&mut self, rows: usize) {
        self.parser.screen_mut().set_scrollback(rows);
    }

    /// Mark it ended, as when its shell exits (the ui gallery).
    #[cfg(test)]
    pub(crate) fn end(&mut self, status: &str) {
        self.status = status.into();
        self.ended = true;
    }

    /// Give it the keyboard when it is next drawn.
    pub fn request_focus(&mut self) {
        self.want_focus = true;
    }

    fn send(&self, m: TermClient) {
        if let Some(l) = &self.link {
            let _ = l.tx.send(m);
        }
    }

    /// Take what the host sent; `true` if anything changed.
    pub fn poll(&mut self) -> bool {
        let Some(link) = &mut self.link else { return false };
        let mut changed = false;
        loop {
            match link.rx.try_recv() {
                Ok(TermHost::Output(bytes)) => {
                    self.parser.process(&bytes);
                    let reply = answers(&bytes, self.parser.screen().cursor_position());
                    if !reply.is_empty() {
                        let _ = link.tx.send(TermClient::Input(reply));
                    }
                    changed = true;
                }
                Ok(TermHost::Started { shell, user }) => {
                    self.status = if user.is_empty() { shell } else { tf!("{shell} as {user}", "{shell} como {user}") };
                    changed = true;
                }
                Ok(TermHost::Exited(code)) => {
                    self.status = tf!("Ended (code {code})", "Encerrado (código {code})");
                    self.ended = true;
                    changed = true;
                }
                Ok(TermHost::Error(e)) => {
                    self.status = e;
                    self.ended = true;
                    changed = true;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if !self.ended {
                        self.status = t!("Disconnected", "Desconectado").into();
                        self.ended = true;
                        changed = true;
                    }
                    self.link = None;
                    break;
                }
            }
        }
        changed
    }

    /// The selection in reading order (start before end), if any.
    fn selection_range(&self) -> Option<(Cell, Cell)> {
        let (a, b) = self.selection?;
        // Within the grid (it may have shrunk since).
        let size = self.parser.screen().size();
        let (rows, cols) = (size.0.get(), size.1.get());
        let fit = |(r, c): Cell| (r.min(rows - 1), c.min(cols - 1));
        let (a, b) = (fit(a), fit(b));
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// The selected text, one line per row (trailing blanks dropped).
    fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        let text = self.parser.screen().contents_between(sr, sc, er, ec.saturating_add(1));
        (!text.is_empty()).then_some(text)
    }

    /// What a paste sends: line ends as Enter, marked as a paste for programs that asked.
    fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let bracketed = self.parser.screen().bracketed_paste();
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(text.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
        if bracketed {
            out.extend_from_slice(b"\x1b[201~");
        }
        out
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        if let Some(text) = self.selected_text() {
            ctx.copy_text(text);
        }
    }

    /// Paste what the system clipboard holds (the right-click menu, Ctrl+Shift+V).
    fn paste_clipboard(&mut self) {
        match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(text) if !text.is_empty() => {
                let bytes = self.paste_bytes(&text);
                self.input(bytes);
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("clipboard: {e}"),
        }
    }

    /// Send typed bytes: back to the live screen, the selection gone.
    fn input(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.parser.screen_mut().set_scrollback(0);
        self.selection = None;
        self.send(TermClient::Input(bytes));
    }

    /// Draw it in the space left in `ui`, and send keys typed while it has the focus.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll();
        let (cw, ch) = ui.fonts_mut(|f| (f.glyph_width(&self.font, 'M'), f.row_height(&self.font)));
        let avail = ui.available_size();
        let cols = ((avail.x / cw).floor() as u16).clamp(10, 1000);
        let rows = ((avail.y / ch).floor() as u16).clamp(2, 500);
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            self.parser.screen_mut().set_size(NonZeroU16::new(rows).unwrap(), NonZeroU16::new(cols).unwrap());
            self.send(TermClient::Resize { cols, rows });
        }
        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
        let id = resp.id;
        let (srows, scols) = {
            let s = self.parser.screen().size();
            (s.0.get(), s.1.get())
        };
        let cell_at = |p: egui::Pos2| -> Cell {
            let c = ((p.x - rect.min.x) / cw).floor().clamp(0.0, f32::from(scols - 1)) as u16;
            let r = ((p.y - rect.min.y) / ch).floor().clamp(0.0, f32::from(srows - 1)) as u16;
            (r, c)
        };

        // Mouse: a drag selects, a click takes the keyboard (and drops the selection).
        if resp.drag_started_by(egui::PointerButton::Primary)
            && let Some(p) = resp.interact_pointer_pos()
        {
            let at = cell_at(p);
            self.selection = Some((at, at));
            resp.request_focus();
        } else if resp.dragged_by(egui::PointerButton::Primary)
            && let (Some(p), Some((a, _))) = (resp.interact_pointer_pos(), self.selection)
        {
            self.selection = Some((a, cell_at(p)));
        }
        if resp.clicked() {
            self.selection = None;
            resp.request_focus();
        }
        if std::mem::take(&mut self.want_focus) {
            resp.request_focus();
        }
        let has_selection = self.selection.is_some();
        let (mut copy, mut paste) = (false, false);
        resp.context_menu(|ui| {
            let copy_btn = egui::Button::new(t!("Copy", "Copiar")).shortcut_text("Ctrl+Shift+C");
            if ui.add_enabled(has_selection, copy_btn).clicked() {
                copy = true;
                ui.close();
            }
            if ui.add(egui::Button::new(t!("Paste", "Colar")).shortcut_text("Ctrl+V")).clicked() {
                paste = true;
                ui.close();
            }
        });
        if copy {
            self.copy_selection(ui.ctx());
        }
        if paste {
            self.paste_clipboard();
            resp.request_focus();
        }

        let focused = resp.has_focus();
        self.focused = focused;
        if focused {
            // Tab, arrows and Escape are the shell's, not egui's focus keys.
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
            self.keys(ui);
        }
        // Wheel: look back through the scrollback.
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if resp.hovered() && wheel != 0.0 {
            let screen = self.parser.screen_mut();
            let lines = (wheel / ch).round() as isize;
            let now = screen.scrollback() as isize;
            screen.set_scrollback((now + lines).max(0) as usize);
            if lines != 0 {
                self.selection = None;
            }
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BG);
        let screen = self.parser.screen();
        for row in 0..srows {
            let mut job = LayoutJob::default();
            for col in 0..scols {
                let Some(cell) = screen.cell(row, col) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let (mut fg, mut bg) = (color(cell.fgcolor(), FG), color(cell.bgcolor(), Color32::TRANSPARENT));
                if cell.inverse() {
                    let back = if bg == Color32::TRANSPARENT { BG } else { bg };
                    bg = if fg == Color32::TRANSPARENT { FG } else { fg };
                    fg = back;
                }
                let text = if cell.has_contents() { cell.contents() } else { " " };
                let format = egui::TextFormat {
                    font_id: self.font.clone(),
                    color: if cell.bold() && fg == FG { Color32::WHITE } else { fg },
                    background: bg,
                    underline: if cell.underline() { egui::Stroke::new(1.0, fg) } else { egui::Stroke::NONE },
                    ..Default::default()
                };
                job.append(text, 0.0, format);
            }
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            painter.galley(rect.min + egui::vec2(0.0, row as f32 * ch), galley, FG);
        }
        // The selection, over the text.
        if let Some(((sr, sc), (er, ec))) = self.selection_range() {
            let fill = crate::ui::tint(crate::ui::pal(ui).accent, 110);
            for row in sr..=er {
                let from = if row == sr { sc } else { 0 };
                let to = if row == er { ec } else { scols - 1 };
                if to < from {
                    continue;
                }
                let min = rect.min + egui::vec2(f32::from(from) * cw, f32::from(row) * ch);
                let r = egui::Rect::from_min_size(min, egui::vec2(f32::from(to - from + 1) * cw, ch));
                painter.rect_filled(r, 0.0, fill);
            }
        }
        // The cursor (on the live screen only).
        let scrolled = screen.scrollback() > 0;
        if !screen.hide_cursor() && !scrolled && !self.ended {
            let (cr, cc) = screen.cursor_position();
            let at = rect.min + egui::vec2(cc as f32 * cw, cr as f32 * ch);
            let r = egui::Rect::from_min_size(at, egui::vec2(cw, ch));
            if focused {
                painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(220, 220, 220, 140));
            } else {
                painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, FG), egui::StrokeKind::Inside);
            }
        }
        // Looking back: say how to return.
        if scrolled {
            history_pill(ui, &painter, rect);
        }
        if !focused && !self.ended {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn keys(&mut self, ui: &egui::Ui) {
        let app_cursor = self.parser.screen().application_cursor();
        let has_selection = self.selection.is_some();
        let mut out: Vec<u8> = Vec::new();
        let (mut copy, mut paste) = (false, false);
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    egui::Event::Text(t) => out.extend_from_slice(t.as_bytes()),
                    egui::Event::Paste(t) => out.extend(self.paste_bytes(t)),
                    // egui turns Ctrl+C / Ctrl+X into these. With text selected Ctrl+C copies
                    // it; otherwise the shell gets the control keys.
                    egui::Event::Copy if has_selection => copy = true,
                    egui::Event::Copy => out.push(0x03),
                    egui::Event::Cut => out.push(0x18),
                    // Ctrl+Shift+C / Ctrl+Shift+V: copy and paste, as in other terminals.
                    egui::Event::Key { key: Key::C, pressed: true, modifiers, .. }
                        if modifiers.ctrl && modifiers.shift =>
                    {
                        copy = true
                    }
                    egui::Event::Key { key: Key::V, pressed: true, modifiers, .. }
                        if modifiers.ctrl && modifiers.shift =>
                    {
                        paste = true
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        if let Some(b) = key_bytes(*key, *modifiers, app_cursor) {
                            if modifiers.alt && !modifiers.ctrl && b.len() == 1 {
                                out.push(0x1b);
                            }
                            out.extend(b);
                        }
                    }
                    _ => {}
                }
            }
        });
        if copy {
            self.copy_selection(ui.ctx());
            self.selection = None;
        }
        if paste {
            self.paste_clipboard();
        }
        self.input(out);
    }
}

/// "History — type to go back", in the top right corner of the terminal.
fn history_pill(ui: &egui::Ui, painter: &egui::Painter, rect: egui::Rect) {
    let p = crate::ui::pal(ui);
    let text = t!("History — type to go back", "Histórico — digite para voltar");
    let galley = painter.layout_no_wrap(text.into(), FontId::proportional(crate::ui::size::SMALL), p.text);
    let w = (galley.size().x + 20.0).min((rect.width() - 16.0).max(40.0));
    let h = galley.size().y + 8.0;
    let pill = egui::Rect::from_min_size(egui::pos2(rect.right() - 8.0 - w, rect.top() + 8.0), egui::vec2(w, h));
    painter.rect(
        pill,
        egui::CornerRadius::same(255),
        p.surface,
        egui::Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    painter.with_clip_rect(pill.shrink(4.0)).galley(pill.min + egui::vec2(10.0, 4.0), galley, p.text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let none = Modifiers::NONE;
        assert_eq!(key_bytes(Key::Enter, none, false), Some(b"\r".to_vec()));
        assert_eq!(key_bytes(Key::ArrowUp, none, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(Key::ArrowUp, none, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes(Key::C, Modifiers::CTRL, false), Some(vec![3]));
        assert_eq!(key_bytes(Key::ArrowLeft, Modifiers::CTRL, false), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(key_bytes(Key::Delete, none, false), Some(b"\x1b[3~".to_vec()));
        assert_eq!(key_bytes(Key::A, none, false), None, "letters come as text");
    }

    #[test]
    fn terminal_answers() {
        assert_eq!(answers(b"\x1b[6n", (4, 9)), b"\x1b[5;10R".to_vec());
        assert_eq!(answers(b"hello", (0, 0)), Vec::<u8>::new());
        assert_eq!(answers(b"\x1b[c\x1b[5n", (0, 0)), b"\x1b[0n\x1b[?1;2c".to_vec());
    }

    #[test]
    fn colors() {
        assert_eq!(indexed(1), Color32::from_rgb(205, 49, 49));
        assert_eq!(indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(indexed(255), Color32::from_rgb(238, 238, 238));
    }
}
