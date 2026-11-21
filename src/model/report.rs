//! Report v4 (`oprobe.report/4`, design v3 §6 and v4 §1/§3): the machine-clean result of one
//! run, with the run's replayable spec (`spec`) and the capacity estimate (`capacity`).
//!
//! Rules: every numeric field name carries its unit (`_us`, `_ms`, `_s`, `_bytes`, `_per_s`,
//! `_pct`, `_ratio` for dimensionless fractions, `_usd`, `_count`); no human-formatted strings
//! in machine fields; enums are lowercase `snake_case`. The JSON Schema is generated from these
//! types (`schemars`). Aggregates shared with the live engine (`trace::Snapshot` parts) carry
//! their unit names through `serde` renames, so their Rust field names may be shorter.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::analysis::cost::CostReport;
use crate::analysis::diagnosis::{Bottleneck, Severity, SlaVerdict};
use crate::config::{Knobs, TransportMode};
use crate::engine::CleanupSummary;
use crate::net::HttpVersion;
use crate::trace::{
    ApiStats, ByteStats, CacheStats, ClientStats, ConnStats, EffectiveKnobs, IntegrityStats,
    OpBreakdown, OpenLoopStats, Quantiles, ResilienceStats, Summary, TimelinePoint, VerbStats,
};

/// Schema id of [`Report`].
pub const REPORT_SCHEMA: &str = "oprobe.report/4";

/// Unit suffixes a numeric report field name ends with (design §6). Each names its unit:
/// `_us` / `_ms` / `_s` time; `_bytes`, `_mib`, `_gib` sizes (`_mib`/`_gib` only where the
/// value is naturally fractional: `client.rss_mib`, `cost.stored_gib`); `_bps` bits per second;
/// `_per_s` rates (`ops_per_s`, `mib_per_s`, `requests_per_s`); `_pct` percent (0–100);
/// `_ratio` dimensionless fractions and multipliers; `_usd` money; `_count` counts (also a
/// count per something, e.g. `requests_per_op_count`, and maps of counts like
/// `status_count`); `_mean` fractional means of counts (`inflight_mean`, v4 §4).
pub const UNIT_SUFFIXES: &[&str] = &[
    "_us", "_ms", "_s", "_bytes", "_mib", "_gib", "_bps", "_per_s", "_pct", "_ratio", "_usd",
    "_count", "_mean",
];

/// Numeric report fields that are not quantities and so carry no unit: identifiers
/// (`id`, `*_id`, `*_ids`, `slowest` op ids, tune trial ids), codes (`exit_code`), the
/// workload `seed`, relative mix `weight`s and the heatmap's `counts` matrix (named by §6);
/// and values whose unit is a sibling `unit` field (`Expectation`, gate checks, tune deltas:
/// `predicted`, `lo`, `hi`, `baseline`, `candidate`).
pub const UNITLESS_FIELDS: &[&str] = &[
    "id",
    "op_ids",
    "slowest",
    "baseline_trial",
    "winner_trial",
    "exit_code",
    "seed",
    "weight",
    "counts",
    "predicted",
    "lo",
    "hi",
    "baseline",
    "candidate",
];

/// Whether a numeric report field name follows the unit rule.
pub fn has_unit(name: &str) -> bool {
    UNIT_SUFFIXES.iter().any(|s| name.ends_with(s))
        || UNITLESS_FIELDS.contains(&name)
        || name.ends_with("_id")
        || name.ends_with("_ids")
}

/// The full report of one run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    /// Always [`REPORT_SCHEMA`].
    pub schema: String,
    pub run: RunInfo,
    /// The replayable definition of the run (`oprobe run --from RUN`).
    pub spec: super::spec::RunSpec,
    pub target: TargetInfo,
    pub workload: WorkloadInfo,
    pub knobs: KnobsInfo,
    pub sla: SlaInfo,
    pub verdict: Verdict,
    pub summary: Summary,
    pub stages: Stages,
    pub apis: Vec<ApiStats>,
    pub verbs: Vec<VerbStats>,
    pub connections: ConnStats,
    pub network: Network,
    pub resilience: ResilienceStats,
    pub bytes: ByteStats,
    pub cache: CacheStats,
    pub integrity: IntegrityStats,
    pub client: ClientStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_loop: Option<OpenLoopStats>,
    pub timeline: Vec<TimelinePoint>,
    pub heatmap: Heatmap,
    pub findings: Vec<Finding>,
    /// How much load the target takes: the in-flight ceiling and the per-op request shape.
    #[serde(default)]
    pub capacity: crate::analysis::capacity::Capacity,
    pub cost: CostReport,
    /// Op ids of the slowest operations (slowest first); their rows are in the run file.
    pub slowest: Vec<u64>,
    pub cleanup: CleanupSummary,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tune: Option<TuneReport>,
}

