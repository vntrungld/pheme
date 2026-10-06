//! The row registry: every row of `docs/testing.md`, including the ones the
//! rig can never run, because a row missing from the registry is a row
//! missing from the report.
//!
//! Each row's comment names the production change that would make it fail;
//! a row whose mutation cannot be named asserts nothing.

use anyhow::{bail, Result};

use crate::verdict::{RowResult, Verdict};

/// How a row is judged.
#[derive(Debug, Clone, Copy)]
pub enum Run {
    /// Never attempted by this rig, for the reason given.
    Skip(&'static str),
    /// Judged by the rig, but not implemented yet. Reported SKIPPED.
    NotYet,
}

#[derive(Debug, Clone, Copy)]
pub struct RowSpec {
    pub id: &'static str,
    pub what: &'static str,
    pub run: Run,
}

const S9: &str =
    "needs a second virtual head; feasible but unverified, deferred rather than guessed at";
const GUI: &str = "needs a framebuffer and clicks -- tier 2 and 3";
const SESSION: &str = "needs a login session, a menu or a browser download -- tier 2 and 3";
const DDC: &str = "no virtual monitor answers VCP 0x60 over i2c; stays manual on real hardware";
const RPM: &str = "the rig has no Fedora VM; the .rpm is covered by packaging/verify-rpm.sh";

const fn row(id: &'static str, what: &'static str, run: Run) -> RowSpec {
    RowSpec { id, what, run }
}

use Run::{NotYet, Skip};

const ROWS: &[RowSpec] = &[
    row("S1", "pairing, and refusal of an untrusted peer", NotYet),
    row("S2", "edge switch", NotYet),
    row("S3", "sliding along the edge does not switch", NotYet),
    row("S4", "typing 100 characters, nothing left held", NotYet),
    row("S5", "modifier across the edge", NotYet),
    row("S6", "scrolling", NotYet),
    row("S7", "lock hotkey", NotYet),
    row("S8", "client disconnect and reconnect", NotYet),
    row("S9", "multi-monitor client", Skip(S9)),
    row("S10", "stats, RTT under 1 ms", NotYet),
    row("A1", "speaker selected, audio plays", NotYet),
    row("A2", "ten minutes of playback with --stats", NotYet),
    row("A3", "network unplugged for 3 s", NotYet),
    row("A4", "Windows client changes output mid-stream", NotYet),
    row("A5", "pause 30 s, resume", NotYet),
    row("A6", "server stops while the client runs", NotYet),
    row("A7", "click track latency", NotYet),
    row("A8", "PipeWire restarted mid-stream", NotYet),
    row("A9", "Windows side of the pair", NotYet),
    row("A10", "Windows server changes output mid-stream", NotYet),
    row("A11", "forced backlog recovers", NotYet),
    row("M1", "Pheme Mic appears and records", NotYet),
    row("M2", "nothing recording, mic closed", NotYet),
    row("M3", "mic closes 3 s after recording stops", NotYet),
    row("M4", "no silence at the start of a recording", NotYet),
    row("M5", "mono mic arrives as two channels", NotYet),
    row("M6", "ten minutes continuous", NotYet),
    row("M7", "network unplugged for 3 s", NotYet),
    row("M8", "server mic unplugged mid-session", NotYet),
    row("M9", "Windows client never opens the server mic", NotYet),
    row("M10", "clap latency", NotYet),
    row("W1", "Wayland: crossing hands input over and back", NotYet),
    row("W2", "Wayland: Shift held across the edge", NotYet),
    row("W3", "Wayland: compositor escape ends capture", NotYet),
    row("W4", "Wayland: client replaced on another edge", NotYet),
    row("W5", "Wayland: client disconnects holding input", NotYet),
    row("W6", "Wayland: lock hotkey", NotYet),
    row("W7", "Wayland: unused edges do not snag", NotYet),
    row("W8", "Wayland: monitor layout changes", NotYet),
    row("W9", "Wayland: permission after server restart", NotYet),
    row("W10", "same build in an X11 session", NotYet),
    row("W11", "Wayland: 100 keystrokes, no stuck key", NotYet),
    row("W12", "Wayland: lock hotkey during active capture", NotYet),
    row(
        "C1",
        "clipboard: Linux X11 server to Windows client",
        NotYet,
    ),
    row(
        "C2",
        "clipboard: Windows server to Linux X11 client",
        NotYet,
    ),
    row(
        "C3",
        "clipboard: KDE Wayland server and Windows client",
        NotYet,
    ),
    row("C4", "clipboard: Unicode and CRLF", NotYet),
    row("C5", "clipboard: 1 MiB then 2 MiB", NotYet),
    row("C6", "clipboard: GNOME Wayland server", NotYet),
    row("C7", "clipboard: copy on both sides in turn", NotYet),
    row("D1", "pheme discover lists the server", NotYet),
    row("D2", "connect by name survives an IP change", NotYet),
    row("D3", "two servers with the same name", NotYet),
    row("D4", "Windows firewall at its default", NotYet),
    row("G1", "first run with no config, Linux KDE", Skip(GUI)),
    row("G2", "first run on Windows", Skip(GUI)),
    row("G3", "pairing from the windows", Skip(GUI)),
    row("G4", "edit the client list and save", Skip(GUI)),
    row("G5", "save over a commented config", Skip(GUI)),
    row("G6", "quit from the tray", Skip(GUI)),
    row("G7", "front-end killed with SIGKILL", Skip(GUI)),
    row("G8", "lock from the tray while on the client", Skip(GUI)),
    row("G9", "pheme devices on both platforms", Skip(GUI)),
    row("G10", "GNOME without the AppIndicator extension", Skip(GUI)),
    row("E1", "pheme displays while off screen", Skip(DDC)),
    row("E2", "display.input switches both ways, twice", Skip(DDC)),
    row(
        "E3",
        "client restarted while the monitor shows the server",
        Skip(DDC),
    ),
    row("E4", "fast sweep across the edge", Skip(DDC)),
    row(
        "E5",
        "switch_display hotkey on the wrong machine",
        Skip(DDC),
    ),
    row("E6", "Switch display button", Skip(DDC)),
    row("E7", "monitor that ignores DDC/CI", Skip(DDC)),
    row("E8", "no [display] section", Skip(DDC)),
    row("E9", "second monitor cable unplugged", Skip(DDC)),
    row("E10", "same display.input on both machines", Skip(DDC)),
    row(
        "F1",
        "apt install of the .deb on a machine with no GTK",
        NotYet,
    ),
    row("F2", "dnf install of the .rpm on a clean Fedora", Skip(RPM)),
    row("F3", "pheme displays right after install", NotYet),
    row("F4", "systemd user service across a sign-in", Skip(SESSION)),
    row("F5", "application menu entry", Skip(SESSION)),
    row("F6", "remove and purge keep only the user config", NotYet),
    row("F7", "Windows installer as a standard user", Skip(SESSION)),
    row("F8", "start at sign-in", Skip(SESSION)),
    row("F9", "Add to PATH, new terminal", Skip(SESSION)),
    row("F10", "install over the top", Skip(SESSION)),
    row("F11", "uninstall on Windows", Skip(SESSION)),
    row("F12", "installer downloaded in a browser", Skip(SESSION)),
];

pub fn registry() -> &'static [RowSpec] {
    ROWS
}

