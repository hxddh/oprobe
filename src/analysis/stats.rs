//! Small, dependency-free statistics used by analysis, `compare` and `watch`: quantiles,
//! robust summaries and percentile-bootstrap confidence intervals with a deterministic RNG
//! (the same inputs always give the same interval).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Nearest-rank quantile (`q` in 0..=1) of unsorted values; 0 for an empty slice.
pub fn quantile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    quantile_mut(&mut v, q)
}

/// [`quantile`] that reorders `v` instead of copying it.
pub fn quantile_mut(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let i = ((v.len() as f64 * q.clamp(0.0, 1.0)).ceil() as usize).clamp(1, v.len()) - 1;
    let (_, x, _) = v.select_nth_unstable_by(i, f64::total_cmp);
    *x
}

/// Nearest-rank quantile of integer samples (µs) in ms.
pub fn quantile_us_ms(v: &mut [u64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let i = ((v.len() as f64 * q.clamp(0.0, 1.0)).ceil() as usize).clamp(1, v.len()) - 1;
    let (_, x, _) = v.select_nth_unstable(i);
    *x as f64 / 1000.0
}

pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// Sample standard deviation (0 for fewer than two values).
pub fn stddev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = mean(values);
    (values.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (values.len() - 1) as f64).sqrt()
}

/// SplitMix64: tiny, fast, deterministic.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform index in `0..n` (`n > 0`).
    pub fn index(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// An estimate with a confidence interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Interval {
    pub estimate: f64,
    pub low: f64,
    pub high: f64,
}

impl Interval {
    pub fn point(v: f64) -> Self {
        Self {
            estimate: v,
            low: v,
            high: v,
        }
    }
    /// The interval excludes zero (a significant difference).
    pub fn excludes_zero(&self) -> bool {
        self.low > 0.0 || self.high < 0.0
    }
}

/// Bootstrap iterations: enough for stable 95 % percentile intervals, cheap on a few
/// thousand samples.
pub const BOOTSTRAP_ITERS: usize = 1000;

/// Cap on samples resampled per bootstrap iteration (larger inputs are thinned evenly): keeps
/// a 1 M-request run's interval to milliseconds.
const BOOTSTRAP_MAX_SAMPLES: usize = 4000;

fn thin(v: &[f64]) -> Vec<f64> {
    if v.len() <= BOOTSTRAP_MAX_SAMPLES {
        return v.to_vec();
    }
    let step = v.len() as f64 / BOOTSTRAP_MAX_SAMPLES as f64;
    (0..BOOTSTRAP_MAX_SAMPLES)
        .map(|i| v[(i as f64 * step) as usize])
        .collect()
}

fn resample(src: &[f64], rng: &mut Rng, buf: &mut Vec<f64>) {
    buf.clear();
    buf.extend((0..src.len()).map(|_| src[rng.index(src.len())]));
}

/// 95 % percentile-bootstrap interval of `stat` over `values`.
pub fn bootstrap(values: &[f64], stat: impl Fn(&mut [f64]) -> f64, seed: u64) -> Interval {
    if values.is_empty() {
        return Interval::default();
    }
    let src = thin(values);
    let estimate = stat(&mut values.to_vec());
    if src.len() < 2 {
        return Interval::point(estimate);
    }
    let mut rng = Rng::new(seed);
    let mut buf = Vec::with_capacity(src.len());
    let mut stats: Vec<f64> = (0..BOOTSTRAP_ITERS)
        .map(|_| {
            resample(&src, &mut rng, &mut buf);
            stat(&mut buf)
        })
        .collect();
    Interval {
        estimate,
        low: quantile_mut(&mut stats, 0.025),
        high: quantile_mut(&mut stats, 0.975),
    }
}

