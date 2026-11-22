//! `rdesk` command line.

#[cfg(feature = "host")]
use std::io::Write as _;
use std::path::PathBuf;
#[cfg(feature = "client")]
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use rdesk_common::account::{self, Session};
use rdesk_common::agent::{self, Agent};
#[cfg(feature = "client")]
use rdesk_common::auth;
use rdesk_common::config::{self, AuthPref, ClientConfig, HostEntry, NetworkConfig, format_host_id, parse_host_id};
#[cfg(feature = "client")]
use rdesk_common::transport;
#[cfg(feature = "host")]
use rdesk_common::transport::HostIdentity;
use rdesk_proto::api::{
    GroupRole, Machine, MachineRequest, NewEnrollKey, SetShares, ShareRole, UpdateMachine, Visibility,
};
#[cfg(feature = "host")]
use ssh_key::PublicKey;
#[cfg(feature = "host")]
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(name = "rdesk", version, about = "Remote desktop over QUIC (iroh), authenticated with 1Password")]
struct Cli {
    /// More logging (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// Use this saved account instead of the one in use, for this command only: its number
    /// in `rdesk server list`, its server, its server's name or its e-mail.
    #[arg(long, global = true, value_name = "ACCOUNT")]
    account: Option<String>,
    /// Without a command: open the app window (saved computers, this computer's host, settings).
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Commands for the machine being controlled.
    #[cfg(feature = "host")]
    #[command(subcommand)]
    Host(HostCmd),
    /// Install rdesk on this Windows computer: the program, the service, Settings → Apps.
    /// Asks for administrator permission.
    #[cfg(all(windows, feature = "host"))]
    Install {
        /// Only accessed, never used to reach others: it asks the server to add it.
        #[arg(long)]
        only_share: bool,
        /// The rdesk server (default: from the program's name, rdesk@server.exe).
        #[arg(long)]
        server: Option<String>,
        /// With --only-share: who will own it (they approve it in their app).
        #[arg(long)]
        owner: Option<String>,
        /// With --only-share: an enrollment key (added at once).
        #[arg(long)]
        key: Option<String>,
        /// With --only-share: its name on the server.
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove rdesk from this Windows computer (the host's settings come back to you).
    #[cfg(all(windows, feature = "host"))]
    Uninstall,
    /// The Windows service: this computer reachable before sign-in, on the lock screen and
    /// through UAC (install, uninstall, status).
    #[cfg(all(windows, feature = "host"))]
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Open the app window: your computers, this computer's host, settings (also the default).
    #[cfg(feature = "client")]
    Ui {
        /// Start hidden in the tray.
        #[arg(long)]
        minimized: bool,
        /// Replace a running copy of the app (used by "Restart as administrator").
        #[arg(long, hide = true)]
        takeover: bool,
    },
    /// Start the app with the system: off, login, or login-admin (Windows, elevated).
    #[cfg(feature = "client")]
    Autostart {
        #[arg(value_parser = ["off", "login", "login-admin"])]
        mode: String,
    },
    /// Open a viewer window to a computer: its name on the server, its host ID, or
    /// ID@address for an unregistered computer on the LAN.
    #[cfg(feature = "client")]
    Connect(ConnectArgs),
    /// Computers on your server: list, register this one, rename, who may access them.
    #[command(subcommand)]
    Machines(MachinesCmd),
    /// Groups of users (to share computers with).
    #[command(subcommand)]
    Groups(GroupsCmd),
    /// Sign in to an rdesk server (opens the browser: Google/GitHub + second factor). The
    /// account is saved next to the others and is used from now on.
    Login {
        /// Server address, e.g. rdesk.example.com (default: the last one used).
        server: Option<String>,
    },
    /// Sign out of the account in use (ends this computer's session on its server).
    Logout,
    /// Who is signed in, until when, and whether the relay answers.
    Whoami,
    /// The servers you signed in to: list them, change the one in use.
    #[command(subcommand)]
    Server(ServerCmd),
    /// Install the version released on your server (Linux, as root: the .deb; run by
    /// rdesk-update.timer).
    Update {
        /// Your rdesk server.
        #[arg(long, env = "RDESK_SERVER")]
        server: String,
        /// This computer's host ID: test computers get versions first.
        #[arg(long, env = "RDESK_MACHINE")]
        machine: Option<String>,
        /// Only say whether there is something to install.
        #[arg(long)]
        check: bool,
    },
    /// Allowed users of the server (system admins only).
    #[command(subcommand)]
    Users(UsersCmd),
    /// List the keys your SSH agent (1Password) offers.
    AgentKeys {
        #[arg(long)]
        agent_sock: Option<String>,
    },
    /// Upload files into the host's inbox (~/Downloads/rdesk by default).
    #[cfg(feature = "client")]
    Send {
        target: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[command(flatten)]
        auth: AuthArgs,
    },
    /// Download a file from the host.
    #[cfg(feature = "client")]
    Get {
        target: String,
        remote: String,
        #[arg(long, default_value = ".")]
        out: PathBuf,
        #[command(flatten)]
        auth: AuthArgs,
    },
    /// List a directory on the host.
    #[cfg(feature = "client")]
    Ls {
        target: String,
        #[arg(default_value = "~")]
        path: String,
        #[command(flatten)]
        auth: AuthArgs,
    },
    /// Port tunnels until Ctrl+C: each `-L LOCAL:HOST:PORT` makes port LOCAL here lead to
    /// HOST:PORT as reached from that computer (`localhost` is the computer itself).
    #[cfg(feature = "client")]
    Tunnel {
        target: String,
        #[arg(short = 'L', required = true)]
        forward: Vec<String>,
        #[command(flatten)]
        auth: AuthArgs,
    },
    /// A shell on a computer, in this terminal (as whoever is signed in there).
    #[cfg(feature = "client")]
    Shell {
        target: String,
        /// With the administrator rights of whoever is signed in (Windows, full control).
        #[arg(long)]
        admin: bool,
        #[command(flatten)]
        auth: AuthArgs,
    },
}

#[cfg(feature = "host")]
#[derive(Subcommand)]
enum FileHelperCmd {
    /// One file stream on standard input and output.
    Serve {
        #[arg(long)]
        inbox: Option<PathBuf>,
    },
    /// A folder listing, as JSON on standard output.
    List { path: String },
    /// A terminal on standard input and output.
    Terminal,
}

#[cfg(all(windows, feature = "host"))]
#[derive(Subcommand)]
enum ServiceCmd {
    /// Install (or update) and start the service; needs an elevated terminal.
    Install,
    /// Stop and remove the service; this computer's host settings come back to you.
    Uninstall,
    /// Is it installed and running?
    Status,
    /// Run by Windows (not by hand).
    #[command(hide = true)]
    Run,
    /// Install a downloaded version (run by the service's host, from the download).
    #[command(hide = true)]
    ApplyUpdate {
        #[arg(long)]
        version: String,
    },
}

#[cfg(feature = "host")]
#[derive(Subcommand)]
enum HostCmd {
    /// Create the host identity and default config.
    Init {
        /// Regenerate the host identity: the host gets a new ID (give it to your clients again).
        #[arg(long)]
        force: bool,
    },
    /// Run the host (foreground).
    Run {
        /// UDP port for direct connections (default from host.toml, 7420).
        #[arg(long)]
        port: Option<u16>,
        /// Use a synthetic test picture instead of the real screen.
        #[arg(long, hide = true)]
        fake_capture: bool,
        /// Log input instead of injecting it.
        #[arg(long, hide = true)]
        fake_input: bool,
        /// Started by the Windows service (stops when standard input closes).
        #[arg(long, hide = true)]
        agent: bool,
    },
    /// Files for the host started by the service, run as the signed-in user.
    #[command(hide = true, subcommand)]
    FileHelper(FileHelperCmd),
    /// Print this host's ID (clients add the host with it).
    #[command(alias = "fingerprint")]
    Id,
    /// Authorize an SSH public key (OpenSSH line, .pub file, or from the agent).
    AddKey {
        key: Option<String>,
        /// Pick the key from the local SSH agent (1Password).
        #[arg(long)]
        from_agent: bool,
        /// With --from-agent: fingerprint of the key to add.
        #[arg(long)]
        key_fp: Option<String>,
        #[arg(long)]
        agent_sock: Option<String>,
    },
    ListKeys,
    RemoveKey {
        fingerprint: String,
    },
    /// Enable password login (stored as an OPAQUE record, never in clear).
    SetPassword {
        /// Read the password from 1Password, e.g. op://Private/rdesk/password
        #[arg(long)]
        op: Option<String>,
    },
    /// Disable password login.
    ClearPassword,
    /// Diagnose permissions, monitors and network.
    Check,
    /// Join a server without signing in here: the next `host run` asks the server to add
    /// this computer (its owner approves it in the app, or a key adds it at once).
    /// The RDESK_SERVER, RDESK_OWNER, RDESK_ENROLL_KEY and RDESK_NAME variables do the same.
    Enroll {
        /// The rdesk server, e.g. rdesk.example.com.
        #[arg(long, required_unless_present = "clear")]
        server: Option<String>,
        /// E-mail of who will own it (they approve it). Without it (and without a key), the
        /// server's admins approve it.
        #[arg(long, conflicts_with = "key")]
        owner: Option<String>,
        /// An enrollment key (Computers → Add a computer): added at once, no approval.
        #[arg(long)]
        key: Option<String>,
        /// Name on the server (default: this computer's name).
        #[arg(long)]
        name: Option<String>,
        /// Stop asking (forget these settings).
        #[arg(long, conflicts_with_all = ["server", "owner", "key", "name"])]
        clear: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum VisArg {
    /// Only you.
    Private,
    /// You and the users/groups you share it with.
    Shared,
    /// Everyone on the server.
    Everyone,
}

impl From<VisArg> for Visibility {
    fn from(v: VisArg) -> Self {
        match v {
            VisArg::Private => Visibility::Private,
            VisArg::Shared => Visibility::Shared,
            VisArg::Everyone => Visibility::Everyone,
        }
    }
}

#[derive(Subcommand)]
enum MachinesCmd {
    /// The computers you can reach (and yours).
    List,
    /// Register this computer with your server (so others with access can reach it).
    #[cfg(feature = "host")]
    Register {
        /// Name on the server (default: this computer's name).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value = "private")]
        visibility: VisArg,
    },
    /// Remove this computer from the server.
    #[cfg(feature = "host")]
    Unregister,
    /// Rename a computer you registered.
    Rename { machine: String, name: String },
    /// Who may see and reach a computer you registered.
    Visibility {
        machine: String,
        #[arg(value_enum)]
        visibility: VisArg,
    },
    /// Show whom a computer is shared with.
    Shares { machine: String },
    /// Set whom a computer is shared with (replaces the list; also sets it to "shared").
    /// Add `:view` or `:files` for view-only or files-only (default: full control).
    Share {
        machine: String,
        /// `email[:control|view|files]` (repeat for several).
        #[arg(long = "user")]
        users: Vec<String>,
        /// `group[:control|view|files]` (repeat for several).
        #[arg(long = "group")]
        groups: Vec<String>,
    },
    /// Show or set (owner only) the other managers of a computer.
    Managers {
        machine: String,
        /// Replace the list with these e-mails (`--set` with none clears it).
        #[arg(long = "set", num_args = 0..)]
        set: Option<Vec<String>>,
    },
    /// The last sessions on a computer (its managers and system admins).
    History { machine: String },
    /// Turn on a computer that is off (Wake-on-LAN, sent by a computer that is on, on its network).
    Wake { machine: String },
    /// Computers waiting for your approval (asked with your e-mail; admins: also without one).
    Requests,
    /// Approve a waiting computer (by its code, as it shows it): it becomes yours.
    Approve {
        /// The code (e.g. 3F2A-91C0) or the request number.
        request: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value = "private")]
        visibility: VisArg,
    },
    /// Refuse a waiting computer.
    Refuse { request: String },
    /// Enrollment keys: add computers (e.g. ready-made images) without approval.
    #[command(subcommand)]
    Keys(KeysCmd),
}

#[derive(Subcommand)]
enum KeysCmd {
    List,
    /// Create a key (printed once; keep it secret).
    Create {
        name: String,
        /// Valid for this many days (1 to 365).
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Usable for a single computer.
        #[arg(long)]
        once: bool,
        /// Who can see the computers it adds.
        #[arg(long, value_enum, default_value = "private")]
        visibility: VisArg,
        /// Share them with these groups: `group[:control|view|files]` (sets "shared").
        #[arg(long = "group")]
        groups: Vec<String>,
    },
    /// Revoke a key (computers it added stay).
    Revoke {
        key: String,
    },
}

#[derive(Subcommand)]
enum GroupsCmd {
    List,
    Create {
        name: String,
    },
    Rename {
        group: String,
        name: String,
    },
    Delete {
        group: String,
    },
    /// Add a member (or change their role with `role`).
    Add {
        group: String,
        email: String,
        /// Make them an admin of the group.
        #[arg(long)]
        admin: bool,
    },
    Role {
        group: String,
        email: String,
        #[arg(value_parser = ["admin", "member"])]
        role: String,
    },
    Remove {
        group: String,
        email: String,
    },
    Leave {
        group: String,
    },
}

#[derive(Subcommand)]
enum UsersCmd {
    List,
    /// Allow an e-mail to sign in.
    Add {
        email: String,
        #[arg(long)]
        admin: bool,
    },
    Remove {
        email: String,
    },
    /// Make a system admin (`on`) or a regular user (`off`).
    Admin {
        email: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Block sign-in and end the user's sessions (`on`), or allow again (`off`).
    Disable {
        email: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Forget the user's second factor (a lost phone) and end their sessions: their next
    /// sign-in sets up a new one.
    #[command(name = "reset-2fa")]
    Reset2fa {
        email: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AuthArg {
    Auto,
    Agent,
    Password,
}

impl From<AuthArg> for AuthPref {
    fn from(a: AuthArg) -> Self {
        match a {
            AuthArg::Auto => AuthPref::Auto,
            AuthArg::Agent => AuthPref::Agent,
            AuthArg::Password => AuthPref::Password,
        }
    }
}

#[derive(Subcommand)]
enum ServerCmd {
    /// The saved accounts (server and e-mail), the one in use marked with *.
    #[command(alias = "ls")]
    List,
    /// Use another saved account from now on: its number in the list, its server, its
    /// server's name or its e-mail.
    Use { account: String },
}

#[derive(Args, Clone)]
struct AuthArgs {
    #[arg(long, value_enum)]
    auth: Option<AuthArg>,
    /// Agent key fingerprint (SHA256:...).
    #[arg(long)]
    key: Option<String>,
    /// 1Password reference for the password.
    #[arg(long)]
    password_ref: Option<String>,
    #[arg(long)]
    agent_sock: Option<String>,
    /// Unencrypted OpenSSH private key file to use instead of the agent.
    #[arg(long)]
    identity: Option<PathBuf>,
}

#[cfg(feature = "client")]
#[derive(Args)]
struct ConnectArgs {
    target: String,
    #[command(flatten)]
    auth: AuthArgs,
    /// Monitor id to show first.
    #[arg(long)]
    monitor: Option<u32>,
    /// Don't share the clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Don't map Cmd to Ctrl (the default on macOS).
    #[arg(long)]
    no_swap_cmd_ctrl: bool,
    /// Map Cmd/Super to Ctrl.
    #[arg(long)]
    swap_cmd_ctrl: bool,
    /// No window: print stats, stop after --frames.
    #[arg(long)]
    headless: bool,
    #[arg(long, default_value_t = 60)]
    frames: u64,
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// With --headless: write the last picture to this .ppm file.
    #[arg(long)]
    dump: Option<PathBuf>,
    /// Open with the file-transfer panel showing.
    #[arg(long)]
    files: bool,
    /// Open a terminal on it right away.
    #[arg(long)]
    terminal: bool,
    /// Started by the app: bring the window forward (and open files or a terminal) when the
    /// app writes `show`, `files` or `terminal` to standard input.
    #[arg(long, hide = true)]
    controlled: bool,
}

/// The raw standard input and output: std's stdout is line-buffered, so binary replies
/// (with no newline) would sit in its buffer while the other side waits for them.
#[cfg(feature = "host")]
fn raw_stdio() -> Result<(std::fs::File, std::fs::File)> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok((
            std::fs::File::from(std::io::stdin().as_handle().try_clone_to_owned()?),
            std::fs::File::from(std::io::stdout().as_handle().try_clone_to_owned()?),
        ))
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        Ok((
            std::fs::File::from(std::io::stdin().as_fd().try_clone_to_owned()?),
            std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?),
        ))
    }
}

/// The file helper of the service's host: one file stream, or one folder listing.
#[cfg(feature = "host")]
fn file_helper(cmd: FileHelperCmd) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    match cmd {
        FileHelperCmd::Serve { inbox } => {
            let inbox = inbox.unwrap_or_else(|| rdesk_common::config::HostConfig::default().inbox_dir());
            let (stdin, stdout) = raw_stdio()?;
            rt.block_on(async {
                let (mut r, mut w) = (tokio::fs::File::from_std(stdin), tokio::fs::File::from_std(stdout));
                rdesk_common::files::serve(&mut r, &mut w, &inbox).await?;
                tokio::io::AsyncWriteExt::flush(&mut w).await?;
                Ok(())
            })
        }
        FileHelperCmd::Terminal => {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let (stdin, stdout) = raw_stdio()?;
            rt.block_on(async {
                let (mut r, mut w) = (tokio::fs::File::from_std(stdin), tokio::fs::File::from_std(stdout));
                rdesk_host::terminal::serve(&mut r, &mut w).await?;
                tokio::io::AsyncWriteExt::flush(&mut w).await?;
                Ok(())
            })
        }
        FileHelperCmd::List { path } => {
            let listing = rt.block_on(rdesk_common::files::list_dir(&path));
            let mut out = std::io::stdout();
            serde_json::to_writer(&mut out, &listing)?;
            out.flush()?;
            Ok(())
        }
    }
}

/// Not elevated: run this same command again as administrator (Windows asks).
#[cfg(all(windows, feature = "host"))]
fn elevate_self() -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let exe = std::env::current_exe()?;
    let args: Vec<String> =
        std::env::args().skip(1).map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a }).collect();
    let (verb, file, params) = (wide("runas".as_ref()), wide(exe.as_os_str()), wide(args.join(" ").as_ref()));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 1;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        bail!("administrator permission was not given");
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
    }
    if code != 0 {
        bail!("the installer (as administrator) failed with code {code}");
    }
    Ok(())
}

