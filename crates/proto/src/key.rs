//! Physical, layout-independent key codes (W3C UI Events `code` values).
//!
//! The client sends *where* a key is on the keyboard, not what character it
//! produces; the host's own keyboard layout then decides the character. This is
//! how RDP/AnyDesk behave and it makes AltGr, dead keys and ABNT2 work.

use serde::{Deserialize, Serialize};

macro_rules! keys {
    ($($k:ident),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum Key { $($k),* }

        impl Key {
            /// Every key, in declaration order (used for exhaustive keymap tests).
            pub const ALL: &'static [Key] = &[$(Key::$k),*];

            pub fn name(self) -> &'static str {
                match self { $(Key::$k => stringify!($k)),* }
            }
        }
    };
}

keys! {
    // Writing system keys
    Backquote, Backslash, BracketLeft, BracketRight, Comma,
    Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    Equal, IntlBackslash, IntlRo, IntlYen,
    KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM,
    KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    Minus, Period, Quote, Semicolon, Slash,
    // Functional keys
    AltLeft, AltRight, Backspace, CapsLock, ContextMenu, ControlLeft, ControlRight,
    Enter, MetaLeft, MetaRight, ShiftLeft, ShiftRight, Space, Tab,
    Convert, KanaMode, NonConvert, Lang1, Lang2,
    // Control pad
    Delete, End, Home, Insert, PageDown, PageUp,
    // Arrows
    ArrowDown, ArrowLeft, ArrowRight, ArrowUp,
    // Numpad
    NumLock, Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7,
    Numpad8, Numpad9, NumpadAdd, NumpadComma, NumpadDecimal, NumpadDivide, NumpadEnter,
    NumpadEqual, NumpadMultiply, NumpadSubtract,
    // Function section
    Escape, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, F23, F24,
    PrintScreen, ScrollLock, Pause,
    // Media
    AudioVolumeDown, AudioVolumeMute, AudioVolumeUp,
    MediaPlayPause, MediaStop, MediaTrackNext, MediaTrackPrevious,
}

impl Key {
    pub fn is_modifier(self) -> bool {
        matches!(
            self,
            Key::AltLeft
                | Key::AltRight
                | Key::ControlLeft
                | Key::ControlRight
                | Key::MetaLeft
                | Key::MetaRight
                | Key::ShiftLeft
                | Key::ShiftRight
        )
    }
}
