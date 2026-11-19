//! Windows only: the service that runs the host as SYSTEM, and what the host needs when
//! it runs that way (following the sign-in/lock/UAC desktop, files as the signed-in user,
//! the session notice).

pub mod desktop;
pub mod files;
pub mod notice;
pub mod proc;
pub mod service;
pub mod update;