#[cfg(all(windows, feature = "host"))]
fn install_cmd(opts: rdesk_host::win::install::InstallOpts) -> Result<()> {
    if !rdesk_host::elevation::is_elevated() {
        return elevate_self();
    }
    println!("{}", rdesk_host::win::install::install(&opts)?);
    let exe = rdesk_host::win::service::program_exe();
    #[cfg(feature = "client")]
    if !opts.only_share {
        // The app starts with Windows (in the tray), from the program the service keeps updated.
        rdesk_client::app::apply_autostart(rdesk_client::app::Autostart::Login, &exe)?;
        if let Some(server) = opts.server.clone().or_else(config::server_from_exe_name) {
            let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG).unwrap_or_default();
            cfg.server.get_or_insert(server);
            config::ensure_config_dir()?;
            config::save_toml(config::CLIENT_CONFIG, &cfg)?;
        }
        println!("Open rdesk from the Start menu ({}).", exe.display());
    }
    Ok(())
}

#[cfg(all(windows, feature = "host"))]
fn uninstall_cmd() -> Result<()> {
    if !rdesk_host::elevation::is_elevated() {
        return elevate_self();
    }
    #[cfg(feature = "client")]
    let _ = rdesk_client::app::apply_autostart(rdesk_client::app::Autostart::Off, &std::env::current_exe()?);
    println!("{}", rdesk_host::win::install::uninstall()?);
    Ok(())
}

