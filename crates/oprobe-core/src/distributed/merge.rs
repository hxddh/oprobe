//! Merging the workers' measurements into one (design v5 §8).
//!
//! Counters, bytes and rates add up exactly. A latency distribution is merged as the mixture
//! of the workers' distributions: each worker's quantiles define a piecewise-linear CDF, the
//! mixture CDF is their count-weighted sum and its quantiles are found by bisection. This is
//! exact for counts, means and maxima, and close for quantiles (exact when the workers agree).
//! Trace rows are concatenated with operation ids made unique per worker and start times
//! shifted onto the common window, so the capacity fit and findings see every worker's load.

use std::collections::BTreeMap;

use crate::trace::util::ratio;
use crate::trace::{
    ApiStats, ByteStats, CacheStats, ConnStats, IntegrityStats, OpBreakdown, OpenLoopStats,
    Quantiles, ResilienceStats, Snapshot, StageStats, Summary, TimelinePoint, TraceRows, VerbStats,
};

use super::protocol::Rows;

/// Bits of an operation id kept from the worker's own id (the worker index goes above).
const OP_ID_BITS: u32 = 48;

/// A worker's operation id, unique across the run.
pub fn global_op_id(worker: usize, op_id: u64) -> u64 {
    if op_id == 0 {
        return 0;
    }
    ((worker as u64 + 1) << OP_ID_BITS) | (op_id & ((1 << OP_ID_BITS) - 1))
}

/// The worker index of a global operation id (`None` for ids not made by [`global_op_id`]).
pub fn worker_of(op_id: u64) -> Option<usize> {
    let w = op_id >> OP_ID_BITS;
    (w > 0).then(|| (w - 1) as usize)
}

