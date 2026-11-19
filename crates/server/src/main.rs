//! `rdesk-server` command line: set up, manage allowed users, run.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use rdesk_server::config::{self, OAuthApp, ServerConfig, Tls};
use rdesk_server::db::Db;

#[derive(Parser)]
#[command(name = "rdesk-server", version, about = "Private server for rdesk: accounts, sign-in and relay")]
struct Cli {
    /// Data directory (config, database, keys). Default: /var/lib/rdesk-server, or
    /// %PROGRAMDATA%\rdesk-server on Windows; RDESK_SERVER_DATA overrides it.
    #[arg(long, global = true)]
    data: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the configuration and the first admin.
    Init {
        /// How the app reaches this server, e.g. https://rdesk.example.com
        #[arg(long)]
        public_url: String,
        /// E-mail of the first admin (signs in with Google/GitHub using this address).
        #[arg(long)]
        admin: String,
        /// Contact e-mail for Let's Encrypt (default: the admin's).
        #[arg(long)]
        acme_contact: Option<String>,
        /// Plain HTTP (local tests, or behind a TLS proxy) instead of Let's Encrypt.
        #[arg(long)]
        no_tls: bool,
        /// TCP address to listen on (default 0.0.0.0:443, or 127.0.0.1:8080 with --no-tls).
        #[arg(long)]
        listen: Option<String>,
        /// Name shown on the sign-in page.
        #[arg(long, default_value = "rdesk")]
        name: String,
    },
    /// Run the server (foreground).
    Run,
    /// Allowed users.
    #[command(subcommand)]
    Users(UsersCmd),
    /// Set a sign-in provider's OAuth app (`google` or `github`).
    Oauth {
        provider: String,
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        client_secret: String,
    },
    /// Show the configuration and the redirect URLs to register with Google/GitHub.
    Info,
    /// Replace the key that signs access tickets (hosts trust the old one for 12 more hours).
    RotateKey,
    /// Copy the database to a file (safe while the server runs).
    Backup { file: PathBuf },
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
    /// Make an admin (`on`) or a regular user (`off`).
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,iroh_relay=warn")),
        )
        .init();
    let data = cli.data.unwrap_or_else(config::default_data_dir);
    match cli.cmd {
        Cmd::Init { public_url, admin, acme_contact, no_tls, listen, name } => {
            if data.join(config::CONFIG_FILE).exists() {
                bail!("{} already exists", data.join(config::CONFIG_FILE).display());
            }
            std::fs::create_dir_all(&data)?;
            let cfg = ServerConfig {
                name,
                public_url: public_url.trim_end_matches('/').to_string(),
                listen: listen.unwrap_or_else(|| if no_tls { "127.0.0.1:8080" } else { "0.0.0.0:443" }.into()),
                tls: if no_tls { Tls::None } else { Tls::Acme },
                acme_contact: Some(acme_contact.unwrap_or_else(|| admin.clone())),
                ..Default::default()
            };
            cfg.validate()?;
            cfg.save(&data)?;
            config::load_or_create_key(&data)?;
            let db = Db::open(&data.join(config::DB_FILE))?;
            let u = db.add_user(&admin, true)?;
            println!("Created {} with admin {}.", data.display(), u.email);
            println!();
            print_info(&cfg);
            Ok(())
        }
        Cmd::Run => {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(async {
                let mut running = rdesk_server::start(&data).await?;
                let cfg = &running.state.cfg;
                if cfg.providers().is_empty() {
                    tracing::warn!("no sign-in provider configured: see `rdesk-server oauth --help`");
                }
                tokio::select! {
                    r = running.wait() => r,
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("shutting down");
                        running.stop().await
                    }
                }
            })
        }
        Cmd::Users(u) => users(&data, u),
        Cmd::Oauth { provider, client_id, client_secret } => {
            let mut cfg = ServerConfig::load(&data)?;
            let app = Some(OAuthApp { client_id, client_secret });
            match provider.as_str() {
                "google" => cfg.google = app,
                "github" => cfg.github = app,
                other => bail!("unknown provider {other} (google or github)"),
            }
            cfg.save(&data)?;
            println!("Saved. Restart the server to use it.");
            Ok(())
        }
        Cmd::RotateKey => {
            let new = config::rotate_key(&data)?;
            println!("New ticket key: {}", hex_short(new.as_bytes()));
            println!("Restart the server. Hosts get the new key on their next sync and keep accepting");
            println!("tickets signed with the old one for 12 hours.");
            Ok(())
        }
        Cmd::Backup { file } => {
            let db = Db::open(&data.join(config::DB_FILE))?;
            db.backup(&file)?;
            println!("Database copied to {}.", file.display());
            println!("Also keep {} and {} safe (they are not in the database).", config::CONFIG_FILE, config::KEY_FILE);
            Ok(())
        }
        Cmd::Info => {
            let cfg = ServerConfig::load(&data)?;
            println!("Data: {}", data.display());
            print_info(&cfg);
            Ok(())
        }
    }
}

