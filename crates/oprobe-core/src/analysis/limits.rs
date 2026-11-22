//! Limit-model fitting (design v5 §4, fixes A1, A3, A5, A7, A8, A9): competing hypotheses
//! about what limits a run, fitted side by side to its request rows and selected by penalised
//! likelihood (BIC: `−2·LL + k·ln n`).
//!
//! **Throttle family** — a Bernoulli model of every request's throttled flag (429/503). Each
//! model predicts which arrivals the service rejects; the observed misclassification rates on
//! either side are its two free noise parameters:
//! - [`HypothesisKind::None`]: throttling at a constant rate, unrelated to load or time
//!   (random 503s);
//! - [`HypothesisKind::InflightLimit`]: arrivals above `ceiling` requests in flight;
//! - [`HypothesisKind::RateLimit`]: a token bucket (`rps`, `burst`) replayed over the arrival
//!   times — the throttle share then depends on the accepted rate per window, not on
//!   concurrency;
//! - [`HypothesisKind::KeyLimit`]: one token bucket per key (or per prefix);
//! - [`HypothesisKind::TimeVarying`]: a token bucket whose rate changes at a change point.
//!
//! **Latency family** — when nothing (significant) is throttled:
//! - [`HypothesisKind::Queueing`]: time to first byte grows with the requests in flight above
//!   `servers` (`W = s · (1 + max(0, L − c) / c)`), against a constant latency;
//! - [`HypothesisKind::BandwidthCap`]: each stream's body rate is the aggregate cap divided by
//!   the concurrent streams (`rate = C / n̄`), against a per-stream rate independent of `n̄`.
//!
//! [`HypothesisKind::ClientBound`] comes from the client's CPU: a saturated load generator
//! overrides the latency family (its throughput says nothing about the service).
//!
//! The selected hypothesis is listed first. Its confidence comes from the BIC gap to the best
//! alternative (≥ 10: high, ≥ 4: medium); no limit is asserted below medium confidence.

use std::collections::HashMap;

use super::capacity::Confidence;
use super::hypothesis::{Hypothesis, HypothesisKind, HypothesisParams};
use crate::model::{OpRow, ReqRow};

/// What the rows do not carry: the client's own saturation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FitInputs {
    /// Busy CPU of the load generator as a share of its cores (1 = every core busy).
    pub client_cpu_busy_ratio: Option<f64>,
}

/// BIC gap to the runner-up for high confidence.
const BIC_HIGH: f64 = 10.0;
/// BIC gap to the runner-up for medium confidence.
const BIC_MEDIUM: f64 = 4.0;
/// Throttled requests below this count are never fitted (a handful of 503s is noise).
const MIN_THROTTLED: u64 = 5;
/// CPU busy share at which the client is the bottleneck.
pub const CLIENT_BOUND_RATIO: f64 = 0.85;

/// One request as the models see it.
#[derive(Clone, Copy, Debug)]
struct Obs {
    /// Arrival at the server (µs).
    t: i64,
    /// Requests in flight at the arrival, itself included.
    level: u32,
    thr: bool,
    ok: bool,
    ttfb_us: u64,
    /// Index into the key table.
    key: u32,
    /// Index into the prefix table.
    prefix: u32,
    body: (i64, i64),
    bytes_down: u64,
}

/// `[start, end)` as the server sees a request (as in `capacity`).
fn server_interval(q: &ReqRow) -> (i64, i64) {
    let start = q.t_us + (q.queue_us + q.connect_us()) as i64;
    let end = if q.ttfb_us > 0 {
        start + (q.send_us + q.ttfb_us) as i64
    } else {
        q.t_us + q.total_us as i64
    };
    (start, end.max(start + 1))
}

fn prefix_of(key: &str) -> &str {
    key.rfind('/').map_or("", |i| &key[..i])
}

/// The rows as observations, in arrival order, with their in-flight levels.
fn observations(reqs: &[&ReqRow], ops: &[&OpRow]) -> (Vec<Obs>, Vec<String>, Vec<String>) {
    let key_of: HashMap<u64, &str> = ops.iter().map(|o| (o.op_id, o.key.as_str())).collect();
    let mut keys: Vec<String> = Vec::new();
    let mut prefixes: Vec<String> = Vec::new();
    let mut kidx: HashMap<&str, u32> = HashMap::new();
    let mut pidx: HashMap<&str, u32> = HashMap::new();
    let iv: Vec<(i64, i64)> = reqs.iter().map(|q| server_interval(q)).collect();
    let mut ev: Vec<(i64, i32, usize)> = Vec::with_capacity(iv.len() * 2);
    for (i, (s, e)) in iv.iter().enumerate() {
        ev.push((*s, 1, i));
        ev.push((*e, -1, i));
    }
    ev.sort_unstable_by_key(|e| (e.0, e.1));
    let mut level = vec![1u32; reqs.len()];
    let mut cur = 0i64;
    for (_, d, i) in &ev {
        cur += i64::from(*d);
        if *d > 0 {
            level[*i] = cur.max(1) as u32;
        }
    }
    let mut out: Vec<Obs> = reqs
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let key = key_of.get(&q.op_id).copied().unwrap_or("");
            let n = kidx.len() as u32;
            let k = *kidx.entry(key).or_insert_with(|| {
                keys.push(key.to_owned());
                n
            });
            let pre = prefix_of(key);
            let n = pidx.len() as u32;
            let p = *pidx.entry(pre).or_insert_with(|| {
                prefixes.push(pre.to_owned());
                n
            });
            let body_start = q.t_us + (q.queue_us + q.connect_us() + q.send_us + q.ttfb_us) as i64;
            Obs {
                t: iv[i].0,
                level: level[i],
                thr: q.is_throttle(),
                ok: !q.is_error() || q.status == 404,
                ttfb_us: q.ttfb_us,
                key: k,
                prefix: p,
                body: (body_start, body_start + q.body_us as i64),
                bytes_down: q.bytes_down,
            }
        })
        .collect();
    out.sort_by_key(|o| o.t);
    (out, keys, prefixes)
}

