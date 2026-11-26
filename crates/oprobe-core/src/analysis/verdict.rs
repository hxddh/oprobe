//! The statistics `oprobe prove` decides with (design v6 §8 "Prove", T9):
//! - **the A/B decision** ([`AbTest`]): interleaved pairs in a balanced, seeded order
//!   ([`candidate_first`]), an estimate that a constant order effect cannot bias, and one
//!   two-sided sequential boundary ([`critical`]) that the verdict, the interval and every
//!   look share, so the chance of ever calling a change that is none stays under 5 %
//!   whenever prove stops;
//! - **the noise floor** ([`noise_floor_ab`]): the per-pair spread learned from each side's
//!   trial-to-trial differences, never below the trials' count resolution;
//! - **intervals that cannot be impossible**: a log-ratio interval for positive metrics
//!   (goodput, latency: `exp(·) − 1`, never below −100 %, [`log_ratio`]); Wilson score
//!   intervals for shares ([`wilson`]) and Newcombe's interval for the difference of two
//!   shares ([`ratio_difference`]), always inside `[−1, 1]`;
//! - **a sequential probability ratio test** ([`Sprt`]) that only ever stops the sampling
//!   early: for "no change worth the minimum effect", or once a change the boundary already
//!   shows is at least that effect. It cannot make a verdict the boundary does not make.

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

/// The smallest noise floor assumed before the trials have spoken (per-pair log ratio).
pub const MIN_NOISE_SD: f64 = 0.02;
/// The smallest effect the sequential test looks for.
pub const MIN_EFFECT: f64 = 0.03;
/// A/B pairs at least before a verdict other than `inconclusive`: with fewer, the spread of
/// the pairs cannot be estimated.
pub const MIN_DECISIVE_PAIRS: usize = 3;
/// A/B pairs at most: the sequential boundary ([`critical`]) is calibrated up to here.
pub const MAX_PAIRS: usize = 12;
/// The two-sided error rate the sequential boundary is calibrated to stay under.
const ALPHA: f64 = 0.05;
/// The share of real minimum effects the futility stop may miss.
const BETA: f64 = 0.10;

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

/// A sequential probability ratio test on the mean signed log ratio of the pairs (positive is
/// better): H0 "no change" against H1+ "better by `delta`" and H1− "worse by `delta`". `alpha`
/// is two-sided, so each side takes half of it; `beta` is the share of real effects a "same"
/// may miss. [`AbTest`] uses it only to stop sampling early, never to make a verdict.
#[derive(Clone, Copy, Debug)]
pub struct Sprt {
    delta: f64,
    alpha: f64,
    beta: f64,
}

impl Sprt {
    pub fn new(delta: f64, alpha: f64, beta: f64) -> Self {
        Self {
            delta: delta.max(1e-3),
            alpha,
            beta,
        }
    }

