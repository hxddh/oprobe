//! Renderers of the Report v3 (`crate::model::Report`) and the regression gate.

pub mod brief;

pub mod diff;
pub mod html;
mod html_assets;
pub mod markdown;
pub mod style;
pub mod terminal;
pub mod view;

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::model::{REPORT_SCHEMA as SCHEMA, Report, Run};

/// Summary of an auto-tuning session (the tuner's own form; reports embed
/// [`crate::model::TuneReport`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TuneSummary {
    /// `goodput`, `p99`, `cost` or `balanced`.
    pub objective: String,
    /// What goodput was measured in: `ops_per_s` or `mib_per_s`.
    pub metric_unit: String,
    pub trials: Vec<crate::tune::Trial>,
    pub baseline_trial: usize,
    /// The trial whose knobs are recommended.
    pub recommended_trial: usize,
    /// The recommendation's confirmation run (a longer window).
    pub winner_trial: usize,
    /// The best trial within the SLA and the throttle budget, ignoring the safety margin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_feasible_trial: Option<usize>,
    /// The baseline met the SLA and the throttle budget.
    pub baseline_feasible: bool,
    /// Objective improvement of the recommendation vs the baseline (ratio, positive =
    /// better); absent when the baseline was infeasible or above the margin (never negative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub improvement: Option<f64>,
    /// One line: what is recommended and why (never a negative improvement).
    pub headline: String,
    /// How much better the best feasible trial measured than the recommendation (the cost of
    /// the safety margin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_cost_ratio: Option<f64>,
    /// Lowest concurrency that exceeded the throttle budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_ceiling: Option<usize>,
    /// The capacity model's concurrency within the margin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_concurrency: Option<usize>,
    /// The pooled capacity estimate over every trial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<crate::analysis::capacity::Capacity>,
    pub max_throttle_ratio: f64,
    /// Trials not dominated on (goodput, p99, cost, throttling).
    #[serde(default)]
    pub pareto: Vec<usize>,
    /// No trial met the SLA: the winner is the least-bad configuration, not a recommendation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_feasible_trial: bool,
    pub notes: Vec<String>,
}

/// Load a report from a JSON file (a bare report or a `-o json` command result).
pub fn load_json(path: &Path) -> Result<Report> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read report `{}`", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("`{}` is not valid JSON", path.display()))?;
    Report::from_json_value(v).with_context(|| format!("cannot load `{}`", path.display()))
}

/// Pretty JSON of a report (what `--json` writes).
/// `http/1.1 · TLSv1.3 · TLS13_AES_128_GCM_SHA256`, skipping parts the transport did not report.
pub fn connection_protocol(c: &crate::trace::ConnStats) -> String {
    let mut parts = vec![if c.alpn.is_empty() {
        "http/1.1".to_owned()
    } else {
        c.alpn.clone()
    }];
    parts.push(if c.tls_version.is_empty() {
        "plain".to_owned()
    } else {
        c.tls_version.clone()
    });
    if !c.cipher.is_empty() {
        parts.push(c.cipher.clone());
    }
    parts.join(" · ")
}

/// `dns 1.2ms/3.4ms · tcp … · proxy … · tls …` (p50/p99), only for phases that took time.
pub fn dial_phases(c: &crate::trace::ConnStats) -> String {
    use crate::util::fmt_ms;
    [
        ("dns", &c.dns),
        ("tcp", &c.tcp),
        ("proxy", &c.proxy_connect),
        ("tls", &c.tls),
    ]
    .iter()
    .filter(|(_, q)| q.count > 0 && q.p99 > 0.0)
    .map(|(n, q)| format!("{n} {}/{}", fmt_ms(q.p50), fmt_ms(q.p99)))
    .collect::<Vec<_>>()
    .join(" · ")
}

pub fn to_json(r: &Report) -> String {
    serde_json::to_string_pretty(r).unwrap_or_default()
}

/// Write `content` to `path` atomically (temporary file + rename), creating parent directories.
pub fn write_file(path: &Path, content: &str) -> Result<()> {
    crate::store::atomic_write(path, content.as_bytes())
}

/// A small but complete run for renderer tests (test support, also used by `oprobe`'s tests).
#[doc(hidden)]
pub mod fixture {
    use std::borrow::Cow;

    use crate::analysis::diagnosis::Severity;
    use crate::model::*;
    use crate::trace::{Quantiles, TimelinePoint, VerbStats};

