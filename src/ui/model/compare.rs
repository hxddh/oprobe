//! Comparing two runs: the regression gate's checks and config differences — the same
//! [`crate::analysis::compare`] as `oprobe compare` and MCP `compare_runs`, significance
//! included — plus which findings appeared, went away or stayed.

use crate::model::{Finding, Run};
use crate::report::diff::{DiffReport, Thresholds};

/// How a finding changed from the baseline to the candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindingChange {
    New,
    Resolved,
    Persisting,
}

impl FindingChange {
    pub fn label(self) -> &'static str {
        match self {
            FindingChange::New => "new",
            FindingChange::Resolved => "resolved",
            FindingChange::Persisting => "still there",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
    pub diff: DiffReport,
    pub findings: Vec<(FindingChange, Finding)>,
}

/// Compare `base` (A) with `cand` (B) with the CLI's default thresholds.
pub fn compare(base_run: &Run, cand_run: &Run) -> Comparison {
    let d = crate::analysis::compare::compare(
        std::slice::from_ref(base_run),
        std::slice::from_ref(cand_run),
        base_run.id(),
        cand_run.id(),
        &Thresholds::default(),
    )
    .gate;
    let (base, cand) = (&base_run.report, &cand_run.report);
    let mut findings = Vec::new();
    for f in &cand.findings {
        let change = if base.findings.iter().any(|b| b.code == f.code) {
            FindingChange::Persisting
        } else {
            FindingChange::New
        };
        findings.push((change, f.clone()));
    }
    for f in &base.findings {
        if !cand.findings.iter().any(|c| c.code == f.code) {
            findings.push((FindingChange::Resolved, f.clone()));
        }
    }
    findings.sort_by_key(|(c, f)| (*c as u8, std::cmp::Reverse(f.severity)));
    Comparison { diff: d, findings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_uses_the_cli_comparison() {
        let a = crate::report::fixture::run();
        let mut b = crate::report::fixture::run();
        b.report.run.id = "20261120-150000-b2c3".into();
        b.report.summary.latency.p99 *= 3.0;
        let cli = crate::analysis::compare::compare(
            std::slice::from_ref(&a),
            std::slice::from_ref(&b),
            a.id(),
            b.id(),
            &Thresholds::default(),
        );
        let app = compare(&a, &b);
        assert_eq!(app.diff, cli.gate);
        assert_eq!(app.diff.passed, cli.passed);
    }
}
