//! Trace-driven what-if estimates: replay the recorded request and operation rows of a run
//! under a different client setting (tail-cut, in-flight cap, suffix reads, a read cache,
//! concurrency) and predict the effect on latency, request count and throughput.
//!
//! These are *estimates*: the replay keeps everything else as measured (server behaviour,
//! network, the order of operations) and changes one thing. Findings quote them as such and
//! pair them with the `oprobe run …` command that measures the real effect.

use std::collections::{HashMap, HashSet, VecDeque};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::stats::{Rng, quantile_us_ms};
use crate::model::{OpRow, ReqRow, Run};

/// The predicted effect of one setting change.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Prediction {
    /// What was simulated: `tail_cut`, `uncapped_inflight`, `suffix_read`, `cache`,
    /// `concurrency`.
    pub kind: String,
    /// The setting, e.g. `tail_cut at 150 ms`.
    pub setting: String,
    /// The predicted metric, e.g. `p99_ms` of `read.range` operations.
    pub metric: String,
    pub current: f64,
    pub predicted: f64,
    /// Requests added (positive) or saved (negative) per 1 000 operations.
    pub requests_per_1k_ops_delta: f64,
    /// Operations / requests the estimate replayed.
    pub replayed_ops_count: u64,
    pub note: String,
}

impl Prediction {
    /// Relative change of the metric (`predicted / current - 1`).
    pub fn change_ratio(&self) -> f64 {
        if self.current > 0.0 {
            self.predicted / self.current - 1.0
        } else {
            0.0
        }
    }
}

fn op_p99_ms(lat_us: impl Iterator<Item = u64>) -> f64 {
    let mut v: Vec<u64> = lat_us.collect();
    quantile_us_ms(&mut v, 0.99)
}

fn reqs_by_op(reqs: &[ReqRow]) -> HashMap<u64, Vec<&ReqRow>> {
    let mut m: HashMap<u64, Vec<&ReqRow>> = HashMap::new();
    for r in reqs.iter().filter(|r| r.op_id != 0) {
        m.entry(r.op_id).or_default().push(r);
    }
    m
}

/// A tail-cut threshold for `api`: 1.5× the request p95, rounded up to 10 ms, at least the
/// p90 × 2 (so ordinary variance is never cut).
pub fn tail_cut_threshold_us(reqs: &[ReqRow], api: &str) -> Option<u64> {
    let mut t: Vec<u64> = reqs
        .iter()
        .filter(|r| r.api == api && !r.is_error())
        .map(|r| r.total_us)
        .collect();
    if t.len() < 50 {
        return None;
    }
    let p95 = quantile_us_ms(&mut t, 0.95) * 1000.0;
    let p90 = quantile_us_ms(&mut t, 0.90) * 1000.0;
    let raw = (p95 * 1.5).max(p90 * 2.0).max(1000.0);
    Some(((raw / 10_000.0).ceil() * 10_000.0) as u64)
}

/// Tail-cut (OpenDAL `TailCutLayer`): a request of `api` slower than `threshold_us` is
/// abandoned at the threshold and re-issued; the re-issue takes a time drawn from the
/// requests of the same API that finished under the threshold. Each operation saves the
/// largest saving of its cut requests (conservative when an operation's requests overlap).
pub fn tail_cut(run: &Run, api: &str, threshold_us: u64) -> Option<Prediction> {
    let fast: Vec<u64> = run
        .reqs
        .iter()
        .filter(|r| r.api == api && !r.is_error() && r.total_us <= threshold_us)
        .map(|r| r.total_us)
        .collect();
    if fast.len() < 20 {
        return None;
    }
    let by_op = reqs_by_op(&run.reqs);
    let mut rng = Rng::new(threshold_us);
    let mut extra = 0u64;
    let affected: Vec<&OpRow> = run
        .ops
        .iter()
        .filter(|o| {
            by_op
                .get(&o.op_id)
                .is_some_and(|v| v.iter().any(|r| r.api == api))
        })
        .collect();
    if affected.len() < 20 {
        return None;
    }
    let before = op_p99_ms(affected.iter().map(|o| o.latency_us));
    let after = op_p99_ms(affected.iter().map(|o| {
        let saving = by_op[&o.op_id]
            .iter()
            .filter(|r| r.api == api && r.total_us > threshold_us)
            .map(|r| {
                extra += 1;
                let redo = fast[rng.index(fast.len())];
                r.total_us.saturating_sub(threshold_us + redo)
            })
            .max()
            .unwrap_or(0);
        o.latency_us.saturating_sub(saving)
    }));
    Some(Prediction {
        kind: "tail_cut".into(),
        setting: format!("tail_cut at {} ms", threshold_us / 1000),
        metric: format!("p99_ms of operations issuing {api}"),
        current: before,
        predicted: after,
        requests_per_1k_ops_delta: extra as f64 * 1000.0 / affected.len() as f64,
        replayed_ops_count: affected.len() as u64,
        note: format!(
            "estimate: replayed {} operations; each {api} slower than {} ms is re-issued and the retry takes a time drawn from the requests that finished in time",
            affected.len(),
            threshold_us / 1000
        ),
    })
}

