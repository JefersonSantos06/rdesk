//! Message types for every stream.
//!
//! Stream layout (see the README):
//! * control: the first bidirectional stream, opened by the client. Carries the
//!   auth handshake ([`ClientMsg`]/[`HostMsg`] auth variants) and then session control.
//! * client-opened unidirectional streams start with a [`StreamOpen`] tag
//!   (`Input` or `Acks`) followed by [`InputEvent`]s or [`FrameAck`]s.
//! * client-opened bidirectional streams after auth start with [`StreamOpen::File`]
//!   followed by a [`FileReq`].
//! * host-opened unidirectional streams carry exactly one video frame:
//!   a [`FrameHeader`] followed by [`Tile`]s until the stream finishes.

use serde::{Deserialize, Serialize};

use crate::key::Key;

pub type Nonce = [u8; 32];
pub type SessionId = [u8; 16];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    Resume,
    PublicKey,
    Password,
    /// An access ticket from the rdesk server ([`crate::ticket`]).
    Ticket,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u32,
    pub name: String,
    /// Position in the host's virtual desktop, in physical pixels.
    pub x: i32,
    pub y: i32,
    /// Size in physical pixels (what capture produces).
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub monitors: Vec<MonitorInfo>,
    pub active_monitor: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthOk {
    pub session_id: SessionId,
    pub resume_token: [u8; 32],
    pub resume_ttl_s: u32,
    pub host: HostInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since the Unix epoch, 0 if unknown.
    pub mtime: u64,
}

/// Messages the client sends on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    // ---- handshake ----
    Hello {
        proto: u16,
        client_name: String,
        methods: Vec<AuthMethod>,
    },
    /// Ask whether a key is authorized before asking 1Password to sign.
    PubkeyQuery {
        public_key: String,
    },
    /// `sshsig` is the armored SSHSIG over the auth payload.
    PubkeyAuth {
        public_key: String,
        sshsig: String,
    },
    PasswordStart {
        ke1: Vec<u8>,
    },
    PasswordFinish {
        ke3: Vec<u8>,
    },
    Resume {
        session_id: SessionId,
        mac: [u8; 32],
    },
    // ---- session ----
    Ping {
        t_us: u64,
    },
    SelectMonitor {
        id: u32,
    },
    SetQuality {
        max_fps: u8,
        jpeg_quality: u8,
    },
    RequestFullFrame,
    Clipboard {
        text: String,
    },
    ListDir {
        req: u32,
        path: String,
    },
    /// Instead of `Hello`: ask whether rdesk is running and whether it is busy, without
    /// logging in. The host answers with [`HostMsg::ProbeInfo`] and closes the connection.
    Probe,
    /// An access ticket (postcard of [`crate::ticket::SignedTicket`]).
    Ticket {
        ticket: Vec<u8>,
    },
}

/// Messages the host sends on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMsg {
    // ---- handshake ----
    Hello {
        proto: u16,
        host_name: String,
        server_nonce: Nonce,
        methods: Vec<AuthMethod>,
    },
    PubkeyOk,
    PubkeyRejected,
    PasswordKe2 {
        ke2: Vec<u8>,
    },
    /// Resume token unknown or expired; the client should fall back to another method.
    ResumeRejected,
    AuthOk(AuthOk),
    AuthFail {
        reason: String,
    },
    // ---- session ----
    Pong {
        t_us: u64,
    },
    Monitors {
        monitors: Vec<MonitorInfo>,
        active: u32,
    },
    Clipboard {
        text: String,
    },
    DirListing {
        req: u32,
        path: String,
        result: Result<Vec<DirEntry>, String>,
    },
    /// Sent right before the host closes the connection because a newer session took over.
    Replaced,
    Notice {
        text: String,
    },
    /// Answer to [`ClientMsg::Probe`]. Sent before authentication, so it only carries what
    /// anyone who can reach the host could learn anyway.
    ProbeInfo {
        proto: u16,
        host_name: String,
        os: String,
        version: String,
        /// Another client is currently connected.
        in_session: bool,
    },
    /// The ticket is valid, but this machine also wants its own password or SSH key.
    TicketOkNeedLocal,
}

/// First message on every client-opened stream other than the control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamOpen {
    Input,
    Acks,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute position on the active monitor, normalized to 0..=65535 on each axis.
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Scroll in "notches" (positive = up / right). Fractions come from touchpads.
    Scroll {
        dx: f32,
        dy: f32,
    },
    Key {
        key: Key,
        down: bool,
    },
    /// Type text directly (IME commit, paste-as-typing).
    Text {
        text: String,
    },
    /// Release every key and button the host believes is held.
    ReleaseAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameAck {
    pub frame_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameHeader {
    pub frame_id: u64,
    pub monitor_id: u32,
    pub width: u32,
    pub height: u32,
    /// The client must discard its framebuffer and expect every tile.
    pub full: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileEncoding {
    /// `data` is 4 bytes, RGBA; the whole tile is that color.
    Solid,
    /// `data` is zstd-compressed tightly-packed RGBA (w*h*4 bytes).
    Zstd,
    /// `data` is a baseline JPEG.
    Jpeg,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tile {
    /// Top-left corner in pixels.
    pub x: u32,
    pub y: u32,
    pub w: u16,
    pub h: u16,
    pub enc: TileEncoding,
    pub data: Vec<u8>,
}

/// First message on a file stream (client → host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileReq {
    /// Upload into the host's inbox directory. `name` is a bare file name.
    Push { name: String, size: u64 },
    /// Download `path` from the host, starting at `offset`.
    Pull { path: String, offset: u64 },
}

/// Host's reply to a [`FileReq`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileResp {
    /// Push accepted; the client should start sending from `offset` (resume).
    Accept {
        offset: u64,
    },
    /// Pull accepted; data follows starting at the requested offset.
    PullMeta {
        name: String,
        size: u64,
    },
    Reject {
        reason: String,
    },
    /// Final status after a push's `Done`.
    Complete {
        result: Result<String, String>,
    },
}

/// Data messages on a file stream, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileData {
    Chunk(Vec<u8>),
    /// BLAKE3 of the entire file (including any resumed prefix).
    Done {
        blake3: [u8; 32],
    },
}
