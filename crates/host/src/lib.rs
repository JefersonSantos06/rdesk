//! rdesk host: everything that runs on the machine being controlled.

pub mod capture;
pub mod input;
pub mod keymap;
pub mod power;
pub mod server;
pub mod session;
pub mod video;

pub use server::{RunOpts, run};
