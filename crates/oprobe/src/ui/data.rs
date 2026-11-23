//! The app's one adapter onto the service layer and its result types: everything the views
//! show is a plain value made here from `oprobe_core` types (`App::list`, `App::show`, the
//! report and its shared views in `report::view`, `OperatorSpec` and its diff). When the core's
//! view model changes, this file is the one to follow it.

use std::collections::BTreeMap;
use std::sync::Arc;

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
    /// The chart that shows its evidence.
    pub chart: Option<view::EvidenceChart>,
    /// The remedy as operator paths with the value to try (empty value: the path alone).
    pub remedy: Vec<(String, String)>,
    /// Operations that show the problem ("Show N examples").
    pub op_ids: Vec<u64>,
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
    /// Waiting before this attempt since the previous one ended (retry backoff), µs.
    pub backoff_us: u64,
    pub start_us: u64,
    pub total_us: u64,
    pub segs: Vec<Seg>,
    pub failed: bool,
    /// What the response and the connection said, as `(what, value)`, in core's order
    /// ([`view::request_signals`]): the provider's error code, `Retry-After`, rtt / min rtt /
    /// retransmits, `Server-Timing`, the HTTP/2 stream wait, flow-control stall and open
    /// streams, the kernel's upload timing, the CDN cache status and the region (only those
    /// the request row has).
    pub signals: Vec<(&'static str, String)>,
}

/// One operation with its requests.
#[derive(Clone, Debug, PartialEq)]
pub struct Waterfall {
    pub op_id: u64,
    pub verb: String,
    pub key: String,
    pub latency_us: u64,
    pub bars: Vec<Bar>,
    /// The async call stack captured while it was in flight (slow operations).
    pub await_tree: Option<String>,
}

/// The request heatmap: time buckets × log latency rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heat {
    pub bucket_s: f64,
    pub bounds_ms: Vec<f64>,
    /// `counts[t][row]`.
    pub counts: Vec<Vec<u64>>,
    pub max: u64,
    /// The time axis's end: the timeline's, so a time reads at the same place on both.
    pub span_s: f64,
    /// The fine rows fitted to the data (every row kept), else the report's 20 fixed rows.
    pub fine: bool,
}

/// One verb with its numbers and where its time went.
#[derive(Clone, Debug, PartialEq)]
pub struct VerbRow {
    pub verb: String,
    pub ops_count: u64,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    pub throttled_ratio: f64,
    pub requests_per_op: f64,
    pub stages: Vec<StageBar>,
}

/// The run with its trace rows, shared by the views (equal when it is the same run, so a
/// model compares without walking its rows).
#[derive(Clone, Debug)]
pub struct Rows(pub Arc<Run>);

impl PartialEq for Rows {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
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
    pub p999_ms: f64,
    pub ops_count: u64,
    pub requests_count: u64,
    pub error_ratio: f64,
    pub throttled_ratio: f64,
    pub stages: Vec<StageBar>,
    pub findings: Vec<FindingRow>,
    pub slowest: Vec<Waterfall>,
    /// The series of the window (every verb).
    pub timeline: view::TimelineView,
    /// The verbs the timeline can be filtered to, busiest first.
    pub timeline_verbs: Vec<String>,
    pub heat: Option<Heat>,
    /// The latency distribution.
    pub cdf: view::LatencyCdf,
    /// Every verb, busiest first.
    pub verbs: Vec<VerbRow>,
    /// The in-flight levels behind a concurrency ceiling (when anything was throttled).
    pub concurrency: Option<view::ConcurrencyView>,
    pub setup: view::SetupView,
    pub trust: view::TrustView,
    /// The backend's requests by API, busiest first.
    pub apis: Vec<view::ApiRow>,
    pub retry: view::RetryView,
    pub bytes: view::BytesView,
    pub spread: view::SpreadView,
    /// Core's connection view: reuse, dials and their phases, the kernel's TCP statistics,
    /// the TLS handshakes and the cold start.
    pub conns: view::ConnectionView,
    /// The multipart part size OpenDAL wrote with and the parts it uploaded (runs that wrote).
    pub write_path: Option<view::WritePathView>,
    pub warnings: Vec<String>,
    /// The run and its rows: what a range, a filter or a drill-down is recomputed from.
    pub rows: Rows,
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

