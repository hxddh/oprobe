//! Whole-operation sampling (design v5 §2).
//!
//! An operation is kept with **all** of its requests, or not at all. Three sets:
//!
//! - **always kept:** every failed or throttled operation;
//! - **slowest:** the slowest 1 % of the operations seen (a bounded min-heap: an operation
//!   pushed out of it is offered to the reservoir);
//! - **reservoir:** a time-stratified reservoir of the rest — one reservoir per stratum
//!   (`stratum_us`, 1 s by default), all with the same capacity, which shrinks when the kept
//!   rows exceed the budget. Early and late parts of a run are represented alike.
//!
//! Memory is bounded by `max_rows` (operation rows plus request rows). Requests issued outside
//! any operation (op id 0) are kept in their own reservoir (a sixteenth of the budget).
//! Aggregates (the recorder's histograms) never depend on what is sampled: findings use the
//! exact counts (fixes M2).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};

use crate::rows::{OpRow, ReqRow};

/// How the sampler keeps rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleConfig {
    /// Budget of kept rows (operations + requests).
    pub max_rows: usize,
    /// Width of one time stratum.
    pub stratum_us: i64,
    /// Share of the slowest operations always kept.
    pub slow_ratio: f64,
}

impl Default for SampleConfig {
    fn default() -> Self {
        Self {
            max_rows: crate::log::DEFAULT_MAX_REQUESTS,
            stratum_us: 1_000_000,
            slow_ratio: 0.01,
        }
    }
}

/// What the sampler kept (summed over shards; `report.sampling`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SamplingStats {
    pub seen_ops: u64,
    pub kept_ops: u64,
    pub seen_requests: u64,
    pub kept_requests: u64,
    /// Kept because they failed, were throttled or were among the slowest.
    pub always_kept_ops: u64,
}

impl SamplingStats {
    pub fn add(&mut self, o: &SamplingStats) {
        self.seen_ops += o.seen_ops;
        self.kept_ops += o.kept_ops;
        self.seen_requests += o.seen_requests;
        self.kept_requests += o.kept_requests;
        self.always_kept_ops += o.always_kept_ops;
    }

    /// Kept / seen operations (1 when nothing was seen).
    pub fn kept_ratio(&self) -> f64 {
        if self.seen_ops == 0 {
            1.0
        } else {
            self.kept_ops as f64 / self.seen_ops as f64
        }
    }
}

/// One operation and its requests.
#[derive(Clone, Debug)]
pub struct WholeOp {
    pub op: OpRow,
    pub reqs: Vec<ReqRow>,
}

impl WholeOp {
    fn rows(&self) -> usize {
        1 + self.reqs.len()
    }
}

#[derive(Debug, Default)]
struct Stratum {
    items: Vec<WholeOp>,
    seen: u64,
}

/// Slow-set entry: ordered by latency, then op id (deterministic).
#[derive(Debug)]
struct Slow(WholeOp);

impl PartialEq for Slow {
    fn eq(&self, o: &Self) -> bool {
        self.key() == o.key()
    }
}
impl Eq for Slow {}
impl PartialOrd for Slow {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Slow {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.key().cmp(&o.key())
    }
}
impl Slow {
    fn key(&self) -> (u64, u64) {
        (self.0.op.latency_us, self.0.op.op_id)
    }
}

/// The whole-operation sampler of one recorder shard.
#[derive(Debug)]
pub struct OpSampler {
    cfg: SampleConfig,
    strata: BTreeMap<i64, Stratum>,
    /// Capacity of every stratum (`usize::MAX` until the budget is first exceeded).
    stratum_cap: usize,
    always: Vec<WholeOp>,
    slow: BinaryHeap<Reverse<Slow>>,
    /// Rows (operations plus requests) of `slow`.
    slow_rows: usize,
    unscoped: Vec<ReqRow>,
    unscoped_seen: u64,
    kept_rows: usize,
    seen_ops: u64,
    seen_reqs: u64,
    rng: SmallRng,
}

impl Default for OpSampler {
    fn default() -> Self {
        Self::new(SampleConfig::default())
    }
}

