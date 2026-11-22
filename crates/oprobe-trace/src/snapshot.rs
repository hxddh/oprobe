//! Aggregated, serializable measurement results (part of the `oprobe.report/v3` schema).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::{Api, RequestRecord};

/// Latency distribution in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Quantiles {
    /// Samples in the distribution.
    #[serde(rename = "samples_count", alias = "count")]
    pub count: u64,
    #[serde(rename = "mean_ms")]
    pub mean: f64,
    #[serde(rename = "p50_ms")]
    pub p50: f64,
    #[serde(rename = "p90_ms")]
    pub p90: f64,
    #[serde(rename = "p99_ms")]
    pub p99: f64,
    #[serde(rename = "p999_ms")]
    pub p999: f64,
    #[serde(rename = "max_ms")]
    pub max: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Preparing,
    Warmup,
    Measure,
    Verify,
    Cleanup,
    Done,
}

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Preparing => "preparing dataset",
            Phase::Warmup => "warming up",
            Phase::Measure => "measuring",
            Phase::Verify => "verifying",
            Phase::Cleanup => "cleaning up",
            Phase::Done => "done",
        }
    }
}

/// Where the wall-clock time of a logical operation went (mean ms per op).
/// `queue + cache + backend + retry + client == latency mean` (exact for each op; see docs/metrics.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpBreakdown {
    /// Waiting for in-flight request permits / connections, with nothing on the wire.
    pub queue_ms: f64,
    /// Local cache lookups.
    pub cache_ms: f64,
    /// At least one backend request on the network (connect, send, TTFB, transfer).
    pub backend_ms: f64,
    /// Retry backoff: the retry layer sleeping between a failed attempt and the next one
    /// (with no request of the operation on the network or queued meanwhile).
    #[serde(default)]
    pub retry_ms: f64,
    /// Everything else: OpenDAL processing, signing, buffering, CRC, response consumption.
    pub client_ms: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerbStats {
    pub verb: String,
    #[serde(rename = "ops_count", alias = "count")]
    pub count: u64,
    #[serde(rename = "errors_count")]
    pub errors: u64,
    #[serde(rename = "throttled_count")]
    pub throttled: u64,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub latency: Quantiles,
    pub breakdown: OpBreakdown,
    #[serde(rename = "requests_per_op_mean")]
    pub requests_per_op: f64,
    #[serde(rename = "mean_bytes")]
    pub bytes_per_op: f64,
    #[serde(rename = "retries_count")]
    pub retries: u64,
}