    /// The run has trace rows (ranges and drill-downs need them).
    pub fn has_rows(&self) -> bool {
        !self.rows.0.ops.is_empty()
    }

    /// The timeline of one verb (`None`: every verb).
    pub fn timeline_for(&self, verb: Option<&str>) -> view::TimelineView {
        verb.and_then(|v| view::TimelineView::for_verb(&self.rows.0.report, v))
            .unwrap_or_else(|| self.timeline.clone())
    }

    /// The latency distribution of one verb (`None`, or a verb without rows: every verb).
    pub fn cdf_for(&self, verb: Option<&str>) -> view::LatencyCdf {
        verb.and_then(|v| view::LatencyCdf::for_verb(&self.rows.0.report, v))
            .unwrap_or_else(|| self.cdf.clone())
    }

    /// The time range `[t0, t1)` (seconds into the window), recomputed from the rows.
    pub fn range(&self, t0: f64, t1: f64) -> RangeModel {
        let v = view::RangeView::of(&self.rows.0, t0, t1);
        let slowest = view::examples(&self.rows.0, &v.slowest_op_ids, 3)
            .iter()
            .map(waterfall_of)
            .collect();
        RangeModel {
            verbs: v.verbs.iter().cloned().map(verb_row).collect(),
            slowest,
            view: v,
        }
    }

    /// Operations `ids` (a finding's examples) request by request, at most `n`.
    pub fn examples(&self, ids: &[u64], n: usize) -> Vec<Waterfall> {
        view::examples(&self.rows.0, ids, n)
            .iter()
            .map(waterfall_of)
            .collect()
    }

    /// One operation request by request.
    pub fn waterfall(&self, op_id: u64) -> Option<Waterfall> {
        view::waterfall(&self.rows.0, op_id)
            .as_ref()
            .map(waterfall_of)
    }

    /// The slowest operations behind a heatmap cell: started in `[t0, t1)` with a latency in
    /// `[lo_ms, hi_ms)`.
    pub fn cell_ops(&self, t0: f64, t1: f64, lo_ms: f64, hi_ms: f64) -> Vec<view::OpLine> {
        view::ops_in(&self.rows.0, t0, t1, lo_ms, hi_ms, 8)
    }

    /// The slowest requests of `api`, in the window or in `range`.
    pub fn api_requests(&self, api: &str, range: Option<(f64, f64)>) -> Vec<view::ReqLine> {
        view::slowest_requests(&self.rows.0, api, range, 8)
    }
}

/// A time range of a run as the Profile shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeModel {
    pub view: view::RangeView,
    pub verbs: Vec<VerbRow>,
    pub slowest: Vec<Waterfall>,
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
    Ok(of_rows(Arc::new(run)))
}

fn verb_row(v: view::VerbView) -> VerbRow {
    VerbRow {
        verb: v.verb,
        ops_count: v.ops_count,
        ops_per_s: v.ops_per_s,
        mib_per_s: v.mib_per_s,
        p50_ms: v.p50_ms,
        p99_ms: v.p99_ms,
        error_ratio: v.error_ratio,
        throttled_ratio: v.throttled_ratio,
        requests_per_op: v.requests_per_op,
        stages: v
            .stages
            .into_iter()
            .map(|s| StageBar {
                key: s.key,
                ratio: s.ratio,
            })
            .collect(),
    }
}

/// The model of a run with its trace rows.
pub fn of_run(run: &Run) -> ProfileModel {
    of_rows(Arc::new(run.clone()))
}

