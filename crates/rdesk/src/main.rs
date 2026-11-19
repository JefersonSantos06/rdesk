//! `rdesk` command line.

use std::io::Write as _;
use std::path::PathBuf;
#[cfg(feature = "client")]
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use rdesk_common::agent::{self, Agent};
#[cfg(feature = "client")]
use rdesk_common::auth;
use rdesk_common::config::{
    self, AuthPref, ClientConfig, HostEntry, KnownHosts, format_fingerprint, parse_fingerprint,
};
#[cfg(feature = "client")]
use rdesk_common::transport;
#[cfg(feature = "host")]
use rdesk_common::transport::HostIdentity;
#[cfg(feature = "host")]
use ssh_key::PublicKey;
#[cfg(feature = "host")]
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(name = "rdesk", version, about = "Remote desktop over QUIC + Tailscale, authenticated with 1Password")]
struct Cli {
    /// More logging (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// Without a command: open the launcher window (saved computers and tailnet devices).
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
    /// Open a viewer window to a host (alias from client.toml or address).
    #[cfg(feature = "client")]
    Connect(ConnectArgs),
    /// Manage saved hosts (client.toml) and trusted fingerprints.
    #[command(subcommand)]
    Hosts(HostsCmd),
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
        /// Regenerate the host certificate (clients will need to re-trust it).
        #[arg(long)]
        force: bool,
    },
    /// Run the host (foreground).
    Run {
        /// Listen address(es); default from host.toml ("tailscale").
        #[arg(long)]
        listen: Vec<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Accept clients outside the tailnet (LAN / loopback testing).
        #[arg(long)]
        allow_non_tailnet: bool,
        /// Use a synthetic test picture instead of the real screen.
        #[arg(long, hide = true)]
        fake_capture: bool,
        /// Log input instead of injecting it.
        #[arg(long, hide = true)]
        fake_input: bool,
    },
    /// Print this host's certificate fingerprint (compare it on first connect).
    Fingerprint,
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
    /// Save a host: `rdesk hosts add casa casa-pc.tailnet-xyz.ts.net`
    Add {
        alias: String,
        addr: String,
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
    /// Pin a host fingerprint without connecting.
    Trust {
        name: String,
        fingerprint: String,
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
    /// Expected host fingerprint (skips the trust prompt on first connect).
    #[arg(long)]
    fingerprint: Option<String>,
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
        0 => "warn,rdesk=info,rdesk_host=info,rdesk_client=info,rdesk_common=info",
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
        "warn,rdesk=info,rdesk_host=info,rdesk_client=info,rdesk_common=info"
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
            println!("Fingerprint: {}", format_fingerprint(&fp));
            println!();
            println!("Next steps:");
            println!("  rdesk host add-key --from-agent      # authorize your 1Password SSH key");
            println!("  rdesk host set-password --op op://…  # optional password fallback");
            println!("  rdesk host run                       # or just `rdesk` for the app");
            Ok(())
        }
        HostCmd::Run { listen, port, allow_non_tailnet, fake_capture, fake_input } => {
            runtime()?.block_on(rdesk_host::run(rdesk_host::RunOpts {
                listen,
                port,
                allow_non_tailnet,
                fake_capture: fake_capture || std::env::var_os("RDESK_FAKE_CAPTURE").is_some(),
                fake_input: fake_input || std::env::var_os("RDESK_FAKE_INPUT").is_some(),
            }))
        }
        HostCmd::Fingerprint => {
            println!("{}", format_fingerprint(&HostIdentity::load()?.fingerprint()));
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
        HostsCmd::Add { alias, addr, auth, key, password_ref } => {
            config::ensure_config_dir()?;
            let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
            cfg.hosts
                .insert(alias.clone(), HostEntry { addr, auth: auth.into(), key, password_ref, ..Default::default() });
            config::save_toml(config::CLIENT_CONFIG, &cfg)?;
            println!("Saved {alias}. Connect with: rdesk connect {alias}");
            Ok(())
        }
        HostsCmd::List => {
            let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
            let kh = KnownHosts::load()?;
            for (alias, e) in &cfg.hosts {
                let fp = kh.entries.get(alias).map(format_fingerprint).unwrap_or_else(|| "(not trusted yet)".into());
                println!("{alias}\t{}\t{:?}\t{fp}", e.addr, e.auth);
            }
            Ok(())
        }
        HostsCmd::Remove { alias } => {
            let mut cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
            cfg.hosts.remove(&alias);
            config::save_toml(config::CLIENT_CONFIG, &cfg)?;
            let mut kh = KnownHosts::load()?;
            kh.entries.remove(&alias);
            kh.save()?;
            Ok(())
        }
        HostsCmd::Trust { name, fingerprint } => {
            config::ensure_config_dir()?;
            let mut kh = KnownHosts::load()?;
            kh.entries.insert(name, parse_fingerprint(&fingerprint)?);
            kh.save()
        }
    }
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
    cfg: ClientConfig,
}

