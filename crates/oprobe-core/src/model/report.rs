//! Report v6 (`oprobe.report/6`, design v3 §6, v4 §1/§3 and v5 §2): the machine-clean result of one
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

use crate::analysis::diagnosis::{Bottleneck, Severity, SlaVerdict};
use crate::engine::CleanupSummary;
use crate::model::TraceInfo;
use crate::trace::{
    ApiStats, ByteStats, CacheStats, ConnStats, EffectiveKnobs, IntegrityStats, LatencyHistogram,
    OpBreakdown, OpenLoopStats, Quantiles, ResilienceStats, Summary, TimelinePoint, VerbStats,
};

/// Schema id of [`Report`].
pub const REPORT_SCHEMA: &str = "oprobe.report/6";

/// Unit suffixes a numeric report field name ends with (design §6). Each names its unit:
/// `_us` / `_ms` / `_s` time; `_bytes`, `_mib`, `_gib` sizes (`_mib`/`_gib` only where the
/// value is naturally fractional: `client.rss_mib`, `cost.stored_gib`); `_bps` bits per second;
/// `_per_s` rates (`ops_per_s`, `mib_per_s`, `requests_per_s`); `_pct` percent (0–100);
/// `_ratio` dimensionless fractions and multipliers; `_usd` money; `_count` counts (also a
/// count per something, e.g. `peak_inflight_per_op_count`, and maps of counts like
/// `status_count`); `_mean` fractional means of counts (`inflight_mean`, v4 §4).
pub const UNIT_SUFFIXES: &[&str] = &[
    "_us", "_ms", "_s", "_bytes", "_mib", "_gib", "_bps", "_per_s", "_pct", "_pp", "_ratio",
    "_usd", "_count", "_mean", "_days",
];