impl Report {
    /// The most severe finding (info when there are none).
    pub fn worst_severity(&self) -> Severity {
        self.findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or_default()
    }

    /// Parse a report from JSON: a bare report, a `{"report": …}` command result, or a
    /// `{"kind":"report", …}` run-file line. Other schemas are rejected with a clear error.
    pub fn from_json_value(mut v: serde_json::Value) -> anyhow::Result<Self> {
        if v.get("schema").is_none()
            && let Some(inner) = v.get_mut("report")
        {
            v = inner.take();
        }
        let bad = |m: String| -> anyhow::Error {
            crate::errors::OprobeError::new(crate::errors::ErrorCode::UsageInvalidArgument, m)
                .with_hint("reports of other oprobe versions are not read: re-run the workload with this oprobe")
                .into()
        };
        match v.get("schema").and_then(|s| s.as_str()) {
            Some(REPORT_SCHEMA) => {}
            Some(other) => {
                return Err(bad(format!(
                    "report schema `{other}` is not supported; this oprobe reads `{REPORT_SCHEMA}`"
                )));
            }
            None => return Err(bad("not an oprobe report".into())),
        }
        if let Some(o) = v.as_object_mut() {
            o.remove("kind");
        }
        Ok(serde_json::from_value(v)?)
    }
}

/// Identity and context of the run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunInfo {
    /// Run id (`YYYYMMDD-HHMMSS-xxxx`).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339.
    pub ended_at: String,
    /// Length of the measurement window.
    pub window_s: f64,
    /// Stopped early (Ctrl-C, Stop, budget): the window is shorter than configured.
    pub interrupted: bool,
    /// Trace rows were sampled (more requests than `trace.max_requests`); aggregates are exact.
    pub sampled: bool,
    pub trace: TraceInfo,
    pub oprobe_version: String,
    pub host: HostInfo,
    pub env: EnvInfo,
}

/// What the run file's trace rows cover.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceInfo {
    pub max_requests_count: u64,
    pub ops_seen_count: u64,
    pub ops_kept_count: u64,
    pub requests_seen_count: u64,
    pub requests_kept_count: u64,
    /// Kept / seen request rows (1 when nothing was sampled away).
    pub sample_ratio: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HostInfo {
    pub os: String,
    pub arch: String,
    pub cpus_count: u64,
    /// FNV-1a hash of the host name (identifies the machine without naming it).
    pub hostname_hash: String,
}

impl HostInfo {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            cpus_count: std::thread::available_parallelism()
                .map(|n| n.get() as u64)
                .unwrap_or(1),
            hostname_hash: format!("{:016x}", super::ids::fnv1a(hostname().as_bytes())),
        }
    }
}

fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname NUL-terminates on success.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

/// Cloud context of the target, when known.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EnvInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Pricing/provider key, e.g. `aws-s3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// The target, with secrets redacted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TargetInfo {
    /// Configured target name, when the run used one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub display: String,
    pub scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Backend options with secrets redacted.
    pub options: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// The workload, resolved (preset defaults applied).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkloadInfo {
    pub preset: String,
    pub duration_s: f64,
    pub warmup_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ops_count: Option<u64>,
    /// Open-loop arrival rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_per_s: Option<f64>,
    /// Key popularity: `uniform`, `zipf:1.1`, `hotset:0.2/0.8`.
    pub access: String,
    pub dataset: DatasetInfo,
    pub mix: Vec<MixEntry>,
    pub verify: bool,
    pub cleanup: bool,
    pub seed: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DatasetInfo {
    pub objects_count: u64,
    pub object_size_bytes: u64,
    pub partitions_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_prefix: Option<String>,
    pub persist: bool,
}

