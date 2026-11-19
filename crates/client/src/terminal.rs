//! The remote terminal on the client: the stream to the host's shell ([`open`]) and an
//! egui widget that draws it ([`TermView`]: a VT screen kept by `vt100`) and turns keys
//! into what a terminal sends.

use std::num::NonZeroU16;

use anyhow::Result;
use egui::{Color32, FontId, Key, Modifiers, text::LayoutJob};
use rdesk_common::transport::Connection;
use rdesk_proto::{MAX_SMALL_MSG, StreamOpen, TermClient, TermHost, read_msg, write_msg};
use tokio::sync::mpsc;

/// One open terminal: keys and sizes go out on `tx`, the host's output comes in on `rx`.
pub struct TerminalLink {
    pub tx: mpsc::UnboundedSender<TermClient>,
    pub rx: mpsc::UnboundedReceiver<TermHost>,
}

/// Input is sent in pieces the host accepts (a big paste becomes several messages).
const INPUT_CHUNK: usize = 32 * 1024;

/// A [`TerminalLink`] handed from the connection to the window (events must be `Clone`).
#[derive(Clone)]
pub struct TermSlot(pub std::sync::Arc<std::sync::Mutex<Option<TerminalLink>>>);

impl std::fmt::Debug for TermSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TermSlot")
    }
}

/// Open a terminal on an authenticated connection; `wake` runs when output arrives.
pub async fn open(conn: Connection, cols: u16, rows: u16, wake: impl Fn() + Send + 'static) -> Result<TerminalLink> {
    let (mut w, mut r) = conn.open_bi().await?;
    write_msg(&mut w, &StreamOpen::Terminal).await?;
    write_msg(&mut w, &TermClient::Start { cols, rows }).await?;
    let (tx, mut out) = mpsc::unbounded_channel::<TermClient>();
    let (inc, rx) = mpsc::unbounded_channel::<TermHost>();
    tokio::spawn(async move {
        while let Some(m) = out.recv().await {
            let parts = match m {
                TermClient::Input(bytes) if bytes.len() > INPUT_CHUNK => {
                    bytes.chunks(INPUT_CHUNK).map(|c| TermClient::Input(c.to_vec())).collect()
                }
                m => vec![m],
            };
            for p in parts {
                if write_msg(&mut w, &p).await.is_err() {
                    return;
                }
            }
        }
        let _ = w.finish();
    });
    tokio::spawn(async move {
        while let Ok(Some(m)) = read_msg::<_, TermHost>(&mut r, MAX_SMALL_MSG).await {
            if inc.send(m).is_err() {
                return;
            }
            wake();
        }
        wake();
    });
    Ok(TerminalLink { tx, rx })
}

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

/// What a terminal answers to the questions programs print: where the cursor is
/// (`ESC[6n`, which Windows' ConPTY asks before anything else), its status (`ESC[5n`) and
/// what kind of terminal it is (`ESC[c`).
pub fn answers(output: &[u8], cursor: (u16, u16)) -> Vec<u8> {
    let mut out = Vec::new();
    let count = |q: &[u8]| output.windows(q.len()).filter(|w| *w == q).count();
    for _ in 0..count(b"\x1b[6n") {
        out.extend_from_slice(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes());
    }
    for _ in 0..count(b"\x1b[5n") {
        out.extend_from_slice(b"\x1b[0n");
    }
    for _ in 0..(count(b"\x1b[c") + count(b"\x1b[0c")) {
        out.extend_from_slice(b"\x1b[?1;2c");
    }
    out
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

/// A terminal window's contents and state.
pub struct TermView {
    parser: vt100::Parser,
    link: Option<TerminalLink>,
    /// "PowerShell as ana", "Exited (code 0)", an error…
    pub status: String,
    pub ended: bool,
    size: (u16, u16),
    font: FontId,
}

impl TermView {
    pub fn new(link: TerminalLink, cols: u16, rows: u16) -> Self {
        let nz = |v: u16| NonZeroU16::new(v.max(2)).unwrap();
        Self {
            parser: vt100::Parser::new(nz(rows), nz(cols), 5000),
            link: Some(link),
            status: "Starting…".into(),
            ended: false,
            size: (cols, rows),
            font: FontId::monospace(14.0),
        }
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
                    self.status = if user.is_empty() { shell } else { format!("{shell} as {user}") };
                    changed = true;
                }
                Ok(TermHost::Exited(code)) => {
                    self.status = format!("Ended (code {code})");
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
                        self.status = "Disconnected".into();
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
        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click());
        let id = resp.id;
        if resp.clicked() {
            resp.request_focus();
        }
        let focused = resp.has_focus();
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
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BG);
        let screen = self.parser.screen();
        let (srows, scols) = (screen.size().0.get(), screen.size().1.get());
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
        // The cursor (on the live screen only).
        if !screen.hide_cursor() && screen.scrollback() == 0 && !self.ended {
            let (cr, cc) = screen.cursor_position();
            let at = rect.min + egui::vec2(cc as f32 * cw, cr as f32 * ch);
            let r = egui::Rect::from_min_size(at, egui::vec2(cw, ch));
            if focused {
                painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(220, 220, 220, 140));
            } else {
                painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, FG), egui::StrokeKind::Inside);
            }
        }
        if !focused && !self.ended {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn keys(&mut self, ui: &egui::Ui) {
        let app_cursor = self.parser.screen().application_cursor();
        let bracketed = self.parser.screen().bracketed_paste();
        let mut out: Vec<u8> = Vec::new();
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    egui::Event::Text(t) => out.extend_from_slice(t.as_bytes()),
                    egui::Event::Paste(t) => {
                        if bracketed {
                            out.extend_from_slice(b"\x1b[200~");
                        }
                        out.extend_from_slice(t.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                        if bracketed {
                            out.extend_from_slice(b"\x1b[201~");
                        }
                    }
                    // egui turns Ctrl+C / Ctrl+X into these: the shell gets the control keys.
                    egui::Event::Copy => out.push(0x03),
                    egui::Event::Cut => out.push(0x18),
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
        if !out.is_empty() {
            // Typing goes back to the live screen.
            self.parser.screen_mut().set_scrollback(0);
            self.send(TermClient::Input(out));
        }
    }
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
