//! The Tune 5 surrogate (design v5 §4, fixes A4, A11): Gaussian processes over the measured
//! trials, one for goodput (net of retry cost) and one for the throttle ratio (on a logit
//! scale), so that
//! - the next trial is the candidate with the highest expected improvement times the chance
//!   it stays within the throttle budget (constrained Bayesian optimisation), and
//! - the recommendation's headroom is a posterior quantile: a configuration is acceptable when
//!   the posterior chance that it exceeds the throttle budget is at most `risk` (`--risk`,
//!   default 0.05), not a fixed 15 % below a ceiling.
//!
//! Inputs are knob vectors scaled to `[0, 1]` ([`features`]); the kernel is squared
//! exponential with a fixed length scale and a measurement-noise term, small enough (n ≤ 40)
//! for a plain Cholesky solve.

use super::Trial;

/// Length scale of the kernel in normalised units.
const LENGTH: f64 = 0.08;
/// Measurement noise (as a share of the output variance).
const NOISE: f64 = 0.02;
/// Throttle ratios are clamped to this floor before the logit (0 throttled ≈ 1e-4).
const THR_FLOOR: f64 = 1e-4;

/// The search space bounds used to scale the knobs.
#[derive(Clone, Copy, Debug)]
pub struct Space {
    pub max_concurrency: usize,
    /// The highest rate tried (ops/s); 0 when no trial is paced.
    pub max_rate: f64,
}

/// A trial's knobs scaled to `[0, 1]`: concurrency (log), rate (share of `max_rate`, 1 for
/// closed loop), io concurrency (log), range gap (log), max in-flight (log, 1 = unlimited),
/// HTTP/2.
pub fn features(t: &Trial, s: &Space) -> Vec<f64> {
    let lg = |v: f64, max: f64| (v.max(1.0).log2() / max.max(2.0).log2()).clamp(0.0, 1.0);
    let k = &t.knobs;
    vec![
        lg(k.concurrency as f64, s.max_concurrency as f64),
        match (t.rate_per_s, s.max_rate > 0.0) {
            (Some(r), true) => (r / s.max_rate).clamp(0.0, 1.0),
            _ => 1.0,
        },
        lg(k.io_concurrency as f64, 16.0) * 0.5,
        lg((k.range_gap.0 / 1024).max(1) as f64, 1024.0) * 0.5,
        if k.max_inflight_requests == 0 {
            1.0
        } else {
            lg(k.max_inflight_requests as f64, 256.0)
        },
        if k.http_version == crate::net::HttpVersion::H2 {
            0.5
        } else {
            0.0
        },
    ]
}

fn kernel(a: &[f64], b: &[f64]) -> f64 {
    let d2: f64 = a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum();
    (-d2 / (2.0 * LENGTH * LENGTH)).exp()
}

/// A fitted Gaussian process (standardised outputs).
pub struct Gp {
    xs: Vec<Vec<f64>>,
    alpha: Vec<f64>,
    chol: Vec<Vec<f64>>,
    mean: f64,
    scale: f64,
}

fn cholesky(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut l = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..=i {
            let s: f64 = (0..j).map(|k| l[i][k] * l[j][k]).sum();
            if i == j {
                let d = a[i][i] - s;
                if d <= 0.0 {
                    return None;
                }
                l[i][j] = d.sqrt();
            } else {
                l[i][j] = (a[i][j] - s) / l[j][j];
            }
        }
    }
    Some(l)
}

fn solve_lower(l: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    let n = b.len();
    let mut y = vec![0.0; n];
    for i in 0..n {
        let s: f64 = (0..i).map(|k| l[i][k] * y[k]).sum();
        y[i] = (b[i] - s) / l[i][i];
    }
    y
}

fn solve_upper_t(l: &[Vec<f64>], y: &[f64]) -> Vec<f64> {
    let n = y.len();
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let s: f64 = (i + 1..n).map(|k| l[k][i] * x[k]).sum();
        x[i] = (y[i] - s) / l[i][i];
    }
    x
}

impl Gp {
    /// Fit to `(x, y)`; `None` with fewer than two points.
    pub fn fit(xs: Vec<Vec<f64>>, ys: &[f64]) -> Option<Self> {
        if xs.len() < 2 || xs.len() != ys.len() {
            return None;
        }
        let n = ys.len() as f64;
        let mean = ys.iter().sum::<f64>() / n;
        let var = ys.iter().map(|y| (y - mean).powi(2)).sum::<f64>() / n;
        let scale = var.sqrt().max(1e-6);
        let z: Vec<f64> = ys.iter().map(|y| (y - mean) / scale).collect();
        let k: Vec<Vec<f64>> = xs
            .iter()
            .enumerate()
            .map(|(i, a)| {
                xs.iter()
                    .enumerate()
                    .map(|(j, b)| kernel(a, b) + if i == j { NOISE } else { 0.0 })
                    .collect()
            })
            .collect();
        let chol = cholesky(&k)?;
        let alpha = solve_upper_t(&chol, &solve_lower(&chol, &z));
        Some(Self {
            xs,
            alpha,
            chol,
            mean,
            scale,
        })
    }

