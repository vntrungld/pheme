//! `pheme-probe`: the rig's instrument. See the crate docs in `lib.rs`.

use anyhow::Result;
use clap::{Parser, Subcommand};

use pheme_probe::{emit, gesture};

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
    }
    Ok(())
}
