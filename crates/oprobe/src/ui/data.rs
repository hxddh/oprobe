//! The app's one adapter onto the service layer and its result types: everything the views
//! show is a plain value made here from `oprobe_core` types (`App::list`, `App::show`, the
//! report and its shared views in `report::view`, `OperatorSpec` and its diff). When the core's
//! view model changes, this file is the one to follow it.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::analysis::diagnosis::Severity;
use crate::app::{App, RunRef};
use crate::model::{Finding, Report, Run, VerdictLevel};
use crate::operator::OperatorSpec;
use crate::report::view;

/// One stored run in the history.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRow {
    pub id: String,
    pub operator: String,
    pub preset: String,
    pub started_at: String,
    pub verdict: VerdictLevel,
    pub headline: String,
    pub ops_per_s: f64,
    pub p99_ms: f64,
    pub partial: bool,
}

/// The stored runs, newest first (`App::list`).
pub fn list(app: &App) -> Result<Vec<RunRow>> {
    Ok(app
        .list()?
        .runs
        .into_iter()
        .map(|r| RunRow {
            operator: r
                .target_name
                .clone()
                .unwrap_or_else(|| crate::util::display_url(&r.target_display)),
            preset: r.preset.clone().unwrap_or_default(),
            partial: r.partial.is_some() || r.interrupted,
            id: r.id,
            started_at: r.started_at,
            verdict: r.verdict_level,
            headline: r.headline,
            ops_per_s: r.ops_per_s,
            p99_ms: r.p99_ms,
        })
        .collect())
}

/// One stage's share of operation time.
#[derive(Clone, Debug, PartialEq)]
pub struct StageBar {
    pub key: String,
    pub ratio: f64,
}

/// A finding as the app shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct FindingRow {
    pub code: String,
    pub severity: Severity,
    pub cause: String,
    pub evidence: String,
    pub advice: String,
    pub items: Vec<(String, String)>,
    /// The remedy as operator paths with the value to try (empty value: the path alone).
    pub remedy: Vec<(String, String)>,
}

/// One stage segment of a request bar (µs).
#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub stage: &'static str,
    pub us: u64,
}

/// One request of a waterfall: when it started after the operation, and its stages.
#[derive(Clone, Debug, PartialEq)]
pub struct Bar {
    pub label: String,
    pub start_us: u64,
    pub total_us: u64,
    pub segs: Vec<Seg>,
    pub failed: bool,
}

/// One of the slowest operations with its requests.
#[derive(Clone, Debug, PartialEq)]
pub struct Waterfall {
    pub op_id: u64,
    pub verb: String,
    pub key: String,
    pub latency_us: u64,
    pub bars: Vec<Bar>,
}

/// One second of the measurement window.
#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub t_s: f64,
    pub ops_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub errors: u64,
    pub throttled: u64,
}

/// The request heatmap: time buckets × log latency rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heat {
    pub bucket_s: f64,
    pub bounds_ms: Vec<f64>,
    /// `counts[t][row]`.
    pub counts: Vec<Vec<u64>>,
    pub max: u64,
}

/// The connections of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Conns {
    pub dialed: u64,
    pub reuse_ratio: f64,
    pub http1: u64,
    pub http2: u64,
    pub tls: String,
    pub peak_inflight: u64,
    /// `(stage, p50 ms, p99 ms)` of DNS, TCP and TLS.
    pub setup: Vec<(&'static str, f64, f64)>,
}

/// Everything the Profile view shows of one run.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileModel {
    pub id: String,
    pub operator_ref: String,
    pub operator: OperatorSpec,
    pub preset: String,
    pub window_s: f64,
    pub verdict: VerdictLevel,
    pub headline: String,
    pub good_ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    pub throttled_ratio: f64,
    pub stages: Vec<StageBar>,
    pub findings: Vec<FindingRow>,
    pub slowest: Vec<Waterfall>,
    pub timeline: Vec<Point>,
    pub heat: Option<Heat>,
    pub conns: Conns,
    pub warnings: Vec<String>,
}

impl ProfileModel {
    /// The objective value of this run (goodput or p99).
    pub fn objective(&self, p99: bool) -> f64 {
        if p99 {
            self.p99_ms
        } else {
            self.good_ops_per_s
        }
    }
}

