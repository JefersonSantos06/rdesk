//! Remote control of the phone: the host's mouse and keyboard become touches, system actions
//! and typed text, which the app's accessibility service (`RemoteControlService.kt`) carries
//! out. It takes them from here one by one through JNI (`InputBridge.next`).
//!
//! The commands are lines of text (coordinates in the shared picture's pixels; the service
//! scales them to the display):
//! - `down x y`, `move x y`, `up`: a finger touching, sliding, lifting (a held button is a
//!   held finger, so a long press and a drag happen live);
//! - `scroll x y dx dy`: wheel notches at a point (a swipe; positive = up / right);
//! - `back`, `home`, `recents`, `screenshot`: system actions;
//! - `volume up|down|mute`;
//! - `text …`: typed into the focused field;
//! - `key backspace|delete|enter|left|right|home|end|select_all|copy|cut|paste`.
//!
//! Mouse and keys: the left button touches; the right button and Esc go back; the middle button
//! and the Windows key go home; the menu key shows the recent apps. Keys arrive as physical
//! keys (no layout), so letters and symbols are read as on a US keyboard.

// Elsewhere nothing reads it (only the JNI calls below do), but it builds and is tested.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::Result;
use rdesk_host::input::Backend;
use rdesk_proto::{Key, MouseButton};

/// Commands waiting for the service; the oldest go first when it falls behind.
const QUEUE_MAX: usize = 512;

struct Queue {
    items: VecDeque<String>,
    /// The accessibility service is on and reading.
    connected: bool,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue { items: VecDeque::new(), connected: false });
static READY: Condvar = Condvar::new();

/// The service started or stopped reading.
pub fn set_connected(on: bool) {
    let mut q = QUEUE.lock().unwrap();
    q.connected = on;
    q.items.clear();
    READY.notify_all();
}

pub fn connected() -> bool {
    QUEUE.lock().unwrap().connected
}

/// The next command, waiting up to `timeout`.
pub fn next(timeout: Duration) -> Option<String> {
    let mut q = QUEUE.lock().unwrap();
    if q.items.is_empty() {
        q = READY.wait_timeout(q, timeout).unwrap().0;
    }
    q.items.pop_front()
}

/// Queue `cmd`; a `move` or `scroll` right after another of its kind replaces or adds to it, so a
/// slow service gets the newest position instead of a backlog.
fn push(cmd: String) {
    let mut q = QUEUE.lock().unwrap();
    if !q.connected {
        return;
    }
    if let Some(last) = q.items.back_mut() {
        if cmd.starts_with("move ") && last.starts_with("move ") {
            *last = cmd;
            return;
        }
        if let (Some(a), Some(b)) = (scroll_of(last), scroll_of(&cmd))
            && (a.0, a.1) == (b.0, b.1)
        {
            *last = format!("scroll {} {} {} {}", a.0, a.1, a.2 + b.2, a.3 + b.3);
            return;
        }
    }
    if q.items.len() >= QUEUE_MAX {
        q.items.pop_front();
    }
    q.items.push_back(cmd);
    READY.notify_all();
}

fn scroll_of(cmd: &str) -> Option<(i32, i32, i32, i32)> {
    let mut it = cmd.strip_prefix("scroll ")?.split(' ').map(|v| v.parse::<i32>().ok());
    Some((it.next()??, it.next()??, it.next()??, it.next()??))
}

fn off() -> anyhow::Error {
    anyhow::anyhow!(rdesk_common::t!(
        "touch control is off on the phone: turn it on in rdesk, This phone",
        "o controle por toque está desligado no celular: ligue no rdesk, em Este celular"
    ))
}

/// The host's input backend on the phone.
#[derive(Default)]
pub struct PhoneInput {
    pointer: (i32, i32),
    touching: bool,
    shift: bool,
    ctrl: bool,
    caps: bool,
}

impl PhoneInput {
    /// A press that needs the service: refused (and reported to the other side) without it.
    fn send(&self, cmd: impl Into<String>) -> Result<()> {
        if !connected() {
            return Err(off());
        }
        push(cmd.into());
        Ok(())
    }
}