/// The mixture of latency distributions.
pub fn mix_quantiles(parts: &[Quantiles]) -> Quantiles {
    let parts: Vec<&Quantiles> = parts.iter().filter(|q| q.count > 0).collect();
    match parts.len() {
        0 => return Quantiles::default(),
        1 => return *parts[0],
        _ => {}
    }
    let total: u64 = parts.iter().map(|q| q.count).sum();
    let w = |q: &Quantiles| q.count as f64 / total as f64;
    let max = parts.iter().map(|q| q.max).fold(0.0, f64::max);
    // Each part's CDF from its quantile knots (monotone, from 0 at 0 to 1 at its max).
    let knots: Vec<[(f64, f64); 6]> = parts
        .iter()
        .map(|q| {
            let mut xs = [0.0, q.p50, q.p90, q.p99, q.p999, q.max];
            for i in 1..xs.len() {
                if xs[i] < xs[i - 1] {
                    xs[i] = xs[i - 1];
                }
            }
            let qs = [0.0, 0.5, 0.9, 0.99, 0.999, 1.0];
            std::array::from_fn(|i| (xs[i], qs[i]))
        })
        .collect();
    let cdf = |x: f64| -> f64 {
        knots
            .iter()
            .zip(&parts)
            .map(|(k, q)| {
                let f = if x >= k[5].0 {
                    1.0
                } else {
                    let mut f = 0.0;
                    for i in 1..6 {
                        let (x0, q0) = k[i - 1];
                        let (x1, q1) = k[i];
                        if x < x1 {
                            f = if x1 > x0 {
                                q0 + (q1 - q0) * (x - x0) / (x1 - x0)
                            } else {
                                q1
                            };
                            break;
                        }
                    }
                    f
                };
                w(q) * f
            })
            .sum()
    };
    let quantile = |p: f64| -> f64 {
        let (mut lo, mut hi) = (0.0, max);
        for _ in 0..64 {
            let mid = (lo + hi) / 2.0;
            if cdf(mid) < p {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        hi
    };
    Quantiles {
        count: total,
        mean: parts.iter().map(|q| q.mean * w(q)).sum(),
        p50: quantile(0.5),
        p90: quantile(0.9),
        p99: quantile(0.99),
        p999: quantile(0.999),
        max,
    }
}

fn weighted(items: impl Iterator<Item = (f64, f64)>) -> f64 {
    let (mut s, mut n) = (0.0, 0.0);
    for (v, w) in items {
        s += v * w;
        n += w;
    }
    ratio(s, n)
}

fn merge_breakdown(parts: &[(OpBreakdown, f64)]) -> OpBreakdown {
    let w = |f: fn(&OpBreakdown) -> f64| weighted(parts.iter().map(|(b, n)| (f(b), *n)));
    OpBreakdown {
        queue_ms: w(|b| b.queue_ms),
        cache_ms: w(|b| b.cache_ms),
        backend_ms: w(|b| b.backend_ms),
        retry_ms: w(|b| b.retry_ms),
        client_ms: w(|b| b.client_ms),
    }
}

fn merge_summary(s: &[&Summary], requests: u64, throttled: u64) -> Summary {
    let ops: u64 = s.iter().map(|x| x.ops).sum();
    let failed: u64 = s.iter().map(|x| x.failed_ops).sum();
    Summary {
        ops,
        ops_per_s: s.iter().map(|x| x.ops_per_s).sum(),
        attempted_ops_per_s: s.iter().map(|x| x.attempted_ops_per_s).sum(),
        failed_ops: failed,
        error_rate: ratio(failed as f64, ops as f64),
        throttled_request_ratio: ratio(throttled as f64, requests as f64),
        throttled_op_ratio: weighted(s.iter().map(|x| (x.throttled_op_ratio, x.ops as f64))),
        mib_per_s: s.iter().map(|x| x.mib_per_s).sum(),
        read_mib_per_s: s.iter().map(|x| x.read_mib_per_s).sum(),
        write_mib_per_s: s.iter().map(|x| x.write_mib_per_s).sum(),
        latency: mix_quantiles(&s.iter().map(|x| x.latency).collect::<Vec<_>>()),
        inflight_ops: s.iter().map(|x| x.inflight_ops).sum(),
    }
}

fn merge_verbs(parts: &[&Snapshot]) -> Vec<VerbStats> {
    let mut by: BTreeMap<String, Vec<&VerbStats>> = BTreeMap::new();
    for s in parts {
        for v in &s.verbs {
            by.entry(v.verb.clone()).or_default().push(v);
        }
    }
    by.into_iter()
        .map(|(verb, vs)| {
            let n = |v: &VerbStats| v.count as f64;
            VerbStats {
                verb,
                count: vs.iter().map(|v| v.count).sum(),
                errors: vs.iter().map(|v| v.errors).sum(),
                throttled: vs.iter().map(|v| v.throttled).sum(),
                ops_per_s: vs.iter().map(|v| v.ops_per_s).sum(),
                mib_per_s: vs.iter().map(|v| v.mib_per_s).sum(),
                latency: mix_quantiles(&vs.iter().map(|v| v.latency).collect::<Vec<_>>()),
                breakdown: merge_breakdown(
                    &vs.iter().map(|v| (v.breakdown, n(v))).collect::<Vec<_>>(),
                ),
                requests_per_op: weighted(vs.iter().map(|v| (v.requests_per_op, n(v)))),
                bytes_per_op: weighted(vs.iter().map(|v| (v.bytes_per_op, n(v)))),
                retries: vs.iter().map(|v| v.retries).sum(),
            }
        })
        .collect()
}

fn merge_stages(st: &[&StageStats]) -> StageStats {
    let m = |f: fn(&StageStats) -> Quantiles| {
        mix_quantiles(&st.iter().map(|s| f(s)).collect::<Vec<_>>())
    };
    StageStats {
        queue: m(|s| s.queue),
        connect: m(|s| s.connect),
        send: m(|s| s.send),
        ttfb: m(|s| s.ttfb),
        body: m(|s| s.body),
        stall: m(|s| s.stall),
        total: m(|s| s.total),
    }
}

fn merge_apis(parts: &[&Snapshot]) -> Vec<ApiStats> {
    let mut by: BTreeMap<String, Vec<&ApiStats>> = BTreeMap::new();
    for s in parts {
        for a in &s.apis {
            by.entry(a.api.label().to_owned()).or_default().push(a);
        }
    }
    let mut out: Vec<ApiStats> = by
        .into_values()
        .map(|v| {
            let mut status = BTreeMap::new();
            for a in &v {
                for (k, n) in &a.status {
                    *status.entry(*k).or_insert(0) += n;
                }
            }
            ApiStats {
                api: v[0].api,
                count: v.iter().map(|a| a.count).sum(),
                errors: v.iter().map(|a| a.errors).sum(),
                throttled: v.iter().map(|a| a.throttled).sum(),
                status,
                reused_pct: weighted(v.iter().map(|a| (a.reused_pct, a.count as f64))),
                bytes_up: v.iter().map(|a| a.bytes_up).sum(),
                bytes_down: v.iter().map(|a| a.bytes_down).sum(),
                stages: merge_stages(&v.iter().map(|a| &a.stages).collect::<Vec<_>>()),
            }
        })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.count));
    out
}