/// A stored run, ready to show (`App::show`).
pub fn profile(app: &App, id: &str) -> Result<ProfileModel> {
    let p = app.show(RunRef::from(id))?;
    let run = match p.full {
        Some(run) => *run,
        None => Run {
            report: p.run.report,
            ops: Vec::new(),
            reqs: Vec::new(),
        },
    };
    Ok(of_run(&run))
}

/// The model of a run with its trace rows.
pub fn of_run(run: &Run) -> ProfileModel {
    let r = &run.report;
    let n = view::NumbersView::of(r);
    let time = view::TimeView::of(r, Some(run));
    let c = &r.connections;
    ProfileModel {
        id: r.run.id.clone(),
        operator_ref: r.spec.operator_ref.clone(),
        operator: r.spec.profile.operator.clone(),
        preset: r.workload_label().to_owned(),
        window_s: r.run.window_s,
        verdict: r.verdict.level,
        headline: r.verdict.headline.clone(),
        good_ops_per_s: n.goodput_ops_per_s,
        mib_per_s: n.mib_per_s,
        p50_ms: n.p50_ms,
        p99_ms: n.p99_ms,
        error_ratio: n.error_ratio,
        throttled_ratio: n.throttled_request_ratio,
        stages: time
            .stages
            .into_iter()
            .map(|s| StageBar {
                key: s.key,
                ratio: s.ratio,
            })
            .collect(),
        findings: findings(r),
        slowest: slowest(run, 6),
        timeline: r
            .timeline
            .iter()
            .map(|p| Point {
                t_s: p.t_s,
                ops_per_s: p.ops_per_s,
                p50_ms: p.p50_ms,
                p99_ms: p.p99_ms,
                errors: p.errors,
                throttled: p.throttled,
            })
            .collect(),
        heat: heat(r),
        conns: Conns {
            dialed: c.dialed,
            reuse_ratio: c.reuse_pct / 100.0,
            http1: c.http1_requests,
            http2: c.http2_requests,
            tls: [c.tls_version.as_str(), c.alpn.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · "),
            peak_inflight: c.peak_inflight_requests,
            setup: vec![
                ("dns", c.dns.p50, c.dns.p99),
                ("tcp", c.tcp.p50, c.tcp.p99),
                ("tls", c.tls.p50, c.tls.p99),
            ],
        },
        warnings: r.warnings.clone(),
    }
}

fn findings(r: &Report) -> Vec<FindingRow> {
    view::findings(r)
        .into_iter()
        .map(|f| {
            let src = r.findings.iter().find(|x| x.code == f.code);
            FindingRow {
                code: f.code.clone(),
                severity: f.severity,
                cause: f.cause,
                evidence: f.evidence,
                advice: f.remedy,
                items: f.items.into_iter().map(|i| (i.label, i.value)).collect(),
                remedy: src.map(remedy).unwrap_or_default(),
            }
        })
        .collect()
}

/// The remedy of a finding as operator paths: the finding's own remedy path (and the paths
/// changed with it), else the remedy path of its code (design v6 §8 "Findings → remedies").
/// Values are left empty: `oprobe prove` sizes them.
pub fn remedy(f: &Finding) -> Vec<(String, String)> {
    if let Some(path) = &f.remedy.path {
        return std::iter::once(path)
            .chain(f.remedy.with.iter())
            .map(|p| (p.clone(), String::new()))
            .collect();
    }
    remedy_paths(&f.code)
        .iter()
        .map(|p| ((*p).to_owned(), String::new()))
        .collect()
}

/// The `OperatorSpec` paths that remedy a finding code.
pub fn remedy_paths(code: &str) -> &'static [&'static str] {
    match code {
        "throttling" => &["layers.concurrent_limit.http", "layers.retry.jitter"],
        "rate_limit" => &["layers.concurrent_limit.http"],
        "bandwidth_cap" => &["layers.throttle.bandwidth"],
        "tail_stragglers" | "tail_latency" => &["layers.tail_cut"],
        "hung_requests" => &["layers.timeout.io"],
        "small_ranges" | "overread" => &["read.gap", "read.concurrent"],
        "slow_multipart" | "upload_bound" => &["write.chunk", "write.concurrent"],
        "repeated_reads" | "cache_opportunity" => &["layers.cache"],
        "cold_connections" => &["http.keep_alive"],
        "http1_connections" => &["http.version"],
        _ => &[],
    }
}

