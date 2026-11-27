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
//! - [`HypothesisKind::BandwidthCap`]: each stream's body rate (response bodies, or request
//!   bodies on an upload-bound link) is the aggregate cap divided by
//!   the concurrent streams (`rate = C / n̄`), against a per-stream rate independent of `n̄`.
//!
//! [`HypothesisKind::ClientBound`] comes from the client's CPU: a saturated load generator
//! overrides the latency family (its throughput says nothing about the service).
//!
//! The selected hypothesis is listed first. Its confidence comes from the BIC gap to the best
//! alternative (≥ 10: high, ≥ 4: medium); no limit is asserted below medium confidence.

use std::collections::HashMap;

use super::capacity::Confidence;
use super::hypothesis::{Direction, Hypothesis, HypothesisKind, HypothesisParams};
use crate::model::{OpRow, ReqRow};

/// What the rows do not carry: the client's own saturation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FitInputs {
    /// Busy CPU of the load generator as a share of its cores (1 = every core busy).
    pub client_cpu_busy_ratio: Option<f64>,
    /// The client's scheduler lagged (`report.self.client_saturated`, T7): its timing, not
    /// the service, set the pace.
    pub client_saturated: bool,
}

/// BIC gap to the runner-up for high confidence.
const BIC_HIGH: f64 = 10.0;
/// BIC gap to the runner-up for medium confidence.
const BIC_MEDIUM: f64 = 4.0;
/// The smallest token-bucket burst tried, as a share of its rate (per second).
const MIN_BURST_FACTOR: f64 = 0.02;
/// The largest token-bucket burst tried, in seconds of its rate.
const MAX_BURST_FACTOR: f64 = 10.0;
/// The token-bucket bursts tried, in seconds of its rate.
const BURST_FACTORS: [f64; 7] = [MIN_BURST_FACTOR, 0.05, 0.1, 0.3, 1.0, 3.0, MAX_BURST_FACTOR];
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
    /// The request's API class (an index: equal for requests of one storage API).
    api: u32,
    body: (i64, i64),
    bytes_down: u64,
    /// When the request body was sent (µs), and its bytes.
    upload: (i64, i64),
    bytes_up: u64,
    /// Service time as the server sees it: send + time to first byte (µs).
    svc_us: i64,
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
    let mut aidx: HashMap<&str, u32> = HashMap::new();
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
            let n = aidx.len() as u32;
            let api = *aidx.entry(q.api.label()).or_insert(n);
            let send_start = q.t_us + (q.queue_us + q.connect_us()) as i64;
            let body_start = send_start + (q.send_us + q.ttfb_us) as i64;
            Obs {
                api,
                t: iv[i].0,
                level: level[i],
                thr: q.is_throttle(),
                ok: !q.is_error() || q.status == 404,
                ttfb_us: q.ttfb_us,
                key: k,
                prefix: p,
                body: (body_start, body_start + q.body_us as i64),
                bytes_down: q.bytes_down,
                upload: (send_start, send_start + q.send_us as i64),
                bytes_up: q.bytes_up,
                svc_us: iv[i].1 - iv[i].0,
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
            let mut hot: Vec<(&i64, &(u64, u64))> = bins.iter().filter(|b| b.1.1 > 0).collect();
            // In time order: the sum is the same on every run (U9).
            hot.sort_unstable_by_key(|b| *b.0);
            (!hot.is_empty()).then(|| {
                hot.iter().map(|b| b.1.0).sum::<u64>() as f64
                    / hot.iter().map(|b| cover(*b.0)).sum::<f64>()
            })
        })
        .collect();
    super::stats::median(&rates)
}

/// Median served requests per second over the fully covered 250 ms bins of `obs` that saw
/// throttling. A segment of a time-varying limit starts at a change point placed on a coarse
/// grid, so it can begin with a few bins of the other rate: pooled (as in [`served_rate`])
/// they pull the estimate toward it — the lower limit of 150/s read as up to ≈200/s, and the
/// advised pace exceeded it — while the median keeps the segment's own rate.
fn served_rate_median(obs: &[Obs]) -> f64 {
    const BIN_US: i64 = 250_000;
    let (Some(t0), Some(t1)) = (obs.first().map(|o| o.t), obs.last().map(|o| o.t)) else {
        return 0.0;
    };
    let full = ((t1 - t0) / BIN_US).max(1);
    let mut bins: HashMap<i64, (u64, u64)> = HashMap::new();
    for o in obs {
        let e = bins.entry((o.t - t0) / BIN_US).or_default();
        e.0 += u64::from(!o.thr);
        e.1 += u64::from(o.thr);
    }
    let rates: Vec<f64> = bins
        .iter()
        .filter(|(b, (_, t))| *t > 0 && (**b < full || full == 1))
        .map(|(_, (served, _))| *served as f64 / (BIN_US as f64 / 1e6))
        .collect();
    if rates.is_empty() {
        return served_rate(obs, None, 0);
    }
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
    // picks the burst and the exact rate. A bucket that allows spikes (a burst of seconds of
    // its rate) refills during each lull and drains in the next throttled stretch, so what
    // the service served then runs above its rate: the grid reaches down to 0.7 of it, and
    // bursts up to [`MAX_BURST_FACTOR`] seconds of it. 9.3.2 tried bursts up to 0.3 s and
    // rates from 0.85 of what was served: a bucket of 50/s with a burst of 200 read as 64/s,
    // one of 200/s with a burst of 1 000 as 276/s. (Lower than 0.7, a short-burst bucket
    // trades its rate for a burst it does not have.)
    let mut best = None;
    for r in log_grid(base * 0.7, base * 1.15, 11) {
        for bf in BURST_FACTORS {
            best = better(&best, eval(r, (r * bf).max(1.0)));
        }
    }
    best
}

/// A cap of `c` in flight serves at most about `c` requests per service time `S`: over any
/// [`CAP_WINDOW_SERVICES`] service times, the requests a cap served stay below this multiple
/// of what it carries (the labs' caps peak at ≈ 1.3× with service-time noise); a token
/// bucket's burst after a lull runs well past it (≈ 2× and more).
const CAP_RATE_RATIO: f64 = 1.5;
/// The window of [`peak_served_ratio`], in mean service times.
const CAP_WINDOW_SERVICES: f64 = 8.0;

/// Request-rate evidence against an in-flight cap of `c` (design 6.3 A1): the most requests
/// the service served (arrivals of served requests) within any window of
/// [`CAP_WINDOW_SERVICES`] mean service times, as a multiple of what `c` requests in flight
/// carry in it (`c / S` per second, Little's law), and the window (µs). A cap never serves
/// much more than that, however blurred its client-side in-flight counts are (arrival times
/// stay exact); a token bucket that refilled during a lull serves its whole burst at once.
fn peak_served_ratio(obs: &[Obs], c: f64, service_s: f64) -> (f64, i64) {
    if c <= 0.0 || service_s <= 0.0 {
        return (0.0, 0);
    }
    let window_us = ((service_s * CAP_WINDOW_SERVICES * 1e6) as i64).max(1);
    let served: Vec<i64> = obs.iter().filter(|o| !o.thr).map(|o| o.t).collect();
    let (mut j, mut peak) = (0usize, 0usize);
    for i in 0..served.len() {
        while served[i] - served[j] >= window_us {
            j += 1;
        }
        peak = peak.max(i + 1 - j);
    }
    (peak as f64 / (c * CAP_WINDOW_SERVICES), window_us)
}

