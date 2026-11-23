//! winit physical key → protocol key.

use egui_winit::winit::keyboard::KeyCode;
use rdesk_proto::Key;

macro_rules! same {
    ($code:expr; $($k:ident),* $(,)?) => {
        match $code {
            $(KeyCode::$k => Some(Key::$k),)*
            KeyCode::SuperLeft => Some(Key::MetaLeft),
            KeyCode::SuperRight => Some(Key::MetaRight),
            KeyCode::Meta => Some(Key::MetaLeft),
            _ => None,
        }
    };
}

pub fn map(code: KeyCode) -> Option<Key> {
    same!(code;
        Backquote, Backslash, BracketLeft, BracketRight, Comma,
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
        Equal, IntlBackslash, IntlRo, IntlYen,
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM,
        KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
        Minus, Period, Quote, Semicolon, Slash,
        AltLeft, AltRight, Backspace, CapsLock, ContextMenu, ControlLeft, ControlRight,
        Enter, ShiftLeft, ShiftRight, Space, Tab,
        Convert, KanaMode, NonConvert, Lang1, Lang2,
        Delete, End, Home, Insert, PageDown, PageUp,
        ArrowDown, ArrowLeft, ArrowRight, ArrowUp,
        NumLock, Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7,
        Numpad8, Numpad9, NumpadAdd, NumpadComma, NumpadDecimal, NumpadDivide, NumpadEnter,
        NumpadEqual, NumpadMultiply, NumpadSubtract,
        Escape, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
        F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, F23, F24,
        PrintScreen, ScrollLock, Pause,
        AudioVolumeDown, AudioVolumeMute, AudioVolumeUp,
        MediaPlayPause, MediaStop, MediaTrackNext, MediaTrackPrevious,
    )
}

/// Mac keyboard → PC host, so shortcuts feel native: Cmd is Ctrl, the left Option is
/// the Windows key and Control is Alt. The right Option stays AltGr (accents and
/// symbols on international layouts).
pub fn swap_cmd_ctrl(k: Key) -> Key {
    match k {
        Key::MetaLeft => Key::ControlLeft,
        Key::MetaRight => Key::ControlRight,
        Key::AltLeft => Key::MetaLeft,
        Key::ControlLeft => Key::AltLeft,
        Key::ControlRight => Key::AltLeft,
        other => other,
    }
}

/// The lock keys on this keyboard, where the system says (Windows, macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locks {
    pub caps: bool,
    /// `None` on a Mac: no Num Lock there (the keypad always types digits).
    pub num: Option<bool>,
}

/// A Mac's Caps Lock never reaches the window as a key (winit reports no event for it), so
/// the viewer looks at the lock keys and tells the host instead.
pub fn locks() -> Option<Locks> {
    Some(Locks { caps: caps_lock()?, num: num_lock() })
}

fn num_lock() -> Option<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_NUMLOCK};
        // SAFETY: no pointers; the low bit is the toggle.
        Some(unsafe { GetKeyState(VK_NUMLOCK as i32) } & 1 != 0)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn caps_lock() -> Option<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};
        // SAFETY: no pointers; the low bit is the toggle.
        Some(unsafe { GetKeyState(VK_CAPITAL as i32) } & 1 != 0)
    }
    #[cfg(target_os = "macos")]
    {
        // CGEventSourceStateID kCGEventSourceStateHIDSystemState, CGEventFlags kCGEventFlagMaskAlphaShift.
        const HID_SYSTEM_STATE: i32 = 1;
        const ALPHA_SHIFT: u64 = 0x0001_0000;
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventSourceFlagsState(state_id: i32) -> u64;
        }
        // SAFETY: a plain query with a valid state id.
        Some(unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) } & ALPHA_SHIFT != 0)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_meta() {
        assert_eq!(map(KeyCode::KeyA), Some(Key::KeyA));
        assert_eq!(map(KeyCode::SuperLeft), Some(Key::MetaLeft));
        assert_eq!(map(KeyCode::IntlRo), Some(Key::IntlRo));
    }

    #[test]
    fn mac_modifiers() {
        assert_eq!(swap_cmd_ctrl(Key::MetaLeft), Key::ControlLeft);
        assert_eq!(swap_cmd_ctrl(Key::MetaRight), Key::ControlRight);
        assert_eq!(swap_cmd_ctrl(Key::AltLeft), Key::MetaLeft, "left Option is the Windows key");
        assert_eq!(swap_cmd_ctrl(Key::ControlLeft), Key::AltLeft);
        assert_eq!(swap_cmd_ctrl(Key::AltRight), Key::AltRight, "right Option stays AltGr");
        assert_eq!(swap_cmd_ctrl(Key::KeyC), Key::KeyC);
    }
}
