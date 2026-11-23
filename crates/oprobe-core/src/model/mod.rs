//! Shared data model (design v3 §4–§6, v4 §1): run ids, trace rows, the run spec and Report v4.
//!
//! Everything here is `Serialize + Deserialize + JsonSchema`; these types are the public,
//! machine-facing contract (run files, `-o json`, MCP, exporters).

pub mod build;
pub mod ids;
pub mod report;
pub mod sections;
pub mod spec;
pub mod trace_file;
/// Trace rows (`oprobe_trace::rows`).
pub use oprobe_trace::rows as trace;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use build::{ReportInput, build_report, build_run, workload_info};
pub use ids::RunId;
pub use report::*;
pub use sections::*;
pub use spec::{Overrides, RunSpec};
pub use trace::{OpBreakdownUs, OpRow, ReqDetail, ReqRow, TcpRow, TlsRow, TraceInfo, WireProtocol};

/// Format id of a run file (design §4).
pub const RUN_FORMAT: &str = "oprobe.run/6";

/// One complete run: its report and trace rows (what a `.oprobe` file holds).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Run {
    pub report: Report,
    pub ops: Vec<OpRow>,
    pub reqs: Vec<ReqRow>,
}

impl Run {
    pub fn id(&self) -> &str {
        &self.report.run.id
    }

    /// The operation row with `op_id`.
    pub fn op(&self, op_id: u64) -> Option<&OpRow> {
        self.ops.iter().find(|o| o.op_id == op_id)
    }

    /// Request rows of operation `op_id`, in start order.
    pub fn requests_of(&self, op_id: u64) -> Vec<&ReqRow> {
        let mut v: Vec<&ReqRow> = self.reqs.iter().filter(|r| r.op_id == op_id).collect();
        v.sort_by_key(|r| r.t_us);
        v
    }

    /// How many operations of the window each kept operation row stands for, when the rows
    /// were sampled (`report.sampling`); only weights other than 1 are listed (look ups
    /// default to 1).
    ///
    /// The samplers keep every failed or throttled operation and (in memory) the slowest 1 %,
    /// and a time-stratified share of the rest: those always-kept rows stand for themselves,
    /// and each other row of an interval for `(operations started in it − its always-kept
    /// rows) ÷ its other rows` (the timeline's exact counts; one ratio over the window for
    /// reports without them). Quantiles over sampled rows are taken with these weights, so
    /// the always-kept tail is not over-weighed.
    pub fn op_weights(&self) -> std::collections::HashMap<u64, f64> {
        use std::collections::HashMap;
        let s = &self.report.sampling;
        let mut out: HashMap<u64, f64> = HashMap::new();
        if s.seen_ops_count == 0 || s.kept_ops_count >= s.seen_ops_count {
            return out;
        }
        let measured: Vec<&OpRow> = self.ops.iter().filter(|o| o.t_us >= 0).collect();
        let forced = |o: &OpRow| !o.ok || o.throttled;
        // The slowest rows the in-memory sampler kept beyond the failed and throttled ones.
        let n_forced = measured.iter().filter(|o| forced(o)).count() as u64;
        let mut rest: Vec<&OpRow> = measured.iter().copied().filter(|o| !forced(o)).collect();
        rest.sort_by_key(|o| std::cmp::Reverse(o.latency_us));
        let slow = (s.always_kept_ops_count.saturating_sub(n_forced) as usize).min(rest.len());
        let sampled = &rest[slow..];
        if sampled.is_empty() {
            return out;
        }
        let tl = &self.report.timeline;
        let interval = |t_us: i64| -> Option<usize> {
            let t = t_us as f64 / 1e6;
            let i = tl.partition_point(|p| p.t_s <= t);
            (i > 0).then(|| i - 1)
        };
        let seen_in = |i: usize| -> u64 {
            let p = &tl[i];
            if p.ops_count > 0 {
                p.ops_count
            } else {
                p.latency_rows_count.iter().sum()
            }
        };
        let have_counts = !tl.is_empty() && (0..tl.len()).any(|i| seen_in(i) > 0);
        if have_counts {
            // Kept rows per interval: (always kept, sampled).
            let mut kept: HashMap<usize, (u64, u64)> = HashMap::new();
            for o in measured.iter().filter(|o| forced(o)).chain(&rest[..slow]) {
                if let Some(i) = interval(o.t_us) {
                    kept.entry(i).or_default().0 += 1;
                }
            }
            for o in sampled {
                if let Some(i) = interval(o.t_us) {
                    kept.entry(i).or_default().1 += 1;
                }
            }
            for o in sampled {
                let Some(i) = interval(o.t_us) else { continue };
                let (always, n) = kept[&i];
                let w = (seen_in(i).saturating_sub(always) as f64 / n as f64).max(1.0);
                if w != 1.0 {
                    out.insert(o.op_id, w);
                }
            }
        } else {
            let unseen = s
                .seen_ops_count
                .saturating_sub(n_forced + slow as u64)
                .max(sampled.len() as u64);
            let w = unseen as f64 / sampled.len() as f64;
            if w != 1.0 {
                out.extend(sampled.iter().map(|o| (o.op_id, w)));
            }
        }
        out
    }
}