impl Backend for PhoneInput {
    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        match key {
            Key::ShiftLeft | Key::ShiftRight => self.shift = down,
            Key::ControlLeft | Key::ControlRight => self.ctrl = down,
            _ => {}
        }
        if !down {
            return Ok(());
        }
        if key == Key::CapsLock {
            self.caps = !self.caps;
            return Ok(());
        }
        let cmd = match key {
            Key::Escape => "back",
            Key::MetaLeft | Key::MetaRight => "home",
            Key::ContextMenu => "recents",
            Key::PrintScreen => "screenshot",
            Key::AudioVolumeUp => "volume up",
            Key::AudioVolumeDown => "volume down",
            Key::AudioVolumeMute => "volume mute",
            Key::Backspace => "key backspace",
            Key::Delete => "key delete",
            Key::Enter | Key::NumpadEnter => "key enter",
            Key::ArrowLeft => "key left",
            Key::ArrowRight => "key right",
            Key::Home => "key home",
            Key::End => "key end",
            Key::KeyA if self.ctrl => "key select_all",
            Key::KeyC if self.ctrl => "key copy",
            Key::KeyX if self.ctrl => "key cut",
            Key::KeyV if self.ctrl => "key paste",
            _ if self.ctrl => return Ok(()),
            _ => match us_char(key, self.shift, self.caps) {
                Some(c) => return self.send(format!("text {c}")),
                None => return Ok(()),
            },
        };
        self.send(cmd)
    }

    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
        self.pointer = (x, y);
        if self.touching {
            push(format!("move {x} {y}"));
        }
        Ok(())
    }

    fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
        let (x, y) = self.pointer;
        match (b, down) {
            (MouseButton::Left, true) => {
                self.send(format!("down {x} {y}"))?;
                self.touching = true;
                Ok(())
            }
            (MouseButton::Left, false) => {
                if std::mem::take(&mut self.touching) {
                    push("up".into());
                }
                Ok(())
            }
            (MouseButton::Right | MouseButton::Back, true) => self.send("back"),
            (MouseButton::Middle, true) => self.send("home"),
            (MouseButton::Forward, true) => self.send("recents"),
            _ => Ok(()),
        }
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        if self.touching {
            return Ok(());
        }
        let (x, y) = self.pointer;
        self.send(format!("scroll {x} {y} {dx} {dy}"))
    }

    fn text(&mut self, s: &str) -> Result<()> {
        if s.is_empty() {
            return Ok(());
        }
        self.send(format!("text {s}"))
    }

    fn pointer(&mut self) -> Option<(i32, i32)> {
        Some(self.pointer)
    }
}

/// What `key` types on a US keyboard.
fn us_char(key: Key, shift: bool, caps: bool) -> Option<char> {
    let pick = |plain: char, shifted: char| Some(if shift { shifted } else { plain });
    let letter = |c: char| Some(if shift != caps { c.to_ascii_uppercase() } else { c });
    match key {
        Key::KeyA => letter('a'),
        Key::KeyB => letter('b'),
        Key::KeyC => letter('c'),
        Key::KeyD => letter('d'),
        Key::KeyE => letter('e'),
        Key::KeyF => letter('f'),
        Key::KeyG => letter('g'),
        Key::KeyH => letter('h'),
        Key::KeyI => letter('i'),
        Key::KeyJ => letter('j'),
        Key::KeyK => letter('k'),
        Key::KeyL => letter('l'),
        Key::KeyM => letter('m'),
        Key::KeyN => letter('n'),
        Key::KeyO => letter('o'),
        Key::KeyP => letter('p'),
        Key::KeyQ => letter('q'),
        Key::KeyR => letter('r'),
        Key::KeyS => letter('s'),
        Key::KeyT => letter('t'),
        Key::KeyU => letter('u'),
        Key::KeyV => letter('v'),
        Key::KeyW => letter('w'),
        Key::KeyX => letter('x'),
        Key::KeyY => letter('y'),
        Key::KeyZ => letter('z'),
        Key::Digit1 => pick('1', '!'),
        Key::Digit2 => pick('2', '@'),
        Key::Digit3 => pick('3', '#'),
        Key::Digit4 => pick('4', '$'),
        Key::Digit5 => pick('5', '%'),
        Key::Digit6 => pick('6', '^'),
        Key::Digit7 => pick('7', '&'),
        Key::Digit8 => pick('8', '*'),
        Key::Digit9 => pick('9', '('),
        Key::Digit0 => pick('0', ')'),
        Key::Minus => pick('-', '_'),
        Key::Equal => pick('=', '+'),
        Key::BracketLeft => pick('[', '{'),
        Key::BracketRight => pick(']', '}'),
        Key::Backslash | Key::IntlBackslash => pick('\\', '|'),
        Key::Semicolon => pick(';', ':'),
        Key::Quote => pick('\'', '"'),
        Key::Backquote => pick('`', '~'),
        Key::Comma => pick(',', '<'),
        Key::Period => pick('.', '>'),
        Key::Slash | Key::IntlRo => pick('/', '?'),
        Key::Space => Some(' '),
        Key::Numpad0 => Some('0'),
        Key::Numpad1 => Some('1'),
        Key::Numpad2 => Some('2'),
        Key::Numpad3 => Some('3'),
        Key::Numpad4 => Some('4'),
        Key::Numpad5 => Some('5'),
        Key::Numpad6 => Some('6'),
        Key::Numpad7 => Some('7'),
        Key::Numpad8 => Some('8'),
        Key::Numpad9 => Some('9'),
        Key::NumpadAdd => Some('+'),
        Key::NumpadSubtract => Some('-'),
        Key::NumpadMultiply => Some('*'),
        Key::NumpadDivide => Some('/'),
        Key::NumpadDecimal | Key::NumpadComma => Some('.'),
        Key::NumpadEqual => Some('='),
        _ => None,
    }
}