/// 95 % bootstrap interval of the relative change `stat(b) / stat(a) - 1` (a fraction).
pub fn bootstrap_rel_change(
    a: &[f64],
    b: &[f64],
    stat: impl Fn(&mut [f64]) -> f64,
    seed: u64,
) -> Interval {
    if a.is_empty() || b.is_empty() {
        return Interval::default();
    }
    let rel = |x: f64, y: f64| if x.abs() > 0.0 { y / x - 1.0 } else { 0.0 };
    let estimate = rel(stat(&mut a.to_vec()), stat(&mut b.to_vec()));
    let (sa, sb) = (thin(a), thin(b));
    if sa.len() < 2 && sb.len() < 2 {
        return Interval::point(estimate);
    }
    let mut rng = Rng::new(seed);
    let (mut ba, mut bb) = (Vec::new(), Vec::new());
    let mut stats: Vec<f64> = (0..BOOTSTRAP_ITERS)
        .map(|_| {
            resample(&sa, &mut rng, &mut ba);
            resample(&sb, &mut rng, &mut bb);
            rel(stat(&mut ba), stat(&mut bb))
        })
        .collect();
    Interval {
        estimate,
        low: quantile_mut(&mut stats, 0.025),
        high: quantile_mut(&mut stats, 0.975),
    }
}

/// `stat` closures for [`bootstrap`].
pub fn p(q: f64) -> impl Fn(&mut [f64]) -> f64 {
    move |v: &mut [f64]| quantile_mut(v, q)
}

pub fn med(v: &mut [f64]) -> f64 {
    median(v)
}

/// Two-proportion z statistic (`k1/n1` vs `k2/n2`); 0 when undefined.
pub fn two_proportion_z(k1: f64, n1: f64, k2: f64, n2: f64) -> f64 {
    if n1 <= 0.0 || n2 <= 0.0 {
        return 0.0;
    }
    let (p1, p2) = (k1 / n1, k2 / n2);
    let pool = (k1 + k2) / (n1 + n2);
    let se = (pool * (1.0 - pool) * (1.0 / n1 + 1.0 / n2)).sqrt();
    if se > 0.0 { (p1 - p2) / se } else { 0.0 }
}

/// Unicode sparkline of `values` (8 levels), at most `width` characters (values are
/// averaged into buckets when there are more).
pub fn sparkline(values: &[f64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let buckets: Vec<f64> = if values.len() <= width {
        values.to_vec()
    } else {
        (0..width)
            .map(|i| {
                let a = i * values.len() / width;
                let b = ((i + 1) * values.len() / width).max(a + 1);
                mean(&values[a..b])
            })
            .collect()
    };
    let lo = buckets.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = buckets.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    buckets
        .iter()
        .map(|v| {
            if hi - lo <= f64::EPSILON {
                BARS[3]
            } else {
                BARS[(((v - lo) / (hi - lo)) * 7.0).round().clamp(0.0, 7.0) as usize]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_and_summaries() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(quantile(&v, 0.5), 50.0);
        assert_eq!(quantile(&v, 0.99), 99.0);
        assert_eq!(quantile(&v, 1.0), 100.0);
        assert_eq!(median(&[3.0, 1.0, 2.0, 4.0]), 2.5);
        assert!((stddev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]) - 2.138).abs() < 0.01);
        let mut us = vec![1000, 2000, 3000];
        assert_eq!(quantile_us_ms(&mut us, 0.5), 2.0);
    }

    #[test]
    fn bootstrap_is_deterministic_and_covers_the_estimate() {
        let v: Vec<f64> = (0..500).map(|i| (i % 50) as f64).collect();
        let a = bootstrap(&v, p(0.5), 7);
        let b = bootstrap(&v, p(0.5), 7);
        assert_eq!(a, b);
        assert!(a.low <= a.estimate && a.estimate <= a.high, "{a:?}");
        // A clear shift is significant; identical samples are not.
        let w: Vec<f64> = v.iter().map(|x| x * 2.0 + 10.0).collect();
        let d = bootstrap_rel_change(&v, &w, p(0.5), 1);
        assert!(d.excludes_zero() && d.estimate > 0.5, "{d:?}");
        let same = bootstrap_rel_change(&v, &v, p(0.5), 1);
        assert!(!same.excludes_zero(), "{same:?}");
    }

    #[test]
    fn z_and_sparkline() {
        assert!(two_proportion_z(50.0, 100.0, 10.0, 1000.0) > 10.0);
        assert_eq!(two_proportion_z(1.0, 0.0, 1.0, 1.0), 0.0);
        let s = sparkline(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], 8);
        assert_eq!(s, "▁▂▃▄▅▆▇█");
        assert_eq!(sparkline(&[1.0; 40], 10).chars().count(), 10);
    }
}
