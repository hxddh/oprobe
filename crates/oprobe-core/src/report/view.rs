//! The one presentation model (design v6 §4: "the app has no logic of its own … and shares
//! one view model with the text output"). Every section is computed here once, from a
//! [`Report`] (and its trace rows when there are any), and rendered by the brief, `show`, and
//! the app. A renderer decides layout and markup only; what a section says — which stages,
//! which labels, which evidence, in which order — is decided here.
//!
//! All types own their data (no lifetimes), so a front end can keep them across frames.
//!
//! | Section | Type |
//! |---|---|
//! | verdict | [`VerdictView`] |
//! | findings with their remedy | [`FindingView`] |
//! | key numbers | [`NumbersView`] |
//! | where the time went (all operations, and the slowest — T4) | [`TimeView`] |
//! | slowest operations, request by request | [`WaterfallOp`] / [`WaterfallRow`] |
//! | timeline series | [`TimelineView`] |
//! | connections | [`ConnectionView`] |
//! | operator stack beside each finding's remedy | [`OperatorView`] |
//! | prove: sizing and A/B pairs | [`ProveView`] |
//! | several operators side by side | [`CompareView`] |
//! | latency distribution | [`LatencyCdf`] |
//! | each verb with its own stage split | [`VerbView`] |
//! | the in-flight levels behind a concurrency ceiling | [`ConcurrencyView`] |
//! | which chart is a finding's evidence | [`EvidenceChart`] |

use crate::analysis::capacity::Confidence;
use crate::analysis::diagnosis::Severity;
use crate::model::{Finding, Report, Run, VerdictLevel};

/// The verdict line.
#[derive(Clone, Debug, PartialEq)]
pub struct VerdictView {
    pub level: VerdictLevel,
    /// One sentence: the top finding's cause, or where the time went.
    pub headline: String,
    pub run_id: String,
    /// The operator as given.
    pub operator: String,
    pub preset: String,
    pub window_s: f64,
    /// The run stopped early (budget, Ctrl-C).
    pub partial: Option<String>,
    /// SLA breaches (reported, never an exit code).
    pub sla_breaches: Vec<String>,
    pub warnings: Vec<String>,
}

impl VerdictView {
    pub fn of(r: &Report) -> Self {
        Self {
            level: r.verdict.level,
            headline: r.verdict.headline.clone(),
            run_id: r.run.id.clone(),
            operator: if r.spec.operator_ref.is_empty() {
                r.target.display.clone()
            } else {
                r.spec.operator_ref.clone()
            },
            preset: r.workload_label().to_owned(),
            window_s: r.run.window_s,
            partial: r
                .run
                .partial
                .as_ref()
                .map(|p| format!("{} after {:.1} s", p.reason.describe(), p.measured_s))
                .or_else(|| r.run.interrupted.then(|| "stopped early".to_owned())),
            sla_breaches: r.verdict.sla.breaches.clone(),
            warnings: r.warnings.clone(),
        }
    }
}

/// One labelled evidence value of a finding.
#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceItem {
    /// The machine name (`throttled_request_ratio`).
    pub key: String,
    /// Human label (`Throttled requests`).
    pub label: String,
    /// Formatted value with its unit (`4.7%`).
    pub value: String,
}

/// One finding: cause · evidence · remedy path · next command.
#[derive(Clone, Debug, PartialEq)]
pub struct FindingView {
    pub severity: Severity,
    pub confidence: Confidence,
    pub code: String,
    pub cause: String,
    /// The evidence in words.
    pub evidence: String,
    pub items: Vec<EvidenceItem>,
    /// The remedy's operator path (`layers.concurrent_limit.http`); `None` when the remedy
    /// is not a layer.
    pub remedy_path: Option<String>,
    /// Paths changed with it.
    pub remedy_with: Vec<String>,
    /// The OpenDAL API.
    pub remedy_opendal: String,
    /// The remedy in words (no numbers).
    pub remedy: String,
    /// `oprobe prove` sizes it.
    pub sized: bool,
    /// `oprobe prove RUN CODE` (or where to look).
    pub next: Option<String>,
    /// Operations that show the problem (their waterfalls are in `slowest` when kept).
    pub op_ids: Vec<u64>,
}

