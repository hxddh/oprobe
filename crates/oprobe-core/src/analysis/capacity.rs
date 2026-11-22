//! Capacity model (design v4 §3): the only source of "how much can I push".
//!
//! [`estimate`] reconstructs the requests in flight over time from the request rows (start and
//! end times), then finds the in-flight level at which the service starts throttling: the
//! lowest level whose arrivals are throttled, bracketed by the highest level whose arrivals are
//! served cleanly ([`Ceiling`]). It also measures how many requests one operation keeps in
//! flight ([`OpShape`]: a parquet scan issues a HEAD, a footer read, then 4 parallel column
//! reads, so it peaks at 4, not at its 6.3 requests per op). The ceiling is never derived from
//! mean latency × rate.
//!
//! [`Capacity::safe_concurrency`] turns both into a worker count that keeps peak in-flight
//! under the ceiling with spare headroom; [`Capacity::predict`] estimates throughput and the
//! chance of throttling at a concurrency. Every finding, simulator, tuner margin and App
//! action that talks about concurrency uses these; nothing else computes a safe concurrency.
//!
//! Ceilings are saved per target and workload in `<data>/capacity.toml` ([`History`]), so
//! later runs and tune can start from them (`source: history`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use super::limits::FitInputs;
use super::hypothesis::{Hypothesis, HypothesisKind};
use crate::model::{OpRow, ReqRow};
use crate::store::TargetId;