/// A clean in-flight step: `(ceiling, log-likelihood, share throttled at or below, above)`.
type Step = (u32, f64, f64, f64);

/// Mean service time (s) of the served requests in `obs`.
fn served_service_s(obs: &[Obs]) -> f64 {
    let (sum, n) = obs
        .iter()
        .filter(|o| !o.thr)
        .fold((0i64, 0u64), |a, o| (a.0 + o.svc_us.max(0), a.1 + 1));
    if n == 0 {
        0.0
    } else {
        sum as f64 / n as f64 / 1e6
    }
}

/// Little's law under an in-flight cap: `rate × service` is at most `c` (with 35 % of slack
/// for the estimates). The bound is one-sided. Only a client that keeps the cap full admits
/// `c / S`; one that backs off after a 503 (OpenDAL's default retry sleeps a second) leaves it
/// mostly empty and admits a fraction of that, so a rate under `c / S` says nothing against
/// the cap. 9.3 asked for ±35 % and so never recognised a bucket that restated a cap met in
/// bursts: on `lab://parquet` over its limit of 24 (≈ 10 ms of service, 2 350 requests/s at
/// the cap) the buckets fitted 550 to 1 700 requests/s, and one with a change point
/// out-fitted the clean step on a slow CI machine. A rate above `c / S` is still what tells a
/// bucket from a cap, here and in [`peak_served_ratio`].
///
/// The fitted burst is no evidence either way and is not consulted: the grid ties it to the
/// rate (2 to 30 % of it), so "a burst of at most twice the step" was a test of the rate under
/// another name (the same lab on macOS CI: 54 against a step of 24, and the cap was refused).
fn little_matches(rate: f64, service_s: f64, c: f64) -> bool {
    let l = rate * service_s;
    c > 0.0 && l > 0.0 && l / c <= 1.35
}

/// Design v5 §4: in a saturated closed loop an in-flight cap `c` with service time `S` admits
/// requests exactly as slots free — ≈ c/S per second, in bursts of ≈ c — which is what a token
/// bucket of that rate and burst replays. A bucket that only restates the in-flight step adds
/// no mechanism; it often fits a little better because arrival times are measured more
/// precisely than the client's in-flight counts (coarse scheduling on Windows blurs the end of
/// a request, so the step softens while arrivals stay sharp). Such a rate fit (`RateLimit`, or
/// `TimeVarying` whose every segment obeys Little's law with its own service time) is evidence
/// for the in-flight limit: the in-flight hypothesis takes its likelihood and, with fewer
/// parameters, is selected. Returns the best such fit's log-likelihood and recall. A genuine
/// bucket keeps its lead: throttling happens well below the step, its rate is more than the
/// cap carries, or the service served bursts at a request rate no cap of `c` carries
/// ([`peak_served_ratio`]: with OpenDAL's default retry the workers sleep 1 s after a 503 and
/// wake in step, so a bucket of rate `r` throttles only above ≈ r·S in flight and its fitted
/// burst sits on the grid's floor — the request rate is what tells it from a cap).
fn rate_restatement(obs: &[Obs], fits: &mut [Fit], step: Step) -> Option<(f64, f64)> {
    let (c, step_ll, lo_share, hi_share) = step;
    let cf = f64::from(c);
    // A clean step: (almost) no throttling at or below the ceiling, much more above it.
    if c < 2 || lo_share > 0.03 || hi_share < 5.0 * lo_share.max(0.002) {
        return None;
    }
    let s_all = served_service_s(obs);
    let t0 = obs.first().map_or(0, |o| o.t);
    let mut best: Option<(f64, f64)> = None;
    for f in fits.iter_mut() {
        let equivalent = match f.kind {
            HypothesisKind::RateLimit => {
                true && f
                    .params
                    .requests_per_s
                    .is_some_and(|r| little_matches(r, s_all, cf))
            }
            HypothesisKind::TimeVarying => {
                let cp = f.params.change_points_s.first().copied().unwrap_or(0.0);
                let cut = t0 + (cp * 1e6) as i64;
                let (a, b): (Vec<Obs>, Vec<Obs>) = obs.iter().partition(|o| o.t < cut);
                let segs = &f.params.segment_requests_per_s;
                true && segs.len() == 2
                    && little_matches(segs[0], served_service_s(&a), cf)
                    && little_matches(segs[1], served_service_s(&b), cf)
            }
            _ => false,
        };
        if equivalent && f.ll > step_ll {
            if best.is_none_or(|b| f.ll > b.0) {
                best = Some((f.ll, f.explains));
            }
            f.evidence.push(format!(
                "indistinguishable from the in-flight limit of {c}: holding {c} requests in flight at ≈{:.1} ms of service admits up to ≈{:.0} requests/s in bursts of ≈{c}, and this bucket asks for no more",
                s_all * 1e3,
                cf / s_all.max(1e-9)
            ));
        }
    }
    best
}

