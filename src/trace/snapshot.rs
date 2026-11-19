//! Aggregated, serializable measurement results (part of the `oprobe.report/v2` schema).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::model::{Api, RequestRecord};

/// Latency distribution in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quantiles {
    pub count: u64,
    pub mean: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
/// `queue + cache + backend + client == latency mean` (exact for each op; see docs/metrics.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpBreakdown {
    /// Waiting for in-flight request permits / connections, with nothing on the wire.
    pub queue_ms: f64,
    /// Local cache lookups.
    pub cache_ms: f64,
    /// At least one backend request on the network (connect, send, TTFB, transfer).
    pub backend_ms: f64,
    /// Everything else: OpenDAL processing, signing, buffering, CRC, response consumption.
    pub client_ms: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VerbStats {
    pub verb: String,
    pub count: u64,
    pub errors: u64,
    pub throttled: u64,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub latency: Quantiles,
    pub breakdown: OpBreakdown,
    pub requests_per_op: f64,
    pub bytes_per_op: f64,
    pub retries: u64,
}

/// Request stage distribution for one API (ms).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiStats {
    pub api: Api,
    pub count: u64,
    pub errors: u64,
    pub throttled: u64,
    pub status: BTreeMap<u16, u64>,
    pub reused_pct: f64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub stages: StageStats,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnStats {
    pub dialed: u64,
    pub dial_failures: u64,
    pub requests: u64,
    pub reused_requests: u64,
    pub reuse_pct: f64,
    pub http1_requests: u64,
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
    pub peak_inflight_requests: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResilienceStats {
    pub retries: u64,
    pub retry_backoff_ms: f64,
    pub throttled_responses: u64,
    pub status_429: u64,
    pub status_503: u64,
    pub max_retry_after_ms: u64,
    pub server_errors: u64,
    pub client_errors: u64,
    pub network_errors: u64,
    pub failed_ops: u64,
    pub error_classes: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ByteStats {
    /// Bytes the application asked to read.
    pub logical_read: u64,
    /// Bytes the application wrote.
    pub logical_write: u64,
    /// Logical read bytes answered by the local cache.
    pub cache_hit_bytes: u64,
    /// Response body bytes downloaded from the backend.
    pub backend_down: u64,
    /// Request body bytes uploaded to the backend.
    pub backend_up: u64,
    /// Raw socket bytes incl. TLS records and HTTP headers.
    pub wire_rx: u64,
    pub wire_tx: u64,
    /// backend_down / logical_read (cache hits lower it, over-fetching raises it).
    pub read_amplification: f64,
    /// backend_down / (logical_read - cache_hit_bytes): over-fetch from range coalescing & chunking.
    pub overread_ratio: f64,
    /// backend_up / logical_write (retried uploads raise it).
    pub write_amplification: f64,
    /// Backend requests per logical operation.
    pub request_amplification: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheStats {
    pub enabled: bool,
    pub lookups: u64,
    pub hits: u64,
    pub hit_ratio: f64,
    pub lookup_ms_mean: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IntegrityStats {
    pub verified: u64,
    pub corrupted: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimelinePoint {
    pub t_s: f64,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub errors: u64,
    pub throttled: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SlowOp {
    pub verb: String,
    pub latency_ms: f64,
    pub at_s: f64,
    pub breakdown: OpBreakdown,
    pub retries: u64,
    pub requests: Vec<RequestRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientStats {
    pub cpu_cores: usize,
    /// Process CPU usage in percent of ONE core (e.g. 350% = 3.5 cores busy).
    pub cpu_pct: f64,
    pub cpu_pct_peak: f64,
    pub rss_mib: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenLoopStats {
    pub target_rate: f64,
    pub achieved_rate: f64,
    /// Ops that could not start because `concurrency` ops were already in flight.
    pub missed: u64,
    pub max_schedule_lag_ms: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub ops: u64,
    pub ops_per_s: f64,
    pub failed_ops: u64,
    pub error_rate: f64,
    pub mib_per_s: f64,
    pub read_mib_per_s: f64,
    pub write_mib_per_s: f64,
    pub latency: Quantiles,
    pub inflight_ops: u64,
}

/// Complete aggregated view of one measurement window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    pub timeline: Vec<TimelinePoint>,
    pub slow_ops: Vec<SlowOp>,
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
