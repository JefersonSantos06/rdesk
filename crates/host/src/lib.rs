//! rdesk host: everything that runs on the machine being controlled.

pub mod admin;
pub mod audio;
pub mod autoq;
pub mod capture;
pub mod consent;
pub mod control;
pub mod cursor;
pub mod elevation;
pub mod enroll;
pub mod h264;
pub mod input;
pub mod ipc;
pub mod keymap;
pub mod localnet;
#[cfg(target_os = "macos")]
pub mod mac_permissions;
#[cfg(any(target_os = "macos", test))]
pub mod mac_update;
pub mod pointer;
pub mod power;
pub mod restart;
pub mod scale;
pub mod scroll;
pub mod server;
pub mod session;
pub mod sync;
pub mod terminal;
pub mod tilecache;
pub mod tunnel;
pub mod update;
pub mod video;
pub mod wake;
#[cfg(all(desktop, target_os = "linux"))]
pub mod wayland;
#[cfg(windows)]
pub mod win;

pub use control::{HostHandle, HostState, HostStatus, Reporter};
pub use server::{RunOpts, run};
