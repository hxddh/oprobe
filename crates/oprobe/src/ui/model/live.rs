//! The live view's judgement while a run is still going (design v5 §9 "Live mode", U14):
//! provisional findings with a confidence that grows with the evidence, a flag for early,
//! noisy numbers, and the ETA of a tuning session. Final findings come from the analysis when
//! the run ends; these never claim more than the counts so far support.

use crate::analysis::diagnosis::Severity;
use crate::trace::Snapshot;

/// Fewer operations than this and the headline numbers are flagged as early (noisy).
pub const EARLY_OPS: u64 = 200;

/// A finding the live numbers suggest, before the analysis has run.
#[derive(Clone, Debug, PartialEq)]
pub struct Provisional {
    /// The finding code it anticipates (`throttling`, `errors`, `tail_latency`).
    pub code: &'static str,
    pub severity: Severity,
    pub text: String,
    /// 0–1: how sure the counts so far make it (the lower end of a Wilson interval against the
    /// threshold, or the share of the window measured).
    pub confidence: f64,
}

/// The lower bound of the 95 % Wilson interval of `k` successes in `n`.
pub fn wilson_lower(k: u64, n: u64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let (k, n) = (k as f64, n as f64);
    let z = 1.96f64;
    let p = k / n;
    let den = 1.0 + z * z / n;
    let centre = p + z * z / (2.0 * n);
    let margin = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt();
    ((centre - margin) / den).max(0.0)
}

/// How sure we are that a ratio of `k` in `n` exceeds `threshold`: 0 when even the observed
/// ratio is below it, rising to 1 as the interval's lower end clears it.
fn confidence_above(k: u64, n: u64, threshold: f64) -> f64 {
    if n == 0 || (k as f64 / n as f64) < threshold {
        return 0.0;
    }
    let lo = wilson_lower(k, n);
    let p = k as f64 / n as f64;
    if lo >= threshold {
        return (0.7 + 0.3 * (n as f64 / 2000.0).min(1.0)).min(0.99);
    }
    // Between the threshold and the observed ratio: partial confidence.
    (0.7 * (lo / threshold).clamp(0.0, 1.0)).max(0.05 * (p / threshold).min(1.0))
}

/// The provisional findings of snapshot `s`, most severe first.
pub fn provisional(s: &Snapshot) -> Vec<Provisional> {
    let mut out = Vec::new();
    let reqs = s.connections.requests;
    let thr = s.resilience.throttled_responses;
    if thr > 0 {
        let c = confidence_above(thr, reqs, 0.01);
        if c > 0.0 {
            let ratio = thr as f64 / reqs.max(1) as f64;
            out.push(Provisional {
                code: "throttling",
                severity: if ratio >= 0.05 {
                    Severity::Critical
                } else {
                    Severity::Warning
                },
                text: format!(
                    "throttling: {thr} of {reqs} requests ({:.1} %) so far",
                    ratio * 100.0
                ),
                confidence: c,
            });
        }
    }
    let ops = s.summary.ops;
    let failed = s.summary.failed_ops;
    if failed > 0 {
        let c = confidence_above(failed, ops, 0.001);
        if c > 0.0 {
            out.push(Provisional {
                code: "errors",
                severity: if failed as f64 / ops.max(1) as f64 >= 0.01 {
                    Severity::Critical
                } else {
                    Severity::Warning
                },
                text: format!("errors: {failed} of {ops} operations failed so far"),
                confidence: c,
            });
        }
    }
    let q = &s.summary.latency;
    if ops >= 50 && q.p50 > 0.0 && q.p99 >= 10.0 * q.p50 {
        // The tail needs ~100 samples per percent to be trusted.
        out.push(Provisional {
            code: "tail_latency",
            severity: Severity::Warning,
            text: format!(
                "tail latency: p99 is {:.0}× the median so far",
                q.p99 / q.p50
            ),
            confidence: (ops as f64 / 10_000.0).clamp(0.1, 0.9),
        });
    }
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.confidence.total_cmp(&a.confidence))
    });
    out
}

/// The headline numbers are still noisy (too few operations measured).
pub fn early(s: &Snapshot) -> bool {
    s.summary.ops < EARLY_OPS
}

/// The time left of a tuning session: the trials still to run at the mean trial length so far
/// (or the configured trial length before the first ends), plus the confirmation run.
pub fn tune_eta_s(done: usize, max_trials: usize, trial_s: f64, elapsed_s: f64) -> Option<f64> {
    if max_trials == 0 || trial_s <= 0.0 {
        return None;
    }
    let per = if done > 0 {
        (elapsed_s / done as f64).max(trial_s)
    } else {
        trial_s * 1.2
    };
    let left = max_trials.saturating_sub(done) as f64 * per + trial_s;
    Some(left.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_bounds_are_honest() {
        assert_eq!(wilson_lower(0, 0), 0.0);
        let lo = wilson_lower(5, 100);
        assert!(lo > 0.015 && lo < 0.05, "{lo}");
        assert!(wilson_lower(500, 10_000) > 0.045);
    }

    #[test]
    fn confidence_grows_with_evidence() {
        let mut s = Snapshot::default();
        s.connections.requests = 40;
        s.resilience.throttled_responses = 2;
        let early = provisional(&s);
        assert_eq!(early.len(), 1);
        assert_eq!(early[0].code, "throttling");
        let c1 = early[0].confidence;
        s.connections.requests = 20_000;
        s.resilience.throttled_responses = 1_000;
        let later = provisional(&s);
        assert!(later[0].confidence > c1, "{} <= {c1}", later[0].confidence);
        assert_eq!(later[0].severity, Severity::Critical);
        // Below the threshold: nothing provisional.
        s.resilience.throttled_responses = 10;
        assert!(provisional(&s).is_empty());
        assert!(super::early(&s));
    }

    #[test]
    fn the_tune_eta_counts_the_trials_left_and_the_confirmation() {
        assert_eq!(tune_eta_s(0, 0, 10.0, 0.0), None);
        let e = tune_eta_s(2, 10, 10.0, 30.0).unwrap();
        assert!((e - (8.0 * 15.0 + 10.0)).abs() < 1e-9, "{e}");
        assert!(tune_eta_s(10, 10, 10.0, 100.0).unwrap() <= 10.0 + 1e-9);
    }
}
