//! Comparing two runs: the regression gate's checks and config differences (the crate's own
//! compare logic), plus which findings appeared, went away or stayed.

use crate::model::{Finding, Report};
use crate::report::diff::{DiffReport, Thresholds, diff};

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
pub fn compare(base: &Report, cand: &Report) -> Comparison {
    let d = diff(base, cand, &Thresholds::default());
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