#[cfg(all(windows, feature = "host"))]
fn service_cmd(cmd: ServiceCmd) -> Result<()> {
    use rdesk_host::win::service::{self, State};
    match cmd {
        ServiceCmd::Install => {
            println!("{}", service::install()?);
            // Starting at login now opens the program the service keeps up to date, normally
            // (the service does the administrator part).
            #[cfg(feature = "client")]
            if rdesk_client::app::current_autostart() != rdesk_client::app::Autostart::Off {
                rdesk_client::app::apply_autostart(rdesk_client::app::Autostart::Login, &service::program_exe())?;
                println!("At login the app starts from {}.", service::program_exe().display());
            }
            Ok(())
        }
        ServiceCmd::Uninstall => {
            println!("{}", service::uninstall()?);
            Ok(())
        }
        ServiceCmd::Status => {
            let text = match service::state()? {
                State::NotInstalled => "not installed (`rdesk service install` in an elevated terminal)",
                State::Stopped => "installed, stopped",
                State::Running => "installed, running",
                State::Changing => "installed, starting or stopping",
            };
            println!("rdesk service: {text}");
            if let Some(dir) = config::service_dir().filter(|d| d.is_dir()) {
                println!("Host files: {}", dir.display());
            }
            if service::state()? == State::Running {
                use rdesk_host::ipc::{Request, Response, request};
                match runtime()?.block_on(request(&Request::Status)) {
                    Ok(Response::Status(s)) => {
                        println!("Host: {:?}", s.state);
                        if let Some(fp) = s.fingerprint {
                            println!("Host ID: {}", format_host_id(&fp));
                        }
                        if let Some(r) = &s.registration {
                            println!("Server: {}", r.as_ref().unwrap_or_else(|e| e));
                        }
                        if let Some(r) = &s.relay {
                            println!("Relay: {}", r.as_ref().unwrap_or_else(|e| e));
                        }
                        if let Some(sess) = &s.session {
                            println!("In use by {} from {}", sess.client, sess.peer);
                        }
                        for e in s.events.iter().rev().take(5).rev() {
                            println!("  {}{}", if e.warning { "! " } else { "" }, e.text);
                        }
                    }
                    Ok(other) => println!("Host: unexpected answer {other:?}"),
                    Err(e) => println!("Host: not answering on its control pipe ({e:#})"),
                }
            }
            Ok(())
        }
        ServiceCmd::Run => service::run(),
        ServiceCmd::ApplyUpdate { version } => service::apply_update(&version),
    }
}

fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;
    let default = match verbose {
        0 => "warn,rdesk=info,rdesk_host=info,rdesk_client=info,rdesk_common=info,iroh::socket::transports::relay=off",
        1 => "info,rdesk=debug,rdesk_host=debug,rdesk_client=debug,rdesk_common=debug",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

/// No console (the desktop app, the service and the host it starts): log to
/// `<dir>/logs/<name>` instead.
fn init_file_logging(verbose: u8, dir: Result<PathBuf>, name: &str) {
    use tracing_subscriber::EnvFilter;
    let file = dir
        .map(|d| d.join("logs"))
        .and_then(|d| std::fs::create_dir_all(&d).map(|_| d).map_err(Into::into))
        .and_then(|d| {
            // Append: a second launch (which only wakes the running app) must not clobber it.
            let path = d.join(name);
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
                let _ = std::fs::remove_file(&path);
            }
            std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(Into::into)
        });
    let Ok(file) = file else {
        return init_logging(verbose);
    };
    let default = if verbose == 0 {
        "warn,rdesk=info,rdesk_host=info,rdesk_client=info,rdesk_common=info,iroh::socket::transports::relay=off"
    } else {
        "info,rdesk=debug,rdesk_host=debug,rdesk_client=debug,rdesk_common=debug"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    #[cfg(feature = "client")]
    let app = matches!(cli.cmd, None | Some(Command::Ui { .. }));
    #[cfg(not(feature = "client"))]
    let app = false;
    #[cfg(feature = "host")]
    let agent = matches!(cli.cmd, Some(Command::Host(HostCmd::Run { agent: true, .. })));
    #[cfg(not(feature = "host"))]
    let agent = false;
    #[cfg(all(windows, feature = "host"))]
    let service = matches!(cli.cmd, Some(Command::Service(ServiceCmd::Run)));
    #[cfg(all(windows, feature = "host"))]
    let updating = matches!(cli.cmd, Some(Command::Service(ServiceCmd::ApplyUpdate { .. })));
    #[cfg(not(all(windows, feature = "host")))]
    let updating = false;
    #[cfg(not(all(windows, feature = "host")))]
    let service = false;
    if app {
        init_file_logging(cli.verbose, config::ensure_config_dir(), "app.log")
    } else if agent {
        init_file_logging(cli.verbose, config::ensure_host_dir(), "host.log")
    } else if updating {
        init_file_logging(cli.verbose, config::ensure_host_dir(), "update.log")
    } else if service {
        init_file_logging(cli.verbose, config::ensure_host_dir(), "service.log")
    } else {
        init_logging(cli.verbose)
    }
    // The language of what people read: the app (and its helpers in the user's session)
    // follows Settings, the service the system's; the command line stays in English.
    // The file and terminal helper runs as the signed-in person, for the service.
    #[cfg(feature = "host")]
    let helper = matches!(cli.cmd, Some(Command::Host(HostCmd::FileHelper(_))));
    #[cfg(not(feature = "host"))]
    let helper = false;
    if app || agent || helper {
        rdesk_common::i18n::init_from_config();
    } else if service {
        rdesk_common::i18n::set(rdesk_common::i18n::system());
    }
    if let Some(sel) = &cli.account {
        Session::pin(&Session::find(sel)?.key());
    }
    let Some(cmd) = cli.cmd else {
        return no_command();
    };
    match cmd {
        #[cfg(feature = "client")]
        Command::Ui { minimized, takeover } => launcher(minimized, takeover),
        #[cfg(feature = "client")]
        Command::Autostart { mode } => {
            let mode = rdesk_client::app::Autostart::parse(&mode).ok_or_else(|| anyhow!("unknown mode {mode}"))?;
            let exe = std::env::current_exe()?;
            rdesk_client::app::apply_autostart(mode, &exe)?;
            println!("Start with the system: {}", mode.arg());
            Ok(())
        }
        #[cfg(feature = "host")]
        Command::Host(h) => host_cmd(h),
        #[cfg(all(windows, feature = "host"))]
        Command::Service(s) => service_cmd(s),
        #[cfg(all(windows, feature = "host"))]
        Command::Install { only_share, server, owner, key, name } => {
            install_cmd(rdesk_host::win::install::InstallOpts { only_share, server, owner, key, name })
        }
        #[cfg(all(windows, feature = "host"))]
        Command::Uninstall => uninstall_cmd(),
        #[cfg(feature = "client")]
        Command::Connect(a) => connect(a),
        Command::Machines(m) => runtime()?.block_on(machines_cmd(m)),
        Command::Groups(g) => runtime()?.block_on(groups_cmd(g)),
        Command::Login { server } => runtime()?.block_on(login(server)),
        Command::Logout => runtime()?.block_on(logout()),
        Command::Whoami => runtime()?.block_on(whoami()),
        Command::Server(c) => server_cmd(c),
        Command::Update { server, machine, check } => runtime()?.block_on(update(&server, machine, check)),
        Command::Users(u) => runtime()?.block_on(users_cmd(u)),
        Command::AgentKeys { agent_sock } => runtime()?.block_on(agent_keys(agent_sock)),
        #[cfg(feature = "client")]
        Command::Send { target, files, auth } => runtime()?.block_on(cli_send(target, files, auth)),
        #[cfg(feature = "client")]
        Command::Get { target, remote, out, auth } => runtime()?.block_on(cli_get(target, remote, out, auth)),
        #[cfg(feature = "client")]
        Command::Ls { target, path, auth } => runtime()?.block_on(cli_ls(target, path, auth)),
        #[cfg(feature = "client")]
        Command::Shell { target, admin, auth } => runtime()?.block_on(cli_shell(target, admin, auth)),
        #[cfg(feature = "client")]
        Command::Tunnel { target, forward, auth } => runtime()?.block_on(cli_tunnel(target, forward, auth)),
    }
}

#[cfg(feature = "client")]
fn no_command() -> Result<()> {
    detach_console_if_owned();
    launcher(false, false)
}

#[cfg(not(feature = "client"))]
fn no_command() -> Result<()> {
    use clap::CommandFactory as _;
    Cli::command().print_help()?;
    Ok(())
}

#[cfg(feature = "client")]
fn launcher(minimized: bool, takeover: bool) -> Result<()> {
    let exe = std::env::current_exe().context("locating the rdesk binary")?;
    rdesk_client::app::run(rdesk_client::app::AppOpts { exe, minimized, takeover })
}

/// Started by double-clicking rdesk.exe, Windows gives us a console window of our own;
/// close it so only the launcher shows. Run from a terminal, the console is shared and stays.
#[cfg(all(windows, feature = "client"))]
fn detach_console_if_owned() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 2];
    // SAFETY: the buffer is valid for `pids.len()` entries.
    if unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) } == 1 {
        unsafe { FreeConsole() };
    }
}

