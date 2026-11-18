//! Shared rdesk logic used by both host and client.

pub mod agent;
pub mod auth;
pub mod clipboard;
pub mod config;
pub mod files;
pub mod tiles;
pub mod transport;

pub use rdesk_proto as proto;

/// Microseconds since an arbitrary process-wide epoch (for RTT pings).
pub fn now_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub fn local_hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}
