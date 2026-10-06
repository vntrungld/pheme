//! The table and the JSON a run produces.

use crate::verdict::{RowResult, Verdict};

/// Every planned row appears in the output, in the planned order. Rows the run
/// never reached become SKIPPED with the reason, so a run that died partway
/// cannot read as a short all-green table.
pub fn finalise(planned: &[&str], done: Vec<RowResult>, aborted: Option<&str>) -> Vec<RowResult> {
    let mut out = done;
    for id in planned {
        if out.iter().any(|r| r.id == *id) {
            continue;
        }
        let reason = match aborted {
            Some(why) => format!("run aborted: {why}"),
            None => "run aborted before this row".to_string(),
        };
        out.push(RowResult {
            id: (*id).to_string(),
            what: String::new(),
            verdict: Verdict::Skipped,
            evidence: reason,
        });
    }
    let rank = |id: &str| planned.iter().position(|p| *p == id).unwrap_or(usize::MAX);
    out.sort_by_key(|r| rank(&r.id));
    out
}

pub fn render_table(rows: &[RowResult]) -> String {
    let w_id = rows.iter().map(|r| r.id.len()).max().unwrap_or(3).max(3);
    let w_what = rows.iter().map(|r| r.what.len()).max().unwrap_or(4).max(4);
    let mut out = format!(
        "{:<w_id$}  {:<w_what$}  {:<12}  {}\n",
        "ROW", "WHAT", "VERDICT", "EVIDENCE"
    );
    for r in rows {
        out.push_str(&format!(
            "{:<w_id$}  {:<w_what$}  {:<12}  {}\n",
            r.id,
            r.what,
            r.verdict.name(),
            r.evidence
        ));
    }
    out
}

pub fn render_json(rows: &[RowResult]) -> String {
    serde_json::json!({ "rows": rows }).to_string()
}

/// Only Fail and Broken make a run unsuccessful. Skipped must not: rows
/// E1-E10 are permanently skipped, and an always-non-zero exit trains
/// everybody to ignore it.
pub fn exit_code(rows: &[RowResult]) -> i32 {
    let bad = rows
        .iter()
        .filter(|r| matches!(r.verdict, Verdict::Fail | Verdict::Broken))
        .count();
    i32::from(bad > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::{RowResult, Verdict};

    fn row(id: &str, v: Verdict) -> RowResult {
        RowResult {
            id: id.into(),
            what: format!("row {id}"),
            verdict: v,
            evidence: String::new(),
        }
    }

    /// The whole reason SKIPPED exists. The production change that breaks
    /// this: filter the table to rows that ran. A reader would then see a
    /// short all-green table and conclude the missing rows were fine.
    #[test]
    fn a_skipped_row_appears_in_the_table_with_its_reason() {
        let rows = vec![
            row("S1", Verdict::Pass),
            RowResult {
                id: "E1".into(),
                what: "monitor answers DDC/CI".into(),
                verdict: Verdict::Skipped,
                evidence: "no virtual monitor answers VCP 0x60".into(),
            },
        ];
        let table = render_table(&rows);
        assert!(table.contains("E1"), "the skipped row vanished:\n{table}");
        assert!(table.contains("no virtual monitor answers VCP 0x60"));
    }

    /// Review Focus 4. The production change that breaks this: return `done`
    /// unchanged. A run that died after two of six rows would print two
    /// passes and nothing else, which reads as "all good".
    #[test]
    fn an_aborted_run_reports_the_rows_it_never_reached() {
        let planned = ["S1", "S2", "S3"];
        let done = vec![row("S1", Verdict::Pass)];
        let final_rows = finalise(&planned, done, Some("ssh to pheme-windows timed out"));

        assert_eq!(final_rows.len(), 3, "unreached rows were dropped");
        for id in ["S2", "S3"] {
            let r = final_rows.iter().find(|r| r.id == id).expect("present");
            assert_eq!(r.verdict, Verdict::Skipped);
            assert!(
                r.evidence.contains("run aborted"),
                "evidence was {:?}",
                r.evidence
            );
        }
    }

    /// An infrastructure failure must never be charged to the product.
    /// The production change that breaks this: map a rig error to Fail.
    #[test]
    fn a_rig_error_is_inconclusive_not_fail() {
        let v = Verdict::from_rig_error("libvirt network pheme-rig is not active");
        assert_eq!(v, Verdict::Inconclusive);
    }

    /// The exit code is what a caller acts on, so it must count the verdicts
    /// that mean "something is wrong" and nothing else. The production change
    /// that breaks this: include Skipped, which would make a normal run with
    /// permanently skipped rows (E1-E10) always exit non-zero and train
    /// everybody to ignore it.
    #[test]
    fn exit_code_counts_failures_and_broken_rows_only() {
        assert_eq!(
            exit_code(&[row("a", Verdict::Pass), row("b", Verdict::Skipped)]),
            0
        );
        assert_eq!(exit_code(&[row("a", Verdict::Inconclusive)]), 0);
        assert_eq!(exit_code(&[row("a", Verdict::Fail)]), 1);
        assert_eq!(exit_code(&[row("a", Verdict::Broken)]), 1);
    }

    /// The table reads in the matrix's own order. The production change that
    /// breaks this: sort by id as a string, which files S10 between S1 and S2.
    #[test]
    fn rows_keep_the_planned_order() {
        let planned = ["S1", "S2", "S10"];
        let done = vec![row("S10", Verdict::Pass), row("S2", Verdict::Pass)];
        let ids: Vec<_> = finalise(&planned, done, None)
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["S1", "S2", "S10"]);
    }

    #[test]
    fn the_json_report_round_trips() {
        let rows = vec![row("S1", Verdict::Pass), row("E1", Verdict::Skipped)];
        let parsed: serde_json::Value =
            serde_json::from_str(&render_json(&rows)).expect("valid JSON");
        assert_eq!(parsed["rows"].as_array().expect("array").len(), 2);
    }
}
