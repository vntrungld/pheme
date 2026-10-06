//! The verdicts, and the rules that keep a report honest.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    /// The criterion was evaluated and met.
    Pass,
    /// The criterion was evaluated and not met.
    Fail,
    /// Not attempted. Always carries a reason.
    Skipped,
    /// Attempted, but the criterion could not be evaluated.
    Inconclusive,
    /// The row itself is untrustworthy -- it passed with pheme switched off.
    Broken,
}

impl Verdict {
    /// Infrastructure failure is not product failure. An SSH timeout or a dead
    /// libvirt network says nothing about pheme, and a report that blames
    /// pheme for an unstarted network is worse than no report.
    pub fn from_rig_error(_reason: &str) -> Verdict {
        Verdict::Inconclusive
    }

    pub fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Skipped => "SKIPPED",
            Verdict::Inconclusive => "INCONCLUSIVE",
            Verdict::Broken => "BROKEN",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RowResult {
    pub id: String,
    pub what: String,
    pub verdict: Verdict,
    pub evidence: String,
}