    /// The decision after `n` pairs whose signed log ratios average `mean` with a per-pair
    /// spread `sd`.
    pub fn decision(&self, n: usize, mean: f64, sd: f64) -> SprtDecision {
        if n == 0 || !mean.is_finite() {
            return SprtDecision::Continue;
        }
        let nf = n as f64;
        let var = sd.max(1e-3).powi(2);
        // log LR of N(μ, σ²) against N(0, σ²).
        let llr = |mu: f64| nf * (mu * mean - mu * mu / 2.0) / var;
        let (up, down) = (llr(self.delta), llr(-self.delta));
        let side = self.alpha / 2.0;
        let accept = ((1.0 - self.beta) / side).ln();
        let reject = (self.beta / (1.0 - side)).ln();
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

/// The smallest change a trial of `ops` operations can resolve, as a log ratio: one operation
/// more or less is `ln(1 + 1/ops)` (2.7 % at 37 operations). Below it, a run of A/A trials that
/// happen to agree exactly says nothing about the noise (0 when nothing was counted).
pub fn count_resolution(ops: u64) -> f64 {
    if ops == 0 {
        0.0
    } else {
        (1.0 + 1.0 / ops as f64).ln()
    }
}

/// The mean squared log ratio of consecutive trials of one side, with how many ratios it
/// averages: twice that side's trial-to-trial variance (von Neumann), robust to a slow drift.
fn successive_msd(x: &[f64]) -> (f64, usize) {
    let d: Vec<f64> = x
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0)
        .map(|w| (w[1] / w[0]).ln())
        .collect();
    let msd = if d.is_empty() {
        0.0
    } else {
        d.iter().map(|x| x * x).sum::<f64>() / d.len() as f64
    };
    (msd, d.len())
}

/// The noise floor of a pair's log ratio `ln(b/a)`: its variance is A's trial-to-trial
/// variance plus B's, each estimated from that side's consecutive trials
/// ([`successive_msd`]: `σ² ≈ MSSD / 2`), so `floor² = (MSSD_A + MSSD_B) / 2`. Until B has
/// two trials it is assumed as noisy as A (an A/A pair: `floor² = MSSD_A`).
/// W8: 9.0 took the A/A spread alone, which overstates a pair's spread by up to √2 when the
/// change steadies B (a cap that ends the throttling of a throttled baseline). At least
/// [`MIN_NOISE_SD`] and the count `resolution` ([`count_resolution`]), and doubled while A has
/// fewer than two differences (they cannot say how precise the floor is). An order effect
/// (the second trial of a pair running better or worse) only raises it.
pub fn noise_floor_ab(a: &[f64], b: &[f64], resolution: f64) -> f64 {
    let (msd_a, na) = successive_msd(a);
    let (msd_b, nb) = successive_msd(b);
    let var = if nb == 0 {
        msd_a
    } else {
        (msd_a + msd_b) / 2.0
    };
    let floor = var.sqrt().max(MIN_NOISE_SD).max(resolution);
    if na < 2 { 2.0 * floor } else { floor }
}

/// Whether pair `pair` (0-based) runs its candidate (B) first. The orders come in blocks of
/// two pairs, one `AB` and one `BA`, and which comes first in a block is a fair coin drawn
/// from `seed` (splitmix64): random across blocks, the same for the same run, and after any
/// number of pairs the two orders differ by at most one. 9.1 drew every pair's order alone:
/// one proof in four ran its first three pairs in the same order, and an order effect (the
/// second trial of a pair running warmer, or against a drained limit) read as the change.
pub fn candidate_first(seed: u64, pair: u64) -> bool {
    let mut z = seed.wrapping_add((pair / 2 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    let block_starts_with_candidate = (z ^ (z >> 31)) >> 63 == 1;
    block_starts_with_candidate == pair.is_multiple_of(2)
}

/// `CRITICAL[K − 3][k − 3]`: the critical value of the sequential test at look `k` (after `k`
/// pairs) of a proof planned for at most `K` pairs, `3 ≤ k ≤ K ≤ 12`.
///
/// The statistic is `T_k = estimate_k / (s_k · √w_k)` as [`AbTest`] computes it (the 2×2
/// crossover estimate, `w_k = (1/n_AB + 1/n_BA) / 4`, `s_k` the sample SD of the `k` log
/// ratios), over orders drawn as [`candidate_first`] draws them. Each row holds one nominal
/// two-sided level at every look (Pocock), chosen so that under the null (i.i.d. normal log
/// ratios; the statistic is scale-free) `P(|T_k| > CRITICAL[K − 3][k − 3] for any k in 3..=K)`
/// is 4.0 % — under the 5 % the verdict promises, with margin for the simulation error and
/// for noise that is not quite normal. The per-look levels are 4.00, 2.40, 1.80, 1.47, 1.26,
/// 1.12, 1.02, 0.93, 0.87 and 0.82 % for K = 3..=12.
///
/// Calibrated offline by simulation: 8 000 000 proofs of 12 pairs (numpy, seed 20261126), the
/// level of each row bisected until the crossing rate was 0.0400, each value rounded up to
/// 0.001. A second run on 8 000 000 fresh proofs (seed 99) gave every row a crossing rate of
/// 0.0400 to 0.0401. The spread [`AbTest`] divides by is never below `s_k` (it is the larger
/// of `s_k` and the noise floor), so its error rate is at most the calibrated one.
const CRITICAL: [&[f64]; MAX_PAIRS - MIN_DECISIVE_PAIRS + 1] = [
    &[4.607],
    &[6.000, 4.238],
    &[6.968, 4.725, 3.814],
    &[7.723, 5.089, 4.053, 3.659],
    &[8.326, 5.373, 4.236, 3.801, 3.493],
    &[8.846, 5.616, 4.387, 3.920, 3.593, 3.418],
    &[9.285, 5.812, 4.507, 4.017, 3.671, 3.490, 3.334],
    &[9.696, 5.993, 4.620, 4.102, 3.743, 3.553, 3.390, 3.292],
    &[
        10.053, 6.141, 4.713, 4.173, 3.803, 3.605, 3.436, 3.334, 3.242,
    ],
    &[
        10.381, 6.279, 4.800, 4.239, 3.857, 3.652, 3.479, 3.374, 3.279, 3.215,
    ],
];

/// The critical value of the interval after `pairs` pairs of a proof planned for at most
/// `max_pairs` ([`CRITICAL`]). Before the first look (fewer than [`MIN_DECISIVE_PAIRS`]) no
/// verdict is read, and the interval shown is Student's plain one on one degree of freedom
/// (12.706), wider than any look's. Both counts are clamped to the calibrated range.
pub fn critical(max_pairs: usize, pairs: usize) -> f64 {
    if pairs < MIN_DECISIVE_PAIRS {
        return t975(1.0);
    }
    let planned = max_pairs.max(pairs).min(MAX_PAIRS);
    let row = CRITICAL[planned - MIN_DECISIVE_PAIRS];
    row[pairs.min(planned) - MIN_DECISIVE_PAIRS]
}

/// One interleaved A/B pair: the objective of its baseline and candidate trials.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AbPair {
    pub baseline: f64,
    pub candidate: f64,
    /// The fewest operations either trial counted (the objective's resolution).
    pub ops_count: u64,
    /// The candidate ran first ([`candidate_first`]).
    pub candidate_first: bool,
}

/// What the pairs so far say about the candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbVerdict {
    Improved,
    Worse,
    Inconclusive,
}

/// Whether an [`AbTest`] wants another pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbDecision {
    /// Run another pair.
    Continue,
    /// The interval excludes no change, by at least the minimum effect.
    Decided,
    /// More pairs would not show a change of the minimum effect.
    Futile,
    /// Every planned pair ran.
    Complete,
}

