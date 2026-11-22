//! rdesk client: connection manager, viewer window and headless mode.

pub mod app;
pub mod audio;
pub mod headless;
pub mod keys;
pub mod terminal;

pub use rdesk_core::{conn, secrets, tunnel};
pub mod ui;
pub mod viewer;
