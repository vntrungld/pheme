//! `pheme` command-line entry point.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pheme_app::config::Config;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "pheme",
    version,
    about = "Keyboard, mouse and audio sharing over the LAN"
)]
struct Cli {
    /// Increase log verbosity (-v, -vv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run as the machine that owns the keyboard and mouse
    Server {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Print a pairing code and accept one pairing before serving
        #[arg(long)]
        pair: bool,
        /// Log RTT and traffic counters every second
        #[arg(long)]
        stats: bool,
    },
    /// Run as the machine being controlled
    Client {
        /// Server address, HOST or HOST:PORT (overrides `connect` in the config)
        host: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        stats: bool,
    },
    /// Pair with a server using the code it displays
    Pair {
        host: String,
        code: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Install OS prerequisites (Linux: uinput udev rule)
    Setup,
    /// List the Pheme servers advertising on this network
    Discover {
        /// How many seconds to listen
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(verbose > 1)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match cli.cmd {
        Cmd::Server {
            config,
            pair,
            stats,
        } => {
            let cfg = Config::load(config.as_deref())?;
            pheme_app::server::main(cfg, pair, stats).await
        }
        Cmd::Client {
            host,
            config,
            stats,
        } => {
            let cfg = Config::load(config.as_deref())?;
            pheme_app::client::main(cfg, host.as_deref(), stats).await
        }
        Cmd::Pair { host, code, config } => {
            let cfg = Config::load(config.as_deref())?;
            pheme_app::client::pair(cfg, &host, &code).await
        }
        Cmd::Setup => pheme_app::setup::run(),
        Cmd::Discover { timeout } => {
            let found =
                pheme_net::discovery::browse(std::time::Duration::from_secs(timeout)).await?;
            if found.is_empty() {
                println!("No Pheme servers found. If one is running, check that UDP port 5353 is not blocked.");
                return Ok(());
            }
            println!("{:<24} {:<22} FINGERPRINT", "NAME", "ADDRESS");
            for f in &found {
                println!(
                    "{:<24} {:<22} {}",
                    f.name,
                    f.addr.to_string(),
                    f.fingerprint.as_deref().unwrap_or("-")
                );
            }
            // Two servers with one name make `connect` ambiguous: whichever
            // answers first wins, and that is not the user's choice.
            for f in &found {
                if found.iter().filter(|o| o.name == f.name).count() > 1 {
                    println!(
                        "\nWarning: more than one server is called {:?}. \
                         `connect = {:?}` will reach whichever answers first; \
                         give them different names, or use an address.",
                        f.name, f.name
                    );
                    break;
                }
            }
            Ok(())
        }
    }
}
