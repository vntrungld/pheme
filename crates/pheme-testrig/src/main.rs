//! `pheme-testrig`: runs the machine-checkable rows of `docs/testing.md`
//! against the rig's VMs and prints a verdict for every row asked for.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use pheme_testrig::report::{exit_code, finalise, render_json, render_table};
use pheme_testrig::rows::{self, without_vms};

#[derive(Parser)]
#[command(about = "The pheme test rig's runner. See testrig/README.md.")]
struct Cli {
    /// Comma-separated row ids, e.g. `S1,S2,E1`. Every row when omitted.
    #[arg(long, default_value = "")]
    rows: String,
    /// Report what would be judged without touching any VM.
    #[arg(long)]
    dry_run: bool,
    /// Where to write the JSON report.
    #[arg(long, default_value = "testrig/.state/report.json")]
    json: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let selected = rows::select(&cli.rows)?;
    let planned: Vec<&str> = selected.iter().map(|r| r.id).collect();

    let mut done = Vec::new();
    for row in &selected {
        if let Some(result) = without_vms(row, cli.dry_run) {
            done.push(result);
        }
    }
    // The report is assembled from what was planned, not from what finished,
    // so a run that stops early still lists every row it was asked for.
    let results = finalise(&planned, done, None);

    if cli.dry_run {
        println!("DRY RUN -- no VM was touched");
    }
    print!("{}", render_table(&results));
    if let Some(dir) = cli.json.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&cli.json, render_json(&results))
        .with_context(|| format!("writing {}", cli.json.display()))?;
    std::process::exit(exit_code(&results));
}
