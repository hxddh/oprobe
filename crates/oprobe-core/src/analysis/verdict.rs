//! The statistics `oprobe prove` decides with (design v6 §8 "Prove", T9):
//! - **intervals that cannot be impossible**: a log-ratio interval for positive metrics
//!   (goodput, latency: `exp(·) − 1`, never below −100 %), paired over interleaved A/B pairs
//!   ([`log_ratio`]); Wilson score intervals for shares ([`wilson`]) and Newcombe's interval
//!   for the difference of two shares ([`ratio_difference`]), always inside `[−1, 1]`;
//! - **a sequential probability ratio test** ([`Sprt`]) on the paired log ratios against a
//!   noise floor learned from A/A pairs, so prove stops as soon as the evidence is in.

use super::stats::Interval;

/// Two-sided 97.5 % quantile of Student's t.
pub fn t975(df: f64) -> f64 {
    const T: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    if !df.is_finite() || df < 1.0 {
        return T[0];
    }
    let i = df.floor() as usize;
    if i > 30 { 1.96 } else { T[i - 1] }
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
/// ratio: paired (`ln bᵢ − ln aᵢ`, interleaved pairs) when `paired` and the sides are as long,
/// else Welch. `floor_sd` is a known lower bound of the per-pair spread (the A/A noise floor),
/// so two identical pairs never claim a zero-width interval. The interval is `exp(·) − 1`:
/// never below −100 %.
pub fn log_ratio(a: &[f64], b: &[f64], paired: bool, floor_sd: f64) -> Interval {
    let la: Vec<f64> = a.iter().filter(|x| **x > 0.0).map(|x| x.ln()).collect();
    let lb: Vec<f64> = b.iter().filter(|x| **x > 0.0).map(|x| x.ln()).collect();
    if la.is_empty() || lb.is_empty() {
        return Interval::default();
    }
    let (d, h) = if paired && la.len() == lb.len() {
        let diffs: Vec<f64> = la.iter().zip(&lb).map(|(x, y)| y - x).collect();
        let (m, v) = mean_var(&diffs);
        let n = diffs.len() as f64;
        let v = v.max(floor_sd * floor_sd);
        (m, t975((n - 1.0).max(1.0)) * (v / n).sqrt())
    } else {
        let ((ma, va), (mb, vb)) = (mean_var(&la), mean_var(&lb));
        let (na, nb) = (la.len() as f64, lb.len() as f64);
        let f = floor_sd * floor_sd;
        let (va, vb) = (va.max(f), vb.max(f));
        let se2 = va / na + vb / nb;
        let den = (va / na).powi(2) / (na - 1.0).max(1.0) + (vb / nb).powi(2) / (nb - 1.0).max(1.0);
        let df = if den > 0.0 {
            se2 * se2 / den
        } else {
            na + nb - 2.0
        };
        (mb - ma, t975(df) * se2.sqrt())
    };
    Interval {
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

/// The difference `kb/nb − ka/na` of two shares with Newcombe's hybrid score interval (95 %):
/// always inside `[−1, 1]`, sensible at 0 events.
pub fn ratio_difference(ka: f64, na: f64, kb: f64, nb: f64) -> Interval {
    let (pa, pb) = (ka / na.max(1.0), kb / nb.max(1.0));
    let (la, ua) = wilson(ka, na);
    let (lb, ub) = wilson(kb, nb);
    let d = pb - pa;
    Interval {
        estimate: d,
        low: (d - ((pb - lb).powi(2) + (ua - pa).powi(2)).sqrt()).max(-1.0),
        high: (d + ((ub - pb).powi(2) + (pa - la).powi(2)).sqrt()).min(1.0),
    }
}

/// What an [`Sprt`] concluded about the candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SprtDecision {
    /// Better by at least the minimum effect.
    Better,
    /// Worse by at least the minimum effect.
    Worse,
    /// Within the minimum effect of the baseline.
    Same,
    /// Keep sampling.
    Continue,
}

/// A sequential probability ratio test on paired log ratios `dᵢ = ln(bᵢ/aᵢ)`, signed so that
/// positive is better: H0 "no change" against H1+ "better by `delta`" and H1− "worse by
/// `delta`", with error rates `alpha` and `beta`. The per-pair spread is the larger of the
/// pairs' own and the A/A noise floor (`noise_sd`), so two lucky pairs cannot decide alone.
#[derive(Clone, Debug)]
pub struct Sprt {
    delta: f64,
    noise_sd: f64,
    alpha: f64,
    beta: f64,
    diffs: Vec<f64>,
}

impl Sprt {
    pub fn new(delta: f64, noise_sd: f64, alpha: f64, beta: f64) -> Self {
        Self {
            delta: delta.max(1e-3),
            noise_sd: noise_sd.max(1e-3),
            alpha,
            beta,
            diffs: Vec::new(),
        }
    }

    /// Add one pair's signed log ratio (positive = the candidate is better).
    pub fn push(&mut self, d: f64) {
        if d.is_finite() {
            self.diffs.push(d);
        }
    }

    pub fn pairs(&self) -> usize {
        self.diffs.len()
    }

    /// The decision after the pairs so far (at least `min_pairs`).
    pub fn decision(&self, min_pairs: usize) -> SprtDecision {
        let n = self.diffs.len();
        if n < min_pairs.max(1) {
            return SprtDecision::Continue;
        }
        let (_, var) = mean_var(&self.diffs);
        let var = var.max(self.noise_sd * self.noise_sd);
        let sum: f64 = self.diffs.iter().sum();
        let nf = n as f64;
        // log LR of N(μ, σ²) against N(0, σ²).
        let llr = |mu: f64| (mu * sum - nf * mu * mu / 2.0) / var;
        let (up, down) = (llr(self.delta), llr(-self.delta));
        let accept = ((1.0 - self.beta) / self.alpha).ln();
        let reject = (self.beta / (1.0 - self.alpha)).ln();
        if up >= accept {
            SprtDecision::Better
        } else if down >= accept {
            SprtDecision::Worse
        } else if up <= reject && down <= reject {
            SprtDecision::Same
        } else {
            SprtDecision::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_are_never_impossible() {
        // A ratio cannot fall below −100 % (T9).
        let i = log_ratio(&[100.0, 120.0], &[1.0, 50.0], true, 0.02);
        assert!(i.low > -1.0 && i.estimate > -1.0, "{i:?}");
        let i = log_ratio(&[100.0], &[3.0], false, 0.05);
        assert!(i.low > -1.0, "{i:?}");
        let (lo, hi) = wilson(0.0, 10.0);
        assert!(lo == 0.0 && hi < 1.0);
        let d = ratio_difference(10.0, 10.0, 0.0, 10.0);
        assert!(d.low >= -1.0 && d.high <= 1.0);
        // Identical pairs still carry the noise floor.
        let i = log_ratio(&[100.0, 100.0], &[100.0, 100.0], true, 0.03);
        assert!(i.low < 0.0 && i.high > 0.0, "{i:?}");
    }

    #[test]
    fn the_sprt_stops_early_on_clear_evidence_and_needs_more_for_noise() {
        let mut s = Sprt::new(0.05, 0.02, 0.05, 0.1);
        s.push(0.4);
        assert_eq!(s.decision(2), SprtDecision::Continue);
        s.push(0.38);
        assert_eq!(s.decision(2), SprtDecision::Better);
        let mut w = Sprt::new(0.05, 0.02, 0.05, 0.1);
        w.push(-0.3);
        w.push(-0.32);
        assert_eq!(w.decision(2), SprtDecision::Worse);
        // Noise around zero: no early verdict either way.
        let mut n = Sprt::new(0.05, 0.08, 0.05, 0.1);
        n.push(0.06);
        n.push(-0.05);
        assert_eq!(n.decision(2), SprtDecision::Continue);
        let mut z = Sprt::new(0.1, 0.01, 0.05, 0.1);
        for _ in 0..4 {
            z.push(0.0);
        }
        assert_eq!(z.decision(2), SprtDecision::Same);
    }
}
