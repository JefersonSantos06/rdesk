//! rdesk client core: everything a client app needs except its window. The desktop app
//! (egui) and the phone app (Flutter, through `rdesk-mobile`) both build on it.

pub mod conn;
pub mod secrets;
pub mod terminal;
