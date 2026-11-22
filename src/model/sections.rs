//! Report sections added by `oprobe.report/5` (design v5 §2): the load generator
//! ([`ClientInfo`]), the workers of a distributed run ([`WorkerInfo`]), the arrival schedule
//! ([`ScheduleStats`]), dimensions ([`Dimensions`]), sampling ([`Sampling`]) and the observer's
//! own cost and health ([`SelfStats`], `report.self`).
//!
//! The foundation defines the types and fills what 4.x already measured; the streams fill the
//! rest: MEAS (`schedule`, `dimensions`, `self`), SCALE (`client`, `workers`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::trace::{ClientStats, OpenLoopStats, Quantiles};

/// The OpenDAL version oprobe links (`report.client.version` of the default client).
pub const OPENDAL_VERSION: &str = "0.59";

/// What generated the load (`report.client`, design v5 §7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    /// `opendal`, `aws_sdk` or `exec`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Request stages this client reports (findings that need others are suppressed).
    #[serde(default)]
    pub phases_available: Vec<String>,
}

impl ClientInfo {
    /// Stages the white-box OpenDAL client observes.
    pub const OPENDAL_PHASES: &'static [&'static str] = &[
        "queue", "connect", "send", "ttfb", "body", "stall", "backoff",
    ];

    /// The default client (OpenDAL through oprobe's transport).
    pub fn opendal() -> Self {
        Self {
            kind: "opendal".into(),
            version: Some(OPENDAL_VERSION.into()),
            phases_available: Self::OPENDAL_PHASES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }

    /// Whether this client reports `phase`.
    pub fn has_phase(&self, phase: &str) -> bool {
        self.phases_available.iter().any(|p| p == phase)
    }
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self::opendal()
    }
}

/// One worker of a distributed run (`report.workers[]`, design v5 §8).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkerInfo {
    pub id: String,
    /// `host:port`, or `local` for a local worker process.
    pub addr: String,
    /// The worker's clock minus the coordinator's (NTP-style exchange).
    pub clock_offset_ms: f64,
    pub ops_count: u64,
    pub requests_count: u64,
    pub ops_per_s: f64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// The arrival schedule (`report.schedule`, design v5 §3): open-loop arrivals that started
/// late or never, and the `schedule` stage (intended → actual start).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleStats {
    /// Arrivals follow a rate (`--rate`, `--shape`, `--replay`); false: closed loop, subject to
    /// coordinated omission.
    pub open_loop: bool,
    /// `constant`, `poisson`, `ramp`, `step`, `diurnal`, `replay` (open loop only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrivals: Option<String>,
    pub intended_ops_count: u64,
    /// Arrivals that never started (censored latencies, counted in the tail as "≥").
    pub missed_arrivals_count: u64,
    pub missed_arrivals_ratio: f64,
    /// Time from an operation's intended start to its actual start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_stage: Option<Quantiles>,
    pub max_schedule_lag_ms: f64,
}

impl ScheduleStats {
    /// What 4.x measured: the open-loop counters.
    pub fn from_open_loop(o: Option<&OpenLoopStats>, window_s: f64) -> Self {
        let Some(o) = o else {
            return Self::default();
        };
        let intended = (o.target_rate * window_s).round().max(0.0) as u64;
        Self {
            open_loop: true,
            arrivals: Some("constant".into()),
            intended_ops_count: intended,
            missed_arrivals_count: o.missed,
            missed_arrivals_ratio: if intended > 0 {
                (o.missed as f64 / intended as f64).min(1.0)
            } else {
                0.0
            },
            schedule_stage: None,
            max_schedule_lag_ms: o.max_schedule_lag_ms,
        }
    }
}

/// One slice of a dimension (a prefix, a size bucket, cold or warm, a tenant).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DimensionSlice {
    pub key: String,
    pub ops_count: u64,
    pub requests_count: u64,
    /// Share of all requests.
    pub share_ratio: f64,
    pub throttled_ratio: f64,
    pub error_ratio: f64,
    pub p99_ms: f64,
}

/// A hot key: its share of the traffic and of the throttling.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HotKey {
    pub key: String,
    pub requests_count: u64,
    pub share_ratio: f64,
    pub throttle_share_ratio: f64,
}

/// Where the traffic and the trouble are (`report.dimensions`, design v5 §3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Dimensions {
    /// Prefix depth of `prefixes` (path segments).
    pub prefix_depth_count: u32,
    #[serde(default)]
    pub prefixes: Vec<DimensionSlice>,
    /// Top keys by requests (at most 20).
    #[serde(default)]
    pub hot_keys: Vec<HotKey>,
    #[serde(default)]
    pub size_buckets: Vec<DimensionSlice>,
    /// `cold` (first read of a key) and `warm` (repeat reads).
    #[serde(default)]
    pub cold_warm: Vec<DimensionSlice>,
    #[serde(default)]
    pub tenants: Vec<DimensionSlice>,
    /// Per-worker slices of a distributed run.
    #[serde(default)]
    pub workers: Vec<DimensionSlice>,
}

/// How the trace rows were sampled (`report.sampling`, design v5 §2): whole operations are
/// kept with all their requests; findings use aggregate counts, never these.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Sampling {
    pub kept_ops_count: u64,
    pub seen_ops_count: u64,
    /// Kept / seen operations (1 when nothing was sampled away).
    pub kept_ratio: f64,
    pub kept_requests_count: u64,
    pub seen_requests_count: u64,
    /// Operations kept regardless of the reservoir: the slowest 1 % and every failed or
    /// throttled operation.
    pub always_kept_ops_count: u64,
    /// `whole_op` (v5) — the sampler.
    pub method: String,
}

/// The observer's own cost and the client's health (`report.self`, design v5 §3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SelfStats {
    /// Process CPU and memory of the client (4.x `report.client`).
    pub process: ClientStats,
    /// Measured observer cost per operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_us_per_op: Option<f64>,
    /// Tokio runtime busy share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_busy_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_queue_depth_count: Option<u64>,
    /// p99 of the 1 ms scheduling-lag probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sched_lag_p99_ms: Option<f64>,
    /// Client CPU after warmup, as a share of the cores in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_current_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_peak_bytes: Option<u64>,
    /// Lag p99 > 5 ms or CPU ≥ 90 % of the cores in use: the server/TTFB stage is unreliable.
    pub client_saturated: bool,
}

impl SelfStats {
    /// What 4.x measured (process CPU and peak RSS).
    pub fn from_process(p: &ClientStats) -> Self {
        Self {
            process: p.clone(),
            rss_peak_bytes: (p.rss_mib > 0.0).then_some((p.rss_mib * 1024.0 * 1024.0) as u64),
            ..Self::default()
        }
    }
}