/// What the app's Kotlin calls (`dev.rdesk.rdesk.InputBridge`).
#[cfg(target_os = "android")]
mod jni_exports {
    use std::time::Duration;

    use jni::JNIEnv;
    use jni::objects::JClass;
    use jni::sys::{jboolean, jint, jstring};

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_InputBridge_connected(_: JNIEnv, _: JClass, on: jboolean) {
        super::set_connected(on != 0);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_InputBridge_next(env: JNIEnv, _: JClass, timeout_ms: jint) -> jstring {
        match super::next(Duration::from_millis(timeout_ms.max(0) as u64)) {
            Some(cmd) => env.new_string(cmd).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut()),
            None => std::ptr::null_mut(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain() -> Vec<String> {
        std::iter::from_fn(|| next(Duration::ZERO)).collect()
    }

    // One test: the queue is global.
    #[test]
    fn mouse_and_keys_become_touches_actions_and_text() {
        let mut p = PhoneInput::default();
        // Without the service: presses are refused, nothing is queued.
        assert!(p.button(MouseButton::Left, true).is_err());
        assert!(p.key(Key::KeyA, true).is_err());
        set_connected(true);

        // A drag: the moves in between collapse into the newest one.
        p.mouse_abs(10, 20).unwrap();
        p.button(MouseButton::Left, true).unwrap();
        p.mouse_abs(11, 21).unwrap();
        p.mouse_abs(12, 22).unwrap();
        p.button(MouseButton::Left, false).unwrap();
        // Moves without a touch: only the pointer.
        p.mouse_abs(30, 40).unwrap();
        assert_eq!(drain(), ["down 10 20", "move 12 22", "up"]);

        // Wheel notches at one point add up; buttons are system actions.
        p.scroll(0, 1).unwrap();
        p.scroll(0, 2).unwrap();
        p.button(MouseButton::Right, true).unwrap();
        p.button(MouseButton::Right, false).unwrap();
        p.button(MouseButton::Middle, true).unwrap();
        assert_eq!(drain(), ["scroll 30 40 0 3", "back", "home"]);

        // Typing: Shift and Caps Lock, Ctrl shortcuts, editing keys, Esc.
        for (k, down) in [
            (Key::ShiftLeft, true),
            (Key::KeyH, true),
            (Key::ShiftLeft, false),
            (Key::KeyI, true),
            (Key::Digit1, true),
            (Key::CapsLock, true),
            (Key::KeyX, true),
            (Key::ControlLeft, true),
            (Key::KeyV, true),
            (Key::ControlLeft, false),
            (Key::Backspace, true),
            (Key::Escape, true),
            (Key::F5, true),
        ] {
            p.key(k, down).unwrap();
        }
        p.text("olá").unwrap();
        assert_eq!(drain(), ["text H", "text i", "text 1", "text X", "key paste", "key backspace", "back", "text olá"]);

        set_connected(false);
        assert!(p.scroll(0, 1).is_err());
        assert!(drain().is_empty());
    }
}