fn ln_clamped(p: f64) -> f64 {
    p.clamp(1e-9, 1.0 - 1e-9).ln()
}

/// Log-likelihood of `k` successes in `n` Bernoulli trials at the MLE rate `k/n`.
fn bern_ll(k: u64, n: u64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let p = k as f64 / n as f64;
    k as f64 * ln_clamped(p) + (n - k) as f64 * ln_clamped(1.0 - p)
}

/// Counts of a two-class prediction: `(predicted clean, of which throttled, predicted
/// throttled, of which throttled)`.
#[derive(Clone, Copy, Debug, Default)]
struct Split {
    n0: u64,
    k0: u64,
    n1: u64,
    k1: u64,
}

impl Split {
    fn merge(self, o: Split) -> Split {
        Split {
            n0: self.n0 + o.n0,
            k0: self.k0 + o.k0,
            n1: self.n1 + o.n1,
            k1: self.k1 + o.k1,
        }
    }
    /// The log-likelihood with the two rates at their MLE. A model whose "throttled" side is
    /// cleaner than its clean side predicts backwards: it scores as no split.
    fn ll(&self) -> f64 {
        let r0 = self.k0 as f64 / self.n0.max(1) as f64;
        let r1 = self.k1 as f64 / self.n1.max(1) as f64;
        if self.n1 > 0 && r1 < r0 {
            return bern_ll(self.k0 + self.k1, self.n0 + self.n1);
        }
        bern_ll(self.k0, self.n0) + bern_ll(self.k1, self.n1)
    }
    /// Share of the throttled requests predicted throttled.
    fn recall(&self) -> f64 {
        self.k1 as f64 / (self.k0 + self.k1).max(1) as f64
    }
}

/// The threshold `c` on `value` that best separates throttled requests (predicted when
/// `value > c`) from served ones, by likelihood: `(c, split)`.
fn best_threshold(values: impl Iterator<Item = (u32, bool)>) -> Option<(u32, Split)> {
    let mut hist: Vec<(u64, u64)> = Vec::new();
    for (v, thr) in values {
        let v = v as usize;
        if hist.len() <= v {
            hist.resize(v + 1, (0, 0));
        }
        hist[v].0 += 1;
        hist[v].1 += u64::from(thr);
    }
    let n: u64 = hist.iter().map(|x| x.0).sum();
    let k: u64 = hist.iter().map(|x| x.1).sum();
    let mut below = (0u64, 0u64);
    let mut best: Option<(u32, Split)> = None;
    for (c, (nl, kl)) in hist.iter().enumerate() {
        below.0 += nl;
        below.1 += kl;
        let s = Split {
            n0: below.0,
            k0: below.1,
            n1: n - below.0,
            k1: k - below.1,
        };
        if s.n1 == 0 {
            break;
        }
        if best.as_ref().is_none_or(|b| s.ll() > b.1.ll()) {
            best = Some((c as u32, s));
        }
    }
    best
}

/// Replay a token bucket of `rate` tokens/s and `burst` over `obs` (arrival order), spending
/// a token for every request the service actually served (so the replay cannot drift away from
/// the run), per key when `by` is given. Returns each request's score: how empty the bucket
/// was when it arrived (quarter tokens below full; higher = more likely throttled).
fn bucket_scores(
    obs: &[Obs],
    rate: f64,
    burst: f64,
    by: Option<&dyn Fn(&Obs) -> u32>,
    nkeys: usize,
) -> Vec<u32> {
    let burst = burst.max(1.0);
    let n = if by.is_some() { nkeys.max(1) } else { 1 };
    let mut tokens = vec![burst; n];
    let mut last = vec![i64::MIN; n];
    obs.iter()
        .map(|o| {
            let k = by.map_or(0, |f| f(o) as usize);
            if last[k] != i64::MIN {
                tokens[k] = (tokens[k] + (o.t - last[k]).max(0) as f64 * 1e-6 * rate).min(burst);
            }
            last[k] = o.t;
            let score = ((burst - tokens[k]) * 4.0).round().max(0.0) as u32;
            if !o.thr {
                tokens[k] = (tokens[k] - 1.0).max(0.0);
            }
            score
        })
        .collect()
}

/// A fitted model before it becomes a [`Hypothesis`].
#[derive(Clone, Debug)]
struct Fit {
    kind: HypothesisKind,
    params: HypothesisParams,
    ll: f64,
    k: f64,
    explains: f64,
    evidence: Vec<String>,
}

