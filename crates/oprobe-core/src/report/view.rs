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
//! | how the run was set up, and how far to trust it | [`SetupView`], [`TrustView`] |
//! | the backend's requests by API, retries, bytes | [`ApiRow`], [`RetryView`], [`BytesView`] |
//! | where the traffic went (keys, sizes, prefixes, remotes) | [`SpreadView`] |

use crate::analysis::capacity::Confidence;
use crate::analysis::diagnosis::Severity;
use crate::model::{Finding, Report, Run, VerdictLevel};

pub use super::range::{OpLine, RangeView, ReqLine, ops_in, slowest_requests};

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

    /// The remedy in one line, each part once: `path + with — words`, or only the words when
    /// the remedy is not a layer (`key_limit`, `client_bound`: the label already is the words).
    pub fn remedy_line(&self) -> String {
        match &self.remedy_path {
            Some(_) => format!("{} — {}", self.remedy_label(), self.remedy),
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
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
    /// Time waited before this attempt since the previous request of the operation ended
    /// (retry backoff), µs; 0 for a first attempt.
    pub wait_before_us: u64,
    /// The provider's error code (`SlowDown`, `InternalError`), when the response had one.
    pub provider_code: Option<String>,
    /// The `Retry-After` the service asked for.
    pub retry_after_ms: Option<u64>,
    /// The connection's smoothed and minimum round-trip time and retransmitted segments
    /// (kernel `TCP_INFO`), when recorded.
    pub rtt_us: Option<u64>,
    pub min_rtt_us: Option<u64>,
    pub retransmits_count: Option<u64>,
    /// The `Server-Timing` header.
    pub server_timing: Option<String>,
    pub request_id: Option<String>,
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
    examples(run, &ids, n)
}

/// The operations `ids` (a finding's examples, in its order) that have rows, at most `n`,
/// request by request.
pub fn examples(run: &Run, ids: &[u64], n: usize) -> Vec<WaterfallOp> {
    ids.iter()
        .filter_map(|id| waterfall(run, *id))
        .take(n)
        .collect()
}

/// One operation, request by request (`None` when the rows do not hold it).
pub fn waterfall(run: &Run, op_id: u64) -> Option<WaterfallOp> {
    let o = run.op(op_id)?;
    let mut prev_end: Option<i64> = None;
    let rows = run
        .requests_of(o.op_id)
        .into_iter()
        .map(|q| {
            // A retry waited from the previous attempt's end to its own start.
            let wait_before_us = match prev_end {
                Some(end) if q.attempt > 1 && q.t_us > end => (q.t_us - end) as u64,
                _ => 0,
            };
            prev_end = Some(prev_end.unwrap_or(i64::MIN).max(q.t_us + q.total_us as i64));
            let detail = q.detail.as_deref();
            WaterfallRow {
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
                wait_before_us,
                provider_code: detail.and_then(|d| d.error_code.clone()),
                retry_after_ms: q.retry_after_ms,
                rtt_us: q.tcp.as_ref().map(|t| t.rtt_us).filter(|v| *v > 0),
                min_rtt_us: q.tcp.as_ref().map(|t| t.min_rtt_us).filter(|v| *v > 0),
                retransmits_count: q.tcp.as_ref().map(|t| t.retrans),
                server_timing: detail.and_then(|d| d.server_timing.clone()),
                request_id: q.request_id.clone(),
            }
        })
        .collect();
    Some(WaterfallOp {
        op_id: o.op_id,
        verb: o.verb.to_string(),
        key: o.key.clone(),
        t_us: o.t_us,
        latency_us: o.latency_us,
        ok: o.ok,
        backoff_us: o.breakdown.retry_us,
        rows,
    })
}

/// The series of the window, one value per interval (1 s for the first ten minutes, then
/// 10 s: `len_s`).
///
/// **When things count** (as `report.timeline`): operations, goodput, bytes and the latency
/// quantiles by the interval the operation started in; `throttled_count` (throttled
/// responses), `requests_per_s`, `retries_count` and `errors_count` (failed operations) by the
/// interval they happened in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineView {
    pub t_s: Vec<f64>,
    /// Length of each interval.
    pub len_s: Vec<f64>,
    /// Operations started per second, failed ones included (the attempted rate).
    pub ops_per_s: Vec<f64>,
    /// Successful operations started per second (failed ones never count).
    pub goodput_ops_per_s: Vec<f64>,
    pub mib_per_s: Vec<f64>,
    pub p50_ms: Vec<f64>,
    pub p90_ms: Vec<f64>,
    pub p99_ms: Vec<f64>,
    pub p999_ms: Vec<f64>,
    pub max_ms: Vec<f64>,
    /// Operations started in each interval: the samples behind its quantiles
    /// ([`TimelineView::reliable`]).
    pub ops_count: Vec<u64>,
    /// Throttled responses received in each interval.
    pub throttled_count: Vec<u64>,
    /// Operations that failed in each interval.
    pub errors_count: Vec<u64>,
    /// Requests sent per second, retries included.
    pub requests_per_s: Vec<f64>,
    /// Retries issued in each interval.
    pub retries_count: Vec<u64>,
    pub inflight_peak_count: Vec<u64>,
    pub new_connections_count: Vec<u64>,
    /// The verb the series are filtered to ([`TimelineView::for_verb`]); `None`: every verb.
    pub verb: Option<String>,
}

