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
        /// Report status to a front-end over this socket and take commands
        /// from it. Hidden because it is not something a person invokes: it is
        /// how `pheme` with no subcommand talks to the child it started.
        #[arg(long, hide = true)]
        ipc: Option<PathBuf>,
    },
    /// Run as the machine being controlled
    Client {
        /// Server address, HOST or HOST:PORT (overrides `connect` in the config)
        host: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        stats: bool,
        /// Report status to a front-end over this socket and take commands
        /// from it. Hidden because it is not something a person invokes: it is
        /// how `pheme` with no subcommand talks to the child it started.
        #[arg(long, hide = true)]
        ipc: Option<PathBuf>,
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
    /// List the audio devices this machine offers
    Devices,
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
            ipc,
        } => {
            let cfg = Config::load(config.as_deref())?;
            pheme_app::server::main(cfg, pair, stats, ipc).await
        }
        Cmd::Client {
            host,
            config,
            stats,
            ipc,
        } => {
            let cfg = Config::load(config.as_deref())?;
            pheme_app::client::main(cfg, host.as_deref(), stats, ipc).await
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
            // A host with several network interfaces (Wi-Fi, Ethernet, a Docker
            // bridge, ...) answers once per interface, so collapse rows by
            // fingerprint: that is the host's real identity, not its name. An
            // entry with no fingerprint cannot be matched to any other, so it
            // stays its own row rather than silently merging into one that
            // might be a different machine.
            struct Host {
                name: String,
                fingerprint: Option<String>,
                addrs: Vec<std::net::SocketAddr>,
            }
            let mut hosts: Vec<Host> = Vec::new();
            for f in &found {
                if let Some(fp) = &f.fingerprint {
                    if let Some(h) = hosts
                        .iter_mut()
                        .find(|h| h.fingerprint.as_deref() == Some(fp.as_str()))
                    {
                        h.addrs.push(f.addr);
                        continue;
                    }
                }
                hosts.push(Host {
                    name: f.name.clone(),
                    fingerprint: f.fingerprint.clone(),
                    addrs: vec![f.addr],
                });
            }
            println!("{:<24} {:<22} FINGERPRINT", "NAME", "ADDRESS");
            for h in &hosts {
                let addrs = h
                    .addrs
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{:<24} {:<22} {}",
                    h.name,
                    addrs,
                    h.fingerprint.as_deref().unwrap_or("-")
                );
            }
            // Two distinct hosts with one name make `connect` ambiguous:
            // whichever answers first wins, and that is not the user's
            // choice. Rows are already collapsed by host, so two rows
            // sharing a name are always two different machines (or one that
            // could not be identified) -- never the same host counted twice.
            for h in &hosts {
                if hosts.iter().filter(|o| o.name == h.name).count() > 1 {
                    println!(
                        "\nWarning: more than one server is called {:?}. \
                         `connect = {:?}` will reach whichever answers first; \
                         give them different names, or use an address.",
                        h.name, h.name
                    );
                    break;
                }
            }
            Ok(())
        }
        Cmd::Devices => {
            let devices = pheme_audio::devices::list_devices()?;
            if devices.is_empty() {
                println!("No audio devices found.");
                return Ok(());
            }
            println!("{:<10} {:<40} DEFAULT", "KIND", "NAME");
            for d in &devices {
                let kind = match d.kind {
                    pheme_audio::devices::DeviceKind::Playback => "playback",
                    pheme_audio::devices::DeviceKind::Capture => "capture",
                };
                println!(
                    "{:<10} {:<40} {}",
                    kind,
                    d.name,
                    if d.is_default { "*" } else { "" }
                );
            }
            Ok(())
        }
    }
}