impl OpSampler {
    pub fn new(cfg: SampleConfig) -> Self {
        Self {
            cfg,
            strata: BTreeMap::new(),
            stratum_cap: usize::MAX,
            always: Vec::new(),
            slow: BinaryHeap::new(),
            slow_rows: 0,
            unscoped: Vec::new(),
            unscoped_seen: 0,
            kept_rows: 0,
            seen_ops: 0,
            seen_reqs: 0,
            rng: SmallRng::seed_from_u64(0x0b5e_7ace),
        }
    }

    /// About how many bytes the kept rows take as JSON (a save reserves room for them
    /// before it takes them).
    pub fn json_len(&self) -> u64 {
        let whole = |w: &WholeOp| crate::file::json_len(&w.op) + crate::file::json_len(&w.reqs);
        self.always.iter().map(whole).sum::<u64>()
            + self.slow.iter().map(|Reverse(Slow(w))| whole(w)).sum::<u64>()
            + self
                .strata
                .values()
                .flat_map(|s| s.items.iter())
                .map(whole)
                .sum::<u64>()
            + crate::file::json_len(&self.unscoped)
    }

    pub fn config(&self) -> SampleConfig {
        self.cfg
    }

    /// Change the row budget (applies from the next offer on).
    pub fn set_max_rows(&mut self, max_rows: usize) {
        self.cfg.max_rows = max_rows;
    }

    /// Rows kept now (operations + requests, unscoped requests included).
    pub fn kept_rows(&self) -> usize {
        self.kept_rows + self.unscoped.len()
    }

    /// Offer one completed operation with all its requests.
    pub fn offer(&mut self, op: OpRow, reqs: Vec<ReqRow>) {
        self.seen_ops += 1;
        self.seen_reqs += reqs.len() as u64;
        let w = WholeOp { op, reqs };
        if !w.op.ok || w.op.throttled {
            // Every failed or throttled operation, while it fits in half the budget.
            if self.always_rows() + w.rows() <= self.cfg.max_rows / 2 {
                self.kept_rows += w.rows();
                self.always.push(w);
                return;
            }
        }
        let slow_cap = ((self.seen_ops as f64 * self.cfg.slow_ratio).ceil() as usize)
            .clamp(1, (self.cfg.max_rows / 10).max(1));
        // The slow set holds at most a quarter of the row budget (failures hold at most half):
        // capped only in operations, 9.3.3 kept 50 times the budget of operations with many
        // requests, which the reservoir's shrinking could not make up for.
        let slow_rows_cap = (self.cfg.max_rows / 4).max(1);
        if self.cfg.max_rows > 0 {
            let qualifies = self.slow.len() < slow_cap
                || self
                    .slow
                    .peek()
                    .is_some_and(|Reverse(m)| w.op.latency_us > m.0.op.latency_us);
            if qualifies {
                self.kept_rows += w.rows();
                self.slow_rows += w.rows();
                self.slow.push(Reverse(Slow(w)));
                // Over either cap: the fastest of the slow set go to the reservoir.
                while self.slow.len() > slow_cap || self.slow_rows > slow_rows_cap {
                    let Some(Reverse(Slow(out))) = self.slow.pop() else {
                        break;
                    };
                    self.kept_rows -= out.rows();
                    self.slow_rows -= out.rows();
                    self.reservoir(out);
                }
                self.shrink();
                return;
            }
        }
        self.reservoir(w);
        self.shrink();
    }

    /// A request issued outside any operation.
    pub fn offer_unscoped(&mut self, row: ReqRow) {
        self.seen_reqs += 1;
        self.unscoped_seen += 1;
        let cap = (self.cfg.max_rows / 16).max(usize::from(self.cfg.max_rows > 0));
        if self.unscoped.len() < cap {
            self.unscoped.push(row);
        } else if cap > 0 {
            let j = self.rng.random_range(0..self.unscoped_seen);
            if (j as usize) < cap {
                self.unscoped[j as usize] = row;
            }
        }
    }

    fn always_rows(&self) -> usize {
        self.always.iter().map(WholeOp::rows).sum()
    }