#[cfg(all(not(windows), feature = "client"))]
fn detach_console_if_owned() {}

#[cfg(feature = "host")]
fn prompt(q: &str) -> Result<String> {
    eprint!("{q}");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

// ---------------------------------------------------------------------------
// Host commands
// ---------------------------------------------------------------------------

#[cfg(feature = "host")]
fn host_cmd(cmd: HostCmd) -> Result<()> {
    match cmd {
        HostCmd::Init { force } => {
            let had = HostIdentity::load().is_ok();
            let fp = if force {
                rdesk_host::admin::regenerate_identity()?
            } else {
                rdesk_host::admin::ensure_initialized()?
            };
            println!(
                "{}",
                if had && !force { "Host identity already exists." } else { "Generated a new host identity." }
            );
            println!("Config directory: {}", config::config_dir()?.display());
            println!("Host ID: {}", format_host_id(&fp));
            println!();
            println!("Next steps:");
            println!("  rdesk host add-key --from-agent      # authorize your 1Password SSH key");
            println!("  rdesk host set-password --op op://…  # optional password fallback");
            println!("  rdesk host run                       # or just `rdesk` for the app");
            Ok(())
        }
        HostCmd::Run { port, fake_capture, fake_input, agent } => {
            runtime()?.block_on(rdesk_host::run(rdesk_host::RunOpts {
                port,
                fake_capture: fake_capture || std::env::var_os("RDESK_FAKE_CAPTURE").is_some(),
                fake_input: fake_input || std::env::var_os("RDESK_FAKE_INPUT").is_some(),
                agent,
            }))
        }
        HostCmd::FileHelper(cmd) => file_helper(cmd),
        HostCmd::Id => {
            println!("{}", format_host_id(&HostIdentity::load()?.fingerprint()));
            Ok(())
        }
        HostCmd::AddKey { key, from_agent, key_fp, agent_sock } => {
            let key = if from_agent {
                runtime()?.block_on(pick_agent_key(agent_sock.as_deref(), key_fp.as_deref()))?
            } else {
                let k = key.ok_or_else(|| anyhow!("give a public key, a .pub file, or --from-agent"))?;
                rdesk_host::admin::parse_public_key(&k)?
            };
            let fp = agent::key_fingerprint(key.key_data());
            if rdesk_host::admin::add_key(&key)? {
                println!("Authorized {fp} {}", key.comment());
            } else {
                println!("Already authorized: {fp}");
            }
            Ok(())
        }
        HostCmd::ListKeys => {
            let keys = rdesk_host::admin::load_keys()?;
            if keys.is_empty() {
                println!("No authorized keys.");
            }
            for k in keys {
                println!("{} {} {}", agent::key_fingerprint(k.key_data()), k.algorithm(), k.comment());
            }
            Ok(())
        }
        HostCmd::RemoveKey { fingerprint } => {
            rdesk_host::admin::remove_key(&fingerprint)?;
            println!("Removed.");
            Ok(())
        }
        HostCmd::SetPassword { op } => {
            let pw: Zeroizing<String> = match op {
                Some(r) => runtime()?.block_on(agent::op_read(&r))?,
                None => {
                    let a = Zeroizing::new(rpassword::prompt_password("New rdesk password: ")?);
                    let b = Zeroizing::new(rpassword::prompt_password("Repeat: ")?);
                    if a != b {
                        bail!("passwords differ");
                    }
                    a
                }
            };
            if pw.chars().count() < 16 {
                eprintln!("warning: use a long random password (1Password can generate one); this one is short");
            }
            eprintln!("Deriving the password record (Argon2id, takes a moment)…");
            rdesk_host::admin::set_password(&pw)?;
            println!("Password login enabled.");
            Ok(())
        }
        HostCmd::ClearPassword => {
            rdesk_host::admin::clear_password()?;
            println!("Password login disabled.");
            Ok(())
        }
        HostCmd::Check => runtime()?.block_on(host_check()),
        HostCmd::Enroll { server, owner, key, name, clear } => {
            if clear {
                rdesk_host::enroll::save(Default::default())?;
                println!("This computer will no longer ask a server to add it.");
                return Ok(());
            }
            rdesk_host::enroll::save(config::EnrollConfig { server, owner, key, name })?;
            if rdesk_common::account::Registration::load().is_some() {
                println!("Saved, but this computer is already registered: unregister it first to join another way.");
            } else {
                println!("Saved. Start the host (`rdesk host run`, or the app): it asks the server to add it.");
            }
            Ok(())
        }
    }
}
#[cfg(feature = "host")]
async fn pick_agent_key(sock: Option<&str>, fp: Option<&str>) -> Result<PublicKey> {
    let mut a = Agent::connect(sock).await?;
    let keys = a.identities().await?;
    if keys.is_empty() {
        bail!("the agent at {} offers no keys", a.path);
    }
    if let Some(fp) = fp {
        return keys
            .into_iter()
            .find(|k| agent::key_fingerprint(k.key_data()) == fp)
            .ok_or_else(|| anyhow!("no agent key with fingerprint {fp}"));
    }
    if keys.len() == 1 {
        return Ok(keys.into_iter().next().unwrap());
    }
    for (i, k) in keys.iter().enumerate() {
        eprintln!("  [{}] {} {} {}", i + 1, agent::key_fingerprint(k.key_data()), k.algorithm(), k.comment());
    }
    let n: usize = prompt("Which key? ")?.parse().context("enter a number")?;
    keys.into_iter().nth(n.wrapping_sub(1)).ok_or_else(|| anyhow!("no such key"))
}

#[cfg(feature = "host")]
async fn host_check() -> Result<()> {
    for l in rdesk_host::admin::diagnose().await {
        println!("{:<14} {}{}", format!("{}:", l.label), if l.ok { "" } else { "⚠ " }, l.value);
    }
    if cfg!(windows) && !rdesk_host::elevation::is_elevated() {
        println!();
        println!("{}", rdesk_host::elevation::NOT_ELEVATED_HINT);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client commands
// ---------------------------------------------------------------------------

/// Find a computer by name, host ID (or its first characters) or server id.
fn find_machine<'a>(list: &'a [Machine], what: &str) -> Result<&'a Machine> {
    let w = what.trim().to_lowercase();
    let hits: Vec<&Machine> = list
        .iter()
        .filter(|m| {
            m.name.to_lowercase() == w || m.id.to_string() == w || (w.len() >= 8 && m.endpoint_id.starts_with(&w))
        })
        .collect();
    match hits.as_slice() {
        [m] => Ok(m),
        [] => bail!("no computer {what:?} that you can reach (see `rdesk machines list`)"),
        _ => bail!("{what:?} matches several computers: use its host ID"),
    }
}

fn vis_label(v: Visibility) -> &'static str {
    match v {
        Visibility::Private => "private",
        Visibility::Shared => "shared",
        Visibility::Everyone => "everyone",
    }
}

