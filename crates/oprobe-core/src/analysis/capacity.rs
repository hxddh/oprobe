//! Capacity model: where the service starts to throttle.
//!
//! [`estimate`] reconstructs the requests in flight over time from the request rows (start and
//! end times), then finds the in-flight level at which the service starts throttling: the
//! lowest level whose arrivals are throttled, bracketed by the highest level whose arrivals are
//! served cleanly ([`Ceiling`]). It also measures how many requests one operation keeps in
//! flight ([`OpShape`]: a parquet scan issues a HEAD, a footer read, then 4 parallel column
//! reads, so it peaks at 4, not at its 6.3 requests per op). The ceiling is never derived from
//! mean latency × rate.
//!
//! Findings report the fitted model as evidence only; sizing a change is `oprobe prove`'s job
//! (a single run varies no load).

use std::collections::{BTreeMap, HashMap};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::hypothesis::{Hypothesis, HypothesisKind};
pub use super::limits::FitInputs;
use crate::model::{OpRow, ReqRow};

/// Serialize a whole-number `f64` (an in-flight level) as an integer.
fn ser_whole<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(if v.is_finite() && *v > 0.0 {
        v.round() as u64
    } else {
        0
    })
}

/// Which rows [`estimate`] reads: requests that started in `[from_us, to_us)` (µs from the
/// measurement window start; warmup rows are negative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub from_us: i64,
    pub to_us: i64,
}

impl Window {
    /// The measurement window of a run (`report.run.window_s`).
    pub fn measured(window_s: f64) -> Self {
        Self {
            from_us: 0,
            to_us: (window_s.max(0.0) * 1e6).ceil() as i64,
        }
    }

    /// Every row, warmup included.
    pub fn all() -> Self {
        Self {
            from_us: i64::MIN,
            to_us: i64::MAX,
        }
    }

    fn contains(&self, t_us: i64) -> bool {
        t_us >= self.from_us && t_us < self.to_us
    }
}

/// The in-flight level the service accepts before it throttles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Ceiling {
    /// In-flight requests the service accepts: the estimate (the likeliest split);
    /// `lo ≤ inflight_count < hi`.
    #[serde(rename = "inflight_count", serialize_with = "ser_whole")]
    #[schemars(with = "u64")]
    pub requests: f64,
    /// Lowest level the limit may lie at: the lowest split about as likely as the best, less
    /// one level of client-side measurement error.
    #[serde(rename = "lo_inflight_count", serialize_with = "ser_whole")]
    #[schemars(with = "u64")]
    pub lo: f64,
    /// First level clearly throttled above every plausible split, plus one level of
    /// measurement error (the limit is in `[lo, hi)`).
    #[serde(rename = "hi_inflight_count", serialize_with = "ser_whole")]
    #[schemars(with = "u64")]
    pub hi: f64,
}

/// Request concurrency of one operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpShape {
    /// Backend requests per operation (mean, retries included).
    #[serde(rename = "requests_per_op_mean")]
    pub requests_per_op: f64,
    /// The most requests one operation had in flight at once (the maximum over operations:
    /// on a busy client the parallel reads of an op often fail to overlap, and a p95 then
    /// under-counts the burst the server sees).
    #[serde(rename = "peak_inflight_per_op_count", serialize_with = "ser_whole")]
    #[schemars(with = "u64")]
    pub peak_inflight_per_op: f64,
    /// Requests one operation keeps in flight on average over its lifetime.
    #[serde(rename = "inflight_per_op_mean")]
    pub mean_inflight_per_op: f64,
    /// Share of operation time with exactly one request in flight (1 = strictly sequential).
    #[serde(rename = "sequential_ratio")]
    pub sequential_fraction: f64,
    /// Operations the shape was measured over.
    pub ops_count: u64,
    /// Logical bytes per operation (mean).
    #[serde(default)]
    pub bytes_per_op_mean: f64,
}

/// Arrivals at one in-flight level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InflightLevel {
    /// Requests in flight when a request arrived (itself included).
    pub inflight_count: u64,
    pub requests_count: u64,
    pub throttled_count: u64,
}

/// What the rows showed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Observed {
    pub window_s: f64,
    pub requests_count: u64,
    /// Requests answered 429 / 503.
    pub throttled_requests_count: u64,
    pub peak_inflight_count: u64,
    /// Time-weighted mean of requests in flight.
    pub inflight_mean: f64,
    /// Highest level a request arrived at and was served cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_clean_inflight_count: Option<u64>,
    /// Lowest level a request arrived at and was throttled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_throttled_inflight_count: Option<u64>,
    /// Operations in flight on average (≈ the workers of a closed-loop run).
    pub ops_inflight_mean: f64,
    /// Operations in flight on average, retry backoff left out: the time operations were
    /// served, which throughput scales with (backoff is waiting, not work; A3).
    #[serde(default)]
    pub ops_service_inflight_mean: f64,
    /// Successful operations per second.
    pub ops_per_s: f64,
    /// Arrivals per in-flight level (the evidence of the ceiling).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<InflightLevel>,
}

/// How far to trust the estimate.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    #[default]
    Low,
    Medium,
    High,
}

/// Where a capacity estimate comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapacitySource {
    /// Estimated from this run's rows.
    #[default]
    ThisRun,
    /// From a tuner search.
    Tune,
    /// Loaded from `<data>/capacity.toml` (an earlier run of the same target and workload).
    History,
}