/// Spare in-flight capacity kept below the ceiling by default (15 %).
pub const DEFAULT_HEADROOM_RATIO: f64 = 0.15;

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
    /// In-flight requests the service accepted cleanly (the estimate; equals `lo`).
    #[serde(rename = "inflight_count", serialize_with = "ser_whole")]
    #[schemars(with = "u64")]
    pub requests: f64,
    /// Highest in-flight level whose arrivals were served without throttling.
    #[serde(rename = "lo_inflight_count", serialize_with = "ser_whole")]
    #[schemars(with = "u64")]
    pub lo: f64,
    /// Lowest in-flight level whose arrivals were throttled (the limit is in `[lo, hi)`).
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
pub fn estimate_with(reqs: &[ReqRow], ops: &[OpRow], window: Window, inputs: &FitInputs) -> Capacity {
    let mut c = estimate_ceiling(reqs, ops, window);
    c.hypotheses = fit(reqs, ops, window, inputs);
    c.reconcile();
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
    cap.confidence = if near_thr >= 20 && near_clean >= 20 && misclass <= 0.05 && width <= 2.0 {
        Confidence::High
    } else if throttled >= 5 && misclass <= 0.15 && width <= (0.2 * ceiling.lo).max(4.0) {
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
    let (cost, k, errors) = best;
    // No split (cost 1.0) explains throttling unrelated to load just as well; a ceiling needs a
    // split that clearly separates the classes (balanced accuracy above 62.5 %).
    if cost > MAX_SPLIT_COST {
        return None;
    }
    // A throttled arrival must exist above the split, or throttling does not follow load.
    lv.iter().find(|(l, _, t)| *l > k && *t > 0)?;
    let share = |c: u64, t: u64| t as f64 / (c + t).max(1) as f64;
    let lo = lv
        .iter()
        .rev()
        .find(|(l, c, t)| *l <= k && c + t >= 3 && share(*c, *t) <= 0.1)
        .or_else(|| lv.iter().rev().find(|(l, c, _)| *l <= k && *c > 0))
        .map_or(0, |x| x.0);
    let hi = lv
        .iter()
        .find(|(l, c, t)| *l > lo && c + t >= 2 && share(*c, *t) >= 0.5)
        .or_else(|| lv.iter().find(|(l, _, t)| *l > lo && *t > 0))?
        .0;
    Some((
        Ceiling {
            requests: lo as f64,
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
    /// This run's estimate, completed by a remembered one (`<data>/capacity.toml`, the same
    /// target and workload): a run that stayed under the ceiling sees no throttling and so no
    /// ceiling of its own, yet the ceiling still holds. The remembered ceiling is used
    /// (`source: history`) unless this run has its own, or served requests cleanly above the
    /// remembered limit (the service changed: the memory is stale). This run's per-op shape
    /// and observations are kept.
    pub fn with_history(self, history: Option<Capacity>) -> Capacity {
        let mut c = self.merge_history(history);
        if c.source == CapacitySource::History {
            // The remembered ceiling is the hypothesis; this run's fits are alternatives.
            let mut hs = super::hypothesis::from_capacity(&c);
            for h in &mut hs {
                h.evidence.push("remembered from an earlier run of this target and workload".into());
            }
            for mut h in std::mem::take(&mut c.hypotheses) {
                h.selected = false;
                hs.push(h);
            }
            c.hypotheses = hs;
        } else if c.hypotheses.is_empty() {
            c.hypotheses = super::hypothesis::from_capacity(&c);
        }
        c
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
                if self.observed.throttled_requests_count > 0 {
                    // Throttling that follows no limit (random 503s): no ceiling either.
                    self.inflight_ceiling = None;
                    self.confidence = sel.confidence;
                    self.confidence_reason = sel.evidence.join("; ");
                } else if sel.kind != HypothesisKind::None {
                    self.confidence_reason = sel.evidence.join("; ");
                }
            }
        }
    }

    fn merge_history(mut self, history: Option<Capacity>) -> Capacity {
        let Some(h) = history else { return self };
        let Some(hc) = h.inflight_ceiling else {
            return self;
        };
        if self.inflight_ceiling.is_some()
            || self
                .observed
                .max_clean_inflight_count
                .is_some_and(|m| m as f64 >= hc.hi)
        {
            return self;
        }
        self.inflight_ceiling = Some(hc);
        if self.rate_ceiling_requests_per_s.is_none() {
            self.rate_ceiling_requests_per_s = h.rate_ceiling_requests_per_s;
        }
        self.confidence = h.confidence;
        self.confidence_reason = format!(
            "remembered from an earlier run of this target and workload ({}); this run peaked at {} requests in flight without crossing it",
            h.confidence_reason, self.observed.peak_inflight_count
        );
        self.source = CapacitySource::History;
        self
    }

    /// The most workers any advice may suggest: [`Capacity::safe_concurrency`] with the
    /// default headroom. `None` when no ceiling is known.
    pub fn max_advisable_concurrency(&self) -> Option<u32> {
        self.safe_concurrency(DEFAULT_HEADROOM_RATIO)
    }

    /// Workers that keep peak in-flight under the ceiling with `headroom_ratio` spare:
    /// `floor(ceiling.lo × (1 − headroom) / peak_inflight_per_op)`, at least 1. `None` when no
    /// ceiling is known.
    pub fn safe_concurrency(&self, headroom_ratio: f64) -> Option<u32> {
        let c = self.inflight_ceiling?;
        let per_op = self.per_op.peak_inflight_per_op.max(1.0);
        let h = headroom_ratio.clamp(0.0, 0.95);
        Some(((c.lo * (1.0 - h) / per_op).floor() as u32).max(1))
    }

    /// Expected throughput, throttling and peak in-flight at `concurrency` workers.
    ///
    /// Throughput scales from the observed rate per worker (operations per second divided by
    /// operations in flight) and saturates where mean in-flight reaches the ceiling; the
    /// interval widens with the distance from the observed concurrency. The throttle
    /// probability rises from 0 to 1 across the ceiling's bracket.
    pub fn predict(&self, concurrency: u32) -> Prediction {
        let c = f64::from(concurrency.max(1));
        let per_op_peak = self.per_op.peak_inflight_per_op.max(1.0);
        let per_op_mean = self.per_op.mean_inflight_per_op.max(1e-6);
        let peak = c * per_op_peak;
        let o = &self.observed;
        // Throughput per worker from the time operations were served: retry backoff is
        // waiting, not work (A3).
        let busy = if o.ops_service_inflight_mean > 0.0 {
            o.ops_service_inflight_mean
        } else {
            o.ops_inflight_mean
        };
        let per_worker = (busy > 0.0).then(|| o.ops_per_s / busy);
        let workers = match self.inflight_ceiling {
            Some(ce) => c.min((ce.hi.max(1.0) / per_op_mean).max(1.0)),
            None => c,
        };
        let bias = self
            .calibration_bias_ratio
            .filter(|b| b.is_finite() && *b > 0.0)
            .unwrap_or(1.0);
        let mut ops = per_worker.map_or(0.0, |r| r * workers) * bias;
        let rel = if busy > 0.0 {
            (0.1 + 0.2 * (c / busy).ln().abs()).min(0.6)
        } else {
            0.6
        };
        let (mut throttle, mut within) = match self.inflight_ceiling {
            Some(ce) => {
                let mid = (ce.lo + ce.hi) / 2.0;
                let width = (ce.hi - ce.lo).max(1.0);
                let p = 1.0 / (1.0 + (-(peak - mid) * 3.0 / width).exp());
                (p, peak <= ce.lo)
            }
            None => {
                let seen = o.peak_inflight_count as f64;
                if peak <= seen || seen == 0.0 {
                    (0.0, true)
                } else {
                    (((peak - seen) / peak).min(0.5), true)
                }
            }
        };
        // The other limit models cap the demand (design v5 §4, prediction).
        if let Some(h) = self.asserted() {
            let rpo = self.clean_requests_per_op();
            let demand_ops = ops;
            let cap_ops = match h.kind {
                HypothesisKind::RateLimit | HypothesisKind::TimeVarying => {
                    h.params.requests_per_s.map(|r| r / rpo)
                }
                HypothesisKind::KeyLimit => h.params.requests_per_s.map(|r| {
                    r / rpo / h.params.key_share_ratio.unwrap_or(1.0).clamp(1e-3, 1.0)
                }),
                HypothesisKind::Queueing => {
                    match (h.params.servers_count, h.params.service_ms) {
                        (Some(n), Some(ms)) if ms > 0.0 => Some(n as f64 * 1000.0 / ms / rpo),
                        _ => None,
                    }
                }
                HypothesisKind::BandwidthCap => h
                    .params
                    .bytes_per_s
                    .filter(|_| self.per_op.bytes_per_op_mean > 0.0)
                    .map(|b| b / self.per_op.bytes_per_op_mean),
                HypothesisKind::ClientBound => Some(o.ops_per_s * 1.05),
                _ => None,
            };
            if let Some(cap_ops) = cap_ops.filter(|x| x.is_finite() && *x > 0.0) {
                if demand_ops > cap_ops {
                    ops = cap_ops * bias;
                    // A rate limit throttles the excess; the others only slow down.
                    if matches!(
                        h.kind,
                        HypothesisKind::RateLimit
                            | HypothesisKind::TimeVarying
                            | HypothesisKind::KeyLimit
                    ) {
                        throttle = throttle.max(1.0 - cap_ops / demand_ops);
                        within = false;
                    }
                } else if matches!(
                    h.kind,
                    HypothesisKind::RateLimit
                        | HypothesisKind::TimeVarying
                        | HypothesisKind::KeyLimit
                ) {
                    throttle = 0.0;
                    within = true;
                }
            }
        }
        let mut confidence = self.confidence;
        if per_worker.is_none() || (busy > 0.0 && rel >= 0.4) {
            confidence = Confidence::Low;
        }
        Prediction {
            concurrency_count: u64::from(concurrency.max(1)),
            peak_inflight_count: peak.ceil() as u64,
            ops_per_s: ops,
            ops_lo_per_s: ops * (1.0 - rel),
            ops_hi_per_s: ops * (1.0 + rel),
            throttle_probability_ratio: throttle.clamp(0.0, 1.0),
            within_ceiling: within,
            confidence,
        }
    }

    /// Requests per operation without the retries throttling caused (what a paced client
    /// sends).
    pub fn clean_requests_per_op(&self) -> f64 {
        let o = &self.observed;
        let thr = crate::util::ratio(
            o.throttled_requests_count as f64,
            o.requests_count as f64,
        );
        (self.per_op.requests_per_op * (1.0 - thr)).max(1.0)
    }

    /// Operations per second a client may be paced at so the selected rate limit (or per-key
    /// limit) is not exceeded, with `headroom_ratio` spare. `None` without an asserted rate
    /// hypothesis.
    pub fn safe_rate(&self, headroom_ratio: f64) -> Option<f64> {
        let h = self.asserted()?;
        let rps = h.params.requests_per_s?;
        let rpo = self.clean_requests_per_op();
        let share = match h.kind {
            HypothesisKind::RateLimit | HypothesisKind::TimeVarying => 1.0,
            HypothesisKind::KeyLimit => h.params.key_share_ratio.unwrap_or(1.0).clamp(1e-3, 1.0),
            _ => return None,
        };
        let h = headroom_ratio.clamp(0.0, 0.95);
        Some((rps / rpo / share * (1.0 - h)).max(1.0))
    }
}

// ---------------------------------------------------------------------------------------
// History: <data>/capacity.toml
// ---------------------------------------------------------------------------------------

/// One remembered estimate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryEntry {
    pub capacity: Capacity,
    /// The run it was estimated from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// RFC 3339.
    pub updated_at: String,
    /// What verifications of this target and workload taught ([`calibrate`]).
    #[serde(default, skip_serializing_if = "Calibration::is_empty")]
    pub calibration: Calibration,
}

/// One verified prediction (`oprobe verify`), as remembered.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Observation {
    /// The finding (or `recommendation`) whose change was verified.
    pub code: String,
    pub outcome: crate::analysis::verification::VerifyOutcome,
    /// Workers of the verified configuration.
    pub concurrency_count: u64,
    /// What [`Capacity::predict`] expected (successful ops/s).
    pub predicted_ops_per_s: f64,
    /// What the candidate runs measured (successful ops/s, mean).
    pub measured_ops_per_s: f64,
    /// Peak requests in flight of the candidate runs (max).
    pub peak_inflight_count: u64,
    /// Throttled responses / requests of the candidate runs.
    pub throttled_request_ratio: f64,
    /// The throttle budget the outcome was judged against.
    pub max_throttle_ratio: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// RFC 3339.
    #[serde(default)]
    pub at: String,
}

