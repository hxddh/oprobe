//! A time range of a run, recomputed from its trace rows (design v6.3 B5, B6): the key
//! numbers, the requests by API, the verbs, the latency distribution and the slowest
//! operations of `[t0, t1)`, and the drill-down queries behind an aggregate (a heatmap cell,
//! an API). `oprobe show --since/--until` and the app's brushed range read the same
//! [`RangeView`].
//!
//! **What counts.** An operation counts in the range it started in (as the timeline's
//! goodput and the heatmap); a request in the range it was sent in.
//!
//! **Open loop.** An arrival that never started has no row: the timeline counts it in the
//! interval it was due in. A range counts those of its intervals (`missed_arrivals_count`) in
//! its percentiles as the whole run does, each at the least it can have taken, and says so
//! (`latency_at_least`): a range of an overloaded run does not read faster than the run.
//!
//! **Sampled rows.** When the run kept only a share of its rows, each kept row stands for
//! [`Run::op_weights`] operations (failed, throttled and the slowest rows stand for
//! themselves): counts are weighted estimates and the quantiles weighted, so the always-kept
//! tail is not over-weighed. A request carries its operation's weight.

use std::collections::{BTreeMap, HashMap};

use schemars::JsonSchema;
use serde::Serialize;

use super::view::{ApiRow, LatencyCdf, StageShare, VerbView};
use crate::model::{OpRow, ReqRow, Run};
/// `num / den` (0 without a denominator), never −0 (an empty `f64` sum is −0).
fn ratio(num: f64, den: f64) -> f64 {
    crate::util::ratio(num, den) + 0.0
}

/// The numbers of a time range of a run, from its trace rows.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct RangeView {
    /// The range, seconds from the window start (clamped to the window).
    pub since_s: f64,
    pub until_s: f64,
    /// The run kept only a share of its rows: counts are weighted estimates.
    pub sampled: bool,
    /// Kept / seen operation rows of the run (1: none sampled away).
    pub kept_ratio: f64,
    /// Operation rows in the range (what the estimates rest on).
    pub rows_count: u64,
    /// Operations started in the range (weighted when sampled).
    pub ops_count: u64,
    /// Operations started in the range that failed (an operation started earlier that times
    /// out inside the range counts in the range it started in).
    pub failed_ops_count: u64,
    /// Failed / every operation started in the range.
    pub error_ratio: f64,
    /// Operations started per second, failed ones included.
    pub ops_per_s: f64,
    /// Successful operations started per second.
    pub goodput_ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    /// The slowest operation that started in the range.
    pub max_ms: f64,
    /// Open-loop arrivals due in the range that never started (the timeline's count of each
    /// interval, by the share of it the range covers).
    #[serde(skip_serializing_if = "is_zero")]
    pub missed_arrivals_count: u64,
    /// The percentiles are lower bounds ("at least"): they count the arrivals that never
    /// started, each at the least it can have taken.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub latency_at_least: bool,
    /// Requests sent in the range, retries included.
    pub requests_count: u64,
    pub throttled_requests_count: u64,
    pub throttled_request_ratio: f64,
    pub retries_count: u64,
    /// Where the operations' time went: `queue`, `cache`, `server`, `retry`, `client`.
    pub stages: Vec<StageShare>,
    /// The requests of each API, busiest first.
    pub apis: Vec<ApiRow>,
    /// Each verb, busiest first.
    pub verbs: Vec<VerbView>,
    /// The slowest operations of the range, slowest first (at most [`RangeView::SLOWEST`]).
    pub slowest_op_ids: Vec<u64>,
    /// The latency distribution of the range (not serialized: the quantiles are).
    #[serde(skip)]
    #[schemars(skip)]
    pub cdf: LatencyCdf,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The least an open-loop arrival that never started can have waited, when it was dropped:
/// the engine drops an arrival once it is more than the operator's timeout, and never less
/// than a second, behind its schedule (`engine::runner`).
const DROPPED_AFTER_US: f64 = 1e6;

/// The open-loop arrivals due in `[lo, hi)` that never started, as `(lower bound µs, count)`
/// per timeline interval. The timeline counts them in the interval they were due in; an
/// interval the range covers in part gives that share of its count. Each was either dropped
/// behind its schedule (it had waited more than [`DROPPED_AFTER_US`]) or still waiting when
/// the window ended (it had waited at least from its interval's end to the window's): its
/// latency is at least the smaller of the two.
fn missed_arrivals(run: &Run, lo: f64, hi: f64) -> Vec<(u64, f64)> {
    let window = run.report.run.window_s;
    run.report
        .timeline
        .iter()
        .filter(|p| p.missed_arrivals > 0)
        .filter_map(|p| {
            let len = if p.len_s > 0.0 { p.len_s } else { 1.0 };
            let end = p.t_s + len;
            let covered = (hi.min(end) - lo.max(p.t_s)).max(0.0) / len;
            let count = p.missed_arrivals as f64 * covered;
            let waited_us = ((window - end).max(0.0) * 1e6).min(DROPPED_AFTER_US);
            (count > 0.0).then_some((waited_us.max(1.0) as u64, count))
        })
        .collect()
}

