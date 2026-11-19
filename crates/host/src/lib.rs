//! rdesk host: everything that runs on the machine being controlled.

pub mod admin;
pub mod capture;
pub mod control;
pub mod elevation;
pub mod input;
pub mod keymap;
pub mod power;
pub mod server;
pub mod session;
pub mod video;

pub use control::{HostHandle, HostState, HostStatus, Reporter};
pub use server::{RunOpts, run};