/// The A/B decision of `oprobe prove`, one pair at a time: pure and synchronous, so the whole
/// rule can be simulated.
///
/// - **Estimate.** The signed log ratio of each pair (positive: the candidate is better). With
///   both orders present it is the mean of the AB-order mean and the BA-order mean (the 2×2
///   crossover estimate): a constant order effect — the second trial of a pair running better
///   or worse — cancels exactly, also when the orders differ by one.
/// - **Spread.** The larger of the pairs' own sample SD (which an order effect only inflates)
///   and the noise floor ([`noise_floor_ab`]); the standard error is that spread times
///   `√((1/n_AB + 1/n_BA) / 4)`.
/// - **Boundary.** One two-sided sequential boundary ([`critical`]): the interval after each
///   pair is a repeated confidence interval, and the verdict is whether it excludes no
///   change. Whatever stops the sampling — the early stop below, the pair limit, a budget —
///   the chance of a verdict on a change that is none stays under 5 %.
/// - **Early stop.** From `min_pairs` on, sampling stops once the interval excludes no change
///   and an [`Sprt`] agrees the change is at least the minimum effect, or once the SPRT says
///   no change of that effect is there (futility). Its effect and spread are plug-ins; they
///   only choose when to stop.
#[derive(Clone, Debug)]
pub struct AbTest {
    higher_is_better: bool,
    min_pairs: usize,
    max_pairs: usize,
    pairs: Vec<AbPair>,
}

impl AbTest {
    /// A test of a metric where more is better (`higher_is_better`: goodput) or less is
    /// (p99), looking from `min_pairs` to `max_pairs` (clamped to
    /// [`MIN_DECISIVE_PAIRS`]..=[`MAX_PAIRS`]).
    pub fn new(higher_is_better: bool, min_pairs: usize, max_pairs: usize) -> Self {
        let max_pairs = max_pairs.clamp(MIN_DECISIVE_PAIRS, MAX_PAIRS);
        Self {
            higher_is_better,
            min_pairs: min_pairs.clamp(MIN_DECISIVE_PAIRS, max_pairs),
            max_pairs,
            pairs: Vec::new(),
        }
    }

    /// Add a completed pair; whether to run another.
    pub fn push(&mut self, pair: AbPair) -> AbDecision {
        self.pairs.push(pair);
        self.decision()
    }

    /// Pairs so far.
    pub fn pairs(&self) -> usize {
        self.pairs.len()
    }

    /// The noise floor after the pairs so far (a per-pair log-ratio spread).
    pub fn noise_floor(&self) -> f64 {
        let a: Vec<f64> = self.pairs.iter().map(|p| p.baseline).collect();
        let b: Vec<f64> = self.pairs.iter().map(|p| p.candidate).collect();
        let resolution = self
            .pairs
            .iter()
            .map(|p| p.ops_count)
            .min()
            .map_or(0.0, count_resolution);
        noise_floor_ab(&a, &b, resolution)
    }

    /// `(estimate, half-width, spread)` of the mean signed log ratio, positive = better.
    fn estimate(&self) -> Option<(f64, f64, f64)> {
        let n = self.pairs.len();
        if n == 0 {
            return None;
        }
        let d: Vec<f64> = self
            .pairs
            .iter()
            .map(|p| {
                let (a, b) = (p.baseline.max(1e-9), p.candidate.max(1e-9));
                if self.higher_is_better {
                    (b / a).ln()
                } else {
                    (a / b).ln()
                }
            })
            .collect();
        let (mean, var) = mean_var(&d);
        let of_order = |first: bool| {
            let v: Vec<f64> = d
                .iter()
                .zip(&self.pairs)
                .filter(|(_, p)| p.candidate_first == first)
                .map(|(x, _)| *x)
                .collect();
            (mean_var(&v).0, v.len() as f64)
        };
        let ((m_ba, n_ba), (m_ab, n_ab)) = (of_order(true), of_order(false));
        let (est, w) = if n_ba > 0.0 && n_ab > 0.0 {
            ((m_ab + m_ba) / 2.0, (1.0 / n_ab + 1.0 / n_ba) / 4.0)
        } else {
            (mean, 1.0 / n as f64)
        };
        let sd = var.sqrt().max(self.noise_floor());
        Some((est, critical(self.max_pairs, n) * sd * w.sqrt(), sd))
    }