impl FindingView {
    pub fn of(f: &Finding) -> Self {
        Self {
            severity: f.severity,
            confidence: f.confidence,
            code: f.code.clone(),
            cause: f.cause.clone(),
            evidence: f.evidence.text.clone(),
            items: f
                .evidence
                .labelled()
                .into_iter()
                .zip(f.evidence.values.keys())
                .map(|((label, value), key)| EvidenceItem {
                    key: key.clone(),
                    label,
                    value,
                })
                .collect(),
            remedy_path: f.remedy.path.clone(),
            remedy_with: f.remedy.with.clone(),
            remedy_opendal: f.remedy.opendal.clone(),
            remedy: f.remedy.text.clone(),
            sized: f.remedy.sized,
            next: f.remedy.next.clone(),
            op_ids: f.evidence.op_ids.clone(),
        }
    }

    /// `layers.concurrent_limit.http + layers.retry.jitter`, or the remedy in words.
    pub fn remedy_label(&self) -> String {
        match &self.remedy_path {
            Some(p) => std::iter::once(p.as_str())
                .chain(self.remedy_with.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" + "),
            None => self.remedy.clone(),
        }
    }
}

/// The findings, most severe first (stable within a severity).
pub fn findings(r: &Report) -> Vec<FindingView> {
    let mut v: Vec<FindingView> = r.findings.iter().map(FindingView::of).collect();
    v.sort_by_key(|f| std::cmp::Reverse(f.severity));
    v
}

/// The key numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct NumbersView {
    pub goodput_ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub ops_count: u64,
    pub failed_ops_count: u64,
    pub error_ratio: f64,
    pub requests_count: u64,
    pub throttled_requests_count: u64,
    pub throttled_request_ratio: f64,
    pub requests_per_op: f64,
}

impl NumbersView {
    pub fn of(r: &Report) -> Self {
        let s = &r.summary;
        let requests = r.connections.requests;
        Self {
            goodput_ops_per_s: r.good_ops_per_s(),
            mib_per_s: s.mib_per_s,
            p50_ms: s.latency.p50,
            p99_ms: s.latency.p99,
            p999_ms: s.latency.p999,
            ops_count: s.ops,
            failed_ops_count: s.failed_ops,
            error_ratio: s.error_rate,
            requests_count: requests,
            throttled_requests_count: r.resilience.throttled_responses,
            throttled_request_ratio: crate::util::ratio(
                r.resilience.throttled_responses as f64,
                requests as f64,
            ),
            requests_per_op: r.bytes.request_amplification,
        }
    }
}

/// One stage's share of time.
#[derive(Clone, Debug, PartialEq)]
pub struct StageShare {
    /// Stable key: `queue`, `connect`, `upload`, `server`, `download`, `retry`, `cache`,
    /// `client` (all operations); `queue`, `connect`, `send`, `ttfb`, `body`, `backoff`,
    /// `client` (the slowest operations).
    pub key: String,
    /// Table title (`server (TTFB)`, `retry backoff`).
    pub title: String,
    /// Share of the time, 0–1.
    pub ratio: f64,
}

/// Where the time went: every operation, and the slowest ones by their real stage split
/// (retry backoff included, T4).
#[derive(Clone, Debug, PartialEq)]
pub struct TimeView {
    /// Every layer in the order of a request's life.
    pub stages: Vec<StageShare>,
    /// The slowest 1 % of operations (at least 5), largest share first; empty without rows.
    pub tail: Vec<StageShare>,
}

impl TimeView {
    pub fn of(r: &Report, run: Option<&Run>) -> Self {
        let sh = &r.stages.shares;
        let server = if r.target.endpoint.is_some() {
            "server (TTFB)"
        } else {
            "backend"
        };
        let s = |key: &str, title: &str, pct: f64| StageShare {
            key: key.to_owned(),
            title: title.to_owned(),
            ratio: pct / 100.0,
        };
        let stages = vec![
            s("queue", "queue", sh.queue_pct),
            s("cache", "cache", sh.cache_pct),
            s("connect", "connect", sh.connect_pct),
            s("upload", "upload", sh.upload_pct),
            s("server", server, sh.server_pct),
            s("download", "download", sh.download_pct),
            s("retry", "retry backoff", sh.retry_pct),
            s("client", "client", sh.client_pct),
        ];
        let tail = run
            .map(crate::analysis::findings::tail_split)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, x)| StageShare {
                key: k.to_owned(),
                title: match k {
                    "ttfb" => "server (TTFB)".to_owned(),
                    "backoff" => "retry backoff".to_owned(),
                    other => other.to_owned(),
                },
                ratio: x,
            })
            .collect();
        Self { stages, tail }
    }

    /// The largest share (0 when there is none).
    pub fn max_ratio(&self) -> f64 {
        self.stages.iter().map(|s| s.ratio).fold(0.0, f64::max)
    }
}