/// Request stage distribution for one API (ms).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StageStats {
    pub queue: Quantiles,
    /// DNS+TCP+proxy+TLS; only requests that dialed are counted.
    pub connect: Quantiles,
    pub send: Quantiles,
    pub ttfb: Quantiles,
    pub body: Quantiles,
    pub stall: Quantiles,
    pub total: Quantiles,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApiStats {
    pub api: Api,
    #[serde(rename = "requests_count", alias = "count")]
    pub count: u64,
    #[serde(rename = "errors_count")]
    pub errors: u64,
    #[serde(rename = "throttled_count")]
    pub throttled: u64,
    #[serde(rename = "status_count")]
    pub status: BTreeMap<u16, u64>,
    pub reused_pct: f64,
    #[serde(rename = "up_bytes", alias = "bytes_up")]
    pub bytes_up: u64,
    #[serde(rename = "down_bytes", alias = "bytes_down")]
    pub bytes_down: u64,
    pub stages: StageStats,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConnStats {
    #[serde(rename = "dialed_count")]
    pub dialed: u64,
    #[serde(rename = "dial_failures_count")]
    pub dial_failures: u64,
    #[serde(rename = "requests_count")]
    pub requests: u64,
    #[serde(rename = "reused_requests_count")]
    pub reused_requests: u64,
    pub reuse_pct: f64,
    #[serde(rename = "http1_requests_count")]
    pub http1_requests: u64,
    #[serde(rename = "http2_requests_count")]
    pub http2_requests: u64,
    pub alpn: String,
    pub tls_version: String,
    pub cipher: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    pub remote_addrs: Vec<String>,
    pub dns: Quantiles,
    pub tcp: Quantiles,
    pub proxy_connect: Quantiles,
    pub tls: Quantiles,
    #[serde(rename = "peak_inflight_requests_count")]
    pub peak_inflight_requests: u64,
    /// Connection setup before warmup absorbed it (design v5 §3, M15): the dials and first
    /// operations of the warmup's cold start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_start: Option<ColdStartStats>,
}

/// The cold start of a run: the connections dialed at the start of warmup and the operations
/// that waited for them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ColdStartStats {
    pub connections_count: u64,
    /// DNS + TCP + proxy + TLS of a fresh connection.
    pub connect: Quantiles,
    /// Latency of the operations whose requests dialed.
    pub first_ops: Quantiles,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResilienceStats {
    #[serde(rename = "retries_count")]
    pub retries: u64,
    pub retry_backoff_ms: f64,
    #[serde(rename = "throttled_responses_count")]
    pub throttled_responses: u64,
    #[serde(rename = "status_429_count")]
    pub status_429: u64,
    #[serde(rename = "status_503_count")]
    pub status_503: u64,
    pub max_retry_after_ms: u64,
    #[serde(rename = "server_errors_count")]
    pub server_errors: u64,
    #[serde(rename = "client_errors_count")]
    pub client_errors: u64,
    #[serde(rename = "network_errors_count")]
    pub network_errors: u64,
    #[serde(rename = "failed_ops_count")]
    pub failed_ops: u64,
    #[serde(rename = "error_classes_count")]
    pub error_classes: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ByteStats {
    /// Bytes the application asked to read.
    #[serde(rename = "logical_read_bytes")]
    pub logical_read: u64,
    /// Bytes the application wrote.
    #[serde(rename = "logical_write_bytes")]
    pub logical_write: u64,
    /// Logical read bytes answered by the local cache.
    pub cache_hit_bytes: u64,
    /// Response body bytes downloaded from the backend.
    #[serde(rename = "backend_down_bytes")]
    pub backend_down: u64,
    /// Request body bytes uploaded to the backend.
    #[serde(rename = "backend_up_bytes")]
    pub backend_up: u64,
    /// Raw socket bytes incl. TLS records and HTTP headers.
    #[serde(rename = "wire_rx_bytes")]
    pub wire_rx: u64,
    #[serde(rename = "wire_tx_bytes")]
    pub wire_tx: u64,
    /// backend_down / logical_read (cache hits lower it, over-fetching raises it).
    #[serde(rename = "read_amplification_ratio")]
    pub read_amplification: f64,
    /// backend_down / (logical_read - cache_hit_bytes): over-fetch from range coalescing & chunking.
    pub overread_ratio: f64,
    /// backend_up / logical_write (retried uploads raise it).
    #[serde(rename = "write_amplification_ratio")]
    pub write_amplification: f64,
    /// Backend requests per logical operation.
    #[serde(rename = "request_amplification_ratio")]
    pub request_amplification: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CacheStats {
    pub enabled: bool,
    #[serde(rename = "lookups_count")]
    pub lookups: u64,
    #[serde(rename = "hits_count")]
    pub hits: u64,
    pub hit_ratio: f64,
    #[serde(rename = "mean_lookup_ms")]
    pub lookup_ms_mean: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IntegrityStats {
    #[serde(rename = "verified_count")]
    pub verified: u64,
    #[serde(rename = "corrupted_count")]
    pub corrupted: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimelinePoint {
    /// Start of this one-second interval, seconds from the measurement window start (the same
    /// convention as `heatmap.t_buckets_s` and `oprobe analyze --by second`).
    pub t_s: f64,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    #[serde(rename = "errors_count")]
    pub errors: u64,
    #[serde(rename = "throttled_count")]
    pub throttled: u64,
    /// Open-loop arrivals due in this interval that never started.
    #[serde(
        rename = "missed_arrivals_count",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub missed_arrivals: u64,
    /// Length of the interval: 1 s for the first ten minutes, 10 s afterwards.
    #[serde(default = "one_s")]
    pub len_s: f64,
    #[serde(default)]
    pub p90_ms: f64,
    #[serde(default)]
    pub p999_ms: f64,
    #[serde(default)]
    pub max_ms: f64,
    /// Peak requests in flight during the interval.
    #[serde(rename = "inflight_peak_count", default)]
    pub inflight_peak: u64,
    /// Connections dialed during the interval.
    #[serde(rename = "new_connections_count", default)]
    pub new_connections: u64,
    /// Client CPU in percent of one core (350 = 3.5 cores busy), when sampled in the interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    /// Operations started in the interval per latency row of the heatmap
    /// (`heatmap.latency_buckets_ms`): the heatmap is built from these, not from sampled rows.
    #[serde(
        rename = "latency_rows_count",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub latency_rows_count: Vec<u64>,
}

fn one_s() -> f64 {
    1.0
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlowOp {
    /// Id of the operation (its row in the run file).
    #[serde(default)]
    pub op_id: u64,
    #[serde(default)]
    pub key: String,
    pub verb: String,
    pub latency_ms: f64,
    pub at_s: f64,
    pub breakdown: OpBreakdown,
    #[serde(rename = "retries_count")]
    pub retries: u64,
    pub requests: Vec<RequestRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientStats {
    #[serde(rename = "cpu_cores_count")]
    pub cpu_cores: usize,
    /// Process CPU usage in percent of ONE core (e.g. 350% = 3.5 cores busy).
    pub cpu_pct: f64,
    #[serde(rename = "peak_cpu_pct")]
    pub cpu_pct_peak: f64,
    pub rss_mib: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpenLoopStats {
    #[serde(rename = "target_per_s")]
    pub target_rate: f64,
    #[serde(rename = "achieved_per_s")]
    pub achieved_rate: f64,
    /// Arrivals that never started: still queued behind `concurrency` busy slots when the
    /// window ended, or dropped after falling more than `knobs.timeout` behind schedule.
    /// Arrivals that waited for a slot and then ran are not missed: their queueing delay is
    /// part of their latency (measured from the intended start).
    #[serde(rename = "missed_count")]
    pub missed: u64,
    pub max_schedule_lag_ms: f64,
    /// Arrival process: `constant`, `poisson`, `ramp`, `step`, `diurnal` (`+poisson`), `replay`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arrivals: String,
    /// Arrivals due inside the window: started plus missed.
    #[serde(default)]
    pub intended_count: u64,
    /// Missed arrivals counted in the latency distribution as censored values (a lower bound:
    /// window end − intended start): `summary.latency` percentiles are then "≥" values.
    #[serde(default)]
    pub censored_count: u64,
    /// The `schedule` stage: intended → actual start of the operations that started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Quantiles>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Summary {
    #[serde(rename = "ops_count")]
    pub ops: u64,
    /// Successful operations per second (goodput; failed operations are not throughput).
    pub ops_per_s: f64,
    /// Operations per second, failed ones included.
    #[serde(default)]
    pub attempted_ops_per_s: f64,
    #[serde(rename = "failed_ops_count")]
    pub failed_ops: u64,
    #[serde(rename = "error_ratio")]
    pub error_rate: f64,
    /// Throttled responses (429/503) / requests: what the SLA's throttle limit checks.
    #[serde(default)]
    pub throttled_request_ratio: f64,
    /// Operations with at least one throttled response / operations.
    #[serde(default)]
    pub throttled_op_ratio: f64,
    pub mib_per_s: f64,
    pub read_mib_per_s: f64,
    pub write_mib_per_s: f64,
    pub latency: Quantiles,
    #[serde(rename = "inflight_ops_count")]
    pub inflight_ops: u64,
}

/// The load generator's own health over the measurement window (design v5 §3): what the
/// self probe and the recorder measured. `report.self` is built from it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SelfObservation {
    /// Measured observer cost per operation (recorder bookkeeping).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_per_op_us: Option<f64>,
    /// Tokio runtime workers' busy share over the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_busy_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_workers_count: Option<u64>,
    /// Deepest global (injection) queue seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_queue_depth_max_count: Option<u64>,
    /// p99 / max lateness of a 1 ms timer task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sched_lag_p99_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sched_lag_max_ms: Option<f64>,
    /// Process CPU over the window, as a share of the cores in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ratio: Option<f64>,
    pub cores_in_use_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_current_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_peak_bytes: Option<u64>,
    /// Lag p99 > 5 ms or CPU ≥ 90 % of the cores in use.
    pub client_saturated: bool,
}

/// Complete aggregated view of one measurement window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub phase: Phase,
    pub elapsed_s: f64,
    pub summary: Summary,
    pub verbs: Vec<VerbStats>,
    pub apis: Vec<ApiStats>,
    pub connections: ConnStats,
    pub resilience: ResilienceStats,
    pub bytes: ByteStats,
    pub cache: CacheStats,
    pub integrity: IntegrityStats,
    pub client: ClientStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_loop: Option<OpenLoopStats>,
    /// The run was stopped early (Ctrl-C / Stop): the window is shorter than configured.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    /// What OpenDAL actually did with the knobs (after the service's limits were applied).
    #[serde(default)]
    pub effective_knobs: EffectiveKnobs,
    pub timeline: Vec<TimelinePoint>,
    pub slow_ops: Vec<SlowOp>,
    /// The client's health during the window (filled by the runner when the run ends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_obs: Option<SelfObservation>,
    /// Prefixes, hot keys, size buckets, cold vs warm reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<crate::dims::DimensionStats>,
}

/// Knob values as OpenDAL applies them, which can differ from what was requested.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EffectiveKnobs {
    /// `knobs.chunk_size` as requested (bytes).
    #[serde(rename = "requested_chunk_bytes")]
    pub requested_chunk_size: u64,
    /// Chunk (multipart part size) OpenDAL writes with: the requested chunk clamped to the
    /// service's `write_multi_min_size ..= write_multi_max_size` (reads use it unchanged).
    #[serde(rename = "write_chunk_bytes")]
    pub write_chunk_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "service_min_part_bytes")]
    pub service_min_part_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "service_max_part_bytes")]
    pub service_max_part_size: Option<u64>,
}

impl EffectiveKnobs {
    /// OpenDAL changed the requested write chunk.
    pub fn write_chunk_clamped(&self) -> bool {
        self.requested_chunk_size != 0 && self.write_chunk_size != self.requested_chunk_size
    }
}

impl Snapshot {
    /// Aggregate request stage means across all APIs, weighted by request count (ms).
    pub fn request_stage_means(&self) -> RequestStageMeans {
        let mut m = RequestStageMeans::default();
        let mut n = 0.0;
        for a in &self.apis {
            let c = a.stages.total.count as f64;
            if c == 0.0 {
                continue;
            }
            n += c;
            m.queue += a.stages.queue.mean * c;
            // connect mean is over dialing requests; convert to per-request contribution
            m.connect += a.stages.connect.mean * a.stages.connect.count as f64;
            m.send += a.stages.send.mean * c;
            m.ttfb += a.stages.ttfb.mean * c;
            m.body += a.stages.body.mean * c;
            m.stall += a.stages.stall.mean * c;
            m.total += a.stages.total.mean * c;
        }
        if n > 0.0 {
            m.queue /= n;
            m.connect /= n;
            m.send /= n;
            m.ttfb /= n;
            m.body /= n;
            m.stall /= n;
            m.total /= n;
        }
        m
    }

    pub fn verb(&self, name: &str) -> Option<&VerbStats> {
        self.verbs.iter().find(|v| v.verb == name)
    }
}

/// Mean per-request stage durations (ms) across all APIs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RequestStageMeans {
    pub queue: f64,
    pub connect: f64,
    pub send: f64,
    pub ttfb: f64,
    pub body: f64,
    pub stall: f64,
    pub total: f64,
}

impl RequestStageMeans {
    pub fn stages(&self) -> [(&'static str, f64); 6] {
        [
            ("queue", self.queue),
            ("connect", self.connect),
            ("send", self.send),
            ("ttfb", self.ttfb),
            ("transfer", self.body),
            ("consumer stall", self.stall),
        ]
    }
}