/// Verification outcomes stored next to the capacity history, and the prediction bias they
/// imply.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Calibration {
    /// Measured / predicted throughput (exponential moving average; 1 = unbiased).
    #[serde(default)]
    pub ops_bias_ratio: f64,
    /// Observations folded into the bias.
    #[serde(default)]
    pub observations_count: u64,
    /// The newest observations (at most [`CALIBRATION_KEEP`]).
    #[serde(default)]
    pub observations: Vec<Observation>,
}

impl Calibration {
    pub fn is_empty(&self) -> bool {
        self.observations_count == 0 && self.observations.is_empty()
    }
}

/// Observations kept per target and workload.
pub const CALIBRATION_KEEP: usize = 20;

/// Fold a verification into the estimate `cap` (in place):
/// - the throughput bias (measured / predicted) moves toward this observation (weight 0.3);
/// - a clean run (throttled ratio within the budget) at a peak above the ceiling's clean
///   level raises it; a throttled run below the ceiling's throttled level lowers it.
pub fn calibrate_capacity(cap: &mut Capacity, cal: &mut Calibration, obs: &Observation) {
    if obs.predicted_ops_per_s > 0.0 && obs.measured_ops_per_s > 0.0 {
        let r = (obs.measured_ops_per_s / obs.predicted_ops_per_s).clamp(0.2, 5.0);
        cal.ops_bias_ratio = if cal.observations_count == 0 || cal.ops_bias_ratio <= 0.0 {
            r
        } else {
            0.7 * cal.ops_bias_ratio + 0.3 * r
        };
        cal.observations_count += 1;
    }
    let peak = obs.peak_inflight_count as f64;
    if peak > 0.0 {
        let clean = obs.throttled_request_ratio <= obs.max_throttle_ratio;
        match cap.inflight_ceiling.as_mut() {
            Some(ce) if clean && peak > ce.lo => {
                ce.lo = peak;
                ce.requests = peak;
                ce.hi = ce.hi.max(peak + 1.0);
            }
            Some(ce) if !clean && peak < ce.hi => {
                ce.hi = peak.max(1.0);
                ce.lo = ce.lo.min((peak - 1.0).max(0.0));
                ce.requests = ce.lo;
            }
            None if !clean && peak > 1.0 => {
                cap.inflight_ceiling = Some(Ceiling {
                    requests: peak - 1.0,
                    lo: peak - 1.0,
                    hi: peak,
                });
                cap.confidence = Confidence::Low;
                cap.confidence_reason =
                    format!("throttled at {peak:.0} requests in flight in a verification run");
            }
            _ => {}
        }
    }
    cal.observations.push(obs.clone());
    if cal.observations.len() > CALIBRATION_KEEP {
        let n = cal.observations.len() - CALIBRATION_KEEP;
        cal.observations.drain(..n);
    }
    if cal.observations_count > 0 {
        cap.calibration_bias_ratio = Some(cal.ops_bias_ratio);
    }
}