    fn reservoir(&mut self, w: WholeOp) {
        if self.cfg.max_rows == 0 {
            return;
        }
        let key = w.op.t_us.div_euclid(self.cfg.stratum_us.max(1));
        let cap = self.stratum_cap;
        let s = self.strata.entry(key).or_default();
        s.seen += 1;
        if s.items.len() < cap {
            self.kept_rows += w.rows();
            s.items.push(w);
        } else {
            let j = self.rng.random_range(0..s.seen);
            if (j as usize) < cap {
                let old = std::mem::replace(&mut s.items[j as usize], w);
                self.kept_rows = self.kept_rows + s.items[j as usize].rows() - old.rows();
            }
        }
    }

    /// Lower the stratum capacity (evicting at random) until the kept rows fit the budget.
    fn shrink(&mut self) {
        while self.kept_rows > self.cfg.max_rows {
            let largest = self
                .strata
                .values()
                .map(|s| s.items.len())
                .max()
                .unwrap_or(0);
            if largest <= 1 {
                // The reservoir cannot shrink further: drop whole strata, oldest first.
                let Some(k) = self.strata.keys().next().copied() else {
                    break;
                };
                let s = self.strata.remove(&k).unwrap_or_default();
                self.kept_rows -= s.items.iter().map(WholeOp::rows).sum::<usize>();
                continue;
            }
            self.stratum_cap = (largest * 3 / 4).max(1);
            let cap = self.stratum_cap;
            for s in self.strata.values_mut() {
                while s.items.len() > cap {
                    let j = self.rng.random_range(0..s.items.len());
                    let out = s.items.swap_remove(j);
                    self.kept_rows -= out.rows();
                }
            }
        }
    }