/// Durations (µs) with the weight each stands for, sorted once: every quantile and the
/// distribution of a range, a verb or an API are read from the one order (a view over a large
/// trace sorted its rows again for each quantile, then for the distribution and the slowest
/// operations).
#[derive(Clone, Debug)]
pub(crate) struct Weighted {
    /// `(µs, weight)`, ascending by µs.
    sorted: Vec<(u64, f64)>,
    /// The sum of the weights (a negative weight counts for nothing).
    total: f64,
}

#[cfg(test)]
thread_local! {
    /// Sorts made by [`Weighted::new`] on this thread (the tests bound them).
    static SORTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Weighted {
    pub(crate) fn new(mut v: Vec<(u64, f64)>) -> Self {
        #[cfg(test)]
        SORTS.with(|n| n.set(n.get() + 1));
        v.sort_unstable_by_key(|x| x.0);
        let total = v.iter().map(|x| x.1.max(0.0)).sum();
        Self { sorted: v, total }
    }

    /// The weighted nearest-rank quantile `q` in ms, as
    /// [`crate::analysis::stats::weighted_quantile_us_ms`] defines it: the smallest value
    /// whose cumulative weight reaches `q` of the total. 0 for no weight.
    pub(crate) fn quantile_ms(&self, q: f64) -> f64 {
        if self.sorted.is_empty() || self.total <= 0.0 {
            return 0.0;
        }
        let target = self.total * q.clamp(0.0, 1.0);
        let mut acc = 0.0;
        for (us, w) in &self.sorted {
            acc += w.max(0.0);
            if acc + 1e-9 * self.total >= target && acc > 0.0 {
                return *us as f64 / 1000.0;
            }
        }
        self.sorted.last().map_or(0.0, |x| x.0 as f64 / 1000.0)
    }

    /// The weighted distribution, at most [`LatencyCdf::MAX_POINTS`] points.
    pub(crate) fn cdf(&self, exact: bool) -> LatencyCdf {
        let total: f64 = self.sorted.iter().map(|x| x.1).sum();
        if self.sorted.is_empty() || total <= 0.0 {
            return LatencyCdf::default();
        }
        let n = self.sorted.len();
        let step = n.div_ceil(LatencyCdf::MAX_POINTS).max(1);
        let mut acc = 0.0;
        let mut pts = Vec::new();
        for (i, (us, w)) in self.sorted.iter().enumerate() {
            acc += w;
            if (i + 1) % step == 0 || i + 1 == n {
                pts.push((*us as f64 / 1000.0, (acc / total).min(1.0)));
            }
        }
        if let Some(last) = pts.last_mut() {
            last.1 = 1.0;
        }
        LatencyCdf { pts, exact }
    }
}

/// The first `n` of `items` in the order of `key` (equal ones in the order given, as a stable
/// sort leaves them): a selection, not a sort of every row.
fn first_n<T, K: Ord>(items: Vec<T>, n: usize, key: impl Fn(&T) -> K) -> Vec<T> {
    if n == 0 {
        return Vec::new();
    }
    let mut v: Vec<(usize, T)> = items.into_iter().enumerate().collect();
    let order = |x: &(usize, T)| (key(&x.1), x.0);
    if v.len() > n {
        v.select_nth_unstable_by_key(n - 1, order);
        v.truncate(n);
    }
    v.sort_unstable_by_key(order);
    v.into_iter().map(|x| x.1).collect()
}

impl RangeView {
    pub const SLOWEST: usize = 5;

    /// The range `[since_s, until_s)` of `run` (seconds from the window start).
    pub fn of(run: &Run, since_s: f64, until_s: f64) -> Self {
        let window = run.report.run.window_s.max(0.0);
        let lo = since_s.clamp(0.0, window.max(since_s.max(0.0)));
        let hi = until_s.max(lo);
        let hi = if window > 0.0 { hi.min(window) } else { hi };
        let span = (hi - lo).max(1e-3);
        let (lo_us, hi_us) = ((lo * 1e6) as i64, (hi * 1e6) as i64);
        let inside = |t: i64| t >= lo_us && t < hi_us;
        let w = run.op_weights();
        let weight = |id: u64| w.get(&id).copied().unwrap_or(1.0);
        let s = &run.report.sampling;
        let sampled = s.seen_ops_count > 0 && s.kept_ops_count < s.seen_ops_count;

        let ops: Vec<&OpRow> = run.ops.iter().filter(|o| inside(o.t_us)).collect();
        let reqs: Vec<&ReqRow> = run.reqs.iter().filter(|q| inside(q.t_us)).collect();
        let total_w: f64 = ops.iter().map(|o| weight(o.op_id)).sum();
        // An operation counts in the range it started in (docs/cli.md), failed or not: the
        // failures, the goodput and the verbs describe the same operations.
        let failed_w: f64 = ops.iter().filter(|o| !o.ok).map(|o| weight(o.op_id)).sum();
        let bytes_w: f64 = ops.iter().map(|o| o.bytes as f64 * weight(o.op_id)).sum();
        // The arrivals that never started count in the percentiles at their lower bound, as
        // in the whole run's (no coordinated omission).
        let missed = missed_arrivals(run, lo, hi);
        let missed_w: f64 = missed.iter().map(|m| m.1).sum();
        let missed_count = missed_w.round() as u64;
        let lat = Weighted::new(
            ops.iter()
                .map(|o| (o.latency_us, weight(o.op_id)))
                .chain(missed.into_iter().filter(|_| missed_count > 0))
                .collect(),
        );
        let req_w = |q: &ReqRow| weight(q.op_id);
        let requests_w: f64 = reqs.iter().map(|q| req_w(q)).sum();
        let throttled_w: f64 = reqs
            .iter()
            .filter(|q| q.is_throttle())
            .map(|q| req_w(q))
            .sum();
        let retries_w: f64 = reqs
            .iter()
            .filter(|q| q.attempt > 1)
            .map(|q| req_w(q))
            .sum();
        Self {
            since_s: lo,
            until_s: hi,
            sampled,
            kept_ratio: if s.seen_ops_count == 0 {
                1.0
            } else {
                s.kept_ratio
            },
            rows_count: ops.len() as u64,
            ops_count: total_w.round() as u64,
            failed_ops_count: failed_w.round() as u64,
            error_ratio: ratio(failed_w, total_w),
            ops_per_s: total_w / span,
            goodput_ops_per_s: (total_w - failed_w).max(0.0) / span,
            mib_per_s: bytes_w / span / (1024.0 * 1024.0),
            p50_ms: lat.quantile_ms(0.5),
            p90_ms: lat.quantile_ms(0.9),
            p99_ms: lat.quantile_ms(0.99),
            p999_ms: lat.quantile_ms(0.999),
            max_ms: ops.iter().map(|o| o.latency_us).max().unwrap_or(0) as f64 / 1000.0,
            missed_arrivals_count: missed_count,
            latency_at_least: missed_count > 0,
            requests_count: requests_w.round() as u64,
            throttled_requests_count: throttled_w.round() as u64,
            throttled_request_ratio: ratio(throttled_w, requests_w),
            retries_count: retries_w.round() as u64,
            stages: stages(&ops, &weight),
            apis: apis(&reqs, &weight),
            verbs: verbs(&ops, &weight, span),
            slowest_op_ids: first_n(ops.clone(), Self::SLOWEST, |o| {
                std::cmp::Reverse(o.latency_us)
            })
            .iter()
            .map(|o| o.op_id)
            .collect(),
            cdf: lat.cdf(!sampled && missed_count == 0),
        }
    }

    /// Nothing started in the range.
    pub fn is_empty(&self) -> bool {
        self.rows_count == 0
    }

    /// The range's length.
    pub fn span_s(&self) -> f64 {
        self.until_s - self.since_s
    }
}

/// Shares of the operations' time by stage (the verbs' keys); empty when nothing was timed.
fn stages(ops: &[&OpRow], weight: &dyn Fn(u64) -> f64) -> Vec<StageShare> {
    let mut parts = [
        ("queue", "queue", 0.0),
        ("cache", "cache", 0.0),
        ("server", "backend", 0.0),
        ("retry", "retry backoff", 0.0),
        ("client", "client", 0.0),
    ];
    for o in ops {
        let w = weight(o.op_id);
        let b = &o.breakdown;
        for (i, us) in [
            b.queue_us,
            b.cache_us,
            b.backend_us,
            b.retry_us,
            b.client_us,
        ]
        .into_iter()
        .enumerate()
        {
            parts[i].2 += us as f64 * w;
        }
    }
    let total: f64 = parts.iter().map(|p| p.2).sum();
    if total <= 0.0 {
        return Vec::new();
    }
    parts
        .iter()
        .map(|(k, t, us)| StageShare {
            key: (*k).to_owned(),
            title: (*t).to_owned(),
            ratio: us / total,
        })
        .collect()
}

fn apis(reqs: &[&ReqRow], weight: &dyn Fn(u64) -> f64) -> Vec<ApiRow> {
    let mut by: BTreeMap<&str, Vec<&ReqRow>> = BTreeMap::new();
    for q in reqs {
        by.entry(q.api.label()).or_default().push(q);
    }
    let mut v: Vec<ApiRow> = by
        .into_iter()
        .map(|(api, qs)| {
            let w = |q: &ReqRow| weight(q.op_id);
            let n: f64 = qs.iter().map(|q| w(q)).sum();
            let mut status: HashMap<u16, f64> = HashMap::new();
            for q in &qs {
                *status.entry(q.status).or_default() += w(q);
            }
            let mut status: Vec<(u16, u64)> = status
                .into_iter()
                .map(|(k, c)| (k, c.round() as u64))
                .collect();
            status.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let ttfb = Weighted::new(qs.iter().map(|q| (q.ttfb_us, w(q))).collect());
            let total = Weighted::new(qs.iter().map(|q| (q.total_us, w(q))).collect());
            let sum = |f: &dyn Fn(&ReqRow) -> f64| qs.iter().map(|q| f(q) * w(q)).sum::<f64>();
            let connected = sum(&|q| f64::from(u8::from(q.status != 0 || q.connect_us() > 0)));
            let is_err = |q: &ReqRow| {
                q.is_error() && q.status != 404 && !q.detail.as_ref().is_some_and(|d| d.cut)
            };
            ApiRow {
                api: api.to_owned(),
                requests_count: n.round() as u64,
                errors_count: sum(&|q| f64::from(u8::from(is_err(q)))).round() as u64,
                throttled_count: sum(&|q| f64::from(u8::from(q.is_throttle()))).round() as u64,
                status,
                reused_ratio: ratio(sum(&|q| f64::from(u8::from(q.reused))), connected),
                up_bytes: sum(&|q| q.bytes_up as f64).round() as u64,
                down_bytes: sum(&|q| q.bytes_down as f64).round() as u64,
                ttfb_p50_ms: ttfb.quantile_ms(0.5),
                ttfb_p99_ms: ttfb.quantile_ms(0.99),
                total_p50_ms: total.quantile_ms(0.5),
                total_p99_ms: total.quantile_ms(0.99),
            }
        })
        .collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.requests_count));
    v
}