impl Fit {
    fn bic(&self, n: f64) -> f64 {
        -2.0 * self.ll + self.k * n.max(2.0).ln()
    }
}

/// `n` log-spaced values from `lo` to `hi`.
fn log_grid(lo: f64, hi: f64, n: usize) -> Vec<f64> {
    let (a, b) = (lo.ln(), hi.ln());
    (0..n)
        .map(|i| (a + (b - a) * i as f64 / (n - 1).max(1) as f64).exp())
        .collect()
}

/// Served requests per second over the 250 ms bins that saw throttling — overall, or the
/// median over the keys that were throttled. Under a token bucket this is its rate: while the
/// bucket is empty the service serves exactly what refills.
fn served_rate(obs: &[Obs], by: Option<&dyn Fn(&Obs) -> u32>, nkeys: usize) -> f64 {
    const BIN_US: i64 = 250_000;
    let Some(t0) = obs.first().map(|o| o.t) else {
        return 0.0;
    };
    let t_end = obs.last().map_or(t0, |o| o.t);
    // The length of bin `b` covered by the observations (the first and last are partial).
    let cover = |b: i64| -> f64 {
        let (lo, hi) = (t0 + b * BIN_US, t0 + (b + 1) * BIN_US);
        (hi.min(t_end + 1) - lo).clamp(1, BIN_US) as f64 / 1e6
    };
    let n = if by.is_some() { nkeys.max(1) } else { 1 };
    // Per key: bin → (served, throttled).
    let mut per: Vec<HashMap<i64, (u64, u64)>> = vec![HashMap::new(); n];
    for o in obs {
        let k = by.map_or(0, |f| f(o) as usize);
        let e = per[k].entry((o.t - t0) / BIN_US).or_default();
        e.0 += u64::from(!o.thr);
        e.1 += u64::from(o.thr);
    }
    let rates: Vec<f64> = per
        .iter()
        .filter_map(|bins| {
            let hot: Vec<(&i64, &(u64, u64))> = bins.iter().filter(|b| b.1.1 > 0).collect();
            (!hot.is_empty()).then(|| {
                hot.iter().map(|b| b.1.0).sum::<u64>() as f64
                    / hot.iter().map(|b| cover(*b.0)).sum::<f64>()
            })
        })
        .collect();
    super::stats::median(&rates)
}

/// The best token bucket over `obs`: `(rate, burst, split)`.
fn best_rate(
    obs: &[Obs],
    by: Option<&dyn Fn(&Obs) -> u32>,
    nkeys: usize,
) -> Option<(f64, f64, Split)> {
    let base = served_rate(obs, by, nkeys);
    if obs.is_empty() || base <= 0.0 {
        return None;
    }
    let eval = |r: f64, b: f64| {
        let sc = bucket_scores(obs, r, b, by, nkeys);
        best_threshold(sc.iter().zip(obs).map(|(v, o)| (*v, o.thr))).map(|(_, s)| (r, b, s))
    };
    let better = |a: &Option<(f64, f64, Split)>, b: Option<(f64, f64, Split)>| match (a, b) {
        (Some(x), Some(y)) if y.2.ll() > x.2.ll() => Some(y),
        (None, y) => y,
        (x, _) => *x,
    };
    // The rate is pinned near what the service served while it throttled; the likelihood
    // picks the burst and the exact rate.
    let mut best = None;
    for r in log_grid(base * 0.85, base * 1.15, 9) {
        for bf in [0.02, 0.05, 0.1, 0.3] {
            best = better(&best, eval(r, (r * bf).max(1.0)));
        }
    }
    best
}