/// One request of a slowest operation, its stages in µs from the operation's start.
#[derive(Clone, Debug, PartialEq)]
pub struct WaterfallRow {
    pub api: String,
    pub status: u16,
    pub attempt: u32,
    /// Start relative to the operation's start (µs).
    pub start_us: i64,
    pub queue_us: u64,
    pub connect_us: u64,
    pub send_us: u64,
    pub ttfb_us: u64,
    pub body_us: u64,
    pub total_us: u64,
    pub reused: bool,
    pub bytes_down: u64,
    pub throttled: bool,
    pub error: Option<String>,
}

/// One of the slowest operations with its requests.
#[derive(Clone, Debug, PartialEq)]
pub struct WaterfallOp {
    pub op_id: u64,
    pub verb: String,
    pub key: String,
    /// Start in the window (µs).
    pub t_us: i64,
    pub latency_us: u64,
    pub ok: bool,
    /// Time sleeping between retries.
    pub backoff_us: u64,
    pub rows: Vec<WaterfallRow>,
}

/// The slowest operations (at most `n`), request by request.
pub fn slowest(run: &Run, n: usize) -> Vec<WaterfallOp> {
    let ids: Vec<u64> = if run.report.slowest.is_empty() {
        let mut ops: Vec<_> = run.ops.iter().collect();
        ops.sort_by_key(|o| std::cmp::Reverse(o.latency_us));
        ops.iter().map(|o| o.op_id).collect()
    } else {
        run.report.slowest.clone()
    };
    ids.into_iter()
        .filter_map(|id| run.op(id))
        .take(n)
        .map(|o| WaterfallOp {
            op_id: o.op_id,
            verb: o.verb.to_string(),
            key: o.key.clone(),
            t_us: o.t_us,
            latency_us: o.latency_us,
            ok: o.ok,
            backoff_us: o.breakdown.retry_us,
            rows: run
                .requests_of(o.op_id)
                .into_iter()
                .map(|q| WaterfallRow {
                    api: q.api.to_string(),
                    status: q.status,
                    attempt: q.attempt,
                    start_us: q.t_us - o.t_us,
                    queue_us: q.queue_us,
                    connect_us: q.connect_us(),
                    send_us: q.send_us,
                    ttfb_us: q.ttfb_us,
                    body_us: q.body_us + q.stall_us,
                    total_us: q.total_us,
                    reused: q.reused,
                    bytes_down: q.bytes_down,
                    throttled: q.is_throttle(),
                    error: q.error.clone(),
                })
                .collect(),
        })
        .collect()
}

/// The per-second series of the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineView {
    pub t_s: Vec<f64>,
    pub ops_per_s: Vec<f64>,
    pub mib_per_s: Vec<f64>,
    pub p50_ms: Vec<f64>,
    pub p99_ms: Vec<f64>,
    pub throttled_count: Vec<u64>,
    pub errors_count: Vec<u64>,
    pub inflight_peak_count: Vec<u64>,
    pub new_connections_count: Vec<u64>,
}

impl TimelineView {
    pub fn of(r: &Report) -> Self {
        let mut v = Self::default();
        for p in &r.timeline {
            v.t_s.push(p.t_s);
            v.ops_per_s.push(p.ops_per_s);
            v.mib_per_s.push(p.mib_per_s);
            v.p50_ms.push(p.p50_ms);
            v.p99_ms.push(p.p99_ms);
            v.throttled_count.push(p.throttled);
            v.errors_count.push(p.errors);
            v.inflight_peak_count.push(p.inflight_peak);
            v.new_connections_count.push(p.new_connections);
        }
        v
    }
}

/// The connections: protocol, reuse, dials (warmup included, T11) and their phases.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionView {
    /// `http/1.1 · plain`, `h2 · TLSv1.3 · …`.
    pub protocol: String,
    pub requests_count: u64,
    /// New connections in the window.
    pub dialed_count: u64,
    /// New connections during the warmup (from the kept rows).
    pub warmup_dialed_count: u64,
    pub reuse_ratio: f64,
    pub peak_inflight_count: u64,
    /// `dns 1.2ms/3.4ms · tcp …` (p50/p99).
    pub phases: String,
    pub remotes_count: u64,
}

