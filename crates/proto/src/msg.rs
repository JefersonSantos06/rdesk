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
    /// Session extensions (bidirectional): [`ExtClient`] in, [`ExtHost`] out. Hosts older
    /// than this variant drop the stream without answering, which tells the client they
    /// have none of the extensions; nothing else in the session is affected.
    Extensions,
    /// Instead of [`StreamOpen::Acks`], to a host that listed [`Feature::Video2`]: start the
    /// picture with frames in the second format ([`Tile2`]), applied by the client in
    /// order, and with what this client can decode.
    Acks2(VideoCaps),
    /// The computer's sound (bidirectional, to a host that listed [`Feature::Audio`]): the host
    /// answers with [`AudioHost`] messages until the client closes the stream.
    Audio,
    /// A TCP connection made by the host (bidirectional, full control): [`TunnelReq`], then
    /// [`TunnelResp`], then the connection's bytes as they are, both ways.
    Tunnel,
}

/// Where a tunnel goes, from the host: `localhost` is the host itself; any other name or
/// address is reached from its network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelReq {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelResp {
    Ok,
    Err(String),
}

/// The host's side of an audio stream ([`StreamOpen::Audio`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioHost {
    /// Opus packets follow, each [`AudioHost::FRAME_MS`] long, at this rate and channel count.
    Start { sample_rate: u32, channels: u8 },
    /// One Opus packet.
    Packet(Vec<u8>),
    /// No sound (or no more): why.
    Error(String),
}

impl AudioHost {
    pub const FRAME_MS: u32 = 20;
}

/// What a client can do with the second frame format ([`StreamOpen::Acks2`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCaps {
    /// How many tiles the client keeps for [`TileData::Cached`] (0: none).
    pub cache_slots: u32,
    /// It decodes [`TileData::H264`].
    pub h264: bool,
}

/// A tile of the second frame format. The client applies frames in `frame_id` order (a
/// [`FrameHeader`] with `full` skips any frame before it); [`TileData::Copy`] reads the
/// picture as it was before the frame, so a frame's copies are resolved before any of its
/// tiles is painted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tile2 {
    /// Top-left corner in pixels.
    pub x: u32,
    pub y: u32,
    pub w: u16,
    pub h: u16,
    pub data: TileData,
    /// Keep the decoded pixels in this cache slot (replacing what was there).
    pub store: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileData {
    /// The whole tile is this color (RGB; alpha is always opaque).
    Solid([u8; 3]),
    /// zstd of tightly packed RGBA.
    Zstd(Vec<u8>),
    /// A baseline JPEG.
    Jpeg(Vec<u8>),
    /// zstd of: color count − 1 (u8), the colors (RGB), then each row's indexes packed
    /// at 1, 2, 4 or 8 bits (the fewest that fit the count), rows padded to a byte.
    Palette(Vec<u8>),
    /// zstd of RGB, each byte minus the same channel of the pixel to its left (the first
    /// pixel of a row: minus the one above it).
    Rgb(Vec<u8>),
    /// The pixels kept in this cache slot.
    Cached(u32),
    /// The pixels at (`x`, `y`) of the picture as it was before this frame.
    Copy { x: u32, y: u32 },
    /// An H.264 access unit covering the whole picture (this tile is the frame's size).
    H264(Vec<u8>),
}

/// The client's side of the extensions stream. New messages are only ever appended, and
/// a client sends one only if the host listed its feature in [`ExtHost::Hello`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExtClient {
    /// First message: the protocol revision of the extensions the client speaks.
    Hello { rev: u16 },
    /// Scale the picture down to fit `width`×`height` (0×0: the monitor's own size). Mouse
    /// coordinates stay normalized to the monitor, so input is unaffected.
    MaxSize { width: u32, height: u32 },
    /// Send the pointer position ([`ExtHost::Cursor`]) while on.
    Cursor { on: bool },
    /// Let the host choose quality, frames per second and size for the link while on
    /// (it reports its choice with [`ExtHost::Quality`]); `SetQuality` sets the ceiling.
    AutoQuality { on: bool },
    /// Send the pointer's image ([`ExtHost::CursorShape`]) while on: the client shows it as
    /// its own pointer over the picture.
    CursorShape { on: bool },
    /// Send images copied on the host ([`ExtHost::ClipImage`]) while on; the client sends its own.
    ClipboardImages { on: bool },
    /// An image copied here, as PNG, for the host's clipboard.
    ClipImage(Vec<u8>),
    /// Restart the computer (full control only). The session drops and reconnects once it is
    /// back.
    Power(PowerAction),
    /// Turn the black screen at the computer on or off (full control only; off when the
    /// session ends).
    Privacy { on: bool },
    /// Block the computer's own keyboard and mouse, or not (full control only; unblocked
    /// when the session ends, and by Ctrl+Alt+Del at the computer).
    BlockInput { on: bool },
    /// A phone's system button or action (full control only).
    Nav(NavAction),
    /// The fingers on the screen now (full control only), each where it is (normalized like
    /// [`InputEvent::MouseMove`]); a finger missing from the next one was lifted, and an empty
    /// one lifts them all. Sent on every change, for pinches and other gestures of several
    /// fingers on a phone.
    Touch(Vec<TouchPoint>),
}