/// A waiting computer by its code (`3F2A-91C0`, any case, dash optional) or number.
fn find_request<'a>(list: &'a [MachineRequest], what: &str) -> Result<&'a MachineRequest> {
    let norm = |s: &str| s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_uppercase();
    let w = norm(what);
    list.iter()
        .find(|r| norm(&r.code) == w || r.id.to_string() == what.trim())
        .ok_or_else(|| anyhow!("no computer waiting with code {what:?} (see `rdesk machines requests`)"))
}

async fn machines_cmd(cmd: MachinesCmd) -> Result<()> {
    let s = session()?;
    let api = s.api()?;
    match cmd {
        MachinesCmd::Requests => {
            let list = api.machine_requests().await?;
            if list.is_empty() {
                println!("No computers waiting for your approval.");
            }
            for r in list {
                let owner = r.owner.map(|o| o.email).unwrap_or_else(|| "(admins)".into());
                println!("{}\t{}\t{}\t{owner}", r.code, r.name, r.os);
            }
        }
        MachinesCmd::Approve { request, name, visibility } => {
            let list = api.machine_requests().await?;
            let r = find_request(&list, &request)?;
            let m = api.approve_machine(r.id, name, visibility.into()).await?;
            println!("{} is yours now ({}); it comes online in a few seconds.", m.name, vis_label(m.visibility));
        }
        MachinesCmd::Refuse { request } => {
            let list = api.machine_requests().await?;
            let r = find_request(&list, &request)?;
            api.reject_machine(r.id).await?;
            println!("Refused {}.", r.name);
        }
        MachinesCmd::Keys(KeysCmd::List) => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
            for k in api.enroll_keys().await? {
                let state = if k.revoked {
                    "revoked".to_string()
                } else if k.expires_at <= now {
                    "expired".into()
                } else if !k.reusable && k.uses > 0 {
                    "used".into()
                } else {
                    format!("{} day(s) left", (k.expires_at - now + 86_399) / 86_400)
                };
                let uses = if k.reusable { "reusable" } else { "single use" };
                println!(
                    "{}\t{}…\t{}\t{uses}, {} computer(s)\t{state}",
                    k.name,
                    k.prefix,
                    vis_label(k.visibility),
                    k.uses
                );
            }
        }
        MachinesCmd::Keys(KeysCmd::Create { name, days, once, visibility, groups }) => {
            let all_groups = api.all_groups().await?;
            let mut gids = Vec::new();
            for spec in &groups {
                let (g, role) = split_role(spec)?;
                let found = all_groups.iter().find(|x| x.name.eq_ignore_ascii_case(&g));
                gids.push((found.ok_or_else(|| anyhow!("no group {g}"))?.id, role));
            }
            let visibility = if gids.is_empty() { visibility.into() } else { Visibility::Shared };
            let req = NewEnrollKey {
                name,
                reusable: !once,
                expires_in_days: days,
                visibility,
                shares: SetShares { users: Vec::new(), groups: gids },
            };
            let c = api.create_enroll_key(&req).await?;
            println!("{}", c.key);
            eprintln!(
                "Shown only now. On the computers to add (or in their service's environment):\n  \
                 RDESK_SERVER={} RDESK_ENROLL_KEY={}\nor: rdesk host enroll --server {} --key {}",
                s.server, c.key, s.server, c.key
            );
        }
        MachinesCmd::Keys(KeysCmd::Revoke { key }) => {
            let keys = api.enroll_keys().await?;
            let w = key.trim();
            let k = keys
                .iter()
                .find(|k| k.name.eq_ignore_ascii_case(w) || k.id.to_string() == w || w.starts_with(&k.prefix))
                .ok_or_else(|| anyhow!("no key {key:?} (see `rdesk machines keys list`)"))?;
            api.revoke_enroll_key(k.id).await?;
            println!("Revoked {}.", k.name);
        }
        MachinesCmd::List => {
            let list = api.machines().await?;
            if list.is_empty() {
                println!("No computers yet. Register one with `rdesk machines register` on it.");
            }
            for m in list {
                let state = if m.in_session {
                    "in use"
                } else if m.online {
                    "online"
                } else {
                    "offline"
                };
                let owner = if m.is_owner { "yours".to_string() } else { format!("by {}", m.owner.email) };
                println!(
                    "{}\t{state}\t{}\t{owner}\t{}{}",
                    m.name,
                    vis_label(m.visibility),
                    &m.endpoint_id[..16],
                    if m.can_access { "" } else { "\t(no access)" }
                );
            }
        }
        #[cfg(feature = "host")]
        MachinesCmd::Register { name, visibility } => {
            let name = name.unwrap_or_else(rdesk_common::local_hostname);
            let r = rdesk_host::admin::register(&s, &name, visibility.into()).await?;
            println!("Registered as {} on {} ({}).", r.name, r.server_name, vis_label(visibility.into()));
            println!("Restart the host (or turn it off and on in the app) to use the server's relay.");
        }
        #[cfg(feature = "host")]
        MachinesCmd::Unregister => {
            rdesk_host::admin::unregister(&s).await?;
            println!("Unregistered. Restart the host.");
        }
        MachinesCmd::Rename { machine, name } => {
            let list = api.machines().await?;
            let m = find_machine(&list, &machine)?;
            let change = UpdateMachine { name: Some(name), visibility: None };
            println!("Renamed to {}.", api.update_machine(m.id, &change).await?.name);
        }
        MachinesCmd::Visibility { machine, visibility } => {
            let list = api.machines().await?;
            let m = find_machine(&list, &machine)?;
            let change = UpdateMachine { name: None, visibility: Some(visibility.into()) };
            let m = api.update_machine(m.id, &change).await?;
            println!("{}: {}.", m.name, vis_label(m.visibility));
        }
        MachinesCmd::Shares { machine } => {
            let list = api.machines().await?;
            let m = find_machine(&list, &machine)?;
            let sh = api.shares(m.id).await?;
            println!("{} ({}):", m.name, vis_label(m.visibility));
            for (u, r) in sh.users {
                println!("  user  {} ({})", u.email, r.label());
            }
            for (g, r) in sh.groups {
                println!("  group {} ({})", g.name, r.label());
            }
        }
        MachinesCmd::Share { machine, users, groups } => {
            let list = api.machines().await?;
            let m = find_machine(&list, &machine)?;
            let dir = api.directory().await?;
            let all_groups = api.all_groups().await?;
            let mut uids = Vec::new();
            for spec in &users {
                let (e, role) = split_role(spec)?;
                let e = e.to_lowercase();
                uids.push((dir.iter().find(|u| u.email == e).ok_or_else(|| anyhow!("no user {e}"))?.id, role));
            }
            let mut gids = Vec::new();
            for spec in &groups {
                let (g, role) = split_role(spec)?;
                let found = all_groups.iter().find(|x| x.name.eq_ignore_ascii_case(&g));
                gids.push((found.ok_or_else(|| anyhow!("no group {g}"))?.id, role));
            }
            api.set_shares(m.id, uids, gids).await?;
            if m.visibility != Visibility::Shared {
                let change = UpdateMachine { name: None, visibility: Some(Visibility::Shared) };
                api.update_machine(m.id, &change).await?;
            }
            println!("{} is shared with {} user(s) and {} group(s).", m.name, users.len(), groups.len());
        }
        MachinesCmd::Managers { machine, set } => {
            let list = api.machines().await?;
            let m = find_machine(&list, &machine)?;
            let current = match set {
                Some(emails) => {
                    let dir = api.directory().await?;
                    let mut ids = Vec::new();
                    for e in emails {
                        let e = e.trim().to_lowercase();
                        ids.push(dir.iter().find(|u| u.email == e).ok_or_else(|| anyhow!("no user {e}"))?.id);
                    }
                    api.set_managers(m.id, ids).await?
                }
                None => api.managers(m.id).await?,
            };
            println!("{} is managed by {} and:", m.name, m.owner.email);
            for u in current.users {
                println!("  {}", u.email);
            }
        }
        MachinesCmd::Wake { machine } => {
            let list = api.machines().await?;
            let m = find_machine(&list, &machine)?;
            let r = api.wake(m.id).await?;
            println!("asked {} computer(s) on its network to wake {}", r.helpers, m.name);
        }
        MachinesCmd::History { machine } => {
            let list = api.machines().await?;
            let m = find_machine(&list, &machine)?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
            for h in api.history(m.id).await? {
                let len = match h.ended_at {
                    Some(t) => format!("{} min", ((t - h.started_at).max(0) + 59) / 60),
                    None => "still connected".into(),
                };
                println!(
                    "{}\t{} ({})\t{len}\t{}\t{}",
                    rdesk_common::directory::ago(h.started_at, now),
                    h.who,
                    h.client,
                    h.access,
                    h.method
                );
            }
        }
    }
    Ok(())
}