impl ConnectionView {
    pub fn of(r: &Report, run: Option<&Run>) -> Self {
        let c = &r.connections;
        Self {
            protocol: super::connection_protocol(c),
            requests_count: c.requests,
            dialed_count: c.dialed,
            warmup_dialed_count: run.map_or(0, |run| {
                run.reqs
                    .iter()
                    .filter(|q| q.t_us < 0 && !q.reused && q.connect_us() > 0)
                    .count() as u64
            }),
            reuse_ratio: c.reuse_pct / 100.0,
            peak_inflight_count: c.peak_inflight_requests,
            phases: super::dial_phases(c),
            remotes_count: r.network.remotes.len() as u64,
        }
    }
}

/// One layer of the operator stack, innermost first.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerView {
    /// `concurrent_limit`, `retry`, `timeout`, `tail_cut`, `throttle`, `cache`.
    pub name: String,
    /// In the stack.
    pub on: bool,
    /// The builder expression (`RetryLayer::new().with_max_times(3)`) when on.
    pub rust: Option<String>,
    /// Findings whose remedy touches this layer.
    pub remedy_of: Vec<String>,
}

/// The operator: its service, layer stack (each with the findings it remedies) and the read,
/// write and HTTP options.
#[derive(Clone, Debug, PartialEq)]
pub struct OperatorView {
    pub url: String,
    pub layers: Vec<LayerView>,
    /// `read.*`, `write.*`, `http.*` values as `path = value`, plus remedies outside layers.
    pub options: Vec<(String, String)>,
    /// `path → finding codes` for every remedy path of the run.
    pub remedies: Vec<(String, Vec<String>)>,
}

impl OperatorView {
    pub fn of(r: &Report) -> Self {
        let s = &r.spec.profile.operator;
        let mut remedies: Vec<(String, Vec<String>)> = Vec::new();
        for f in &r.findings {
            for p in f.remedy.path.iter().chain(&f.remedy.with) {
                match remedies.iter_mut().find(|(k, _)| k == p) {
                    Some((_, v)) => v.push(f.code.clone()),
                    None => remedies.push((p.clone(), vec![f.code.clone()])),
                }
            }
        }
        let layers = crate::operator::diff::LAYER_ORDER
            .iter()
            .map(|name| {
                let rust = crate::operator::diff::layer_rust(s, name);
                let prefix = format!("layers.{name}");
                let mut remedy_of: Vec<String> = remedies
                    .iter()
                    .filter(|(p, _)| *p == prefix || p.starts_with(&format!("{prefix}.")))
                    .flat_map(|(_, c)| c.clone())
                    .collect();
                remedy_of.dedup();
                LayerView {
                    name: (*name).to_owned(),
                    on: rust.is_some(),
                    rust,
                    remedy_of,
                }
            })
            .collect();
        let mut options = vec![
            (
                "read.concurrent".to_owned(),
                s.read_concurrent().to_string(),
            ),
            (
                "read.chunk".to_owned(),
                s.read.chunk.map_or("auto".into(), |c| c.to_string()),
            ),
            (
                "read.gap".to_owned(),
                s.read.gap.map_or("auto".into(), |c| c.to_string()),
            ),
            (
                "write.concurrent".to_owned(),
                s.write_concurrent().to_string(),
            ),
            (
                "write.chunk".to_owned(),
                s.write.chunk.map_or("auto".into(), |c| c.to_string()),
            ),
            ("http.keep_alive".to_owned(), s.http.keep_alive.to_string()),
        ];
        options.push((
            "http.version".to_owned(),
            serde_json::to_value(s.http.version)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        ));
        Self {
            url: s.display_url(),
            layers,
            options,
            remedies,
        }
    }
}

/// Everything `show` and the app's Profile view draw for one run.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileView {
    pub verdict: VerdictView,
    pub findings: Vec<FindingView>,
    pub numbers: NumbersView,
    pub time: TimeView,
    pub slowest: Vec<WaterfallOp>,
    pub timeline: TimelineView,
    pub connections: ConnectionView,
    pub operator: OperatorView,
}