fn of_rows(rows: Arc<Run>) -> ProfileModel {
    let run: &Run = &rows;
    let r = &run.report;
    let n = view::NumbersView::of(r);
    let time = view::TimeView::of(r, Some(run));
    let timeline = view::TimelineView::of(r);
    let heat = heat(r, run, &timeline);
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
        p999_ms: n.p999_ms,
        ops_count: n.ops_count,
        requests_count: n.requests_count,
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
        slowest: view::slowest(run, 3).iter().map(waterfall_of).collect(),
        timeline,
        timeline_verbs: view::TimelineView::verbs(r),
        heat,
        cdf: view::LatencyCdf::of(r, Some(run)),
        verbs: view::verbs(r).into_iter().map(verb_row).collect(),
        concurrency: view::ConcurrencyView::of(r),
        setup: view::SetupView::of(r),
        trust: view::TrustView::of(r),
        apis: view::apis(r),
        retry: view::RetryView::of(r),
        bytes: view::BytesView::of(r),
        spread: view::SpreadView::of(r),
        conns: view::ConnectionView::of(r, Some(run)),
        write_path: view::WritePathView::of(r),
        warnings: r.warnings.clone(),
        rows: Rows(rows.clone()),
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
                chart: view::EvidenceChart::of(&f.code),
                remedy: src.map(remedy).unwrap_or_default(),
                op_ids: f.op_ids,
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

/// An operation's waterfall (core's [`view::WaterfallOp`]) as bars: each request from the
/// operation's first event, its stages, the wait before it and what it said.
fn waterfall_of(w: &view::WaterfallOp) -> Waterfall {
    let t0 = w.rows.iter().map(|r| r.start_us).min().unwrap_or(0).min(0);
    Waterfall {
        op_id: w.op_id,
        verb: w.verb.clone(),
        key: w.key.clone(),
        latency_us: w.latency_us,
        bars: w
            .rows
            .iter()
            .map(|r| {
                let parts = [
                    ("queue", r.queue_us),
                    ("connect", r.connect_us),
                    ("send", r.send_us),
                    ("ttfb", r.ttfb_us),
                    ("body", r.body_us),
                    // The body waiting for the consumer: client time, not the network's.
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
                let signals: Vec<(&'static str, String)> = r
                    .signals
                    .iter()
                    .map(|s| {
                        let v = match &s.value {
                            view::SignalValue::Us(us) => crate::ui::fmt::dur_us(*us as f64),
                            view::SignalValue::Ms(ms) => crate::ui::fmt::dur_ms(*ms as f64),
                            view::SignalValue::Count(n) => crate::ui::fmt::count(*n),
                            view::SignalValue::Text(t) => t.clone(),
                        };
                        (s.key, v)
                    })
                    .collect();
                Bar {
                    backoff_us: r.wait_before_us,
                    label: if r.attempt > 1 {
                        format!("{} #{} {status}", r.api, r.attempt)
                    } else {
                        format!("{} {status}", r.api)
                    },
                    start_us: (r.start_us - t0).max(0) as u64,
                    total_us: r.total_us,
                    segs,
                    failed: r.status == 0 || r.status >= 400,
                    signals,
                }
            })
            .collect(),
        await_tree: w.await_tree.clone(),
    }
}

fn heat(r: &Report, run: &Run, t: &view::TimelineView) -> Option<Heat> {
    let h = view::HeatmapView::of(r, Some(run));
    if h.counts.is_empty() {
        return None;
    }
    let cols = h.counts.len() as f64;
    Some(Heat {
        bucket_s: h.bucket_width_s.max(1e-3),
        max: h.max_count(),
        bounds_ms: h.latency_buckets_ms,
        counts: h.counts,
        span_s: (h.bucket_width_s * cols)
            .max(t.t_s.last().zip(t.len_s.last()).map_or(0.0, |(a, b)| a + b)),
        fine: h.fine,
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
    /// A decimal number (`layers.tail_cut.safety_factor`).
    Number,
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
            f("layers.tail_cut.safety_factor", Number, "safety_factor"),
            f("layers.tail_cut.min_samples", Count, "min_samples"),
            f("layers.tail_cut.min_deadline", Duration, "min_deadline"),
            f("layers.tail_cut.max_deadline", Duration, "max_deadline"),
            f("layers.tail_cut.window", Duration, "window"),
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
        "layers.tail_cut.safety_factor" => {
            opt(l.tail_cut.as_ref().map(|t| t.safety_factor.to_string()))
        }
        "layers.tail_cut.min_samples" => {
            opt(l.tail_cut.as_ref().map(|t| t.min_samples.to_string()))
        }
        "layers.tail_cut.min_deadline" => {
            opt(l.tail_cut.as_ref().map(|t| t.min_deadline.to_string()))
        }
        "layers.tail_cut.max_deadline" => opt(l
            .tail_cut
            .as_ref()
            .and_then(|t| t.max_deadline)
            .map(|d| d.to_string())),
        "layers.tail_cut.window" => opt(l.tail_cut.as_ref().map(|t| t.window.to_string())),
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

/// The edits as `PATH=VALUE` pairs in the order they apply (core's
/// [`crate::operator::normalize_edits`]): the preview ([`apply`]) and `prove --with`
/// ([`prove_with`]) read the same list.
pub fn normalized(edits: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let pairs: Vec<(String, String)> = edits.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    crate::operator::normalize_edits(&pairs)
}

/// The edits as `prove --with` arguments.
pub fn prove_with(edits: &BTreeMap<String, String>) -> Vec<String> {
    normalized(edits)
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect()
}

/// `base` with `edits` applied; the first edit that does not apply and why.
pub fn apply(
    base: &OperatorSpec,
    edits: &BTreeMap<String, String>,
) -> (OperatorSpec, Option<(String, String)>) {
    let pairs = normalized(edits);
    let mut whole = base.clone();
    if whole.apply_edits(&pairs).is_ok() {
        return (whole, None);
    }
    // Find the edit at fault: apply them one by one.
    let mut s = base.clone();
    let mut bad = None;
    let mut pending_toggle = None;
    for (k, v) in &pairs {
        let mut next = s.clone();
        match next.apply_edits(&[(k.clone(), v.clone())]) {
            Ok(()) => s = next,
            // A layer switched on that needs a field (`concurrent_limit` needs `http`) is
            // complete once its field applies.
            Err(e) if k.matches('.').count() == 1 && OperatorSpec::check_edit(k, v).is_ok() => {
                pending_toggle.get_or_insert((k.clone(), e.message));
            }
            Err(e) => {
                bad.get_or_insert((k.clone(), e.message));
            }
        }
    }
    let toggle_still_off = pending_toggle
        .as_ref()
        .is_some_and(|(k, _)| value_of(&s, k) != "on");
    let bad = bad.or(pending_toggle.filter(|_| toggle_still_off));
    let bad = bad.or_else(|| {
        whole
            .clone()
            .apply_edits(&pairs)
            .err()
            .map(|e| (String::new(), e.message))
    });
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

/// The runs as the sidebar lists them: grouped under their operator, the group of the newest
/// run first, newest first within a group (the keyboard walks this order).
pub fn sidebar_order(runs: &[RunRow]) -> Vec<RunRow> {
    let mut groups: Vec<(&str, Vec<RunRow>)> = Vec::new();
    for r in runs {
        match groups.iter_mut().find(|g| g.0 == r.operator) {
            Some(g) => g.1.push(r.clone()),
            None => groups.push((r.operator.as_str(), vec![r.clone()])),
        }
    }
    groups.into_iter().flat_map(|(_, v)| v).collect()
}

/// What a run of a real target will do (`App::plan`), for New Run to show before Run.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanView {
    /// One line per operator: `target · preset · N workers · warm-up + window`.
    pub targets: Vec<String>,
    /// Estimated operations and requests, bytes moved, in total.
    pub ops: String,
    pub requests: String,
    pub bytes: String,
    /// Objects written and deleted again, and the prefix they are under.
    pub written: u64,
    pub deleted: u64,
    pub prefixes: Vec<String>,
    pub read_only: bool,
    pub eta_s: f64,
    /// The budgets that stop the run (`None`: none is set).
    pub budgets: Option<String>,
    pub warnings: Vec<String>,
    pub command: String,
}

/// The plan of a run in the app's terms.
pub fn plan_view(p: &crate::app::RunPlan) -> PlanView {
    let b = p
        .plans
        .first()
        .map(|x| x.budgets.clone())
        .unwrap_or_default();
    let mut budgets = Vec::new();
    if let Some(n) = b.max_requests_count {
        budgets.push(crate::ui::i18n::trf("plan.max_requests", &[&n]));
    }
    if let Some(n) = b.max_bytes {
        budgets.push(crate::util::fmt_bytes(n));
    }
    if let Some(s) = b.max_duration_s {
        budgets.push(crate::ui::fmt::clock_s(s));
    }
    PlanView {
        targets: p
            .plans
            .iter()
            .map(|x| {
                crate::ui::i18n::trf(
                    "plan.target",
                    &[
                        &x.target,
                        &x.preset,
                        &x.concurrency_count,
                        &crate::ui::fmt::clock_s(x.warmup_s),
                        &crate::ui::fmt::clock_s(x.duration_s),
                    ],
                )
            })
            .collect(),
        ops: crate::ui::fmt::compact(
            p.plans.iter().map(|x| x.estimated_ops_count).sum::<u64>() as f64
        ),
        requests: crate::ui::fmt::compact(
            p.plans
                .iter()
                .map(|x| x.estimated_requests_count)
                .sum::<u64>() as f64,
        ),
        bytes: crate::util::fmt_bytes(p.plans.iter().map(|x| x.estimated_bytes).sum()),
        written: p.plans.iter().map(|x| x.objects_written_count).sum(),
        deleted: p.plans.iter().map(|x| x.objects_deleted_count).sum(),
        prefixes: p.plans.iter().map(|x| x.run_prefix.clone()).collect(),
        read_only: p.plans.iter().all(|x| x.read_only),
        eta_s: p.plans.iter().map(|x| x.eta_s).sum(),
        budgets: (!budgets.is_empty()).then(|| budgets.join(" · ")),
        warnings: p.plans.iter().flat_map(|x| x.warnings.clone()).collect(),
        command: p
            .plans
            .iter()
            .map(|x| x.command.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    }
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
        assert_eq!(apply(&base, &edits).1.unwrap().0, "read.chunk");
        edits.remove("read.chunk");
        edits.remove("layers.concurrent_limit.http");
        assert_eq!(
            apply(&base, &edits).1.unwrap().0,
            "layers.concurrent_limit",
            "a limit layer without a limit"
        );
        assert_eq!(layer_of("layers.retry.jitter"), Some("layers.retry"));
        assert_eq!(layer_of("layers.retry"), None);
    }

    #[test]
    fn a_waterfall_bar_has_a_stall_segment_core_signals_and_the_call_stack() {
        let mut run = crate::report::fixture::run();
        run.reqs[2].stall_us = 1_500;
        run.reqs[2].detail = Some(Box::new(crate::model::ReqDetail {
            flow_stall_us: Some(7_000),
            cache_status: Some("miss".into()),
            ..Default::default()
        }));
        run.ops[1].await_tree = Some("read\n  http".into());
        let m = of_run(&run);
        let w = m.waterfall(2).unwrap();
        let b = &w.bars[1];
        assert!(b.segs.iter().any(|s| s.stage == "stall" && s.us == 1_500));
        assert!(b.segs.iter().any(|s| s.stage == "body" && s.us == 4_000));
        assert_eq!(
            b.signals,
            vec![
                ("flow_stall", crate::ui::fmt::dur_us(7_000.0)),
                ("cache_status", "miss".to_owned())
            ]
        );
        assert_eq!(w.await_tree.as_deref(), Some("read\n  http"));
        // The connections are core's one model.
        assert_eq!(m.conns, view::ConnectionView::of(&run.report, Some(&run)));
    }

    /// What the app passes to `prove --with` is what its preview shows: core applies the
    /// same edits, whatever their order, to the same operator.
    #[test]
    fn prove_with_applies_what_the_preview_shows() {
        let base = OperatorSpec::from_url("s3://b/x");
        let mut edits = BTreeMap::new();
        for (k, v) in [
            ("layers.concurrent_limit", "on"),
            ("layers.concurrent_limit.http", "24"),
            ("layers.retry", "off"),
            ("layers.retry.max_times", "5"),
            ("layers.tail_cut.percentile", "99"),
            ("read.gap", "1MiB"),
        ] {
            edits.insert(k.to_owned(), v.to_owned());
        }
        let (preview, bad) = apply(&base, &edits);
        assert_eq!(bad, None);
        assert!(preview.layers.retry.is_none());
        let with = prove_with(&edits);
        // Core's `--with` parsing and application (prove's own path).
        let pairs: Vec<(String, String)> = with
            .iter()
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap();
                (k.to_owned(), v.to_owned())
            })
            .collect();
        let mut proved = base.clone();
        proved.apply_edits(&pairs).unwrap();
        assert_eq!(proved, preview);
        // And the raw map, in any order.
        let mut raw: Vec<(String, String)> = edits.clone().into_iter().collect();
        raw.reverse();
        let mut proved = base.clone();
        proved.apply_edits(&raw).unwrap();
        assert_eq!(proved, preview);
    }
}