/// Store a verification outcome in the history file at `path` and calibrate the remembered
/// capacity of `target` / `workload_hash` (starting from `base` when nothing is remembered).
/// Returns the calibrated capacity.
pub fn calibrate(
    path: &Path,
    target: &TargetId,
    workload_hash: &str,
    base: &Capacity,
    obs: &Observation,
) -> anyhow::Result<Capacity> {
    let mut h = History::load(path)?;
    let entry = h
        .targets
        .entry(target.to_string())
        .or_default()
        .entry(workload_hash.to_owned())
        .or_insert_with(|| {
            let mut c = base.clone();
            c.observed.levels.clear();
            HistoryEntry {
                capacity: c,
                run_id: obs.run_id.clone(),
                updated_at: String::new(),
                calibration: Calibration::default(),
            }
        });
    calibrate_capacity(&mut entry.capacity, &mut entry.calibration, obs);
    entry.updated_at = crate::util::now_rfc3339();
    let out = entry.capacity.clone();
    h.save(path)?;
    Ok(out)
}

/// `<data>/capacity.toml`: estimates keyed by target identity, then workload hash.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct History {
    #[serde(default)]
    pub targets: BTreeMap<String, BTreeMap<String, HistoryEntry>>,
}

impl History {
    /// Load `path` (missing: empty).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw).map_err(|e| {
                crate::errors::OprobeError::new(
                    crate::errors::ErrorCode::ConfigInvalid,
                    format!("invalid capacity history `{}`: {e}", path.display()),
                )
                .with_hint("delete the file; it is rebuilt by later runs")
                .into()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => {
                Err(anyhow::Error::from(e).context(format!("cannot read `{}`", path.display())))
            }
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        crate::store::atomic_write(path, text.as_bytes())
    }

    /// The remembered capacity of `target` for `workload_hash` (`source: history`).
    pub fn get(&self, target: &TargetId, workload_hash: &str) -> Option<Capacity> {
        let e = self.targets.get(&target.to_string())?.get(workload_hash)?;
        let mut c = e.capacity.clone();
        c.source = CapacitySource::History;
        if e.calibration.observations_count > 0 {
            c.calibration_bias_ratio = Some(e.calibration.ops_bias_ratio);
        }
        Some(c)
    }

    /// Remember `cap` (its per-level evidence is not kept).
    pub fn put(
        &mut self,
        target: &TargetId,
        workload_hash: &str,
        cap: &Capacity,
        run_id: Option<&str>,
    ) {
        let mut c = cap.clone();
        c.observed.levels.clear();
        let m = self.targets.entry(target.to_string()).or_default();
        // Verification outcomes outlive a new estimate; the bias keeps applying.
        let calibration = m
            .remove(workload_hash)
            .map(|e| e.calibration)
            .unwrap_or_default();
        if calibration.observations_count > 0 {
            c.calibration_bias_ratio = Some(calibration.ops_bias_ratio);
        }
        m.insert(
            workload_hash.to_owned(),
            HistoryEntry {
                capacity: c,
                run_id: run_id.map(str::to_owned),
                updated_at: crate::util::now_rfc3339(),
                calibration,
            },
        );
    }
}

