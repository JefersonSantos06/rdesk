//! Shared rdesk logic used by both host and client.

pub mod account;
pub mod agent;
pub mod auth;
pub mod clipboard;
pub mod config;
pub mod directory;
pub mod files;
#[cfg(feature = "h264")]
pub mod h264;
pub mod i18n;
pub mod release;
pub mod ticket;
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

static DEVICE_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Name this device by `name` instead of its host name (a phone's own name, as the user
/// set it; its host name is meaningless there). Set once, at start.
pub fn set_device_name(name: String) {
    let _ = DEVICE_NAME.set(name);
}

pub fn local_hostname() -> String {
    if let Some(n) = DEVICE_NAME.get() {
        return n.clone();
    }
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