/// The throttle-family fits, and the confidence cap of a selected in-flight limit that
/// absorbed an equivalent rate fit.
fn throttle_family(
    obs: &[Obs],
    keys: &[String],
    prefixes: &[String],
) -> (Vec<Fit>, Option<Confidence>) {
    let mut step: Option<Step> = None;
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
    // In-flight: a step at `c`, tolerant of ±1 level of client-side measurement error.
    if let Some((c, s)) = best_threshold(obs.iter().map(|o| (o.level, o.thr))) {
        let lv: Vec<(u64, u64, u64)> = (1..=c + 64)
            .map(|l| {
                let (n, t) = obs
                    .iter()
                    .filter(|o| o.level == l)
                    .fold((0u64, 0u64), |a, o| (a.0 + 1, a.1 + u64::from(o.thr)));
                (u64::from(l), n - t, t)
            })
            .collect();
        let c2 = super::capacity::nudge_past_clean_levels(&lv, u64::from(c)) as u32;
        let s = if c2 == c {
            s
        } else {
            let mut sp = Split::default();
            for o in obs {
                if o.level > c2 {
                    sp.n1 += 1;
                    sp.k1 += u64::from(o.thr);
                } else {
                    sp.n0 += 1;
                    sp.k0 += u64::from(o.thr);
                }
            }
            sp
        };
        let c = c2;
        let lo_share = s.k0 as f64 / s.n0.max(1) as f64;
        let hi_share = s.k1 as f64 / s.n1.max(1) as f64;
        step = Some((c, s.ll(), lo_share, hi_share));
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
            let base = served_rate_median(seg).max(r_all * 0.05);
            log_grid(base * 0.85, base * 1.15, 9)
                .into_iter()
                .filter_map(|r| {
                    let sc = bucket_scores(seg, r, b_all, None, 0);
                    best_threshold(sc.iter().zip(seg).map(|(v, o)| (*v, o.thr)))
                        .map(|(_, s)| (r, s))
                })
                // The first (lowest) rate of equally likely ones: `max_by` keeps the last, so a
                // flat likelihood put the lower limit at the top of the grid (+15 %), and the
                // pacing advice derived from it at the limit itself.
                .fold(None::<(f64, Split)>, |best, x| match best {
                    Some(b) if b.1.ll() >= x.1.ll() => Some(b),
                    _ => Some(x),
                })
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
    // Request-rate evidence (design 6.3 A1): bursts of served requests no cap of `c` carries
    // refute the in-flight limit, whatever its step fits: it explains no more than constant
    // throttling, and no bucket restates it.
    let refuted = step.and_then(|(c, ..)| {
        let s = served_service_s(obs);
        let (peak, window_us) = peak_served_ratio(obs, f64::from(c), s);
        (peak >= CAP_RATE_RATIO).then_some((c, peak, window_us, s))
    });
    if let Some((c, peak, window_us, s)) = refuted {
        let why = format!(
            "within {:.0} ms the service served {peak:.1}× the requests {c} in flight carry at ≈{:.1} ms each",
            window_us as f64 / 1e3,
            s * 1e3
        );
        for f in fits.iter_mut() {
            match f.kind {
                HypothesisKind::InflightLimit => {
                    f.ll = bern_ll(k, n);
                    f.explains = 0.0;
                    f.evidence.push(format!(
                        "refuted by the request rate: {why}; a cap never does"
                    ));
                }
                HypothesisKind::RateLimit | HypothesisKind::TimeVarying => f.evidence.push(
                    format!("not an in-flight limit of {c}: {why} (a bucket's burst)"),
                ),
                _ => {}
            }
        }
    }
    let restated = step
        .filter(|_| refuted.is_none())
        .and_then(|st| rate_restatement(obs, &mut fits, st));
    if let Some((ll, explains)) = restated
        && let Some(f) = fits
            .iter_mut()
            .find(|f| f.kind == HypothesisKind::InflightLimit)
    {
        f.ll = f.ll.max(ll);
        f.explains = f.explains.max(explains);
    }
    (fits, restated.map(|_| Confidence::Medium))
}

/// Gaussian log-likelihood at the MLE variance of residuals with sum of squares `sse`.
fn gauss_ll(sse: f64, n: f64) -> f64 {
    if n < 1.0 {
        return 0.0;
    }
    let var = (sse / n).max(1e-12);
    -0.5 * n * ((2.0 * std::f64::consts::PI * var).ln() + 1.0)
}

/// The requests in flight must at least double between the quietest and the busiest
/// well-populated level for one run to assert queueing: a closed loop that keeps its workers
/// busy varies no load, and a curve over 14 to 16 requests in flight is not a limit (N2).
const QUEUEING_LOAD_SPAN: u32 = 2;

/// A level of requests in flight is populated from this many answered requests.
const POPULATED: f64 = 10.0;

/// The time a queue of `servers` adds at `level` requests in flight, as a multiple of the
/// service time: `1 + max(0, level − servers) / servers`.
fn queue_factor(level: u32, servers: u32) -> f64 {
    1.0 + level.saturating_sub(servers) as f64 / servers.max(1) as f64
}

/// Sufficient statistics of `ln(ttfb ms)` in one cell: count, sum, sum of squares.
type LnStats = (f64, f64, f64);

/// Queueing vs constant latency over the time to first byte by in-flight level, and whether
/// the run varied its load enough to assert it ([`QUEUEING_LOAD_SPAN`]).
///
/// Every storage API has its own service time (K2-7): the queue is one — the level counts
/// every request in flight — but a HEAD answers faster than a ranged GET at any level. 9.2
/// fitted one service time to all of them, so a workload whose quick requests run at a low
/// level and whose slow ones at a high level (a parquet scan: a HEAD, a footer, then the
/// column ranges side by side) read as latency that grows with load. Both models now carry
/// one intercept per API, and the growth is what is left within an API.
fn queueing_fits(obs: &[Obs]) -> Option<(Fit, Fit, bool)> {
    // Per (API, level) sufficient statistics of ln(ttfb ms).
    let mut cells: HashMap<(u32, u32), LnStats> = HashMap::new();
    for o in obs.iter().filter(|o| o.ok && !o.thr && o.ttfb_us > 0) {
        let y = (o.ttfb_us as f64 / 1000.0).ln();
        let e = cells.entry((o.api, o.level)).or_default();
        e.0 += 1.0;
        e.1 += y;
        e.2 += y * y;
    }
    // In a fixed order: the sums are the same on every run (U9).
    let mut cells: Vec<((u32, u32), LnStats)> = cells.into_iter().collect();
    cells.sort_by_key(|x| x.0);
    let mut apis: Vec<u32> = cells.iter().map(|x| x.0.0).collect();
    apis.dedup();
    let n: f64 = cells.iter().map(|(_, s)| s.0).sum();
    // Per level over every API: how many requests, and their mean distance (in ln) from
    // their own API's mean — the growth that is not a difference between APIs.
    let api_mean: HashMap<u32, f64> = apis
        .iter()
        .map(|a| {
            let (cnt, sum) = cells
                .iter()
                .filter(|x| x.0.0 == *a)
                .fold((0.0, 0.0), |acc, (_, s)| (acc.0 + s.0, acc.1 + s.1));
            (*a, sum / cnt)
        })
        .collect();
    let mut levels: Vec<(u32, f64, f64)> = Vec::new();
    for ((a, l), s) in &cells {
        let dev = s.1 - s.0 * api_mean[a];
        match levels.iter_mut().find(|x| x.0 == *l) {
            Some(x) => {
                x.1 += s.0;
                x.2 += dev;
            }
            None => levels.push((*l, s.0, dev)),
        }
    }
    levels.sort_by_key(|x| x.0);
    let populated = levels.iter().filter(|x| x.1 >= POPULATED).count();
    if populated < 3 || n < 50.0 {
        return None;
    }
    let sy: f64 = cells.iter().map(|(_, s)| s.1).sum();
    let typical_ms = (sy / n).exp();
    let null_sse: f64 = cells
        .iter()
        .map(|((a, _), s)| {
            let m = api_mean[a];
            s.2 - 2.0 * m * s.1 + s.0 * m * m
        })
        .sum();
    let intercepts = apis.len() as f64;
    let null = Fit {
        kind: HypothesisKind::None,
        params: HypothesisParams {
            service_ms: Some(typical_ms),
            ..Default::default()
        },
        ll: gauss_ll(null_sse, n),
        k: intercepts + 1.0,
        explains: 0.0,
        evidence: vec![format!(
            "time to first byte stays ≈{typical_ms:.1} ms whatever the requests in flight"
        )],
    };
    let max_level = levels.last().map_or(1, |x| x.0);
    // (servers, ln of the typical service time, sum of squares)
    let mut best: Option<(u32, f64, f64)> = None;
    for c in 1..max_level {
        let g = |l: u32| queue_factor(l, c).ln();
        let (mut sse, mut shifts) = (0.0, 0.0);
        for a in &apis {
            let of_api = || cells.iter().filter(|x| x.0.0 == *a);
            let cnt: f64 = of_api().map(|(_, s)| s.0).sum();
            // Best ln s of this API: mean of y − g(L).
            let shift: f64 = of_api().map(|((_, l), s)| s.1 - s.0 * g(*l)).sum::<f64>() / cnt;
            sse += of_api()
                .map(|((_, l), s)| {
                    let m = shift + g(*l);
                    s.2 - 2.0 * m * s.1 + s.0 * m * m
                })
                .sum::<f64>();
            shifts += shift * cnt;
        }
        if best.as_ref().is_none_or(|b| sse < b.2) {
            best = Some((c, shifts / n, sse));
        }
    }
    let (c, ln_s, sse) = best?;
    let s_ms = ln_s.exp();
    // Effect size: at the busiest well-populated level latency must be ≥ 25 % above service.
    let busiest = levels
        .iter()
        .rev()
        .find(|x| x.1 >= POPULATED)
        .map_or(c, |x| x.0);
    let factor = queue_factor(busiest, c);
    // The typical request's time to first byte at a level.
    let mean_at = |l: u32| {
        levels
            .iter()
            .find(|x| x.0 == l)
            .map_or(0.0, |x| typical_ms * (x.2 / x.1).exp())
    };
    let lowest = levels.iter().find(|x| x.1 >= POPULATED).map_or(c, |x| x.0);
    let observed_growth = mean_at(busiest) / mean_at(lowest).max(1e-9);
    let mut q = Fit {
        kind: HypothesisKind::Queueing,
        params: HypothesisParams {
            service_ms: Some(s_ms),
            servers_count: Some(u64::from(c)),
            ..Default::default()
        },
        ll: gauss_ll(sse, n),
        k: intercepts + 2.0,
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
    let varied = busiest >= QUEUEING_LOAD_SPAN * lowest;
    if !varied {
        q.evidence.push(format!(
            "the requests in flight stayed between {lowest} and {busiest}: one run that varies no load cannot assert it (vary it with `--set shape=ramp:FROM..TO/D`)"
        ));
    }
    Some((q, null, varied))
}

/// What a fitted queue of `servers` costs the requests of a window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueueWait {
    /// The busiest populated level of requests in flight.
    pub busiest_inflight_count: u32,
    /// The mean wait in the queue of a request that arrived at that level (ms).
    pub busiest_wait_ms: f64,
    /// The wait in the queue of every answered request together (µs).
    pub wait_us: f64,
}

/// The wait the queueing model of `servers` attributes to the requests that started in
/// `[from_us, to_us)`: of a request that arrived at `L` in flight, the share `1 − 1/factor(L)`
/// of its own time to first byte. It is the model's wait, request by request: unlike "time
/// to first byte beyond the service time", noise around the service time adds nothing to it.
pub fn queue_wait(reqs: &[ReqRow], window: (i64, i64), servers: u64) -> Option<QueueWait> {
    let servers = u32::try_from(servers).ok().filter(|s| *s > 0)?;
    let rows: Vec<&ReqRow> = reqs
        .iter()
        .filter(|q| q.t_us >= window.0 && q.t_us < window.1)
        .collect();
    let (obs, _, _) = observations(&rows, &[]);
    let answered = || obs.iter().filter(|o| o.ok && !o.thr && o.ttfb_us > 0);
    let wait = |o: &Obs| o.ttfb_us as f64 * (1.0 - 1.0 / queue_factor(o.level, servers));
    let mut per_level: HashMap<u32, (f64, f64)> = HashMap::new();
    for o in answered() {
        let e = per_level.entry(o.level).or_default();
        e.0 += 1.0;
        e.1 += wait(o);
    }
    let (busiest, (count, sum)) = per_level
        .into_iter()
        .filter(|(_, s)| s.0 >= POPULATED)
        .max_by_key(|(l, _)| *l)?;
    Some(QueueWait {
        busiest_inflight_count: busiest,
        busiest_wait_ms: sum / count / 1000.0,
        wait_us: answered().map(wait).sum(),
    })
}

/// How the time to first byte moved as the requests in flight rose, over the requests of a
/// window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TtfbGrowth {
    /// Slope of `ln ttfb` on `ln level`, within each API: 0 for a service that answers as
    /// fast whatever is in flight, 1 for one where every further request only waits.
    pub elasticity: f64,
    /// Standard error of the slope.
    pub stderr: f64,
    /// The busiest populated level, and the typical time to first byte there (ms).
    pub busy_inflight_count: u32,
    pub busy_ttfb_ms: f64,
    /// The typical time to first byte at half that level or less (ms).
    pub quiet_ttfb_ms: f64,
}