/// How much load the target takes (design v4 §3); `report.capacity`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Capacity {
    /// In-flight requests the server accepts before it throttles (none: no throttling seen,
    /// or throttling unrelated to concurrency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inflight_ceiling: Option<Ceiling>,
    /// The fitted rate limit (requests per second) when the selected hypothesis is a rate
    /// limit (the lowest rate of a time-varying one); never the observed throughput.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_ceiling_requests_per_s: Option<f64>,
    pub per_op: OpShape,
    pub observed: Observed,
    pub confidence: Confidence,
    /// Why the confidence is what it is.
    pub confidence_reason: String,
    pub source: CapacitySource,
    /// Measured / predicted throughput of earlier verifications of this target and workload
    /// ([`calibrate`]); [`Capacity::predict`] scales its throughput by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_bias_ratio: Option<f64>,
    /// Competing limit models, the selected one first (design v5 §4).
    #[serde(default)]
    pub hypotheses: Vec<super::hypothesis::Hypothesis>,
}

/// What [`Capacity::predict`] expects at a concurrency.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Prediction {
    pub concurrency_count: u64,
    /// Requests in flight at the peak (`concurrency × peak_inflight_per_op`).
    pub peak_inflight_count: u64,
    /// Successful operations per second (point estimate and interval).
    pub ops_per_s: f64,
    pub ops_lo_per_s: f64,
    pub ops_hi_per_s: f64,
    /// Chance that the configuration gets throttled.
    pub throttle_probability_ratio: f64,
    /// Peak in-flight stays at or below the ceiling's clean level.
    pub within_ceiling: bool,
    pub confidence: Confidence,
}

/// `[start, end)` of a request as the server sees it: after the client's queue and the
/// connection setup, until the first response byte (the server is done with it then; the
/// body transfer is the client's). Rows without a time to first byte end with the request.
fn interval(q: &ReqRow) -> (i64, i64) {
    let start = q.t_us + (q.queue_us + q.connect_us()) as i64;
    let end = if q.ttfb_us > 0 {
        start + (q.send_us + q.ttfb_us) as i64
    } else {
        q.t_us + q.total_us as i64
    };
    (start, end.max(start + 1))
}

/// Peak concurrency and time with exactly one in flight, over `iv` (sorted or not).
fn sweep(iv: &[(i64, i64)]) -> (u64, i64, i64) {
    let mut ev: Vec<(i64, i32)> = Vec::with_capacity(iv.len() * 2);
    for (s, e) in iv {
        ev.push((*s, 1));
        ev.push((*e, -1));
    }
    // Ends before starts at the same instant.
    ev.sort_unstable();
    let (mut cur, mut peak, mut single, mut busy) = (0i64, 0i64, 0i64, 0i64);
    let mut last = ev.first().map_or(0, |e| e.0);
    for (t, d) in ev {
        if cur == 1 {
            single += t - last;
        }
        if cur >= 1 {
            busy += t - last;
        }
        last = t;
        cur += i64::from(d);
        peak = peak.max(cur);
    }
    (peak.max(0) as u64, single, busy)
}

fn quantile(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let i = ((v.len() as f64 * q).ceil() as usize).clamp(1, v.len()) - 1;
    v[i]
}

/// The per-operation request concurrency of the rows.
fn op_shape(reqs: &[&ReqRow], ops: &[&OpRow]) -> OpShape {
    let mut by_op: HashMap<u64, Vec<(i64, i64)>> = HashMap::new();
    for q in reqs.iter().filter(|q| q.op_id != 0) {
        by_op.entry(q.op_id).or_default().push(interval(q));
    }
    if by_op.is_empty() {
        return OpShape::default();
    }
    let latency: HashMap<u64, u64> = ops.iter().map(|o| (o.op_id, o.latency_us)).collect();
    let mut peaks = Vec::with_capacity(by_op.len());
    let (mut single_total, mut span_total, mut inflight_total) = (0f64, 0f64, 0f64);
    for (id, iv) in &by_op {
        let (peak, single, busy) = sweep(iv);
        peaks.push(peak as f64);
        let req_time: i64 = iv.iter().map(|(s, e)| e - s).sum();
        let span = latency
            .get(id)
            .map(|l| *l as f64)
            .filter(|l| *l > 0.0)
            .unwrap_or(busy as f64)
            .max(busy as f64)
            .max(1.0);
        single_total += single as f64;
        span_total += span;
        inflight_total += req_time as f64 / span;
    }
    let n = by_op.len() as f64;
    let requests_per_op = if ops.is_empty() {
        reqs.iter().filter(|q| q.op_id != 0).count() as f64 / n
    } else {
        ops.iter().map(|o| o.requests as f64).sum::<f64>() / ops.len() as f64
    };
    OpShape {
        requests_per_op,
        peak_inflight_per_op: peaks.iter().copied().fold(1.0, f64::max),
        mean_inflight_per_op: inflight_total / n,
        sequential_fraction: (single_total / span_total).clamp(0.0, 1.0),
        ops_count: by_op.len() as u64,
        bytes_per_op_mean: if ops.is_empty() {
            0.0
        } else {
            ops.iter().map(|o| o.bytes as f64).sum::<f64>() / ops.len() as f64
        },
    }
}