    /// The kept operations, their requests and the unscoped requests (unsorted), and what
    /// was kept; the sampler is reset.
    pub fn take(&mut self) -> (Vec<OpRow>, Vec<ReqRow>, SamplingStats) {
        let mut ops = Vec::new();
        let mut reqs = Vec::new();
        let always_kept = (self.always.len() + self.slow.len()) as u64;
        let strata = std::mem::take(&mut self.strata);
        let total_reservoir_seen = self.seen_ops.saturating_sub(always_kept);
        let surviving_strata_seen: u64 = strata
            .values()
            .filter(|s| !s.items.is_empty())
            .map(|s| s.seen)
            .sum();
        let stratum_scale = if surviving_strata_seen > 0 && total_reservoir_seen > surviving_strata_seen {
            total_reservoir_seen as f64 / surviving_strata_seen as f64
        } else {
            1.0
        };
        let mut push = |mut w: WholeOp, weight: f64| {
            w.op.weight = weight.max(1.0);
            ops.push(w.op);
            reqs.extend(w.reqs);
        };
        for w in self.always.drain(..) {
            push(w, 1.0);
        }
        for Reverse(Slow(w)) in std::mem::take(&mut self.slow).into_vec() {
            push(w, 1.0);
        }
        for (_, s) in strata {
            if s.items.is_empty() {
                continue;
            }
            let w_stratum = (s.seen as f64 / s.items.len() as f64) * stratum_scale;
            for w in s.items {
                push(w, w_stratum);
            }
        }
        let stats = SamplingStats {
            seen_ops: self.seen_ops,
            kept_ops: ops.len() as u64,
            seen_requests: self.seen_reqs,
            kept_requests: (reqs.len() + self.unscoped.len()) as u64,
            always_kept_ops: always_kept,
        };
        reqs.append(&mut self.unscoped);
        let cfg = self.cfg;
        *self = Self::new(cfg);
        (ops, reqs, stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{op_row, req_row};
    use crate::record::{Api, Protocol, RequestRecord, RequestTiming};

    fn req(op: u64, status: u16) -> ReqRow {
        let r = RequestRecord {
            operation: None,
            api: Api::GetObject,
            method: "GET".into(),
            status,
            protocol: Protocol::Http1,
            reused: true,
            conn_id: 1,
            start_offset_us: 0,
            timing: RequestTiming {
                total_us: 10,
                ..Default::default()
            },
            bytes_up: 0,
            bytes_down: 1,
            request_id: None,
            retry_after_ms: None,
            error: None,
            remote: None,
            server: None,
            service_op: None,
            service: None,
            tls: None,
            tcp: None,
            detail: None,
        };
        req_row(r, op, 0, 1)
    }

    fn op(id: u64, t_us: i64, latency_us: u64, ok: bool) -> OpRow {
        op_row(
            id,
            "read",
            format!("k{id}"),
            t_us,
            latency_us,
            1,
            (!ok).then(|| "boom".to_owned()),
            false,
            0,
            [0; 5],
            2,
        )
    }

    /// T7: operations with many requests and rising latency stay within the row budget.
    /// 9.3.3 capped the slow set in operations only: one shard of 625 rows, fed 20 000
    /// operations of 512 requests each, kept 31 806 rows.
    #[test]
    fn the_slow_set_stays_within_the_row_budget() {
        let mut s = OpSampler::new(SampleConfig {
            max_rows: 625,
            ..SampleConfig::default()
        });
        let mut most = 0;
        for i in 1..=20_000u64 {
            let reqs = (0..512).map(|_| req(i, 200)).collect();
            s.offer(op(i, (i as i64) * 1_000, i, true), reqs);
            most = most.max(s.kept_rows());
        }
        assert!(most <= 625, "kept {most} rows of a budget of 625");
        let (ops, reqs, _) = s.take();
        // Whole operations still: each kept one has all its requests.
        assert_eq!(reqs.len(), ops.len() * 512);
    }

    #[test]
    fn keeps_whole_operations_failures_and_the_slowest() {
        let mut s = OpSampler::new(SampleConfig {
            max_rows: 3_000,
            ..SampleConfig::default()
        });
        for i in 1..=10_000u64 {
            let ok = i % 1000 != 0;
            let lat = if i == 7_777 { 1_000_000 } else { 100 + i % 50 };
            s.offer(
                op(i, (i as i64) * 1_000, lat, ok),
                vec![req(i, if ok { 200 } else { 500 }), req(i, 200)],
            );
            assert!(s.kept_rows() <= 3_000);
        }
        let (ops, reqs, st) = s.take();
        assert_eq!(st.seen_ops, 10_000);
        assert_eq!(st.kept_ops as usize, ops.len());
        assert!(ops.len() > 500, "{}", ops.len());
        // Every kept op has all of its requests; no request without its op.
        for o in &ops {
            assert_eq!(reqs.iter().filter(|r| r.op_id == o.op_id).count(), 2);
        }
        assert_eq!(reqs.len(), ops.len() * 2);
        // Every failure and the slowest operation are kept.
        assert_eq!(ops.iter().filter(|o| !o.ok).count(), 10);
        assert!(ops.iter().any(|o| o.op_id == 7_777));
        // Strata cover the whole run (10 s): early and late operations are represented.
        assert!(ops.iter().any(|o| o.t_us < 1_000_000 && o.ok));
        assert!(ops.iter().any(|o| o.t_us > 9_000_000 && o.ok));
        assert!(st.kept_ratio() < 1.0);
        // Exact inverse-probability weights sum to seen_ops; forced/slow rows have weight 1.
        let total_w: f64 = ops.iter().map(|o| o.weight).sum();
        assert!((total_w - st.seen_ops as f64).abs() < 1e-6, "{total_w}");
        assert!(ops.iter().filter(|o| !o.ok).all(|o| (o.weight - 1.0).abs() < 1e-9));
    }

    #[test]
    fn keeps_everything_under_the_budget() {
        let mut s = OpSampler::default();
        for i in 1..=100u64 {
            s.offer(op(i, 0, 10, true), vec![req(i, 200)]);
        }
        s.offer_unscoped(req(0, 200));
        let (ops, reqs, st) = s.take();
        assert_eq!(ops.len(), 100);
        assert_eq!(reqs.len(), 101);
        assert_eq!(st.kept_ratio(), 1.0);
        assert_eq!(st.kept_requests, 101);
    }
}