impl TtfbGrowth {
    /// The time to first byte stayed put as the requests in flight at least doubled, beyond
    /// doubt (less than a quarter of proportional growth, two standard errors included): the
    /// service had room for all of them.
    pub fn flat(&self) -> bool {
        self.elasticity + 2.0 * self.stderr < 0.25
    }
}

/// [`TtfbGrowth`] of the requests that started in `[from_us, to_us)`; `None` when the run
/// did not send enough of them at half its busiest level or less.
///
/// "Enough" is twice the busiest level. The rows of a window do not show the requests still
/// in flight from before it, so its first requests only seem to arrive at few in flight —
/// at most one per level below the busiest. They carry the busy level's time to first byte
/// and would make any service look as if it had room; among twice as many real ones they
/// cannot.
pub fn ttfb_growth(reqs: &[ReqRow], window: (i64, i64)) -> Option<TtfbGrowth> {
    let rows: Vec<&ReqRow> = reqs
        .iter()
        .filter(|q| q.t_us >= window.0 && q.t_us < window.1)
        .collect();
    let (obs, _, _) = observations(&rows, &[]);
    // (API, level, ln level, ln ttfb ms)
    let pts: Vec<(u32, u32, f64, f64)> = obs
        .iter()
        .filter(|o| o.ok && !o.thr && o.ttfb_us > 0)
        .map(|o| {
            (
                o.api,
                o.level,
                f64::from(o.level).ln(),
                (o.ttfb_us as f64 / 1000.0).ln(),
            )
        })
        .collect();
    let mut per_level: HashMap<u32, f64> = HashMap::new();
    for p in &pts {
        *per_level.entry(p.1).or_default() += 1.0;
    }
    let busy = per_level
        .iter()
        .filter(|(_, n)| **n >= POPULATED)
        .map(|(l, _)| *l)
        .max()?;
    let geo_mean = |keep: &dyn Fn(u32) -> bool, at_least: usize| {
        let v: Vec<f64> = pts.iter().filter(|p| keep(p.1)).map(|p| p.3).collect();
        (v.len() >= at_least).then(|| (v.iter().sum::<f64>() / v.len() as f64).exp())
    };
    let quiet_ttfb_ms = geo_mean(&|l| l * 2 <= busy, 2 * busy as usize)?;
    let busy_ttfb_ms = geo_mean(&|l| l == busy, 1)?;
    // Within-API least squares: both axes centred on their API's mean.
    let mut means: HashMap<u32, (f64, f64, f64)> = HashMap::new();
    for p in &pts {
        let e = means.entry(p.0).or_default();
        e.0 += 1.0;
        e.1 += p.2;
        e.2 += p.3;
    }
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for p in &pts {
        let m = means[&p.0];
        let (x, y) = (p.2 - m.1 / m.0, p.3 - m.2 / m.0);
        sxx += x * x;
        sxy += x * y;
        syy += y * y;
    }
    let dof = pts.len() as f64 - means.len() as f64 - 1.0;
    if sxx <= 0.0 || dof < 30.0 {
        return None;
    }
    let elasticity = sxy / sxx;
    let residual = (syy - elasticity * sxy).max(0.0);
    Some(TtfbGrowth {
        elasticity,
        stderr: (residual / dof / sxx).sqrt(),
        busy_inflight_count: busy,
        busy_ttfb_ms,
        quiet_ttfb_ms,
    })
}