/// Estimate the capacity from a run's rows (requests that started in `window`).
pub fn estimate(reqs: &[ReqRow], ops: &[OpRow], window: Window) -> Capacity {
    estimate_with(reqs, ops, window, &FitInputs::default())
}

/// The limit models fitted to the rows of `window` (design v5 §4), the selected one first.
pub fn fit(reqs: &[ReqRow], ops: &[OpRow], window: Window, inputs: &FitInputs) -> Vec<Hypothesis> {
    super::limits::fit(reqs, ops, (window.from_us, window.to_us), inputs)
}

/// [`estimate`] with what the rows do not carry (the client's CPU): the in-flight ceiling,
/// the per-op shape and the fitted limit models. The ceiling (and the rate) are kept only
/// when the selected hypothesis asserts them: a rate limit is never reported as an in-flight
/// ceiling (A1), and random throttling sets no rate (A5).
pub fn estimate_with(
    reqs: &[ReqRow],
    ops: &[OpRow],
    window: Window,
    inputs: &FitInputs,
) -> Capacity {
    let mut c = estimate_ceiling(reqs, ops, window);
    c.hypotheses = fit(reqs, ops, window, inputs);
    c.reconcile();
    c
}

/// Time between pooled trials (µs): far longer than any trial, so their in-flight levels and
/// token buckets never interact.
const POOL_GAP_US: i64 = 10_000_000_000;

/// One trial of a pooled estimate: its rows (µs from its own window start) and the length of
/// its measurement window.
pub struct PooledTrial<'a> {
    pub reqs: &'a [ReqRow],
    pub ops: &'a [OpRow],
    pub window_s: f64,
}