fn slowest(run: &Run, n: usize) -> Vec<Waterfall> {
    let mut ids: Vec<u64> = run.report.slowest.clone();
    if ids.is_empty() {
        let mut ops: Vec<_> = run.ops.iter().collect();
        ops.sort_by_key(|o| std::cmp::Reverse(o.latency_us));
        ids = ops.iter().map(|o| o.op_id).collect();
    }
    ids.into_iter()
        .filter_map(|id| run.op(id))
        .take(n)
        .map(|op| {
            let mut reqs: Vec<_> = run.reqs.iter().filter(|r| r.op_id == op.op_id).collect();
            reqs.sort_by_key(|r| (r.t_us, r.attempt));
            let t0 = reqs
                .iter()
                .map(|r| r.t_us)
                .min()
                .unwrap_or(op.t_us)
                .min(op.t_us);
            Waterfall {
                op_id: op.op_id,
                verb: op.verb.to_string(),
                key: op.key.clone(),
                latency_us: op.latency_us,
                bars: reqs
                    .into_iter()
                    .map(|r| {
                        let parts = [
                            ("queue", r.queue_us),
                            ("connect", r.connect_us()),
                            ("send", r.send_us),
                            ("ttfb", r.ttfb_us),
                            ("body", r.body_us),
                            ("stall", r.stall_us),
                        ];
                        let mut segs: Vec<Seg> = parts
                            .into_iter()
                            .filter(|(_, us)| *us > 0)
                            .map(|(stage, us)| Seg { stage, us })
                            .collect();
                        let sum: u64 = segs.iter().map(|s| s.us).sum();
                        if r.total_us > sum {
                            segs.push(Seg {
                                stage: "client",
                                us: r.total_us - sum,
                            });
                        }
                        let status = if r.status == 0 {
                            "—".to_owned()
                        } else {
                            r.status.to_string()
                        };
                        Bar {
                            label: if r.attempt > 1 {
                                format!("{} #{} {status}", r.api, r.attempt)
                            } else {
                                format!("{} {status}", r.api)
                            },
                            start_us: (r.t_us - t0).max(0) as u64,
                            total_us: r.total_us,
                            segs,
                            failed: r.status == 0 || r.status >= 400,
                        }
                    })
                    .collect(),
            }
        })
        .collect()
}

fn heat(r: &Report) -> Option<Heat> {
    let h = &r.heatmap;
    if h.counts.is_empty() {
        return None;
    }
    let max = h.counts.iter().flatten().copied().max().unwrap_or(0);
    Some(Heat {
        bucket_s: h.bucket_width_s.max(1e-3),
        bounds_ms: h.latency_buckets_ms.clone(),
        counts: h.counts.clone(),
        max,
    })
}

// ---------------------------------------------------------------------------------------
// The operator: its layers, read/write and HTTP settings as fields
// ---------------------------------------------------------------------------------------

/// What kind of value a field holds (decides its control).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// A layer in or out of the stack (`layers.NAME`).
    Layer,
    Count,
    Size,
    Duration,
    Switch,
    /// One of a few words (`http.version`).
    Choice(&'static [&'static str]),
}

/// One editable value of the operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub path: &'static str,
    pub kind: FieldKind,
    /// The OpenDAL API it sets.
    pub api: &'static str,
}

const fn f(path: &'static str, kind: FieldKind, api: &'static str) -> Field {
    Field { path, kind, api }
}

use FieldKind::*;