/// Numeric report fields that are not quantities and so carry no unit: identifiers
/// (`id`, `*_id`, `*_ids`, `slowest` op ids, tune trial ids), codes (`exit_code`), the
/// workload `seed`, relative mix `weight`s, the heatmap's `counts` matrix (named by §6), a matrix
/// cell's `rank`, a TCP `port`, HTTP `status` and exit `code`s; the parts of an interval
/// (`estimate`, `low`, `high`) whose unit is in the name of the field holding it
/// (`p99_ms.estimate`);
/// and values whose unit is a sibling `unit` field (`Expectation`, gate checks, tune deltas:
/// `predicted`, `lo`, `hi`, `baseline`, `candidate`, watch SLO `value` and `limit`).
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
    "rank",
    "port",
    "status",
    "code",
    "value",
    "limit",
    "abs_limit",
    "estimate",
    "low",
    "high",
    "log_likelihood",
    // Bayesian information criterion of a limit model (a score; lower is better).
    "bic",
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
    /// The replayable definition of the run (what `oprobe prove` replays).
    pub spec: super::spec::RunSpec,
    pub target: TargetInfo,
    pub workload: WorkloadInfo,
    /// The multipart part size OpenDAL wrote with (the requested `write.chunk` clamped to the
    /// service's part-size limits).
    #[serde(default)]
    pub effective_write: EffectiveKnobs,
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
    /// The observer's own cost and the client's health (design v5 §3; `report.self`).
    #[serde(rename = "self", default)]
    pub self_stats: super::SelfStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_loop: Option<OpenLoopStats>,
    /// Arrival schedule: missed arrivals and the schedule stage (design v5 §3).
    #[serde(default)]
    pub schedule: super::ScheduleStats,
    /// Where the traffic and the trouble are: prefixes, hot keys, sizes, cold/warm, tenants.
    #[serde(default)]
    pub dimensions: super::Dimensions,
    /// How the trace rows were sampled (findings use aggregate counts, never these).
    #[serde(default)]
    pub sampling: super::Sampling,
    /// Per interval: 1 s for the first ten minutes, then 10 s. Operation counts and quantiles
    /// by operation start; requests, throttled responses, retries and failures by when they
    /// happened (see [`TimelinePoint`]).
    pub timeline: Vec<TimelinePoint>,
    pub heatmap: Heatmap,
    /// Every operation's latency as a fine log-scale histogram (20 bins per decade, never
    /// sampled): what the latency CDF is drawn from. Absent in reports before 6.3.
    #[serde(default, skip_serializing_if = "LatencyHistogram::is_empty")]
    pub latency_histogram: LatencyHistogram,
    pub findings: Vec<Finding>,
    /// How much load the target takes: the in-flight ceiling and the per-op request shape.
    #[serde(default)]
    pub capacity: crate::analysis::capacity::Capacity,
    /// Op ids of the slowest operations (slowest first); their rows are in the run file.
    pub slowest: Vec<u64>,
    pub cleanup: CleanupSummary,
    #[serde(default)]
    pub warnings: Vec<String>,
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

    /// Goodput: successful operations per second (failed operations never count).
    pub fn good_ops_per_s(&self) -> f64 {
        let s = &self.summary;
        if self.run.window_s > 0.0 {
            s.ops.saturating_sub(s.failed_ops) as f64 / self.run.window_s
        } else {
            s.ops_per_s
        }
    }

    /// A production trace (`run.source = trace`), not a run oprobe drove.
    pub fn is_trace(&self) -> bool {
        self.run.source == RunSource::Trace
    }

    /// What `oprobe show` takes to open this report again: the run id, or a trace's file (its
    /// `run.label`, the path as it was given).
    pub fn show_ref(&self) -> &str {
        match (&self.run.label, self.is_trace()) {
            (Some(path), true) => path,
            _ => &self.run.id,
        }
    }

    /// The workload in words: the preset, or `production trace`.
    pub fn workload_label(&self) -> &str {
        if self.is_trace() {
            TRACE_WORKLOAD_LABEL
        } else {
            &self.workload.preset
        }
    }

    /// The run stopped early (`run.partial`, or the legacy `run.interrupted` flag).
    pub fn is_partial(&self) -> bool {
        self.run.partial.is_some() || self.run.interrupted
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
    /// Where the traffic came from: a workload oprobe drove (`run`), or a production trace
    /// an embedded `oprobe-trace` layer recorded (`trace`: no workload, nothing to replay).
    #[serde(default)]
    pub source: RunSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The `oprobe prove` this run is a trial of (trials are hidden from `show --list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339.
    pub ended_at: String,
    /// Length of the measurement window.
    pub window_s: f64,
    /// Stopped early (Ctrl-C, Stop, budget): the window is shorter than configured.
    pub interrupted: bool,
    /// Why and when the run stopped early (absent for a complete run). A partial run is never
    /// accepted as a baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<PartialInfo>,
    /// Trace rows were sampled (more requests than `trace.max_requests`); aggregates are exact.
    pub sampled: bool,
    pub trace: TraceInfo,
    pub oprobe_version: String,
    pub host: HostInfo,
    pub env: EnvInfo,
}

/// Where a run's traffic came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunSource {
    /// A workload `oprobe run` drove (replayable: `oprobe prove` can A/B it).
    #[default]
    Run,
    /// A production trace written by the `oprobe-trace` layer (`oprobe.trace/7`, or a 6.x
    /// `oprobe.trace/6`): the application's own traffic, with no workload to replay.
    Trace,
}

impl RunSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Trace => "trace",
        }
    }
}

/// What a production trace's report shows where a run shows its workload preset.
pub const TRACE_WORKLOAD_LABEL: &str = "production trace";

/// Why a run stopped before its configured end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PartialReason {
    /// Ctrl-C, Stop or a cancelled job.
    #[default]
    Interrupted,
    /// A safety budget (`--max-requests`, `--max-bytes`, `--max-duration`, `--max-cost-usd`).
    Budget,
    /// A fatal error ended the measurement.
    Error,
}

impl PartialReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Budget => "budget",
            Self::Error => "error",
        }
    }

    /// `interrupted` / `stopped by a budget` / `stopped by an error`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Budget => "stopped by a budget",
            Self::Error => "stopped by an error",
        }
    }
}

/// A run that stopped early (`report.run.partial`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PartialInfo {
    pub reason: PartialReason,
    /// Seconds of the measurement window that ran.
    pub measured_s: f64,
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

/// Service level objectives the run was checked against.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlaInfo {
    /// A breach fails the command (exit 3).
    pub enforced: bool,
    pub max_error_ratio: f64,
    /// Throttled responses / requests the run may have (`sla.max_throttle_rate`).
    #[serde(
        default,
        rename = "max_throttled_request_ratio",
        skip_serializing_if = "Option::is_none"
    )]
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

    /// How text output names the level: `OK`, `Review`, `Attention`, `Critical` (JSON keeps
    /// [`Self::as_str`]).
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Review => "Review",
            Self::Attention => "Attention",
            Self::Critical => "Critical",
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
    /// `ip:port`, or `no connection` for the requests that failed without one (resets).
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

const _: () = {
    // The recorder's interval columns use the heatmap's bounds.
    let (a, b) = (
        HEATMAP_LATENCY_BOUNDS_MS,
        crate::trace::recorder::HEAT_BOUNDS_MS,
    );
    let mut i = 0;
    while i < a.len() {
        assert!(a[i] == b[i]);
        i += 1;
    }
};

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

    /// The heatmap of the interval sketches (every operation, not the sampled rows; design v5
    /// §3, M11): at most `max_columns` time buckets of whole seconds over `window_s`. `None`
    /// when the timeline carries no per-interval counts.
    pub fn from_timeline(
        timeline: &[TimelinePoint],
        window_s: f64,
        max_columns: usize,
    ) -> Option<Self> {
        if timeline.iter().all(|p| p.latency_rows_count.is_empty()) {
            return None;
        }
        let window = window_s.max(0.0);
        let width = (window / max_columns.max(1) as f64).ceil().max(1.0);
        let n_t = ((window / width).ceil() as usize).max(1);
        let bounds = HEATMAP_LATENCY_BOUNDS_MS;
        let mut counts = vec![vec![0u64; bounds.len()]; n_t];
        for p in timeline {
            let t = (p.t_s.max(0.0) / width) as usize;
            for (row, n) in counts[t.min(n_t - 1)].iter_mut().zip(&p.latency_rows_count) {
                *row += n;
            }
        }
        Some(Self {
            t_buckets_s: (0..n_t).map(|i| i as f64 * width).collect(),
            bucket_width_s: width,
            latency_buckets_ms: bounds.to_vec(),
            counts,
        })
    }
}

/// A finding (design v6 §3 "Explain"): a cause, the evidence (the requests themselves) and
/// **one** OpenDAL remedy. No sizing numbers: a single run varies no load (T1, T2); `oprobe
/// prove` sizes the remedy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    /// Stable code ([`crate::analysis::catalog`]), e.g. `throttling`, `tail_stragglers`.
    pub code: String,
    pub severity: Severity,
    /// How sure the cause is: `high` only when the limit model explains most of the effect
    /// and agrees across the run's halves (T8).
    #[serde(default)]
    pub confidence: crate::analysis::capacity::Confidence,
    /// One sentence: what is wrong.
    pub cause: String,
    pub evidence: Evidence,
    pub remedy: Remedy,
}

/// The one remedy of a finding: an `OperatorSpec` path (design v6 §8 "Findings → remedies").
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Remedy {
    /// The operator path the remedy changes (`layers.concurrent_limit.http`, `read.gap`);
    /// absent when the remedy is not a layer (`key_limit`, `client_bound`, `errors`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Paths changed together with it (`layers.retry.jitter`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with: Vec<String>,
    /// The OpenDAL API, e.g. `ConcurrentLimitLayer::with_http_concurrent_limit`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub opendal: String,
    /// `oprobe prove` sizes the value with a bracket search (otherwise it switches it on).
    pub sized: bool,
    /// The remedy in one sentence (no numbers).
    pub text: String,
    /// The next command (`oprobe prove RUN CODE`, or `oprobe show RUN --by …` when there is
    /// nothing to prove).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

impl Remedy {
    /// `oprobe prove` can test this remedy (it changes the operator).
    pub fn provable(&self) -> bool {
        self.path.is_some()
    }
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

impl Evidence {
    /// The named values for people (P2-8): a label without the unit suffix and the value in
    /// its unit — ratios as percentages, times as ms/s, sizes in bytes units, counts as whole
    /// numbers.
    pub fn labelled(&self) -> Vec<(String, String)> {
        self.values
            .iter()
            .map(|(k, v)| evidence_label(k, *v))
            .collect()
    }
}

/// `throttled_request_ratio` = 0.047 → (`Throttled requests`, `4.7%`).
pub fn evidence_label(key: &str, v: f64) -> (String, String) {
    use crate::util::{fmt_bytes, fmt_ms, fmt_usd};
    let known: Option<&str> = match key {
        "throttled_request_ratio" => Some("Throttled requests"),
        "throttled_op_ratio" => Some("Throttled operations"),
        "error_ratio" => Some("Failed operations"),
        "p99_to_p50_ratio" => Some("p99 / p50"),
        "overread_ratio" => Some("Over-read"),
        "ttfb_pct" => Some("Time waiting for the first byte"),
        "retry_pct" => Some("Time in retry backoff"),
        "time_cost_ratio" => Some("Share of the time it costs"),
        "queue_pct" => Some("Time queued"),
        "connect_pct" => Some("Time connecting"),
        "reuse_pct" => Some("Connection reuse"),
        "straggler_pct" => Some("Stragglers"),
        "straggler_ttfb_ms" => Some("Straggler wait (p50)"),
        "ceiling_lo_inflight_count" => Some("Clean up to (in flight)"),
        "ceiling_hi_inflight_count" => Some("Throttled from (in flight)"),
        "peak_inflight_per_op_count" => Some("In flight per operation (peak)"),
        "extra_requests_per_1k_ops_mean" => Some("Extra requests per 1\u{202f}000 ops"),
        "saved_requests_per_1k_ops_mean" => Some("Requests saved per 1\u{202f}000 reads"),
        "mean_request_bytes" => Some("Mean request size"),
        "tail_cut_deadline_ms" => Some("Tail-cut deadline"),
        "tail_cut_blind_s" => Some("Tail-cut blind for"),
        "accepted_requests_per_s" => Some("Accepted while throttled"),
        "monthly_usd" => Some("Projected monthly bill"),
        _ => None,
    };
    let label = known.map(str::to_owned).unwrap_or_else(|| {
        let bare = UNIT_SUFFIXES
            .iter()
            .filter(|s| key.ends_with(*s))
            .max_by_key(|s| s.len())
            .map_or(key, |s| &key[..key.len() - s.len()]);
        let words = bare.replace('_', " ");
        let mut c = words.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => key.to_owned(),
        }
    });
    let multiplier = key.contains("_to_") || key == "overread_ratio";
    let value = if key.ends_with("_ratio") && multiplier {
        format!("{v:.2}×")
    } else if key.ends_with("_ratio") {
        pct(v * 100.0)
    } else if key.ends_with("_pct") {
        pct(v)
    } else if key.ends_with("_ms") {
        fmt_ms(v)
    } else if key.ends_with("_us") {
        fmt_ms(v / 1000.0)
    } else if key.ends_with("_s") && !key.ends_with("_per_s") {
        format!("{v:.1} s")
    } else if key.ends_with("_bytes") {
        fmt_bytes(v.max(0.0) as u64)
    } else if key.ends_with("_usd") {
        fmt_usd(v)
    } else if key.ends_with("_per_s") {
        let unit = if key.contains("requests") {
            "requests/s"
        } else if key.contains("mib") {
            "MiB/s"
        } else {
            "ops/s"
        };
        format!(
            "{} {unit}",
            group_thousands(v, if v.abs() < 10.0 { 1 } else { 0 })
        )
    } else if key.ends_with("_count") || key.contains("_per_1k_") {
        // Counts, and counts per 1 000 operations, are whole numbers.
        group_thousands(v, 0)
    } else {
        group_thousands(v, 2)
    };
    (label, value)
}

/// A percentage with as many decimals as it needs: `4.7%`, `0.12%`, `100%`.
fn pct(p: f64) -> String {
    if p == 0.0 || p.abs() >= 10.0 {
        format!("{p:.0}%")
    } else if p.abs() >= 0.1 {
        format!("{p:.1}%")
    } else {
        format!("{p:.2}%")
    }
}

/// `12345.6` → `12 346` (narrow no-break spaces between groups of three, `decimals` after the
/// point): the number format of evidence values, also used in evidence texts.
pub fn group_thousands(v: f64, decimals: usize) -> String {
    let s = format!("{:.*}", decimals, v.abs());
    let (int, frac) = s.split_once('.').map_or((s.as_str(), ""), |(a, b)| (a, b));
    let mut out = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(ch);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    if v < 0.0 && v.abs() >= 0.5 * 10f64.powi(-(decimals as i32)) {
        out.insert(0, '−');
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimeWindow {
    pub from_s: f64,
    pub to_s: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `*_count` field of every registered schema is an integer (design v4 §4).

    #[test]
    fn evidence_values_read_as_labels_units_and_percentages() {
        let e = Evidence {
            values: [
                ("throttled_request_ratio", 0.0468),
                ("p99_ms", 262.918),
                ("retry_pct", 12.0),
                ("ceiling_lo_inflight_count", 20.0),
                ("mean_request_bytes", 65536.0),
                ("p99_to_p50_ratio", 30.0),
                ("ceiling_requests_per_s", 2410.4),
                ("extra_requests_per_1k_ops_mean", -1031.0),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
            ..Default::default()
        };
        let l: BTreeMap<String, String> = e.labelled().into_iter().collect();
        assert_eq!(l["Throttled requests"], "4.7%");
        assert_eq!(l["Time in retry backoff"], "12%");
        assert_eq!(l["Clean up to (in flight)"], "20");
        assert_eq!(l["p99 / p50"], "30.00×");
        assert_eq!(l["Ceiling requests"], "2\u{202f}410 requests/s");
        assert_eq!(l["Extra requests per 1\u{202f}000 ops"], "−1\u{202f}031");
        assert!(l["Mean request size"].contains("KiB"), "{l:?}");
        assert!(l["P99"].ends_with("ms"), "{l:?}");
        assert!(!l.keys().any(|k| k.contains('_')), "{l:?}");
    }
}