/// One weighted operation of the mix.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MixEntry {
    pub weight: u32,
    /// `read`, `parquet_scan`, `write`, `multipart_write`, `stat`, `list`.
    pub op: String,
    /// Read range / write size (absent: whole object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
    /// `parquet_scan`: footer read with a suffix range, no HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<bool>,
    /// `delete`: keys per batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_count: Option<u32>,
    /// `list_paged`: keys per page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_count: Option<u32>,
    /// `metadata_write`: user-metadata pairs per object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_entries_count: Option<u32>,
}

/// Client knob values in machine units.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KnobValues {
    pub concurrency_count: u64,
    pub io_concurrency_count: u64,
    pub chunk_bytes: u64,
    pub range_gap_bytes: u64,
    /// 0 = unlimited.
    pub max_inflight_requests_count: u64,
    pub http_version: HttpVersion,
    pub transport: TransportMode,
    pub keep_alive: bool,
    pub cache: bool,
    pub cache_bytes: u64,
    pub retries_count: u64,
    pub tail_cut: bool,
    pub timeout_ms: u64,
    /// Addressing style, checksum and SSE (S3).
    #[serde(
        default,
        skip_serializing_if = "crate::config::ProviderKnobs::is_default"
    )]
    pub provider: crate::config::ProviderKnobs,
}

impl From<&Knobs> for KnobValues {
    fn from(k: &Knobs) -> Self {
        Self {
            concurrency_count: k.concurrency as u64,
            io_concurrency_count: k.io_concurrency as u64,
            chunk_bytes: k.chunk_size.0,
            range_gap_bytes: k.range_gap.0,
            max_inflight_requests_count: k.max_inflight_requests as u64,
            http_version: k.http_version,
            transport: k.transport,
            keep_alive: k.keep_alive,
            cache: k.cache,
            cache_bytes: k.cache_size.0,
            retries_count: k.retries as u64,
            tail_cut: k.tail_cut,
            timeout_ms: k.timeout.get().as_millis() as u64,
            provider: k.provider.clone(),
        }
    }
}

impl KnobValues {
    /// Back to the profile form (e.g. to re-run with these knobs).
    pub fn to_knobs(&self) -> Knobs {
        Knobs {
            concurrency: self.concurrency_count as usize,
            io_concurrency: self.io_concurrency_count as usize,
            chunk_size: crate::util::ByteSize(self.chunk_bytes),
            range_gap: crate::util::ByteSize(self.range_gap_bytes),
            max_inflight_requests: self.max_inflight_requests_count as usize,
            http_version: self.http_version,
            transport: self.transport,
            keep_alive: self.keep_alive,
            cache: self.cache,
            cache_size: crate::util::ByteSize(self.cache_bytes),
            retries: self.retries_count as usize,
            tail_cut: self.tail_cut,
            timeout: crate::util::HumanDuration(std::time::Duration::from_millis(self.timeout_ms)),
            provider: self.provider.clone(),
        }
    }
}

/// Knobs as requested and as OpenDAL applied them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KnobsInfo {
    pub requested: KnobValues,
    pub effective: EffectiveKnobValues,
}

/// The effective configuration: the requested values with the service's limits applied.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EffectiveKnobValues {
    #[serde(flatten)]
    pub values: KnobValues,
    /// Multipart part size OpenDAL writes with (the chunk clamped to the service's limits).
    pub write_chunk_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_min_part_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_max_part_bytes: Option<u64>,
}

impl EffectiveKnobValues {
    pub fn new(requested: &Knobs, e: &EffectiveKnobs) -> Self {
        Self {
            values: requested.into(),
            write_chunk_bytes: if e.write_chunk_size == 0 {
                requested.chunk_size.0
            } else {
                e.write_chunk_size
            },
            service_min_part_bytes: e.service_min_part_size,
            service_max_part_bytes: e.service_max_part_size,
        }
    }

    /// OpenDAL changed the requested write chunk.
    pub fn write_chunk_clamped(&self) -> bool {
        self.write_chunk_bytes != self.values.chunk_bytes
    }
}

/// Service level objectives the run was checked against.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlaInfo {
    /// A breach fails the command (exit 3).
    pub enforced: bool,
    pub max_error_ratio: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_throttle_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
}

impl From<&crate::config::Sla> for SlaInfo {
    fn from(s: &crate::config::Sla) -> Self {
        Self {
            enforced: s.enforce,
            max_error_ratio: s.max_error_rate,
            max_throttle_ratio: s.max_throttle_rate,
            p99_ms: s.p99_ms,
        }
    }
}

