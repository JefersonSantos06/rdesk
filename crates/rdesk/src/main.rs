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
    /// Sign in to your rdesk server (opens the browser: Google/GitHub + second factor).
    Login {
        /// Server address, e.g. rdesk.example.com (default: the last one used).
        server: Option<String>,
    },
    /// Sign out (ends this computer's session on the server).
    Logout,
    /// Who is signed in, until when, and whether the relay answers.
    Whoami,
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
    },
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

/// The desktop app usually has no console: log to `<config>/logs/app.log` instead.
fn init_app_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;
    let file = config::ensure_config_dir()
        .map(|d| d.join("logs"))
        .and_then(|d| std::fs::create_dir_all(&d).map(|_| d).map_err(Into::into))
        .and_then(|d| {
            // Append: a second launch (which only wakes the running app) must not clobber it.
            let path = d.join("app.log");
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
    if app {
        init_app_logging(cli.verbose)
    } else {
        init_logging(cli.verbose)
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
        #[cfg(feature = "client")]
        Command::Connect(a) => connect(a),
        Command::Machines(m) => runtime()?.block_on(machines_cmd(m)),
        Command::Groups(g) => runtime()?.block_on(groups_cmd(g)),
        Command::Login { server } => runtime()?.block_on(login(server)),
        Command::Logout => runtime()?.block_on(logout()),
        Command::Whoami => runtime()?.block_on(whoami()),
        Command::Users(u) => runtime()?.block_on(users_cmd(u)),
        Command::AgentKeys { agent_sock } => runtime()?.block_on(agent_keys(agent_sock)),
        #[cfg(feature = "client")]
        Command::Send { target, files, auth } => runtime()?.block_on(cli_send(target, files, auth)),
        #[cfg(feature = "client")]
        Command::Get { target, remote, out, auth } => runtime()?.block_on(cli_get(target, remote, out, auth)),
        #[cfg(feature = "client")]
        Command::Ls { target, path, auth } => runtime()?.block_on(cli_ls(target, path, auth)),
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
        HostCmd::Run { port, fake_capture, fake_input } => runtime()?.block_on(rdesk_host::run(rdesk_host::RunOpts {
            port,
            fake_capture: fake_capture || std::env::var_os("RDESK_FAKE_CAPTURE").is_some(),
            fake_input: fake_input || std::env::var_os("RDESK_FAKE_INPUT").is_some(),
        })),
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

/// The saved session, or an error telling how to get one.
fn session() -> Result<Session> {
    Session::load().ok_or_else(|| anyhow!("not signed in: run `rdesk login <server>`"))
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
        println!("Not signed in.");
        return Ok(());
    };
    if let Err(e) = s.api()?.logout().await {
        eprintln!("warning: the server did not confirm ({e:#}); the local session is removed anyway");
    }
    Session::clear()?;
    println!("Signed out of {}.", s.server_name);
    Ok(())
}

async fn whoami() -> Result<()> {
    use rdesk_common::transport::{self, Role};
    let Some(s) = Session::load() else {
        println!("Not signed in. Run `rdesk login <server>`.");
        return Ok(());
    };
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
            Session::clear()?;
            bail!("the server ended this session ({e:#}): run `rdesk login` again");
        }
        Err(e) => println!("{} on {} (server not reachable: {e:#})", s.user.email, s.server),
    }
    let net = config::load_network();
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
            let change = rdesk_proto::api::UpdateUser { is_admin: Some(state == "on"), disabled: None };
            api.update_user(u.id, &change).await?;
            println!("{}: admin {state}.", u.email);
        }
        UsersCmd::Disable { email, state } => {
            let u = find(&email).await?;
            let change = rdesk_proto::api::UpdateUser { is_admin: None, disabled: Some(state == "on") };
            api.update_user(u.id, &change).await?;
            println!("{}: disabled {state}.", u.email);
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
    cfg: ClientConfig,
    net: NetworkConfig,
}

/// `target` is a computer's name on the server, its host ID (or the start of it), or
/// `ID@address` for an unregistered computer on the LAN.
#[cfg(feature = "client")]
async fn resolve_target(target: &str, a: &AuthArgs) -> Result<Target> {
    // Using rdesk needs an account on your server.
    let s = session()?;
    let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    let (name, id_hex, addr, machine) = match target.split_once('@') {
        Some((id, addr)) => {
            parse_host_id(id).with_context(|| format!("{target}: expected ID@address"))?;
            (id.chars().take(12).collect::<String>(), id.to_string(), addr.to_string(), None)
        }
        None => {
            let list = s.api()?.machines().await?;
            let m = find_machine(&list, target)?;
            if !m.can_access {
                bail!("{} is not shared with you", m.name);
            }
            let target = rdesk_client::conn::MachineTarget { id: m.id, addrs: m.addrs.clone() };
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
    Ok(Target { name, entry, id, machine, cfg, net: config::load_network() })
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
    let swap = a.swap_cmd_ctrl || t.cfg.swap_cmd_ctrl || (cfg!(target_os = "macos") && !a.no_swap_cmd_ctrl);
    let download_dir = directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    rdesk_client::viewer::run(rdesk_client::viewer::ViewerOpts {
        connect: opts,
        swap_cmd_ctrl: swap,
        download_dir,
        open_files: a.files,
    })
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
        Some(m) => Some(
            rdesk_client::conn::get_ticket(m.id, ep.id().as_bytes(), &mut None).await.map_err(|(_, e)| anyhow!(e))?,
        ),
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