impl TimelineView {
    pub fn of(r: &Report) -> Self {
        let mut v = Self::default();
        let n = r.timeline.len();
        for (i, p) in r.timeline.iter().enumerate() {
            let len = interval_len(p);
            let width = interval_width(r, p, i + 1 == n);
            // Reports before 6.3 carry no exact counts: derive them from the rates.
            let ops = if p.ops_count > 0 {
                p.ops_count
            } else {
                (p.ops_per_s * width).round().max(0.0) as u64
            };
            v.t_s.push(p.t_s);
            v.len_s.push(len);
            v.ops_per_s.push(p.ops_per_s);
            v.goodput_ops_per_s.push(if p.ops_count > 0 {
                p.goodput_ops_per_s
            } else {
                // Before 6.3 failures counted by operation start, like the operations.
                (p.ops_per_s - p.errors as f64 / width).max(0.0)
            });
            v.mib_per_s.push(p.mib_per_s);
            v.p50_ms.push(p.p50_ms);
            v.p90_ms.push(p.p90_ms);
            v.p99_ms.push(p.p99_ms);
            v.p999_ms.push(p.p999_ms);
            v.max_ms.push(p.max_ms);
            v.ops_count.push(ops);
            v.throttled_count.push(p.throttled);
            v.errors_count.push(p.errors);
            v.requests_per_s.push(p.requests_per_s);
            v.retries_count.push(p.retries_count);
            v.inflight_peak_count.push(p.inflight_peak);
            v.new_connections_count.push(p.new_connections);
        }
        v
    }

    pub fn len(&self) -> usize {
        self.t_s.len()
    }

    pub fn is_empty(&self) -> bool {
        self.t_s.is_empty()
    }

    /// At most `max_rows` rows: consecutive intervals merged into rows of equal count. Rates are
    /// averaged over time, counts summed, p50 averaged, and p90, p99, p99.9 and max are the
    /// worst interval's (a merged row never hides a spike). A series this view does not carry
    /// (a verb's requests) stays empty. Returns the series and how many intervals each row
    /// merges.
    pub fn downsample(&self, max_rows: usize) -> (Self, usize) {
        let n = self.len();
        let k = n.div_ceil(max_rows.max(1)).max(1);
        if k == 1 {
            return (self.clone(), 1);
        }
        let mut v = Self {
            verb: self.verb.clone(),
            ..Self::default()
        };
        let mut i = 0;
        while i < n {
            let j = (i + k).min(n);
            let r = i..j;
            let len: f64 = self.len_s[r.clone()].iter().sum();
            let tw = |x: &[f64], out: &mut Vec<f64>| {
                if x.len() == n {
                    out.push(
                        x[r.clone()]
                            .iter()
                            .zip(&self.len_s[r.clone()])
                            .map(|(a, l)| a * l)
                            .sum::<f64>()
                            / len.max(1e-9),
                    );
                }
            };
            let worst = |x: &[f64], out: &mut Vec<f64>| {
                if x.len() == n {
                    out.push(x[r.clone()].iter().copied().fold(0.0, f64::max));
                }
            };
            let sum = |x: &[u64], out: &mut Vec<u64>| {
                if x.len() == n {
                    out.push(x[r.clone()].iter().sum::<u64>());
                }
            };
            v.t_s.push(self.t_s[i]);
            v.len_s.push(len);
            tw(&self.ops_per_s, &mut v.ops_per_s);
            tw(&self.goodput_ops_per_s, &mut v.goodput_ops_per_s);
            tw(&self.mib_per_s, &mut v.mib_per_s);
            tw(&self.p50_ms, &mut v.p50_ms);
            worst(&self.p90_ms, &mut v.p90_ms);
            worst(&self.p99_ms, &mut v.p99_ms);
            worst(&self.p999_ms, &mut v.p999_ms);
            worst(&self.max_ms, &mut v.max_ms);
            sum(&self.ops_count, &mut v.ops_count);
            sum(&self.throttled_count, &mut v.throttled_count);
            sum(&self.errors_count, &mut v.errors_count);
            tw(&self.requests_per_s, &mut v.requests_per_s);
            sum(&self.retries_count, &mut v.retries_count);
            if self.inflight_peak_count.len() == n {
                v.inflight_peak_count.push(
                    self.inflight_peak_count[r.clone()]
                        .iter()
                        .copied()
                        .max()
                        .unwrap_or(0),
                );
            }
            sum(&self.new_connections_count, &mut v.new_connections_count);
            i = j;
        }
        (v, k)
    }

    /// The verbs the timeline has per-interval counters for, busiest first (empty for reports
    /// before 6.3).
    pub fn verbs(r: &Report) -> Vec<String> {
        let mut n: std::collections::BTreeMap<&str, u64> = Default::default();
        for p in &r.timeline {
            for v in &p.verbs {
                *n.entry(v.verb.as_str()).or_default() += v.ops_count;
            }
        }
        let mut v: Vec<(&str, u64)> = n.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v.into_iter().map(|(k, _)| k.to_owned()).collect()
    }