/// No in-flight cap: every operation loses the time its requests waited for a permit.
pub fn uncapped_inflight(run: &Run) -> Option<Prediction> {
    let queued: u64 = run.ops.iter().map(|o| o.breakdown.queue_us).sum();
    if run.ops.len() < 20 || queued == 0 {
        return None;
    }
    let before = op_p99_ms(run.ops.iter().map(|o| o.latency_us));
    let after = op_p99_ms(
        run.ops
            .iter()
            .map(|o| o.latency_us.saturating_sub(o.breakdown.queue_us)),
    );
    Some(Prediction {
        kind: "uncapped_inflight".into(),
        setting: "max_inflight_requests = 0 (unlimited)".into(),
        metric: "p99_ms of all operations".into(),
        current: before,
        predicted: after,
        requests_per_1k_ops_delta: 0.0,
        replayed_ops_count: run.ops.len() as u64,
        note: "estimate: the queue wait is removed and nothing else changes (more parallel requests may slow the server or trigger throttling)".into(),
    })
}

/// Operations that `HEAD` (stat) an object and then read it: a suffix range read
/// (`bytes=-N`) of the footer needs no HEAD.
pub fn suffix_read(run: &Run) -> Option<Prediction> {
    let by_op = reqs_by_op(&run.reqs);
    let mut ops = 0u64;
    let mut heads = 0u64;
    let lat: Vec<(u64, u64)> = run
        .ops
        .iter()
        .map(|o| {
            let v = by_op.get(&o.op_id).map(Vec::as_slice).unwrap_or(&[]);
            let has_get = v.iter().any(|r| r.api == "GetObject");
            let head_us: u64 = v
                .iter()
                .filter(|r| r.api == "HeadObject" && has_get)
                .map(|r| {
                    heads += 1;
                    r.total_us
                })
                .sum();
            if head_us > 0 {
                ops += 1;
            }
            (o.latency_us, o.latency_us.saturating_sub(head_us))
        })
        .collect();
    if ops < 20 || (ops as f64) < 0.1 * run.ops.len() as f64 {
        return None;
    }
    let mut a: Vec<u64> = lat.iter().map(|x| x.0).collect();
    let mut b: Vec<u64> = lat.iter().map(|x| x.1).collect();
    Some(Prediction {
        kind: "suffix_read".into(),
        setting: "suffix range reads (no HEAD before the footer read)".into(),
        metric: "p50_ms of all operations".into(),
        current: quantile_us_ms(&mut a, 0.5),
        predicted: quantile_us_ms(&mut b, 0.5),
        requests_per_1k_ops_delta: -(heads as f64) * 1000.0 / run.ops.len().max(1) as f64,
        replayed_ops_count: run.ops.len() as u64,
        note: format!(
            "estimate: {ops} operations issued a HEAD before reading; their HEAD time is removed"
        ),
    })
}

/// Replay the read operations' keys through an LRU cache of `capacity_bytes` (object size =
/// the bytes each read returned); a hit costs no backend request.
pub fn cache(run: &Run, capacity_bytes: u64) -> Option<Prediction> {
    let mut reads: Vec<&OpRow> = run
        .ops
        .iter()
        .filter(|o| o.ok && o.verb.starts_with("read") && o.bytes > 0)
        .collect();
    if reads.len() < 50 {
        return None;
    }
    reads.sort_by_key(|o| o.t_us);
    let mut lru: VecDeque<(&str, u64)> = VecDeque::new();
    let mut present: HashSet<&str> = HashSet::new();
    let mut used = 0u64;
    let (mut hits, mut saved_reqs) = (0u64, 0u64);
    let mut lat_after = Vec::with_capacity(reads.len());
    for o in &reads {
        let key = o.key.as_str();
        if present.contains(key) {
            hits += 1;
            saved_reqs += o.requests as u64;
            if let Some(i) = lru.iter().position(|(k, _)| *k == key) {
                let e = lru.remove(i).expect("present");
                lru.push_back(e);
            }
            lat_after.push(o.breakdown.client_us + o.breakdown.cache_us);
            continue;
        }
        lat_after.push(o.latency_us);
        if o.bytes > capacity_bytes {
            continue;
        }
        while used + o.bytes > capacity_bytes {
            let Some((k, b)) = lru.pop_front() else { break };
            present.remove(k);
            used -= b;
        }
        lru.push_back((key, o.bytes));
        present.insert(key);
        used += o.bytes;
    }
    if hits == 0 {
        return None;
    }
    let mut before: Vec<u64> = reads.iter().map(|o| o.latency_us).collect();
    Some(Prediction {
        kind: "cache".into(),
        setting: format!("read cache of {}", crate::util::ByteSize(capacity_bytes)),
        metric: "p50_ms of read operations".into(),
        current: quantile_us_ms(&mut before, 0.5),
        predicted: quantile_us_ms(&mut lat_after, 0.5),
        requests_per_1k_ops_delta: -(saved_reqs as f64) * 1000.0 / reads.len() as f64,
        replayed_ops_count: reads.len() as u64,
        note: format!(
            "estimate: replayed {} reads through an LRU; {:.0}% would hit (whole objects as they were read; real hit rates depend on how ranges are cached)",
            reads.len(),
            hits as f64 * 100.0 / reads.len() as f64
        ),
    })
}