impl ProfileView {
    /// The view of a report (`run`: its trace rows, for the tail split, the waterfalls and
    /// the warmup dials).
    pub fn of(r: &Report, run: Option<&Run>) -> Self {
        Self {
            verdict: VerdictView::of(r),
            findings: findings(r),
            numbers: NumbersView::of(r),
            time: TimeView::of(r, run),
            slowest: run.map(|run| slowest(run, 5)).unwrap_or_default(),
            timeline: TimelineView::of(r),
            connections: ConnectionView::of(r, run),
            operator: OperatorView::of(r),
        }
    }
}

/// One A/B pair of a prove.
#[derive(Clone, Debug, PartialEq)]
pub struct PairView {
    /// 1-based.
    pub pair: u64,
    /// The objective of the baseline (A) and the candidate (B).
    pub baseline: f64,
    pub candidate: f64,
    /// `candidate / baseline − 1`.
    pub change_ratio: f64,
    pub baseline_throttled_ratio: f64,
    pub candidate_throttled_ratio: f64,
}

/// One sizing trial.
#[derive(Clone, Debug, PartialEq)]
pub struct SizingView {
    pub value: String,
    /// The objective at this value.
    pub objective: f64,
    pub throttled_ratio: f64,
    pub feasible: bool,
    /// The chosen knee.
    pub chosen: bool,
}

/// A prove as every front end shows it: the sizing trials, the A/B pairs, the verdict and
/// the proven diff.
#[derive(Clone, Debug, PartialEq)]
pub struct ProveView {
    pub verdict: crate::app::prove::ProveVerdict,
    pub objective: crate::app::prove::ProveObjective,
    /// `goodput` or `p99`, and the unit (`ops/s`, `ms`).
    pub objective_label: String,
    pub objective_unit: String,
    pub delta: crate::analysis::stats::Interval,
    pub noise_floor_ratio: f64,
    pub sized_path: Option<String>,
    pub sizing: Vec<SizingView>,
    pub pairs: Vec<PairView>,
    pub change: Vec<String>,
    pub rust: String,
    pub toml: String,
    pub environment: String,
    pub notes: Vec<String>,
}

impl ProveView {
    pub fn of(p: &crate::app::prove::ProveOutput) -> Self {
        use crate::app::prove::{ProveObjective, TrialKind};
        let obj = |t: &crate::app::prove::ProveTrial| match p.objective {
            ProveObjective::Goodput => t.goodput_ops_per_s,
            ProveObjective::P99 => t.p99_ms,
        };
        let chosen = p.sizing.as_ref().map(|s| s.value.clone());
        let sizing = p
            .trials
            .iter()
            .filter(|t| t.kind == TrialKind::Sizing)
            .map(|t| SizingView {
                value: t.value.clone().unwrap_or_default(),
                objective: obj(t),
                throttled_ratio: t.throttled_request_ratio,
                feasible: t.feasible,
                chosen: t.value.is_some() && t.value == chosen,
            })
            .collect();
        let mut pairs = Vec::new();
        for i in 0..p.pairs_count {
            let a = p
                .trials
                .iter()
                .find(|t| t.kind == TrialKind::Baseline && t.pair == Some(i));
            let b = p
                .trials
                .iter()
                .find(|t| t.kind == TrialKind::Candidate && t.pair == Some(i));
            if let (Some(a), Some(b)) = (a, b) {
                pairs.push(PairView {
                    pair: i + 1,
                    baseline: obj(a),
                    candidate: obj(b),
                    change_ratio: if obj(a) > 0.0 {
                        obj(b) / obj(a) - 1.0
                    } else {
                        0.0
                    },
                    baseline_throttled_ratio: a.throttled_request_ratio,
                    candidate_throttled_ratio: b.throttled_request_ratio,
                });
            }
        }
        Self {
            verdict: p.verdict,
            objective: p.objective,
            objective_label: p.objective.label().to_owned(),
            objective_unit: match p.objective {
                ProveObjective::Goodput => "ops/s".into(),
                ProveObjective::P99 => "ms".into(),
            },
            delta: p.objective_delta,
            noise_floor_ratio: p.noise_floor_ratio,
            sized_path: p.sizing.as_ref().map(|s| s.path.clone()),
            sizing,
            pairs,
            change: p.change.clone(),
            rust: p.diff.rust.clone(),
            toml: p.diff.toml.clone(),
            environment: p.environment.clone(),
            notes: p.notes.clone(),
        }
    }
}

/// One operator of a comparison.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct CompareRow {
    /// The operator as given (its label).
    pub operator: String,
    pub run_id: String,
    pub goodput_ops_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub throttled_request_ratio: f64,
    pub failed_ops_count: u64,
    pub error_ratio: f64,
    /// The top finding's code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_finding: Option<String>,
}

