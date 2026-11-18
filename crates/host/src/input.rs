//! Input injection on a dedicated thread.
//!
//! * Windows: `SendInput` with scan codes (layout-independent) and absolute
//!   coordinates over the whole virtual desktop.
//! * macOS / Linux X11: `enigo` with raw key codes from [`crate::keymap`].
//! * `RDESK_FAKE_INPUT=1`: log only (tests, headless machines).
//!
//! The thread remembers which keys and buttons are held so they can all be
//! released when a session ends: a lost key-up must never leave Ctrl stuck.

use std::collections::BTreeSet;
use std::sync::mpsc;

use anyhow::Result;
use rdesk_proto::{InputEvent, Key, MouseButton};

use crate::capture::InputRect;

pub enum InputCmd {
    /// Map normalized coordinates onto this rectangle from now on.
    Monitor(InputRect),
    Event(InputEvent),
}

#[derive(Clone)]
pub struct Injector {
    tx: mpsc::Sender<InputCmd>,
}

impl Injector {
    pub fn spawn(fake: bool) -> Self {
        let (tx, rx) = mpsc::channel::<InputCmd>();
        std::thread::Builder::new()
            .name("rdesk-input".into())
            .spawn(move || {
                let mut backend = make_backend(fake);
                let mut state = State::default();
                while let Ok(cmd) = rx.recv() {
                    if let Err(e) = state.handle(backend.as_mut(), cmd) {
                        tracing::warn!("input injection failed: {e:#}");
                    }
                }
                state.release_all(backend.as_mut());
            })
            .expect("spawn input thread");
        Self { tx }
    }

    pub fn send(&self, cmd: InputCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn event(&self, ev: InputEvent) {
        self.send(InputCmd::Event(ev));
    }
}

fn make_backend(fake: bool) -> Box<dyn Backend> {
    if fake {
        return Box::new(LogBackend);
    }
    match platform_backend() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("input injection unavailable ({e:#}); remote input will be ignored");
            Box::new(LogBackend)
        }
    }
}

pub trait Backend {
    fn key(&mut self, key: Key, down: bool) -> Result<()>;
    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, b: MouseButton, down: bool) -> Result<()>;
    /// Whole notches; positive = up / right.
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn text(&mut self, s: &str) -> Result<()>;
}

#[derive(Default)]
struct State {
    rect: Option<InputRect>,
    keys: BTreeSet<Key>,
    buttons: Vec<MouseButton>,
    scroll_acc: (f32, f32),
}

impl State {
    fn handle(&mut self, b: &mut dyn Backend, cmd: InputCmd) -> Result<()> {
        match cmd {
            InputCmd::Monitor(r) => self.rect = Some(r),
            InputCmd::Event(ev) => match ev {
                InputEvent::MouseMove { x, y } => {
                    if let Some(r) = self.rect {
                        let (px, py) = map_point(r, x, y);
                        b.mouse_abs(px, py)?;
                    }
                }
                InputEvent::MouseButton { button, down } => {
                    if down {
                        if !self.buttons.contains(&button) {
                            self.buttons.push(button);
                        }
                    } else {
                        self.buttons.retain(|&x| x != button);
                    }
                    b.button(button, down)?;
                }
                InputEvent::Scroll { dx, dy } => {
                    self.scroll_acc.0 += dx.clamp(-50.0, 50.0);
                    self.scroll_acc.1 += dy.clamp(-50.0, 50.0);
                    let (ix, iy) = (self.scroll_acc.0.trunc(), self.scroll_acc.1.trunc());
                    if ix != 0.0 || iy != 0.0 {
                        self.scroll_acc.0 -= ix;
                        self.scroll_acc.1 -= iy;
                        b.scroll(ix as i32, iy as i32)?;
                    }
                }
                InputEvent::Key { key, down } => {
                    if down {
                        self.keys.insert(key);
                    } else if !self.keys.remove(&key) {
                        // Releasing a key we never pressed is harmless but pointless.
                        return Ok(());
                    }
                    b.key(key, down)?;
                }
                InputEvent::Text { text } => b.text(&text)?,
                InputEvent::ReleaseAll => self.release_all(b),
            },
        }
        Ok(())
    }

