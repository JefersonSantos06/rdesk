//! Shared rdesk logic used by both host and client.

pub mod agent;
pub mod auth;
pub mod clipboard;
pub mod config;
pub mod directory;
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

/// A command for a helper CLI (`tailscale`, `op`). On Windows it gets no console window of
/// its own, which would otherwise flash up whenever the GUI launcher (detached from any
/// console) runs one.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