/// When `o` moved its body in direction `dir` (µs), and how many bytes.
fn stream(dir: Direction, o: &Obs) -> ((i64, i64), u64) {
    match dir {
        Direction::Download => (o.body, o.bytes_down),
        Direction::Upload => (o.upload, o.bytes_up),
    }
}

/// Aggregate vs per-stream bandwidth over the body rates of large transfers in one
/// direction: responses, or — by the same standards — request bodies (a saturated uplink
/// slows uploads as a shared cap slows downloads).
fn bandwidth_fits(obs: &[Obs], dir: Direction) -> Option<(Fit, Fit, f64)> {
    let streams: Vec<(bool, (i64, i64), u64)> = obs
        .iter()
        .map(|o| {
            let (span, bytes) = stream(dir, o);
            (o.ok, span, bytes)
        })
        .collect();
    // Concurrent bodies over time.
    let mut ev: Vec<(i64, i32)> = streams
        .iter()
        .filter(|(_, span, bytes)| *bytes > 0 && span.1 > span.0)
        .flat_map(|(_, span, _)| [(span.0, 1), (span.1, -1)])
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
    for (_, span, bytes) in streams
        .iter()
        .filter(|(ok, span, bytes)| *ok && *bytes >= 128 * 1024 && span.1 - span.0 >= 1000)
    {
        let d = (span.1 - span.0) as f64;
        let nbar = ((area_at(span.1) - area_at(span.0)) / d).max(1.0);
        xs.push(nbar.ln());
        ys.push((*bytes as f64 / (d / 1e6)).ln());
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
            "each {}stream moves ≈{:.1} MiB/s however many run at once (slope {slope:+.2} of ln rate on ln streams): more streams add throughput",
            if dir == Direction::Upload {
                "upload "
            } else {
                ""
            },
            mib(my.exp())
        )],
    };
    let agg = Fit {
        kind: HypothesisKind::BandwidthCap,
        params: HypothesisParams {
            bytes_per_s: Some(agg_mean.exp()),
            direction: Some(dir),
            ..Default::default()
        },
        ll: gauss_ll(agg_sse, n),
        k: 2.0,
        explains: (-slope).clamp(0.0, 1.0),
        evidence: vec![format!(
            "{} ≈{:.1} MiB/s: a stream's rate falls as 1/n with the streams running beside it (slope {slope:+.2} of ln rate on ln streams, −1 = a shared cap)",
            match dir {
                Direction::Download => "the streams share",
                Direction::Upload => "the upload streams share",
            },
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
    // T8: "high" only when the model explains most of the effect.
    let confidence = if f.kind != HypothesisKind::None
        && f.explains < crate::analysis::findings::HIGH_EXPLAINS_RATIO
    {
        confidence.min(Confidence::Medium)
    } else {
        confidence
    };
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
    let client_bound = cpu >= CLIENT_BOUND_RATIO || inputs.client_saturated;
    let client = || Hypothesis {
        kind: HypothesisKind::ClientBound,
        params: HypothesisParams::default(),
        log_likelihood: None,
        bic: None,
        evidence: vec![if cpu >= CLIENT_BOUND_RATIO {
            format!(
                "the load generator kept {:.0}% of its CPU busy: it, not the service, set the pace",
                cpu * 100.0
            )
        } else {
            "the load generator's scheduler lagged: its timing, not the service, set the pace"
                .to_owned()
        }],
        confidence: if cpu >= 0.95 {
            Confidence::High
        } else {
            Confidence::Medium
        },
        explains_ratio: 1.0,
        selected: true,
    };
    if thr >= MIN_THROTTLED {
        let (fits, cap) = throttle_family(&obs, &keys, &prefixes);
        let mut hs = rank(fits, n, thr >= 20);
        if let (Some(cap), Some(h)) = (cap, hs.first_mut())
            && h.kind == HypothesisKind::InflightLimit
        {
            h.confidence = h.confidence.min(cap);
            h.evidence.push(
                "a token bucket restating this limit fits about as well (one closed-loop run cannot tell them apart; `oprobe prove` varies the limit)"
                    .into(),
            );
        }
        return hs;
    }
    // Latency family.
    let mut out: Vec<Hypothesis> = Vec::new();
    let q = queueing_fits(&obs);
    let gain = |a: &Fit, b: &Fit| b.bic(n) - a.bic(n);
    // The direction whose bodies a shared cap explains better (9.1 fitted downloads only, so
    // a saturated uplink was left to read as hung requests).
    let bw = [Direction::Download, Direction::Upload]
        .into_iter()
        .filter_map(|d| bandwidth_fits(&obs, d))
        .max_by(|x, y| gain(&x.0, &x.1).total_cmp(&gain(&y.0, &y.1)));
    let q_gain = q
        .as_ref()
        .map_or(f64::NEG_INFINITY, |(q, nl, _)| gain(q, nl));
    let bw_gain = bw
        .as_ref()
        .map_or(f64::NEG_INFINITY, |(a, p, _)| gain(a, p));
    let mut pick: Option<Vec<Hypothesis>> = None;
    if bw_gain > 0.0 && bw_gain >= q_gain {
        let (a, p, _) = bw.clone().expect("gain implies a fit");
        pick = Some(rank(vec![a, p], n, true));
    } else if q_gain > 0.0 {
        let (qf, nl, varied) = q.clone().expect("gain implies a fit");
        let mut hs = rank(vec![qf, nl], n, true);
        if !varied {
            hs[0].confidence = Confidence::Low;
        }
        pick = Some(hs);
    }
    match pick {
        Some(mut hs) => {
            // The other family's fit as an unselected alternative.
            if hs[0].kind == HypothesisKind::BandwidthCap
                && let Some((qf, _, _)) = q
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
            for (alt, _, _) in q.into_iter() {
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
pub(crate) mod tests {
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

    /// A closed loop of `workers` against a server whose `admit(arrival_ms, in_flight)` decides
    /// between serving (≈ `svc_ms`) and a fast 503 (retried after ≈ `backoff_ms`). The client
    /// sees each response up to `late_ms` late (coarse scheduling on Windows) and only then
    /// sends its next request, so its in-flight count at an arrival includes requests the
    /// server already finished: the in-flight step softens while arrival times stay exact.
    fn closed_loop(
        workers: usize,
        svc_ms: f64,
        late_ms: f64,
        backoff_ms: f64,
        dur_ms: f64,
        mut admit: impl FnMut(f64, usize) -> bool,
    ) -> (Vec<ReqRow>, Vec<OpRow>) {
        let mut x = 0x9e3779b97f4a7c15u64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut next: Vec<f64> = (0..workers).map(|w| w as f64 * 0.05).collect();
        let mut busy: Vec<f64> = Vec::new();
        let (mut reqs, mut ops) = (Vec::new(), Vec::new());
        let mut id = 0u64;
        loop {
            let (w, a) = next
                .iter()
                .copied()
                .enumerate()
                .min_by(|p, q| p.1.total_cmp(&q.1))
                .unwrap();
            if a >= dur_ms {
                break;
            }
            busy.retain(|e| *e > a);
            let thr = !admit(a, busy.len());
            let end = if thr {
                a + 0.3
            } else {
                let e = a + svc_ms * (0.6 + 0.8 * rnd());
                busy.push(e);
                e
            };
            let seen = end + rnd() * late_ms;
            id += 1;
            reqs.push(req(a, seen - a, if thr { 503 } else { 200 }, id));
            let key = (rnd() * 500.0) as u64;
            ops.push(op(id, &format!("p{}/k{key}", key % 5), a));
            let backoff = if thr { backoff_ms * (0.5 + rnd()) } else { 0.0 };
            next[w] = seen + 0.05 + rnd() * 0.2 + backoff;
        }
        (reqs, ops)
    }

    /// Windows CI (lab://demo, limit 24): the client's in-flight counts blur while arrivals stay
    /// sharp, and a token bucket of ≈ c/S per second with a burst of ≈ c fitted better than the
    /// step. The bucket restates the in-flight cap (Little's law), so the cap is selected.
    #[test]
    fn a_bucket_that_restates_an_inflight_cap_selects_the_cap() {
        for (late, backoff) in [(3.0, 40.0), (3.0, 100.0), (2.0, 100.0)] {
            let (reqs, ops) = closed_loop(32, 5.0, late, backoff, 3000.0, |_, n| n < 24);
            let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
            let s = selected(&hs);
            assert_eq!(
                s.kind,
                HypothesisKind::InflightLimit,
                "late {late}: {hs:#?}"
            );
            // Client-side levels read high: the step lies at or a few levels above 24.
            let c = s.params.ceiling_inflight_count.unwrap();
            assert!((22..=32).contains(&c), "late {late}: {c}");
            assert!(s.confidence >= Confidence::Medium, "late {late}: {s:#?}");
            let rate = hs
                .iter()
                .find(|h| h.kind == HypothesisKind::RateLimit)
                .expect("the bucket stays listed");
            assert!(
                rate.evidence
                    .iter()
                    .any(|e| e.contains("indistinguishable")),
                "{rate:#?}"
            );
        }
    }

    /// The fits of a release-CI run of `lab://parquet` with 24 workers over the lab's limit of
    /// 24 in flight: a clean step (nothing throttled at or below 24), and a bucket whose rate
    /// "changed" from 1 131 to 595 requests/s that fitted the bursts better. Both rates are far
    /// under the 2 400 requests/s the cap carries at 10 ms of service, because the workers
    /// sleep a second after a 503: the bucket restates the cap.
    #[test]
    fn a_bucket_slower_than_an_unsaturated_cap_restates_it() {
        let (reqs, ops) = closed_loop(48, 10.0, 1.0, 1000.0, 2300.0, |_, n| n < 24);
        let (r, o): (Vec<&ReqRow>, Vec<&OpRow>) = (reqs.iter().collect(), ops.iter().collect());
        let (obs, _, _) = observations(&r, &o);
        let bucket = |segs: Vec<f64>, burst: u64| Fit {
            kind: HypothesisKind::TimeVarying,
            params: HypothesisParams {
                requests_per_s: segs.iter().copied().reduce(f64::min),
                burst_count: Some(burst),
                change_points_s: vec![0.93],
                segment_requests_per_s: segs,
                ..Default::default()
            },
            ll: -53.7,
            k: 8.0,
            explains: 1.0,
            evidence: Vec::new(),
        };
        let step: Step = (24, -83.3, 0.0, 0.777);
        let mut fits = vec![bucket(vec![1131.0, 595.0], 48)];
        assert_eq!(rate_restatement(&obs, &mut fits, step), Some((-53.7, 1.0)));
        assert!(fits[0].evidence[0].contains("indistinguishable"));
        // macOS CI, the same lab: a burst of 54, more than twice the step.
        let mut fits = vec![bucket(vec![829.8, 1062.6], 54)];
        assert!(rate_restatement(&obs, &mut fits, step).is_some());
        // A rate no cap of 24 carries at this service time is a bucket's own.
        let mut fits = vec![bucket(vec![4000.0, 3600.0], 48)];
        assert_eq!(rate_restatement(&obs, &mut fits, step), None);
    }

    /// The same blurred client against a genuine bucket (300/s, burst 15): still a rate limit.
    #[test]
    fn a_genuine_bucket_under_the_same_blur_stays_a_rate_limit() {
        let (mut tokens, mut last) = (15.0f64, 0.0f64);
        let (reqs, ops) = closed_loop(16, 5.0, 3.0, 40.0, 3000.0, |t, _| {
            tokens = (tokens + (t - last) / 1000.0 * 300.0).min(15.0);
            last = t;
            if tokens >= 1.0 {
                tokens -= 1.0;
                true
            } else {
                false
            }
        });
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::RateLimit, "{hs:#?}");
        let rps = s.params.requests_per_s.unwrap();
        assert!((rps - 300.0).abs() < 45.0, "{rps}");
    }

    /// A genuine bucket of a low rate and a large burst (50/s, burst 200: a quota that allows
    /// four seconds' spike) is a rate limit of its own rate, under a quick-retrying client and
    /// under OpenDAL's default one-second retry alike. 9.3.2 searched bursts up to 0.3 s of
    /// the rate and rates from 0.85 of what was served: the bucket that could not hold the
    /// burst took a higher rate instead, 64/s and 66/s here (over 64 buckets of 20 to 200/s
    /// with bursts of 2.5 to 5 s, 8 to 64 workers: a median error of 11.6 %, 23 within 10 %
    /// and one not found; now 4.5 %, 54 within 10 %, all found).
    #[test]
    fn a_low_rate_large_burst_bucket_is_a_rate_limit_of_its_rate() {
        for (late, backoff) in [(0.3, 40.0), (1.0, 1000.0)] {
            let (mut tokens, mut last) = (200.0f64, 0.0f64);
            let (reqs, ops) = closed_loop(16, 5.0, late, backoff, 8000.0, |t, _| {
                tokens = (tokens + (t - last) / 1000.0 * 50.0).min(200.0);
                last = t;
                if tokens >= 1.0 {
                    tokens -= 1.0;
                    true
                } else {
                    false
                }
            });
            let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
            let s = selected(&hs);
            assert!(
                matches!(
                    s.kind,
                    HypothesisKind::RateLimit | HypothesisKind::TimeVarying
                ),
                "late {late}, backoff {backoff}: {hs:#?}"
            );
            let rps = s.params.requests_per_s.unwrap();
            assert!((rps - 50.0).abs() < 7.5, "late {late}: {rps}");
        }
    }

    /// Request-rate evidence: a cap never serves much more than `c / S`, however blurred the
    /// client's in-flight counts; a bucket that refilled during a lull serves its burst at once.
    #[test]
    fn served_bursts_tell_a_bucket_from_a_cap() {
        let peak = |reqs: &[ReqRow], ops: &[OpRow], c: f64| {
            let r: Vec<&ReqRow> = reqs.iter().collect();
            let o: Vec<&OpRow> = ops.iter().collect();
            let (obs, _, _) = observations(&r, &o);
            peak_served_ratio(&obs, c, served_service_s(&obs)).0
        };
        for (c, late) in [(24usize, 3.0), (24, 0.3), (8, 1.0), (2, 0.3)] {
            let (reqs, ops) = closed_loop(32, 5.0, late, 100.0, 3000.0, |_, n| n < c);
            let p = peak(&reqs, &ops, c as f64);
            assert!(p < CAP_RATE_RATIO, "cap {c}, late {late}: {p}");
        }
        // Two requests in flight for 1 s (5 ms each), then a refilled burst of 20 at once.
        let (mut reqs, mut ops) = (Vec::new(), Vec::new());
        let mut id = 0u64;
        let mut push = |t: f64| {
            id += 1;
            reqs.push(req(t, 5.0, 200, id));
            ops.push(op(id, "k", t));
        };
        for i in 0..400 {
            push(f64::from(i) * 2.5);
        }
        for i in 0..20 {
            push(1000.0 + f64::from(i) * 0.05);
        }
        let p = peak(&reqs, &ops, 2.0);
        assert!(p >= 2.0, "{p}");
    }

    /// `lab://ratelimit` with OpenDAL's default retry (1 s backoff, no jitter): 16 workers
    /// against a bucket of 300/s (burst 20) spend most of their time asleep, wake in step and
    /// throttle only above a couple of requests in flight, where a cap of ≈ r·S (Little's
    /// law) would also throttle; the bursts the bucket serves after a lull rule the cap out.
    #[test]
    fn a_bucket_under_default_retry_is_a_rate_limit() {
        for (late, seed_shift) in [(0.3, 0.0), (1.0, 0.01), (3.0, 0.02)] {
            let (mut tokens, mut last) = (20.0f64, 0.0f64);
            let (reqs, ops) = closed_loop(16, 5.0 + seed_shift, late, 1000.0, 3000.0, |t, _| {
                tokens = (tokens + (t - last) / 1000.0 * 300.0).min(20.0);
                last = t;
                if tokens >= 1.0 {
                    tokens -= 1.0;
                    true
                } else {
                    false
                }
            });
            let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
            let s = selected(&hs);
            assert!(
                matches!(
                    s.kind,
                    HypothesisKind::RateLimit | HypothesisKind::TimeVarying
                ),
                "late {late}: {hs:#?}"
            );
        }
    }

    /// The default-retry client against a genuine cap of 2 in flight stays an in-flight
    /// limit: a cap serves no bursts.
    #[test]
    fn a_small_cap_under_default_retry_is_an_inflight_limit() {
        for late in [0.3, 3.0] {
            let (reqs, ops) = closed_loop(16, 5.0, late, 1000.0, 3000.0, |_, n| n < 2);
            let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
            let s = selected(&hs);
            assert_eq!(
                s.kind,
                HypothesisKind::InflightLimit,
                "late {late}: {hs:#?}"
            );
        }
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

    /// Bursts of 1 to 8 transfers of 1 MiB over a link that carries 8 MiB/s in all: each
    /// stream gets the cap divided by the streams beside it. `upload` puts the bytes in the
    /// request body (the send stage), else in the response body.
    pub(crate) fn capped_link(upload: bool) -> (Vec<ReqRow>, Vec<OpRow>) {
        let (mut reqs, mut ops) = (Vec::new(), Vec::new());
        let (mib, cap) = (1u64 << 20, 8.0 * (1u64 << 20) as f64);
        let mut id = 0;
        let mut t0 = 0i64;
        for burst in 0..60 {
            let streams = 1 + burst % 8;
            let move_us = (mib as f64 / (cap / streams as f64) * 1e6) as u64;
            for j in 0..streams {
                id += 1;
                let mut r = crate::analysis::outliers::test_req(t0 + j as i64, 0);
                r.op_id = id;
                (r.send_us, r.ttfb_us) = (10, 5_000);
                if upload {
                    r.api = crate::trace::Api::PutObject;
                    r.method = std::borrow::Cow::Borrowed("PUT");
                    (r.send_us, r.bytes_up, r.bytes_down) = (move_us, mib, 0);
                } else {
                    (r.body_us, r.bytes_down) = (move_us, mib);
                }
                r.total_us = r.send_us + r.ttfb_us + r.body_us;
                reqs.push(r);
                ops.push(op(id, "k", t0 as f64 / 1000.0));
            }
            t0 += move_us as i64 + 50_000;
        }
        (reqs, ops)
    }

    /// 9.1 fitted the cap on response bodies only: a saturated uplink had no limit model.
    #[test]
    fn a_saturated_uplink_is_a_bandwidth_cap_on_the_uploads() {
        let (reqs, ops) = capped_link(true);
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::BandwidthCap, "{hs:#?}");
        assert_eq!(s.params.direction, Some(Direction::Upload), "{s:#?}");
        assert!(s.confidence >= Confidence::Medium, "{s:#?}");
        let mib_s = s.params.bytes_per_s.unwrap() / (1u64 << 20) as f64;
        assert!((7.0..9.0).contains(&mib_s), "{mib_s}");
        assert!(s.explains_ratio > 0.9, "{s:#?}");
        // The same link on the way down is the cap it always was, and says so.
        let (reqs, ops) = capped_link(false);
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::BandwidthCap, "{hs:#?}");
        assert_eq!(s.params.direction, Some(Direction::Download), "{s:#?}");
        assert!(
            s.evidence[0].starts_with("the streams share"),
            "{:?}",
            s.evidence
        );
        // Uploads that each keep their rate whatever runs beside them are no cap.
        let (mut reqs, ops) = capped_link(true);
        for r in &mut reqs {
            r.send_us = 125_000;
            r.total_us = r.send_us + r.ttfb_us;
        }
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        assert_ne!(selected(&hs).kind, HypothesisKind::BandwidthCap, "{hs:#?}");
    }

    /// Bursts of `widths(burst)` requests, each waiting ≈10 ms of service behind 4 servers:
    /// the time to first byte grows with the requests already in flight.
    pub(crate) fn queueing_bursts(widths: impl Fn(usize) -> usize) -> (Vec<ReqRow>, Vec<OpRow>) {
        queueing_bursts_of(10.0, widths)
    }

    /// [`queueing_bursts`] with a service time of `service_ms`.
    pub(crate) fn queueing_bursts_of(
        service_ms: f64,
        widths: impl Fn(usize) -> usize,
    ) -> (Vec<ReqRow>, Vec<OpRow>) {
        let mut reqs = Vec::new();
        let mut ops = Vec::new();
        let mut id = 0;
        let mut x = 3u64;
        for burst in 0..300 {
            let t0 = burst as f64 * 200.0;
            for j in 0..widths(burst) {
                id += 1;
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let noise = 1.0 + ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.2;
                let lat =
                    service_ms * (1.0 + ((j as u64 + 1).saturating_sub(4)) as f64 / 4.0) * noise;
                reqs.push(req(t0 + j as f64 * 0.01, lat, 200, id));
                ops.push(op(id, "k", t0));
            }
        }
        (reqs, ops)
    }

    /// The same service under closed loops of `phases` workers, 3 s each: every worker sends
    /// its next request when the last one ends, so the requests in flight stay at the workers.
    pub(crate) fn queueing_closed_loops(phases: &[usize]) -> (Vec<ReqRow>, Vec<OpRow>) {
        let (mut reqs, mut ops) = (Vec::new(), Vec::new());
        let mut id = 0;
        let mut x = 7u64;
        for (phase, workers) in phases.iter().enumerate() {
            let t0 = phase as f64 * 3100.0;
            let mut next: Vec<f64> = (0..*workers).map(|w| t0 + w as f64 * 0.01).collect();
            loop {
                let (w, at) = next
                    .iter()
                    .copied()
                    .enumerate()
                    .min_by(|p, q| p.1.total_cmp(&q.1))
                    .unwrap();
                if at >= t0 + 3000.0 {
                    break;
                }
                id += 1;
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let noise = 1.0 + ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.2;
                let lat = 10.0 * (1.0 + (*workers as f64 - 4.0).max(0.0) / 4.0) * noise;
                reqs.push(req(at, lat, 200, id));
                ops.push(op(id, "k", at));
                next[w] = at + lat + 0.02;
            }
        }
        (reqs, ops)
    }

    /// N2: a run that keeps 12 to 19 requests in flight has not varied its load by a factor
    /// of two. Its curve fits queueing best, but one such run asserts nothing (low confidence,
    /// and it says why).
    #[test]
    fn queueing_is_asserted_only_when_the_load_varied() {
        let (reqs, ops) = queueing_bursts(|burst| 2 + burst % 14);
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::Queueing, "{hs:#?}");
        assert!(s.confidence >= Confidence::Medium, "{s:#?}");
        assert_eq!(s.evidence.len(), 1, "{:?}", s.evidence);
        // The same service seen only between 12 and 19 in flight.
        let (reqs, ops) = queueing_closed_loops(&[12, 14, 16, 19]);
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::Queueing, "{hs:#?}");
        assert_eq!(s.confidence, Confidence::Low, "{s:#?}");
        assert!(
            s.evidence.iter().any(|e| e.contains("varies no load")),
            "{s:#?}"
        );
        assert!(crate::analysis::hypothesis::asserted(&hs).is_none());
        // Twice the load between the quietest and the busiest phase is a varied load.
        let (reqs, ops) = queueing_closed_loops(&[8, 12, 16]);
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::Queueing, "{hs:#?}");
        assert!(s.confidence >= Confidence::Medium, "{s:#?}");
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
                client_saturated: false,
            },
        );
        assert_eq!(selected(&hs).kind, HypothesisKind::ClientBound);
    }

    /// A scan-like workload: per burst two HEADs (2 ms each) and then six ranged GETs (20 ms
    /// each) side by side, so the HEADs arrive at 1–2 requests in flight and the GETs at 3–8.
    /// `get_ms(level)` is a GET's time to first byte.
    fn heads_then_gets(get_ms: impl Fn(usize) -> f64) -> (Vec<ReqRow>, Vec<OpRow>) {
        let (mut reqs, mut ops) = (Vec::new(), Vec::new());
        let mut id = 0;
        let mut x = 5u64;
        for burst in 0..300 {
            let t0 = burst as f64 * 100.0;
            for j in 0..8usize {
                id += 1;
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let noise = 1.0 + ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.2;
                let head = j < 2;
                let lat = if head { 2.0 } else { get_ms(j + 1) } * noise;
                let mut r = req(t0 + j as f64 * 0.01, lat, 200, id);
                if head {
                    r.api = crate::trace::Api::HeadObject;
                    r.method = std::borrow::Cow::Borrowed("HEAD");
                }
                reqs.push(r);
                ops.push(op(id, "k", t0));
            }
        }
        (reqs, ops)
    }

    /// K2-7: 9.2 fitted one service time to every API, so quick HEADs at a low level and
    /// slower GETs at a high one read as a time to first byte that grows with the requests in
    /// flight ("2 ms at 1, 20 ms at 8"). Each API has its own service time: nothing grew.
    #[test]
    fn apis_that_differ_in_service_time_are_not_queueing() {
        let (reqs, ops) = heads_then_gets(|_| 20.0);
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::None, "{hs:#?}");
        assert!(
            crate::analysis::hypothesis::asserted(&hs)
                .is_some_and(|h| h.kind == HypothesisKind::None)
        );
        // A queue behind the same mix is still found, within the GETs: four served at once.
        let (reqs, ops) = heads_then_gets(|level| 20.0 * (level as f64 / 4.0).max(1.0));
        let hs = fit(&reqs, &ops, (i64::MIN, i64::MAX), &FitInputs::default());
        let s = selected(&hs);
        assert_eq!(s.kind, HypothesisKind::Queueing, "{hs:#?}");
        assert!(s.confidence >= Confidence::Medium, "{s:#?}");
        let c = s.params.servers_count.unwrap();
        assert!((3..=5).contains(&c), "{c}");
    }
}
