//! `pheme` command-line entry point.

use std::path::PathBuf;

use anyhow::Context;
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
    /// With no subcommand, opens the front-end window instead.
    #[command(subcommand)]
    cmd: Option<Cmd>,
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
    /// List the monitors this machine can switch, and the inputs they take
    Displays,
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let Some(cmd) = cli.cmd else {
        // No subcommand: open the front-end window. This builds and owns
        // its own tokio runtime rather than sharing one with the block
        // below, so the GUI's blocking event loop never runs nested inside
        // a runtime that is already driving it -- see `frontend::run`.
        return pheme_app::frontend::run();
    };
    // Every existing subcommand keeps its exact behaviour, just no longer
    // riding on `#[tokio::main]`'s own runtime: `Runtime::new()` builds the
    // same multi-threaded, fully-enabled runtime that macro used to.
    let rt = tokio::runtime::Runtime::new().context("building the tokio runtime")?;
    rt.block_on(run_subcommand(cmd))
}

async fn run_subcommand(cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
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
        Cmd::Displays => {
            let mut found = pheme_display::enumerate();
            if found.is_empty() {
                println!(
                    "No monitor answered DDC/CI.\n\
                     On Linux, run `pheme setup` and check that /dev/i2c-* is readable; \
                     `ddcutil detect` is a useful second opinion.\n\
                     Many monitors also have a DDC/CI switch in their on-screen menu, \
                     and some laptop docks and adapters do not carry the i2c lines at all."
                );
                return Ok(());
            }
            println!(
                "{:<40} {:<14} {:<8} SUPPORTED",
                "IDENTITY", "LOCATION", "CURRENT"
            );
            for m in found.iter_mut() {
                // Read before the borrow of `m` is split across the two
                // calls below; both take `&mut self`.
                let current = match m.get_input() {
                    Ok(v) => format_vcp_value(v),
                    Err(_) => "-".to_string(),
                };
                // Advisory: plenty of monitors return no capability string,
                // or one that omits inputs they do accept. The current
                // value above is the reliable half -- switch the input by
                // hand, re-run this, and read off the number.
                let supported = match m.capabilities() {
                    Ok(caps) => {
                        let vals = pheme_display::caps::input_values_from_caps(&caps);
                        format_supported(&vals)
                    }
                    Err(_) => "-".to_string(),
                };
                println!(
                    "{:<40} {:<14} {:<8} {}",
                    m.identity(),
                    m.location(),
                    current,
                    supported
                );
            }
            Ok(())
        }
    }
}

/// A VCP value the way `ddcutil` and MCCS document it: two lower-case hex
/// digits, `0x`-prefixed. The padding matters -- MCCS assigns `0x0F` to
/// DisplayPort-1, and an unpadded `0xf` would read as a different value to
/// anyone cross-checking against a monitor's own on-screen menu.
fn format_vcp_value(v: u16) -> String {
    format!("0x{v:02x}")
}

/// The capability string's advisory input list, space-joined, or `-` when it
/// named none -- which is common and is not an error (see `pick`'s doc
/// comment in `pheme_display` for why the list cannot be relied on).
fn format_supported(vals: &[u16]) -> String {
    if vals.is_empty() {
        "-".to_string()
    } else {
        vals.iter()
            .map(|v| format_vcp_value(*v))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod displays_format_tests {
    use super::{format_supported, format_vcp_value};

    /// Break it by dropping the `02` width: `0x0f` becomes `0xf`, which is
    /// no longer the two-digit form MCCS and `ddcutil` print, and no longer
    /// what a person reads off the monitor's own on-screen menu.
    #[test]
    fn a_single_hex_digit_is_zero_padded() {
        assert_eq!(format_vcp_value(0x0f), "0x0f");
    }

    #[test]
    fn a_two_digit_value_is_unchanged() {
        assert_eq!(format_vcp_value(0x11), "0x11");
    }

    /// The empty capability list is the common case (see the brief this
    /// command was written for), not an error, so it prints `-` rather
    /// than an empty string a column would swallow.
    #[test]
    fn no_supported_values_prints_a_dash() {
        assert_eq!(format_supported(&[]), "-");
    }

    /// Break it by joining with no separator or the wrong one: the values
    /// run together and are no longer individually readable.
    #[test]
    fn several_supported_values_are_space_joined_in_order() {
        assert_eq!(format_supported(&[0x0f, 0x11, 0x12]), "0x0f 0x11 0x12");
    }
}