    /// The series of one verb: its operations, goodput, failures and throttled responses per
    /// interval, its p50 / p90 / p99 / p99.9 / max at the heatmap's row resolution (the upper
    /// bound of the row reaching the quantile). Requests, retries, bytes, in-flight and new
    /// connections are not split by verb: they stay empty. `None` when the timeline has no
    /// counters for `verb`.
    pub fn for_verb(r: &Report, verb: &str) -> Option<Self> {
        if !r
            .timeline
            .iter()
            .any(|p| p.verbs.iter().any(|v| v.verb == verb))
        {
            return None;
        }
        let bounds = crate::model::HEATMAP_LATENCY_BOUNDS_MS;
        let mut v = Self {
            verb: Some(verb.to_owned()),
            ..Self::default()
        };
        let n = r.timeline.len();
        for (i, p) in r.timeline.iter().enumerate() {
            let width = interval_width(r, p, i + 1 == n);
            let x = p.verbs.iter().find(|x| x.verb == verb);
            let ops = x.map_or(0, |x| x.ops_count);
            let rows = x.map(|x| x.latency_rows_count.as_slice()).unwrap_or(&[]);
            let q = |q: f64| row_quantile_ms(rows, &bounds, q);
            v.t_s.push(p.t_s);
            v.len_s.push(interval_len(p));
            v.ops_per_s.push(ops as f64 / width);
            v.goodput_ops_per_s
                .push(x.map_or(0, |x| x.ok_ops_count) as f64 / width);
            v.p50_ms.push(q(0.5));
            v.p90_ms.push(q(0.9));
            v.p99_ms.push(q(0.99));
            v.p999_ms.push(q(0.999));
            v.max_ms.push(q(1.0));
            v.ops_count.push(ops);
            v.throttled_count.push(x.map_or(0, |x| x.throttled_count));
            v.errors_count.push(x.map_or(0, |x| x.errors_count));
        }
        Some(v)
    }

    /// The fewest operations an interval needs for its quantile `q` (0–1) to be an estimate
    /// rather than its slowest few operations: `n ≥ 10 / (1 − q)` (10 for p50, 100 for p90,
    /// 1 000 for p99, 10 000 for p99.9), so at least ten operations lie above it.
    pub fn min_ops_for(q: f64) -> u64 {
        (10.0 / (1.0 - q.clamp(0.0, 0.9999))).ceil() as u64
    }

    /// Per interval: whether it has enough operations for quantile `q`
    /// ([`TimelineView::min_ops_for`]). A renderer mutes (or omits) the others.
    pub fn reliable(&self, q: f64) -> Vec<bool> {
        let min = Self::min_ops_for(q);
        self.ops_count.iter().map(|n| *n >= min).collect()
    }
}

/// The nominal length of a timeline interval (1 s for reports without one).
fn interval_len(p: &crate::trace::TimelinePoint) -> f64 {
    if p.len_s > 0.0 { p.len_s } else { 1.0 }
}

/// The time a timeline interval's rates are over: the last interval ends with the window.
fn interval_width(r: &Report, p: &crate::trace::TimelinePoint, last: bool) -> f64 {
    let len = interval_len(p);
    if last && r.run.window_s > p.t_s {
        (r.run.window_s - p.t_s).clamp(0.05, len)
    } else {
        len
    }
}