/// `name[:control|view|files]`.
fn split_role(spec: &str) -> Result<(String, ShareRole)> {
    match spec.rsplit_once(':') {
        Some((name, role)) => match ShareRole::parse(role.trim()) {
            Some(r) => Ok((name.trim().to_string(), r)),
            None => bail!("{role}: use control, view or files"),
        },
        None => Ok((spec.trim().to_string(), ShareRole::Control)),
    }
}

async fn groups_cmd(cmd: GroupsCmd) -> Result<()> {
    let api = session()?.api()?;
    let find = async |name: &str| -> Result<rdesk_proto::api::Group> {
        api.groups()
            .await?
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| anyhow!("you are not in a group called {name:?}"))
    };
    let member = |g: &rdesk_proto::api::Group, email: &str| -> Result<i64> {
        let e = email.trim().to_lowercase();
        g.members
            .iter()
            .find(|m| m.user.email == e)
            .map(|m| m.user.id)
            .ok_or_else(|| anyhow!("{e} is not in {}", g.name))
    };
    match cmd {
        GroupsCmd::List => {
            for g in api.groups().await? {
                let mine = match g.my_role {
                    Some(GroupRole::Admin) => " (you: admin)",
                    Some(GroupRole::Member) => " (you: member)",
                    None => "",
                };
                println!("{}{mine}", g.name);
                for m in g.members {
                    println!("  {}{}", m.user.email, if m.role == GroupRole::Admin { " (admin)" } else { "" });
                }
            }
        }
        GroupsCmd::Create { name } => println!("Created {}.", api.create_group(&name).await?.name),
        GroupsCmd::Rename { group, name } => {
            let g = find(&group).await?;
            println!("Renamed to {}.", api.rename_group(g.id, &name).await?.name);
        }
        GroupsCmd::Delete { group } => {
            let g = find(&group).await?;
            api.delete_group(g.id).await?;
            println!("Deleted {}.", g.name);
        }
        GroupsCmd::Add { group, email, admin } => {
            let g = find(&group).await?;
            let role = if admin { GroupRole::Admin } else { GroupRole::Member };
            api.add_member(g.id, &email, role).await?;
            println!("{} is in {}.", email.trim(), g.name);
        }
        GroupsCmd::Role { group, email, role } => {
            let g = find(&group).await?;
            let role = if role == "admin" { GroupRole::Admin } else { GroupRole::Member };
            api.set_member_role(g.id, member(&g, &email)?, role).await?;
            println!("Done.");
        }
        GroupsCmd::Remove { group, email } => {
            let g = find(&group).await?;
            api.remove_member(g.id, member(&g, &email)?).await?;
            println!("Removed {} from {}.", email.trim(), g.name);
        }
        GroupsCmd::Leave { group } => {
            let g = find(&group).await?;
            let me = session()?.user.id;
            api.remove_member(g.id, me).await?;
            println!("You left {}.", g.name);
        }
    }
    Ok(())
}

/// `rdesk update`: the released version, checked against the release keys built in here.
async fn update(server: &str, machine: Option<String>, check: bool) -> Result<()> {
    use rdesk_common::release::{check_file, download, file_for, this_platform, verify_manifest};
    use rdesk_proto::release::{FileKind, parse_version};
    let base = account::normalize_server(server)?;
    let mut url = format!("{base}{}", rdesk_proto::api::PATH_UPDATE_LATEST);
    if let Some(m) = machine.filter(|m| !m.trim().is_empty()) {
        url.push_str(&format!("?machine={}", m.trim()));
    }
    let offer: Option<rdesk_proto::api::UpdateOffer> = serde_json::from_slice(&download(&url, 1 << 20).await?)?;
    let Some(offer) = offer else {
        println!("Nothing released on {base} yet.");
        return Ok(());
    };
    let mine = env!("CARGO_PKG_VERSION");
    if parse_version(&offer.version) <= parse_version(mine) {
        println!("Up to date ({mine}; released: {}).", offer.version);
        return Ok(());
    }
    let m = verify_manifest(offer.manifest.as_bytes(), &offer.signature)?;
    if m.version != offer.version {
        bail!("the signed manifest is for {}, not {}", m.version, offer.version);
    }
    if check {
        println!("rdesk {} can be installed (this is {mine}).", m.version);
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        bail!(
            "rdesk update installs Debian packages (Linux); on Windows the service and on a Mac the app update by themselves"
        );
    }
    let platform = this_platform();
    let file =
        file_for(&m, FileKind::Deb, &platform).ok_or_else(|| anyhow!("{} has no package for {platform}", m.version))?;
    let data = download(&format!("{}{}", offer.download_base, file.name), file.size).await?;
    check_file(file, &data)?;
    let dir = std::path::Path::new("/var/cache/rdesk");
    std::fs::create_dir_all(dir)?;
    let deb = dir.join(&file.name);
    std::fs::write(&deb, &data)?;
    let status = std::process::Command::new("dpkg").arg("-i").arg(&deb).status()?;
    if !status.success() {
        bail!("dpkg -i {} failed", deb.display());
    }
    let _ = std::fs::remove_file(&deb);
    println!("Installed rdesk {} (hosts restart when nobody is connected).", m.version);
    Ok(())
}

/// The saved session, or an error telling how to get one.
fn session() -> Result<Session> {
    Session::load().ok_or_else(|| match Session::active_key().and_then(|k| Session::find(&k).ok()) {
        Some(s) => anyhow!("the sign-in to {} ended: run `rdesk login {}`", s.label(), s.server),
        None => anyhow!("not signed in: run `rdesk login <server>` (or `rdesk server use`)"),
    })
}

fn server_cmd(c: ServerCmd) -> Result<()> {
    match c {
        ServerCmd::List => {
            let all = Session::all();
            if all.is_empty() {
                println!("No accounts. Run `rdesk login <server>`.");
                return Ok(());
            }
            let active = Session::active_key();
            for (i, s) in all.iter().enumerate() {
                let mark = if active.as_deref() == Some(s.key().as_str()) { "*" } else { " " };
                let state =
                    if s.expired() { "sign in again".to_string() } else { format!("{} more day(s)", s.days_left()) };
                println!("{mark} {}  {}  {}  ({}, {state})", i + 1, s.server_name, s.user.email, s.server);
            }
            Ok(())
        }
        ServerCmd::Use { account } => {
            let s = Session::find(&account)?;
            Session::activate(&s.key())?;
            if s.expired() {
                println!("Using {}. Its sign-in ended: run `rdesk login {}`.", s.label(), s.server);
            } else {
                println!("Using {}.", s.label());
            }
            Ok(())
        }
    }
}

async fn login(server: Option<String>) -> Result<()> {
    let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    let server = match server.or_else(|| cfg.server.clone()) {
        Some(s) => account::normalize_server(&s)?,
        None => bail!("which server? `rdesk login rdesk.example.com`"),
    };
    eprintln!("Opening the browser to sign in to {server}…");
    let s = account::login(&server, &rdesk_common::local_hostname(), |url| {
        if account::open_browser(url).is_err() {
            eprintln!("Open this address in your browser:\n  {url}");
        }
        Ok(())
    })
    .await?;
    config::ensure_config_dir()?;
    cfg.server = Some(s.server.clone());
    config::save_toml(config::CLIENT_CONFIG, &cfg)?;
    println!("Signed in to {} as {} (for {} days).", s.server_name, s.user.email, s.days_left());
    Ok(())
}

async fn logout() -> Result<()> {
    let Some(s) = Session::load() else {
        // Its sign-in had already ended: just forget it.
        match Session::active_key().and_then(|k| Session::find(&k).ok()) {
            Some(s) => {
                Session::remove(&s.key())?;
                println!("Removed {}.", s.label());
            }
            None => println!("Not signed in."),
        }
        return Ok(());
    };
    if let Err(e) = s.api()?.logout().await {
        eprintln!("warning: the server did not confirm ({e:#}); the local session is removed anyway");
    }
    Session::remove(&s.key())?;
    println!("Signed out of {}.", s.label());
    Ok(())
}