/// The capacity over several trials (tune, `probe limits`), each read over its own
/// measurement window (fixes A11: never `Window::all()` across trials). Observed rates are per
/// second of measurement (`window_s` is the sum of the trials' windows).
pub fn estimate_pooled(trials: &[PooledTrial<'_>], inputs: &FitInputs) -> Capacity {
    let mut reqs = Vec::new();
    let mut ops = Vec::new();
    let mut total_s = 0.0;
    for (i, t) in trials.iter().enumerate() {
        let w = Window::measured(t.window_s);
        let off = i as i64 * POOL_GAP_US;
        reqs.extend(t.reqs.iter().filter(|q| w.contains(q.t_us)).map(|q| {
            let mut q = q.clone();
            q.t_us += off;
            q
        }));
        ops.extend(t.ops.iter().filter(|o| w.contains(o.t_us)).map(|o| {
            let mut o = o.clone();
            o.t_us += off;
            o
        }));
        total_s += t.window_s.max(0.0);
    }
    let mut c = estimate_with(&reqs, &ops, Window::all(), inputs);
    if total_s > 0.0 {
        let o = &mut c.observed;
        let span = o.window_s;
        let ok = ops.iter().filter(|o| o.ok).count() as f64;
        let busy: f64 = ops.iter().map(|o| o.latency_us as f64).sum();
        let service: f64 = ops
            .iter()
            .map(|o| o.latency_us.saturating_sub(o.breakdown.retry_us) as f64)
            .sum();
        o.window_s = total_s;
        o.ops_per_s = ok / total_s;
        o.ops_inflight_mean = busy / (total_s * 1e6);
        o.ops_service_inflight_mean = service / (total_s * 1e6);
        if span > 0.0 {
            // The time-weighted in-flight mean over the gaps is meaningless: rescale.
            o.inflight_mean *= span / total_s;
        }
    }
    c
}

fn estimate_ceiling(reqs: &[ReqRow], ops: &[OpRow], window: Window) -> Capacity {
    let reqs: Vec<&ReqRow> = reqs.iter().filter(|q| window.contains(q.t_us)).collect();
    let ops: Vec<&OpRow> = ops.iter().filter(|o| window.contains(o.t_us)).collect();
    let mut cap = Capacity {
        per_op: op_shape(&reqs, &ops),
        ..Default::default()
    };
    if reqs.is_empty() {
        cap.confidence_reason = "no request rows in the window".into();
        return cap;
    }
    // Arrival levels: requests in flight when each request arrived (itself included).
    let iv: Vec<(i64, i64)> = reqs.iter().map(|q| interval(q)).collect();
    let mut ev: Vec<(i64, i32, usize)> = Vec::with_capacity(iv.len() * 2);
    for (i, (s, e)) in iv.iter().enumerate() {
        ev.push((*s, 1, i));
        ev.push((*e, -1, i));
    }
    // At the same instant, ends (-1) come before starts (+1).
    ev.sort_unstable_by_key(|e| (e.0, e.1));
    let mut levels: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    let (mut cur, mut peak) = (0i64, 0i64);
    let (mut area, mut last) = (0f64, ev[0].0);
    let first = ev[0].0;
    let mut end_t = first;
    for (t, d, i) in &ev {
        area += cur as f64 * (t - last) as f64;
        last = *t;
        end_t = *t;
        cur += i64::from(*d);
        peak = peak.max(cur);
        if *d > 0 {
            let e = levels.entry(cur.max(1) as u64).or_default();
            e.0 += 1;
            if reqs[*i].is_throttle() {
                e.1 += 1;
            }
        }
    }
    let span_us = match (window.from_us, window.to_us) {
        (i64::MIN, _) | (_, i64::MAX) => (end_t - first).max(1),
        (a, b) => (b - a).max(1),
    };
    let window_s = span_us as f64 / 1e6;
    let throttled: u64 = levels.values().map(|v| v.1).sum();
    let ok_ops = ops.iter().filter(|o| o.ok).count() as f64;
    let ops_busy: f64 = ops.iter().map(|o| o.latency_us as f64).sum();
    let ops_service: f64 = ops
        .iter()
        .map(|o| o.latency_us.saturating_sub(o.breakdown.retry_us) as f64)
        .sum();
    cap.observed = Observed {
        window_s,
        requests_count: reqs.len() as u64,
        throttled_requests_count: throttled,
        peak_inflight_count: peak.max(0) as u64,
        inflight_mean: area / (end_t - first).max(1) as f64,
        max_clean_inflight_count: levels
            .iter()
            .rev()
            .find(|(_, v)| v.0 > v.1)
            .map(|(l, _)| *l),
        min_throttled_inflight_count: levels.iter().find(|(_, v)| v.1 > 0).map(|(l, _)| *l),
        ops_inflight_mean: ops_busy / span_us as f64,
        ops_service_inflight_mean: ops_service / span_us as f64,
        ops_per_s: ok_ops / window_s,
        levels: levels
            .iter()
            .map(|(l, (n, t))| InflightLevel {
                inflight_count: *l,
                requests_count: *n,
                throttled_count: *t,
            })
            .collect(),
    };
    cap.rate_ceiling_requests_per_s = rate_ceiling(&reqs);
    if throttled == 0 {
        cap.confidence = if reqs.len() >= 1000 {
            Confidence::Medium
        } else {
            Confidence::Low
        };
        cap.confidence_reason = format!(
            "no throttling up to {} requests in flight: the ceiling is above that",
            cap.observed.peak_inflight_count
        );
        return cap;
    }
    let (ceiling, errors) = match find_ceiling(&levels) {
        Some(c) => c,
        None => {
            cap.confidence = Confidence::Low;
            cap.confidence_reason = format!(
                "{throttled} throttled responses at every in-flight level: throttling does not follow concurrency (a rate limit?)"
            );
            return cap;
        }
    };
    let near = |l: u64| (l as f64) >= ceiling.lo - 2.0 && (l as f64) <= ceiling.hi + 2.0;
    let near_clean: u64 = levels
        .iter()
        .filter(|(l, _)| near(**l))
        .map(|(_, v)| v.0 - v.1)
        .sum();
    let near_thr: u64 = levels
        .iter()
        .filter(|(l, _)| near(**l))
        .map(|(_, v)| v.1)
        .sum();
    let misclass = errors as f64 / reqs.len() as f64;
    let width = ceiling.hi - ceiling.lo;
    // The bracket carries one level of measurement error on each side (`find_ceiling`).
    cap.confidence = if near_thr >= 20 && near_clean >= 20 && misclass <= 0.05 && width <= 4.0 {
        Confidence::High
    } else if throttled >= 5 && misclass <= 0.15 && width <= (0.2 * ceiling.lo).max(4.0) + 2.0 {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    cap.confidence_reason = format!(
        "throttling starts between {:.0} and {:.0} requests in flight ({near_clean} clean and {near_thr} throttled arrivals near the boundary; {:.1}% of arrivals on the wrong side)",
        ceiling.lo,
        ceiling.hi,
        misclass * 100.0
    );
    cap.inflight_ceiling = Some(ceiling);
    cap
}

/// The threshold level `k` that best separates clean arrivals (at `≤ k`) from throttled ones
/// (at `> k`), and how many arrivals fall on the wrong side. Client-side timing blurs the
/// boundary by a level or two, so the bracket is read from the per-level throttle share around
/// `k`: `lo` is the highest level at or below `k` where at most 10 % of (≥ 3) arrivals were
/// throttled, `hi` the lowest level above `lo` where at least half of them were. `None` when no
/// throttled arrival lies above the best threshold.
/// The highest class-balanced error a ceiling split may have (1.0 = no better than no split).
const MAX_SPLIT_COST: f64 = 0.75;

/// Log-likelihood within which a split is as plausible as the best (≈ a 95 % interval).
const PLAUSIBLE_SPLIT_LL: f64 = 2.0;

/// The level at or below which throttling is rarest and above which it is most common: the
/// split of `lv` (`(level, clean, throttled)`, in level order) that maximises the likelihood of
/// one throttled share below it and another above — `(best, lowest, highest)` of the splits
/// within [`PLAUSIBLE_SPLIT_LL`] of it.
fn likelihood_split(lv: &[(u64, u64, u64)]) -> (u64, u64, u64) {
    let bern = |k: u64, n: u64| {
        if k == 0 || k == n {
            return 0.0;
        }
        let p = k as f64 / n as f64;
        k as f64 * p.ln() + (n - k) as f64 * (1.0 - p).ln()
    };
    let tot_c: u64 = lv.iter().map(|x| x.1).sum();
    let tot_t: u64 = lv.iter().map(|x| x.2).sum();
    let (mut c0, mut t0) = (0u64, 0u64);
    let lls: Vec<(u64, f64)> = lv
        .iter()
        .map(|(l, c, t)| {
            c0 += c;
            t0 += t;
            (
                *l,
                bern(t0, c0 + t0) + bern(tot_t - t0, tot_c - c0 + tot_t - t0),
            )
        })
        .collect();
    let mut best = (f64::NEG_INFINITY, 0u64);
    for (l, ll) in &lls {
        if *ll > best.0 + 1e-9 {
            best = (*ll, *l);
        }
    }
    let plausible = lls
        .iter()
        .filter(|(_, ll)| *ll >= best.0 - PLAUSIBLE_SPLIT_LL)
        .map(|(l, _)| *l);
    let low = plausible.clone().min().unwrap_or(best.1);
    let high = plausible.max().unwrap_or(best.1);
    (best.1, low, high)
}

/// Move the split `k` up past the levels right above it that were served (almost) cleanly:
/// ≥ 3 arrivals, at most 10 % throttled. `lv` is `(level, clean, throttled)` in level order.
pub(crate) fn nudge_past_clean_levels(lv: &[(u64, u64, u64)], mut k: u64) -> u64 {
    let start = k;
    for (l, c, t) in lv.iter().filter(|x| x.0 > start) {
        let n = c + t;
        if *l == k + 1 && n >= 3 && (*t as f64) <= 0.1 * n as f64 {
            k = *l;
        } else {
            break;
        }
    }
    k
}

fn find_ceiling(levels: &BTreeMap<u64, (u64, u64)>) -> Option<(Ceiling, u64)> {
    let lv: Vec<(u64, u64, u64)> = levels.iter().map(|(l, (n, t))| (*l, n - t, *t)).collect();
    let total_clean: u64 = lv.iter().map(|x| x.1).sum();
    let total_thr: u64 = lv.iter().map(|x| x.2).sum();
    // The split minimises the class-balanced error: throttled responses are usually a small
    // minority (11 of ~2000 requests on a parquet lab), and client-side in-flight levels read a
    // little high, so with equal weights "no split" wins and throttling looks unrelated to load.
    // Each class weighs 1/its size, so a throttled response below the split costs as much as
    // the clean requests above it in proportion.
    let (wt, wc) = (
        1.0 / total_thr.max(1) as f64,
        1.0 / total_clean.max(1) as f64,
    );
    // k = 0 (everything throttled-side) first, then each observed level.
    let mut best = (1.0f64, 0u64, total_clean);
    let (mut thr_below, mut clean_below) = (0u64, 0u64);
    for (l, c, t) in &lv {
        thr_below += t;
        clean_below += c;
        let clean_above = total_clean - clean_below;
        let cost = thr_below as f64 * wt + clean_above as f64 * wc;
        if cost < best.0 - 1e-12 {
            best = (cost, *l, thr_below + clean_above);
        }
    }
    let (cost, _, errors) = best;
    // No split (cost 1.0) explains throttling unrelated to load just as well; a ceiling needs a
    // split that clearly separates the classes (balanced accuracy above 62.5 %).
    if cost > MAX_SPLIT_COST {
        return None;
    }
    // Where the split lies is a question of rates, not of counts: the level at which the
    // throttled share jumps (a two-rate Bernoulli likelihood). The balanced cost above decides
    // only whether there is a ceiling at all. With a dozen throttled responses each weighs as
    // much as hundreds of clean requests there, so on a loaded machine — where the client's
    // in-flight count also reads a level or more low now and then — five stray throttles at
    // 18–23 (≈ 0.7 % of those levels, against 25–60 % from 24 on) pulled the split down to
    // 17 on a lab limited at 24.
    let (k, k_low, k_high) = likelihood_split(&lv);
    // The client's in-flight count is a level or so off the server's now and then (coarse
    // scheduling on Windows and macOS, a loaded machine), both ways: a few throttles land below
    // the limit, a few clean arrivals above it. Near the limit, levels have few arrivals, so
    // the best split can err either way by a level or two (17–22 and 25 on a lab limited at 24).
    // The bracket therefore spans every split about as likely as the best (profile likelihood
    // within 2: ≈ 95 %): `lo` is the lowest of them, `hi` the first clearly throttled level
    // above the highest — and above the levels just past the best split that still served
    // ≥ 90 % of their (≥ 3) arrivals cleanly.
    let k_hi = nudge_past_clean_levels(&lv, k).max(k_high);
    // A throttled arrival must exist above the split, or throttling does not follow load.
    lv.iter().find(|(l, _, t)| *l > k && *t > 0)?;
    let share = |c: u64, t: u64| t as f64 / (c + t).max(1) as f64;
    let above = k_hi.max(k_low);
    let hi = lv
        .iter()
        .find(|(l, c, t)| *l > above && c + t >= 2 && share(*c, *t) >= 0.5)
        .or_else(|| lv.iter().find(|(l, _, t)| *l > above && *t > 0))
        .or_else(|| lv.iter().find(|(l, _, t)| *l > k_low && *t > 0))?
        .0;
    // The client's count is a level off the server's now and then, either way, and near the
    // limit the levels hold a handful of arrivals each: whether 24 or 25 was the last clean
    // level is within that error (CI: a clean 23, 24 and 25 on a lab limited at 24, a loaded
    // machine's lone throttles at 21–23). The bracket includes one level of it on each side;
    // the estimate stays the likeliest split.
    let lo = k_low.saturating_sub(1);
    let hi = hi + 1;
    Some((
        Ceiling {
            requests: (k as f64).clamp(lo as f64, (hi - 1) as f64),
            lo: lo as f64,
            hi: hi as f64,
        },
        errors,
    ))
}

/// Median accepted requests per second over the seconds with throttling.
fn rate_ceiling(reqs: &[&ReqRow]) -> Option<f64> {
    let mut per: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    for q in reqs {
        let e = per.entry(q.t_us.div_euclid(1_000_000)).or_default();
        if q.is_throttle() {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }
    let mut rates: Vec<f64> = per
        .values()
        .filter(|e| e.0 > 0)
        .map(|e| e.1 as f64)
        .collect();
    (!rates.is_empty()).then(|| quantile(&mut rates, 0.5))
}

impl Capacity {
    /// The estimate with its hypotheses filled in (from the 4.x fields when the fit left
    /// none).
    pub fn finished(mut self) -> Capacity {
        if self.hypotheses.is_empty() {
            self.hypotheses = super::hypothesis::from_capacity(&self);
        }
        self
    }

    /// The selected limit model.
    pub fn selected(&self) -> Option<&Hypothesis> {
        super::hypothesis::selected(&self.hypotheses)
    }

    /// The selected limit model when its confidence is medium or high.
    pub fn asserted(&self) -> Option<&Hypothesis> {
        super::hypothesis::asserted(&self.hypotheses)
    }

    /// Make the 4.x fields agree with the selected hypothesis.
    fn reconcile(&mut self) {
        let Some(sel) = self.selected().cloned() else {
            return;
        };
        match sel.kind {
            HypothesisKind::InflightLimit => {
                self.rate_ceiling_requests_per_s = None;
                if self.inflight_ceiling.is_none()
                    && let Some(c) = sel.params.ceiling_inflight_count
                {
                    self.inflight_ceiling = Some(Ceiling {
                        requests: c as f64,
                        lo: c as f64,
                        hi: c as f64 + 1.0,
                    });
                    self.confidence = sel.confidence;
                    self.confidence_reason = sel.evidence.join("; ");
                }
            }
            HypothesisKind::RateLimit | HypothesisKind::TimeVarying => {
                self.inflight_ceiling = None;
                self.rate_ceiling_requests_per_s = sel.params.requests_per_s;
                self.confidence = sel.confidence;
                self.confidence_reason = sel.evidence.join("; ");
            }
            HypothesisKind::KeyLimit | HypothesisKind::ClientBound => {
                self.inflight_ceiling = None;
                self.rate_ceiling_requests_per_s = None;
                self.confidence = sel.confidence;
                self.confidence_reason = sel.evidence.join("; ");
            }
            HypothesisKind::Queueing | HypothesisKind::BandwidthCap | HypothesisKind::None => {
                self.rate_ceiling_requests_per_s = None;
                if self.hypotheses.iter().any(|h| {
                    matches!(
                        h.kind,
                        HypothesisKind::InflightLimit | HypothesisKind::RateLimit
                    )
                }) {
                    // The throttle family was fitted and throttling follows no limit (random
                    // 503s): no ceiling either.
                    self.inflight_ceiling = None;
                    self.confidence = sel.confidence;
                    self.confidence_reason = sel.evidence.join("; ");
                } else if sel.kind != HypothesisKind::None {
                    self.confidence_reason = sel.evidence.join("; ");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::outliers::test_req;

    /// A closed-loop run against a server that throttles above `limit` in flight: `workers`
    /// workers, each op issuing `shape` (sequential groups of parallel requests) of `ms` each;
    /// a throttled request is answered in 0.2 ms. Simulated in time order.
    pub(crate) fn synthetic(
        workers: usize,
        limit: usize,
        shape: &[usize],
        ms: u64,
        ops_per_worker: usize,
    ) -> (Vec<ReqRow>, Vec<OpRow>) {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        struct W {
            op_id: u64,
            op_start: i64,
            group: usize,
            requests: u32,
            throttled: bool,
            done: usize,
        }
        let mut reqs = Vec::new();
        let mut ops = Vec::new();
        let mut inflight: Vec<i64> = Vec::new();
        let mut next_id = 1u64;
        let mut ws: Vec<W> = Vec::new();
        let mut heap = BinaryHeap::new();
        for w in 0..workers {
            let t = w as i64 * 37;
            ws.push(W {
                op_id: next_id,
                op_start: t,
                group: 0,
                requests: 0,
                throttled: false,
                done: 0,
            });
            next_id += 1;
            heap.push(Reverse((t, w)));
        }
        while let Some(Reverse((t, w))) = heap.pop() {
            let st = &mut ws[w];
            if st.group == shape.len() {
                let mut o = crate::analysis::simulate::tests::op(
                    st.op_id,
                    st.op_start,
                    (t - st.op_start) as u64,
                );
                o.requests = st.requests;
                o.throttled = st.throttled;
                ops.push(o);
                st.done += 1;
                if st.done == ops_per_worker {
                    continue;
                }
                st.op_id = next_id;
                next_id += 1;
                st.op_start = t + 50;
                st.group = 0;
                st.requests = 0;
                st.throttled = false;
                heap.push(Reverse((t + 50, w)));
                continue;
            }
            let mut group_end = t;
            for _ in 0..shape[st.group] {
                inflight.retain(|e| *e > t);
                let thr = inflight.len() + 1 > limit;
                let dur = if thr { 200 } else { ms as i64 * 1000 };
                let mut q = test_req(t, dur as u64);
                q.op_id = st.op_id;
                if thr {
                    q.status = 503;
                    st.throttled = true;
                }
                inflight.push(t + dur);
                reqs.push(q);
                st.requests += 1;
                group_end = group_end.max(t + dur);
            }
            st.group += 1;
            heap.push(Reverse((group_end, w)));
        }
        reqs.sort_by_key(|q| q.t_us);
        (reqs, ops)
    }

    #[test]
    fn brackets_the_inflight_limit_of_sequential_ops() {
        let (reqs, ops) = synthetic(30, 24, &[1], 5, 60);
        let c = estimate(&reqs, &ops, Window::all());
        let ce = c.inflight_ceiling.expect("ceiling");
        assert!(
            ce.lo <= 24.0 && 24.0 < ce.hi,
            "{ce:?} {}",
            c.confidence_reason
        );
        // The estimate is the likeliest split (24 here), inside the bracket.
        assert!(ce.lo <= ce.requests && ce.requests < ce.hi, "{ce:?}");
        assert_eq!(ce.requests, 24.0, "{ce:?}");
        assert!(
            (c.per_op.peak_inflight_per_op - 1.0).abs() < 1e-9,
            "{:?}",
            c.per_op
        );
        assert!(c.per_op.sequential_fraction > 0.99, "{:?}", c.per_op);
        assert!(c.observed.throttled_requests_count > 0);
        assert!(c.observed.min_throttled_inflight_count.unwrap() > 24);
        assert!(c.confidence >= Confidence::Medium, "{c:#?}");
    }

    #[test]
    fn parquet_shape_peaks_at_its_parallel_reads() {
        // HEAD, footer, then 4 parallel column reads: 6 requests per op, peak 4.
        let (reqs, ops) = synthetic(8, 24, &[1, 1, 4], 5, 40);
        let c = estimate(&reqs, &ops, Window::all());
        let s = c.per_op;
        assert!((s.requests_per_op - 6.0).abs() < 0.01, "{s:?}");
        assert_eq!(s.peak_inflight_per_op, 4.0, "{s:?}");
        assert!(
            s.sequential_fraction > 0.5 && s.sequential_fraction < 0.8,
            "{s:?}"
        );
        assert!(
            s.mean_inflight_per_op > 1.5 && s.mean_inflight_per_op < 2.5,
            "{s:?}"
        );
        let ce = c
            .inflight_ceiling
            .expect("8 workers × 4 = 32 > 24 throttles");
        assert!(ce.lo <= 24.0 && 24.0 < ce.hi, "{ce:?}");
    }

    #[test]
    fn no_throttling_means_no_ceiling() {
        let (reqs, ops) = synthetic(8, 1000, &[1], 5, 20);
        let c = estimate(&reqs, &ops, Window::all());
        assert!(c.inflight_ceiling.is_none());
        assert_eq!(c.observed.peak_inflight_count, 8);
        assert!(
            c.confidence_reason.contains("no throttling"),
            "{}",
            c.confidence_reason
        );
        assert!(estimate(&[], &[], Window::all()).inflight_ceiling.is_none());
    }

    #[test]
    fn a_few_throttled_responses_above_the_limit_still_bracket_it() {
        // A parquet lab on a slow CI machine: 11 throttled responses, all above 24 in flight,
        // and clean requests up to 28 (client-side levels read a little high). With equal
        // weights "no split" (11 errors) beat the right split (80 clean above 24).
        let mut levels = BTreeMap::new();
        for l in 1..=23u64 {
            levels.insert(l, (100, 0));
        }
        levels.insert(24, (60, 0));
        for l in 25..=28u64 {
            levels.insert(l, (20 + 3, 3));
        }
        levels.insert(29, (2, 2));
        let (c, _) = find_ceiling(&levels).expect("a ceiling");
        assert!(c.lo <= 24.0 && 24.0 < c.hi, "{c:?}");
    }

    /// Windows CI, parquet lab (limit 24): one throttled arrival at level 24 (the client's
    /// in-flight count a level off the server's) must not pull the ceiling below 24.
    #[test]
    fn a_lone_throttle_one_level_low_does_not_lower_the_ceiling() {
        let ci = [
            (23u64, 89u64, 0u64),
            (24, 37, 1),
            (25, 36, 8),
            (26, 18, 6),
            (27, 11, 3),
            (28, 4, 1),
        ];
        let exactly: BTreeMap<u64, (u64, u64)> =
            ci.iter().map(|(l, n, t)| (*l, (*n, *t))).collect();
        // The same levels behind a run's many clean arrivals at lower levels: the
        // class-balanced split alone then prefers 23.
        let mut with_low = exactly.clone();
        for l in 1..=22u64 {
            with_low.insert(l, (200, 0));
        }
        for levels in [exactly, with_low] {
            let (c, _) = find_ceiling(&levels).expect("a ceiling");
            assert!(c.lo <= 24.0 && 24.0 < c.hi, "{c:?}");
        }
    }

    /// Windows CI, parquet lab (limit 24): a clean level 25 (1 throttled of 13, the client's
    /// count a level high) must not lift the ceiling above 24 either.
    /// A parquet lab (limit 24) on a loaded 4-core machine: stray throttles one per level at
    /// 18–23 (the client's in-flight count reads low when it is descheduled) must not pull the
    /// ceiling down to 17.
    #[test]
    fn stray_throttles_below_the_limit_do_not_lower_the_ceiling() {
        let mut levels: BTreeMap<u64, (u64, u64)> = (1..=9u64).map(|l| (l, (200, 0))).collect();
        for (l, n, t) in [
            (10u64, 211u64, 0u64),
            (11, 293, 0),
            (12, 210, 0),
            (13, 256, 0),
            (14, 370, 0),
            (15, 217, 0),
            (16, 238, 0),
            (17, 279, 0),
            (18, 146, 1),
            (19, 145, 1),
            (20, 153, 1),
            (21, 75, 1),
            (22, 65, 0),
            (23, 57, 1),
            (24, 13, 1),
            (25, 15, 5),
            (26, 8, 1),
            (27, 5, 3),
            (28, 3, 1),
            (29, 2, 1),
        ] {
            levels.insert(l, (n, t));
        }
        let (c, _) = find_ceiling(&levels).expect("a ceiling");
        assert!(c.lo <= 24.0 && 24.0 < c.hi, "{c:?}");
        // 24 × 0.85 / 4 parquet requests per op → 4 or 5 workers, not 3.
        assert!(c.lo >= 22.0, "{c:?}");
    }

    /// macOS CI, parquet lab (limit 24): throttles at 23 and 24 and a clean 25 (0 of 5). The
    /// best split is 25; the bracket must still reach down to the limit.
    #[test]
    fn a_small_clean_level_above_the_limit_does_not_lift_the_bracket() {
        let mut levels: BTreeMap<u64, (u64, u64)> = (1..=21u64).map(|l| (l, (100, 0))).collect();
        for (l, n, t) in [
            (22u64, 28u64, 0u64),
            (23, 27, 1),
            (24, 8, 1),
            (25, 5, 0),
            (26, 5, 2),
            (27, 3, 2),
            (28, 2, 2),
            (29, 2, 2),
        ] {
            levels.insert(l, (n, t));
        }
        let (c, _) = find_ceiling(&levels).expect("a ceiling");
        assert!(c.lo <= 24.0 && 24.0 < c.hi, "{c:?}");
        assert!(c.lo >= 19.0, "{c:?}");
    }

    #[test]
    fn a_clean_level_one_above_the_limit_does_not_raise_the_ceiling() {
        let win: [(u64, u64, u64); 29] = [
            (1, 4, 0),
            (2, 5, 0),
            (3, 5, 0),
            (4, 9, 0),
            (5, 33, 0),
            (6, 103, 0),
            (7, 140, 0),
            (8, 163, 0),
            (9, 136, 0),
            (10, 168, 0),
            (11, 179, 0),
            (12, 137, 0),
            (13, 145, 1),
            (14, 164, 0),
            (15, 137, 1),
            (16, 173, 0),
            (17, 187, 0),
            (18, 131, 0),
            (19, 126, 0),
            (20, 158, 0),
            (21, 61, 0),
            (22, 44, 0),
            (23, 37, 0),
            (24, 17, 0),
            (25, 13, 1),
            (26, 8, 1),
            (27, 5, 2),
            (28, 4, 2),
            (29, 1, 0),
        ];
        let levels: BTreeMap<u64, (u64, u64)> =
            win.iter().map(|(l, n, t)| (*l, (*n, *t))).collect();
        let (c, _) = find_ceiling(&levels).expect("a ceiling");
        assert!(c.lo <= 24.0 && 24.0 < c.hi, "{c:?}");
    }

    #[test]
    fn throttling_unrelated_to_concurrency_is_no_inflight_ceiling() {
        let (mut reqs, ops) = synthetic(8, 1000, &[1], 5, 50);
        for (i, q) in reqs.iter_mut().enumerate() {
            if i % 10 == 0 {
                q.status = 503;
            }
        }
        let c = estimate(&reqs, &ops, Window::all());
        assert!(c.inflight_ceiling.is_none(), "{c:#?}");
        // Every 10th request, whatever the load or rate: no limit, and no rate to pace at
        // (design v5 §4, A5: the accepted rate is not a rate limit).
        let s = c.selected().unwrap();
        assert_eq!(s.kind, HypothesisKind::None, "{:#?}", c.hypotheses);
        assert!(c.rate_ceiling_requests_per_s.is_none());
    }

    #[test]
    fn window_filters_rows() {
        let (reqs, ops) = synthetic(30, 24, &[1], 5, 60);
        let w = Window {
            from_us: 0,
            to_us: 10_000,
        };
        let c = estimate(&reqs, &ops, w);
        assert!(c.observed.requests_count < reqs.len() as u64);
        assert!((c.observed.window_s - 0.01).abs() < 1e-9);
        assert_eq!(Window::measured(2.0).to_us, 2_000_000);
    }

    #[test]
    fn serializes_levels_as_integers_and_round_trips() {
        let (reqs, ops) = synthetic(30, 24, &[1], 5, 30);
        let c = estimate(&reqs, &ops, Window::all());
        let v = serde_json::to_value(&c).unwrap();
        assert!(v["inflight_ceiling"]["lo_inflight_count"].is_u64(), "{v}");
        assert!(v["per_op"]["peak_inflight_per_op_count"].is_u64(), "{v}");
        let back: Capacity = serde_json::from_value(v).unwrap();
        assert_eq!(back.inflight_ceiling, c.inflight_ceiling);
    }
}
