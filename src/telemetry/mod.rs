//! White-box storage telemetry contracts, 7-stage microsecond waterfall,
//! per-verb isolated HDR histograms, slow-request `AwaitTree` call-stack traces,
//! remote cleanup audit records, and root-cause bottleneck diagnosis.

pub mod diagnosis;
pub mod registry;

use serde::{Deserialize, Serialize};

use crate::net_probe::EndpointHandshakeSummary;
pub use diagnosis::{diagnose_runtime_error, ActionableErrorHint, BottleneckDiagnosis, Severity};
pub use registry::{MeasurementPhase, OpTraceGuard, OpTraceScope, TelemetryRegistry};

/// Seven-stage white-box latency waterfall (mean per measured operation/request, in microseconds),
/// decomposed strictly from real middleware and socket-level instrumentation without heuristics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StageWaterfall {
    /// Stage 1: Client concurrency semaphore queue wait (`ConcurrentLimitLayer`).
    pub client_queue_us: u64,
    /// Stage 2: Local hybrid cache lookup & slice time (`WhiteboxCacheLayer`).
    pub local_cache_us: u64,
    /// Stage 3: Total connection setup & checkout (`dns_lookup_us + tcp_connect_us + tls_handshake_us + pool_checkout_us`).
    pub dns_tcp_tls_us: u64,
    /// Sub-stage 3a: DNS lookup duration across cold dials averaged per HTTP transaction.
    pub dns_lookup_us: u64,
    /// Sub-stage 3b: TCP SYN -> SYN-ACK connect duration averaged per HTTP transaction.
    pub tcp_connect_us: u64,
    /// Sub-stage 3c: TLS 1.2/1.3 + ALPN handshake duration averaged per HTTP transaction.
    pub tls_handshake_us: u64,
    /// Sub-stage 3d: Connection pool checkout + HTTP request header/body wire write duration.
    pub pool_checkout_us: u64,
    /// Stage 4: Storage gateway Time-To-First-Byte (from socket handshake/request-write completion to HTTP response headers).
    pub remote_ttfb_us: u64,
    /// Stage 5: Streaming payload transfer (from HTTP response headers to final body frame EOF).
    pub body_stream_us: u64,
    /// Stage 6: Multipart upload server merge/commit (`CompleteMultipartUpload` / `oio::Write::close`).
    pub multipart_merge_us: u64,
    /// Stage 7: End-to-end client CRC32C payload integrity verification.
    pub verify_crc_us: u64,
}

impl StageWaterfall {
    /// Total mean pipeline latency in microseconds across all 7 non-overlapping stages.
    pub fn total_us(&self) -> u64 {
        self.client_queue_us
            + self.local_cache_us
            + self.dns_tcp_tls_us
            + self.remote_ttfb_us
            + self.body_stream_us
            + self.multipart_merge_us
            + self.verify_crc_us
    }

    /// Total mean pipeline latency in milliseconds.
    pub fn total_ms(&self) -> f64 {
        self.total_us() as f64 / 1000.0
    }

    /// Returns `(stage_label, latency_ms, share_pct)` for all 7 white-box stages.
    pub fn breakdown_pct(&self) -> [(&'static str, f64, f64); 7] {
        let tot = self.total_us().max(1) as f64;
        let pct = |v: u64| (v as f64 / tot) * 100.0;
        let ms = |v: u64| v as f64 / 1000.0;
        [
            (
                "1.ClientQueue (Semaphore)",
                ms(self.client_queue_us),
                pct(self.client_queue_us),
            ),
            (
                "2.LocalCache (Moka W-TinyLFU)",
                ms(self.local_cache_us),
                pct(self.local_cache_us),
            ),
            (
                "3.DNS/TCP/TLS & Pool",
                ms(self.dns_tcp_tls_us),
                pct(self.dns_tcp_tls_us),
            ),
            (
                "4.Gateway TTFB",
                ms(self.remote_ttfb_us),
                pct(self.remote_ttfb_us),
            ),
            (
                "5.Body Stream Transfer",
                ms(self.body_stream_us),
                pct(self.body_stream_us),
            ),
            (
                "6.Multipart Merge/Commit",
                ms(self.multipart_merge_us),
                pct(self.multipart_merge_us),
            ),
            (
                "7.CRC32C Integrity Verify",
                ms(self.verify_crc_us),
                pct(self.verify_crc_us),
            ),
        ]
    }
}

/// Real HDR latency quantiles (P50 / P95 / P99 in ms) for individual low-level network & storage stages.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StageQuantiles {
    pub cold_dns_p50_ms: f64,
    pub cold_dns_p99_ms: f64,
    pub cold_tcp_p50_ms: f64,
    pub cold_tcp_p99_ms: f64,
    pub cold_tls_p50_ms: f64,
    pub cold_tls_p99_ms: f64,
    pub gateway_ttfb_p50_ms: f64,
    pub gateway_ttfb_p95_ms: f64,
    pub gateway_ttfb_p99_ms: f64,
    pub body_stream_p50_ms: f64,
    pub body_stream_p99_ms: f64,
    pub multipart_merge_p50_ms: f64,
    pub multipart_merge_p99_ms: f64,
}