/// The rows a `--rows` list names, in the registry's order; every row when
/// the list is empty. An id the registry does not know is an error, never
/// ignored: a typo would otherwise shrink the run and the table would still
/// look complete.
pub fn select(list: &str) -> Result<Vec<RowSpec>> {
    let wanted: Vec<&str> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if wanted.is_empty() {
        return Ok(ROWS.to_vec());
    }
    if let Some(unknown) = wanted.iter().find(|w| !ROWS.iter().any(|r| r.id == **w)) {
        bail!("no row named {unknown:?}; see docs/testing.md for the ids");
    }
    Ok(ROWS
        .iter()
        .filter(|r| wanted.contains(&r.id))
        .copied()
        .collect())
}

/// The verdict for a row that needs no VM to decide: a permanent skip, a row
/// not implemented yet, or any row in a dry run. `None` means the row has to
/// be run.
pub fn without_vms(row: &RowSpec, dry_run: bool) -> Option<RowResult> {
    let evidence = match row.run {
        Run::Skip(why) => why,
        Run::NotYet => "not implemented in the rig yet",
    };
    // Every arm above is decidable without a VM until rows can run, at which
    // point `dry_run` is what keeps a dry run from touching one.
    let _ = dry_run;
    Some(RowResult {
        id: row.id.to_string(),
        what: row.what.to_string(),
        verdict: Verdict::Skipped,
        evidence: evidence.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every row the manual matrix names, read from the matrix itself: the
    /// table rows (`| A1 |`) and sub-project 1's numbered steps (`**S1 `).
    fn matrix_ids() -> BTreeSet<String> {
        let doc = include_str!("../../../../docs/testing.md");
        let mut ids = BTreeSet::new();
        for line in doc.lines() {
            let cell = line
                .strip_prefix("| ")
                .and_then(|l| l.split_whitespace().next());
            let step = line
                .split_once("**S")
                .map(|(_, rest)| rest.split_whitespace().next().unwrap_or(""));
            for (prefix, id) in [("", cell), ("S", step)] {
                if let Some(id) = id {
                    let id = format!("{prefix}{id}");
                    let mut chars = id.chars();
                    let letter = chars.next().is_some_and(|c| c.is_ascii_uppercase());
                    let digits = chars.as_str();
                    if letter && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                        ids.insert(id);
                    }
                }
            }
        }
        ids
    }

    /// A row missing from the registry is a row missing from the report --
    /// and a report that silently omits rows reads as "the rest were fine".
    /// The production change that breaks this: forget to register a row.
    #[test]
    fn every_row_in_the_matrix_is_registered() {
        let registered: BTreeSet<String> = registry().iter().map(|r| r.id.to_string()).collect();
        let want = matrix_ids();
        assert!(want.len() > 70, "the matrix parser found only {want:?}");
        let missing: Vec<_> = want.difference(&registered).collect();
        assert!(missing.is_empty(), "not registered: {missing:?}");
    }

    #[test]
    fn row_ids_are_unique() {
        let mut seen = BTreeSet::new();
        for r in registry() {
            assert!(seen.insert(r.id), "{} registered twice", r.id);
        }
    }

    /// SKIPPED always carries a reason; a bare skip tells the reader nothing.
    #[test]
    fn every_permanently_skipped_row_says_why() {
        for r in registry() {
            if let Run::Skip(why) = r.run {
                assert!(why.len() > 10, "{} is skipped with no real reason", r.id);
            }
        }
    }

    /// The production change that breaks this: ignore ids the registry does
    /// not know. A typo in `--rows S2,S33` would then run S2 alone and the
    /// table would look complete.
    #[test]
    fn an_unknown_row_id_is_an_error() {
        assert!(select("S2,S33").is_err());
        assert_eq!(select("S2, E1").expect("known").len(), 2);
    }

    /// The production change that breaks this: let a row with no
    /// implementation fall through to PASS, or out of the table.
    #[test]
    fn a_row_with_nothing_to_run_yet_says_so() {
        let r = row("X1", "example", Run::NotYet);
        let got = without_vms(&r, false).expect("decided without a VM");
        assert_eq!(got.verdict, Verdict::Skipped);
        assert!(got.evidence.contains("not implemented"), "{}", got.evidence);
    }

    /// A dry run touches no VM, so a row it would have run is SKIPPED with
    /// that reason -- never a pass for a row that did nothing.
    #[test]
    fn a_dry_run_judges_nothing() {
        let r = row("S2", "edge switch", Run::NotYet);
        let got = without_vms(&r, true).expect("decided without a VM");
        assert_eq!(got.verdict, Verdict::Skipped);
        let e1 = select("E1").expect("known")[0];
        let got = without_vms(&e1, true).expect("permanent skips need no VM");
        assert!(got.evidence.contains("VCP 0x60"), "{}", got.evidence);
    }

    #[test]
    fn no_selection_means_every_row() {
        assert_eq!(select("").expect("all").len(), registry().len());
    }
}