/// Several operators on the same workload, side by side, and how they differ.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct CompareView {
    pub rows: Vec<CompareRow>,
    /// One line per operator after the first: `service` (scheme, endpoint, bucket) and/or the
    /// layer and option paths that differ from the first operator.
    pub differences: Vec<String>,
}

impl CompareView {
    pub fn of(reports: &[&Report]) -> Self {
        let rows = reports
            .iter()
            .map(|r| {
                let n = NumbersView::of(r);
                CompareRow {
                    operator: VerdictView::of(r).operator,
                    run_id: r.run.id.clone(),
                    goodput_ops_per_s: n.goodput_ops_per_s,
                    p50_ms: n.p50_ms,
                    p99_ms: n.p99_ms,
                    throttled_request_ratio: n.throttled_request_ratio,
                    failed_ops_count: n.failed_ops_count,
                    error_ratio: n.error_ratio,
                    top_finding: r.findings.first().map(|f| f.code.clone()),
                }
            })
            .collect();
        let mut differences = Vec::new();
        if let Some(first) = reports.first() {
            let a = &first.spec.profile.operator;
            for r in &reports[1..] {
                let b = &r.spec.profile.operator;
                let d = crate::operator::diff::diff(a, b);
                let service: Vec<&str> = d
                    .changes
                    .iter()
                    .filter(|c| c.path == "url" || c.path.starts_with("options."))
                    .map(|c| c.path.as_str())
                    .collect();
                let layers: Vec<&str> = d
                    .changes
                    .iter()
                    .filter(|c| c.path != "url" && !c.path.starts_with("options."))
                    .map(|c| c.path.as_str())
                    .collect();
                let name = VerdictView::of(r).operator;
                differences.push(match (service.is_empty(), layers.is_empty()) {
                    (true, true) => format!("{name}: the same operator"),
                    (false, true) => format!("{name}: differs in service ({})", service.join(", ")),
                    (true, false) => format!("{name}: differs in layers ({})", layers.join(", ")),
                    (false, false) => format!(
                        "{name}: differs in service ({}) and layers ({})",
                        service.join(", "),
                        layers.join(", ")
                    ),
                });
            }
        }
        Self { rows, differences }
    }
}

/// The latency distribution: the share of operations at or below each latency (a CDF), from
/// the trace rows (exact) when every operation was kept, else from the heatmap's fixed log
/// buckets, which count every operation (the sampler keeps the slowest and the failed ones
/// always, so sampled rows over-weigh the tail). At most [`LatencyCdf::MAX_POINTS`] points.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LatencyCdf {
    /// `(latency ms, share 0–1)`, latency ascending, the share reaching 1.
    pub pts: Vec<(f64, f64)>,
    /// From the trace rows (otherwise from the heatmap buckets).
    pub exact: bool,
}

impl LatencyCdf {
    pub const MAX_POINTS: usize = 240;

    pub fn of(r: &Report, run: Option<&Run>) -> Self {
        // Sampled rows are not a fair sample of the latencies.
        let complete = r.sampling.kept_ratio >= 1.0 || r.sampling.seen_ops_count == 0;
        let mut lat: Vec<u64> = run
            .filter(|_| complete)
            .map(|run| {
                run.ops
                    .iter()
                    .filter(|o| o.t_us >= 0)
                    .map(|o| o.latency_us)
                    .collect()
            })
            .unwrap_or_default();
        if !lat.is_empty() {
            lat.sort_unstable();
            let n = lat.len();
            let step = n.div_ceil(Self::MAX_POINTS).max(1);
            let mut pts: Vec<(f64, f64)> = (step - 1..n)
                .step_by(step)
                .map(|i| (lat[i] as f64 / 1000.0, (i + 1) as f64 / n as f64))
                .collect();
            if pts.last().map(|p| p.1) != Some(1.0) {
                pts.push((lat[n - 1] as f64 / 1000.0, 1.0));
            }
            return Self { pts, exact: true };
        }
        let h = &r.heatmap;
        let rows = h.latency_buckets_ms.len();
        let mut per_row = vec![0u64; rows];
        for col in &h.counts {
            for (i, c) in col.iter().enumerate().take(rows) {
                per_row[i] += c;
            }
        }
        let total: u64 = per_row.iter().sum();
        if total == 0 {
            return Self::default();
        }
        let mut acc = 0u64;
        let mut pts = Vec::new();
        for (i, c) in per_row.iter().enumerate() {
            if *c == 0 {
                continue;
            }
            acc += c;
            // A bucket's operations are at or below its upper bound (the open last bucket: twice
            // its lower bound).
            let upper = h
                .latency_buckets_ms
                .get(i + 1)
                .copied()
                .unwrap_or(h.latency_buckets_ms[i] * 2.0);
            pts.push((upper, acc as f64 / total as f64));
        }
        Self { pts, exact: false }
    }