/// One finger in [`ExtClient::Touch`]: the same `id` from when it touches until it lifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchPoint {
    pub id: u8,
    pub x: u16,
    pub y: u16,
}

impl TouchPoint {
    /// Fingers a phone takes at once.
    pub const MAX: usize = 10;
}

/// What [`ExtClient::Nav`] does on a phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavAction {
    Back,
    Home,
    Recents,
    Notifications,
    QuickSettings,
    LockScreen,
    Screenshot,
}

/// The host's side of the extensions stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExtHost {
    /// Answer to [`ExtClient::Hello`]: what this host can do.
    Hello { rev: u16, features: Vec<Feature> },
    /// Where the pointer is on the active monitor (normalized 0..=65535 like input), or
    /// `visible: false` when it is elsewhere (another monitor) or cannot be known.
    Cursor { x: u16, y: u16, visible: bool },
    /// What automatic quality chose: frames per second, JPEG quality, and the picture's
    /// scale in percent of the monitor.
    Quality { fps: u8, jpeg_quality: u8, scale_pct: u8 },
    /// The pointer changed: its image (sent the first time this `id` is used; after that
    /// only the id), or `None` with id 0 when the pointer is hidden.
    CursorShape { id: u64, image: Option<CursorShape> },
    /// An image copied on the host, as PNG, for the client's clipboard.
    ClipImage(Vec<u8>),
    /// Something to tell the user (an action that failed or happened).
    Notice(String),
}

/// A pointer image: tightly packed RGBA (straight alpha), at most [`CursorShape::MAX`]
/// pixels on a side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    /// The point of the image that sits at the pointer's position.
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

impl CursorShape {
    pub const MAX: u16 = 96;
}

/// What the extensions stream can carry (see [`ExtHost::Hello`]). Unknown features are
/// never sent to an older client: the host lists only what the client's `rev` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Feature {
    MaxSize,
    Cursor,
    /// The second frame format ([`StreamOpen::Acks2`]).
    Video2,
    /// The host adjusts quality and size to the link ([`ExtClient::AutoQuality`]).
    AutoQuality,
    /// The pointer's image ([`ExtClient::CursorShape`], [`ExtHost::CursorShape`]).
    CursorShape,
    /// The computer's sound ([`StreamOpen::Audio`]).
    Audio,
    /// Images copied on either side ([`ExtClient::ClipboardImages`]).
    ClipboardImage,
    /// Restarting the computer ([`ExtClient::Power`]).
    Power,
    /// A black screen for whoever is at the computer while the session sees it as it is
    /// ([`ExtClient::Privacy`]).
    Privacy,
    /// The computer's own keyboard and mouse can be blocked ([`ExtClient::BlockInput`]).
    BlockInput,
    /// Port tunnels ([`StreamOpen::Tunnel`]).
    Tunnel,
    /// A phone's system buttons ([`ExtClient::Nav`]).
    Nav,
    /// Typed characters go as [`InputEvent::Text`], not as the keys that make them: the host
    /// has no keyboard layout of its own (a phone), so a key's position would type the wrong
    /// letter. Keys that type nothing (Backspace, arrows, shortcuts) still go as keys.
    TextKeys,
    /// Fingers on a phone's screen ([`ExtClient::Touch`]).
    MultiTouch,
}

/// What [`ExtClient::Power`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerAction {
    Restart,
    /// Restart in safe mode with networking (Windows), reachable again from there; the next
    /// restart is a normal one.
    RestartSafeMode,
}

/// Revision of the extensions protocol (1: picture size limit and pointer position;
/// 2: second frame format, automatic quality, pointer image; 3: phones' system buttons and
/// typing as text).
pub const EXT_REV: u16 = 3;

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
