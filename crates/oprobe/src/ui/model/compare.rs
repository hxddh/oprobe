//! Comparing two runs: the regression gate's checks and config differences — the same
//! [`crate::analysis::compare`] as `oprobe compare` and MCP `compare_runs`, significance
//! included — plus which findings appeared, went away or stayed.

use crate::model::{Finding, Report, Run};
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
    /// Why the two runs are not comparable (design v4 §7: a different target identity or
    /// workload hash); empty when they are. A non-comparable pair gets no regression verdict.
    pub not_comparable: Vec<String>,
}

impl Comparison {
    pub fn comparable(&self) -> bool {
        self.not_comparable.is_empty()
    }
}

/// Why `a` and `b` did not measure the same thing: the target identities differ, or the
/// workload keys (dataset, mix, access, rate, warmup, duration, max ops) that differ.
pub fn not_comparable(a: &Report, b: &Report) -> Vec<String> {
    let mut why = Vec::new();
    let (ta, tb) = (a.spec.target_id(), b.spec.target_id());
    let show = |t: &Option<crate::store::TargetId>, r: &Report| {
        t.as_ref()
            .map(|t| t.to_string())
            .unwrap_or_else(|| r.spec.target_ref.clone())
    };
    if ta.is_none() || ta != tb {
        why.push(format!("target: {} ≠ {}", show(&ta, a), show(&tb, b)));
    }
    if a.spec.workload_hash() != b.spec.workload_hash() {
        let (wa, wb) = (
            a.spec.resolved_profile().workload,
            b.spec.resolved_profile().workload,
        );
        let keys: [(&str, String, String); 7] = [
            (
                "dataset",
                j(&wa.effective_dataset()),
                j(&wb.effective_dataset()),
            ),
            ("mix", j(&wa.effective_mix()), j(&wb.effective_mix())),
            ("access", j(&wa.access), j(&wb.access)),
            ("rate", j(&wa.rate), j(&wb.rate)),
            ("warmup", wa.warmup.to_string(), wb.warmup.to_string()),
            ("duration", wa.duration.to_string(), wb.duration.to_string()),
            ("max_ops", j(&wa.max_ops), j(&wb.max_ops)),
        ];
        let mut any = false;
        for (k, x, y) in keys {
            if x != y {
                any = true;
                why.push(if x.len() + y.len() < 60 {
                    format!("{k}: {x} ≠ {y}")
                } else {
                    format!("{k} differs")
                });
            }
        }
        if !any {
            why.push("workload differs".into());
        }
    }
    why
}

/// Compact JSON of a workload key (for showing what differs).
fn j<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v)
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned()
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
    Comparison {
        diff: d,
        findings,
        not_comparable: not_comparable(base, cand),
    }
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

    #[test]
    fn a_different_workload_or_target_is_not_comparable() {
        let a = crate::report::fixture::run();
        assert!(compare(&a, &a).comparable());
        let mut b = a.clone();
        b.report.spec.profile.workload.duration = crate::util::HumanDuration::secs(99);
        let why = not_comparable(&a.report, &b.report);
        assert!(why.iter().any(|w| w.starts_with("duration:")), "{why:?}");
        let mut c = a.clone();
        c.report.spec.profile.target = crate::config::TargetSpec::from_url("s3://other/x").unwrap();
        let why = not_comparable(&a.report, &c.report);
        assert!(why.iter().any(|w| w.starts_with("target:")), "{why:?}");
    }
}