/// The workload key a target's capacity is remembered under: the spec's workload hash with
/// the run's length left out (duration, warmup, `max_ops`). How long a run lasts does not
/// change how much load the target takes, so a verification that only lengthens the run (or
/// a replay with a shorter warmup) still finds the remembered ceiling.
pub fn history_key(spec: &crate::model::RunSpec) -> String {
    let mut s = spec.clone();
    let d = crate::config::WorkloadSpec::default();
    let w = &mut s.profile.workload;
    w.duration = d.duration;
    w.warmup = d.warmup;
    w.max_ops = None;
    s.workload_hash()
}

/// The remembered capacity of `target` / `workload_hash` in the history file at `path`.
pub fn load(path: &Path, target: &TargetId, workload_hash: &str) -> Option<Capacity> {
    History::load(path).ok()?.get(target, workload_hash)
}

/// Remember `cap` for `target` / `workload_hash` in the history file at `path` (only
/// estimates with a ceiling are worth keeping; others are ignored).
pub fn save(
    path: &Path,
    target: &TargetId,
    workload_hash: &str,
    cap: &Capacity,
    run_id: Option<&str>,
) -> anyhow::Result<()> {
    if cap.inflight_ceiling.is_none() {
        return Ok(());
    }
    let mut h = History::load(path)?;
    h.put(target, workload_hash, cap, run_id);
    h.save(path)
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
        assert_eq!(ce.requests, ce.lo);
        assert!(
            (c.per_op.peak_inflight_per_op - 1.0).abs() < 1e-9,
            "{:?}",
            c.per_op
        );
        assert!(c.per_op.sequential_fraction > 0.99, "{:?}", c.per_op);
        assert_eq!(
            c.safe_concurrency(0.15),
            Some((ce.lo * 0.85).floor() as u32)
        );
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
        // 24 × 0.85 / 4 = 5.1 → 5 workers, not 24 / 6.
        assert_eq!(c.safe_concurrency(DEFAULT_HEADROOM_RATIO), Some(5));
        let p = c.predict(5);
        assert_eq!(p.peak_inflight_count, 20);
        assert!(
            p.within_ceiling && p.throttle_probability_ratio < 0.05,
            "{p:?}"
        );
        let p = c.predict(8);
        assert!(
            !p.within_ceiling && p.throttle_probability_ratio > 0.9,
            "{p:?}"
        );
    }

    #[test]
    fn no_throttling_means_no_ceiling() {
        let (reqs, ops) = synthetic(8, 1000, &[1], 5, 20);
        let c = estimate(&reqs, &ops, Window::all());
        assert!(c.inflight_ceiling.is_none());
        assert!(c.safe_concurrency(0.15).is_none());
        assert_eq!(c.observed.peak_inflight_count, 8);
        assert!(
            c.confidence_reason.contains("no throttling"),
            "{}",
            c.confidence_reason
        );
        let p = c.predict(4);
        assert_eq!(p.throttle_probability_ratio, 0.0);
        assert!(p.ops_per_s > 0.0);
        // Twice the workers: twice the throughput (wide interval), unknown throttling.
        let p2 = c.predict(16);
        assert!(p2.ops_per_s > p.ops_per_s * 3.5, "{p:?} {p2:?}");
        assert!(p2.ops_lo_per_s < p2.ops_per_s && p2.ops_hi_per_s > p2.ops_per_s);
        assert!(p2.throttle_probability_ratio > 0.0);
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
        assert!(c.safe_rate(0.15).is_none());
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

    #[test]
    fn calibrate_adjusts_the_ceiling_and_the_bias() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capacity.toml");
        let (reqs, ops) = synthetic(30, 24, &[1], 5, 30);
        let c = estimate(&reqs, &ops, Window::all());
        let lo = c.inflight_ceiling.unwrap().lo;
        let t = TargetId::parse("s3://bench/data").unwrap();
        let p = c.predict(10);
        let obs = Observation {
            code: "throttling".into(),
            outcome: crate::analysis::verification::VerifyOutcome::Confirmed,
            concurrency_count: 10,
            predicted_ops_per_s: p.ops_per_s,
            measured_ops_per_s: p.ops_per_s * 0.5,
            peak_inflight_count: lo as u64 + 3,
            throttled_request_ratio: 0.0,
            max_throttle_ratio: 0.001,
            ..Default::default()
        };
        let after = calibrate(&path, &t, "sha256:aa", &c, &obs).unwrap();
        // A clean run above the clean level raises it.
        assert_eq!(after.inflight_ceiling.unwrap().lo, lo + 3.0);
        assert!((after.calibration_bias_ratio.unwrap() - 0.5).abs() < 1e-9);
        let back = load(&path, &t, "sha256:aa").unwrap();
        assert!(
            (back.predict(10).ops_per_s - p.ops_per_s * 0.5).abs() < 1e-6 * p.ops_per_s.max(1.0)
        );
        // A throttled run below the throttled level lowers it.
        let obs2 = Observation {
            peak_inflight_count: 20,
            throttled_request_ratio: 0.05,
            measured_ops_per_s: p.ops_per_s,
            ..obs
        };
        let after = calibrate(&path, &t, "sha256:aa", &c, &obs2).unwrap();
        let ce = after.inflight_ceiling.unwrap();
        assert_eq!(ce.hi, 20.0);
        assert!(ce.lo <= 19.0);
        let h = History::load(&path).unwrap();
        let e = &h.targets[&t.to_string()]["sha256:aa"];
        assert_eq!(e.calibration.observations.len(), 2);
        assert!((e.calibration.ops_bias_ratio - (0.7 * 0.5 + 0.3)).abs() < 1e-9);
        // A new estimate keeps the outcomes.
        save(&path, &t, "sha256:aa", &c, Some("run-2")).unwrap();
        let h = History::load(&path).unwrap();
        assert_eq!(
            h.targets[&t.to_string()]["sha256:aa"]
                .calibration
                .observations
                .len(),
            2
        );
    }

    #[test]
    fn a_run_under_the_ceiling_keeps_the_remembered_one() {
        let (reqs, ops) = synthetic(30, 24, &[1], 5, 60);
        let throttled = estimate(&reqs, &ops, Window::all());
        let (reqs, ops) = synthetic(20, 24, &[1], 5, 60);
        let clean = estimate(&reqs, &ops, Window::all());
        assert!(clean.inflight_ceiling.is_none());
        let merged = clean.clone().with_history(Some(throttled.clone()));
        assert_eq!(merged.inflight_ceiling, throttled.inflight_ceiling);
        assert_eq!(merged.source, CapacitySource::History);
        assert_eq!(merged.per_op, clean.per_op);
        assert_eq!(
            merged.safe_concurrency(0.15),
            throttled.safe_concurrency(0.15)
        );
        assert!(merged.confidence_reason.contains("remembered"));
        // Its own ceiling wins; no memory changes nothing.
        let own = throttled.clone().with_history(Some(clean.clone()));
        assert_eq!(own.source, CapacitySource::ThisRun);
        assert_eq!(clean.clone().with_history(None), clean);
        // Clean arrivals above the remembered limit: the memory is stale.
        let (reqs, ops) = synthetic(40, 1000, &[1], 5, 30);
        let bigger = estimate(&reqs, &ops, Window::all());
        let stale = bigger.clone().with_history(Some(throttled));
        assert!(stale.inflight_ceiling.is_none(), "{stale:#?}");
        assert_eq!(stale.max_advisable_concurrency(), None);
    }

    #[test]
    fn history_key_ignores_the_run_length() {
        use crate::model::{Overrides, RunSpec};
        let s = RunSpec::default();
        let longer = s.with(&Overrides::new().with_set("duration", "60s"));
        let warm = s.with(&Overrides::new().with_set("warmup", "0s"));
        assert_ne!(s.workload_hash(), longer.workload_hash());
        assert_eq!(history_key(&s), history_key(&longer));
        assert_eq!(history_key(&s), history_key(&warm));
        let other = s.with(&Overrides::new().with_set("objects", "7"));
        assert_ne!(history_key(&s), history_key(&other));
    }

    #[test]
    fn history_is_keyed_by_target_and_workload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capacity.toml");
        let (reqs, ops) = synthetic(30, 24, &[1], 5, 30);
        let c = estimate(&reqs, &ops, Window::all());
        let t = TargetId::parse("s3://bench/data").unwrap();
        assert!(load(&path, &t, "sha256:aa").is_none());
        save(&path, &t, "sha256:aa", &c, Some("run-1")).unwrap();
        let back = load(&path, &t, "sha256:aa").expect("saved");
        assert_eq!(back.source, CapacitySource::History);
        assert_eq!(back.inflight_ceiling, c.inflight_ceiling);
        assert!(back.observed.levels.is_empty());
        assert!(load(&path, &t, "sha256:bb").is_none());
        let other = TargetId::parse("s3://bench/other").unwrap();
        assert!(load(&path, &other, "sha256:aa").is_none());
        // Estimates without a ceiling are not remembered.
        save(&path, &other, "sha256:aa", &Capacity::default(), None).unwrap();
        assert!(load(&path, &other, "sha256:aa").is_none());
    }
}