fn throttle_family(obs: &[Obs], keys: &[String], prefixes: &[String]) -> Vec<Fit> {
    let n = obs.len() as u64;
    let k: u64 = obs.iter().filter(|o| o.thr).count() as u64;
    let mut fits = Vec::new();
    // None: a constant throttle rate.
    fits.push(Fit {
        kind: HypothesisKind::None,
        params: HypothesisParams::default(),
        ll: bern_ll(k, n),
        k: 1.0,
        explains: 0.0,
        evidence: vec![format!(
            "throttling at a constant {:.2}% of requests explains the data as well as any limit: it does not follow load, rate or keys",
            k as f64 / n.max(1) as f64 * 100.0
        )],
    });
    // In-flight: a step at `c`.
    if let Some((c, s)) = best_threshold(obs.iter().map(|o| (o.level, o.thr))) {
        let lo_share = s.k0 as f64 / s.n0.max(1) as f64;
        let hi_share = s.k1 as f64 / s.n1.max(1) as f64;
        fits.push(Fit {
            kind: HypothesisKind::InflightLimit,
            params: HypothesisParams {
                ceiling_inflight_count: Some(u64::from(c)),
                ..Default::default()
            },
            ll: s.ll(),
            k: 3.0,
            explains: s.recall(),
            evidence: vec![format!(
                "{:.1}% of arrivals above {c} requests in flight were throttled, {:.1}% at or below",
                hi_share * 100.0,
                lo_share * 100.0
            )],
        });
    }
    // Rate: a sliding-window limiter over all requests.
    let global = best_rate(obs, None, 0);
    if let Some((r, b, s)) = &global {
        fits.push(Fit {
            kind: HypothesisKind::RateLimit,
            params: HypothesisParams {
                requests_per_s: Some(*r),
                burst_count: Some(b.round() as u64),
                ..Default::default()
            },
            ll: s.ll(),
            k: 5.0,
            explains: s.recall(),
            evidence: vec![format!(
                "a token bucket of ≈{r:.0} requests/s (burst {b:.0}) replayed over the arrivals predicts {:.0}% of the throttled responses",
                s.recall() * 100.0
            )],
        });
    }
    // Key / prefix: one limiter per key.
    for (by_prefix, names) in [(false, keys), (true, prefixes)] {
        if names.len() < 2 {
            continue;
        }
        let pick = move |o: &Obs| if by_prefix { o.prefix } else { o.key };
        let Some((r, b, s)) = best_rate(obs, Some(&pick), names.len()) else {
            continue;
        };
        let mut per: Vec<(u64, u64)> = vec![(0, 0); names.len()];
        for o in obs {
            let e = &mut per[pick(o) as usize];
            e.0 += 1;
            e.1 += u64::from(o.thr);
        }
        let hot = per
            .iter()
            .enumerate()
            .max_by_key(|(_, (_, t))| *t)
            .map(|(i, (a, t))| (names[i].clone(), *a, *t));
        let what = if by_prefix { "prefix" } else { "key" };
        let mut evidence = vec![format!(
            "one token bucket of ≈{r:.1} requests/s per {what} (burst {b:.0}) predicts {:.0}% of the throttled responses",
            s.recall() * 100.0
        )];
        let throttled_keys = per.iter().filter(|(_, t)| *t > 0).count();
        if let Some((name, nreq, nthr)) = &hot {
            evidence.push(format!(
                "the hottest {what} `{name}` took {:.1}% of requests and {:.1}% of the throttled responses; {throttled_keys} of {} {what}s were throttled",
                *nreq as f64 / n.max(1) as f64 * 100.0,
                *nthr as f64 / k.max(1) as f64 * 100.0,
                names.len(),
            ));
        }
        let fit = Fit {
            kind: HypothesisKind::KeyLimit,
            params: HypothesisParams {
                requests_per_s: Some(r),
                burst_count: Some(b.round() as u64),
                key_share_ratio: hot.as_ref().map(|h| h.1 as f64 / n.max(1) as f64),
                key: hot.map(|h| h.0),
                ..Default::default()
            },
            ll: s.ll(),
            k: 6.0,
            explains: s.recall(),
            evidence,
        };
        match fits.iter().position(|f| f.kind == HypothesisKind::KeyLimit) {
            Some(i) if fits[i].ll >= fit.ll => {}
            Some(i) => fits[i] = fit,
            None => fits.push(fit),
        }
    }
    // Time-varying: the bucket's rate changes once (its own rate on each side of a change
    // point, the same burst).
    if let Some((r_all, b_all, _)) = global
        && obs.len() >= 40
    {
        let seg_fit = |seg: &[Obs]| -> Option<(f64, Split)> {
            let base = served_rate(seg, None, 0).max(r_all * 0.05);
            log_grid(base * 0.85, base * 1.15, 9)
                .into_iter()
                .filter_map(|r| {
                    let sc = bucket_scores(seg, r, b_all, None, 0);
                    best_threshold(sc.iter().zip(seg).map(|(v, o)| (*v, o.thr)))
                        .map(|(_, s)| (r, s))
                })
                .max_by(|x, y| x.1.ll().total_cmp(&y.1.ll()))
        };
        let mut best: Option<(usize, f64, f64, Split)> = None;
        for q in (2..=18).map(|i| i as f64 / 20.0) {
            let cut = ((obs.len() as f64) * q) as usize;
            if cut < 20 || obs.len() - cut < 20 {
                continue;
            }
            let (Some((r1, s1)), Some((r2, s2))) = (seg_fit(&obs[..cut]), seg_fit(&obs[cut..]))
            else {
                continue;
            };
            // Both sides must show the limit: enough throttling and at least half a second.
            let side_ok = |seg: &[Obs], sp: &Split| {
                sp.k0 + sp.k1 >= 10
                    && seg.last().map_or(0, |o| o.t) - seg.first().map_or(0, |o| o.t) >= 500_000
            };
            if !side_ok(&obs[..cut], &s1) || !side_ok(&obs[cut..], &s2) {
                continue;
            }
            let s = s1.merge(s2);
            if best.as_ref().is_none_or(|x| s.ll() > x.3.ll()) {
                best = Some((cut, r1, r2, s));
            }
        }
        if let Some((cut, r1, r2, s)) = best {
            let at_s = (obs[cut].t - obs[0].t) as f64 / 1e6;
            // A change smaller than 25 % is the same limit.
            if r1.max(r2) / r1.min(r2).max(1e-9) >= 1.25 {
                fits.push(Fit {
                    kind: HypothesisKind::TimeVarying,
                    params: HypothesisParams {
                        requests_per_s: Some(r1.min(r2)),
                        burst_count: Some(b_all.round() as u64),
                        change_points_s: vec![at_s],
                        segment_requests_per_s: vec![r1, r2],
                        ..Default::default()
                    },
                    ll: s.ll(),
                    k: 8.0,
                    explains: s.recall(),
                    evidence: vec![format!(
                        "the limit changed from ≈{r1:.0} to ≈{r2:.0} requests/s {at_s:.1} s into the window (change point)"
                    )],
                });
            }
        }
    }
    fits
}