    /// The objective's relative change (candidate vs baseline) with its sequential,
    /// order-adjusted 95 % interval, in the metric's own direction (goodput +: more; p99 −:
    /// faster), as `exp(·) − 1` — never below −100 %.
    pub fn interval(&self) -> Interval {
        let Some((est, half, _)) = self.estimate() else {
            return Interval::default();
        };
        let (est, lo, hi) = if self.higher_is_better {
            (est, est - half, est + half)
        } else {
            (-est, -est - half, -est + half)
        };
        Interval {
            estimate: est.exp() - 1.0,
            low: lo.exp() - 1.0,
            high: hi.exp() - 1.0,
        }
    }

    /// The verdict of the pairs so far: whether the interval excludes no change (never with
    /// fewer than [`MIN_DECISIVE_PAIRS`]).
    pub fn verdict(&self) -> AbVerdict {
        match self.estimate() {
            Some((est, half, _)) if self.pairs.len() >= MIN_DECISIVE_PAIRS => {
                if est - half > 0.0 {
                    AbVerdict::Improved
                } else if est + half < 0.0 {
                    AbVerdict::Worse
                } else {
                    AbVerdict::Inconclusive
                }
            }
            _ => AbVerdict::Inconclusive,
        }
    }

    /// Whether to run another pair.
    pub fn decision(&self) -> AbDecision {
        let n = self.pairs.len();
        let Some((est, _, sd)) = self.estimate().filter(|_| n >= self.min_pairs) else {
            return AbDecision::Continue;
        };
        let delta = (2.0 * self.noise_floor()).max(MIN_EFFECT);
        let sprt = Sprt::new(delta, ALPHA, BETA).decision(n, est, sd);
        match (self.verdict(), sprt) {
            (AbVerdict::Improved, SprtDecision::Better)
            | (AbVerdict::Worse, SprtDecision::Worse) => AbDecision::Decided,
            (_, SprtDecision::Same) => AbDecision::Futile,
            _ if n >= self.max_pairs => AbDecision::Complete,
            _ => AbDecision::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::SmallRng;
    use rand::{Rng, SeedableRng};

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
        let s = Sprt::new(0.05, 0.05, 0.1);
        assert_eq!(s.decision(0, 0.4, 0.02), SprtDecision::Continue);
        assert_eq!(s.decision(2, 0.39, 0.02), SprtDecision::Better);
        assert_eq!(s.decision(2, -0.31, 0.02), SprtDecision::Worse);
        // Noise around zero: no early verdict either way.
        assert_eq!(s.decision(2, 0.005, 0.08), SprtDecision::Continue);
        assert_eq!(
            Sprt::new(0.1, 0.05, 0.1).decision(4, 0.0, 0.01),
            SprtDecision::Same
        );
        // Two-sided: each side takes half of alpha, so "better" needs more than a one-sided
        // test at the same alpha would (9.1 gave each side the whole 5 %).
        let (n, mean, sd) = (3, 0.045, 0.03);
        assert_eq!(
            Sprt::new(0.06, 0.10, 0.1).decision(n, mean, sd),
            SprtDecision::Better
        );
        assert_eq!(
            Sprt::new(0.06, 0.05, 0.1).decision(n, mean, sd),
            SprtDecision::Continue
        );
    }

    fn pair(baseline: f64, candidate: f64, candidate_first: bool) -> AbPair {
        AbPair {
            baseline,
            candidate,
            ops_count: 10_000,
            candidate_first,
        }
    }

    /// The A/A floor of baseline values alone (no candidate yet).
    fn noise_floor(a: &[f64], resolution: f64) -> f64 {
        noise_floor_ab(a, &[], resolution)
    }

    #[test]
    fn the_noise_floor_has_a_minimum() {
        // No A/A difference yet: the minimum, widened.
        assert_eq!(noise_floor(&[100.0], 0.0), 2.0 * MIN_NOISE_SD);
        let f = noise_floor(&[100.0, 120.0, 100.0], 0.0);
        assert!(f > 0.15 && f < 0.2, "{f}");
    }

    /// W8: a steady candidate against a noisy (throttled) baseline: the pair's spread is A's
    /// alone, not the A/A spread of two noisy trials (√2 more).
    #[test]
    fn a_steady_candidate_does_not_double_the_baselines_noise() {
        let a = [100.0, 150.0, 100.0, 150.0];
        let steady = [60.0, 60.0, 60.0, 60.0];
        let aa = noise_floor(&a, 0.0);
        let ab = noise_floor_ab(&a, &steady, 0.0);
        assert!((ab - aa / 2f64.sqrt()).abs() < 1e-12, "{ab} vs {aa}");
        // As noisy as A: the A/A floor.
        assert!((noise_floor_ab(&a, &a, 0.0) - aa).abs() < 1e-12);
        // One B trial says nothing about B: A/A.
        assert_eq!(noise_floor_ab(&a, &[60.0], 0.0), aa);
        // Minimums hold.
        assert_eq!(noise_floor_ab(&[100.0; 4], &[100.0; 4], 0.0), MIN_NOISE_SD);
    }

    #[test]
    fn one_aa_difference_is_not_a_precise_floor() {
        let one = noise_floor(&[100.0, 110.0], 0.0);
        let d = (110.0f64 / 100.0).ln();
        assert!((one - 2.0 * d).abs() < 1e-12, "{one}");
    }

    #[test]
    fn a_one_operation_difference_is_not_a_change() {
        // The macOS CI trials: ~37 operations each, the A/A trials agreeing exactly and the
        // candidates one or two operations short: "worse" from rounding unless the floor is
        // at least what one operation resolves.
        let a = [37.0, 37.0, 37.0, 38.0, 37.0];
        let b = [36.0, 35.0, 37.0, 37.0, 35.0];
        let floor = noise_floor(&a, count_resolution(35));
        assert_eq!(floor, count_resolution(35));
        assert!(floor > MIN_NOISE_SD && floor > noise_floor(&a, 0.0));
        let mut t = AbTest::new(true, 3, 5);
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            t.push(AbPair {
                ops_count: 35,
                ..pair(*x, *y, i % 2 == 1)
            });
        }
        assert!(t.noise_floor() >= floor);
        assert_eq!(t.verdict(), AbVerdict::Inconclusive);
        let i = t.interval();
        assert!(i.low < 0.0 && i.high > 0.0, "{i:?}");
        // Thousands of operations resolve far below the minimum floor: nothing changes.
        assert_eq!(
            noise_floor(&a, count_resolution(10_000)),
            noise_floor(&a, 0.0)
        );
    }