fn merge_connections(c: &[&ConnStats]) -> ConnStats {
    let first = |f: fn(&ConnStats) -> &String| {
        c.iter()
            .map(|x| f(x))
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default()
    };
    let mut remote: Vec<String> = c.iter().flat_map(|x| x.remote_addrs.clone()).collect();
    remote.sort();
    remote.dedup();
    let requests: u64 = c.iter().map(|x| x.requests).sum();
    let reused: u64 = c.iter().map(|x| x.reused_requests).sum();
    let q =
        |f: fn(&ConnStats) -> Quantiles| mix_quantiles(&c.iter().map(|x| f(x)).collect::<Vec<_>>());
    ConnStats {
        dialed: c.iter().map(|x| x.dialed).sum(),
        dial_failures: c.iter().map(|x| x.dial_failures).sum(),
        requests,
        reused_requests: reused,
        reuse_pct: 100.0 * ratio(reused as f64, requests as f64),
        http1_requests: c.iter().map(|x| x.http1_requests).sum(),
        http2_requests: c.iter().map(|x| x.http2_requests).sum(),
        alpn: first(|x| &x.alpn),
        tls_version: first(|x| &x.tls_version),
        cipher: first(|x| &x.cipher),
        proxy: c.iter().find_map(|x| x.proxy.clone()),
        remote_addrs: remote,
        dns: q(|x| x.dns),
        tcp: q(|x| x.tcp),
        proxy_connect: q(|x| x.proxy_connect),
        tls: q(|x| x.tls),
        peak_inflight_requests: c.iter().map(|x| x.peak_inflight_requests).sum(),
        // Every worker dials its own first connection; the first worker's sample stands for all.
        cold_start: c.iter().find_map(|x| x.cold_start.clone()),
    }
}

fn merge_resilience(r: &[&ResilienceStats]) -> ResilienceStats {
    let mut classes = BTreeMap::new();
    for x in r {
        for (k, n) in &x.error_classes {
            *classes.entry(k.clone()).or_insert(0) += n;
        }
    }
    ResilienceStats {
        retries: r.iter().map(|x| x.retries).sum(),
        retry_backoff_ms: r.iter().map(|x| x.retry_backoff_ms).sum(),
        throttled_responses: r.iter().map(|x| x.throttled_responses).sum(),
        status_429: r.iter().map(|x| x.status_429).sum(),
        status_503: r.iter().map(|x| x.status_503).sum(),
        max_retry_after_ms: r.iter().map(|x| x.max_retry_after_ms).max().unwrap_or(0),
        server_errors: r.iter().map(|x| x.server_errors).sum(),
        client_errors: r.iter().map(|x| x.client_errors).sum(),
        network_errors: r.iter().map(|x| x.network_errors).sum(),
        failed_ops: r.iter().map(|x| x.failed_ops).sum(),
        error_classes: classes,
    }
}

fn merge_bytes(b: &[&ByteStats], requests: u64, ops: u64) -> ByteStats {
    let s = |f: fn(&ByteStats) -> u64| b.iter().map(|x| f(x)).sum::<u64>();
    let mut o = ByteStats {
        logical_read: s(|x| x.logical_read),
        logical_write: s(|x| x.logical_write),
        cache_hit_bytes: s(|x| x.cache_hit_bytes),
        backend_down: s(|x| x.backend_down),
        backend_up: s(|x| x.backend_up),
        wire_rx: s(|x| x.wire_rx),
        wire_tx: s(|x| x.wire_tx),
        ..Default::default()
    };
    // The recorder's definitions (`oprobe_trace::Recorder::snapshot`).
    o.read_amplification = ratio(o.backend_down as f64, o.logical_read as f64);
    let uncached = o.logical_read.saturating_sub(o.cache_hit_bytes);
    o.overread_ratio = ratio(o.backend_down as f64, uncached as f64);
    o.write_amplification = ratio(o.backend_up as f64, o.logical_write as f64);
    o.request_amplification = ratio(requests as f64, ops as f64);
    o
}