/// Gaussian log-likelihood at the MLE variance of residuals with sum of squares `sse`.
fn gauss_ll(sse: f64, n: f64) -> f64 {
    if n < 1.0 {
        return 0.0;
    }
    let var = (sse / n).max(1e-12);
    -0.5 * n * ((2.0 * std::f64::consts::PI * var).ln() + 1.0)
}

/// Queueing vs constant latency over the time to first byte by in-flight level.
fn queueing_fits(obs: &[Obs]) -> Option<(Fit, Fit)> {
    // Per-level sufficient statistics of ln(ttfb ms).
    let mut lv: HashMap<u32, (f64, f64, f64)> = HashMap::new();
    for o in obs.iter().filter(|o| o.ok && !o.thr && o.ttfb_us > 0) {
        let y = (o.ttfb_us as f64 / 1000.0).ln();
        let e = lv.entry(o.level).or_default();
        e.0 += 1.0;
        e.1 += y;
        e.2 += y * y;
    }
    let mut levels: Vec<(u32, (f64, f64, f64))> = lv.into_iter().collect();
    levels.sort_by_key(|x| x.0);
    let populated = levels.iter().filter(|(_, s)| s.0 >= 10.0).count();
    let n: f64 = levels.iter().map(|(_, s)| s.0).sum();
    if populated < 3 || n < 50.0 {
        return None;
    }
    let sy: f64 = levels.iter().map(|(_, s)| s.1).sum();
    let syy: f64 = levels.iter().map(|(_, s)| s.2).sum();
    let null_sse = syy - sy * sy / n;
    let null = Fit {
        kind: HypothesisKind::None,
        params: HypothesisParams {
            service_ms: Some((sy / n).exp()),
            ..Default::default()
        },
        ll: gauss_ll(null_sse, n),
        k: 2.0,
        explains: 0.0,
        evidence: vec![format!(
            "time to first byte stays ≈{:.1} ms whatever the requests in flight",
            (sy / n).exp()
        )],
    };
    let max_level = levels.last().map_or(1, |x| x.0);
    let mut best: Option<(u32, f64, f64)> = None;
    for c in 1..max_level {
        let g = |l: u32| (1.0 + (l.saturating_sub(c)) as f64 / c as f64).ln();
        // Best ln s: mean of y − g(L).
        let shift: f64 = levels.iter().map(|(l, s)| s.1 - s.0 * g(*l)).sum::<f64>() / n;
        let sse: f64 = levels
            .iter()
            .map(|(l, s)| {
                let m = shift + g(*l);
                s.2 - 2.0 * m * s.1 + s.0 * m * m
            })
            .sum();
        if best.as_ref().is_none_or(|b| sse < b.2) {
            best = Some((c, shift.exp(), sse));
        }
    }
    let (c, s_ms, sse) = best?;
    // Effect size: at the busiest well-populated level latency must be ≥ 25 % above service.
    let busiest = levels
        .iter()
        .rev()
        .find(|(_, s)| s.0 >= 10.0)
        .map_or(c, |x| x.0);
    let factor = 1.0 + busiest.saturating_sub(c) as f64 / c as f64;
    let mean_at = |l: u32| {
        levels
            .iter()
            .find(|x| x.0 == l)
            .map_or(0.0, |(_, s)| (s.1 / s.0).exp())
    };
    let lowest = levels.iter().find(|(_, s)| s.0 >= 10.0).map_or(c, |x| x.0);
    let observed_growth = mean_at(busiest) / mean_at(lowest).max(1e-9);
    let mut q = Fit {
        kind: HypothesisKind::Queueing,
        params: HypothesisParams {
            service_ms: Some(s_ms),
            servers_count: Some(u64::from(c)),
            ..Default::default()
        },
        ll: gauss_ll(sse, n),
        k: 3.0,
        explains: ((factor - 1.0) / factor).clamp(0.0, 1.0),
        evidence: vec![format!(
            "time to first byte grows with the requests in flight: {:.1} ms at {lowest}, {:.1} ms at {busiest} (≈{s_ms:.1} ms of service with {c} served at once)",
            mean_at(lowest),
            mean_at(busiest),
        )],
    };
    if factor < 1.25 || observed_growth < 1.2 {
        // Too small to matter: the queueing fit cannot win.
        q.ll = null.ll - 1.0;
    }
    Some((q, null))
}