    /// 9.1.0's order: one independent coin per pair.
    fn coin_9_1(seed: u64, pair: u64) -> bool {
        let mut z = seed.wrapping_add((pair + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) >> 63 == 1
    }

    #[test]
    fn the_ab_order_is_balanced_in_blocks_drawn_from_the_seed() {
        let orders = |seed: u64| {
            (0..12)
                .map(|p| candidate_first(seed, p))
                .collect::<Vec<_>>()
        };
        let mut starts_with_candidate = 0;
        for seed in (0..1000u64).chain([u64::MAX, u64::MAX - 1]) {
            let o = orders(seed);
            assert_eq!(o, orders(seed), "the same seed gives the same orders");
            // Every block of two holds one AB and one BA …
            for block in o.chunks(2) {
                assert_ne!(block[0], block[1], "{seed}: {o:?}");
            }
            // … so after any number of pairs the orders differ by at most one.
            for k in 1..=12 {
                let first = o[..k].iter().filter(|b| **b).count() as i64;
                assert!((2 * first - k as i64).abs() <= 1, "{seed} after {k}: {o:?}");
            }
            starts_with_candidate += usize::from(o[0]);
        }
        // Which order leads is a fair coin across seeds, and varies from block to block.
        assert!(
            (400..=600).contains(&starts_with_candidate),
            "{starts_with_candidate}"
        );
        assert!((0..100u64).any(|s| candidate_first(s, 0) != candidate_first(s, 2)));
        assert_ne!(orders(1), orders(2));
        // 9.1's coin ran the first three pairs in one order for about a seed in four.
        let same = (0..1000u64)
            .filter(|s| coin_9_1(*s, 0) == coin_9_1(*s, 1) && coin_9_1(*s, 1) == coin_9_1(*s, 2))
            .count();
        assert!((200..=300).contains(&same), "{same}");
    }

    #[test]
    fn a_constant_order_effect_cancels_in_the_estimate() {
        // The candidate is +10 %; whichever trial runs second in a pair runs 30 % faster.
        let (effect, carry) = (1.10f64, 1.30f64);
        for higher_is_better in [true, false] {
            for lead in [true, false] {
                let mut t = AbTest::new(higher_is_better, 3, 6);
                // Three pairs: the orders differ by one.
                for (i, first) in [lead, !lead, lead].into_iter().enumerate() {
                    let base = 100.0 + i as f64;
                    let (a, b) = if first {
                        (base * carry, base * effect)
                    } else {
                        (base, base * effect * carry)
                    };
                    t.push(pair(a, b, first));
                }
                let i = t.interval();
                assert!(
                    (i.estimate - (effect - 1.0)).abs() < 1e-9,
                    "{higher_is_better} {lead}: {i:?}"
                );
                assert!(i.low < i.estimate && i.estimate < i.high);
            }
        }
        // The plain mean of the same pairs (9.1's estimate) is off by a third of the effect.
        let d = [
            effect.ln() - carry.ln(),
            effect.ln() + carry.ln(),
            effect.ln() - carry.ln(),
        ];
        assert!((mean_var(&d).0 - effect.ln()).abs() > 0.08);
    }

    #[test]
    fn the_boundary_tightens_with_looks_and_widens_with_the_plan() {
        for planned in MIN_DECISIVE_PAIRS..=MAX_PAIRS {
            for k in MIN_DECISIVE_PAIRS..planned {
                assert!(critical(planned, k) > critical(planned, k + 1));
            }
            if planned < MAX_PAIRS {
                // More looks ahead, a stricter look now.
                assert!(critical(planned + 1, planned) > critical(planned, planned));
            }
            // Every look is stricter than the fixed-sample Student interval 9.1 used.
            for k in MIN_DECISIVE_PAIRS..=planned {
                assert!(critical(planned, k) > t975((k - 1) as f64), "{planned} {k}");
            }
        }
        // Before the first look the interval is wider than any look's.
        assert_eq!(critical(6, 1), 12.706);
        assert!(critical(12, 2) > critical(12, 3));
        // Out of range: clamped, never a panic.
        assert_eq!(critical(2, 3), critical(3, 3));
        assert_eq!(critical(6, 9), critical(9, 9));
        assert_eq!(critical(40, 13), critical(12, 12));
    }

    #[test]
    fn the_test_stops_on_clear_evidence_and_on_none() {
        // +20 % every pair, hardly any noise: decided at the first look.
        let mut t = AbTest::new(true, 3, 6);
        assert_eq!(t.push(pair(100.0, 120.0, false)), AbDecision::Continue);
        assert_eq!(t.verdict(), AbVerdict::Inconclusive);
        assert_eq!(t.push(pair(101.0, 121.0, true)), AbDecision::Continue);
        assert_eq!(
            t.verdict(),
            AbVerdict::Inconclusive,
            "two pairs decide nothing"
        );
        assert_eq!(t.push(pair(99.0, 119.0, true)), AbDecision::Decided);
        assert_eq!(t.verdict(), AbVerdict::Improved);
        assert!(t.interval().low > 0.0);
        // p99: lower is better, and the interval keeps the metric's own sign.
        let mut t = AbTest::new(false, 3, 6);
        for (i, (a, b)) in [(50.0, 40.0), (51.0, 40.5), (49.0, 39.5)]
            .into_iter()
            .enumerate()
        {
            t.push(pair(a, b, i % 2 == 0));
        }
        assert_eq!(t.verdict(), AbVerdict::Improved);
        assert!(t.interval().high < 0.0, "{:?}", t.interval());
        let mut t = AbTest::new(true, 3, 6);
        for (i, (a, b)) in [(100.0, 70.0), (101.0, 71.0), (99.0, 70.0)]
            .into_iter()
            .enumerate()
        {
            t.push(pair(a, b, i % 2 == 0));
        }
        assert_eq!(
            (t.decision(), t.verdict()),
            (AbDecision::Decided, AbVerdict::Worse)
        );
        // No change at all: futile at the first look.
        let mut t = AbTest::new(true, 3, 6);
        for i in 0..3 {
            t.push(pair(100.0, 100.0, i % 2 == 0));
        }
        assert_eq!(
            (t.decision(), t.verdict()),
            (AbDecision::Futile, AbVerdict::Inconclusive)
        );
        // An effect near the floor is neither: every planned pair runs.
        let mut t = AbTest::new(true, 3, 4);
        let mut last = AbDecision::Continue;
        for (i, b) in [103.0, 101.0, 104.0, 102.0].into_iter().enumerate() {
            assert_eq!(last, AbDecision::Continue);
            last = t.push(pair(100.0, b, i % 2 == 0));
        }
        assert_eq!(last, AbDecision::Complete);
        // An empty test has nothing to say.
        let t = AbTest::new(true, 3, 6);
        assert_eq!(t.interval(), Interval::default());
        assert_eq!(
            (t.decision(), t.verdict()),
            (AbDecision::Continue, AbVerdict::Inconclusive)
        );
    }

    /// A standard normal draw (Box–Muller).
    fn normal(rng: &mut SmallRng) -> f64 {
        let u: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * rng.r#gen::<f64>()).cos()
    }

    /// What a simulated service does to a pair of trials: each trial's objective is
    /// log-normal with spread `sigma`, the candidate's is `effect` (log) higher, and the
    /// trial that runs second in its pair is `carry` (log) higher.
    #[derive(Clone, Copy)]
    struct World {
        sigma: f64,
        effect: f64,
        carry: f64,
    }

    impl World {
        fn pair(&self, rng: &mut SmallRng, candidate_first: bool) -> AbPair {
            let mut a = self.sigma * normal(rng);
            let mut b = self.effect + self.sigma * normal(rng);
            if candidate_first {
                a += self.carry;
            } else {
                b += self.carry;
            }
            pair(100.0 * a.exp(), 100.0 * b.exp(), candidate_first)
        }

        /// One proof as `prove` runs it: the real schedule, the real test, to its stop.
        fn prove(&self, rng: &mut SmallRng, max_pairs: usize) -> AbTest {
            let seed = rng.r#gen::<u64>();
            let mut t = AbTest::new(true, MIN_DECISIVE_PAIRS, max_pairs);
            loop {
                let first = candidate_first(seed, t.pairs() as u64);
                if t.push(self.pair(rng, first)) != AbDecision::Continue {
                    return t;
                }
            }
        }

        /// The share of `proofs` proofs that end with a verdict other than `inconclusive`.
        fn verdict_rate(&self, seed: u64, proofs: usize, max_pairs: usize) -> f64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            let hits = (0..proofs)
                .filter(|_| self.prove(&mut rng, max_pairs).verdict() != AbVerdict::Inconclusive)
                .count();
            hits as f64 / proofs as f64
        }
    }

