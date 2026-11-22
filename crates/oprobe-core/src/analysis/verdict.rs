//! Two-sided net verdicts (design v5 §4, Verify 5 and compare): what a change did overall.
//!
//! verify, compare and the App share this module, so they can never disagree:
//! - **intervals** that cannot be impossible: a log-ratio interval for positive metrics
//!   (throughput, latency: never below −100 %), paired when runs are interleaved
//!   ([`log_ratio`]); Wilson score intervals for ratios ([`wilson`]) and Newcombe's interval for
//!   the difference of two ratios ([`ratio_difference`]), always inside `[−1, 1]` (fixes A12);
//! - **the net verdict** ([`assess`]): `improved` (better, nothing significantly worse),
//!   `regressed`, `traded` (lists both sides), `no_change` (tight intervals within the noise) or
//!   `inconclusive`;
//! - **a sequential probability ratio test** ([`Sprt`]) over interleaved A/B pairs, so verify
//!   stops as soon as the evidence is in (fixes A2).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The net effect of a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetVerdict {
    /// Better on the objective, nothing significantly worse.
    Improved,
    /// Worse on the objective.
    Regressed,
    /// Some metrics better, others worse (e.g. "throttling −4.6 pp, goodput −21 %").
    Traded,
    /// No significant change.
    NoChange,
    /// Not enough evidence either way (never shown as "same").
    Inconclusive,
}

impl NetVerdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Improved => "improved",
            Self::Regressed => "regressed",
            Self::Traded => "traded",
            Self::NoChange => "no change",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// One metric that moved, with its relative change.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MetricMove {
    /// `ops_per_s`, `p99_ms`, `throttled_request_ratio`, …
    pub metric: String,
    pub delta_ratio: f64,
}

/// A net verdict with what got better and what got worse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NetAssessment {
    pub verdict: NetVerdict,
    #[serde(default)]
    pub better: Vec<MetricMove>,
    #[serde(default)]
    pub worse: Vec<MetricMove>,
}

impl NetAssessment {
    /// One sentence: "traded: throttled_request_ratio −95.0 %, ops_per_s −21.0 %".
    pub fn summary(&self) -> String {
        let fmt = |m: &MetricMove| format!("{} {:+.1} %", m.metric, m.delta_ratio * 100.0);
        let moves: Vec<String> = self.better.iter().chain(&self.worse).map(fmt).collect();
        if moves.is_empty() {
            self.verdict.label().to_owned()
        } else {
            format!("{}: {}", self.verdict.label(), moves.join(", "))
        }
    }
}

/// One metric's change between two sides, ready for [`assess`].
#[derive(Clone, Debug, PartialEq)]
pub struct MetricChange {
    pub metric: String,
    /// The change: relative (`+0.1` = 10 % higher) for positive metrics, absolute (percentage
    /// points as a fraction) for ratios.
    pub change: super::stats::Interval,
    /// Lower values are better (latency, throttling, errors).
    pub lower_is_better: bool,
    /// Changes smaller than this are noise.
    pub floor: f64,
}

impl MetricChange {
    /// The interval excludes zero and the estimate exceeds the noise floor.
    pub fn significant(&self) -> bool {
        self.change.excludes_zero() && self.change.estimate.abs() > self.floor
    }
    /// The whole interval lies within the noise floor.
    pub fn negligible(&self) -> bool {
        self.change.low >= -self.floor && self.change.high <= self.floor
    }
    fn better(&self) -> bool {
        self.significant() && ((self.change.estimate < 0.0) == self.lower_is_better)
    }
    fn worse(&self) -> bool {
        self.significant() && !self.better()
    }
}

/// The net verdict over `changes`; `objective` names the metric a regression is judged on
/// (any significantly worse metric with nothing better is a regression as well).
pub fn assess(changes: &[MetricChange]) -> NetAssessment {
    let mv = |c: &MetricChange| MetricMove {
        metric: c.metric.clone(),
        delta_ratio: c.change.estimate,
    };
    let better: Vec<MetricMove> = changes.iter().filter(|c| c.better()).map(mv).collect();
    let worse: Vec<MetricMove> = changes.iter().filter(|c| c.worse()).map(mv).collect();
    let verdict = match (better.is_empty(), worse.is_empty()) {
        (false, true) => NetVerdict::Improved,
        (true, false) => NetVerdict::Regressed,
        (false, false) => NetVerdict::Traded,
        (true, true) if !changes.is_empty() && changes.iter().all(MetricChange::negligible) => {
            NetVerdict::NoChange
        }
        (true, true) => NetVerdict::Inconclusive,
    };
    NetAssessment {
        verdict,
        better,
        worse,
    }
}

/// Two-sided 97.5 % quantile of Student's t.
fn t975(df: f64) -> f64 {
    super::compare::t975(df)
}