    fn release_all(&mut self, b: &mut dyn Backend) {
        // Release non-modifiers first so modifiers don't trigger shortcuts on the way up.
        let mut keys: Vec<Key> = std::mem::take(&mut self.keys).into_iter().collect();
        keys.sort_by_key(|k| k.is_modifier());
        for k in keys {
            let _ = b.key(k, false);
        }
        for btn in std::mem::take(&mut self.buttons) {
            let _ = b.button(btn, false);
        }
        self.scroll_acc = (0.0, 0.0);
    }
}

/// Normalized 0..=65535 → absolute coordinates inside `r`.
pub fn map_point(r: InputRect, x: u16, y: u16) -> (i32, i32) {
    let fx = x as f64 / 65535.0;
    let fy = y as f64 / 65535.0;
    let px = r.x + (fx * (r.w.saturating_sub(1)) as f64).round() as i32;
    let py = r.y + (fy * (r.h.saturating_sub(1)) as f64).round() as i32;
    (px, py)
}

struct LogBackend;

impl Backend for LogBackend {
    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        tracing::debug!("fake input: key {key:?} down={down}");
        Ok(())
    }
    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
        tracing::trace!("fake input: mouse {x},{y}");
        Ok(())
    }
    fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
        tracing::debug!("fake input: button {b:?} down={down}");
        Ok(())
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        tracing::debug!("fake input: scroll {dx},{dy}");
        Ok(())
    }
    fn text(&mut self, s: &str) -> Result<()> {
        tracing::debug!("fake input: text {} chars", s.chars().count());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn platform_backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(win::WinBackend))
}

#[cfg(windows)]
mod win {
    use super::*;
    use crate::keymap::{self, WinKey};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    pub struct WinBackend;

    fn send(inputs: &[INPUT]) -> Result<()> {
        let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
        if n as usize != inputs.len() {
            // Typically UIPI: the foreground window is elevated and we are not.
            anyhow::bail!("SendInput injected {n}/{} events (is an elevated window focused?)", inputs.len());
        }
        Ok(())
    }

