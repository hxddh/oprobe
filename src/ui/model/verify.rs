//! The App's Verify (design v4 §5 §11): a finding's change is replayed on the run's spec, then
//! the original and the verify run are compared and the hypothesis judged — `confirmed`,
//! `refuted` or `inconclusive` — against the finding's expectations, with numbers.
//!
//! TODO(v4 stream T): when `oprobe verify` / `VerifyResult` land, run that (A/B interleaved,
//! repeated) and show its verdict; this one-shot judgement is the adapter until then.

use crate::analysis::verification::{Direction, Expectation, VerifyOutcome};
use crate::model::Report;
use crate::ui::model::metrics::metric;

/// What the verify run says about the hypothesis.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// The finding verified.
    pub code: String,
    pub outcome: VerifyOutcome,
    /// One line per expectation (or the comparison's summary), with numbers.
    pub lines: Vec<String>,
}

impl Verdict {
    pub fn word(&self) -> &'static str {
        match self.outcome {
            VerifyOutcome::Confirmed => "confirmed",
            VerifyOutcome::Refuted => "refuted",
            VerifyOutcome::Inconclusive => "inconclusive",
        }
    }
}

/// Relative change that counts as a move (one run per side: below this is noise).
const NOISE_RATIO: f64 = 0.03;

fn shown(v: f64, unit: &str) -> String {
    use crate::ui::model::fmt;
    match unit {
        "ratio" => fmt::ratio(v),
        "ms" => fmt::dur_ms(v),
        "ops_per_s" => format!("{} ops/s", fmt::compact(v)),
        "mib_per_s" => format!("{} MiB/s", fmt::compact(v)),
        u => format!("{} {u}", fmt::sig3(v)),
    }
}

fn judge_one(e: &Expectation, base: f64, cand: f64) -> (VerifyOutcome, String) {
    use VerifyOutcome::*;
    let rel = if base.abs() > 1e-12 {
        (cand - base) / base.abs()
    } else if cand.abs() > 1e-12 {
        cand.signum()
    } else {
        0.0
    };
    let outcome = match e.direction {
        Direction::Increase if rel > NOISE_RATIO => Confirmed,
        Direction::Increase if rel < -NOISE_RATIO => Refuted,
        Direction::Decrease if rel < -NOISE_RATIO => Confirmed,
        Direction::Decrease if rel > NOISE_RATIO => Refuted,
        Direction::Increase | Direction::Decrease => Inconclusive,
        Direction::AtMost => {
            if cand <= e.hi.max(e.predicted) {
                Confirmed
            } else {
                Refuted
            }
        }
        Direction::AtLeast => {
            if cand >= e.lo.min(e.predicted) {
                Confirmed
            } else {
                Refuted
            }
        }
        Direction::Unchanged => {
            if rel.abs() <= NOISE_RATIO || (e.lo <= cand && cand <= e.hi) {
                Confirmed
            } else {
                Refuted
            }
        }
    };
    let line = format!(
        "{}: {} → {} (expected {} {}, {}–{})",
        e.metric,
        shown(base, &e.unit),
        shown(cand, &e.unit),
        match e.direction {
            Direction::Increase => "up to",
            Direction::Decrease => "down to",
            Direction::AtMost => "at most",
            Direction::AtLeast => "at least",
            Direction::Unchanged => "about",
        },
        shown(e.predicted, &e.unit),
        shown(e.lo, &e.unit),
        shown(e.hi, &e.unit),
    );
    (outcome, line)
}

/// Judge finding `code`'s `expect` with `base` (the original run) and `cand` (the verify run).
/// Without expectations the regression gate decides: a regression refutes, an improvement
/// without one confirms, anything else is inconclusive.
pub fn judge(
    code: &str,
    expect: &[Expectation],
    base: &Report,
    cand: &Report,
    gate: &crate::report::diff::DiffReport,
) -> Verdict {
    use crate::report::diff::Status;
    let mut lines = Vec::new();
    let outcome = if expect.is_empty() {
        let improved: Vec<&str> = gate
            .checks
            .iter()
            .filter(|c| c.status == Status::Improved)
            .map(|c| c.name.as_str())
            .collect();
        let worse: Vec<&str> = gate
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .map(|c| c.name.as_str())
            .collect();
        if !worse.is_empty() {
            lines.push(format!("worse: {}", worse.join(", ")));
        }
        if !improved.is_empty() {
            lines.push(format!("better: {}", improved.join(", ")));
        }
        if !worse.is_empty() {
            VerifyOutcome::Refuted
        } else if !improved.is_empty() {
            VerifyOutcome::Confirmed
        } else {
            lines.push("no metric moved beyond the gate's thresholds".into());
            VerifyOutcome::Inconclusive
        }
    } else {
        let mut outs = Vec::new();
        for e in expect {
            match (metric(base, &e.metric), metric(cand, &e.metric)) {
                (Some(b), Some(c)) => {
                    let (o, l) = judge_one(e, b, c);
                    outs.push(o);
                    lines.push(l);
                }
                _ => {
                    outs.push(VerifyOutcome::Inconclusive);
                    lines.push(format!("{}: not measured", e.metric));
                }
            }
        }
        if outs.contains(&VerifyOutcome::Refuted) {
            VerifyOutcome::Refuted
        } else if outs.iter().all(|o| *o == VerifyOutcome::Confirmed) {
            VerifyOutcome::Confirmed
        } else {
            VerifyOutcome::Inconclusive
        }
    };
    lines.push("one run per side: low confidence (`oprobe verify` repeats and interleaves)".into());
    Verdict {
        code: code.to_owned(),
        outcome,
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(metric: &str, direction: Direction, predicted: f64, lo: f64, hi: f64) -> Expectation {
        Expectation {
            metric: metric.into(),
            direction,
            predicted,
            lo,
            hi,
            unit: "ops_per_s".into(),
        }
    }

    #[test]
    fn expectations_decide_with_numbers() {
        let run = crate::report::fixture::run();
        let gate = crate::ui::model::compare::compare(&run, &run).diff;
        let base = run.report;
        let mut cand = base.clone();
        cand.summary.ops_per_s = base.summary.ops_per_s * 1.5;
        let up = exp("summary.ops_per_s", Direction::Increase, 1.0, 0.0, 2.0);
        let v = judge("throttling", std::slice::from_ref(&up), &base, &cand, &gate);
        assert_eq!(v.outcome, VerifyOutcome::Confirmed);
        assert!(v.lines[0].contains("summary.ops_per_s"), "{:?}", v.lines);
        let v = judge("throttling", std::slice::from_ref(&up), &cand, &base, &gate);
        assert_eq!(v.outcome, VerifyOutcome::Refuted);
        let v = judge("throttling", &[up], &base, &base, &gate);
        assert_eq!(v.outcome, VerifyOutcome::Inconclusive);
        let unknown = exp("nope", Direction::Increase, 1.0, 0.0, 2.0);
        assert_eq!(
            judge("x", &[unknown], &base, &cand, &gate).outcome,
            VerifyOutcome::Inconclusive
        );
    }
}
