//! `pheme-probe`: the rig's instrument. See the crate docs in `lib.rs`.

use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};

use pheme_probe::record::{record, Watch};
use pheme_probe::{emit, gesture, selfcheck, tally};

#[derive(Parser)]
#[command(about = "Emits and records input for the pheme test rig. Test equipment, not product.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Emit a gesture, e.g. `emit "move 2000 0; type Hi; hold LeftShift"`.
    Emit { spec: String },
    /// Record what arrives for N seconds and print its tally as JSON.
    Record {
        #[arg(long)]
        seconds: u64,
        /// Record the evdev device whose name contains this, instead of
        /// pheme's own. Linux only.
        #[arg(long)]
        device_hint: Option<String>,
    },
    /// Emit and record on this machine, with pheme not running, and fail
    /// unless the recorder heard exactly what was sent.
    Selfcheck {
        #[arg(long)]
        device_hint: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Emit { spec } => {
            // Parsed before the device exists, so a bad spec emits nothing.
            let events = gesture::expand(&gesture::parse(&spec)?);
            let mut inj = emit::open()?;
            emit::send(inj.as_mut(), &events)?;
            println!("emitted {} events", events.len());
        }
        Cmd::Record {
            seconds,
            device_hint,
        } => {
            let events = record(Watch {
                deadline: Instant::now() + Duration::from_secs(seconds),
                device_hint: device_hint.as_deref(),
                ready: &mut || eprintln!("recording"),
                // A recording window, ended by its deadline: the runner
                // judges the tally, so the probe does not decide when enough
                // has arrived.
                done: &|_| false,
            })?;
            println!("{}", serde_json::to_string(&tally::tally(&events))?);
        }
        Cmd::Selfcheck { device_hint } => {
            let t = selfcheck::run(device_hint, Duration::from_secs(10))?;
            println!("{}", serde_json::to_string(&t)?);
        }
    }
    Ok(())
}