fn verbs(ops: &[&OpRow], weight: &dyn Fn(u64) -> f64, span: f64) -> Vec<VerbView> {
    let mut by: BTreeMap<&str, Vec<&OpRow>> = BTreeMap::new();
    for o in ops {
        by.entry(o.verb.as_ref()).or_default().push(o);
    }
    let mut v: Vec<VerbView> = by
        .into_iter()
        .map(|(verb, os)| {
            let n: f64 = os.iter().map(|o| weight(o.op_id)).sum();
            let share = |f: &dyn Fn(&OpRow) -> bool| {
                ratio(os.iter().filter(|o| f(o)).map(|o| weight(o.op_id)).sum(), n)
            };
            let lat = Weighted::new(os.iter().map(|o| (o.latency_us, weight(o.op_id))).collect());
            VerbView {
                verb: verb.to_owned(),
                ops_count: n.round() as u64,
                ops_per_s: n / span,
                mib_per_s: os
                    .iter()
                    .map(|o| o.bytes as f64 * weight(o.op_id))
                    .sum::<f64>()
                    / span
                    / (1024.0 * 1024.0),
                p50_ms: lat.quantile_ms(0.5),
                p99_ms: lat.quantile_ms(0.99),
                error_ratio: share(&|o| !o.ok),
                throttled_ratio: share(&|o| o.throttled),
                requests_per_op: ratio(
                    os.iter()
                        .map(|o| f64::from(o.requests) * weight(o.op_id))
                        .sum(),
                    n,
                ),
                stages: stages(&os, weight),
            }
        })
        .collect();
    v.sort_by_key(|v| std::cmp::Reverse(v.ops_count));
    v
}