/// The latency at quantile `q` of counts per latency row (`bounds` the rows' lower bounds):
/// the upper bound of the row that reaches it (twice the lower bound for the open last row);
/// 0 without counts.
fn row_quantile_ms(rows: &[u64], bounds: &[f64], q: f64) -> f64 {
    let total: u64 = rows.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let rank = ((total as f64 * q.clamp(0.0, 1.0)).ceil() as u64).max(1);
    let mut acc = 0u64;
    for (i, c) in rows.iter().enumerate() {
        acc += c;
        if acc >= rank {
            return bounds
                .get(i + 1)
                .copied()
                .unwrap_or_else(|| bounds.get(i).map_or(0.0, |b| b * 2.0));
        }
    }
    0.0
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

/// The latency distribution: the share of operations at or below each latency (a CDF).
///
/// Drawn from, in order:
/// 1. the trace rows (exact) when they hold every operation the run measured — nothing
///    sampled away and no censored open-loop arrival the rows cannot show;
/// 2. the run's latency histogram (`report.latency_histogram`: every operation, 20 bins per
///    decade, each point at its bin's upper bound) — never biased by row sampling, which
///    keeps the slowest and failed operations always and so over-weighs the tail;
/// 3. the heatmap's 20 coarse rows (reports before 6.3).
///
/// At most [`LatencyCdf::MAX_POINTS`] points.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LatencyCdf {
    /// `(latency ms, share 0–1)`, latency ascending, the share reaching 1.
    pub pts: Vec<(f64, f64)>,
    /// From the exact trace rows (otherwise from the histogram or the heatmap's rows).
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
        // The rows win only when they are every operation of the histogram (a censored
        // arrival or a row the stream thinned is in the histogram, not in the rows).
        let hist = &r.latency_histogram;
        if !hist.is_empty() && lat.len() as u64 != hist.total() {
            lat.clear();
        }
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
        if !hist.is_empty() {
            let total = hist.total() as f64;
            let mut acc = 0u64;
            let mut pts: Vec<(f64, f64)> = hist
                .upper_ms
                .iter()
                .zip(&hist.counts)
                .map(|(upper, c)| {
                    acc += c;
                    (*upper, acc as f64 / total)
                })
                .collect();
            // The last bin ends at the slowest operation.
            if let Some(last) = pts.last_mut() {
                last.0 = last
                    .0
                    .min(hist.max_ms.max(0.0))
                    .max(hist.lower_ms.last().copied().unwrap_or(0.0));
                last.1 = 1.0;
            }
            if pts.len() > Self::MAX_POINTS {
                let step = pts.len().div_ceil(Self::MAX_POINTS);
                let last = *pts.last().expect("non-empty");
                pts = pts.into_iter().skip(step - 1).step_by(step).collect();
                if pts.last() != Some(&last) {
                    pts.push(last);
                }
            }
            return Self { pts, exact: false };
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

    /// The distribution of one verb, from the timeline's per-verb latency rows (every
    /// operation of the verb, at the heatmap's row resolution: each point at its row's upper
    /// bound). `None` when the timeline has no rows for `verb`.
    pub fn for_verb(r: &Report, verb: &str) -> Option<Self> {
        let bounds = crate::model::HEATMAP_LATENCY_BOUNDS_MS;
        let mut rows = vec![0u64; bounds.len()];
        for p in &r.timeline {
            for v in p.verbs.iter().filter(|v| v.verb == verb) {
                for (i, c) in v.latency_rows_count.iter().enumerate().take(rows.len()) {
                    rows[i] += c;
                }
            }
        }
        let total: u64 = rows.iter().sum();
        if total == 0 {
            return None;
        }
        let mut acc = 0u64;
        let pts = rows
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .map(|(i, c)| {
                acc += c;
                let upper = bounds.get(i + 1).copied().unwrap_or(bounds[i] * 2.0);
                (upper, acc as f64 / total as f64)
            })
            .collect();
        Some(Self { pts, exact: false })
    }
}

/// Operation latency over time for drawing: `counts[t][l]` operations started in time column
/// `t` with a latency in row `l`.
///
/// When the trace rows hold every operation of the window, the rows are fitted to the data in
/// 1-1.5-2-3-5-7 steps per decade ([`crate::analysis::segments::fine_heatmap`]: 6.6 ms and
/// 9.9 ms fall in different rows); otherwise (sampled rows over-weigh the tail) it is the
/// report's heatmap, 20 fixed rows in 1-2-5 steps that count every operation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeatmapView {
    /// Start of each time column, seconds from the window start.
    pub t_buckets_s: Vec<f64>,
    pub bucket_width_s: f64,
    /// Lower bound of each latency row (ms); the last row is open-ended.
    pub latency_buckets_ms: Vec<f64>,
    pub counts: Vec<Vec<u64>>,
    /// The fine rows fitted to the data (otherwise the report's fixed rows).
    pub fine: bool,
}

impl HeatmapView {
    /// Time columns of the fine heatmap at most.
    pub const MAX_COLUMNS: usize = 120;

    pub fn of(r: &Report, run: Option<&Run>) -> Self {
        let complete = r.sampling.kept_ratio >= 1.0 || r.sampling.seen_ops_count == 0;
        if let Some(run) = run.filter(|_| complete)
            && run.ops.iter().any(|o| o.t_us >= 0)
        {
            let h = crate::analysis::segments::fine_heatmap(
                &run.ops,
                r.run.window_s,
                Self::MAX_COLUMNS,
            );
            return Self {
                t_buckets_s: h.t_buckets_s,
                bucket_width_s: h.bucket_width_s,
                latency_buckets_ms: h.latency_buckets_ms,
                counts: h.counts,
                fine: true,
            };
        }
        let h = &r.heatmap;
        Self {
            t_buckets_s: h.t_buckets_s.clone(),
            bucket_width_s: h.bucket_width_s,
            latency_buckets_ms: h.latency_buckets_ms.clone(),
            counts: h.counts.clone(),
            fine: false,
        }
    }

    /// The largest cell (the colour scale's top; 0 when empty).
    pub fn max_count(&self) -> u64 {
        self.counts.iter().flatten().copied().max().unwrap_or(0)
    }
}

/// One verb (`read.range`, `write.multipart`) with its numbers and where its time went.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
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
    /// Throttled responses per second over the run: each 429/503 in the interval its
    /// response arrived (`TimelineView::throttled_count` over `len_s`), retries included —
    /// a rate limit shows as a plateau where it bit, not smeared back to the operations'
    /// starts.
    ThrottledOverTime,
    /// Bandwidth (MiB/s) over the run.
    Bandwidth,
    /// The latency distribution with its median and tail marked ([`LatencyCdf`]).
    Latency,
    /// Failed operations per second over the run (each in the interval it failed).
    ErrorsOverTime,
    /// The hottest keys with their share of the traffic and of the throttling
    /// ([`SpreadView::hot_keys`]).
    HotKeys,
    /// Bytes asked for against bytes moved, and requests per operation ([`BytesView`]).
    Amplification,
}

impl EvidenceChart {
    /// The evidence chart of a finding code (`None`: the evidence is in its numbers).
    pub fn of(code: &str) -> Option<Self> {
        Some(match code {
            "throttling" => Self::Concurrency,
            "key_limit" | "repeated_reads" => Self::HotKeys,
            "small_ranges" => Self::Amplification,
            "rate_limit" => Self::ThrottledOverTime,
            "bandwidth_cap" => Self::Bandwidth,
            "tail_stragglers" | "hung_requests" => Self::Latency,
            "errors" => Self::ErrorsOverTime,
            _ => return None,
        })
    }
}

