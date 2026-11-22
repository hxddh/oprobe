//! The worker protocol, `oprobe.worker/1` (design v5 §8): JSON over HTTP/2 (h2c, or TLS with
//! `--tls-cert`), every request authenticated with `Authorization: Bearer TOKEN`.
//!
//! | Request | Body → answer | |
//! |---|---|---|
//! | `GET /v1/hello` | → [`Hello`] | version, the worker's clock (NTP-style exchange), capabilities |
//! | `POST /v1/prepare` | [`Job`] → [`Ack`] | open the session and prepare the dataset |
//! | `POST /v1/start` | [`Start`] → [`Ack`] | run the window at a common instant (worker clock) |
//! | `GET /v1/status` | → [`Status`] | progress, every second |
//! | `POST /v1/cancel` | → [`Ack`] | stop now (the window ends, results are kept) |
//! | `GET /v1/result` | → [`WorkerResult`] (zstd JSON), or 202 while running | |

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::clients::ClientChoice;
use crate::config::Profile;
use crate::engine::CleanupSummary;
use crate::guard::Budgets;
use crate::model::RunSpec;
use crate::trace::{OpRow, ReqRow, SamplingStats, Snapshot, TraceInfo};

/// Protocol name and major version.
pub const PROTOCOL: &str = "oprobe.worker/1";

/// `GET /v1/hello`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Hello {
    pub protocol: String,
    /// The worker's oprobe version (must equal the coordinator's).
    pub version: String,
    /// The worker's wall clock when it answered (µs since the Unix epoch).
    pub clock_unix_us: i64,
    pub cpu_cores_count: usize,
    /// Clients it can drive (`opendal`, `aws_sdk`, `exec` when started with `--allow-exec`).
    pub clients: Vec<String>,
    /// A job is in progress.
    pub busy: bool,
}

/// `POST /v1/prepare`: one worker's share of a distributed run.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Job {
    pub run_id: String,
    /// Index of this worker (0-based) and the number of workers.
    pub index: u32,
    pub count: u32,
    /// The profile with its share (concurrency, rate, dataset), credentials included: sent
    /// only to loopback or TLS workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// Otherwise the replayable spec (secrets redacted): the worker resolves credentials from
    /// its own configuration and environment, as `oprobe run --from` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<RunSpec>,
    pub client: ClientChoice,
    /// The worker's own prefix: it writes and cleans up only under it.
    pub run_prefix: String,
    /// This worker's share of the budgets (the coordinator enforces the global ones).
    pub budgets: Budgets,
    pub read_only: bool,
    pub trace_max_requests_count: usize,
}

/// `POST /v1/start`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Start {
    /// When to start, on the worker's clock (µs since the Unix epoch).
    pub at_unix_us: i64,
}

/// A plain acknowledgement.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Ack {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where a worker is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Idle,
    Preparing,
    Ready,
    Running,
    Done,
    Failed,
}

/// `GET /v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Status {
    pub state: State,
    pub elapsed_s: f64,
    pub ops_count: u64,
    pub failed_ops_count: u64,
    pub requests_count: u64,
    /// Bytes on the wire, both ways (what byte budgets count).
    pub bytes: u64,
    pub ops_per_s: f64,
    /// The worker's latest live snapshot (the coordinator merges them for live progress).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Box<Snapshot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Trace rows on the wire.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rows {
    pub ops: Vec<OpRow>,
    pub reqs: Vec<ReqRow>,
    pub info: TraceInfo,
    pub sampling: WireSampling,
}

/// [`SamplingStats`] on the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSampling {
    pub seen_ops: u64,
    pub kept_ops: u64,
    pub seen_requests: u64,
    pub kept_requests: u64,
    pub always_kept_ops: u64,
}

impl From<SamplingStats> for WireSampling {
    fn from(s: SamplingStats) -> Self {
        Self {
            seen_ops: s.seen_ops,
            kept_ops: s.kept_ops,
            seen_requests: s.seen_requests,
            kept_requests: s.kept_requests,
            always_kept_ops: s.always_kept_ops,
        }
    }
}

impl From<WireSampling> for SamplingStats {
    fn from(s: WireSampling) -> Self {
        Self {
            seen_ops: s.seen_ops,
            kept_ops: s.kept_ops,
            seen_requests: s.seen_requests,
            kept_requests: s.kept_requests,
            always_kept_ops: s.always_kept_ops,
        }
    }
}

/// `GET /v1/result`: everything the coordinator merges.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerResult {
    pub snapshot: Snapshot,
    pub rows: Rows,
    pub cleanup: CleanupSummary,
    pub warnings: Vec<String>,
    /// The worker's profile after normalisation (target, effective mix).
    pub profile: Profile,
    /// When the measurement window opened, on the worker's clock (µs since the Unix epoch).
    pub window_start_unix_us: i64,
    /// How late the run started after the requested instant (ms).
    pub start_lateness_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<crate::model::ClientInfo>,
    /// The budget that stopped this worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breach: Option<String>,
    pub cancelled: bool,
}

/// Microseconds since the Unix epoch, now.
pub fn now_unix_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