/// The operator's sections, each with its fields (a layer first, then its parameters).
pub const SECTIONS: &[(&str, &[Field])] = &[
    (
        "layers",
        &[
            f("layers.concurrent_limit", Layer, "ConcurrentLimitLayer"),
            f(
                "layers.concurrent_limit.http",
                Count,
                "with_http_concurrent_limit",
            ),
            f("layers.retry", Layer, "RetryLayer"),
            f("layers.retry.max_times", Count, "with_max_times"),
            f("layers.retry.jitter", Switch, "with_jitter"),
            f("layers.retry.min_delay", Duration, "with_min_delay"),
            f("layers.retry.max_delay", Duration, "with_max_delay"),
            f("layers.timeout", Layer, "TimeoutLayer"),
            f("layers.timeout.op", Duration, "with_timeout"),
            f("layers.timeout.io", Duration, "with_io_timeout"),
            f("layers.tail_cut", Layer, "TailCutLayer"),
            f("layers.tail_cut.percentile", Count, "percentile"),
            f("layers.throttle", Layer, "ThrottleLayer"),
            f("layers.throttle.bandwidth", Size, "bandwidth"),
            f("layers.throttle.burst", Size, "burst"),
            f("layers.cache", Layer, "FoyerLayer"),
            f("layers.cache.memory", Size, "memory"),
        ],
    ),
    (
        "read",
        &[
            f("read.chunk", Size, "read_with().chunk()"),
            f("read.concurrent", Count, "read_with().concurrent()"),
            f("read.gap", Size, "read_with().gap()"),
        ],
    ),
    (
        "write",
        &[
            f("write.chunk", Size, "writer_with().chunk()"),
            f("write.concurrent", Count, "writer_with().concurrent()"),
        ],
    ),
    (
        "http",
        &[
            f("http.version", Choice(&["auto", "h1", "h2"]), "HttpFetch"),
            f("http.keep_alive", Switch, "HttpFetch"),
        ],
    ),
];

/// The layer a field belongs to (`layers.retry.jitter` → `layers.retry`), if any.
pub fn layer_of(path: &str) -> Option<&str> {
    let mut parts = path.splitn(3, '.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("layers"), Some(_), Some(_)) => path.rfind('.').map(|i| &path[..i]),
        _ => None,
    }
}

/// The current value of `path` in `s` as text (`""` when unset; `on`/`off` for layers and
/// switches).
pub fn value_of(s: &OperatorSpec, path: &str) -> String {
    let on = |b: bool| if b { "on" } else { "off" }.to_owned();
    let opt = |v: Option<String>| v.unwrap_or_default();
    let l = &s.layers;
    match path {
        "layers.concurrent_limit" => on(l.concurrent_limit.is_some()),
        "layers.concurrent_limit.http" => opt(l
            .concurrent_limit
            .as_ref()
            .and_then(|c| c.http)
            .map(|n| n.to_string())),
        "layers.retry" => on(l.retry.is_some()),
        "layers.retry.max_times" => opt(l.retry.as_ref().map(|r| r.max_times.to_string())),
        "layers.retry.jitter" => on(l.retry.as_ref().is_some_and(|r| r.jitter)),
        "layers.retry.min_delay" => opt(l.retry.as_ref().map(|r| r.min_delay.to_string())),
        "layers.retry.max_delay" => opt(l.retry.as_ref().map(|r| r.max_delay.to_string())),
        "layers.timeout" => on(l.timeout.is_some()),
        "layers.timeout.op" => opt(l.timeout.as_ref().map(|t| t.op.to_string())),
        "layers.timeout.io" => opt(l.timeout.as_ref().map(|t| t.io.to_string())),
        "layers.tail_cut" => on(l.tail_cut.is_some()),
        "layers.tail_cut.percentile" => opt(l.tail_cut.as_ref().map(|t| t.percentile.to_string())),
        "layers.throttle" => on(l.throttle.is_some()),
        "layers.throttle.bandwidth" => opt(l.throttle.as_ref().map(|t| t.bandwidth.to_string())),
        "layers.throttle.burst" => opt(l.throttle.as_ref().map(|t| t.burst.to_string())),
        "layers.cache" => on(l.cache.is_some()),
        "layers.cache.memory" => opt(l.cache.as_ref().map(|c| c.memory.to_string())),
        "read.chunk" => opt(s.read.chunk.map(|v| v.to_string())),
        "read.concurrent" => opt(s.read.concurrent.map(|v| v.to_string())),
        "read.gap" => opt(s.read.gap.map(|v| v.to_string())),
        "write.chunk" => opt(s.write.chunk.map(|v| v.to_string())),
        "write.concurrent" => opt(s.write.concurrent.map(|v| v.to_string())),
        "http.version" => match s.http.version {
            crate::net::HttpVersion::Auto => "auto",
            crate::net::HttpVersion::H1 => "h1",
            crate::net::HttpVersion::H2 => "h2",
        }
        .to_owned(),
        "http.keep_alive" => on(s.http.keep_alive),
        _ => String::new(),
    }
}

