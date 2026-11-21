//! Shared data model (design v3 §4–§6, v4 §1): run ids, trace rows, the run spec and Report v4.
//!
//! Everything here is `Serialize + Deserialize + JsonSchema`; these types are the public,
//! machine-facing contract (run files, `-o json`, MCP, exporters).

pub mod build;
pub mod ids;
pub mod report;
pub mod spec;
pub mod trace;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use build::{ReportInput, build_report, build_run, workload_info};
pub use ids::RunId;
pub use report::*;
pub use spec::{Overrides, RunSpec};
pub use trace::{OpBreakdownUs, OpRow, ReqRow, TcpRow, TlsRow, WireProtocol};

/// Format id of a run file (design §4).
pub const RUN_FORMAT: &str = "oprobe.run/4";

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
}
