mod config;
mod credentials;
mod daemon;
mod desktop;
mod nextcloud;
mod push;
mod rclone;
mod tray;
mod watch;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::daemon::Prompt;
use crate::rclone::{Bisync, Mode, Outcome};

/// Keeps a local folder in sync with Nextcloud, using rclone bisync.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file
    #[arg(long, env = "NC_SYNC_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync continuously, on local and remote changes
    Run,
    /// Sync once
    Sync {
        /// Rebuild the sync state by merging both sides; never deletes
        #[arg(long, conflicts_with = "force")]
        resync: bool,
        /// Sync even if more than half of the files would be deleted
        #[arg(long)]
        force: bool,
    },
    /// Log in through the browser and store an app password in the keyring
    Login,
    /// Revoke the app password and remove it from the keyring
    Logout,
}

// a mostly idle daemon; blocking work goes to spawn_blocking
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_logging();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("no other TLS provider is installed");

    let cli = Cli::parse();
    let config = Config::load(&cli.config.unwrap_or_else(Config::default_path))?;
    let client = nextcloud::client()?;

    match cli.command {
        Command::Run => daemon::run(config, client).await,
        Command::Sync { resync, force } => {
            let credentials = credentials::load(&config.server)
                .await?
                .context("not logged in, run `nc-sync login` first")?;
            let bisync = Bisync::nextcloud(&config, &credentials).await?;
            let mode = match (resync, force) {
                (true, _) => Mode::Resync,
                (_, true) => Mode::Force,
                _ => Mode::Normal,
            };
            match daemon::sync(&bisync, mode, &client, &credentials).await? {
                Outcome::Synced { .. } => Ok(()),
                Outcome::Unreachable => bail!("cannot reach the server"),
                Outcome::Unauthorized => {
                    bail!("the server rejected the app password, run `nc-sync login`")
                }
                Outcome::SafetyAbort {
                    reason,
                    forceable: true,
                } => {
                    bail!("{reason}\nRerun with --force if that is intended.")
                }
                Outcome::SafetyAbort { reason, .. } => bail!("{reason}"),
                Outcome::NeedsResync => bail!("bisync needs a resync, rerun with --resync"),
                Outcome::Failed(err) => bail!("sync failed: {err}"),
            }
        }
        Command::Login => {
            let credentials = daemon::login(&client, &config.server, Prompt::Browser).await?;
            println!("Logged in as {}", credentials.login_name);
            Ok(())
        }
        Command::Logout => {
            let Some(credentials) = credentials::load(&config.server).await? else {
                println!("Not logged in");
                return Ok(());
            };
            if let Err(err) = nextcloud::revoke(&client, &credentials).await {
                tracing::warn!("{err:#}, removing it locally anyway");
            }
            credentials::delete(&config.server).await?;
            println!("Logged out");
            Ok(())
        }
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    // journald adds its own timestamps
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().init();
    } else {
        builder.init();
    }
}