/// `base` with `edits` applied in order; the first edit that does not apply and why.
pub fn apply(
    base: &OperatorSpec,
    edits: &BTreeMap<String, String>,
) -> (OperatorSpec, Option<(String, String)>) {
    let mut s = base.clone();
    let mut failed = Vec::new();
    // Layers first, so a field of a layer just switched on lands in it; the fields of a layer
    // switched off are left out.
    let mut ordered: Vec<(&String, &String)> = edits
        .iter()
        .filter(|(k, _)| layer_of(k).is_none_or(|l| edits.get(l).is_none_or(|v| v != "off")))
        .collect();
    ordered.sort_by_key(|(k, _)| layer_of(k).is_some());
    for (k, v) in ordered {
        if let Err(e) = s.set(k, v) {
            failed.push((k.clone(), e.message));
        }
    }
    // A layer that needs a parameter to be on (`concurrent_limit`) is on once one is set.
    let bad = failed
        .into_iter()
        .find(|(k, _)| layer_of(k).is_some() || value_of(&s, k) != "on");
    (s, bad)
}

/// The change from `base` to `edited` as Rust and as `op.toml` lines.
pub fn diff(base: &OperatorSpec, edited: &OperatorSpec) -> (String, String) {
    let d = crate::operator::diff::diff(base, edited);
    (d.rust, d.toml)
}

// ---------------------------------------------------------------------------------------
// Compare
// ---------------------------------------------------------------------------------------

/// One metric across the compared runs: the values and, from the second on, the change
/// against the first.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricRow {
    pub key: &'static str,
    pub higher_is_better: bool,
    pub values: Vec<f64>,
}

/// The metrics compared side by side.
pub fn compare_rows(ps: &[ProfileModel]) -> Vec<MetricRow> {
    let row = |key, higher_is_better, f: fn(&ProfileModel) -> f64| MetricRow {
        key,
        higher_is_better,
        values: ps.iter().map(f).collect(),
    };
    vec![
        row("goodput", true, |p| p.good_ops_per_s),
        row("bandwidth", true, |p| p.mib_per_s),
        row("p50", false, |p| p.p50_ms),
        row("p99", false, |p| p.p99_ms),
        row("errors", false, |p| p.error_ratio),
        row("throttled", false, |p| p.throttled_ratio),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_reads_and_writes_back() {
        let base = OperatorSpec::from_url("memory://");
        for (_, fields) in SECTIONS {
            for fd in *fields {
                let v = value_of(&base, fd.path);
                if fd.kind == FieldKind::Layer {
                    assert!(v == "on" || v == "off", "{}: {v}", fd.path);
                }
            }
        }
        let mut edits = BTreeMap::new();
        edits.insert("layers.concurrent_limit.http".to_owned(), "24".to_owned());
        edits.insert("layers.concurrent_limit".to_owned(), "on".to_owned());
        edits.insert("layers.retry.jitter".to_owned(), "on".to_owned());
        let (s, bad) = apply(&base, &edits);
        assert_eq!(bad, None);
        assert_eq!(value_of(&s, "layers.concurrent_limit.http"), "24");
        assert_eq!(value_of(&s, "layers.retry.jitter"), "on");
        let (rust, toml) = diff(&base, &s);
        assert!(rust.contains("ConcurrentLimitLayer"), "{rust}");
        assert!(toml.contains("concurrent_limit"), "{toml}");
        edits.insert("read.chunk".to_owned(), "1".to_owned());
        assert!(apply(&base, &edits).1.is_some());
        assert_eq!(layer_of("layers.retry.jitter"), Some("layers.retry"));
        assert_eq!(layer_of("layers.retry"), None);
    }
}