/// Aggregate vs per-stream bandwidth over the body rates of large responses.
fn bandwidth_fits(obs: &[Obs]) -> Option<(Fit, Fit, f64)> {
    // Concurrent bodies over time.
    let mut ev: Vec<(i64, i32)> = obs
        .iter()
        .filter(|o| o.bytes_down > 0 && o.body.1 > o.body.0)
        .flat_map(|o| [(o.body.0, 1), (o.body.1, -1)])
        .collect();
    if ev.len() < 60 {
        return None;
    }
    ev.sort_unstable();
    // Cumulative area at each event time.
    let mut times = Vec::with_capacity(ev.len());
    let mut area = Vec::with_capacity(ev.len());
    let mut counts = Vec::with_capacity(ev.len());
    let (mut cur, mut a, mut last) = (0i64, 0f64, ev[0].0);
    for (t, d) in &ev {
        a += cur as f64 * (t - last) as f64;
        last = *t;
        cur += i64::from(*d);
        times.push(*t);
        area.push(a);
        counts.push(cur);
    }
    let area_at = |t: i64| -> f64 {
        let i = times.partition_point(|x| *x <= t);
        if i == 0 {
            return 0.0;
        }
        area[i - 1] + counts[i - 1] as f64 * (t - times[i - 1]) as f64
    };
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for o in obs
        .iter()
        .filter(|o| o.ok && o.bytes_down >= 128 * 1024 && o.body.1 - o.body.0 >= 1000)
    {
        let d = (o.body.1 - o.body.0) as f64;
        let nbar = ((area_at(o.body.1) - area_at(o.body.0)) / d).max(1.0);
        xs.push(nbar.ln());
        ys.push((o.bytes_down as f64 / (d / 1e6)).ln());
    }
    let n = xs.len() as f64;
    if n < 30.0 {
        return None;
    }
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    if (sxx / n).sqrt() < 0.02 {
        // The streams never varied: per-stream and aggregate caps look the same.
        return None;
    }
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let slope = sxy / sxx;
    let per_sse: f64 = ys.iter().map(|y| (y - my) * (y - my)).sum();
    let agg_mean = my + mx;
    let agg_sse: f64 = xs
        .iter()
        .zip(&ys)
        .map(|(x, y)| (y + x - agg_mean).powi(2))
        .sum();
    let mib = |v: f64| v / (1024.0 * 1024.0);
    let per = Fit {
        kind: HypothesisKind::None,
        params: HypothesisParams {
            bytes_per_s: Some(my.exp()),
            ..Default::default()
        },
        ll: gauss_ll(per_sse, n),
        k: 2.0,
        explains: 0.0,
        evidence: vec![format!(
            "each stream moves ≈{:.1} MiB/s however many run at once (slope {slope:+.2} of ln rate on ln streams): more streams add throughput",
            mib(my.exp())
        )],
    };
    let agg = Fit {
        kind: HypothesisKind::BandwidthCap,
        params: HypothesisParams {
            bytes_per_s: Some(agg_mean.exp()),
            ..Default::default()
        },
        ll: gauss_ll(agg_sse, n),
        k: 2.0,
        explains: (-slope).clamp(0.0, 1.0),
        evidence: vec![format!(
            "the streams share ≈{:.1} MiB/s: a stream's rate falls as 1/n with the streams running beside it (slope {slope:+.2} of ln rate on ln streams, −1 = a shared cap)",
            mib(agg_mean.exp())
        )],
    };
    Some((agg, per, slope))
}

fn confidence_of(gap: f64, enough: bool) -> Confidence {
    if gap >= BIC_HIGH && enough {
        Confidence::High
    } else if gap >= BIC_MEDIUM {
        Confidence::Medium
    } else {
        Confidence::Low
    }
}

fn to_hypothesis(f: Fit, n: f64, selected: bool, confidence: Confidence) -> Hypothesis {
    Hypothesis {
        kind: f.kind,
        bic: Some(f.bic(n)),
        params: f.params,
        log_likelihood: Some(f.ll),
        evidence: f.evidence,
        confidence,
        explains_ratio: f.explains.clamp(0.0, 1.0),
        selected,
    }
}

/// Rank `fits` by BIC; the best is selected with the confidence of its gap to the next.
fn rank(mut fits: Vec<Fit>, n: f64, enough: bool) -> Vec<Hypothesis> {
    fits.sort_by(|a, b| a.bic(n).total_cmp(&b.bic(n)));
    let gap = match (fits.first(), fits.get(1)) {
        (Some(a), Some(b)) => b.bic(n) - a.bic(n),
        _ => 0.0,
    };
    let best_bic = fits.first().map_or(0.0, |f| f.bic(n));
    fits.into_iter()
        .enumerate()
        .map(|(i, f)| {
            let c = if i == 0 {
                confidence_of(gap, enough)
            } else {
                // An alternative is as likely as the gap allows.
                confidence_of(-(f.bic(n) - best_bic), enough)
            };
            to_hypothesis(f, n, i == 0, c)
        })
        .collect()
}

/// Fit the limit models to the rows that started in `[from_us, to_us)`; the selected
/// hypothesis first.
pub fn fit(
    reqs: &[ReqRow],
    ops: &[OpRow],
    window: (i64, i64),
    inputs: &FitInputs,
) -> Vec<Hypothesis> {
    let inw = |t: i64| t >= window.0 && t < window.1;
    let reqs: Vec<&ReqRow> = reqs.iter().filter(|q| inw(q.t_us)).collect();
    let ops: Vec<&OpRow> = ops.iter().filter(|o| inw(o.t_us)).collect();
    fit_rows(&reqs, &ops, inputs)
}

fn none_hypothesis(evidence: String, confidence: Confidence) -> Hypothesis {
    Hypothesis {
        kind: HypothesisKind::None,
        evidence: vec![evidence],
        confidence,
        selected: true,
        ..Default::default()
    }
}