fn print_info(cfg: &ServerConfig) {
    let base = cfg.public_url.trim_end_matches('/');
    println!("Public URL:  {base}");
    println!(
        "Listen:      {} ({})",
        cfg.listen,
        if cfg.tls == Tls::Acme { "HTTPS, Let's Encrypt" } else { "plain HTTP" }
    );
    if cfg.tls == Tls::Acme && cfg.relay_quic_port != 0 {
        println!("Relay QUIC:  UDP {} (open it in the firewall)", cfg.relay_quic_port);
    }
    println!(
        "Sign-in:     {}",
        if cfg.providers().is_empty() { "none configured".into() } else { cfg.providers().join(", ") }
    );
    println!();
    println!("Redirect URLs for the OAuth apps:");
    println!("  Google: {base}/login/oauth/google/callback");
    println!("  GitHub: {base}/login/oauth/github/callback");
    println!("Then: rdesk-server oauth google --client-id … --client-secret …");
}

fn users(data: &std::path::Path, cmd: UsersCmd) -> Result<()> {
    let db = Db::open(&data.join(config::DB_FILE))?;
    let find = |email: &str| -> Result<rdesk_server::db::User> {
        db.user_by_email(email)?.ok_or_else(|| anyhow::anyhow!("{email} is not on the list"))
    };
    match cmd {
        UsersCmd::List => {
            for u in db.users()? {
                let twofa = match (u.totp_secret.is_some(), db.passkeys(u.id)?.len()) {
                    (false, 0) => "2FA not set up".to_string(),
                    (t, n) => format!(
                        "2FA: {}{}",
                        if t { "totp " } else { "" },
                        if n > 0 { format!("{n} passkey(s)") } else { String::new() }
                    ),
                };
                println!(
                    "{}{}{}\t{}",
                    u.email,
                    if u.is_admin { " (admin)" } else { "" },
                    if u.disabled { " (disabled)" } else { "" },
                    twofa
                );
            }
        }
        UsersCmd::Add { email, admin } => {
            let u = db.add_user(&email, admin)?;
            db.audit(None, "user-added", &format!("{} (cli)", u.email));
            println!("{} can now sign in.", u.email);
        }
        UsersCmd::Remove { email } => {
            let u = find(&email)?;
            db.ensure_not_last_admin(u.id)?;
            db.revoke_user(u.id)?;
            db.remove_user(u.id)?;
            db.audit(None, "user-removed", &format!("{} (cli)", u.email));
            println!("Removed {}.", u.email);
        }
        UsersCmd::Admin { email, state } => {
            let u = find(&email)?;
            db.set_admin(u.id, state == "on")?;
            println!("{}: admin {state}.", u.email);
        }
        UsersCmd::Disable { email, state } => {
            let u = find(&email)?;
            if state == "on" {
                db.ensure_not_last_admin(u.id)?;
            }
            db.set_disabled(u.id, state == "on")?;
            if state == "on" {
                db.revoke_user(u.id)?;
            }
            println!("{}: disabled {state}.", u.email);
        }
    }
    Ok(())
}

fn hex_short(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}
