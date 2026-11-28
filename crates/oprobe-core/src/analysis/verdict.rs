//! The statistics `oprobe prove` decides with (design v6 §8 "Prove", T9):
//! - **the A/B decision** ([`AbTest`]): interleaved pairs in a balanced, seeded order
//!   ([`candidate_first`]), an estimate that a constant order effect cannot bias, and one
//!   two-sided sequential boundary ([`critical`]) that the verdict, the interval and every
//!   look share, so the chance of ever calling a change that is none stays under 5 %
//!   whenever prove stops;
//! - **the noise floor** ([`noise_floor_paired`]): the per-pair spread learned from the paired
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
///
/// Pairs are formed before anything is left out: a pair with a value that is not positive
/// goes whole. (9.3.4 filtered each side first, so one zero on each side, in different pairs,
/// paired every later trial with another pair's.)
pub fn log_ratio(a: &[f64], b: &[f64], paired: bool, floor_sd: f64) -> Interval {
    if paired && a.len() == b.len() {
        let diffs: Vec<f64> = a
            .iter()
            .zip(b)
            .filter(|(x, y)| **x > 0.0 && **y > 0.0)
            .map(|(x, y)| y.ln() - x.ln())
            .collect();
        if diffs.is_empty() {
            return Interval::default();
        }
        let (m, v) = mean_var(&diffs);
        let n = diffs.len() as f64;
        let v = v.max(floor_sd * floor_sd);
        return interval(m, t975((n - 1.0).max(1.0)) * (v / n).sqrt());
    }
    let la: Vec<f64> = a.iter().filter(|x| **x > 0.0).map(|x| x.ln()).collect();
    let lb: Vec<f64> = b.iter().filter(|x| **x > 0.0).map(|x| x.ln()).collect();
    if la.is_empty() || lb.is_empty() {
        return Interval::default();
    }
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
    interval(mb - ma, t975(df) * se2.sqrt())
}