fn fit_rows(reqs: &[&ReqRow], ops: &[&OpRow], inputs: &FitInputs) -> Vec<Hypothesis> {
    if reqs.is_empty() {
        return vec![none_hypothesis(
            "no request rows in the window".into(),
            Confidence::Low,
        )];
    }
    let (obs, keys, prefixes) = observations(reqs, ops);
    let n = obs.len() as f64;
    let thr = obs.iter().filter(|o| o.thr).count() as u64;
    let thr_ratio = thr as f64 / n;
    let cpu = inputs.client_cpu_busy_ratio.unwrap_or(0.0);
    let client_bound = cpu >= CLIENT_BOUND_RATIO;
    let client = || Hypothesis {
        kind: HypothesisKind::ClientBound,
        params: HypothesisParams::default(),
        log_likelihood: None,
        bic: None,
        evidence: vec![format!(
            "the load generator kept {:.0}% of its CPU busy: it, not the service, set the pace",
            cpu * 100.0
        )],
        confidence: if cpu >= 0.95 {
            Confidence::High
        } else {
            Confidence::Medium
        },
        explains_ratio: 1.0,
        selected: true,
    };
    if thr >= MIN_THROTTLED {
        let fits = throttle_family(&obs, &keys, &prefixes);
        return rank(fits, n, thr >= 20);
    }
    // Latency family.
    let mut out: Vec<Hypothesis> = Vec::new();
    let q = queueing_fits(&obs);
    let bw = bandwidth_fits(&obs);
    let gain = |a: &Fit, b: &Fit| b.bic(n) - a.bic(n);
    let q_gain = q.as_ref().map_or(f64::NEG_INFINITY, |(q, nl)| gain(q, nl));
    let bw_gain = bw
        .as_ref()
        .map_or(f64::NEG_INFINITY, |(a, p, _)| gain(a, p));
    let mut pick: Option<Vec<Hypothesis>> = None;
    if bw_gain > 0.0 && bw_gain >= q_gain {
        let (a, p, _) = bw.clone().expect("gain implies a fit");
        pick = Some(rank(vec![a, p], n, true));
    } else if q_gain > 0.0 {
        let (qf, nl) = q.clone().expect("gain implies a fit");
        pick = Some(rank(vec![qf, nl], n, true));
    }
    match pick {
        Some(mut hs) => {
            // The other family's fit as an unselected alternative.
            if hs[0].kind == HypothesisKind::BandwidthCap
                && let Some((qf, _)) = q
            {
                hs.push(to_hypothesis(qf, n, false, Confidence::Low));
            } else if hs[0].kind == HypothesisKind::Queueing
                && let Some((a, _, _)) = bw
            {
                hs.push(to_hypothesis(a, n, false, Confidence::Low));
            }
            out = hs;
        }
        None => {
            let mut evidence = if thr > 0 {
                format!(
                    "{thr} throttled responses ({:.2}% of requests): too few to fit a limit",
                    thr_ratio * 100.0
                )
            } else {
                "no request was throttled and latency did not grow with load: no limit was reached"
                    .into()
            };
            if let Some((_, p, _)) = &bw {
                evidence = format!("{evidence}; {}", p.evidence[0]);
            }
            out.push(none_hypothesis(
                evidence,
                if reqs.len() >= 1000 {
                    Confidence::Medium
                } else {
                    Confidence::Low
                },
            ));
            for (alt, _) in q.into_iter() {
                out.push(to_hypothesis(alt, n, false, Confidence::Low));
            }
            for (alt, _, _) in bw.into_iter() {
                out.push(to_hypothesis(alt, n, false, Confidence::Low));
            }
        }
    }
    if client_bound {
        for h in &mut out {
            h.selected = false;
        }
        out.insert(0, client());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(t_ms: f64, ttfb_ms: f64, status: u16, op_id: u64) -> ReqRow {
        let mut r = crate::analysis::outliers::test_req(
            (t_ms * 1000.0) as i64,
            (ttfb_ms * 1000.0) as u64 + 10,
        );
        r.op_id = op_id;
        r.status = status;
        r
    }

    fn op(id: u64, key: &str, t_ms: f64) -> OpRow {
        let mut o = crate::analysis::simulate::tests::op(id, (t_ms * 1000.0) as i64, 1000);
        o.key = key.into();
        o
    }

    /// Arrivals every `gap_ms` for `dur_ms`, judged by `throttle(t_ms, level, key)`.
    fn synth(
        gap_ms: f64,
        dur_ms: f64,
        lat_ms: impl Fn(usize) -> f64,
        keys: usize,
        mut throttle: impl FnMut(f64, usize, usize) -> bool,
    ) -> (Vec<ReqRow>, Vec<OpRow>) {
        let mut reqs = Vec::new();
        let mut ops = Vec::new();
        let mut i = 0usize;
        let mut t = 0.0;
        while t < dur_ms {
            let key = if keys > 1 {
                // A skewed key choice: key 0 every third request.
                if i.is_multiple_of(3) {
                    0
                } else {
                    1 + i % (keys - 1)
                }
            } else {
                0
            };
            let level = 1 + i % 16;
            let thr = throttle(t, level, key);
            let lat = if thr { 0.2 } else { lat_ms(level) };
            reqs.push(req(t, lat, if thr { 503 } else { 200 }, i as u64 + 1));
            ops.push(op(i as u64 + 1, &format!("k{key}"), t));
            i += 1;
            t += gap_ms;
        }
        (reqs, ops)
    }

    fn selected(hs: &[Hypothesis]) -> &Hypothesis {
        hs.iter().find(|h| h.selected).expect("one selected")
    }

    #[test]
    fn a_token_bucket_is_a_rate_limit() {
        // 1000 arrivals/s against a bucket of 300/s.
        let mut tokens = 10.0f64;
        let mut last = 0.0;
        let (reqs, ops) = synth(
            1.0,
            4000.0,
            |_| 5.0,
            1,
            |t, _, _| {
                tokens = (tokens + (t - last) / 1000.0 * 300.0).min(10.0);
                last = t;
                if tokens >= 1.0 {
                    tokens -= 1.0;
                    false
                } else {
                    true
                }
            },
        );
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::RateLimit, "{hs:#?}");
        let rps = s.params.requests_per_s.unwrap();
        assert!((rps - 300.0).abs() < 30.0, "{rps}");
        assert!(s.confidence >= Confidence::Medium);
        assert!(s.explains_ratio > 0.6, "{}", s.explains_ratio);
    }

    #[test]
    fn crossing_a_concurrency_is_an_inflight_limit() {
        // Synthetic levels come from overlapping intervals: build them explicitly.
        let mut reqs = Vec::new();
        let mut ops = Vec::new();
        let mut id = 0;
        for burst in 0..200 {
            let t0 = burst as f64 * 50.0;
            let width = 4 + burst % 28;
            for j in 0..width {
                id += 1;
                let thr = j >= 24;
                reqs.push(req(
                    t0 + j as f64 * 0.01,
                    if thr { 0.3 } else { 10.0 },
                    if thr { 503 } else { 200 },
                    id,
                ));
                ops.push(op(id, "k", t0));
            }
        }
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::InflightLimit, "{hs:#?}");
        let c = s.params.ceiling_inflight_count.unwrap();
        assert!((23..=25).contains(&c), "{c}");
    }

    #[test]
    fn random_throttling_is_no_limit() {
        let mut x = 7u64;
        let (reqs, ops) = synth(
            1.0,
            4000.0,
            |_| 5.0,
            1,
            |_, _, _| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 33).is_multiple_of(20)
            },
        );
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        assert_eq!(selected(&hs).kind, HypothesisKind::None, "{hs:#?}");
    }

    #[test]
    fn a_hot_key_is_a_key_limit() {
        let mut buckets = [(4.0f64, 0.0f64); 8];
        let (reqs, ops) = synth(
            1.0,
            4000.0,
            |_| 5.0,
            8,
            |t, _, k| {
                let (tok, last) = &mut buckets[k];
                *tok = (*tok + (t - *last) / 1000.0 * 100.0).min(4.0);
                *last = t;
                if *tok >= 1.0 {
                    *tok -= 1.0;
                    false
                } else {
                    true
                }
            },
        );
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::KeyLimit, "{hs:#?}");
        assert_eq!(s.params.key.as_deref(), Some("k0"));
    }

    #[test]
    fn a_rate_that_drops_is_time_varying() {
        let mut tokens = 10.0f64;
        let mut last = 0.0;
        let (reqs, ops) = synth(
            1.0,
            4000.0,
            |_| 5.0,
            1,
            |t, _, _| {
                let rate = if t < 2000.0 { 600.0 } else { 150.0 };
                tokens = (tokens + (t - last) / 1000.0 * rate).min(10.0);
                last = t;
                if tokens >= 1.0 {
                    tokens -= 1.0;
                    false
                } else {
                    true
                }
            },
        );
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::TimeVarying, "{hs:#?}");
        let at = s.params.change_points_s[0];
        assert!((at - 2.0).abs() < 0.3, "{at}");
        assert!((s.params.requests_per_s.unwrap() - 150.0).abs() < 30.0);
    }

    #[test]
    fn latency_that_grows_with_load_is_queueing() {
        let mut reqs = Vec::new();
        let mut ops = Vec::new();
        let mut id = 0;
        let mut x = 3u64;
        for burst in 0..300 {
            let t0 = burst as f64 * 200.0;
            let width = 2 + burst % 14;
            for j in 0..width {
                id += 1;
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let noise = 1.0 + ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.2;
                let lat = 10.0 * (1.0 + ((j as u64 + 1).saturating_sub(4)) as f64 / 4.0) * noise;
                reqs.push(req(t0 + j as f64 * 0.01, lat, 200, id));
                ops.push(op(id, "k", t0));
            }
        }
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::Queueing, "{hs:#?}");
        let c = s.params.servers_count.unwrap();
        assert!((3..=5).contains(&c), "{c}");
    }

    #[test]
    fn a_busy_client_is_client_bound() {
        let (reqs, ops) = synth(1.0, 1000.0, |_| 5.0, 1, |_, _, _| false);
        let hs = fit(
            &reqs,
            &ops,
            (i64::MIN, i64::MAX),
            &FitInputs {
                client_cpu_busy_ratio: Some(0.97),
            },
        );
        assert_eq!(selected(&hs).kind, HypothesisKind::ClientBound);
    }
}