/// How the run was set up: enough to read its numbers and to run it again.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupView {
    /// The target as shown (`lab://throttle`, `s3://bucket/prefix`), and its endpoint.
    pub target: String,
    pub endpoint: Option<String>,
    pub preset: String,
    pub concurrency_count: u64,
    pub duration_s: f64,
    pub warmup_s: f64,
    /// Operations per second asked for (open loop), else closed loop.
    pub rate_per_s: Option<f64>,
    pub objects_count: u64,
    pub object_size_bytes: u64,
    /// The workload's mix: `(weight, operation)`.
    pub mix: Vec<(u32, String)>,
    pub seed: u64,
    pub started_at: String,
    pub host: String,
    pub oprobe_version: String,
    /// An equivalent `oprobe run …` command.
    pub command: String,
}

impl SetupView {
    pub fn of(r: &Report) -> Self {
        let w = &r.workload;
        let h = &r.run.host;
        Self {
            target: r.target.display.clone(),
            endpoint: r.target.endpoint.clone(),
            preset: w.preset.clone(),
            concurrency_count: r.spec.profile.workload.concurrency as u64,
            duration_s: w.duration_s,
            warmup_s: w.warmup_s,
            rate_per_s: w.rate_per_s,
            objects_count: w.dataset.objects_count,
            object_size_bytes: w.dataset.object_size_bytes,
            mix: w.mix.iter().map(|m| (m.weight, m.op.clone())).collect(),
            seed: w.seed,
            started_at: r.run.started_at.clone(),
            host: format!("{} {} · {} CPUs", h.os, h.arch, h.cpus_count),
            oprobe_version: r.run.oprobe_version.clone(),
            command: r.spec.command(),
        }
    }
}

/// How far to trust the run's numbers: how much was measured, whether every operation was
/// kept, and whether the client itself kept up.
#[derive(Clone, Debug, PartialEq)]
pub struct TrustView {
    pub ops_count: u64,
    pub requests_count: u64,
    pub window_s: f64,
    /// Kept / seen operations in the trace rows (1: none sampled away).
    pub kept_ratio: f64,
    /// The client's CPU over the cores in use, and its scheduling lag p99.
    pub cpu_ratio: Option<f64>,
    pub sched_lag_p99_ms: Option<f64>,
    /// The client could not keep up: the server/TTFB split is unreliable.
    pub client_saturated: bool,
    /// Arrivals of an open-loop run that could not start on time.
    pub missed_arrivals_ratio: Option<f64>,
    /// Why the run is partial (interrupted, a budget), if it is.
    pub partial: Option<String>,
}

impl TrustView {
    pub fn of(r: &Report) -> Self {
        let sf = &r.self_stats;
        Self {
            ops_count: r.summary.ops,
            requests_count: r.connections.requests,
            window_s: r.run.window_s,
            kept_ratio: if r.sampling.seen_ops_count == 0 {
                1.0
            } else {
                r.sampling.kept_ratio
            },
            cpu_ratio: sf.cpu_ratio,
            sched_lag_p99_ms: sf.sched_lag_p99_ms,
            client_saturated: sf.client_saturated,
            missed_arrivals_ratio: r
                .schedule
                .open_loop
                .then_some(r.schedule.missed_arrivals_ratio),
            partial: r
                .run
                .partial
                .as_ref()
                .map(|p| format!("{:?}", p.reason).to_lowercase())
                .or_else(|| r.run.interrupted.then(|| "interrupted".to_owned())),
        }
    }

    /// Something makes the numbers less certain (shown up front, not only in the details).
    pub fn concerns(&self) -> bool {
        self.client_saturated
            || self.partial.is_some()
            || self.missed_arrivals_ratio.is_some_and(|m| m > 0.01)
            || self.ops_count < 100
    }
}

/// The backend's requests of one API (`GetObject`, `PutObject`, …).
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct ApiRow {
    pub api: String,
    pub requests_count: u64,
    pub errors_count: u64,
    pub throttled_count: u64,
    /// The status codes answered, most frequent first (`0`: no response).
    pub status: Vec<(u16, u64)>,
    pub reused_ratio: f64,
    pub up_bytes: u64,
    pub down_bytes: u64,
    pub ttfb_p50_ms: f64,
    pub ttfb_p99_ms: f64,
    pub total_p50_ms: f64,
    pub total_p99_ms: f64,
}

/// Every API the run called, busiest first.
pub fn apis(r: &Report) -> Vec<ApiRow> {
    let mut v: Vec<ApiRow> = r
        .apis
        .iter()
        .filter(|a| a.count > 0)
        .map(|a| {
            let mut status: Vec<(u16, u64)> = a.status.iter().map(|(k, v)| (*k, *v)).collect();
            status.sort_by_key(|s| std::cmp::Reverse(s.1));
            ApiRow {
                api: a.api.label().to_owned(),
                requests_count: a.count,
                errors_count: a.errors,
                throttled_count: a.throttled,
                status,
                reused_ratio: a.reused_pct / 100.0,
                up_bytes: a.bytes_up,
                down_bytes: a.bytes_down,
                ttfb_p50_ms: a.stages.ttfb.p50,
                ttfb_p99_ms: a.stages.ttfb.p99,
                total_p50_ms: a.stages.total.p50,
                total_p99_ms: a.stages.total.p99,
            }
        })
        .collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.requests_count));
    v
}