/// Overall judgement.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum VerdictLevel {
    #[default]
    Ok,
    /// Worth a look (informational findings, warnings, partial run).
    Review,
    /// Warning findings.
    Attention,
    /// Critical findings or a breached SLA.
    Critical,
}

impl VerdictLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Review => "review",
            Self::Attention => "attention",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Verdict {
    pub level: VerdictLevel,
    /// One sentence: where the time went.
    pub headline: String,
    /// What `oprobe run` exits with for this result (0, 3 SLA breach, 130 interrupted; 4/5 when
    /// a safety rail stopped it).
    pub exit_code: u8,
    pub bottleneck: Bottleneck,
    pub sla: SlaVerdict,
}

/// Where operation time went.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Stages {
    pub bottleneck: Bottleneck,
    /// Share of total operation time per layer (sums to ~100).
    pub shares: StageShares,
    /// Mean breakdown per verb.
    pub per_verb: Vec<VerbBreakdown>,
    /// p99 of each request stage across every request (from the trace rows).
    pub request_p99: RequestStagesP99,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StageShares {
    pub queue_pct: f64,
    pub cache_pct: f64,
    pub connect_pct: f64,
    pub upload_pct: f64,
    pub server_pct: f64,
    pub download_pct: f64,
    pub retry_pct: f64,
    pub client_pct: f64,
}

impl From<&crate::analysis::diagnosis::TimeShares> for StageShares {
    fn from(s: &crate::analysis::diagnosis::TimeShares) -> Self {
        Self {
            queue_pct: s.queue * 100.0,
            cache_pct: s.cache * 100.0,
            connect_pct: s.connect * 100.0,
            upload_pct: s.upload * 100.0,
            server_pct: s.server * 100.0,
            download_pct: s.download * 100.0,
            retry_pct: s.retry * 100.0,
            client_pct: s.client * 100.0,
        }
    }
}