    pub fn report() -> Report {
        let q = |p50: f64, p99: f64| Quantiles {
            count: 100,
            mean: p50 * 1.2,
            p50,
            p90: p99 * 0.7,
            p99,
            p999: p99 * 1.5,
            max: p99 * 2.0,
        };
        let mut r = Report {
            spec: Default::default(),
            capacity: Default::default(),
            schema: REPORT_SCHEMA.into(),
            run: RunInfo {
                id: "20261120-143005-a1b2".into(),
                label: Some("base".into()),
                started_at: "2026-11-20T14:30:05Z".into(),
                ended_at: "2026-11-20T14:30:15Z".into(),
                window_s: 10.0,
                oprobe_version: "3.0.0".into(),
                ..Default::default()
            },
            target: TargetInfo {
                name: Some("prod".into()),
                display: "s3://bucket/prefix".into(),
                scheme: "s3".into(),
                ..Default::default()
            },
            workload: WorkloadInfo {
                preset: "parquet".into(),
                duration_s: 10.0,
                access: "uniform".into(),
                ..Default::default()
            },
            knobs: Default::default(),
            sla: Default::default(),
            verdict: Verdict {
                level: VerdictLevel::Attention,
                headline: "61% of operation time is server time to first byte".into(),
                ..Default::default()
            },
            summary: Default::default(),
            stages: Default::default(),
            apis: vec![],
            verbs: vec![VerbStats {
                verb: "read.range".into(),
                count: 1000,
                ops_per_s: 100.0,
                mib_per_s: 12.5,
                latency: q(8.0, 120.0),
                requests_per_op: 1.1,
                ..Default::default()
            }],
            connections: Default::default(),
            network: Default::default(),
            resilience: Default::default(),
            bytes: Default::default(),
            cache: Default::default(),
            integrity: Default::default(),
            client: Default::default(),
            self_stats: Default::default(),
            schedule: Default::default(),
            workers: Vec::new(),
            dimensions: Default::default(),
            sampling: Default::default(),
            open_loop: None,
            timeline: (1..=10)
                .map(|t| TimelinePoint {
                    t_s: t as f64,
                    ops_per_s: 90.0 + t as f64,
                    mib_per_s: 12.0,
                    p50_ms: 8.0,
                    p99_ms: if t == 6 { 900.0 } else { 110.0 },
                    errors: u64::from(t == 6),
                    throttled: if t == 6 { 12 } else { 0 },
                    len_s: 1.0,
                    ..Default::default()
                })
                .collect(),
            heatmap: Default::default(),
            findings: vec![Finding {
                code: "throttling".into(),
                severity: Severity::Warning,
                cause: "The service throttled 12 requests (503 SlowDown)".into(),
                evidence: Evidence {
                    text: "12 of 1100 requests answered 503 at 5–6 s".into(),
                    op_ids: vec![2],
                    windows: vec![TimeWindow {
                        from_s: 5.0,
                        to_s: 6.0,
                    }],
                    ..Default::default()
                },
                advice: "Lower the concurrency to 16".into(),
                verification: Some(Verification {
                    description: "re-run with fewer workers".into(),
                    rerun: "oprobe run --from 20261120-143005-a1b2 --set concurrency=16".into(),
                    ..Default::default()
                }),
            }],
            cost: Default::default(),
            slowest: vec![2, 1],
            cleanup: Default::default(),
            warnings: vec![],
            tune: None,
        };
        r.summary.ops = 1000;
        r.summary.ops_per_s = 100.0;
        r.summary.latency = q(8.0, 120.0);
        r.stages.shares.server_pct = 61.0;
        r.stages.shares.queue_pct = 20.0;
        r.stages.shares.download_pct = 19.0;
        r.resilience.throttled_responses = 12;
        r
    }

    pub fn req(op_id: u64, t_us: i64, status: u16, reused: bool) -> ReqRow {
        ReqRow {
            t_us,
            op_id,
            attempt: 1,
            api: Cow::Borrowed("GetObject"),
            method: Cow::Borrowed("GET"),
            status,
            protocol: WireProtocol::H1,
            reused,
            conn_id: 3,
            remote: Some("10.0.0.1:443".parse().unwrap()),
            queue_us: 100,
            dns_us: if reused { 0 } else { 2_000 },
            tcp_us: if reused { 0 } else { 3_000 },
            proxy_us: 0,
            tls_us: if reused { 0 } else { 9_000 },
            send_us: 50,
            ttfb_us: 30_000,
            body_us: 4_000,
            stall_us: 0,
            total_us: if reused { 34_150 } else { 48_150 },
            bytes_up: 0,
            bytes_down: 65_536,
            request_id: Some("REQ1".into()),
            retry_after_ms: None,
            error_code: None,
            error: None,
            tls: None,
            tcp: None,
        }
    }

    pub fn run() -> Run {
        let mut report = report();
        let op = |op_id: u64, t_us: i64, latency_us: u64| OpRow {
            op_id,
            verb: Cow::Borrowed("read.range"),
            key: format!("bench/obj-{op_id}"),
            t_us,
            latency_us,
            bytes: 65_536,
            ok: true,
            error_code: None,
            error: None,
            throttled: op_id == 2,
            attempts: if op_id == 2 { 2 } else { 1 },
            breakdown: OpBreakdownUs {
                backend_us: latency_us,
                ..Default::default()
            },
            requests: 1,
            await_tree: None,
        };
        let ops = vec![op(1, 1_000, 48_150), op(2, 5_200_000, 900_000)];
        let mut throttled = req(2, 5_200_000, 503, true);
        throttled.error = Some("SlowDown".into());
        let reqs = vec![req(1, 1_000, 200, false), throttled, {
            let mut r = req(2, 5_300_000, 200, true);
            r.attempt = 2;
            r
        }];
        report.heatmap = Heatmap::from_ops(&ops, 10.0, 60);
        Run { report, ops, reqs }
    }
}