// Fields the merge does not know (added to the trace types later) keep their defaults.
#[allow(clippy::needless_update)]
fn merge_timeline(parts: &[&Snapshot]) -> Vec<TimelinePoint> {
    let mut by: BTreeMap<i64, Vec<&TimelinePoint>> = BTreeMap::new();
    for s in parts {
        for p in &s.timeline {
            by.entry((p.t_s * 1000.0).round() as i64)
                .or_default()
                .push(p);
        }
    }
    by.into_iter()
        .map(|(t, ps)| TimelinePoint {
            t_s: t as f64 / 1000.0,
            ops_per_s: ps.iter().map(|p| p.ops_per_s).sum(),
            mib_per_s: ps.iter().map(|p| p.mib_per_s).sum(),
            p50_ms: weighted(ps.iter().map(|p| (p.p50_ms, p.ops_per_s.max(1e-9)))),
            // The worst worker's p99 bounds the second's p99 from below; never understate it.
            p99_ms: ps.iter().map(|p| p.p99_ms).fold(0.0, f64::max),
            errors: ps.iter().map(|p| p.errors).sum(),
            throttled: ps.iter().map(|p| p.throttled).sum(),
            ..Default::default()
        })
        .collect()
}

/// One snapshot of the whole run from the workers' snapshots (in worker order).
// Fields the merge does not know (added to the trace types later) keep their defaults.
#[allow(clippy::needless_update)]
pub fn merge_snapshots(parts: &[Snapshot]) -> Snapshot {
    let refs: Vec<&Snapshot> = parts.iter().collect();
    let Some(first) = refs.first() else {
        return Snapshot::default();
    };
    let apis = merge_apis(&refs);
    let requests: u64 = apis.iter().map(|a| a.count).sum();
    let throttled: u64 = apis.iter().map(|a| a.throttled).sum();
    let summary = merge_summary(
        &refs.iter().map(|s| &s.summary).collect::<Vec<_>>(),
        requests,
        throttled,
    );
    let ops = summary.ops;
    let open_loop: Vec<&OpenLoopStats> = refs.iter().filter_map(|s| s.open_loop.as_ref()).collect();
    let cache: Vec<&CacheStats> = refs.iter().map(|s| &s.cache).collect();
    let lookups: u64 = cache.iter().map(|c| c.lookups).sum();
    let hits: u64 = cache.iter().map(|c| c.hits).sum();
    let mut slow: Vec<_> = refs
        .iter()
        .enumerate()
        .flat_map(|(w, s)| {
            s.slow_ops.iter().cloned().map(move |mut o| {
                o.op_id = global_op_id(w, o.op_id);
                o
            })
        })
        .collect();
    slow.sort_by(|a, b| b.latency_ms.total_cmp(&a.latency_ms));
    slow.truncate(refs.iter().map(|s| s.slow_ops.len()).max().unwrap_or(0));
    Snapshot {
        phase: first.phase,
        elapsed_s: refs.iter().map(|s| s.elapsed_s).fold(0.0, f64::max),
        summary,
        verbs: merge_verbs(&refs),
        apis,
        connections: merge_connections(&refs.iter().map(|s| &s.connections).collect::<Vec<_>>()),
        resilience: merge_resilience(&refs.iter().map(|s| &s.resilience).collect::<Vec<_>>()),
        bytes: merge_bytes(
            &refs.iter().map(|s| &s.bytes).collect::<Vec<_>>(),
            requests,
            ops,
        ),
        cache: CacheStats {
            enabled: cache.iter().any(|c| c.enabled),
            lookups,
            hits,
            hit_ratio: ratio(hits as f64, lookups as f64),
            lookup_ms_mean: weighted(cache.iter().map(|c| (c.lookup_ms_mean, c.lookups as f64))),
        },
        integrity: IntegrityStats {
            verified: refs.iter().map(|s| s.integrity.verified).sum(),
            corrupted: refs.iter().map(|s| s.integrity.corrupted).sum(),
        },
        client: crate::trace::ClientStats {
            cpu_cores: refs.iter().map(|s| s.client.cpu_cores).sum(),
            cpu_pct: refs.iter().map(|s| s.client.cpu_pct).sum(),
            cpu_pct_peak: refs.iter().map(|s| s.client.cpu_pct_peak).sum(),
            rss_mib: refs.iter().map(|s| s.client.rss_mib).sum(),
        },
        open_loop: (!open_loop.is_empty()).then(|| OpenLoopStats {
            target_rate: open_loop.iter().map(|o| o.target_rate).sum(),
            achieved_rate: open_loop.iter().map(|o| o.achieved_rate).sum(),
            missed: open_loop.iter().map(|o| o.missed).sum(),
            max_schedule_lag_ms: open_loop
                .iter()
                .map(|o| o.max_schedule_lag_ms)
                .fold(0.0, f64::max),
            ..Default::default()
        }),
        interrupted: refs.iter().any(|s| s.interrupted),
        effective_knobs: first.effective_knobs.clone(),
        timeline: merge_timeline(&refs),
        slow_ops: slow,
        ..Default::default()
    }
}