fn mean_var(v: &[f64]) -> (f64, f64) {
    let n = v.len() as f64;
    if n == 0.0 {
        return (0.0, 0.0);
    }
    let m = v.iter().sum::<f64>() / n;
    let var = if v.len() > 1 {
        v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    (m, var)
}

/// The relative change of `b` over `a` (positive values) with a 95 % interval on the log
/// ratio: paired (`ln bᵢ − ln aᵢ`, the interleaved pairs of verify) when `paired` and the
/// sides are as long, else Welch. The interval is `exp(·) − 1`: never below −100 %.
pub fn log_ratio(a: &[f64], b: &[f64], paired: bool) -> super::stats::Interval {
    let la: Vec<f64> = a.iter().filter(|x| **x > 0.0).map(|x| x.ln()).collect();
    let lb: Vec<f64> = b.iter().filter(|x| **x > 0.0).map(|x| x.ln()).collect();
    if la.is_empty() || lb.is_empty() {
        return super::stats::Interval::default();
    }
    let (d, h) = if paired && la.len() == lb.len() && la.len() >= 2 {
        let diffs: Vec<f64> = la.iter().zip(&lb).map(|(x, y)| y - x).collect();
        let (m, v) = mean_var(&diffs);
        let n = diffs.len() as f64;
        (m, t975(n - 1.0) * (v / n).sqrt())
    } else {
        let ((ma, mut va), (mb, mut vb)) = (mean_var(&la), mean_var(&lb));
        let (na, nb) = (la.len() as f64, lb.len() as f64);
        if la.len() < 2 {
            va = vb;
        }
        if lb.len() < 2 {
            vb = va;
        }
        let se2 = va / na + vb / nb;
        let den = (va / na).powi(2) / (na - 1.0).max(1.0) + (vb / nb).powi(2) / (nb - 1.0).max(1.0);
        let df = if den > 0.0 { se2 * se2 / den } else { na + nb - 2.0 };
        (mb - ma, t975(df) * se2.sqrt())
    };
    super::stats::Interval {
        estimate: d.exp() - 1.0,
        low: (d - h).exp() - 1.0,
        high: (d + h).exp() - 1.0,
    }
}

/// Wilson score interval (95 %) of `k` successes in `n` trials: always inside `[0, 1]`.
pub fn wilson(k: f64, n: f64) -> (f64, f64) {
    if n <= 0.0 {
        return (0.0, 1.0);
    }
    let z = 1.959_964f64;
    let p = (k / n).clamp(0.0, 1.0);
    let den = 1.0 + z * z / n;
    let centre = (p + z * z / (2.0 * n)) / den;
    let half = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / den;
    ((centre - half).max(0.0), (centre + half).min(1.0))
}

/// The difference `kb/nb − ka/na` of two ratios with Newcombe's hybrid score interval (95 %):
/// always inside `[−1, 1]`, sensible at 0 events.
pub fn ratio_difference(ka: f64, na: f64, kb: f64, nb: f64) -> super::stats::Interval {
    let (pa, pb) = (ka / na.max(1.0), kb / nb.max(1.0));
    let (la, ua) = wilson(ka, na);
    let (lb, ub) = wilson(kb, nb);
    let d = pb - pa;
    super::stats::Interval {
        estimate: d,
        low: (d - ((pb - lb).powi(2) + (ua - pa).powi(2)).sqrt()).max(-1.0),
        high: (d + ((ub - pb).powi(2) + (pa - la).powi(2)).sqrt()).min(1.0),
    }
}

/// What an [`Sprt`] concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SprtDecision {
    /// The change exceeds the minimum effect (either direction).
    Changed,
    /// The change is smaller than the minimum effect.
    Unchanged,
    /// Keep sampling.
    Continue,
}

/// A two-sided sequential probability ratio test on paired log ratios `ln(bᵢ/aᵢ)`: H0 "no
/// change" against H1 "a change of at least `delta` either way", with error rates `alpha` and
/// `beta`. The variance is estimated from the pairs (with a floor), so it works from the second
/// pair on.
#[derive(Clone, Debug)]
pub struct Sprt {
    delta: f64,
    alpha: f64,
    beta: f64,
    diffs: Vec<f64>,
}

impl Sprt {
    pub fn new(delta: f64, alpha: f64, beta: f64) -> Self {
        Self {
            delta: delta.max(1e-3),
            alpha,
            beta,
            diffs: Vec::new(),
        }
    }

    /// Add one interleaved pair (baseline, candidate) of a positive metric.
    pub fn push(&mut self, a: f64, b: f64) {
        if a > 0.0 && b > 0.0 {
            self.diffs.push((b / a).ln());
        }
    }

    pub fn pairs(&self) -> usize {
        self.diffs.len()
    }

