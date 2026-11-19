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
