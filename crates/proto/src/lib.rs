//! rdesk wire protocol.

pub mod api;
pub mod codec;
pub mod key;
pub mod msg;
pub mod ticket;

pub use codec::{
    CodecError, MAX_CONTROL_MSG, MAX_FILE_MSG, MAX_SMALL_MSG, MAX_VIDEO_MSG, decode, encode, encode_into, expect_msg,
    read_msg, write_msg,
};
pub use key::Key;
pub use msg::*;

/// Bumped on any incompatible change to the message types (2: access tickets).
pub const PROTOCOL_VERSION: u16 = 2;
/// QUIC ALPN identifier.
pub const ALPN: &[u8] = b"rdesk/1";
/// Default UDP port.
pub const DEFAULT_PORT: u16 = 7420;
/// Size of a video tile edge, in pixels.
pub const TILE: u32 = 64;

#[cfg(test)]
mod tests;
