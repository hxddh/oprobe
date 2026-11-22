//! Trace-driven what-if estimates: replay the recorded request and operation rows of a run
//! under a different client setting (tail-cut, a read cache) and predict the effect on
//! latency and request count. Findings use them as evidence; `oprobe prove` measures.
//!
//! These are *estimates*: the replay keeps everything else as measured (server behaviour,
//! network, the order of operations) and changes one thing. Findings quote them as such and
//! pair them with the `oprobe run …` command that measures the real effect.

use std::collections::{HashMap, HashSet, VecDeque};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use std::time::Duration;

use super::stats::{Rng, quantile_us_ms};
use crate::engine::stack::{RetryParams, TAIL_CUT_BLIND_START, TailCutHistogram, TailCutParams};
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
    /// Lower end of the prediction interval (same unit as `predicted`).
    pub predicted_lo: f64,
    /// Upper end of the prediction interval.
    pub predicted_hi: f64,
    /// Requests added (positive) or saved (negative) per 1 000 operations.
    pub requests_per_1k_ops_delta: f64,
    /// Operations / requests the estimate replayed.
    pub replayed_ops_count: u64,
    pub note: String,
}

impl Prediction {
    /// A replay's interval: between "no effect" (`current`) and the replayed value.
    fn bounded(mut self) -> Self {
        self.predicted_lo = self.predicted.min(self.current);
        self.predicted_hi = self.predicted.max(self.current);
        self
    }

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

/// How the measured client's `TailCutLayer` would behave on a run: the layer's parameters
/// (`engine::stack`, the same values the client is built with), the retry backoff a cut
/// request waits before it is re-issued, and when the layer starts cutting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TailCutSetup {
    pub params: TailCutParams,
    /// Mean wait before the re-issue (the retry layer's first backoff), µs.
    pub backoff_us: u64,
    /// Requests that start before this time (µs from the window start) are never cut and
    /// never counted: the layer is blind for its first 10 s ([`TAIL_CUT_BLIND_START`]).
    pub blind_until_us: i64,
}

impl TailCutSetup {
    /// The layer on the run's operator (tail-cut switched on), built when the warmup began.
    /// `None` when retries are off: a cut request would fail instead of being re-issued.
    pub fn of(r: &crate::model::Report) -> Option<Self> {
        let op = &r.spec.profile.operator;
        let retry = RetryParams::of(op.layers.retry.as_ref()?);
        let params = op
            .layers
            .tail_cut
            .clone()
            .unwrap_or_default()
            .params(op.op_timeout());
        Some(Self {
            params,
            backoff_us: retry.first_backoff_mean().as_micros() as u64,
            blind_until_us: ((TAIL_CUT_BLIND_START.as_secs_f64() - r.workload.warmup_s) * 1e6)
                as i64,
        })
    }

    /// The same layer on a long-lived client (no blind start): the steady state.
    pub fn steady(mut self) -> Self {
        self.blind_until_us = i64::MIN;
        self
    }

    /// Seconds of the window `[0, window_s)` the layer was blind for.
    pub fn blind_s(&self, window_s: f64) -> f64 {
        (self.blind_until_us as f64 / 1e6).clamp(0.0, window_s.max(0.0))
    }
}

/// A trace replayed through `TailCutLayer`'s rule for one API.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TailCutReplay {
    pub prediction: Prediction,
    /// The deadline the layer holds at the end of the replay (its steady state), µs; `None`
    /// when it never had enough samples.
    pub deadline_us: Option<u64>,
    /// Requests the layer cut and re-issued.
    pub cut_count: u64,
    /// Requests slower than the final deadline that started while the layer was blind.
    pub blind_count: u64,
}

