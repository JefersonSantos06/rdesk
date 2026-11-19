//! rdesk host: everything that runs on the machine being controlled.

pub mod admin;
pub mod capture;
pub mod control;
pub mod elevation;
pub mod enroll;
pub mod input;
pub mod ipc;
pub mod keymap;
pub mod localnet;
pub mod power;
pub mod server;
pub mod session;
pub mod sync;
pub mod terminal;
pub mod update;
pub mod video;
#[cfg(windows)]
pub mod win;

pub use control::{HostHandle, HostState, HostStatus, Reporter};
pub use server::{RunOpts, run};