/// The relative change of a log ratio `d` with half-width `h`.
fn interval(d: f64, h: f64) -> Interval {
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

    /// The decision on an estimate `mean` of the signed log ratio whose variance is `weight`
    /// times a pair's, the pairs spreading by `sd`: `weight` is `1/n` for the plain mean of `n`
    /// pairs, and the crossover estimate's `(1/n_AB + 1/n_BA) / 4` in [`AbTest`] (9.2 passed
    /// the pair count here, so with an odd number of pairs the stop assumed a tighter estimate
    /// than the interval beside it).
    pub fn decision(&self, weight: f64, mean: f64, sd: f64) -> SprtDecision {
        if !(weight.is_finite() && weight > 0.0 && mean.is_finite()) {
            return SprtDecision::Continue;
        }
        let var = sd.max(1e-3).powi(2) * weight;
        // log LR of N(μ, σ²·w) against N(0, σ²·w).
        let llr = |mu: f64| (mu * mean - mu * mu / 2.0) / var;
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

/// The latencies around a trial's p99 differ by more than this: the p99 sits on a cliff
/// ([`p99_spread`]), and which side of it a trial lands on is chance.
pub const P99_CLIFF_RATIO: f64 = 2.0;

/// The ranks (1-based, within `1..=n`) that bracket the p99 of `n` sorted latencies:
/// `0.99·n ± 2·√(0.0099·n)`, two standard deviations of how many of `n` samples fall below
/// the true p99 (a binomial count). The latencies at these ranks are the p99's order-statistic
/// interval. `None` when nothing was measured.
pub fn p99_rank_band(n: u64) -> Option<(u64, u64)> {
    if n == 0 {
        return None;
    }
    let centre = 0.99 * n as f64;
    let half = 2.0 * (0.0099 * n as f64).sqrt();
    let lo = ((centre - half).floor().max(1.0) as u64).min(n);
    let hi = ((centre + half).ceil().max(1.0) as u64).min(n);
    Some((lo, hi))
}

/// The spread of one trial's p99 as a log ratio, from the latencies `lo_ms` and `hi_ms` at the
/// ranks of [`p99_rank_band`]: the band is two standard deviations either way, so a quarter of
/// its log width. It is to a p99 what [`count_resolution`] is to a count: what one trial cannot
/// resolve, however well two trials happen to agree. 0 when the band has no width.
pub fn p99_spread(lo_ms: f64, hi_ms: f64) -> f64 {
    if lo_ms > 0.0 && hi_ms > lo_ms && hi_ms.is_finite() {
        (hi_ms / lo_ms).ln() / 4.0
    } else {
        0.0
    }
}

/// A trial whose p99 spreads by `spread` ([`p99_spread`]) has latencies around its p99 that
/// differ by more than [`P99_CLIFF_RATIO`].
pub fn p99_on_a_cliff(spread: f64) -> bool {
    spread > P99_CLIFF_RATIO.ln() / 4.0
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
/// crossover estimate, `w_k = (1/n_AB + 1/n_BA) / 4`, `s_k` the spread of [`crossover`]: from
/// the fourth pair the pooled within-order SD on `k − 2` degrees of freedom, at the third the
/// SD of all three log ratios), over orders drawn as [`candidate_first`] draws them. Each row
/// holds one nominal two-sided level at every look (Pocock): the null quantile of `|T_k|` at
/// that level (Student's t on `k − 2` degrees of freedom from the fourth look; simulated at
/// the third), chosen so that under the null (i.i.d. normal log ratios; the statistic is
/// scale-free) `P(|T_k| > CRITICAL[K − 3][k − 3] for any k in 3..=K)` is 4.0 % — under the
/// 5 % the verdict promises, with margin for the simulation error and for noise that is not
/// quite normal. The per-look levels are 4.00, 2.24, 1.65, 1.33, 1.14, 1.01, 0.93, 0.85,
/// 0.80 and 0.75 % for K = 3..=12. The fourth look's value can exceed the third's: its spread
/// has two degrees of freedom, the third's has two as well but is a different statistic.
///
/// 9.3.2 divided by the SD of all `k` log ratios at every look: an order effect (the second
/// trial of every pair better or worse by γ) put γ² into it, and a +15 % change under such an
/// effect was found in under 5 % of proofs. The pooled within-order SD leaves the order effect
/// out. At the third look it would have one degree of freedom, and the look could decide
/// nothing; the SD of all three is kept there, which an order effect can only inflate (that
/// look then waits for the next).
///
/// Calibrated offline by simulation: 4 000 000 proofs of 12 pairs (numpy, seed 20261127), the
/// level of each row bisected until the crossing rate was 0.0400, each value rounded up to
/// 0.001. A second run on 4 000 000 fresh proofs (seed 99) gave every row a crossing rate of
/// 0.0398 to 0.0400. The spread [`AbTest`] divides by is never below `s_k` (it is the larger
/// of `s_k` and the noise floor), so its error rate is at most the calibrated one.
const CRITICAL: [&[f64]; MAX_PAIRS - MIN_DECISIVE_PAIRS + 1] = [
    &[4.612],
    &[6.230, 6.571],
    &[7.290, 7.702, 4.881],
    &[8.138, 8.597, 5.278, 4.241],
    &[8.782, 9.268, 5.567, 4.428, 3.898],
    &[9.333, 9.856, 5.813, 4.586, 4.019, 3.696],
    &[9.791, 10.327, 6.006, 4.709, 4.112, 3.774, 3.558],
    &[10.224, 10.775, 6.187, 4.823, 4.199, 3.846, 3.621, 3.466],
    &[
        10.558, 11.135, 6.331, 4.913, 4.266, 3.902, 3.670, 3.510, 3.393,
    ],
    &[
        10.889, 11.485, 6.469, 4.999, 4.331, 3.955, 3.716, 3.552, 3.431, 3.340,
    ],
];

/// The critical value of the interval after `pairs` pairs of a proof planned for at most
/// `max_pairs` ([`CRITICAL`]). Before the first look (fewer than [`MIN_DECISIVE_PAIRS`]) no
/// verdict is read, and the interval shown is the wider of Student's plain one on one degree
/// of freedom (12.706) and the plan's widest look. Both counts are clamped to the calibrated
/// range.
pub fn critical(max_pairs: usize, pairs: usize) -> f64 {
    let planned = max_pairs.max(pairs).clamp(MIN_DECISIVE_PAIRS, MAX_PAIRS);
    let row = CRITICAL[planned - MIN_DECISIVE_PAIRS];
    if pairs < MIN_DECISIVE_PAIRS {
        return t975(1.0).max(row.iter().copied().fold(0.0, f64::max));
    }
    row[pairs.min(planned) - MIN_DECISIVE_PAIRS]
}

/// One interleaved A/B pair: the objective of its baseline and candidate trials.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AbPair {
    pub baseline: f64,
    pub candidate: f64,
    /// The fewest operations either trial counted (the resolution of a count).
    pub ops_count: u64,
    /// A spread of this pair's log ratio that its two values cannot show, 0 when none is
    /// known: for a p99, both trials' order-statistic spreads ([`p99_spread`]) combined.
    pub resolution: f64,
    /// The candidate ran first ([`candidate_first`]).
    pub candidate_first: bool,
}

impl AbPair {
    /// Both values can be compared as a ratio: finite and above zero. A trial with no
    /// successful operation has no goodput, and one that measured nothing has no p99; 9.2
    /// clamped such a value to 1e-9, so a candidate with no latency at all "improved" the p99
    /// by 100 %.
    pub fn comparable(&self) -> bool {
        [self.baseline, self.candidate]
            .iter()
            .all(|v| v.is_finite() && *v > 0.0)
    }
}

/// The smallest change a proof planned for `max_pairs` pairs can show when a pair's log ratio
/// spreads by `sd`, as a log ratio: the half-width of its interval at the last look, on the
/// balanced schedule ([`candidate_first`]).
pub fn resolvable(max_pairs: usize, sd: f64) -> f64 {
    let k = max_pairs.clamp(MIN_DECISIVE_PAIRS, MAX_PAIRS);
    let (n_first, n_second) = (k.div_ceil(2) as f64, (k / 2) as f64);
    let w = (1.0 / n_first + 1.0 / n_second) / 4.0;
    critical(k, k) * sd * w.sqrt()
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

/// The 2×2 crossover statistics of the signed log ratios `d` of pairs whose candidate ran
/// first where `candidate_first` says so: `(estimate, w, s)`, the estimate's variance being
/// `w` times a pair's. With both orders present the estimate is the mean of the two orders'
/// means, `w = (1/n_AB + 1/n_BA) / 4`, and, from four pairs, `s` the pooled within-order SD
/// (each order's deviations from its own mean, on `n − 2` degrees of freedom): an order effect
/// moves the two orders' means apart and is left out of `s`, as it is out of the estimate.
/// With fewer pairs (the pooled SD would have one degree of freedom or none) and with one order
/// only, `s` is the sample SD of all of `d`. [`CRITICAL`] is calibrated on
/// `estimate / (s · √w)`.
fn crossover(d: &[f64], candidate_first: &[bool]) -> (f64, f64, f64) {
    /// From how many pairs the spread is the pooled within-order SD (two degrees of freedom).
    const POOLED_FROM_PAIRS: usize = 4;
    let of_order = |first: bool| -> Vec<f64> {
        d.iter()
            .zip(candidate_first)
            .filter(|(_, f)| **f == first)
            .map(|(x, _)| *x)
            .collect()
    };
    let (ba, ab) = (of_order(true), of_order(false));
    let (n_ba, n_ab) = (ba.len() as f64, ab.len() as f64);
    if n_ba > 0.0 && n_ab > 0.0 && d.len() >= POOLED_FROM_PAIRS {
        let ((m_ba, v_ba), (m_ab, v_ab)) = (mean_var(&ba), mean_var(&ab));
        let pooled = ((n_ba - 1.0) * v_ba + (n_ab - 1.0) * v_ab) / (n_ba + n_ab - 2.0);
        (
            (m_ab + m_ba) / 2.0,
            (1.0 / n_ab + 1.0 / n_ba) / 4.0,
            pooled.sqrt(),
        )
    } else if n_ba > 0.0 && n_ab > 0.0 {
        let ((m_ba, _), (m_ab, _)) = (mean_var(&ba), mean_var(&ab));
        (
            (m_ab + m_ba) / 2.0,
            (1.0 / n_ab + 1.0 / n_ba) / 4.0,
            mean_var(d).1.sqrt(),
        )
    } else {
        let (mean, var) = mean_var(d);
        (mean, 1.0 / d.len().max(1) as f64, var.sqrt())
    }
}

/// The noise floor of a pair's log ratio from the pairs themselves: the paired differences
/// `d` (in pair order) of each order (`candidate_first`), and the mean squared difference of
/// consecutive ones of the same order (von Neumann: `σ² ≈ MSSD / 2`).
///
/// Paired, as the design is: what moves both trials of a pair alike (the service slowing
/// through the proof, a neighbour's load, a cache warming) is not in `d`, and within one order
/// an order effect is not in a difference of consecutive `d` either; a slow drift of the
/// change itself barely is. 9.3.2 learned the floor from each arm's own consecutive trials:
/// a drift common to both arms raised it although the pairs cancel it, and with it the
/// interval. At least [`MIN_NOISE_SD`] and the count `resolution` ([`count_resolution`]), and
/// doubled while there is no difference yet (fewer than three pairs: nothing says how precise
/// it is).
pub fn noise_floor_paired(d: &[f64], candidate_first: &[bool], resolution: f64) -> f64 {
    let mut squares = Vec::new();
    for order in [true, false] {
        let mine: Vec<f64> = d
            .iter()
            .zip(candidate_first)
            .filter(|(x, f)| **f == order && x.is_finite())
            .map(|(x, _)| *x)
            .collect();
        squares.extend(mine.windows(2).map(|w| (w[1] - w[0]).powi(2)));
    }
    let var = if squares.is_empty() {
        0.0
    } else {
        squares.iter().sum::<f64>() / squares.len() as f64 / 2.0
    };
    let floor = var.sqrt().max(MIN_NOISE_SD).max(resolution);
    if squares.is_empty() {
        2.0 * floor
    } else {
        floor
    }
}

/// [`noise_floor_paired`] of the pairs `a[i]`, `b[i]` (positive values; the log ratios
/// `ln(b/a)`) in the orders they ran (`candidate_first[i]`): a metric shown beside the
/// objective. 9.3.4 took every pair as run in one order, so an order effect (the second trial
/// of a pair running warmer) read as noise and widened the metric's interval.
pub fn noise_floor_of_pairs(
    a: &[f64],
    b: &[f64],
    candidate_first: &[bool],
    resolution: f64,
) -> f64 {
    let (d, first): (Vec<f64>, Vec<bool>) = a
        .iter()
        .zip(b)
        .zip(candidate_first)
        .filter(|((x, y), _)| **x > 0.0 && **y > 0.0)
        .map(|((x, y), f)| ((y / x).ln(), *f))
        .unzip();
    noise_floor_paired(&d, &first, resolution)
}

/// The A/B decision of `oprobe prove`, one pair at a time: pure and synchronous, so the whole
/// rule can be simulated.
///
/// - **Estimate.** The signed log ratio of each pair (positive: the candidate is better). With
///   both orders present it is the mean of the AB-order mean and the BA-order mean (the 2×2
///   crossover estimate): a constant order effect — the second trial of a pair running better
///   or worse — cancels exactly, also when the orders differ by one.
/// - **Spread.** The larger of the pairs' own sample SD (which an order effect only inflates)
///   and the noise floor ([`noise_floor_paired`]); the standard error is that spread times
///   `√((1/n_AB + 1/n_BA) / 4)`.
/// - **Boundary.** One two-sided sequential boundary ([`critical`]): the interval after each
///   pair is a repeated confidence interval, and the verdict is whether it excludes no
///   change. Whatever stops the sampling — the early stop below, the pair limit, a budget —
///   the chance of a verdict on a change that is none stays under 5 %.
/// - **Early stop.** From `min_pairs` on, sampling stops once the interval excludes no change
///   and an [`Sprt`] agrees the change is at least the minimum effect, or once the SPRT says
///   no change of that effect is there (futility). The minimum effect is the smallest change
///   the planned pairs can show at the spread seen so far ([`resolvable`], at least
///   [`MIN_EFFECT`]): a proof is abandoned only for a change it could not have shown anyway.
///   (9.2 took twice the noise floor: a real 15 % gain at a 15 % floor was given up at the
///   third pair.) The effect and the spread are plug-ins; they only choose when to stop, so
///   the boundary's error rate holds. An estimate read after an early stop is biased away
///   from zero (the proof stopped because it was large); the interval's coverage is not.
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

    /// Add a completed pair; whether to run another. A pair that cannot be compared
    /// ([`AbPair::comparable`]) is not added: it is never judged.
    pub fn push(&mut self, pair: AbPair) -> AbDecision {
        if pair.comparable() {
            self.pairs.push(pair);
        }
        self.decision()
    }

    /// Pairs so far.
    pub fn pairs(&self) -> usize {
        self.pairs.len()
    }

    /// The noise floor after the pairs so far (a per-pair log-ratio spread), from the paired
    /// differences ([`noise_floor_paired`]).
    pub fn noise_floor(&self) -> f64 {
        let d: Vec<f64> = self.pairs.iter().map(|p| self.signed(p)).collect();
        let orders: Vec<bool> = self.pairs.iter().map(|p| p.candidate_first).collect();
        let count = self
            .pairs
            .iter()
            .map(|p| p.ops_count)
            .min()
            .map_or(0.0, count_resolution);
        // The pairs' own resolution, as the spread it adds to a pair on average.
        let own = if self.pairs.is_empty() {
            0.0
        } else {
            (self.pairs.iter().map(|p| p.resolution.powi(2)).sum::<f64>() / self.pairs.len() as f64)
                .sqrt()
        };
        noise_floor_paired(&d, &orders, count.max(own))
    }

    /// The signed log ratio of pair `p`: positive when the candidate is better.
    fn signed(&self, p: &AbPair) -> f64 {
        if self.higher_is_better {
            (p.candidate / p.baseline).ln()
        } else {
            (p.baseline / p.candidate).ln()
        }
    }

    /// The smallest change this proof can show at the spread of its pairs so far, as a
    /// relative change ([`resolvable`]): what a futility stop gave up on. `None` before the
    /// first pair.
    pub fn resolvable_change(&self) -> Option<f64> {
        let (_, _, sd, _) = self.estimate()?;
        Some(resolvable(self.max_pairs, sd).max(MIN_EFFECT).exp() - 1.0)
    }

    /// `(estimate, half-width, spread, weight)` of the mean signed log ratio, positive =
    /// better; the estimate's variance is `weight` times a pair's.
    fn estimate(&self) -> Option<(f64, f64, f64, f64)> {
        let n = self.pairs.len();
        if n == 0 {
            return None;
        }
        let d: Vec<f64> = self.pairs.iter().map(|p| self.signed(p)).collect();
        let orders: Vec<bool> = self.pairs.iter().map(|p| p.candidate_first).collect();
        let (est, w, s) = crossover(&d, &orders);
        let sd = s.max(self.noise_floor());
        Some((est, critical(self.max_pairs, n) * sd * w.sqrt(), sd, w))
    }

    /// The objective's relative change (candidate vs baseline) with its sequential,
    /// order-adjusted 95 % interval, in the metric's own direction (goodput +: more; p99 −:
    /// faster), as `exp(·) − 1` — never below −100 %.
    pub fn interval(&self) -> Interval {
        let Some((est, half, _, _)) = self.estimate() else {
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
            Some((est, half, _, _)) if self.pairs.len() >= MIN_DECISIVE_PAIRS => {
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
        let Some((est, _, sd, w)) = self.estimate().filter(|_| n >= self.min_pairs) else {
            return AbDecision::Continue;
        };
        let delta = resolvable(self.max_pairs, sd).max(MIN_EFFECT);
        let sprt = Sprt::new(delta, ALPHA, BETA).decision(w, est, sd);
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

    /// Pairs are formed before values are left out. 9.3.4 filtered each side first: with a
    /// zero baseline in pair 0 and a zero candidate in pair 3, the sides kept equal lengths and
    /// every pair in between was compared with its neighbour's.
    #[test]
    fn a_missing_value_drops_its_pair_not_the_alignment() {
        let a = [0.0, 100.0, 200.0, 400.0, 800.0];
        let b = [50.0, 110.0, 220.0, 0.0, 880.0];
        let i = log_ratio(&a, &b, true, 0.0);
        assert!((i.estimate - 0.10).abs() < 1e-9, "{i:?}");
        assert!(i.low > 0.09 && i.high < 0.11, "{i:?}");
        // Nothing comparable: no interval.
        assert_eq!(log_ratio(&[0.0], &[1.0], true, 0.0), Interval::default());
    }

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
        assert_eq!(s.decision(0.0, 0.4, 0.02), SprtDecision::Continue);
        assert_eq!(s.decision(0.5, 0.39, 0.02), SprtDecision::Better);
        assert_eq!(s.decision(0.5, -0.31, 0.02), SprtDecision::Worse);
        // Noise around zero: no early verdict either way.
        assert_eq!(s.decision(0.5, 0.005, 0.08), SprtDecision::Continue);
        assert_eq!(
            Sprt::new(0.1, 0.05, 0.1).decision(0.25, 0.0, 0.01),
            SprtDecision::Same
        );
        // Two-sided: each side takes half of alpha, so "better" needs more than a one-sided
        // test at the same alpha would (9.1 gave each side the whole 5 %).
        let (w, mean, sd) = (1.0 / 3.0, 0.045, 0.03);
        assert_eq!(
            Sprt::new(0.06, 0.10, 0.1).decision(w, mean, sd),
            SprtDecision::Better
        );
        assert_eq!(
            Sprt::new(0.06, 0.05, 0.1).decision(w, mean, sd),
            SprtDecision::Continue
        );
    }

    fn pair(baseline: f64, candidate: f64, candidate_first: bool) -> AbPair {
        AbPair {
            baseline,
            candidate,
            ops_count: 10_000,
            resolution: 0.0,
            candidate_first,
        }
    }

    /// The floor of pairs `a[i]`, `b[i]` in one order.
    fn noise_floor(a: &[f64], b: &[f64], resolution: f64) -> f64 {
        noise_floor_of_pairs(a, b, &vec![false; a.len()], resolution)
    }

    #[test]
    fn the_noise_floor_has_a_minimum() {
        // No difference of consecutive pairs yet: the minimum, widened.
        assert_eq!(noise_floor(&[100.0], &[100.0], 0.0), 2.0 * MIN_NOISE_SD);
        let f = noise_floor(&[100.0, 120.0, 100.0], &[100.0; 3], 0.0);
        assert!(f > 0.12 && f < 0.14, "{f}");
        // Identical pairs: the minimum.
        assert_eq!(noise_floor(&[100.0; 4], &[100.0; 4], 0.0), MIN_NOISE_SD);
    }

    /// What moves both trials of a pair alike is not in the pair's log ratio, and so not in
    /// the floor: a service slowing through the proof (or a neighbour's load coming and
    /// going) by tens of percent leaves the floor of steady pairs at its minimum. 9.3.2
    /// learned it from each arm's consecutive trials and took the drift for noise.
    #[test]
    fn a_drift_common_to_both_arms_is_not_noise() {
        let drift = [1.0, 1.3, 0.8, 1.5, 0.9, 1.4];
        let a: Vec<f64> = drift.iter().map(|k| 100.0 * k).collect();
        let b: Vec<f64> = drift.iter().map(|k| 120.0 * k).collect();
        assert_eq!(noise_floor(&a, &b, 0.0), MIN_NOISE_SD);
        // A steady candidate against a noisy baseline: the pair's spread is the baseline's.
        let noisy = [100.0, 150.0, 100.0, 150.0, 100.0];
        let f = noise_floor(&noisy, &[60.0; 5], 0.0);
        let step = (1.5f64).ln();
        assert!((f - step / 2f64.sqrt()).abs() < 1e-12, "{f}");
    }

    /// An order effect (the second trial of a pair better by γ) alternates the sign of the
    /// pairs' log ratios with their order: within one order it cancels, and the floor of
    /// otherwise steady pairs stays at its minimum.
    #[test]
    fn an_order_effect_is_not_noise() {
        let gamma = 0.2;
        let orders = [true, false, false, true, true, false];
        let d: Vec<f64> = orders
            .iter()
            .map(|first| 0.05 + if *first { -gamma } else { gamma })
            .collect();
        assert_eq!(noise_floor_paired(&d, &orders, 0.0), MIN_NOISE_SD);
        // Unaware of the orders it would have been the order effect.
        assert!(noise_floor_paired(&d, &[false; 6], 0.0) > gamma);
    }

    #[test]
    fn no_difference_is_not_a_precise_floor() {
        let one = noise_floor(&[100.0, 110.0], &[100.0, 100.0], 0.0);
        let d = (110.0f64 / 100.0).ln();
        assert!((one - d / 2f64.sqrt()).abs() < 1e-12, "{one}");
        // Two pairs of different orders: no difference within an order yet.
        let d = [0.1, 0.3];
        assert_eq!(
            noise_floor_paired(&d, &[true, false], 0.0),
            2.0 * MIN_NOISE_SD
        );
    }

    #[test]
    fn a_one_operation_difference_is_not_a_change() {
        // The macOS CI trials: ~37 operations each, the A/A trials agreeing exactly and the
        // candidates one or two operations short: "worse" from rounding unless the floor is
        // at least what one operation resolves.
        let a = [37.0, 37.0, 37.0, 38.0, 37.0];
        let b = [36.0, 35.0, 37.0, 37.0, 35.0];
        let floor = noise_floor(&a, &a, count_resolution(35));
        assert_eq!(floor, count_resolution(35));
        assert!(floor > MIN_NOISE_SD && floor > noise_floor(&a, &a, 0.0));
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
            noise_floor(&a, &b, count_resolution(10_000)),
            noise_floor(&a, &b, 0.0)
        );
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
            // From the fourth look, where the spread is the pooled within-order SD (the third
            // reads another statistic).
            for k in MIN_DECISIVE_PAIRS + 1..planned {
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

    /// The checked-in table against the statistic and the schedule as they are coded: over
    /// 120 000 null proofs of 12 pairs, the boundary of each plan is crossed at some look in
    /// 4 % of them (within the simulation's error), with no help from the noise floor.
    #[test]
    fn the_boundary_is_crossed_in_four_percent_of_null_proofs() {
        let mut rng = SmallRng::seed_from_u64(20_261_126);
        let proofs = 120_000;
        // Proofs that crossed the boundary of each plan, by its planned pairs.
        let mut crossed: std::collections::BTreeMap<usize, u32> = Default::default();
        for _ in 0..proofs {
            let seed = rng.r#gen::<u64>();
            let d: Vec<f64> = (0..MAX_PAIRS).map(|_| normal(&mut rng)).collect();
            let orders: Vec<bool> = (0..MAX_PAIRS as u64)
                .map(|p| candidate_first(seed, p))
                .collect();
            let t: Vec<f64> = (MIN_DECISIVE_PAIRS..=MAX_PAIRS)
                .map(|k| {
                    let (est, w, s) = crossover(&d[..k], &orders[..k]);
                    (est / (s * w.sqrt())).abs()
                })
                .collect();
            for planned in MIN_DECISIVE_PAIRS..=MAX_PAIRS {
                let ever = (MIN_DECISIVE_PAIRS..=planned)
                    .any(|k| t[k - MIN_DECISIVE_PAIRS] > critical(planned, k));
                *crossed.entry(planned).or_default() += u32::from(ever);
            }
        }
        assert_eq!(crossed.len(), MAX_PAIRS - MIN_DECISIVE_PAIRS + 1);
        for (planned, count) in crossed {
            let rate = f64::from(count) / f64::from(proofs);
            // 0.04 ± 4 standard errors of 120 000 proofs, and under the 5 % promised.
            assert!((0.0377..=0.0423).contains(&rate), "{planned} pairs: {rate}");
        }
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
        let mut t = AbTest::new(true, 3, 5);
        let mut last = AbDecision::Continue;
        for (i, b) in [103.0, 101.0, 104.0, 102.0, 103.0].into_iter().enumerate() {
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
    /// log-normal with spread `sigma`, the candidate's is `effect` (log) higher, the trial
    /// that runs second in its pair is `carry` (log) higher, and both trials of a pair move
    /// with the service's level, a random walk of `drift` (log) per pair.
    #[derive(Clone, Copy)]
    struct World {
        sigma: f64,
        effect: f64,
        carry: f64,
        drift: f64,
    }

    impl World {
        fn pair(&self, rng: &mut SmallRng, candidate_first: bool) -> AbPair {
            self.pair_at(rng, candidate_first, 0.0)
        }

        /// A pair at service level `level` (log).
        fn pair_at(&self, rng: &mut SmallRng, candidate_first: bool, level: f64) -> AbPair {
            let mut a = level + self.sigma * normal(rng);
            let mut b = level + self.effect + self.sigma * normal(rng);
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
            let mut level = 0.0;
            loop {
                let first = candidate_first(seed, t.pairs() as u64);
                level += self.drift * normal(rng);
                if t.push(self.pair_at(rng, first, level)) != AbDecision::Continue {
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
                    drift: 0.0,
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
                    drift: 0.0,
                };
                let rate =
                    w.verdict_rate(77_000 + 10 * max_pairs as u64 + i as u64, 2000, max_pairs);
                assert!(rate <= 0.05, "{max_pairs} pairs, carry {k} σ: {rate}");
            }
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
            drift: 0.0,
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
                drift: 0.0,
            };
            let power = w.verdict_rate(4243, 2000, 12);
            assert!(power > 0.9, "carry {carry}: {power}");
        }
        // And a worse candidate is called worse, not improved.
        let w = World {
            sigma,
            effect: -0.3,
            carry: 0.0,
            drift: 0.0,
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
                drift: 0.0,
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

    /// K3-1: a value that is zero, negative or not finite is no ratio. 9.2 clamped it to
    /// 1e-9: a candidate with no latency at all "improved" the p99 by 100 %.
    #[test]
    fn a_pair_without_a_value_is_never_judged() {
        for (a, b) in [
            (50.0, 0.0),
            (0.0, 50.0),
            (50.0, -1.0),
            (50.0, f64::NAN),
            (f64::INFINITY, 50.0),
        ] {
            assert!(!pair(a, b, false).comparable(), "{a} {b}");
            for higher_is_better in [true, false] {
                let mut t = AbTest::new(higher_is_better, 3, 6);
                for i in 0..6 {
                    assert_eq!(t.push(pair(a, b, i % 2 == 0)), AbDecision::Continue);
                }
                assert_eq!(t.pairs(), 0);
                assert_eq!(t.verdict(), AbVerdict::Inconclusive);
                assert_eq!(t.interval(), Interval::default());
            }
        }
        // Among good pairs it changes nothing.
        let mut t = AbTest::new(false, 3, 6);
        let mut with = t.clone();
        for (i, (a, b)) in [(50.0, 40.0), (51.0, 40.5), (49.0, 39.5)]
            .into_iter()
            .enumerate()
        {
            t.push(pair(a, b, i % 2 == 0));
            with.push(pair(a, 0.0, i % 2 == 0));
            with.push(pair(a, b, i % 2 == 0));
        }
        assert_eq!(with.interval(), t.interval());
        assert_eq!(t.verdict(), AbVerdict::Improved);
    }

    /// K3-4: the futility stop gives up only on what the plan could not have shown. 9.2's
    /// minimum effect was twice the noise floor: at a 15 % floor it looked for 30 %, and a
    /// proof whose pairs showed a steady +7 % was abandoned at the third pair.
    #[test]
    fn futility_gives_up_only_on_what_the_plan_cannot_show() {
        // What a plan can show shrinks with its pairs: 2.8 σ for three, 1.5 σ for six, under
        // one σ for twelve.
        let sd = 0.15;
        let shown: Vec<f64> = [3, 6, 12].iter().map(|k| resolvable(*k, sd) / sd).collect();
        assert!(
            (shown[0] - 4.612 * 0.375f64.sqrt()).abs() < 1e-9,
            "{shown:?}"
        );
        assert!((shown[1] - 4.241 / 6f64.sqrt()).abs() < 1e-9, "{shown:?}");
        assert!((shown[2] - 3.340 / 12f64.sqrt()).abs() < 1e-9, "{shown:?}");
        assert_eq!(resolvable(40, sd), resolvable(12, sd));
        // Three pairs spreading by 15 % (the candidate's trials swing against the baseline's),
        // the candidate +7 % on the crossover estimate.
        let pairs = |max_pairs: usize| {
            let mut t = AbTest::new(true, 3, max_pairs);
            let mut last = AbDecision::Continue;
            for (i, (a, d)) in [(100.0, 0.268), (116.0, -0.022), (100.0, 0.056)]
                .into_iter()
                .enumerate()
            {
                last = t.push(pair(a, a * f64::exp(d), i % 2 == 1));
            }
            (t, last)
        };
        let (t, last) = pairs(12);
        let (est, _, sd, _) = t.estimate().unwrap();
        assert!((est - 0.07).abs() < 1e-9, "{est}");
        assert!((sd - 0.15).abs() < 0.001, "{sd}");
        assert!(
            (t.noise_floor() - 0.15).abs() < 0.001,
            "{}",
            t.noise_floor()
        );
        // 9.2's rule on the same pairs (twice a 15 % floor): futile.
        let old = Sprt::new((2.0 * sd).max(MIN_EFFECT), ALPHA, BETA).decision(1.0 / 3.0, est, sd);
        assert_eq!(old, SprtDecision::Same);
        // Twelve pairs can show less than a floor: the proof goes on.
        assert_eq!(last, AbDecision::Continue);
        let shown = t.resolvable_change().unwrap();
        assert!(shown > 0.13 && shown < 0.16, "{shown}");
        // No change at all, both sides swinging 20 % together: the swing is in neither pair's
        // log ratio, and the proof is futile at its first look whatever its plan. (9.3.2 took
        // the common swing for noise and ran twelve-pair plans to their sixth pair.)
        for (max_pairs, at) in [(6, 3), (12, 3)] {
            let mut t = AbTest::new(true, 3, max_pairs);
            let mut stopped = None;
            for i in 0..max_pairs {
                let a = [100.0, 120.0][i % 2];
                if t.push(pair(a, a, i % 2 == 0)) != AbDecision::Continue {
                    stopped = Some((t.pairs(), t.decision()));
                    break;
                }
            }
            assert_eq!(stopped, Some((at, AbDecision::Futile)), "{max_pairs}");
        }
        assert_eq!(AbTest::new(true, 3, 6).resolvable_change(), None);
    }

    /// The early stop reads the same variance as the interval: the crossover estimate's
    /// weight, not `1/n` (they differ when the orders are unbalanced: 0.375 against 0.333 at
    /// three pairs).
    #[test]
    fn the_sprt_reads_the_estimates_own_variance() {
        let s = Sprt::new(0.1, 0.05, 0.1);
        // The same evidence is weaker on a less precise estimate.
        assert_eq!(s.decision(1.0 / 3.0, 0.082, 0.05), SprtDecision::Better);
        assert_eq!(s.decision(0.375, 0.082, 0.05), SprtDecision::Continue);
        assert_eq!(s.decision(f64::NAN, 0.3, 0.05), SprtDecision::Continue);
        assert_eq!(s.decision(0.25, f64::NAN, 0.05), SprtDecision::Continue);
    }

    /// K3-9: the ranks around a p99, and what their latencies say about it.
    #[test]
    fn the_p99_has_an_order_statistic_resolution() {
        assert_eq!(p99_rank_band(0), None);
        // 1000 samples: 990 ± 2·√9.9 = 983.7..996.3.
        assert_eq!(p99_rank_band(1000), Some((983, 997)));
        // Few samples: the band reaches the slowest one.
        assert_eq!(p99_rank_band(100), Some((97, 100)));
        assert_eq!(p99_rank_band(1), Some((1, 1)));
        assert_eq!(p99_rank_band(100_000), Some((98_937, 99_063)));
        assert_eq!(p99_spread(10.0, 10.0), 0.0);
        assert_eq!(p99_spread(0.0, 10.0), 0.0);
        assert!((p99_spread(10.0, 20.0) - 2f64.ln() / 4.0).abs() < 1e-12);
        // More than 2× is a cliff; exactly 2× is not.
        assert!(!p99_on_a_cliff(p99_spread(10.0, 20.0)));
        assert!(p99_on_a_cliff(p99_spread(10.0, 21.0)));
        // A pair's resolution is a floor of the noise, like a count's.
        let mut t = AbTest::new(false, 3, 6);
        let mut plain = t.clone();
        for i in 0..4 {
            let p = pair(100.0, 100.0, i % 2 == 0);
            plain.push(p);
            t.push(AbPair {
                resolution: 0.3,
                ..p
            });
        }
        assert_eq!(plain.noise_floor(), MIN_NOISE_SD);
        assert!((t.noise_floor() - 0.3).abs() < 1e-12);
        let w = |i: Interval| i.high - i.low;
        assert!(w(t.interval()) > 10.0 * w(plain.interval()));
    }

    /// The power the 9.3.2 statistics lost, back: a +15 % change found as often under a
    /// symmetric order effect (the second trial of every pair 15 % better or worse) and
    /// under a drift both arms share (the service's level a random walk of 25 % per pair)
    /// as without them. 9.3.2 put the order effect into the spread and the drift into the
    /// noise floor: on these seeds it found the change in 78.6 % of six-pair proofs without
    /// them, but in 1.1 % and 1.2 % under the order effect and 2.0 % under the drift (27.9 %
    /// and 4.5 % of twelve-pair proofs). Now: 66 to 70 % at six pairs whatever the order
    /// effect or drift (the pooled spread's lost degree of freedom costs some 9 points
    /// without either), 99.5 % at twelve; a change that is none, at most 2.4 %.
    #[test]
    fn a_real_change_is_found_through_an_order_effect_and_a_drift() {
        let (sigma, effect) = (0.05, 1.15f64.ln());
        let world = |carry, drift| World {
            sigma,
            effect,
            carry,
            drift,
        };
        for (max_pairs, at_least) in [(6, 0.6), (12, 0.95)] {
            let plain = world(0.0, 0.0).verdict_rate(9_300, 3000, max_pairs);
            assert!(plain >= at_least, "{max_pairs} pairs, plain: {plain}");
            for (carry, drift) in [(0.15, 0.0), (-0.15, 0.0), (0.0, 0.25), (0.15, 0.25)] {
                let rate = world(carry, drift).verdict_rate(9_301, 3000, max_pairs);
                assert!(
                    rate >= at_least && rate >= plain - 0.1,
                    "{max_pairs} pairs, order {carry}, drift {drift}: {rate} (plain {plain})"
                );
            }
        }
    }

    /// Neither the pooled within-order spread nor the paired floor lets a change that is
    /// none through more often: under order effects and drifts, for every plan, at most 5 %.
    #[test]
    fn no_change_is_called_through_an_order_effect_or_a_drift() {
        for max_pairs in MIN_DECISIVE_PAIRS..=MAX_PAIRS {
            for (i, (carry, drift)) in [(0.15, 0.0), (-0.15, 0.0), (0.0, 0.25), (0.15, 0.25)]
                .into_iter()
                .enumerate()
            {
                let w = World {
                    sigma: 0.05,
                    effect: 0.0,
                    carry,
                    drift,
                };
                let rate =
                    w.verdict_rate(5_000 + 10 * max_pairs as u64 + i as u64, 3000, max_pairs);
                assert!(
                    rate <= 0.05,
                    "{max_pairs} pairs, order {carry}, drift {drift}: {rate}"
                );
            }
        }
    }
}