/// Retries and failures.
#[derive(Clone, Debug, PartialEq)]
pub struct RetryView {
    pub retries_count: u64,
    /// Time operations spent waiting between attempts, in total.
    pub backoff_ms: f64,
    pub status_429_count: u64,
    pub status_503_count: u64,
    /// The longest `Retry-After` the service asked for.
    pub max_retry_after_ms: u64,
    pub server_errors_count: u64,
    pub client_errors_count: u64,
    pub network_errors_count: u64,
    pub failed_ops_count: u64,
    /// Failure classes, most frequent first.
    pub classes: Vec<(String, u64)>,
}

impl RetryView {
    pub fn of(r: &Report) -> Self {
        let x = &r.resilience;
        let mut classes: Vec<(String, u64)> = x
            .error_classes
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        classes.sort_by_key(|c| std::cmp::Reverse(c.1));
        Self {
            retries_count: x.retries,
            backoff_ms: x.retry_backoff_ms,
            status_429_count: x.status_429,
            status_503_count: x.status_503,
            max_retry_after_ms: x.max_retry_after_ms,
            server_errors_count: x.server_errors,
            client_errors_count: x.client_errors,
            network_errors_count: x.network_errors,
            failed_ops_count: x.failed_ops,
            classes,
        }
    }

    /// Nothing was retried and nothing failed.
    pub fn quiet(&self) -> bool {
        self.retries_count == 0
            && self.failed_ops_count == 0
            && self.status_429_count == 0
            && self.status_503_count == 0
    }
}

/// Bytes asked for against bytes moved.
#[derive(Clone, Debug, PartialEq)]
pub struct BytesView {
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub cache_hit_bytes: u64,
    pub backend_down_bytes: u64,
    pub backend_up_bytes: u64,
    pub wire_rx_bytes: u64,
    pub wire_tx_bytes: u64,
    /// Downloaded / asked for (over-fetching raises it, cache hits lower it).
    pub read_amplification_ratio: f64,
    /// Downloaded / asked for past the cache.
    pub overread_ratio: f64,
    /// Uploaded / written (retried uploads raise it).
    pub write_amplification_ratio: f64,
    pub requests_per_op: f64,
}

impl BytesView {
    pub fn of(r: &Report) -> Self {
        let b = &r.bytes;
        Self {
            logical_read_bytes: b.logical_read,
            logical_write_bytes: b.logical_write,
            cache_hit_bytes: b.cache_hit_bytes,
            backend_down_bytes: b.backend_down,
            backend_up_bytes: b.backend_up,
            wire_rx_bytes: b.wire_rx,
            wire_tx_bytes: b.wire_tx,
            read_amplification_ratio: b.read_amplification,
            overread_ratio: b.overread_ratio,
            write_amplification_ratio: b.write_amplification,
            requests_per_op: b.request_amplification,
        }
    }
}

/// One slice of the traffic (a key, a size bucket, a prefix, a remote).
#[derive(Clone, Debug, PartialEq)]
pub struct Slice {
    pub key: String,
    pub requests_count: u64,
    /// Share of all requests.
    pub share_ratio: f64,
    /// Share of the slice's own requests that were throttled (prefixes, sizes); `None` for
    /// hot keys, which carry `throttle_share_ratio` instead.
    pub throttled_ratio: Option<f64>,
    /// Share of **all** throttled responses that went to this slice (hot keys: how much of
    /// the throttling one key draws); `None` for the other slices.
    pub throttle_share_ratio: Option<f64>,
    pub error_ratio: Option<f64>,
    pub p99_ms: Option<f64>,
}

/// Where the traffic went: the hottest keys, object sizes, prefixes and the server addresses.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SpreadView {
    pub hot_keys: Vec<Slice>,
    pub sizes: Vec<Slice>,
    pub prefixes: Vec<Slice>,
    pub remotes: Vec<Slice>,
}

impl SpreadView {
    pub const TOP: usize = 8;