    #[test]
    fn a_change_that_is_none_is_called_in_under_five_percent_of_proofs() {
        // A and B identically distributed, at a noise below, near and far above the minimum
        // floor, for every plan.
        for max_pairs in MIN_DECISIVE_PAIRS..=MAX_PAIRS {
            for (i, sigma) in [0.004, 0.03, 0.1, 0.5].into_iter().enumerate() {
                let w = World {
                    sigma,
                    effect: 0.0,
                    carry: 0.0,
                };
                let rate = w.verdict_rate(1000 * max_pairs as u64 + i as u64, 3000, max_pairs);
                assert!(rate <= 0.05, "{max_pairs} pairs, σ {sigma}: {rate}");
            }
            // One arm perfectly steady (a cap that ends all variation): the floor then learns
            // from one arm only, and the rate still holds.
            let mut rng = SmallRng::seed_from_u64(555 + max_pairs as u64);
            let proofs = 3000;
            let called = (0..proofs)
                .filter(|_| {
                    let seed = rng.r#gen::<u64>();
                    let mut t = AbTest::new(true, MIN_DECISIVE_PAIRS, max_pairs);
                    loop {
                        let first = candidate_first(seed, t.pairs() as u64);
                        let a = 100.0 * (0.1 * normal(&mut rng)).exp();
                        if t.push(pair(a, 100.0, first)) != AbDecision::Continue {
                            break t.verdict() != AbVerdict::Inconclusive;
                        }
                    }
                })
                .count();
            let rate = called as f64 / proofs as f64;
            assert!(
                rate <= 0.05,
                "{max_pairs} pairs, a steady candidate: {rate}"
            );
        }
    }

