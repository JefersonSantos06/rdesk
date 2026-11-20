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

/// What a session may do. Granted by the server's ticket (per share: control, view only,
/// files only); enforced by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access {
    /// See the screen.
    pub screen: bool,
    /// Keyboard, mouse and clipboard.
    pub input: bool,
    /// Send, browse and download files.
    pub files: bool,
    /// Open a terminal (a shell as the signed-in user). Not in [`AuthOk`] on the wire (that
    /// keeps the older layout, see [`WireAccess`]): the host knows it from the ticket.
    #[serde(default)]
    pub terminal: bool,
}

/// [`Access`] as [`AuthOk`] carries it: the first protocol version's three fields, so
/// older and newer apps and hosts keep understanding each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireAccess {
    pub screen: bool,
    pub input: bool,
    pub files: bool,
}

impl From<Access> for WireAccess {
    fn from(a: Access) -> Self {
        WireAccess { screen: a.screen, input: a.input, files: a.files }
    }
}

impl From<WireAccess> for Access {
    /// Full control includes the terminal; the wire does not say more.
    fn from(w: WireAccess) -> Self {
        let full = w.screen && w.input && w.files;
        Access { screen: w.screen, input: w.input, files: w.files, terminal: full }
    }
}

impl Access {
    pub const FULL: Access = Access { screen: true, input: true, files: true, terminal: true };
    pub const VIEW: Access = Access { screen: true, input: false, files: false, terminal: false };
    pub const FILES: Access = Access { screen: false, input: false, files: true, terminal: false };
    pub const TERMINAL: Access = Access { screen: false, input: false, files: false, terminal: true };
    pub const NONE: Access = Access { screen: false, input: false, files: false, terminal: false };

    pub fn union(self, o: Access) -> Access {
        Access {
            screen: self.screen || o.screen,
            input: self.input || o.input,
            files: self.files || o.files,
            terminal: self.terminal || o.terminal,
        }
    }

    /// Everything `o` allows is allowed here too.
    pub fn covers(self, o: Access) -> bool {
        (self.screen || !o.screen)
            && (self.input || !o.input)
            && (self.files || !o.files)
            && (self.terminal || !o.terminal)
    }

    pub fn is_none(self) -> bool {
        self == Access::NONE
    }

    /// `"screen,input,files"` (the ticket's form).
    pub fn to_text(self) -> String {
        let mut v = Vec::new();
        if self.screen {
            v.push("screen");
        }
        if self.input {
            v.push("input");
        }
        if self.files {
            v.push("files");
        }
        if self.terminal {
            v.push("terminal");
        }
        v.join(",")
    }

    pub fn from_text(s: &str) -> Access {
        // Tickets of the first version said "control".
        if s.trim() == "control" {
            return Access::FULL;
        }
        let has = |w: &str| s.split(',').any(|p| p.trim() == w);
        let (screen, input, files) = (has("screen"), has("input"), has("files"));
        // Servers before the terminal wrote full control as "screen,input,files".
        let terminal = has("terminal") || (screen && input && files);
        Access { screen, input, files, terminal }
    }

    /// "full control", "view only", "files only", "view + files"…
    pub fn label(self) -> String {
        match self {
            Access::FULL => "full control".into(),
            Access::VIEW => "view only".into(),
            Access::FILES => "files only".into(),
            Access::TERMINAL => "terminal only".into(),
            Access::NONE => "no access".into(),
            a => a.to_text().replace(',', " + "),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthOk {
    pub session_id: SessionId,
    pub resume_token: [u8; 32],
    pub resume_ttl_s: u32,
    pub host: HostInfo,
    /// What this session may do (the host enforces it).
    pub access: WireAccess,
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
    /// logging in. Hosts no longer answer it (they refuse with `AuthFail`): status comes
    /// from the rdesk server.
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
    /// A terminal (bidirectional): [`TermClient`] in, [`TermHost`] out.
    Terminal,
}

/// The client's side of a terminal stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TermClient {
    /// First message: the window's size in characters.
    Start {
        cols: u16,
        rows: u16,
    },
    /// Keys, as a terminal sends them (UTF-8 and escape sequences).
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// First message instead of `Start`: the shell with administrator rights (Windows, full
    /// control only, if the host allows it). Older hosts answer it with an error.
    StartAdmin {
        cols: u16,
        rows: u16,
    },
}

impl TermClient {
    /// A first message: the size, and whether it asks for administrator rights.
    pub fn start(&self) -> Option<(u16, u16, bool)> {
        match *self {
            TermClient::Start { cols, rows } => Some((cols, rows, false)),
            TermClient::StartAdmin { cols, rows } => Some((cols, rows, true)),
            _ => None,
        }
    }
}

/// The host's side of a terminal stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TermHost {
    /// The shell runs: which one, and as whom.
    Started {
        shell: String,
        user: String,
    },
    Output(Vec<u8>),
    /// The shell ended (its exit code).
    Exited(u32),
    /// It could not start (or stopped for this reason).
    Error(String),
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