    pub fn of(r: &Report) -> Self {
        let d = &r.dimensions;
        let slice = |x: &crate::model::DimensionSlice| Slice {
            key: x.key.clone(),
            requests_count: x.requests_count,
            share_ratio: x.share_ratio,
            throttled_ratio: Some(x.throttled_ratio),
            throttle_share_ratio: None,
            error_ratio: Some(x.error_ratio),
            p99_ms: Some(x.p99_ms),
        };
        let total: u64 = r.network.remotes.iter().map(|x| x.requests_count).sum();
        Self {
            hot_keys: d
                .hot_keys
                .iter()
                .take(Self::TOP)
                .map(|k| Slice {
                    key: k.key.clone(),
                    requests_count: k.requests_count,
                    share_ratio: k.share_ratio,
                    throttled_ratio: None,
                    throttle_share_ratio: Some(k.throttle_share_ratio),
                    error_ratio: None,
                    p99_ms: None,
                })
                .collect(),
            sizes: d.size_buckets.iter().take(Self::TOP).map(slice).collect(),
            prefixes: d.prefixes.iter().take(Self::TOP).map(slice).collect(),
            remotes: r
                .network
                .remotes
                .iter()
                .take(Self::TOP)
                .map(|x| Slice {
                    key: x.remote.clone(),
                    requests_count: x.requests_count,
                    share_ratio: crate::util::ratio(x.requests_count as f64, total as f64),
                    throttled_ratio: None,
                    throttle_share_ratio: None,
                    error_ratio: Some(crate::util::ratio(
                        x.errors_count as f64,
                        x.requests_count as f64,
                    )),
                    p99_ms: Some(x.p99_ms),
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hot_keys.is_empty()
            && self.sizes.is_empty()
            && self.prefixes.is_empty()
            && self.remotes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_timeline_is_downsampled_without_hiding_spikes() {
        let n = 900;
        let t = TimelineView {
            t_s: (0..n).map(|i| i as f64).collect(),
            len_s: vec![1.0; n],
            ops_per_s: vec![100.0; n],
            mib_per_s: vec![1.0; n],
            p50_ms: vec![5.0; n],
            p99_ms: (0..n).map(|i| if i == 451 { 900.0 } else { 9.0 }).collect(),
            p999_ms: vec![10.0; n],
            max_ms: vec![12.0; n],
            throttled_count: vec![1; n],
            errors_count: vec![0; n],
            inflight_peak_count: vec![4; n],
            new_connections_count: vec![0; n],
            ..Default::default()
        };
        let (d, k) = t.downsample(60);
        assert_eq!(k, 15);
        assert_eq!(d.len(), 60);
        assert_eq!(d.len_s[0], 15.0);
        assert_eq!(d.t_s[1], 15.0);
        assert!((d.ops_per_s[0] - 100.0).abs() < 1e-9);
        assert_eq!(d.throttled_count[0], 15);
        // The 900 ms second survives the merge.
        assert_eq!(d.p99_ms[30], 900.0);
        let (same, k) = t.downsample(1000);
        assert_eq!((same.len(), k), (n, 1));
    }

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

    /// A log-normal-ish known distribution: 1 000 operations, 1 % at 100 ms.
    fn with_histogram(mut r: Report) -> Report {
        let lat = |i: u64| -> u64 {
            match i {
                0..500 => 2_000,
                500..900 => 5_000,
                900..990 => 20_000,
                _ => 100_000,
            }
        };
        r.latency_histogram =
            crate::trace::LatencyHistogram::from_counts((0..1000).map(|i| (lat(i), 1)));
        r
    }

    #[test]
    fn the_latency_cdf_comes_from_the_histogram_when_rows_are_sampled() {
        let run = crate::report::fixture::run();
        let mut sampled = run.clone();
        sampled.report = with_histogram(run.report.clone());
        sampled.report.sampling.kept_ratio = 0.1;
        sampled.report.sampling.seen_ops_count = 1000;
        let cdf = LatencyCdf::of(&sampled.report, Some(&sampled));
        assert!(!cdf.exact);
        assert_eq!(cdf.pts.len(), 4, "{:?}", cdf.pts);
        // Each point is its bin's upper bound with the exact share at or below it.
        let at = |ms: f64| cdf.pts.iter().find(|p| p.0 >= ms).unwrap().1;
        assert_eq!(at(2.0), 0.5);
        assert_eq!(at(5.0), 0.9);
        assert_eq!(at(20.0), 0.99);
        assert_eq!(cdf.pts.last().unwrap(), &(100.0, 1.0));
        // Bins are 12 % wide: the quantiles are within a bin of the truth.
        let p50 = cdf.quantile(0.5).unwrap();
        assert!((2.0..2.0 * 1.13).contains(&p50), "{p50}");
        assert_eq!(cdf.quantile(0.995), Some(100.0));
        // Rows that are every operation of the histogram stay exact; fewer rows do not.
        let mut complete = run.clone();
        complete.report = with_histogram(run.report.clone());
        assert!(!LatencyCdf::of(&complete.report, Some(&complete)).exact);
        let n = run.ops.iter().filter(|o| o.t_us >= 0).count() as u64;
        complete.report.latency_histogram =
            crate::trace::LatencyHistogram::from_counts([(3_000, n)]);
        assert!(LatencyCdf::of(&complete.report, Some(&complete)).exact);
    }

    #[test]
    fn the_timeline_view_counts_goodput_requests_and_reliable_quantiles() {
        let mut r = crate::report::fixture::report();
        r.run.window_s = 3.0;
        let p = |ops: u64, ok: u64| crate::trace::TimelinePoint {
            len_s: 1.0,
            ops_per_s: ops as f64,
            ops_count: ops,
            ok_ops_count: ok,
            goodput_ops_per_s: ok as f64,
            requests_per_s: 2.0 * ops as f64,
            requests_count: 2 * ops,
            retries_count: ops - ok,
            ..Default::default()
        };
        r.timeline = vec![p(5, 5), p(200, 150), p(2000, 2000)];
        for (i, t) in r.timeline.iter_mut().enumerate() {
            t.t_s = i as f64;
            t.verbs = vec![crate::trace::VerbInterval {
                verb: "read".into(),
                ops_count: t.ops_count,
                ok_ops_count: t.ok_ops_count,
                errors_count: t.ops_count - t.ok_ops_count,
                latency_rows_count: {
                    // Every read in the 10–20 ms row.
                    let mut rows = vec![0u64; 20];
                    rows[7] = t.ops_count;
                    rows
                },
                ..Default::default()
            }];
        }
        let v = TimelineView::of(&r);
        assert_eq!(v.goodput_ops_per_s, vec![5.0, 150.0, 2000.0]);
        assert_eq!(v.ops_per_s, vec![5.0, 200.0, 2000.0]);
        assert_eq!(v.requests_per_s, vec![10.0, 400.0, 4000.0]);
        assert_eq!(v.retries_count, vec![0, 50, 0]);
        assert_eq!(TimelineView::min_ops_for(0.5), 20);
        assert_eq!(TimelineView::min_ops_for(0.99), 1000);
        assert_eq!(v.reliable(0.5), vec![false, true, true]);
        assert_eq!(v.reliable(0.99), vec![false, false, true]);
        // One verb's series, quantiles at row resolution.
        assert_eq!(TimelineView::verbs(&r), vec!["read".to_owned()]);
        let read = TimelineView::for_verb(&r, "read").unwrap();
        assert_eq!(read.verb.as_deref(), Some("read"));
        assert_eq!(read.goodput_ops_per_s, vec![5.0, 150.0, 2000.0]);
        assert_eq!(read.errors_count, vec![0, 50, 0]);
        assert_eq!(read.p99_ms, vec![20.0, 20.0, 20.0]);
        assert!(TimelineView::for_verb(&r, "stat").is_none());
        // Reports before 6.3: goodput from the rate less the failures (both by start).
        let mut old = r.clone();
        old.timeline = vec![crate::trace::TimelinePoint {
            len_s: 1.0,
            ops_per_s: 10.0,
            errors: 4,
            ..Default::default()
        }];
        let v = TimelineView::of(&old);
        assert_eq!(v.ops_count, vec![10]);
        assert_eq!(v.goodput_ops_per_s, vec![6.0]);
    }

    #[test]
    fn the_heatmap_view_uses_fine_rows_only_for_complete_rows() {
        let run = crate::report::fixture::run();
        let fine = HeatmapView::of(&run.report, Some(&run));
        assert!(fine.fine);
        let kept: u64 = fine.counts.iter().flatten().sum();
        assert_eq!(kept, run.ops.iter().filter(|o| o.t_us >= 0).count() as u64);
        // Rows in 1-1.5-2-3-5-7 steps.
        let steps: Vec<f64> = fine.latency_buckets_ms[1..]
            .windows(2)
            .map(|w| w[1] / w[0])
            .collect();
        assert!(
            steps.iter().all(|s| *s <= 1.7),
            "{:?}",
            fine.latency_buckets_ms
        );
        let mut sampled = run.clone();
        sampled.report.sampling.kept_ratio = 0.5;
        sampled.report.sampling.seen_ops_count = 100;
        let coarse = HeatmapView::of(&sampled.report, Some(&sampled));
        assert!(!coarse.fine);
        assert_eq!(coarse.counts, run.report.heatmap.counts);
    }

    #[test]
    fn a_remedy_that_is_not_a_layer_is_said_once() {
        let f = crate::model::Finding {
            code: "key_limit".into(),
            remedy: crate::analysis::findings::remedy("key_limit", None),
            ..Default::default()
        };
        let v = FindingView::of(&f);
        assert_eq!(v.remedy_line(), v.remedy);
        let f = crate::model::Finding {
            code: "throttling".into(),
            remedy: crate::analysis::findings::remedy("throttling", None),
            ..Default::default()
        };
        let v = FindingView::of(&f);
        assert!(
            v.remedy_line()
                .starts_with("layers.concurrent_limit.http + layers.retry.jitter — cap"),
            "{}",
            v.remedy_line()
        );
    }

    #[test]
    fn hot_keys_carry_their_share_of_the_throttling_not_a_throttled_ratio() {
        let mut r = crate::report::fixture::report();
        r.dimensions.hot_keys = vec![crate::model::HotKey {
            key: "k".into(),
            requests_count: 10,
            share_ratio: 0.5,
            throttle_share_ratio: 0.8,
        }];
        let sp = SpreadView::of(&r);
        assert_eq!(sp.hot_keys[0].throttle_share_ratio, Some(0.8));
        assert_eq!(sp.hot_keys[0].throttled_ratio, None);
    }

    #[test]
    fn every_finding_code_with_a_chart_is_in_the_catalog() {
        for code in [
            "throttling",
            "key_limit",
            "repeated_reads",
            "small_ranges",
            "rate_limit",
            "bandwidth_cap",
            "tail_stragglers",
            "hung_requests",
            "errors",
        ] {
            assert!(EvidenceChart::of(code).is_some(), "{code}");
            assert!(crate::analysis::catalog::get(code).is_some(), "{code}");
        }
        assert_eq!(EvidenceChart::of("cold_connections"), None);
    }

    #[test]
    fn the_details_read_the_fixture() {
        let run = crate::report::fixture::run();
        let r = &run.report;
        let setup = SetupView::of(r);
        assert!(setup.command.starts_with("oprobe run"), "{}", setup.command);
        assert_eq!(setup.preset, r.workload.preset);
        let trust = TrustView::of(r);
        assert_eq!(trust.ops_count, r.summary.ops);
        assert!(trust.kept_ratio > 0.0 && trust.kept_ratio <= 1.0);
        let a = apis(r);
        assert!(
            a.windows(2)
                .all(|w| w[0].requests_count >= w[1].requests_count)
        );
        assert_eq!(
            a.iter().map(|x| x.requests_count).sum::<u64>(),
            r.apis.iter().map(|x| x.count).sum::<u64>()
        );
        let retry = RetryView::of(r);
        assert_eq!(retry.retries_count, r.resilience.retries);
        let _ = BytesView::of(r);
        let sp = SpreadView::of(r);
        assert!(sp.hot_keys.len() <= SpreadView::TOP);
    }
}