/// One operation as a drill-down lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct OpLine {
    pub op_id: u64,
    pub verb: String,
    pub key: String,
    /// Start, µs from the window start.
    pub t_us: i64,
    pub latency_us: u64,
    pub ok: bool,
    /// The status of its last request (0: no response, or no request rows).
    pub status: u16,
    /// 1 + retries.
    pub attempts: u32,
    pub throttled: bool,
}

impl OpLine {
    fn of(run: &Run, o: &OpRow) -> Self {
        Self {
            op_id: o.op_id,
            verb: o.verb.to_string(),
            key: o.key.clone(),
            t_us: o.t_us,
            latency_us: o.latency_us,
            ok: o.ok,
            status: run.requests_of(o.op_id).last().map_or(0, |q| q.status),
            attempts: o.attempts,
            throttled: o.throttled,
        }
    }
}

/// The slowest operations (at most `n`) that started in `[since_s, until_s)` with a latency
/// in `[lo_ms, hi_ms)` — the operations behind a heatmap cell, slowest first. (Rows only:
/// when the run was sampled, these are the kept ones.)
pub fn ops_in(
    run: &Run,
    since_s: f64,
    until_s: f64,
    lo_ms: f64,
    hi_ms: f64,
    n: usize,
) -> Vec<OpLine> {
    let (t0, t1) = ((since_s * 1e6) as i64, (until_s * 1e6) as i64);
    let (l0, l1) = (lo_ms * 1000.0, hi_ms * 1000.0);
    let v: Vec<&OpRow> = run
        .ops
        .iter()
        .filter(|o| o.t_us >= t0 && o.t_us < t1)
        .filter(|o| (o.latency_us as f64) >= l0 && (o.latency_us as f64) < l1)
        .collect();
    first_n(v, n, |o| std::cmp::Reverse(o.latency_us))
        .into_iter()
        .map(|o| OpLine::of(run, o))
        .collect()
}