#[cfg(feature = "client")]
fn resolve_target(target: &str, a: &AuthArgs) -> Result<Target> {
    let cfg: ClientConfig = config::load_toml(config::CLIENT_CONFIG)?;
    let mut entry =
        cfg.hosts.get(target).cloned().unwrap_or_else(|| HostEntry { addr: target.to_string(), ..Default::default() });
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
    Ok(Target { name: target.to_string(), entry, cfg })
}

/// Return the pinned fingerprint for `t`, asking the user to trust it on first contact.
#[cfg(feature = "client")]
async fn ensure_trusted(t: &Target, expected: Option<&str>) -> Result<[u8; 32]> {
    let mut kh = KnownHosts::load()?;
    if let Some(fp) = expected {
        let fp = parse_fingerprint(fp)?;
        if let Some(old) = kh.entries.get(&t.name)
            && *old != fp
        {
            bail!("{} is already trusted with a different fingerprint; run `rdesk hosts trust` to replace it", t.name);
        }
        config::ensure_config_dir()?;
        kh.entries.insert(t.name.clone(), fp);
        kh.save()?;
        return Ok(fp);
    }
    if let Some(fp) = kh.entries.get(&t.name) {
        return Ok(*fp);
    }
    let addr = transport::resolve(&t.entry.addr, rdesk_proto::DEFAULT_PORT).await?;
    let ep = transport::client_endpoint(&addr)?;
    let c = transport::connect(&ep, addr, None, &t.cfg.transport, Duration::from_secs(10)).await?;
    let fp = c.host_fingerprint;
    c.conn.close(transport::close::UNTRUSTED_HOST, b"checking fingerprint");
    eprintln!("First connection to {} ({addr}).", t.name);
    eprintln!("Host fingerprint: {}", format_fingerprint(&fp));
    eprintln!("Compare with `rdesk host fingerprint` on that machine.");
    let ans = prompt("Trust this host? [y/N] ")?;
    if !matches!(ans.to_lowercase().as_str(), "y" | "yes" | "s" | "sim") {
        bail!("host not trusted");
    }
    config::ensure_config_dir()?;
    kh.entries.insert(t.name.clone(), fp);
    kh.save()?;
    Ok(fp)
}

#[cfg(feature = "client")]
fn connect(a: ConnectArgs) -> Result<()> {
    use rdesk_client::conn::ConnectOpts;
    let t = resolve_target(&a.target, &a.auth)?;
    let pin = {
        let rt = runtime()?;
        rt.block_on(ensure_trusted(&t, a.auth.fingerprint.as_deref()))?
    };
    let opts = ConnectOpts {
        name: t.name.clone(),
        // The window asks in a dialog; --headless asks on the terminal.
        ask_password_in_ui: !a.headless,
        entry: t.entry.clone(),
        pin,
        monitor: a.monitor,
        transport: t.cfg.transport.clone(),
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

/// Connect + authenticate without the video/UI machinery, for one-shot CLI commands.
#[cfg(feature = "client")]
async fn cli_session(t: &Target, a: &AuthArgs) -> Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream)> {
    let pin = ensure_trusted(t, a.fingerprint.as_deref()).await?;
    let addr = transport::resolve(&t.entry.addr, rdesk_proto::DEFAULT_PORT).await?;
    let ep = transport::client_endpoint(&addr)?;
    let c = transport::connect(&ep, addr, Some(pin), &t.cfg.transport, Duration::from_secs(10)).await?;
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
    let (conn, _w, _r) = cli_session(&t, &a).await?;
    for f in files {
        let mut p = progress_printer(f.display().to_string());
        let saved = rdesk_common::files::push(&conn, &f, &mut p).await?;
        eprintln!();
        println!("{} → host inbox as {saved}", f.display());
    }
    conn.close(transport::close::NORMAL, b"done");
    Ok(())
}

#[cfg(feature = "client")]
async fn cli_get(target: String, remote: String, out: PathBuf, a: AuthArgs) -> Result<()> {
    let t = resolve_target(&target, &a)?;
    let (conn, _w, _r) = cli_session(&t, &a).await?;
    let mut p = progress_printer(remote.clone());
    let path = rdesk_common::files::pull(&conn, &remote, &out, &mut p).await?;
    eprintln!();
    println!("saved {}", path.display());
    conn.close(transport::close::NORMAL, b"done");
    Ok(())
}

#[cfg(feature = "client")]
async fn cli_ls(target: String, path: String, a: AuthArgs) -> Result<()> {
    use rdesk_proto::{ClientMsg, HostMsg};
    let t = resolve_target(&target, &a)?;
    let (conn, mut w, mut r) = cli_session(&t, &a).await?;
    rdesk_proto::write_msg(&mut w, &ClientMsg::ListDir { req: 1, path }).await?;
    loop {
        match rdesk_proto::expect_msg::<_, HostMsg>(&mut r, rdesk_proto::MAX_CONTROL_MSG).await? {
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
    conn.close(transport::close::NORMAL, b"done");
    Ok(())
}