impl StageShares {
    /// `(name, pct)` in display order.
    pub fn parts(&self) -> [(&'static str, f64); 8] {
        [
            ("queue", self.queue_pct),
            ("cache", self.cache_pct),
            ("connect", self.connect_pct),
            ("upload", self.upload_pct),
            ("server", self.server_pct),
            ("download", self.download_pct),
            ("retry", self.retry_pct),
            ("client", self.client_pct),
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerbBreakdown {
    pub verb: String,
    pub breakdown: OpBreakdown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RequestStagesP99 {
    pub queue_ms: f64,
    /// Requests that dialed only.
    pub connect_ms: f64,
    pub send_ms: f64,
    pub ttfb_ms: f64,
    pub body_ms: f64,
    pub stall_ms: f64,
    pub total_ms: f64,
}

/// Network view: TLS, TCP and the remote addresses requests went to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Network {
    pub tls: TlsInfo,
    pub tcp: TcpInfo,
    pub remotes: Vec<RemoteStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TlsInfo {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alpn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    pub handshake: Quantiles,
    /// Full / resumed / HelloRetryRequest handshakes, versions, ciphers and groups.
    #[serde(default)]
    pub handshakes: crate::net::stats::TlsHandshakeStats,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TcpInfo {
    pub dns: Quantiles,
    pub connect: Quantiles,
    pub proxy_connect: Quantiles,
    pub dialed_count: u64,
    /// Kernel TCP statistics at request end (Linux `TCP_INFO`, macOS `TCP_CONNECTION_INFO`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<crate::net::stats::TcpKernelStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteStats {
    /// `ip:port`.
    pub remote: String,
    pub requests_count: u64,
    pub errors_count: u64,
    #[serde(rename = "down_bytes", alias = "bytes_down")]
    pub bytes_down: u64,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

/// Operation latency over time: `counts[t][l]` operations started in time bucket `t` whose
/// latency falls in latency bucket `l` (from the trace rows; sampled when the run is).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Heatmap {
    /// Start of each time bucket, seconds from the window start.
    pub t_buckets_s: Vec<f64>,
    pub bucket_width_s: f64,
    /// Lower bound of each latency bucket (log scale; the last bucket is open-ended).
    pub latency_buckets_ms: Vec<f64>,
    pub counts: Vec<Vec<u64>>,
}

/// Fixed log-scale latency bucket bounds (ms): comparable across runs.
pub const HEATMAP_LATENCY_BOUNDS_MS: [f64; 20] = [
    0.0, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0,
    5_000.0, 10_000.0, 20_000.0, 50_000.0, 100_000.0,
];

impl Heatmap {
    /// At most `max_columns` time buckets of whole seconds over `window_s`.
    pub fn from_ops(ops: &[super::OpRow], window_s: f64, max_columns: usize) -> Self {
        let window = window_s.max(0.0);
        let width = (window / max_columns.max(1) as f64).ceil().max(1.0);
        let n_t = ((window / width).ceil() as usize).max(1);
        let bounds = HEATMAP_LATENCY_BOUNDS_MS;
        let mut counts = vec![vec![0u64; bounds.len()]; n_t];
        for o in ops {
            let t = (o.t_us.max(0) as f64 / 1e6 / width) as usize;
            let ms = o.latency_us as f64 / 1000.0;
            let l = bounds.partition_point(|b| *b <= ms).saturating_sub(1);
            counts[t.min(n_t - 1)][l] += 1;
        }
        Self {
            t_buckets_s: (0..n_t).map(|i| i as f64 * width).collect(),
            bucket_width_s: width,
            latency_buckets_ms: bounds.to_vec(),
            counts,
        }
    }
}

/// A diagnosis item (design §1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    /// Stable code, e.g. `throttling`, `cold_connections`.
    pub code: String,
    pub severity: Severity,
    /// One sentence: what is wrong.
    pub cause: String,
    pub evidence: Evidence,
    /// What to change.
    pub advice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

/// The numbers and trace references behind a finding.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    /// The evidence in words, with numbers.
    pub text: String,
    /// Named measurements (unit-suffixed keys).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, f64>,
    /// Operations that show the problem.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub op_ids: Vec<u64>,
    /// Time windows (seconds from the window start) where it shows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<TimeWindow>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimeWindow {
    pub from_s: f64,
    pub to_s: f64,
}

/// A concrete way to test a finding's advice (v4 §5).
pub use crate::analysis::verification::{Expectation, Verification};

impl From<&crate::analysis::diagnosis::Finding> for Finding {
    fn from(f: &crate::analysis::diagnosis::Finding) -> Self {
        Self {
            code: f.code.clone(),
            severity: f.severity,
            cause: f.title.clone(),
            evidence: Evidence {
                text: f.evidence.clone(),
                ..Default::default()
            },
            advice: f.action.clone(),
            verification: None,
        }
    }
}

/// The tuner's search, embedded in the winner's report.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TuneReport {
    pub objective: String,
    pub trials: Vec<TrialRow>,
    pub baseline_trial: u64,
    pub winner_trial: u64,
    /// Objective change of the winner vs the baseline trial.
    pub improvement_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_ceiling_count: Option<u64>,
    /// No trial met the SLA: the winner is the least-bad configuration.
    pub no_feasible_trial: bool,
    pub notes: Vec<String>,
}

/// One tuner trial.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TrialRow {
    pub id: u64,
    pub stage: String,
    pub knobs: KnobValues,
    /// Successful operations per second.
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    pub throttle_ratio: f64,
    pub throttled_responses_count: u64,
    pub cost_per_million_ops_usd: f64,
    pub feasible: bool,
    pub note: String,
}

impl From<&crate::tune::Trial> for TrialRow {
    fn from(t: &crate::tune::Trial) -> Self {
        Self {
            id: t.id as u64,
            stage: t.stage.clone(),
            knobs: (&t.knobs).into(),
            ops_per_s: t.ops_per_s,
            mib_per_s: t.mib_per_s,
            p50_ms: t.p50_ms,
            p99_ms: t.p99_ms,
            error_ratio: t.error_rate,
            throttle_ratio: t.throttle_rate,
            throttled_responses_count: t.throttled_responses,
            cost_per_million_ops_usd: t.cost_per_million_ops,
            feasible: t.feasible,
            note: t.note.clone(),
        }
    }
}

impl From<&crate::report::TuneSummary> for TuneReport {
    fn from(s: &crate::report::TuneSummary) -> Self {
        Self {
            objective: s.objective.clone(),
            trials: s.trials.iter().map(TrialRow::from).collect(),
            baseline_trial: s.baseline_trial as u64,
            winner_trial: s.winner_trial as u64,
            improvement_pct: s.improvement * 100.0,
            throttle_ceiling_count: s.throttle_ceiling.map(|c| c as u64),
            no_feasible_trial: s.no_feasible_trial,
            notes: s.notes.clone(),
        }
    }
}
