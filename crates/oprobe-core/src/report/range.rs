//! A time range of a run, recomputed from its trace rows (design v6.3 B5, B6): the key
//! numbers, the requests by API, the verbs, the latency distribution and the slowest
//! operations of `[t0, t1)`, and the drill-down queries behind an aggregate (a heatmap cell,
//! an API). `oprobe show --since/--until` and the app's brushed range read the same
//! [`RangeView`].
//!
//! **What counts.** An operation counts in the range it started in (as the timeline's
//! goodput and the heatmap); a request in the range it was sent in.
//!
//! **Sampled rows.** When the run kept only a share of its rows, each kept row stands for
//! [`Run::op_weights`] operations (failed, throttled and the slowest rows stand for
//! themselves): counts are weighted estimates and the quantiles weighted, so the always-kept
//! tail is not over-weighed. A request carries its operation's weight.

use std::collections::{BTreeMap, HashMap};

use schemars::JsonSchema;
use serde::Serialize;

use super::view::{ApiRow, LatencyCdf, StageShare, VerbView};
use crate::analysis::stats::weighted_quantile_us_ms;
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
    /// Operations that failed in the range: by when they ended, as the timeline counts
    /// failures (an operation started earlier that times out inside the range counts here).
    pub failed_ops_count: u64,
    /// Failed / every operation that ended in the range.
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
    pub max_ms: f64,
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

/// An operation's weight and latency, the pair the weighted quantiles take.
fn wq(v: &mut [(u64, f64)], q: f64) -> f64 {
    weighted_quantile_us_ms(v, q)
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
        // Goodput as the timeline counts it: operations started in the range that succeeded.
        let failed_started_w: f64 = ops.iter().filter(|o| !o.ok).map(|o| weight(o.op_id)).sum();
        // Failures as the timeline counts them: when the operation ended, whenever it started,
        // against every operation that ended in the range.
        let ended = |o: &&OpRow| inside(o.t_us.saturating_add(o.latency_us as i64));
        let ended_w: f64 = run.ops.iter().filter(ended).map(|o| weight(o.op_id)).sum();
        let failed_w: f64 = run
            .ops
            .iter()
            .filter(ended)
            .filter(|o| !o.ok)
            .map(|o| weight(o.op_id))
            .sum();
        let bytes_w: f64 = ops.iter().map(|o| o.bytes as f64 * weight(o.op_id)).sum();
        let mut lat: Vec<(u64, f64)> = ops
            .iter()
            .map(|o| (o.latency_us, weight(o.op_id)))
            .collect();
        let q = |lat: &mut Vec<(u64, f64)>, q: f64| wq(lat, q);
        let (p50, p90, p99, p999, max) = (
            q(&mut lat, 0.5),
            q(&mut lat, 0.9),
            q(&mut lat, 0.99),
            q(&mut lat, 0.999),
            ops.iter().map(|o| o.latency_us).max().unwrap_or(0) as f64 / 1000.0,
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
        let mut slow: Vec<&OpRow> = ops.clone();
        slow.sort_by_key(|o| std::cmp::Reverse(o.latency_us));
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
            error_ratio: ratio(failed_w, ended_w),
            ops_per_s: total_w / span,
            goodput_ops_per_s: (total_w - failed_started_w).max(0.0) / span,
            mib_per_s: bytes_w / span / (1024.0 * 1024.0),
            p50_ms: p50,
            p90_ms: p90,
            p99_ms: p99,
            p999_ms: p999,
            max_ms: max,
            requests_count: requests_w.round() as u64,
            throttled_requests_count: throttled_w.round() as u64,
            throttled_request_ratio: ratio(throttled_w, requests_w),
            retries_count: retries_w.round() as u64,
            stages: stages(&ops, &weight),
            apis: apis(&reqs, &weight),
            verbs: verbs(&ops, &weight, span),
            slowest_op_ids: slow.iter().take(Self::SLOWEST).map(|o| o.op_id).collect(),
            cdf: cdf(&mut lat, !sampled),
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
            let mut ttfb: Vec<(u64, f64)> = qs.iter().map(|q| (q.ttfb_us, w(q))).collect();
            let mut total: Vec<(u64, f64)> = qs.iter().map(|q| (q.total_us, w(q))).collect();
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
                ttfb_p50_ms: wq(&mut ttfb, 0.5),
                ttfb_p99_ms: wq(&mut ttfb, 0.99),
                total_p50_ms: wq(&mut total, 0.5),
                total_p99_ms: wq(&mut total, 0.99),
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
            let mut lat: Vec<(u64, f64)> =
                os.iter().map(|o| (o.latency_us, weight(o.op_id))).collect();
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
                p50_ms: wq(&mut lat, 0.5),
                p99_ms: wq(&mut lat, 0.99),
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

/// The weighted distribution of `lat` (µs, weight), at most [`LatencyCdf::MAX_POINTS`].
pub(crate) fn cdf(lat: &mut [(u64, f64)], exact: bool) -> LatencyCdf {
    let total: f64 = lat.iter().map(|x| x.1).sum();
    if lat.is_empty() || total <= 0.0 {
        return LatencyCdf::default();
    }
    lat.sort_unstable_by_key(|x| x.0);
    let n = lat.len();
    let step = n.div_ceil(LatencyCdf::MAX_POINTS).max(1);
    let mut acc = 0.0;
    let mut pts = Vec::new();
    for (i, (us, w)) in lat.iter().enumerate() {
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
    let mut v: Vec<&OpRow> = run
        .ops
        .iter()
        .filter(|o| o.t_us >= t0 && o.t_us < t1)
        .filter(|o| (o.latency_us as f64) >= l0 && (o.latency_us as f64) < l1)
        .collect();
    v.sort_by_key(|o| std::cmp::Reverse(o.latency_us));
    v.into_iter().take(n).map(|o| OpLine::of(run, o)).collect()
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
    let mut v: Vec<&ReqRow> = run
        .reqs
        .iter()
        .filter(|q| (q.api.label() == api || q.api_name() == api) && inside(q.t_us))
        .collect();
    v.sort_by_key(|q| std::cmp::Reverse(q.total_us));
    v.into_iter()
        .take(n)
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

    #[test]
    fn a_range_counts_failures_when_they_happened_as_the_timeline_does() {
        let mut run = run();
        // Op 3 started at 0.2 s; it now retries until it fails at 7.4 s.
        let o = run.ops.iter_mut().find(|o| o.op_id == 3).unwrap();
        o.latency_us = 7_200_000;
        o.ok = false;
        let late = RangeView::of(&run, 7.0, 8.0);
        assert_eq!(
            late.failed_ops_count, 1,
            "the failure lands where it happened"
        );
        // Eleven others ended in [7, 8): the 110 ms op started at 6.9 s and the ten started
        // at 7.0..7.9 s. 1 failure of 12.
        assert!(
            (late.error_ratio - 1.0 / 12.0).abs() < 1e-9,
            "{}",
            late.error_ratio
        );
        // Goodput stays the operations that started in the range and succeeded.
        assert!((late.goodput_ops_per_s - 10.0).abs() < 1e-9);
        let early = RangeView::of(&run, 0.0, 1.0);
        assert_eq!(early.failed_ops_count, 0);
        assert!((early.goodput_ops_per_s - 9.0).abs() < 1e-9);
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
}
