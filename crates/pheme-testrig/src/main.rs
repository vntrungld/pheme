//! `pheme-testrig`: runs the machine-checkable rows of `docs/testing.md`
//! against the rig's VMs and prints a verdict for every row asked for.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use pheme_testrig::deploy::{self, Source};
use pheme_testrig::lifecycle::{self, Os, RunLock};
use pheme_testrig::report::{exit_code, finalise, render_json, render_table};
use pheme_testrig::rows::{self, pkg, without_vms};
use pheme_testrig::verdict::RowResult;
use pheme_testrig::vm::RigError;

const STATE: &str = "testrig/.state";

#[derive(Parser)]
#[command(about = "The pheme test rig's runner. See testrig/README.md.")]
struct Cli {
    /// Comma-separated row ids, e.g. `S1,S2,E1`. Every row when omitted.
    #[arg(long, default_value = "")]
    rows: String,
    /// Report what would be judged without touching any VM.
    #[arg(long)]
    dry_run: bool,
    /// Install the packages found in this directory.
    #[arg(long, conflicts_with = "ci_run")]
    from_local: Option<PathBuf>,
    /// Install the packages a release.yml run built, by run id.
    #[arg(long)]
    ci_run: Option<String>,
    /// Leave the VMs running afterwards, for debugging.
    #[arg(long)]
    keep: bool,
    /// Where to write the JSON report.
    #[arg(long, default_value = "testrig/.state/report.json")]
    json: PathBuf,
}

/// Installs the `.deb` on a freshly reverted Ubuntu VM and judges the
/// package rows. Every failure here is the rig's, typed as such.
fn run_package_rows(source: &Source, ids: &[&str], keep: bool) -> Result<Vec<RowResult>, RigError> {
    let into = Path::new(STATE).join("artifacts");
    let _ = std::fs::remove_dir_all(&into);
    std::fs::create_dir_all(&into)
        .map_err(|e| RigError(format!("creating {}: {e}", into.display())))?;
    let artifacts = deploy::fetch(source, &into)?;
    let domain = lifecycle::conf("UBUNTU_VM")?;
    let ip = lifecycle::prepare(&domain, Os::Linux)?;
    let judged = pkg::run(&ip, &artifacts, ids);
    // Finished even when judging failed, so the next run's revert is still
    // checked against a canary.
    let finished = lifecycle::finish(&domain, Os::Linux, &ip, keep);
    let judged = judged?;
    finished?;
    Ok(judged)
}

fn main() -> Result<()> {
    // Not exit() inside run(): exit skips destructors, and the run lock is
    // released by one.
    let code = run(Cli::parse())?;
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let selected = rows::select(&cli.rows)?;
    let planned: Vec<&str> = selected.iter().map(|r| r.id).collect();

    let mut done = Vec::new();
    let mut to_run = Vec::new();
    for row in &selected {
        match without_vms(row, cli.dry_run) {
            Some(result) => done.push(result),
            None => to_run.push(*row),
        }
    }

    let mut source_line = None;
    let mut rig_error = None;
    let _lock;
    if !to_run.is_empty() {
        let source = match (&cli.from_local, &cli.ci_run) {
            (Some(dir), None) => Source::Local(dir.clone()),
            (None, Some(id)) => Source::CiRun(id.clone()),
            _ => bail!("rows {planned:?} install pheme: pass --from-local <dir> or --ci-run <id>"),
        };
        // Refused outright rather than reported: a second run must not even
        // start against VMs another run is using.
        _lock = RunLock::acquire(Path::new(STATE))?;
        source_line = Some(format!("packages: {source}"));
        let ids: Vec<&str> = to_run.iter().map(|r| r.id).collect();
        match run_package_rows(&source, &ids, cli.keep) {
            Ok(results) => done.extend(results),
            Err(e) => {
                // Infrastructure failure is not product failure: every row
                // that depended on it is INCONCLUSIVE, never FAIL.
                for row in &to_run {
                    done.push(RowResult {
                        id: row.id.into(),
                        what: row.what.into(),
                        verdict: e.verdict(),
                        evidence: e.to_string(),
                    });
                }
                rig_error = Some(e);
            }
        }
    }
    // The report is assembled from what was planned, not from what finished,
    // so a run that stops early still lists every row it was asked for.
    let results = finalise(&planned, done, None);

    if cli.dry_run {
        println!("DRY RUN -- no VM was touched");
    }
    if let Some(line) = source_line {
        println!("{line}");
    }
    print!("{}", render_table(&results));
    if let Some(e) = rig_error {
        println!("\nthe run hit a {e}; its rows are INCONCLUSIVE, not failed");
    }
    if let Some(dir) = cli.json.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&cli.json, render_json(&results))
        .with_context(|| format!("writing {}", cli.json.display()))?;
    Ok(exit_code(&results))
}
