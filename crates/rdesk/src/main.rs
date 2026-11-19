//! `rdesk` command line.

#[cfg(feature = "host")]
use std::io::Write as _;
use std::path::PathBuf;
#[cfg(feature = "client")]
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use rdesk_common::agent::{self, Agent};
#[cfg(feature = "client")]
use rdesk_common::auth;
use rdesk_common::config::{self, AuthPref, ClientConfig, HostEntry, NetworkConfig, format_host_id, parse_host_id};
#[cfg(feature = "client")]
use rdesk_common::transport;
#[cfg(feature = "host")]
use rdesk_common::transport::HostIdentity;
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
    /// Open a viewer window to a host: a saved name, a host ID, or ID@address.
    #[cfg(feature = "client")]
    Connect(ConnectArgs),
    /// Manage saved hosts (client.toml).
    #[command(subcommand)]
    Hosts(HostsCmd),
    /// Network settings shared by the host and the client (network.toml).
    #[command(subcommand)]
    Net(NetCmd),
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
}

#[derive(Subcommand)]
enum HostsCmd {
    /// Save a host: `rdesk hosts add casa <host ID> [--addr 192.168.0.10]`
    Add {
        alias: String,
        /// The host's ID (`rdesk host id` on that machine).
        id: String,
        /// Direct address (LAN, port forwarding); optional with a relay.
        #[arg(long)]
        addr: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        auth: AuthArg,
        /// Agent key fingerprint to use (SHA256:...).
        #[arg(long)]
        key: Option<String>,
        /// 1Password reference for the password, e.g. op://Private/rdesk/password
        #[arg(long)]
        password_ref: Option<String>,
    },
    List,
    Remove {
        alias: String,
    },
}

#[derive(Subcommand)]
enum NetCmd {
    /// Show the network settings and whether the relay answers.
    Status,
    /// Use your private relay (`https://relay.example.com`), or `off` for none.
    Relay { url: String },
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
    /// Host ID, when the target is an address (`--id <ID> 192.168.0.10`).
    #[arg(long)]
    id: Option<String>,
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
        Command::Hosts(h) => hosts_cmd(h),
        Command::Net(n) => net_cmd(n),
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

fn hosts_cmd(cmd: HostsCmd) -> Result<()> {
    match cmd {
        HostsCmd::Add { alias, id, addr, auth, key, password_ref } => {
            let id = format_host_id(&parse_host_id(&id)?);
            config::ensure_config_dir()?;
            let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
            let addr = addr.unwrap_or_default();
            if addr.is_empty() && config::load_network().relay.is_none() {
                eprintln!(
                    "note: no relay configured, so {alias} is reachable only with --addr (see `rdesk net relay`)"
                );
            }
            cfg.hosts.insert(
                alias.clone(),
                HostEntry { id, addr, auth: auth.into(), key, password_ref, ..Default::default() },
            );
            config::save_toml(config::CLIENT_CONFIG, &cfg)?;
            println!("Saved {alias}. Connect with: rdesk connect {alias}");
            Ok(())
        }
        HostsCmd::List => {
            let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
            for (alias, e) in &cfg.hosts {
                let addr = if e.addr.is_empty() { "-" } else { e.addr.as_str() };
                let id = if e.id.is_empty() { "(no ID)" } else { e.id.as_str() };
                println!("{alias}\t{id}\t{addr}\t{:?}", e.auth);
            }
            Ok(())
        }
        HostsCmd::Remove { alias } => {
            let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
            cfg.hosts.remove(&alias);
            config::save_toml(config::CLIENT_CONFIG, &cfg)
        }
    }
}

fn net_cmd(cmd: NetCmd) -> Result<()> {
    match cmd {
        NetCmd::Relay { url } => {
            let mut net = config::load_network();
            net.relay = if matches!(url.trim(), "" | "off" | "none") { None } else { Some(url.trim().to_string()) };
            rdesk_common::transport::relay_url(&net)?;
            config::ensure_config_dir()?;
            config::save_toml(config::NETWORK_CONFIG, &net)?;
            match &net.relay {
                Some(u) => println!("Relay set to {u}. Restart running hosts to use it."),
                None => println!("No relay: computers are reached by address only."),
            }
            Ok(())
        }
        NetCmd::Status => runtime()?.block_on(net_status()),
    }
}

async fn net_status() -> Result<()> {
    use rdesk_common::transport::{self, Role};
    let net = config::load_network();
    println!("Config:  {}", config::config_path(config::NETWORK_CONFIG)?.display());
    let Some(url) = transport::relay_url(&net)? else {
        println!("Relay:   none (direct addresses only)");
        return Ok(());
    };
    let ep = transport::bind(None, &net, &Default::default(), Role::Client).await?;
    let r = transport::wait_relay(&ep, &net, std::time::Duration::from_secs(8)).await;
    ep.close().await;
    match r {
        Ok(_) => println!("Relay:   {url} (reachable)"),
        Err(e) => println!("Relay:   {url} — {e:#}"),
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
    cfg: ClientConfig,
    net: NetworkConfig,
}

/// `target` is a saved name, a host ID, `ID@address`, or an address with `--id`.
#[cfg(feature = "client")]
fn resolve_target(target: &str, a: &AuthArgs) -> Result<Target> {
    let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    let mut entry = match cfg.hosts.get(target) {
        Some(e) => e.clone(),
        None => {
            let (id, addr) = match (target.split_once('@'), &a.id) {
                (Some((id, addr)), _) => (id.to_string(), addr.to_string()),
                (None, Some(id)) => (id.clone(), target.to_string()),
                (None, None) => (target.to_string(), String::new()),
            };
            if parse_host_id(&id).is_err() {
                bail!("{target}: not a saved computer (rdesk hosts list) nor a host ID; use ID, ID@address or --id");
            }
            HostEntry { id, addr, ..Default::default() }
        }
    };
    if let Some(id) = &a.id {
        entry.id = id.clone();
    }
    let id = parse_host_id(&entry.id).with_context(|| format!("{target} has no valid host ID"))?;
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
    Ok(Target { name: target.to_string(), entry, id, cfg, net: config::load_network() })
}

#[cfg(feature = "client")]
fn connect(a: ConnectArgs) -> Result<()> {
    use rdesk_client::conn::ConnectOpts;
    let t = resolve_target(&a.target, &a.auth)?;
    let opts = ConnectOpts {
        name: t.name.clone(),
        // The window asks in a dialog; --headless asks on the terminal.
        ask_password_in_ui: !a.headless,
        entry: t.entry.clone(),
        id: t.id,
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
    let c = transport::connect(ep, t.id, Some(&t.entry.addr), &t.net, Duration::from_secs(10)).await?;
    let (mut w, mut r) = c.conn.open_bi().await?;
    let used = std::sync::Arc::new(std::sync::Mutex::new(None));
    let asker = rdesk_client::conn::password_asker(&t.name, &t.entry, used.clone(), None);
    let mut creds = rdesk_client::conn::credentials(&t.entry, None, asker).await;
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
    let t = resolve_target(&target, &a)?;
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
    let t = resolve_target(&target, &a)?;
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
    let t = resolve_target(&target, &a)?;
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