async fn whoami() -> Result<()> {
    use rdesk_common::transport::{self, Role};
    let Some(s) = Session::load() else {
        println!("Not signed in. Run `rdesk login <server>`.");
        return Ok(());
    };
    let others = Session::all().len().saturating_sub(1);
    if others > 0 {
        println!("{others} other saved account(s): `rdesk server list`.");
    }
    match s.api()?.me().await {
        Ok(me) => println!(
            "{} on {} ({}){}, {} more day(s)",
            me.email,
            s.server_name,
            s.server,
            if me.is_admin { ", system admin" } else { "" },
            s.days_left()
        ),
        Err(e) if e.downcast_ref::<account::NotSignedIn>().is_some() => {
            Session::end(&s.key())?;
            bail!("the server ended this session ({e:#}): run `rdesk login {}` again", s.server);
        }
        Err(e) => println!("{} on {} (server not reachable: {e:#})", s.user.email, s.server),
    }
    let net = s.network();
    let ep = transport::bind(None, &net, &Default::default(), Role::Client).await?;
    let r = transport::wait_relay(&ep, &net, std::time::Duration::from_secs(8)).await;
    ep.close().await;
    match r {
        Ok(_) => println!("Relay: {} (reachable)", s.relay_url),
        Err(e) => println!("Relay: {} — {e:#}", s.relay_url),
    }
    Ok(())
}

async fn users_cmd(cmd: UsersCmd) -> Result<()> {
    let api = session()?.api()?;
    let find = async |email: &str| -> Result<rdesk_proto::api::AdminUser> {
        let e = email.trim().to_lowercase();
        api.users().await?.into_iter().find(|u| u.email == e).ok_or_else(|| anyhow!("{email} is not on the list"))
    };
    match cmd {
        UsersCmd::List => {
            for u in api.users().await? {
                let twofa = match (u.has_totp, u.passkeys) {
                    (false, 0) => "2FA not set up yet".to_string(),
                    (t, n) => format!(
                        "2FA: {}{}",
                        if t { "TOTP " } else { "" },
                        if n > 0 { format!("{n} passkey(s)") } else { String::new() }
                    ),
                };
                println!(
                    "{}{}{}\t{twofa}",
                    u.email,
                    if u.is_admin { " (admin)" } else { "" },
                    if u.disabled { " (disabled)" } else { "" }
                );
            }
        }
        UsersCmd::Add { email, admin } => {
            let u = api.add_user(&email, admin).await?;
            println!("{} can now sign in.", u.email);
        }
        UsersCmd::Remove { email } => {
            let u = find(&email).await?;
            api.remove_user(u.id).await?;
            println!("Removed {}.", u.email);
        }
        UsersCmd::Admin { email, state } => {
            let u = find(&email).await?;
            let change = rdesk_proto::api::UpdateUser { is_admin: Some(state == "on"), ..Default::default() };
            api.update_user(u.id, &change).await?;
            println!("{}: admin {state}.", u.email);
        }
        UsersCmd::Disable { email, state } => {
            let u = find(&email).await?;
            let change = rdesk_proto::api::UpdateUser { disabled: Some(state == "on"), ..Default::default() };
            api.update_user(u.id, &change).await?;
            println!("{}: disabled {state}.", u.email);
        }
        UsersCmd::Reset2fa { email } => {
            let u = find(&email).await?;
            let change = rdesk_proto::api::UpdateUser { reset_second_factor: true, ..Default::default() };
            let after = api.update_user(u.id, &change).await?;
            if after.has_totp || after.passkeys > 0 {
                bail!("the server kept {}'s second factor: update the server first", u.email);
            }
            println!("{}: second factor reset; they set up a new one at their next sign-in.", u.email);
        }
    }
    Ok(())
}

async fn agent_keys(sock: Option<String>) -> Result<()> {
    let mut a = Agent::connect(sock.as_deref()).await?;
    println!("Agent: {}", a.path);
    for k in a.identities().await? {
        println!("{} {} {}", agent::key_fingerprint(k.key_data()), k.algorithm(), k.comment());
        println!("    {}", k.to_openssh()?.trim());
    }
    Ok(())
}

#[cfg(feature = "client")]
struct Target {
    name: String,
    entry: HostEntry,
    id: [u8; 32],
    machine: Option<rdesk_client::conn::MachineTarget>,
    /// The account it was found with ("server · e-mail"), for the window's title.
    account: Option<String>,
    cfg: ClientConfig,
    net: NetworkConfig,
}

/// `target` is a computer's name on the server, its host ID (or the start of it), or
/// `ID@address` for an unregistered computer on the LAN.
#[cfg(feature = "client")]
async fn resolve_target(target: &str, a: &AuthArgs) -> Result<Target> {
    let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    // Saved on this device to reach without the server (by name, or the start of its ID).
    let wanted = target.trim().to_lowercase();
    let local = cfg.hosts.values().find(|e| {
        !e.name.is_empty() && (e.name.to_lowercase() == wanted || (wanted.len() >= 8 && e.id.starts_with(&wanted)))
    });
    let mut via: Option<Session> = None;
    let (name, id_hex, addr, machine) = match target.split_once('@') {
        // Direct, without the server (the computer must allow it: local network, own login).
        Some((id, addr)) => {
            parse_host_id(id).with_context(|| format!("{target}: expected ID@address"))?;
            (id.chars().take(12).collect::<String>(), id.to_string(), addr.to_string(), None)
        }
        None if let Some(e) = local => (e.name.clone(), e.id.clone(), e.addr.clone(), None),
        // A whole host ID, not signed in: look for it on the local network (mDNS).
        None if Session::load().is_none() && parse_host_id(target.trim()).is_ok() => {
            (target.trim().chars().take(12).collect(), target.trim().to_string(), String::new(), None)
        }
        None => {
            // A computer on your server: needs you signed in there.
            let s = session()?;
            let list = s.api()?.machines().await?;
            let m = find_machine(&list, target)?;
            if !m.can_access {
                bail!("{} is not shared with you", m.name);
            }
            let target = rdesk_client::conn::MachineTarget { id: m.id, addrs: m.addrs.clone(), account: Some(s.key()) };
            via = Some(s);
            (m.name.clone(), m.endpoint_id.clone(), String::new(), Some(target))
        }
    };
    let id = parse_host_id(&id_hex)?;
    // Login options for this computer's extra lock (password / SSH key), by host ID.
    let mut entry = cfg.hosts.get(&format_host_id(&id)).cloned().unwrap_or_default();
    entry.id = format_host_id(&id);
    if !addr.is_empty() {
        entry.addr = addr;
    }
    if let Some(x) = a.auth {
        entry.auth = x.into();
    }
    if a.key.is_some() {
        entry.key = a.key.clone();
    }
    if a.password_ref.is_some() {
        entry.password_ref = a.password_ref.clone();
    }
    if a.agent_sock.is_some() {
        entry.agent_socket = a.agent_sock.clone();
    }
    if a.identity.is_some() {
        entry.identity = a.identity.clone();
    }
    // Without the server: no relay, the address or the local network (mDNS) only.
    let net = via.as_ref().map(Session::network).unwrap_or_default();
    let account = via.map(|s| s.label());
    Ok(Target { name, entry, id, machine, account, cfg, net })
}

#[cfg(feature = "client")]
fn connect(a: ConnectArgs) -> Result<()> {
    use rdesk_client::conn::ConnectOpts;
    let t = runtime()?.block_on(resolve_target(&a.target, &a.auth))?;
    let opts = ConnectOpts {
        name: t.name.clone(),
        // The window asks in a dialog; --headless asks on the terminal.
        ask_password_in_ui: !a.headless,
        entry: t.entry.clone(),
        id: t.id,
        machine: t.machine.clone(),
        monitor: a.monitor,
        transport: t.cfg.transport.clone(),
        net: t.net.clone(),
        clipboard: !a.no_clipboard,
        // A terminal opens without the picture (and the host does not capture the screen)
        // until the window asks for it.
        video: a.headless || !a.terminal,
        // The window adapts the picture to the link and shows the host's pointer.
        extensions: rdesk_client::conn::Extensions {
            auto_quality: !a.headless,
            cursor_shape: !a.headless,
            ..Default::default()
        },
        device_key: None,
        audio: None,
    };
    if a.headless {
        return rdesk_client::headless::run(
            opts,
            rdesk_client::headless::HeadlessOpts {
                frames: a.frames,
                timeout: Duration::from_secs(a.timeout),
                dump: a.dump,
            },
        );
    }
    // The session window speaks the app's language.
    rdesk_common::i18n::init_from_config();
    let swap = a.swap_cmd_ctrl || t.cfg.swap_cmd_ctrl || (cfg!(target_os = "macos") && !a.no_swap_cmd_ctrl);
    let download_dir = directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    rdesk_client::viewer::run(rdesk_client::viewer::ViewerOpts {
        connect: opts,
        swap_cmd_ctrl: swap,
        download_dir,
        open_files: a.files,
        open_terminal: a.terminal,
        control_stdin: a.controlled,
        account: t.account,
    })
}