    /// The latency at quantile `q` (0–1): the first point whose share reaches it.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        self.pts.iter().find(|p| p.1 >= q).map(|p| p.0)
    }
}

/// One verb (`read.range`, `write.multipart`) with its numbers and where its time went.
#[derive(Clone, Debug, PartialEq)]
pub struct VerbView {
    pub verb: String,
    pub ops_count: u64,
    pub ops_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    pub throttled_ratio: f64,
    pub requests_per_op: f64,
    /// Shares of the verb's operation time: `queue`, `cache`, `server` (the backend),
    /// `retry`, `client`; they sum to 1 (empty when nothing was timed).
    pub stages: Vec<StageShare>,
}

/// Every verb of the run, busiest first.
pub fn verbs(r: &Report) -> Vec<VerbView> {
    let mut v: Vec<VerbView> = r
        .verbs
        .iter()
        .filter(|v| v.count > 0)
        .map(|v| {
            let b = &v.breakdown;
            let parts = [
                ("queue", "queue", b.queue_ms),
                ("cache", "cache", b.cache_ms),
                ("server", "backend", b.backend_ms),
                ("retry", "retry backoff", b.retry_ms),
                ("client", "client", b.client_ms),
            ];
            let total: f64 = parts.iter().map(|p| p.2.max(0.0)).sum();
            let stages = if total > 0.0 {
                parts
                    .iter()
                    .map(|(k, t, ms)| StageShare {
                        key: (*k).to_owned(),
                        title: (*t).to_owned(),
                        ratio: ms.max(0.0) / total,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let n = v.count as f64;
            VerbView {
                verb: v.verb.clone(),
                ops_count: v.count,
                ops_per_s: v.ops_per_s,
                p50_ms: v.latency.p50,
                p99_ms: v.latency.p99,
                error_ratio: v.errors as f64 / n,
                throttled_ratio: v.throttled as f64 / n,
                requests_per_op: v.requests_per_op,
                stages,
            }
        })
        .collect();
    v.sort_by_key(|v| std::cmp::Reverse(v.ops_count));
    v
}

/// One in-flight level: requests that arrived with this many in flight, and the share of them
/// throttled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelPoint {
    pub inflight_count: u64,
    pub requests_count: u64,
    pub throttled_ratio: f64,
}

/// The evidence of a concurrency ceiling: the share throttled at each in-flight level, and the
/// fitted ceiling with its interval (`lo ≤ limit < hi`).
#[derive(Clone, Debug, PartialEq)]
pub struct ConcurrencyView {
    pub levels: Vec<LevelPoint>,
    /// `(estimate, lo, hi)` in-flight requests, when a ceiling was fitted.
    pub ceiling: Option<(f64, f64, f64)>,
    pub confidence: Confidence,
}

impl ConcurrencyView {
    /// `None` when the run recorded no in-flight levels or nothing was throttled.
    pub fn of(r: &Report) -> Option<Self> {
        let o = &r.capacity.observed;
        if o.levels.is_empty() || o.throttled_requests_count == 0 {
            return None;
        }
        Some(Self {
            levels: o
                .levels
                .iter()
                .filter(|l| l.requests_count > 0)
                .map(|l| LevelPoint {
                    inflight_count: l.inflight_count,
                    requests_count: l.requests_count,
                    throttled_ratio: l.throttled_count as f64 / l.requests_count as f64,
                })
                .collect(),
            ceiling: r
                .capacity
                .inflight_ceiling
                .map(|c| (c.requests, c.lo, c.hi)),
            confidence: r.capacity.confidence,
        })
    }
}

/// The chart that shows a finding's evidence (the renderer draws it beside the finding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceChart {
    /// Share throttled per in-flight level, with the fitted ceiling ([`ConcurrencyView`]).
    Concurrency,
    /// Throttled responses per second over the run.
    ThrottledOverTime,
    /// Bandwidth (MiB/s) over the run.
    Bandwidth,
    /// The latency distribution with its median and tail marked ([`LatencyCdf`]).
    Latency,
    /// Failed operations per second over the run.
    ErrorsOverTime,
}

impl EvidenceChart {
    /// The evidence chart of a finding code (`None`: the evidence is in its numbers).
    pub fn of(code: &str) -> Option<Self> {
        Some(match code {
            "throttling" | "key_limit" => Self::Concurrency,
            "rate_limit" => Self::ThrottledOverTime,
            "bandwidth_cap" => Self::Bandwidth,
            "tail_stragglers" | "hung_requests" => Self::Latency,
            "errors" => Self::ErrorsOverTime,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_view_covers_every_section_of_the_fixture() {
        let run = crate::report::fixture::run();
        let r = &run.report;
        let v = ProfileView::of(r, Some(&run));
        assert_eq!(v.time.stages.len(), 8);
        let sum: f64 = v.time.stages.iter().map(|s| s.ratio).sum();
        assert!(sum > 0.5 && sum < 1.5, "{sum}");
        assert!(!v.time.tail.is_empty());
        assert_eq!(v.findings.len(), r.findings.len());
        assert_eq!(
            v.findings[0].remedy_path.as_deref(),
            Some("layers.concurrent_limit.http")
        );
        assert_eq!(v.numbers.p99_ms, r.summary.latency.p99);
        assert_eq!(v.slowest[0].op_id, 2);
        assert_eq!(v.slowest[0].rows.len(), 2);
        assert!(v.slowest[0].rows[0].throttled);
        assert_eq!(v.timeline.t_s.len(), r.timeline.len());
        assert_eq!(v.operator.layers.len(), 6);
        let cl = v
            .operator
            .layers
            .iter()
            .find(|l| l.name == "concurrent_limit")
            .unwrap();
        assert_eq!(cl.remedy_of, vec!["throttling"]);
        assert_eq!(v.connections.warmup_dialed_count, 0);
    }

    #[test]
    fn a_comparison_says_whether_the_service_or_the_layers_differ() {
        let a = crate::report::fixture::report();
        let mut b = a.clone();
        b.spec.profile.operator.url = "r2://other/prefix".into();
        let mut c = a.clone();
        c.spec
            .profile
            .operator
            .set("layers.concurrent_limit.http", "8")
            .unwrap();
        let v = CompareView::of(&[&a, &b, &c]);
        assert_eq!(v.rows.len(), 3);
        assert!(v.differences[0].contains("service"), "{:?}", v.differences);
        assert!(v.differences[1].contains("layers"), "{:?}", v.differences);
    }

    #[test]
    fn the_latency_cdf_rises_to_one_from_rows_or_buckets() {
        let run = crate::report::fixture::run();
        let exact = LatencyCdf::of(&run.report, Some(&run));
        assert!(exact.exact);
        assert_eq!(exact.pts.last().unwrap().1, 1.0);
        assert!(
            exact
                .pts
                .windows(2)
                .all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1)
        );
        let mut r = run.report.clone();
        r.heatmap.latency_buckets_ms = vec![0.0, 1.0, 10.0];
        r.heatmap.counts = vec![vec![0, 3, 1], vec![0, 0, 4]];
        let b = LatencyCdf::of(&r, None);
        assert!(!b.exact);
        // Sampled rows over-weigh the tail: the buckets, which count every operation, are used.
        let mut sampled = run.clone();
        sampled.report = r.clone();
        sampled.report.sampling.kept_ratio = 0.5;
        sampled.report.sampling.seen_ops_count = 100;
        assert_eq!(LatencyCdf::of(&sampled.report, Some(&sampled)), b);
        assert_eq!(b.pts, vec![(10.0, 3.0 / 8.0), (20.0, 1.0)]);
        assert_eq!(b.quantile(0.5), Some(20.0));
    }

    #[test]
    fn every_finding_code_with_a_chart_is_in_the_catalog() {
        for code in [
            "throttling",
            "key_limit",
            "rate_limit",
            "bandwidth_cap",
            "tail_stragglers",
            "hung_requests",
            "errors",
        ] {
            assert!(EvidenceChart::of(code).is_some(), "{code}");
            assert!(crate::analysis::catalog::get(code).is_some(), "{code}");
        }
        assert_eq!(EvidenceChart::of("small_ranges"), None);
    }
}