    fn kbd(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        }
    }

    fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        }
    }

    impl Backend for WinBackend {
        fn key(&mut self, key: Key, down: bool) -> Result<()> {
            let up = if down { 0 } else { KEYEVENTF_KEYUP };
            match keymap::windows(key) {
                Some(WinKey::Scan { code, ext }) => {
                    let flags = KEYEVENTF_SCANCODE | up | if ext { KEYEVENTF_EXTENDEDKEY } else { 0 };
                    send(&[kbd(0, code, flags)])
                }
                Some(WinKey::Vk(vk)) => send(&[kbd(vk, 0, up)]),
                None => Ok(()),
            }
        }

        fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
            let (vx, vy, vw, vh) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            let nx = (((x - vx) as i64 * 65535) / (vw.max(2) - 1) as i64) as i32;
            let ny = (((y - vy) as i64 * 65535) / (vh.max(2) - 1) as i64) as i32;
            send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)])
        }

        fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
            let (flags, data) = match (b, down) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, 1),
                (MouseButton::Back, false) => (MOUSEEVENTF_XUP, 1),
                (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
                (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, 2),
            };
            send(&[mouse(0, 0, data, flags)])
        }

        fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
            const WHEEL_DELTA: i32 = 120;
            let mut v = Vec::new();
            if dy != 0 {
                v.push(mouse(0, 0, dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
            }
            if dx != 0 {
                v.push(mouse(0, 0, dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
            }
            send(&v)
        }

        fn text(&mut self, s: &str) -> Result<()> {
            let mut v = Vec::new();
            for unit in s.encode_utf16() {
                v.push(kbd(0, unit, KEYEVENTF_UNICODE));
                v.push(kbd(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
            send(&v)
        }
    }
}

// ---------------------------------------------------------------------------
// macOS / Linux (enigo)
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
fn platform_backend() -> Result<Box<dyn Backend>> {
    let enigo = enigo::Enigo::new(&enigo::Settings::default())
        .map_err(|e| anyhow::anyhow!("{e:?} (macOS: grant Accessibility; Linux: needs an X11 session)"))?;
    Ok(Box::new(EnigoBackend { enigo }))
}

#[cfg(not(windows))]
struct EnigoBackend {
    enigo: enigo::Enigo,
}

#[cfg(not(windows))]
impl Backend for EnigoBackend {
    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        use enigo::{Direction, Keyboard};
        #[cfg(target_os = "macos")]
        let code = crate::keymap::macos(key);
        #[cfg(not(target_os = "macos"))]
        let code = crate::keymap::evdev(key).map(|c| c + 8);
        let Some(code) = code else { return Ok(()) };
        let dir = if down { Direction::Press } else { Direction::Release };
        self.enigo.raw(code, dir).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
        use enigo::{Coordinate, Mouse};
        self.enigo.move_mouse(x, y, Coordinate::Abs).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
        use enigo::{Button, Direction, Mouse};
        let btn = match b {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
            MouseButton::Middle => Button::Middle,
            MouseButton::Back => Button::Back,
            MouseButton::Forward => Button::Forward,
        };
        let dir = if down { Direction::Press } else { Direction::Release };
        self.enigo.button(btn, dir).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        use enigo::{Axis, Mouse};
        // enigo: positive = down / right.
        if dy != 0 {
            self.enigo.scroll(-dy, Axis::Vertical).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        if dx != 0 {
            self.enigo.scroll(dx, Axis::Horizontal).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        Ok(())
    }

    fn text(&mut self, s: &str) -> Result<()> {
        use enigo::Keyboard;
        self.enigo.text(s).map_err(|e| anyhow::anyhow!("{e:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct Rec(Arc<Mutex<Vec<String>>>);

    impl Backend for Rec {
        fn key(&mut self, key: Key, down: bool) -> Result<()> {
            self.0.lock().unwrap().push(format!("key {key:?} {down}"));
            Ok(())
        }
        fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
            self.0.lock().unwrap().push(format!("move {x} {y}"));
            Ok(())
        }
        fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
            self.0.lock().unwrap().push(format!("btn {b:?} {down}"));
            Ok(())
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.0.lock().unwrap().push(format!("scroll {dx} {dy}"));
            Ok(())
        }
        fn text(&mut self, s: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("text {s}"));
            Ok(())
        }
    }

    #[test]
    fn maps_corners_onto_monitor_rect() {
        let r = InputRect { x: -1920, y: 0, w: 1920, h: 1080 };
        assert_eq!(map_point(r, 0, 0), (-1920, 0));
        assert_eq!(map_point(r, 65535, 65535), (-1, 1079));
        assert_eq!(map_point(r, 32768, 32768), (-960, 540));
    }

    #[test]
    fn release_all_lifts_everything_modifiers_last() {
        let rec = Rec::default();
        let mut b = rec.clone();
        let mut s = State::default();
        for ev in [
            InputEvent::Key { key: Key::ControlLeft, down: true },
            InputEvent::Key { key: Key::KeyC, down: true },
            InputEvent::MouseButton { button: MouseButton::Left, down: true },
            InputEvent::ReleaseAll,
        ] {
            s.handle(&mut b, InputCmd::Event(ev)).unwrap();
        }
        let log = rec.0.lock().unwrap().clone();
        assert_eq!(log[3..], ["key KeyC false", "key ControlLeft false", "btn Left false"].map(String::from));
        assert!(s.keys.is_empty() && s.buttons.is_empty());
    }

    #[test]
    fn stray_key_up_is_ignored_and_scroll_accumulates() {
        let rec = Rec::default();
        let mut b = rec.clone();
        let mut s = State::default();
        s.handle(&mut b, InputCmd::Event(InputEvent::Key { key: Key::KeyA, down: false })).unwrap();
        for _ in 0..3 {
            s.handle(&mut b, InputCmd::Event(InputEvent::Scroll { dx: 0.0, dy: 0.4 })).unwrap();
        }
        assert_eq!(rec.0.lock().unwrap().clone(), vec!["scroll 0 1".to_string()]);
    }
}