/// Closed-loop throughput at `concurrency` workers (Little's law on the measured latency),
/// capped by the throttle ceiling when one was observed.
pub fn concurrency(run: &Run, to: u64, ceiling_ops_per_s: Option<f64>) -> Option<Prediction> {
    let r = &run.report;
    let from = r.knobs.requested.concurrency_count.max(1);
    if r.summary.ops < 20 || r.summary.ops_per_s <= 0.0 || r.workload.rate_per_s.is_some() {
        return None;
    }
    let scaled = r.summary.ops_per_s * to as f64 / from as f64;
    let predicted = ceiling_ops_per_s.map_or(scaled, |c| scaled.min(c));
    Some(Prediction {
        kind: "concurrency".into(),
        setting: format!("concurrency {to}"),
        metric: "ops_per_s".into(),
        current: r.summary.ops_per_s,
        predicted,
        requests_per_1k_ops_delta: 0.0,
        replayed_ops_count: r.summary.ops,
        note: if ceiling_ops_per_s.is_some() {
            "estimate: throughput scales with workers at the measured latency until the observed throttle ceiling".into()
        } else {
            "estimate: throughput scales with workers at the measured latency (holds while the server, network and client CPU keep up)".into()
        },
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::analysis::outliers::test_req;
    use std::borrow::Cow;

    pub(crate) fn op(op_id: u64, t_us: i64, latency_us: u64) -> OpRow {
        OpRow {
            op_id,
            verb: Cow::Borrowed("read.range"),
            key: format!("k{}", op_id % 10),
            t_us,
            latency_us,
            bytes: 1024,
            ok: true,
            error_code: None,
            error: None,
            throttled: false,
            attempts: 1,
            breakdown: crate::model::OpBreakdownUs {
                backend_us: latency_us,
                ..Default::default()
            },
            requests: 1,
            await_tree: None,
        }
    }

    /// 1 000 single-request reads at ~10 ms, 2.5 % stragglers at 830 ms.
    pub(crate) fn straggler_run() -> Run {
        let mut ops = Vec::new();
        let mut reqs = Vec::new();
        for i in 0..1000u64 {
            let slow = i % 40 == 7;
            let lat = if slow { 840_000 } else { 9_000 + (i % 5) * 500 };
            ops.push(op(i + 1, i as i64 * 5000, lat));
            let mut r = test_req(i as i64 * 5000, lat);
            r.op_id = i + 1;
            reqs.push(r);
        }
        let mut report: crate::model::Report =
            serde_json::from_value(crate::analysis::findings::tests::empty_report_json()).unwrap();
        report.summary.ops = 1000;
        report.summary.ops_per_s = 200.0;
        report.knobs.requested.concurrency_count = 2;
        Run { report, ops, reqs }
    }

    #[test]
    fn tail_cut_replay_cuts_the_p99() {
        let run = straggler_run();
        let t = tail_cut_threshold_us(&run.reqs, "GetObject").unwrap();
        assert!((10_000..100_000).contains(&t), "{t}");
        let p = tail_cut(&run, "GetObject", t).unwrap();
        assert!(p.current > 800.0, "{p:?}");
        assert!(p.predicted < 100.0, "{p:?}");
        assert!((20.0..30.0).contains(&p.requests_per_1k_ops_delta), "{p:?}");
    }

    #[test]
    fn cache_and_concurrency_estimates() {
        let run = straggler_run();
        // Ten keys of 1 KiB: everything after the first touch hits.
        let c = cache(&run, 1 << 20).unwrap();
        assert!(c.requests_per_1k_ops_delta < -900.0, "{c:?}");
        let c2 = concurrency(&run, 8, None).unwrap();
        assert_eq!(c2.predicted, 800.0);
        let c3 = concurrency(&run, 8, Some(300.0)).unwrap();
        assert_eq!(c3.predicted, 300.0);
        assert!(uncapped_inflight(&run).is_none());
    }
}