/// Replay `api`'s requests (in start order) through `TailCutLayer`: each successful request
/// is recorded in the layer's histogram, and a request slower than the deadline in force when
/// it starts ([`TailCutParams::deadline`], the layer's rule) is cut at the deadline and
/// re-issued after the retry backoff; the re-issue takes a time drawn from the requests that
/// finished within the deadline. Each operation saves the largest saving of its cut requests
/// (conservative when an operation's requests overlap).
pub fn tail_cut(run: &Run, api: &str, setup: &TailCutSetup) -> Option<TailCutReplay> {
    let rows: Vec<&ReqRow> = run.reqs.iter().filter(|r| r.api == api).collect();
    let mut ok: Vec<u64> = rows
        .iter()
        .filter(|r| !r.is_error())
        .map(|r| r.total_us)
        .collect();
    if ok.len() < 50 {
        return None;
    }
    ok.sort_unstable();
    let by_op = reqs_by_op(&run.reqs);
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
    // A fixed seed: the same trace always gives the same estimate.
    let mut rng = Rng::new(0x7a11_c075);
    let mut hist = TailCutHistogram::default();
    let mut saving: HashMap<u64, u64> = HashMap::new();
    let (mut cut, mut blind_over) = (0u64, Vec::new());
    for q in &rows {
        if q.t_us < setup.blind_until_us {
            if !q.is_error() {
                blind_over.push(q.total_us);
            }
            continue;
        }
        let deadline = setup.params.deadline(&hist).map(|d| d.as_micros() as u64);
        match deadline {
            Some(d) if !q.is_error() && q.total_us > d => {
                let fast = ok.partition_point(|t| *t <= d);
                if fast == 0 {
                    hist.record(Duration::from_micros(q.total_us));
                    continue;
                }
                let redo = ok[rng.index(fast)];
                cut += 1;
                hist.record(Duration::from_micros(redo));
                let s = q.total_us.saturating_sub(d + setup.backoff_us + redo);
                let e = saving.entry(q.op_id).or_default();
                *e = (*e).max(s);
            }
            _ if !q.is_error() => hist.record(Duration::from_micros(q.total_us)),
            _ => {}
        }
    }
    let deadline_us = setup.params.deadline(&hist).map(|d| d.as_micros() as u64);
    // Stragglers the blind layer let through, judged by the deadline it would have held
    // (from every successful request when it never armed).
    let reference = deadline_us.or_else(|| {
        let mut all = TailCutHistogram::default();
        for t in &ok {
            all.record(Duration::from_micros(*t));
        }
        setup.params.deadline(&all).map(|d| d.as_micros() as u64)
    });
    let blind_count = reference.map_or(0, |d| blind_over.iter().filter(|t| **t > d).count()) as u64;
    let before = op_p99_ms(affected.iter().map(|o| o.latency_us));
    let after = op_p99_ms(affected.iter().map(|o| {
        o.latency_us
            .saturating_sub(saving.get(&o.op_id).copied().unwrap_or(0))
    }));
    let d_ms = deadline_us.map_or(0, |d| d / 1000);
    Some(TailCutReplay {
        prediction: Prediction {
            kind: "tail_cut".into(),
            setting: format!("tail_cut (deadline {d_ms} ms)"),
            metric: format!("p99_ms of operations issuing {api}"),
            current: before,
            predicted: after,
            predicted_lo: after,
            predicted_hi: before.max(after),
            requests_per_1k_ops_delta: cut as f64 * 1000.0 / affected.len() as f64,
            replayed_ops_count: affected.len() as u64,
            note: format!(
                "estimate: replayed {} operations through TailCutLayer's rule (p{} of recent {api} latencies × {}, at least {} ms); a cut request is re-issued after the retry backoff and takes a time drawn from the requests that finished in time",
                affected.len(),
                setup.params.percentile,
                setup.params.safety_factor,
                setup.params.min_deadline.as_millis()
            ),
        },
        deadline_us,
        cut_count: cut,
        blind_count,
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
        predicted_lo: 0.0,
        predicted_hi: 0.0,
        requests_per_1k_ops_delta: -(saved_reqs as f64) * 1000.0 / reads.len() as f64,
        replayed_ops_count: reads.len() as u64,
        note: format!(
            "estimate: replayed {} reads through an LRU; {:.0}% would hit (whole objects as they were read; real hit rates depend on how ranges are cached)",
            reads.len(),
            hits as f64 * 100.0 / reads.len() as f64
        ),
    }
    .bounded())
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
        report.spec.profile.workload.concurrency = 2;
        report.spec.profile.operator.layers.retry = Some(crate::operator::Retry {
            min_delay: crate::util::HumanDuration(std::time::Duration::from_millis(50)),
            jitter: true,
            ..Default::default()
        });
        Run { report, ops, reqs }
    }

    #[test]
    fn tail_cut_replay_follows_the_layer() {
        let run = straggler_run();
        let setup = TailCutSetup::of(&run.report).unwrap();
        // A long-lived client: 2.5 % stragglers, the deadline sits at the 50 ms floor.
        let t = tail_cut(&run, "GetObject", &setup.steady()).unwrap();
        assert_eq!(t.deadline_us, Some(50_000));
        let p = &t.prediction;
        assert!(p.current > 800.0, "{p:?}");
        // Cut at 50 ms, re-issued after a 75 ms backoff: ~135 ms instead of 840 ms.
        assert!(p.predicted < 200.0, "{p:?}");
        assert!(p.predicted_lo <= p.predicted && p.predicted <= p.predicted_hi);
        assert!((20.0..30.0).contains(&p.requests_per_1k_ops_delta), "{p:?}");
        assert_eq!(t.blind_count, 0);
        // The first 10 s of a fresh client (this run is 5 s long, no warmup): nothing is cut.
        let blind = tail_cut(&run, "GetObject", &setup).unwrap();
        assert_eq!(blind.cut_count, 0);
        assert!(blind.blind_count >= 20, "{blind:?}");
        assert_eq!(blind.prediction.predicted, blind.prediction.current);
        assert!((setup.blind_s(5.0) - 5.0).abs() < 1e-9);
        // Without retries a cut request would fail: no estimate.
        let mut r = run.report.clone();
        r.spec.profile.operator.layers.retry = None;
        assert!(TailCutSetup::of(&r).is_none());
    }

    #[test]
    fn tail_cut_cannot_cut_stragglers_that_set_the_p95() {
        let mut run = straggler_run();
        for (i, q) in run.reqs.iter_mut().enumerate() {
            if i % 10 == 3 {
                q.total_us = 840_000;
                q.ttfb_us = 839_990;
            }
        }
        let setup = TailCutSetup::of(&run.report).unwrap().steady();
        let t = tail_cut(&run, "GetObject", &setup).unwrap();
        assert!(t.deadline_us.unwrap() > 840_000, "{t:?}");
        assert!(t.cut_count < 10, "{t:?}");
    }

    #[test]
    fn cache_estimates() {
        let run = straggler_run();
        // Ten keys of 1 KiB: everything after the first touch hits.
        let c = cache(&run, 1 << 20).unwrap();
        assert!(c.requests_per_1k_ops_delta < -900.0, "{c:?}");
    }
}