    /// The decision after the pairs so far.
    pub fn decision(&self) -> SprtDecision {
        let n = self.diffs.len();
        if n < 2 {
            return SprtDecision::Continue;
        }
        let (_, var) = mean_var(&self.diffs);
        // Floor: 1 % of the effect, so identical pairs do not divide by zero.
        let var = var.max((0.1 * self.delta).powi(2));
        let sum: f64 = self.diffs.iter().sum();
        let nf = n as f64;
        // log LR of N(±δ, σ²) against N(0, σ²), the larger of the two directions.
        let llr = |mu: f64| (mu * sum - nf * mu * mu / 2.0) / var;
        let up = llr(self.delta).max(llr(-self.delta));
        let a = ((1.0 - self.beta) / self.alpha).ln();
        let b = (self.beta / (1.0 - self.alpha)).ln();
        if up >= a {
            SprtDecision::Changed
        } else if up <= b {
            SprtDecision::Unchanged
        } else {
            SprtDecision::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::stats::Interval;

    fn ch(metric: &str, est: f64, lo: f64, hi: f64, lower_better: bool) -> MetricChange {
        MetricChange {
            metric: metric.into(),
            change: Interval {
                estimate: est,
                low: lo,
                high: hi,
            },
            lower_is_better: lower_better,
            floor: 0.02,
        }
    }

    #[test]
    fn net_verdicts_are_two_sided() {
        let thr_down = ch("throttled_request_ratio", -0.046, -0.05, -0.04, true);
        let goodput_down = ch("ops_per_s", -0.21, -0.25, -0.17, false);
        let goodput_up = ch("ops_per_s", 0.3, 0.2, 0.4, false);
        let p99_flat = ch("p99_ms", 0.0, -0.01, 0.01, true);
        let p99_noisy = ch("p99_ms", 0.05, -0.3, 0.4, true);
        assert_eq!(assess(&[thr_down.clone(), goodput_down.clone()]).verdict, NetVerdict::Traded);
        assert_eq!(assess(&[goodput_up.clone(), p99_flat.clone()]).verdict, NetVerdict::Improved);
        assert_eq!(assess(&[goodput_down, p99_flat.clone()]).verdict, NetVerdict::Regressed);
        assert_eq!(assess(&[p99_flat.clone()]).verdict, NetVerdict::NoChange);
        assert_eq!(assess(&[p99_flat, p99_noisy]).verdict, NetVerdict::Inconclusive);
        let t = assess(&[thr_down, goodput_up]);
        assert_eq!(t.verdict, NetVerdict::Improved);
        assert_eq!(t.better.len(), 2);
    }

    #[test]
    fn intervals_are_never_impossible() {
        // Two runs per side, a big drop: the log-ratio interval stays above −100 %.
        let i = log_ratio(&[100.0, 140.0], &[2.0, 1.0], false);
        assert!(i.low > -1.0 && i.estimate < -0.9, "{i:?}");
        let p = log_ratio(&[100.0, 102.0, 98.0], &[80.0, 81.0, 79.0], true);
        assert!(p.low > -0.25 && p.high < -0.15, "{p:?}");
        let (lo, hi) = wilson(0.0, 1000.0);
        assert_eq!(lo, 0.0);
        assert!(hi > 0.0 && hi < 0.01);
        let d = ratio_difference(50.0, 1000.0, 0.0, 1000.0);
        assert!(d.low >= -1.0 && d.high < 0.0, "{d:?}");
        assert!((d.estimate + 0.05).abs() < 1e-9);
    }

    #[test]
    fn the_sprt_stops_early_on_clear_evidence() {
        let mut s = Sprt::new(0.05, 0.05, 0.1);
        s.push(100.0, 130.0);
        s.push(101.0, 131.0);
        assert_eq!(s.decision(), SprtDecision::Changed);
        let mut s = Sprt::new(0.05, 0.05, 0.1);
        for _ in 0..4 {
            s.push(100.0, 100.2);
        }
        assert_eq!(s.decision(), SprtDecision::Unchanged);
        let mut s = Sprt::new(0.05, 0.05, 0.1);
        s.push(100.0, 140.0);
        s.push(100.0, 70.0);
        assert_eq!(s.decision(), SprtDecision::Continue);
    }

    #[test]
    fn summaries_name_both_sides_of_a_trade() {
        let a = NetAssessment {
            verdict: NetVerdict::Traded,
            better: vec![MetricMove {
                metric: "throttled_request_ratio".into(),
                delta_ratio: -0.95,
            }],
            worse: vec![MetricMove {
                metric: "ops_per_s".into(),
                delta_ratio: -0.21,
            }],
        };
        assert_eq!(
            a.summary(),
            "traded: throttled_request_ratio -95.0 %, ops_per_s -21.0 %"
        );
        assert_eq!(
            serde_json::to_value(NetVerdict::NoChange).unwrap(),
            "no_change"
        );
    }
}