/// Per-operation-verb isolated HDR quantile distribution (preventing fast `stat` calls from diluting `read`/`write` tails).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VerbMetricsRow {
    pub verb: String,
    pub count: u64,
    pub errors: u64,
    pub ops_per_sec: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub max_ms: f64,
    pub stddev_ms: f64,
    pub throughput_mib_s: f64,
    pub single_stream_mib_s: f64,
    pub throttled_count: u64,
    pub throttle_429_503_pct: f64,
    pub retry_count: u64,
}

/// Sliding 100ms time-series sample driving TUI live Sparklines and CLI live progress.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeSeriesPoint {
    pub elapsed_ms: u64,
    pub throughput_mib_s: f64,
    pub p99_ms: f64,
    pub inflight: usize,
    pub ops_completed: u64,
}

/// Captured async call-stack trace of an observed slow storage operation via `await-tree`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SlowRequestTrace {
    pub task_id: u64,
    pub root_span: String,
    pub observed_ms: f64,
    pub tree_dump: String,
}

/// Remote test object lifecycle & automatic cleanup audit summary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CleanupSummary {
    pub auto_cleanup_enabled: bool,
    pub run_prefix: String,
    pub objects_created: u64,
    pub objects_deleted: u64,
    pub remaining_objects: u64,
    pub bytes_cleaned: u64,
    pub cleanup_ms: f64,
    pub verified_clean: bool,
}

/// Transport, connection pool, amplification, cache, and loadgen host resource snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TransportAndHostSnapshot {
    pub socket_reuse_pct: f64,
    pub cold_connections_dialed: u64,
    pub reused_connections: u64,
    pub negotiated_alpn: String,
    pub tls_version: String,
    pub cipher_suite: String,
    pub active_inflight: usize,
    pub peak_inflight: usize,
    pub total_http_requests: u64,
    pub class_a_rpcs: u64,
    pub class_b_rpcs: u64,
    pub multipart_initiations: u64,
    pub multipart_parts_uploaded: u64,
    pub multipart_completions: u64,
    pub http_200_count: u64,
    pub http_206_range_count: u64,
    pub http_403_auth_count: u64,
    pub http_404_count: u64,
    pub http_429_count: u64,
    pub http_500_count: u64,
    pub http_503_slowdown_count: u64,
    pub http_other_err_count: u64,
    pub network_err_count: u64,
    pub retry_attempts: u64,
    pub retry_backoff_ms: f64,
    pub tail_cut_cancels: u64,
    pub cache_lookups: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_hit_ratio_pct: f64,
    pub cache_bytes_served: u64,
    pub logical_ops: u64,
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub logical_bytes: u64,
    pub storage_read_bytes: u64,
    pub physical_wire_bytes: u64,
    pub wire_bytes_rx: u64,
    pub wire_bytes_tx: u64,
    pub coalesced_gap_waste_bytes: u64,
    /// Storage read amplification (`storage_read_bytes / uncached_logical_read_bytes >= 1.0`).
    pub read_amplification: f64,
    /// Effective wire-to-logical read ratio (`wire_bytes_rx / logical_read_bytes`, `< 1.0` when local cache saves remote I/O).
    pub net_wire_read_ratio: f64,
    /// Wire-to-logical write amplification (`wire_bytes_tx / logical_write_bytes`, `> 1.0` when retries re-upload parts).
    pub write_amplification: f64,
    /// Storage RPC amplification (`backend_rpcs / logical_ops`).
    pub rpc_amplification: f64,
    pub crc32c_verified_ok: u64,
    pub crc32c_corrupted: u64,
    pub elapsed_secs: f64,
    pub aggregate_mib_s: f64,
    pub wire_mib_s: f64,
    pub mib_per_cpu_core: f64,
    pub cpu_cores: usize,
    pub process_rss_mib: u64,
    pub recent_request_ids: Vec<String>,
}

/// Complete white-box telemetry snapshot emitted live (every 100ms) and persisted in final reports.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetryFrame {
    pub waterfall: StageWaterfall,
    pub stage_quantiles: StageQuantiles,
    pub handshake: EndpointHandshakeSummary,
    pub verbs: Vec<VerbMetricsRow>,
    pub timeseries: Vec<TimeSeriesPoint>,
    pub transport: TransportAndHostSnapshot,
    pub diagnosis: BottleneckDiagnosis,
    pub slowest_traces: Vec<SlowRequestTrace>,
    pub await_tree_dump: String,
    pub cleanup: CleanupSummary,
}