    #[test]
    fn an_order_effect_does_not_raise_the_false_positives() {
        // The second trial of every pair is better or worse by 0.5 to 8 σ.
        let sigma = 0.05;
        for max_pairs in MIN_DECISIVE_PAIRS..=MAX_PAIRS {
            for (i, k) in [0.5, -0.5, 2.0, -2.0, 8.0, -8.0].into_iter().enumerate() {
                let w = World {
                    sigma,
                    effect: 0.0,
                    carry: k * sigma,
                };
                let rate =
                    w.verdict_rate(77_000 + 10 * max_pairs as u64 + i as u64, 2000, max_pairs);
                assert!(rate <= 0.05, "{max_pairs} pairs, carry {k} σ: {rate}");
            }
        }
    }

    /// 9.1.0's rule on pairs drawn by `next(pair index)` (signed log ratios aside, the same
    /// floor): an SPRT with the whole alpha on each side, its effect and spread plugged in
    /// anew at each look; a stop as soon as it agrees with a fixed-sample Student interval; and
    /// that interval deciding alone when the SPRT did not. Returns whether a change was called.
    fn rule_9_1(max_pairs: usize, mut next: impl FnMut(u64) -> AbPair) -> bool {
        let (mut a, mut b, mut d) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            let p = next(a.len() as u64);
            a.push(p.baseline);
            b.push(p.candidate);
            d.push((p.candidate / p.baseline).ln());
            let n = d.len();
            let floor = noise_floor_ab(&a, &b, 0.0);
            let (mean, var) = mean_var(&d);
            let sd = var.sqrt().max(floor);
            let sprt = if n >= MIN_DECISIVE_PAIRS {
                Sprt::new((2.0 * floor).max(MIN_EFFECT), 0.10, 0.10).decision(n, mean, sd)
            } else {
                SprtDecision::Continue
            };
            let half = t975(n as f64 - 1.0) * sd / (n as f64).sqrt();
            let excludes_zero = n >= MIN_DECISIVE_PAIRS && mean.abs() > half;
            let agrees = match sprt {
                SprtDecision::Better | SprtDecision::Worse => excludes_zero,
                SprtDecision::Same => true,
                SprtDecision::Continue => false,
            };
            if agrees || n >= max_pairs {
                let decided = matches!(sprt, SprtDecision::Better | SprtDecision::Worse);
                return excludes_zero && (decided || mean.abs() > floor);
            }
        }
    }

    #[test]
    fn the_9_1_rule_called_an_order_effect_a_change() {
        // No change; the second trial of a pair runs 4 σ better (a warm cache, a refilled
        // bucket). 9.1's independent coins put most pairs of a short proof in one order often
        // enough, and its unadjusted interval then calls the order effect a change, in 15 to
        // 25 % of proofs; the balanced schedule and the crossover estimate do not.
        let w = World {
            sigma: 0.05,
            effect: 0.0,
            carry: 0.20,
        };
        let proofs = 4000;
        for max_pairs in [3, 6, 12] {
            let mut rng = SmallRng::seed_from_u64(9100 + max_pairs as u64);
            let old = (0..proofs)
                .filter(|_| {
                    let seed = rng.r#gen::<u64>();
                    rule_9_1(max_pairs, |i| w.pair(&mut rng, coin_9_1(seed, i)))
                })
                .count() as f64
                / proofs as f64;
            assert!(old > 0.10, "{max_pairs} pairs: 9.1 called {old}");
            let new = w.verdict_rate(9100 + max_pairs as u64, proofs, max_pairs);
            assert!(new <= 0.05, "{max_pairs} pairs: {new}");
        }
    }

    #[test]
    fn a_real_change_is_still_found() {
        // A change of 3 σ of a pair's log ratio (σ_pair = √2 σ_trial): found within the default
        // six pairs nearly always, and before the last of them on average.
        let sigma = 0.05;
        let sigma_pair = 2f64.sqrt() * sigma;
        let w = World {
            sigma,
            effect: 3.0 * sigma_pair,
            carry: 0.0,
        };
        let mut rng = SmallRng::seed_from_u64(4242);
        let (mut found, mut pairs) = (0, 0);
        let proofs = 2000;
        for _ in 0..proofs {
            let t = w.prove(&mut rng, 6);
            found += usize::from(t.verdict() == AbVerdict::Improved);
            pairs += t.pairs();
        }
        let (power, mean_pairs) = (found as f64 / proofs as f64, pairs as f64 / proofs as f64);
        assert!(power > 0.95, "{power}");
        assert!(mean_pairs < 5.0, "{mean_pairs}");
        // A smaller one (2 σ of a pair) needs the pairs: twelve find it, also under an order
        // effect of its own size. (A much larger order effect widens the interval: it costs
        // pairs, never a wrong verdict.)
        for carry in [0.0, sigma, -sigma] {
            let w = World {
                sigma,
                effect: 2.0 * sigma_pair,
                carry,
            };
            let power = w.verdict_rate(4243, 2000, 12);
            assert!(power > 0.9, "carry {carry}: {power}");
        }
        // And a worse candidate is called worse, not improved.
        let w = World {
            sigma,
            effect: -0.3,
            carry: 0.0,
        };
        let t = w.prove(&mut SmallRng::seed_from_u64(1), 6);
        assert_eq!(t.verdict(), AbVerdict::Worse);
    }

    #[test]
    fn the_verdict_is_the_interval_at_every_look() {
        // Whatever the pairs, a verdict is exactly an interval that excludes no change, and a
        // "decided" stop always carries one.
        let mut rng = SmallRng::seed_from_u64(31);
        for i in 0..400 {
            let w = World {
                sigma: 0.05,
                effect: [0.0, 0.05, 0.2, -0.2][i % 4],
                carry: [0.0, 0.1][i % 2],
            };
            let max_pairs = 3 + i % 10;
            let seed = rng.r#gen::<u64>();
            let mut t = AbTest::new(i % 3 != 0, 3, max_pairs);
            for k in 0..max_pairs {
                let d = t.push(w.pair(&mut rng, candidate_first(seed, k as u64)));
                let iv = t.interval();
                let excludes = iv.low > 0.0 || iv.high < 0.0;
                let called = t.verdict() != AbVerdict::Inconclusive;
                assert_eq!(called, excludes && t.pairs() >= 3, "{iv:?}");
                assert!(d != AbDecision::Decided || called);
                assert!(d != AbDecision::Continue || k + 1 < max_pairs);
            }
        }
    }
}
