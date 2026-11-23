//! Aggregated, serializable measurement results (part of the `oprobe.report/v3` schema).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::{Api, RequestRecord};

/// Latency distribution in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct ColdStartStats {
    pub connections_count: u64,
    /// DNS + TCP + proxy + TLS of a fresh connection.
    pub connect: Quantiles,
    /// Latency of the operations whose requests dialed.
    pub first_ops: Quantiles,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct IntegrityStats {
    #[serde(rename = "verified_count")]
    pub verified: u64,
    #[serde(rename = "corrupted_count")]
    pub corrupted: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
/// One interval of the measurement window.
///
/// **When things count.** Operation counts (`ops_*`, `ok_ops_count`, `throttled_ops_count`),
/// bytes, the latency quantiles and `latency_rows_count` belong to the interval the operation
/// **started** in (they describe the same operations). Events count in the interval they
/// **happened** in: a request when it was sent (`requests_count`), a throttled response when
/// it arrived (`throttled_count`), a retry when it was issued (`retries_count`) and a failure
/// when the operation ended (`errors_count`) — a 503 at +7 s is drawn at +7 s, not at the
/// second its operation began.
pub struct TimelinePoint {
    /// Start of the interval, seconds from the measurement window start (the same convention
    /// as `heatmap.t_buckets_s`).
    pub t_s: f64,
    /// Operations started in the interval per second, failed ones included (the attempted
    /// rate; `goodput_ops_per_s` counts successes only).
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    /// Operations that failed in the interval (counted when they ended).
    #[serde(rename = "errors_count")]
    pub errors: u64,
    /// Throttled responses (429/503) received in the interval, retries included (counted when
    /// each response arrived). Before 6.3 this counted throttled operations by their start.
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
    /// Operations started in the interval (the sample count behind its quantiles).
    #[serde(default)]
    pub ops_count: u64,
    /// Operations started in the interval that succeeded.
    #[serde(default)]
    pub ok_ops_count: u64,
    /// Goodput: successful operations started in the interval per second (failed operations
    /// never count; `ops_per_s` is the attempted rate).
    #[serde(default)]
    pub goodput_ops_per_s: f64,
    /// Operations started in the interval that saw at least one throttled response.
    #[serde(rename = "throttled_ops_count", default)]
    pub throttled_ops: u64,
    /// Backend requests sent in the interval: every attempt, retries included.
    #[serde(default)]
    pub requests_count: u64,
    /// `requests_count` per second of the interval.
    #[serde(default)]
    pub requests_per_s: f64,
    /// Retries issued in the interval (when the retry layer sent the next attempt).
    #[serde(default)]
    pub retries_count: u64,
    /// The interval's counters per verb (`read`, `stat`, …), so a view can filter by verb.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verbs: Vec<VerbInterval>,
}

/// One verb's part of a [`TimelinePoint`], with the same time conventions: operations,
/// successes, latency rows and the latency sum by operation start; failures when they ended;
/// throttled responses when they arrived.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct VerbInterval {
    pub verb: String,
    #[serde(default)]
    pub ops_count: u64,
    #[serde(default)]
    pub ok_ops_count: u64,
    #[serde(default)]
    pub errors_count: u64,
    /// Throttled responses received in the interval by this verb's operations.
    #[serde(default)]
    pub throttled_count: u64,
    /// Sum of the latencies of the operations started in the interval (mean = sum ÷ ops).
    #[serde(default)]
    pub latency_sum_ms: f64,
    /// Operations started in the interval per heatmap latency row (as
    /// `TimelinePoint::latency_rows_count`): quantiles at row resolution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latency_rows_count: Vec<u64>,
}

/// The run's whole operation latency distribution as a log-scale histogram with
/// [`LatencyHistogram::BINS_PER_DECADE`] bins per decade (each bin ≈ 12 % wide): every
/// operation of the window, never sampled. Only non-empty bins are listed, ascending.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct LatencyHistogram {
    /// Bins per decade of the log scale.
    #[serde(rename = "bins_per_decade_count")]
    pub bins_per_decade: u32,
    /// Lower bound of each non-empty bin (ms).
    pub lower_ms: Vec<f64>,
    /// Upper bound of each non-empty bin (ms, exclusive).
    pub upper_ms: Vec<f64>,
    /// Operations per bin.
    pub counts: Vec<u64>,
    /// The slowest operation (ms).
    pub max_ms: f64,
}

impl LatencyHistogram {
    /// Bins per decade: bins 1.122× wide (6.6 ms and 9.9 ms fall three bins apart).
    pub const BINS_PER_DECADE: u32 = 20;

    /// Nothing recorded.
    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|c| *c == 0)
    }

    /// Operations in the histogram.
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The bin of a latency in µs (bin 0 starts at 1 µs).
    pub fn bin_of(us: u64) -> i32 {
        ((us.max(1) as f64).log10() * f64::from(Self::BINS_PER_DECADE) + 1e-9).floor() as i32
    }

    /// `[lower, upper)` of bin `i` in ms.
    pub fn bounds_ms(i: i32) -> (f64, f64) {
        let b = f64::from(Self::BINS_PER_DECADE);
        (
            10f64.powf(f64::from(i) / b) / 1000.0,
            10f64.powf(f64::from(i + 1) / b) / 1000.0,
        )
    }

    /// The histogram of `(latency µs, count)` pairs.
    pub fn from_counts(values: impl IntoIterator<Item = (u64, u64)>) -> Self {
        let mut bins: BTreeMap<i32, u64> = BTreeMap::new();
        let mut max_us = 0u64;
        for (us, n) in values {
            if n == 0 {
                continue;
            }
            *bins.entry(Self::bin_of(us)).or_default() += n;
            max_us = max_us.max(us);
        }
        let mut h = Self {
            bins_per_decade: Self::BINS_PER_DECADE,
            max_ms: max_us as f64 / 1000.0,
            ..Default::default()
        };
        for (i, n) in bins {
            let (lo, hi) = Self::bounds_ms(i);
            h.lower_ms.push(lo);
            h.upper_ms.push(hi);
            h.counts.push(n);
        }
        h
    }

    /// The latency (ms) at quantile `q` (0–1): the upper bound of the bin that reaches it,
    /// never above the slowest operation. `None` when empty.
    pub fn quantile_ms(&self, q: f64) -> Option<f64> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let rank = ((total as f64 * q.clamp(0.0, 1.0)).ceil() as u64).max(1);
        let mut acc = 0u64;
        for (i, n) in self.counts.iter().enumerate() {
            acc += n;
            if acc >= rank {
                return Some(self.upper_ms[i].min(self.max_ms).max(self.lower_ms[i]));
            }
        }
        Some(self.max_ms)
    }
}

fn one_s() -> f64 {
    1.0
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct ClientStats {
    #[serde(rename = "cpu_cores_count")]
    pub cpu_cores: usize,
    /// Process CPU usage in percent of ONE core (e.g. 350% = 3.5 cores busy).
    pub cpu_pct: f64,
    #[serde(rename = "peak_cpu_pct")]
    pub cpu_pct_peak: f64,
    pub rss_mib: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
    /// Every operation's latency (censored open-loop arrivals included, as in
    /// `summary.latency`) as a fine log-scale histogram.
    #[serde(default, skip_serializing_if = "LatencyHistogram::is_empty")]
    pub latency_histogram: LatencyHistogram,
}

/// Knob values as OpenDAL applies them, which can differ from what was requested.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
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