/// The workers' trace rows as one: operation ids made unique, start times shifted by each
/// worker's `shift_us` (its window start minus the earliest one, on the coordinator's clock).
pub fn merge_rows(parts: Vec<(Rows, i64)>) -> TraceRows {
    let shards = parts
        .into_iter()
        .enumerate()
        .map(|(w, (r, shift))| {
            let mut ops = r.ops;
            for o in &mut ops {
                o.op_id = global_op_id(w, o.op_id);
                o.t_us += shift;
            }
            let mut reqs = r.reqs;
            for q in &mut reqs {
                q.op_id = global_op_id(w, q.op_id);
                q.t_us += shift;
            }
            TraceRows {
                ops,
                reqs,
                info: r.info,
                sampling: r.sampling.into(),
            }
        })
        .collect();
    TraceRows::merge(shards)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(count: u64, p50: f64, p99: f64) -> Quantiles {
        Quantiles {
            count,
            mean: p50,
            p50,
            p90: (p50 + p99) / 2.0,
            p99,
            p999: p99 * 1.1,
            max: p99 * 1.2,
        }
    }

    #[test]
    fn mixtures_keep_counts_means_and_maxima() {
        let a = q(100, 10.0, 50.0);
        // One part: returned as it is; identical parts: the same quantiles.
        assert_eq!(mix_quantiles(&[a]), a);
        let m = mix_quantiles(&[a, a]);
        assert_eq!(m.count, 200);
        assert!(
            (m.p50 - 10.0).abs() < 1e-6 && (m.p99 - 50.0).abs() < 1e-6,
            "{m:?}"
        );
        // A slow worker pulls the merged tail up, not the median.
        let slow = q(100, 100.0, 500.0);
        let m = mix_quantiles(&[a, slow]);
        assert_eq!(m.max, 600.0);
        assert!((m.mean - 55.0).abs() < 1e-9);
        assert!(m.p50 > a.p50 && m.p50 < slow.p50, "{m:?}");
        assert!(m.p99 > 400.0, "{m:?}");
        assert_eq!(mix_quantiles(&[]), Quantiles::default());
    }

    #[test]
    fn snapshots_add_up() {
        let mut s = Snapshot::default();
        s.summary.ops = 100;
        s.summary.ops_per_s = 50.0;
        s.summary.failed_ops = 1;
        s.summary.latency = q(100, 10.0, 50.0);
        s.apis.push(ApiStats {
            count: 200,
            throttled: 4,
            ..Default::default()
        });
        s.bytes.logical_read = 1000;
        s.bytes.backend_down = 1500;
        s.slow_ops.push(crate::trace::SlowOp {
            op_id: 7,
            latency_ms: 50.0,
            ..Default::default()
        });
        let m = merge_snapshots(&[s.clone(), s.clone(), s]);
        assert_eq!(m.summary.ops, 300);
        assert_eq!(m.summary.failed_ops, 3);
        assert!((m.summary.ops_per_s - 150.0).abs() < 1e-9);
        assert_eq!(m.apis[0].count, 600);
        assert!((m.summary.throttled_request_ratio - 0.02).abs() < 1e-9);
        assert!((m.bytes.read_amplification - 1.5).abs() < 1e-9);
        assert_eq!(m.slow_ops.len(), 1);
        assert_eq!(worker_of(m.slow_ops[0].op_id), Some(0));
    }

    #[test]
    fn op_ids_stay_unique_per_worker() {
        assert_eq!(global_op_id(0, 0), 0);
        assert_ne!(global_op_id(0, 5), global_op_id(1, 5));
        assert_eq!(worker_of(global_op_id(2, 9)), Some(2));
        assert_eq!(worker_of(9), None);
    }
}
