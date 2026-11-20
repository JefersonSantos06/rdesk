//! rdesk client: connection manager, viewer window and headless mode.

pub mod app;
pub mod headless;
pub mod keys;
pub mod terminal;

pub use rdesk_core::{conn, secrets};
pub mod ui;
pub mod viewer;