/// This terminal in raw mode (keys go through as they are), restored when dropped.
#[cfg(feature = "client")]
struct RawTerminal {
    #[cfg(windows)]
    modes: (u32, u32),
}

#[cfg(feature = "client")]
impl RawTerminal {
    fn enable() -> Result<Self> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{
                ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
                ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                SetConsoleMode,
            };
            unsafe {
                let (i, o) = (GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE));
                let (mut mi, mut mo) = (0u32, 0u32);
                if GetConsoleMode(i, &mut mi) == 0 || GetConsoleMode(o, &mut mo) == 0 {
                    bail!("rdesk shell needs a console");
                }
                SetConsoleMode(
                    i,
                    (mi & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
                        | ENABLE_VIRTUAL_TERMINAL_INPUT,
                );
                SetConsoleMode(o, mo | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                Ok(Self { modes: (mi, mo) })
            }
        }
        #[cfg(not(windows))]
        {
            crossterm::terminal::enable_raw_mode()?;
            Ok(Self {})
        }
    }
}

#[cfg(feature = "client")]
impl Drop for RawTerminal {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Console::{
                GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
            };
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.modes.0);
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), self.modes.1);
        }
        #[cfg(not(windows))]
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// `rdesk shell`: the computer's shell in this terminal.
#[cfg(feature = "client")]
async fn cli_tunnel(target: String, forward: Vec<String>, a: AuthArgs) -> Result<()> {
    let specs: Vec<rdesk_client::tunnel::TunnelSpec> =
        forward.iter().map(|f| rdesk_client::tunnel::TunnelSpec::parse(f)).collect::<Result<_>>()?;
    let t = resolve_target(&target, &a).await?;
    let s = cli_session(&t).await?;
    let mut tasks = tokio::task::JoinSet::new();
    for spec in specs {
        eprintln!("localhost:{} -> {}:{} (from {})", spec.local, spec.host, spec.port, t.name);
        tasks.spawn(rdesk_client::tunnel::forward(s.conn.clone(), spec, |e| eprintln!("tunnel: {e}")));
    }
    eprintln!("Ctrl+C to stop.");
    let r = tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        r = tasks.join_next() => match r {
            Some(Ok(Err(e))) => Err(e),
            _ => Ok(()),
        },
        e = s.conn.closed() => Err(anyhow::anyhow!("connection closed: {e}")),
    };
    s.close().await;
    r
}

#[cfg(feature = "client")]
async fn cli_shell(target: String, admin: bool, a: AuthArgs) -> Result<()> {
    use rdesk_proto::{TermClient, TermHost};
    use std::io::Read as _;
    let t = resolve_target(&target, &a).await?;
    let s = cli_session(&t).await?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let mut link = rdesk_client::terminal::open(s.conn.clone(), cols, rows, admin, || {}).await?;
    let raw = RawTerminal::enable()?;
    let keys = link.tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || keys.send(TermClient::Input(buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });
    let sizes = link.tx.clone();
    tokio::spawn(async move {
        let mut last = (cols, rows);
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Ok(now) = crossterm::terminal::size()
                && now != last
            {
                last = now;
                if sizes.send(TermClient::Resize { cols: now.0, rows: now.1 }).is_err() {
                    break;
                }
            }
        }
    });
    let mut out = std::io::stdout();
    let mut ended = Ok(0u32);
    while let Some(m) = link.rx.recv().await {
        match m {
            TermHost::Output(b) => {
                out.write_all(&b)?;
                out.flush()?;
            }
            TermHost::Started { .. } => {}
            TermHost::Exited(code) => {
                ended = Ok(code);
                break;
            }
            TermHost::Error(e) => {
                ended = Err(e);
                break;
            }
        }
    }
    drop(raw);
    s.close().await;
    match ended {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code as i32),
        Err(e) => bail!(e),
    }
}

/// A logged-in connection for one-shot CLI commands (no video/UI machinery).
#[cfg(feature = "client")]
struct CliSession {
    ep: transport::Endpoint,
    conn: transport::Connection,
    w: transport::SendStream,
    r: transport::RecvStream,
}

#[cfg(feature = "client")]
impl CliSession {
    async fn close(self) {
        self.conn.close(transport::close::NORMAL, b"done");
        let _ = tokio::time::timeout(Duration::from_secs(3), self.ep.close()).await;
    }
}

#[cfg(feature = "client")]
async fn cli_session(t: &Target) -> Result<CliSession> {
    let ep = transport::bind(None, &t.net, &t.cfg.transport, transport::Role::Client).await?;
    if t.machine.is_none()
        && let Err(e) = transport::local_discovery(&ep, false)
    {
        tracing::debug!("{e:#}");
    }
    match cli_login(t, &ep).await {
        Ok((conn, w, r)) => Ok(CliSession { ep, conn, w, r }),
        Err(e) => {
            let _ = tokio::time::timeout(Duration::from_secs(3), ep.close()).await;
            Err(e)
        }
    }
}

#[cfg(feature = "client")]
async fn cli_login(
    t: &Target,
    ep: &transport::Endpoint,
) -> Result<(transport::Connection, transport::SendStream, transport::RecvStream)> {
    let ticket = match &t.machine {
        Some(m) => {
            Some(rdesk_client::conn::get_ticket(m, ep.id().as_bytes(), &mut None).await.map_err(|(_, e)| anyhow!(e))?)
        }
        None => None,
    };
    let mut direct: Vec<String> = t.machine.as_ref().map(|m| m.addrs.clone()).unwrap_or_default();
    if !t.entry.addr.is_empty() {
        direct.insert(0, t.entry.addr.clone());
    }
    let c = transport::connect_with(ep, t.id, &direct, &t.net, Duration::from_secs(10)).await?;
    let (mut w, mut r) = c.conn.open_bi().await?;
    let used = std::sync::Arc::new(std::sync::Mutex::new(None));
    let asker = rdesk_client::conn::password_asker(&t.name, &t.entry, used.clone(), None);
    let mut creds = rdesk_client::conn::credentials(&t.entry, None, asker).await;
    creds.ticket = ticket;
    auth::client_handshake(&mut r, &mut w, c.ek, c.host_fingerprint, &mut creds).await?;
    // We don't watch the screen: ask for the minimum.
    rdesk_proto::write_msg(&mut w, &rdesk_proto::ClientMsg::SetQuality { max_fps: 1, jpeg_quality: 10 }).await?;
    Ok((c.conn, w, r))
}

#[cfg(feature = "client")]
fn progress_printer(name: String) -> impl FnMut(u64, u64) + Send {
    let mut last = std::time::Instant::now();
    move |done, total| {
        if last.elapsed() > Duration::from_millis(250) || done == total {
            last = std::time::Instant::now();
            let pct = (done * 100).checked_div(total).unwrap_or(100);
            eprint!("\r{name}: {pct}% ({done}/{total} bytes)   ");
        }
    }
}

#[cfg(feature = "client")]
async fn cli_send(target: String, files: Vec<PathBuf>, a: AuthArgs) -> Result<()> {
    let t = resolve_target(&target, &a).await?;
    let s = cli_session(&t).await?;
    for f in files {
        let mut p = progress_printer(f.display().to_string());
        let saved = rdesk_common::files::push(&s.conn, &f, &mut p).await?;
        eprintln!();
        println!("{} → host inbox as {saved}", f.display());
    }
    s.close().await;
    Ok(())
}

#[cfg(feature = "client")]
async fn cli_get(target: String, remote: String, out: PathBuf, a: AuthArgs) -> Result<()> {
    let t = resolve_target(&target, &a).await?;
    let s = cli_session(&t).await?;
    let mut p = progress_printer(remote.clone());
    let path = rdesk_common::files::pull(&s.conn, &remote, &out, &mut p).await?;
    eprintln!();
    println!("saved {}", path.display());
    s.close().await;
    Ok(())
}

#[cfg(feature = "client")]
async fn cli_ls(target: String, path: String, a: AuthArgs) -> Result<()> {
    use rdesk_proto::{ClientMsg, HostMsg};
    let t = resolve_target(&target, &a).await?;
    let mut s = cli_session(&t).await?;
    rdesk_proto::write_msg(&mut s.w, &ClientMsg::ListDir { req: 1, path }).await?;
    loop {
        match rdesk_proto::expect_msg::<_, HostMsg>(&mut s.r, rdesk_proto::MAX_CONTROL_MSG).await? {
            HostMsg::DirListing { path, result, .. } => {
                println!("{path}");
                for e in result.map_err(|e| anyhow!(e))? {
                    if e.is_dir {
                        println!("  {}/", e.name);
                    } else {
                        println!("  {}  ({} bytes)", e.name, e.size);
                    }
                }
                break;
            }
            _ => continue,
        }
    }
    s.close().await;
    Ok(())
}