    /// Posterior mean and standard deviation at `x` (output units).
    pub fn predict(&self, x: &[f64]) -> (f64, f64) {
        let ks: Vec<f64> = self.xs.iter().map(|a| kernel(a, x)).collect();
        let mu: f64 = ks.iter().zip(&self.alpha).map(|(k, a)| k * a).sum();
        let v = solve_lower(&self.chol, &ks);
        let var = (1.0 + NOISE - v.iter().map(|x| x * x).sum::<f64>()).max(1e-9);
        (self.mean + mu * self.scale, var.sqrt() * self.scale)
    }
}

/// Standard normal CDF (Abramowitz–Stegun 7.1.26 via erf).
pub fn phi(z: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * z.abs() / std::f64::consts::SQRT_2);
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-(z * z) / 2.0).exp();
    if z >= 0.0 {
        0.5 * (1.0 + y)
    } else {
        0.5 * (1.0 - y)
    }
}

fn pdf(z: f64) -> f64 {
    (-(z * z) / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

fn logit(p: f64) -> f64 {
    let p = p.clamp(THR_FLOOR, 1.0 - THR_FLOOR);
    (p / (1.0 - p)).ln()
}

/// Goodput net of retry cost: the share of requests wasted on throttled attempts is taken off.
pub fn net_goodput(t: &Trial, bytes: bool) -> f64 {
    let g = if bytes { t.mib_per_s } else { t.ops_per_s };
    g * (1.0 - t.throttled_request_ratio.clamp(0.0, 1.0))
}

/// The two surrogates over the measured trials.
pub struct Surrogate {
    pub space: Space,
    goodput: Option<Gp>,
    throttle: Option<Gp>,
    budget_logit: f64,
    best: f64,
}

impl Surrogate {
    /// Fit over `trials` (confirmation runs left out).
    pub fn fit(trials: &[Trial], space: Space, bytes: bool, budget: f64) -> Self {
        let pts: Vec<&Trial> = trials.iter().filter(|t| t.stage != "confirm").collect();
        let xs: Vec<Vec<f64>> = pts.iter().map(|t| features(t, &space)).collect();
        let g: Vec<f64> = pts.iter().map(|t| net_goodput(t, bytes)).collect();
        let th: Vec<f64> = pts
            .iter()
            .map(|t| logit(t.throttled_request_ratio))
            .collect();
        let best = pts
            .iter()
            .filter(|t| t.throttled_request_ratio <= budget && t.feasible)
            .map(|t| net_goodput(t, bytes))
            .fold(0.0, f64::max);
        Self {
            space,
            goodput: Gp::fit(xs.clone(), &g),
            throttle: Gp::fit(xs, &th),
            budget_logit: logit(budget.max(THR_FLOOR * 2.0)),
            best,
        }
    }

    /// Posterior chance that `t`'s configuration throttles more than the budget.
    pub fn exceed_probability(&self, t: &Trial) -> Option<f64> {
        let gp = self.throttle.as_ref()?;
        let (mu, sd) = gp.predict(&features(t, &self.space));
        Some(1.0 - phi((self.budget_logit - mu) / sd.max(1e-6)))
    }

    /// Constrained expected improvement of a candidate configuration.
    pub fn acquisition(&self, t: &Trial) -> Option<f64> {
        let gp = self.goodput.as_ref()?;
        let (mu, sd) = gp.predict(&features(t, &self.space));
        let z = (mu - self.best) / sd.max(1e-9);
        let ei = (mu - self.best) * phi(z) + sd * pdf(z);
        let feasible = 1.0 - self.exceed_probability(t).unwrap_or(0.5);
        Some(ei.max(0.0) * feasible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Knobs;

    fn trial(c: usize, ops: f64, thr: f64) -> Trial {
        Trial {
            stage: "concurrency".into(),
            knobs: Knobs {
                concurrency: c,
                ..Knobs::default()
            },
            ops_per_s: ops,
            throttled_request_ratio: thr,
            feasible: thr <= 0.001,
            ..Default::default()
        }
    }

    #[test]
    fn the_posterior_trusts_repeated_clean_trials() {
        let space = Space {
            max_concurrency: 64,
            max_rate: 0.0,
        };
        // Clean at 24 twice, throttled from 32.
        let trials = vec![
            trial(4, 400.0, 0.0),
            trial(8, 800.0, 0.0),
            trial(16, 1500.0, 0.0),
            trial(24, 2100.0, 0.0),
            trial(24, 2080.0, 0.0),
            trial(32, 2000.0, 0.05),
            trial(64, 1500.0, 0.3),
        ];
        let s = Surrogate::fit(&trials, space, false, 0.001);
        let p24 = s.exceed_probability(&trial(24, 0.0, 0.0)).unwrap();
        let p32 = s.exceed_probability(&trial(32, 0.0, 0.0)).unwrap();
        assert!(p24 < 0.05, "{p24}");
        assert!(p32 > 0.5, "{p32}");
        // Exploring beyond the throttled region is not attractive.
        let a20 = s.acquisition(&trial(20, 0.0, 0.0)).unwrap();
        let a64 = s.acquisition(&trial(64, 0.0, 0.0)).unwrap();
        assert!(a20 >= a64, "{a20} {a64}");
        assert!((phi(0.0) - 0.5).abs() < 1e-6 && (phi(1.96) - 0.975).abs() < 1e-3);
    }
}