/// One request as a drill-down lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReqLine {
    pub op_id: u64,
    /// The operation's key (empty when its row was not kept).
    pub key: String,
    pub api: String,
    pub status: u16,
    pub attempt: u32,
    /// Sent, µs from the window start.
    pub t_us: i64,
    pub ttfb_us: u64,
    pub total_us: u64,
    /// The provider's error code (`SlowDown`), when the response had one.
    pub provider_code: Option<String>,
}

/// The slowest requests (at most `n`) of `api` (`GetObject`), in the window or in
/// `[since_s, until_s)` when given — the requests behind a requests-by-API row.
pub fn slowest_requests(run: &Run, api: &str, range: Option<(f64, f64)>, n: usize) -> Vec<ReqLine> {
    let inside = |t: i64| match range {
        Some((a, b)) => t >= (a * 1e6) as i64 && t < (b * 1e6) as i64,
        None => t >= 0,
    };
    let v: Vec<&ReqRow> = run
        .reqs
        .iter()
        .filter(|q| (q.api.label() == api || q.api_name() == api) && inside(q.t_us))
        .collect();
    first_n(v, n, |q| std::cmp::Reverse(q.total_us))
        .into_iter()
        .map(|q| ReqLine {
            op_id: q.op_id,
            key: run.op(q.op_id).map(|o| o.key.clone()).unwrap_or_default(),
            api: q.api.label().to_owned(),
            status: q.status,
            attempt: q.attempt,
            t_us: q.t_us,
            ttfb_us: q.ttfb_us,
            total_us: q.total_us,
            provider_code: q.detail.as_ref().and_then(|d| d.error_code.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::fixture;
    use std::borrow::Cow;

    /// Ten operations a second for 10 s: op `i` starts at `i × 100 ms` and takes
    /// `(i % 10 + 1) ms`, the ones in second 6 take 100 ms more and the second request of
    /// every fifth was throttled first.
    fn run() -> Run {
        let mut run = fixture::run();
        run.ops.clear();
        run.reqs.clear();
        for i in 0..100u64 {
            let t_us = i as i64 * 100_000;
            let slow = (60..70).contains(&i);
            let latency_us = (i % 10 + 1) * 1000 + if slow { 100_000 } else { 0 };
            let throttled = i % 5 == 0;
            run.ops.push(OpRow {
                op_id: i + 1,
                verb: Cow::Borrowed(if i % 2 == 0 { "read" } else { "stat" }),
                key: format!("k{i}"),
                t_us,
                latency_us,
                bytes: 1024 * 1024,
                ok: i != 61,
                error_code: None,
                error: None,
                throttled,
                attempts: if throttled { 2 } else { 1 },
                breakdown: crate::model::OpBreakdownUs {
                    backend_us: latency_us,
                    ..Default::default()
                },
                requests: if throttled { 2 } else { 1 },
                await_tree: None,
                weight: 1.0,
            });
            let mut first = fixture::req(i + 1, t_us, if throttled { 503 } else { 200 }, true);
            first.total_us = latency_us / 2;
            first.ttfb_us = latency_us / 4;
            run.reqs.push(first);
            if throttled {
                let mut again = fixture::req(i + 1, t_us + 500, 200, true);
                again.attempt = 2;
                again.total_us = latency_us / 2;
                run.reqs.push(again);
            }
        }
        run.report.sampling = Default::default();
        run
    }

    #[test]
    fn a_range_counts_and_ranks_its_own_operations() {
        let run = run();
        // [3 s, 8 s): ops 30..80, fifty of them.
        let v = RangeView::of(&run, 3.0, 8.0);
        assert!(!v.sampled);
        assert_eq!((v.rows_count, v.ops_count), (50, 50));
        assert_eq!(v.failed_ops_count, 1);
        assert!((v.ops_per_s - 10.0).abs() < 1e-9);
        assert!((v.goodput_ops_per_s - 49.0 / 5.0).abs() < 1e-9);
        assert!((v.mib_per_s - 10.0).abs() < 1e-9);
        // Latencies: 1..10 ms five times each, ten of them 100 ms more. p50 is the 25th of
        // the sorted fifty: 40 fast ones are 1..10 ms four times each → 25th = 7 ms.
        assert_eq!(v.p50_ms, 7.0);
        // The p90 is the 45th: among the ten slow ones (101..110 ms), the fifth: 105 ms.
        assert_eq!(v.p90_ms, 105.0);
        assert_eq!(v.max_ms, 110.0);
        // Requests: 50 first attempts and 10 retries (every fifth op), 10 throttled.
        assert_eq!(v.requests_count, 60);
        assert_eq!(v.retries_count, 10);
        assert_eq!(v.throttled_requests_count, 10);
        assert!((v.throttled_request_ratio - 10.0 / 60.0).abs() < 1e-9);
        assert_eq!(v.apis.len(), 1);
        assert_eq!(v.apis[0].requests_count, 60);
        assert_eq!(v.apis[0].status[0], (200, 50));
        assert_eq!(v.verbs.len(), 2);
        assert_eq!(v.verbs[0].ops_count, 25);
        assert_eq!(v.slowest_op_ids[0], 70, "op 69 (id 70) is 110 ms");
        assert_eq!(v.cdf.pts.last().unwrap(), &(110.0, 1.0));
        assert!(v.cdf.exact);
        assert_eq!(
            v.stages.iter().find(|s| s.key == "server").unwrap().ratio,
            1.0
        );
        // The whole window, and an empty range.
        assert_eq!(RangeView::of(&run, 0.0, 10.0).ops_count, 100);
        assert!(RangeView::of(&run, 20.0, 30.0).is_empty());
    }

    /// K2-11. A range of an open-loop run read its percentiles from the operations that
    /// started, and said nothing of the arrivals that never did: a second in which the
    /// service fell behind read as fast as the rest. The range counts the missed arrivals
    /// of its intervals, in its percentiles too (each at the least it waited), and marks
    /// them "at least", as the whole run's are.
    #[test]
    fn a_range_of_an_open_loop_run_counts_its_missed_arrivals() {
        let mut run = run();
        // One timeline interval a second; in seconds 6 and 7 ten arrivals each never started.
        run.report.timeline = (0..10)
            .map(|t| crate::trace::TimelinePoint {
                t_s: t as f64,
                len_s: 1.0,
                ops_count: 10,
                missed_arrivals: if t == 6 || t == 7 { 10 } else { 0 },
                ..Default::default()
            })
            .collect();
        // A range without them is as before: exact, nothing "at least".
        let calm = RangeView::of(&run, 0.0, 5.0);
        assert_eq!(calm.missed_arrivals_count, 0);
        assert!(!calm.latency_at_least && calm.cdf.exact);
        let json = serde_json::to_value(&calm).unwrap();
        assert!(
            json.get("missed_arrivals_count").is_none() && json.get("latency_at_least").is_none()
        );
        // [5 s, 8 s): thirty operations started (ten of them 101..110 ms), twenty arrivals
        // never did. Of the fifty scheduled, the p50 is the 25th: a started one; the p90 is
        // the 45th: past the thirty that started, a missed arrival at its lower bound (they
        // were dropped after waiting at least a second).
        let v = RangeView::of(&run, 5.0, 8.0);
        assert_eq!((v.ops_count, v.missed_arrivals_count), (30, 20));
        assert!(v.latency_at_least && !v.cdf.exact);
        assert_eq!(v.p50_ms, 105.0);
        assert_eq!(v.p90_ms, 1000.0);
        assert_eq!(v.p99_ms, 1000.0);
        // 9.2's numbers, from the rows alone: a p90 of 107 ms beside twenty lost arrivals.
        let rows_only = {
            let mut r = run.clone();
            r.report
                .timeline
                .iter_mut()
                .for_each(|p| p.missed_arrivals = 0);
            RangeView::of(&r, 5.0, 8.0)
        };
        assert_eq!((rows_only.p50_ms, rows_only.p90_ms), (8.0, 107.0));
        // The slowest operation is of those that started.
        assert_eq!(v.max_ms, 110.0);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["missed_arrivals_count"], 20);
        assert_eq!(json["latency_at_least"], true);
        // A range that covers part of an interval counts that share of its arrivals.
        assert_eq!(RangeView::of(&run, 6.5, 7.0).missed_arrivals_count, 5);
        // An arrival still waiting when the window ended had waited at least from its
        // interval's end: the last interval's bound is no second.
        run.report.timeline[9].missed_arrivals = 40;
        let end = RangeView::of(&run, 9.0, 10.0);
        assert_eq!((end.ops_count, end.missed_arrivals_count), (10, 40));
        assert!(end.latency_at_least && end.p50_ms < 1.0, "{}", end.p50_ms);
        // The text says so.
        let text = crate::report::terminal::range_section(&run, &v);
        assert!(text.contains("p50 ≥ 105"), "{text}");
        assert!(text.contains("20 open-loop arrivals"), "{text}");
        let text = crate::report::terminal::range_section(&run, &calm);
        assert!(!text.contains('≥') && !text.contains("arrivals"), "{text}");
    }

    #[test]
    fn a_range_counts_failures_by_when_they_started_as_its_goodput_does() {
        let mut run = run();
        // Op 3 started at 0.2 s; it now retries until it fails at 7.4 s.
        let o = run.ops.iter_mut().find(|o| o.op_id == 3).unwrap();
        o.latency_us = 7_200_000;
        o.ok = false;
        // It counts where it started (docs/cli.md: "an operation counts in the range it
        // started in"), with the goodput and the verbs of the same operations.
        let early = RangeView::of(&run, 0.0, 1.0);
        assert_eq!(early.ops_count, 10);
        assert_eq!(early.failed_ops_count, 1);
        assert!(
            (early.error_ratio - 0.1).abs() < 1e-9,
            "{}",
            early.error_ratio
        );
        assert!((early.goodput_ops_per_s - 9.0).abs() < 1e-9);
        // [7, 8) started ten operations, all served: no failure there, whatever ended there.
        let late = RangeView::of(&run, 7.0, 8.0);
        assert_eq!(late.failed_ops_count, 0);
        assert_eq!(late.error_ratio, 0.0);
        assert!((late.goodput_ops_per_s - 10.0).abs() < 1e-9);
        // Failures, goodput and the verbs never contradict each other.
        for (a, b) in [(0.0, 1.0), (3.0, 8.0), (6.0, 7.0), (0.0, 10.0), (2.5, 9.5)] {
            let v = RangeView::of(&run, a, b);
            let good = v.goodput_ops_per_s * v.span_s();
            assert!(
                (v.failed_ops_count as f64 + good - v.ops_count as f64).abs() < 1e-6,
                "[{a}, {b}): {v:?}"
            );
            let verb_errors: f64 = v
                .verbs
                .iter()
                .map(|x| x.error_ratio * x.ops_count as f64)
                .sum();
            assert_eq!(verb_errors.round() as u64, v.failed_ops_count, "[{a}, {b})");
        }
    }

    #[test]
    fn a_sampled_range_weighs_each_row_by_what_it_stands_for() {
        let mut run = run();
        // Keep the throttled and failed rows and every other one of the rest: the run saw
        // 200 operations (twice as many clean ones as kept).
        run.ops.retain(|o| o.throttled || !o.ok || o.op_id % 2 == 0);
        let kept = run.ops.len() as u64;
        let forced = run.ops.iter().filter(|o| o.throttled || !o.ok).count() as u64;
        let clean_kept = kept - forced;
        run.report.sampling.seen_ops_count = forced + 2 * clean_kept;
        run.report.sampling.kept_ops_count = kept;
        run.report.sampling.kept_ratio = kept as f64 / run.report.sampling.seen_ops_count as f64;
        run.report.timeline.clear();
        let v = RangeView::of(&run, 0.0, 10.0);
        assert!(v.sampled);
        assert_eq!(v.rows_count, kept);
        // Each clean row stands for two operations; the forced ones for themselves.
        assert_eq!(v.ops_count, forced + 2 * clean_kept);
        assert!(!v.cdf.exact);
        // Unweighted, the forced rows (every fifth, all fast-or-slow alike) would be a
        // larger share; weighted, the median is a clean operation's.
        let mut plain: Vec<u64> = run.ops.iter().map(|o| o.latency_us).collect();
        let unweighted = crate::analysis::stats::quantile_us_ms(&mut plain, 0.99);
        assert!(v.p99_ms <= unweighted);
    }

    #[test]
    fn drill_downs_list_the_operations_behind_a_cell_and_an_api() {
        let run = run();
        let cell = ops_in(&run, 6.0, 7.0, 100.0, 200.0, 3);
        assert_eq!(cell.len(), 3);
        assert_eq!(cell[0].op_id, 70);
        assert_eq!(cell[0].latency_us, 110_000);
        assert_eq!(cell[0].status, 200);
        assert!(ops_in(&run, 0.0, 1.0, 100.0, 200.0, 3).is_empty());
        let slow = slowest_requests(&run, "GetObject", Some((6.0, 7.0)), 2);
        assert_eq!(slow.len(), 2);
        assert!(slow[0].total_us >= slow[1].total_us);
        assert_eq!(slow[0].key, "k69");
        assert!(slowest_requests(&run, "PutObject", None, 2).is_empty());
        // A finding's examples, request by request, with the retry's wait.
        let ex = super::super::view::examples(&run, &[61, 999], 5);
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].rows.len(), 2);
        assert_eq!(ex[0].rows[1].attempt, 2);
    }

    /// A fixed synthetic trace: `n` operations over 100 s on four verbs, latencies from a
    /// seeded generator with many ties, every seventh failed, every ninth sampled (it stands
    /// for 2, 4 or 8 operations), one or two requests each on three APIs.
    fn synthetic(n: u64) -> Run {
        let mut run = fixture::run();
        run.ops.clear();
        run.reqs.clear();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for i in 0..n {
            let t_us = (i * 100_000_000 / n) as i64;
            let latency_us = 500 + next() % 400 * 250;
            run.ops.push(OpRow {
                op_id: i + 1,
                verb: Cow::Borrowed(["read.full", "read.range", "stat", "write"][(i % 4) as usize]),
                key: format!("k{}", i % 97),
                t_us,
                latency_us,
                bytes: next() % 65_536,
                ok: i % 7 != 3,
                error_code: None,
                error: None,
                throttled: i % 11 == 5,
                attempts: 1,
                breakdown: crate::model::OpBreakdownUs {
                    backend_us: latency_us / 2,
                    queue_us: latency_us / 2,
                    ..Default::default()
                },
                requests: 1 + (i % 3 == 0) as u32,
                await_tree: None,
                weight: if i % 9 == 4 {
                    (2 << (i % 3)) as f64
                } else {
                    1.0
                },
            });
            for k in 0..1 + u64::from(i % 3 == 0) {
                let mut q = fixture::req(i + 1, t_us + k as i64 * 300, 200, k > 0);
                q.api = [
                    crate::trace::Api::GetObject,
                    crate::trace::Api::HeadObject,
                    crate::trace::Api::PutObject,
                ][((i + k) % 3) as usize];
                q.attempt = k as u32 + 1;
                q.ttfb_us = 100 + next() % 300 * 50;
                q.total_us = q.ttfb_us + next() % 200 * 50;
                run.reqs.push(q);
            }
        }
        run.report.run.window_s = 100.0;
        run.report.sampling = Default::default();
        run
    }

    /// A range sorts its rows once per order it reads, however many quantiles are read from
    /// each, and its numbers are those of the fixed synthetic trace.
    #[test]
    fn a_range_sorts_once_and_reads_the_expected_numbers() {
        let run = synthetic(20_000);
        for (a, b, rows, quantiles) in [
            (0.0, 100.0, 20_000, [51.5, 90.75, 99.25, 100.25]),
            (12.5, 60.0, 9_500, [51.75, 90.75, 99.5, 100.25]),
            (99.0, 100.0, 200, [51.5, 95.25, 99.5, 99.75]),
            (40.0, 40.2, 40, [64.75, 89.5, 98.0, 98.0]),
        ] {
            SORTS.with(|n| n.set(0));
            let v = RangeView::of(&run, a, b);
            let sorts = SORTS.with(|n| n.get());
            // One order for the range, one per verb, two per API (TTFB and total): however
            // many rows and however many quantiles are read from each.
            assert_eq!(sorts, 1 + v.verbs.len() + 2 * v.apis.len(), "[{a}, {b})");
            assert_eq!((v.verbs.len(), v.apis.len()), (4, 3));
            assert_eq!(v.rows_count, rows, "[{a}, {b})");
            assert_eq!(
                [v.p50_ms, v.p90_ms, v.p99_ms, v.p999_ms],
                quantiles,
                "[{a}, {b})"
            );
        }
        let v = RangeView::of(&run, 0.0, 100.0);
        // The distribution: from the fastest to the slowest, the share reaching 1.
        let n = v.cdf.pts.len();
        assert_eq!(n, 239);
        assert_eq!((v.cdf.pts[0].0, v.cdf.pts[n / 2].0), (0.75, 51.25));
        assert_eq!(v.cdf.pts[n - 1], (100.25, 1.0));
        // The slowest first, the earlier of equals first.
        assert_eq!(v.slowest_op_ids, [220, 1057, 1082, 1104, 1658]);
        let verbs: Vec<(&str, f64, f64)> = v
            .verbs
            .iter()
            .map(|x| (x.verb.as_str(), x.p50_ms, x.p99_ms))
            .collect();
        assert_eq!(
            verbs,
            [
                ("read.full", 51.5, 99.5),
                ("read.range", 52.0, 99.5),
                ("stat", 51.75, 99.5),
                ("write", 50.75, 99.25)
            ]
        );
        let apis: Vec<(&str, [f64; 4])> = v
            .apis
            .iter()
            .map(|x| {
                (
                    x.api.as_str(),
                    [x.ttfb_p50_ms, x.ttfb_p99_ms, x.total_p50_ms, x.total_p99_ms],
                )
            })
            .collect();
        assert_eq!(
            apis,
            [
                ("HeadObject", [7.6, 14.95, 12.6, 23.35]),
                ("GetObject", [7.75, 14.95, 12.6, 23.1]),
                ("PutObject", [7.3, 14.9, 12.45, 23.35])
            ]
        );
        // A verb's distribution over a range: one sort.
        SORTS.with(|n| n.set(0));
        let cdf = LatencyCdf::of_range_verb(&run, 0.0, 100.0, "stat").unwrap();
        assert_eq!(SORTS.with(|n| n.get()), 1);
        assert_eq!(cdf.pts.last().unwrap().1, 1.0);
        // The drill-downs select too: the slowest first, the earlier of equals first.
        let cell: Vec<u64> = ops_in(&run, 0.0, 100.0, 0.0, 1e9, 8)
            .iter()
            .map(|o| o.op_id)
            .collect();
        assert_eq!(cell, [220, 1057, 1082, 1104, 1658, 2365, 2388, 2879]);
        let listed: Vec<u64> = slowest_requests(&run, "GetObject", None, 8)
            .iter()
            .map(|q| q.op_id)
            .collect();
        assert_eq!(listed, [7510, 7789, 6688, 9307, 5977, 12589, 13762, 7003]);
        // Few rows, and none.
        assert_eq!(first_n(Vec::<u64>::new(), 5, |x| *x), Vec::<u64>::new());
        assert_eq!(first_n(vec![3, 1, 2], 5, |x| *x), [1, 2, 3]);
        assert_eq!(first_n(vec![3, 1, 2], 0, |x| *x), Vec::<i32>::new());
        assert_eq!(Weighted::new(Vec::new()).quantile_ms(0.5), 0.0);
        assert_eq!(Weighted::new(Vec::new()).cdf(true), LatencyCdf::default());
    }
}
