//! Report sections: the arrival schedule ([`ScheduleStats`]), dimensions ([`Dimensions`]),
//! sampling ([`Sampling`]) and the observer's own cost and health ([`SelfStats`],
//! `report.self`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::trace::{ClientStats, OpenLoopStats, Quantiles};

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

/// Missed-arrival share above which capacity advice is suppressed and `schedule_backlog`
/// explains the backlog (design v5 §3).
pub const SCHEDULE_BACKLOG_RATIO: f64 = 0.01;

impl ScheduleStats {
    /// The schedule of a run: open-loop arrivals (intended, missed, the schedule stage), or a
    /// closed-loop run (`open_loop: false`: subject to coordinated omission).
    pub fn from_open_loop(o: Option<&OpenLoopStats>, window_s: f64) -> Self {
        let Some(o) = o else {
            return Self::default();
        };
        // Runs recorded before the intended count existed: estimate it from the rate.
        let intended = if o.intended_count > 0 {
            o.intended_count
        } else {
            (o.target_rate * window_s).round().max(0.0) as u64
        };
        Self {
            open_loop: true,
            arrivals: Some(if o.arrivals.is_empty() {
                "constant".into()
            } else {
                o.arrivals.clone()
            }),
            intended_ops_count: intended.max(o.missed),
            missed_arrivals_count: o.missed,
            missed_arrivals_ratio: if intended > 0 {
                (o.missed as f64 / intended as f64).min(1.0)
            } else {
                0.0
            },
            schedule_stage: o.schedule,
            max_schedule_lag_ms: o.max_schedule_lag_ms,
        }
    }

    /// More than [`SCHEDULE_BACKLOG_RATIO`] of the arrivals never started: the latency tail
    /// holds censored values and the throughput is what the client managed, not the target.
    pub fn backlogged(&self) -> bool {
        self.open_loop && self.missed_arrivals_ratio > SCHEDULE_BACKLOG_RATIO
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

impl Dimensions {
    /// The report form of the recorder's dimension aggregates.
    pub fn from_stats(d: Option<&crate::trace::dims::DimensionStats>) -> Self {
        let Some(d) = d else {
            return Self::default();
        };
        let total = d.total_requests_count.max(1) as f64;
        let slice = |s: &crate::trace::dims::SliceStats| DimensionSlice {
            key: s.key.clone(),
            ops_count: s.ops_count,
            requests_count: s.requests_count,
            share_ratio: s.requests_count as f64 / total,
            throttled_ratio: if s.ops_count == 0 {
                0.0
            } else {
                s.throttled_count as f64 / s.ops_count as f64
            },
            error_ratio: if s.ops_count == 0 {
                0.0
            } else {
                s.errors_count as f64 / s.ops_count as f64
            },
            p99_ms: s.p99_ms,
        };
        Self {
            prefix_depth_count: d.prefix_depth_count,
            prefixes: d.prefixes.iter().map(slice).collect(),
            hot_keys: d
                .hot_keys
                .iter()
                .map(|k| HotKey {
                    key: k.key.clone(),
                    requests_count: k.requests_count,
                    share_ratio: k.requests_count as f64 / total,
                    throttle_share_ratio: if d.total_throttled_count == 0 {
                        0.0
                    } else {
                        k.throttled_count as f64 / d.total_throttled_count as f64
                    },
                })
                .collect(),
            size_buckets: d.size_buckets.iter().map(slice).collect(),
            cold_warm: d.cold_warm.iter().map(slice).collect(),
            tenants: Vec::new(),
            workers: Vec::new(),
        }
    }
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

impl Sampling {
    /// What the whole-operation sampler kept.
    pub fn from_stats(s: &crate::trace::SamplingStats) -> Self {
        Self {
            kept_ops_count: s.kept_ops,
            seen_ops_count: s.seen_ops,
            kept_ratio: s.kept_ratio(),
            kept_requests_count: s.kept_requests,
            seen_requests_count: s.seen_requests,
            always_kept_ops_count: s.always_kept_ops,
            method: "whole_op".into(),
        }
    }
}

/// The observer's own cost and the client's health (`report.self`, design v5 §3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SelfStats {
    /// Process CPU and memory of the client (4.x `report.client`).
    pub process: ClientStats,
    /// Measured observer cost per operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_per_op_us: Option<f64>,
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

    /// The process stats plus what the self probe measured over the window.
    pub fn from_observation(p: &ClientStats, o: Option<&crate::trace::SelfObservation>) -> Self {
        let mut s = Self::from_process(p);
        let Some(o) = o else {
            return s;
        };
        s.observer_per_op_us = o.observer_per_op_us;
        s.runtime_busy_ratio = o.runtime_busy_ratio;
        s.global_queue_depth_count = o.global_queue_depth_max_count;
        s.sched_lag_p99_ms = o.sched_lag_p99_ms;
        s.cpu_ratio = o.cpu_ratio;
        s.rss_current_bytes = o.rss_current_bytes;
        s.rss_peak_bytes = o.rss_peak_bytes.or(s.rss_peak_bytes);
        s.client_saturated = o.client_saturated;
        s
    }
}
