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
//! | slowest operations, request by request (with what each request recorded) | [`WaterfallOp`] / [`WaterfallRow`] / [`RequestSignal`] |
//! | timeline series | [`TimelineView`] |
//! | connections: dials, the kernel's TCP view, TLS handshakes, the cold start | [`ConnectionView`] |
//! | the write path: the part size OpenDAL used, the parts uploaded | [`WritePathView`] |
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
                .map(|p| format!("{} after {:.1} s", p.reason.describe(), p.measured_s)),
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
    /// Formatted value with its unit (`4.7%`), in the terminal's format.
    pub value: String,
    /// The number, in [`EvidenceItem::unit`] (a share is 0–1, also for `_pct` keys), for a
    /// front end that formats numbers its own way.
    pub amount: f64,
    pub unit: EvidenceUnit,
}

/// The unit of an evidence value, from its key's suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceUnit {
    /// Milliseconds (`_ms`, and `_us` converted).
    Ms,
    /// Seconds (`_s`).
    Seconds,
    /// A share, 0–1 (`_ratio`, and `_pct` divided by 100).
    Share,
    /// A multiple (`p99_to_p50_ratio`, `overread_ratio`): `3.20×`.
    Times,
    Bytes,
    BytesPerS,
    MibPerS,
    RequestsPerS,
    OpsPerS,
    /// A whole count (`_count`, and counts per 1 000 operations).
    Count,
    Usd,
    /// A plain number (`_mean`).
    Number,
}

impl EvidenceUnit {
    /// The unit of `key` and its value converted to that unit.
    pub fn of(key: &str, v: f64) -> (Self, f64) {
        let multiple = key.contains("_to_") || key == "overread_ratio";
        if key.ends_with("_ratio") {
            if multiple {
                (Self::Times, v)
            } else {
                (Self::Share, v)
            }
        } else if key.ends_with("_pct") {
            (Self::Share, v / 100.0)
        } else if key.ends_with("_ms") {
            (Self::Ms, v)
        } else if key.ends_with("_us") {
            (Self::Ms, v / 1000.0)
        } else if key.ends_with("_per_s") {
            (
                if key.contains("bytes") {
                    Self::BytesPerS
                } else if key.contains("mib") {
                    Self::MibPerS
                } else if key.contains("requests") {
                    Self::RequestsPerS
                } else {
                    Self::OpsPerS
                },
                v,
            )
        } else if key.ends_with("_s") {
            (Self::Seconds, v)
        } else if key.ends_with("_bytes") {
            (Self::Bytes, v)
        } else if key.ends_with("_usd") {
            (Self::Usd, v)
        } else if key.ends_with("_count") || key.contains("_per_1k_") {
            (Self::Count, v)
        } else {
            (Self::Number, v)
        }
    }
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
                .zip(f.evidence.values.values())
                .map(|(((label, value), key), v)| {
                    let (unit, amount) = EvidenceUnit::of(key, *v);
                    EvidenceItem {
                        key: key.clone(),
                        label,
                        value,
                        amount,
                        unit,
                    }
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

    /// What a finding's card says under its title, given the verdict's headline (9.1 review §2
    /// item 1): the cause, unless the headline already says it; the evidence in words only
    /// when there are no named values (the values' table holds the numbers).
    pub fn card(&self, headline: &str) -> FindingCard {
        let first = |t: &str| {
            t.split(". ")
                .next()
                .unwrap_or(t)
                .trim()
                .trim_end_matches('.')
                .to_owned()
        };
        let cause = first(&self.cause);
        let repeated = !cause.is_empty() && first(headline) == cause;
        FindingCard {
            cause: (!repeated && !self.cause.is_empty()).then(|| self.cause.clone()),
            evidence: (self.items.is_empty() && !self.evidence.is_empty())
                .then(|| self.evidence.clone()),
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

/// The text of a finding's card ([`FindingView::card`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FindingCard {
    pub cause: Option<String>,
    pub evidence: Option<String>,
}

/// The findings in the report's order (already ranked: `invalid_run` leads a run that cannot
/// be judged, then the most severe), so the text and JSON agree.
pub fn findings(r: &Report) -> Vec<FindingView> {
    r.findings.iter().map(FindingView::of).collect()
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
    /// The response body transfer, without the consumer's stall (`stall_us`).
    pub body_us: u64,
    /// The response body waited for the consumer to read it (client time, its own segment).
    pub stall_us: u64,
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
    /// What the response and the connection said, in the order a front end lists them
    /// ([`request_signals`]).
    pub signals: Vec<RequestSignal>,
}

/// The value of a [`RequestSignal`]: a front end formats it.
#[derive(Clone, Debug, PartialEq)]
pub enum SignalValue {
    /// A duration in µs.
    Us(u64),
    /// A duration in ms.
    Ms(u64),
    Count(u64),
    Text(String),
}

/// One thing a request row recorded beyond its stage timings: the provider's error code,
/// `Retry-After`, the connection's round trip and retransmits, `Server-Timing`, the HTTP/2
/// open streams, the kernel's upload acknowledgement and buffered send, the CDN cache status
/// and the bucket's region.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestSignal {
    /// Stable key: `code`, `retry_after`, `rtt`, `min_rtt`, `retransmits`, `server_timing`,
    /// `streams`, `upload_acked`, `send_buffered`, `cache_status`, `region`.
    pub key: &'static str,
    pub value: SignalValue,
}

impl RequestSignal {
    /// Signals that explain a slow request on their own (shown in the compact terminal
    /// report); the others (round trip, kernel timings, `Server-Timing`, region) only with
    /// `--detail`.
    pub fn explains(&self) -> bool {
        matches!(
            self.key,
            "code" | "retry_after" | "tail_cut" | "aborted" | "cache_status"
        )
    }

    /// `upload acked 3.1ms`: the signal in the terminal's words.
    pub fn text(&self) -> String {
        let label = match self.key {
            "retry_after" => "retry-after",
            "tail_cut" => "tail-cut",
            "aborted" => "body",
            "min_rtt" => "min rtt",
            "server_timing" => "server-timing",
            "streams" => "h2 streams",
            "upload_acked" => "upload acked",
            "send_buffered" => "send buffered",
            "cache_status" => "cache",
            k => k,
        };
        let value = match &self.value {
            SignalValue::Us(us) => crate::util::fmt_ms(*us as f64 / 1000.0),
            SignalValue::Ms(ms) => crate::util::fmt_ms(*ms as f64),
            SignalValue::Count(n) => n.to_string(),
            SignalValue::Text(t) => t.clone(),
        };
        format!("{label} {value}")
    }
}

/// What one request row recorded beyond its stage timings, in a fixed order (only those the
/// row has; the request id is not one: front ends show it on its own). The edge PoP, cache
/// age, storage class and encryption headers stay in the rows and the JSON, not here.
pub fn request_signals(q: &crate::model::ReqRow) -> Vec<RequestSignal> {
    let d = q.detail.as_deref();
    let tcp = q.tcp.as_deref();
    let nonzero = |v: Option<u64>| v.filter(|v| *v > 0);
    let all: [(&'static str, Option<SignalValue>); 13] = [
        (
            "code",
            d.and_then(|d| d.error_code.clone()).map(SignalValue::Text),
        ),
        ("retry_after", q.retry_after_ms.map(SignalValue::Ms)),
        (
            "tail_cut",
            d.filter(|d| d.cut)
                .map(|_| SignalValue::Text("hedged".into())),
        ),
        (
            "aborted",
            d.filter(|d| d.aborted && !d.cut)
                .map(|_| SignalValue::Text("incomplete".into())),
        ),
        ("rtt", nonzero(tcp.map(|t| t.rtt_us)).map(SignalValue::Us)),
        (
            "min_rtt",
            nonzero(tcp.map(|t| t.min_rtt_us)).map(SignalValue::Us),
        ),
        (
            "retransmits",
            nonzero(tcp.map(|t| t.retrans)).map(SignalValue::Count),
        ),
        (
            "server_timing",
            d.and_then(|d| d.server_timing.clone())
                .map(SignalValue::Text),
        ),
        (
            "streams",
            d.and_then(|d| d.streams_count)
                .map(|n| SignalValue::Count(u64::from(n))),
        ),
        (
            "upload_acked",
            d.and_then(|d| d.upload_acked_us).map(SignalValue::Us),
        ),
        (
            "send_buffered",
            d.and_then(|d| d.send_buffered_us).map(SignalValue::Us),
        ),
        (
            "cache_status",
            d.and_then(|d| d.cache_status.clone())
                .map(SignalValue::Text),
        ),
        (
            "region",
            d.and_then(|d| d.region.clone()).map(SignalValue::Text),
        ),
    ];
    all.into_iter()
        .filter_map(|(key, value)| value.map(|value| RequestSignal { key, value }))
        .collect()
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
    /// The async call stack captured while the operation was in flight (slow operations).
    pub await_tree: Option<String>,
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
                api: q.api_name().to_owned(),
                status: q.status,
                attempt: q.attempt,
                start_us: q.t_us - o.t_us,
                queue_us: q.queue_us,
                connect_us: q.connect_us(),
                send_us: q.send_us,
                ttfb_us: q.ttfb_us,
                body_us: q.body_us,
                stall_us: q.stall_us,
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
                signals: request_signals(q),
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
        await_tree: o.await_tree.clone(),
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
    /// Connections dialed in each interval (connection churn behind a latency spike).
    pub new_connections_count: Vec<u64>,
    /// The client's CPU in percent of one core (350: 3.5 cores busy), where it was sampled.
    pub cpu_pct: Vec<Option<f64>>,
    /// Open-loop arrivals due in each interval that never started.
    pub missed_arrivals_count: Vec<u64>,
    /// The verb the series are filtered to ([`TimelineView::for_verb`]); `None`: every verb.
    pub verb: Option<String>,
}

impl TimelineView {
    pub fn of(r: &Report) -> Self {
        let mut v = Self::default();
        let timeline = window_points(r);
        let n = timeline.len();
        for (i, p) in timeline.iter().enumerate() {
            let width = interval_width(r, p, i + 1 == n);
            // The last interval ends with the window (the charts end where the tiles do).
            let len = if i + 1 == n { width } else { interval_len(p) };
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
            v.cpu_pct.push(p.cpu_pct);
            v.missed_arrivals_count.push(p.missed_arrivals);
        }
        v
    }

    /// The client's CPU in cores busy per interval (0 where it was not sampled); `None` when
    /// no interval sampled it.
    pub fn cpu_cores(&self) -> Option<Vec<f64>> {
        self.cpu_pct.iter().any(Option::is_some).then(|| {
            self.cpu_pct
                .iter()
                .map(|c| c.unwrap_or(0.0) / 100.0)
                .collect()
        })
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
            sum(&self.missed_arrivals_count, &mut v.missed_arrivals_count);
            if self.cpu_pct.len() == n {
                // Averaged over the sampled intervals' time.
                let (busy, t) = self.cpu_pct[r.clone()]
                    .iter()
                    .zip(&self.len_s[r.clone()])
                    .filter_map(|(c, l)| c.map(|c| (c * l, *l)))
                    .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
                v.cpu_pct.push((t > 0.0).then(|| busy / t));
            }
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
        let timeline = window_points(r);
        let n = timeline.len();
        for (i, p) in timeline.iter().enumerate() {
            let width = interval_width(r, p, i + 1 == n);
            let x = p.verbs.iter().find(|x| x.verb == verb);
            let ops = x.map_or(0, |x| x.ops_count);
            let rows = x.map(|x| x.latency_rows_count.as_slice()).unwrap_or(&[]);
            let q = |q: f64| row_quantile_ms(rows, &bounds, q);
            v.t_s.push(p.t_s);
            v.len_s
                .push(if i + 1 == n { width } else { interval_len(p) });
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

    /// Which charts of the timeline say something of their own (9.1 review §2 item 3):
    /// requests per second only when an operation is not one request; throughput as its own
    /// chart only when the bytes per operation vary, else as goodput's right axis; the client's
    /// CPU only when the client was saturated (`client_saturated`, [`TrustView`]).
    pub fn layout(&self, client_saturated: bool) -> TimelineLayout {
        let active: Vec<usize> = (0..self.len())
            .filter(|i| self.ops_per_s.get(*i).is_some_and(|v| *v > 0.0))
            .collect();
        let ratio_of = |v: &[f64], i: usize| v.get(i).copied().unwrap_or(0.0) / self.ops_per_s[i];
        let steady = |v: &[f64], centre: f64, tol: f64| {
            !active.is_empty()
                && active.iter().all(|i| {
                    let r = ratio_of(v, *i);
                    (r - centre).abs() <= tol * centre.abs().max(1e-12)
                })
        };
        let has_requests =
            self.requests_per_s.len() == self.len() && self.requests_per_s.iter().any(|v| *v > 0.0);
        let requests = has_requests && !steady(&self.requests_per_s, 1.0, 0.02);
        let moved = self.mib_per_s.len() == self.len() && self.mib_per_s.iter().any(|v| *v > 0.0);
        let throughput = if !moved {
            ThroughputChart::None
        } else {
            let (mib, ops) = active.iter().fold((0.0, 0.0), |(m, o), i| {
                let l = self.len_s.get(*i).copied().unwrap_or(1.0);
                (m + self.mib_per_s[*i] * l, o + self.ops_per_s[*i] * l)
            });
            let per_op = if ops > 0.0 { mib / ops } else { 0.0 };
            let no_failures = self
                .ops_per_s
                .iter()
                .zip(&self.goodput_ops_per_s)
                .all(|(a, g)| (a - g).abs() <= 1e-9 * a.abs().max(1.0));
            if per_op > 0.0 && no_failures && steady(&self.mib_per_s, per_op, 0.02) {
                ThroughputChart::OnGoodput { mib_per_op: per_op }
            } else {
                ThroughputChart::Own
            }
        };
        TimelineLayout {
            requests,
            throughput,
            cpu: client_saturated && self.cpu_cores().is_some(),
        }
    }
}

/// Which timeline charts a front end draws ([`TimelineView::layout`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineLayout {
    /// Requests per second differ from operations per second (retries, multipart, pages).
    pub requests: bool,
    pub throughput: ThroughputChart,
    /// The client's CPU over time (only when the client was saturated).
    pub cpu: bool,
}

/// Where the timeline draws bytes per second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ThroughputChart {
    /// Nothing moved.
    None,
    /// Its own chart: the bytes per operation vary.
    Own,
    /// The goodput chart's right axis: every operation moved `mib_per_op` MiB, so throughput
    /// is goodput times it (the same shape).
    OnGoodput { mib_per_op: f64 },
}

/// The timeline clipped to the measurement window: an interval that starts at or after the
/// window's end (the drain, where operations started in the window finish) holds no started
/// operation, only events — its requests, retries, throttled responses and failures are folded
/// into the window's last interval, so no chart runs past the window or drops to zero there.
fn window_points(r: &Report) -> std::borrow::Cow<'_, [crate::trace::TimelinePoint]> {
    let w = r.run.window_s;
    let inside = r.timeline.iter().take_while(|p| p.t_s < w - 1e-6).count();
    if w <= 0.0 || inside == 0 || inside == r.timeline.len() {
        return std::borrow::Cow::Borrowed(&r.timeline);
    }
    let mut pts: Vec<crate::trace::TimelinePoint> = r.timeline[..inside].to_vec();
    let last_i = pts.len() - 1;
    let width = interval_width(r, &pts[last_i], true);
    let p = &mut pts[last_i];
    // Requests as a count (reports before 6.3 carry only the rate).
    let mut requests = if p.requests_count > 0 {
        p.requests_count as f64
    } else {
        p.requests_per_s * width
    };
    for s in &r.timeline[inside..] {
        p.errors += s.errors;
        p.throttled += s.throttled;
        p.throttled_ops += s.throttled_ops;
        p.retries_count += s.retries_count;
        p.missed_arrivals += s.missed_arrivals;
        p.new_connections += s.new_connections;
        p.inflight_peak = p.inflight_peak.max(s.inflight_peak);
        p.requests_count += s.requests_count;
        requests += if s.requests_count > 0 {
            s.requests_count as f64
        } else {
            s.requests_per_s * interval_len(s)
        };
        for v in &s.verbs {
            if let Some(x) = p.verbs.iter_mut().find(|x| x.verb == v.verb) {
                x.errors_count += v.errors_count;
                x.throttled_count += v.throttled_count;
            } else {
                p.verbs.push(crate::trace::VerbInterval {
                    verb: v.verb.clone(),
                    errors_count: v.errors_count,
                    throttled_count: v.throttled_count,
                    ..Default::default()
                });
            }
        }
    }
    p.requests_per_s = requests / width.max(1e-9);
    std::borrow::Cow::Owned(pts)
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

/// One phase of dialing a connection (`dns`, `tcp`, `proxy` — the proxy's CONNECT —, `tls`),
/// p50 and p99 over the dials that had it.
#[derive(Clone, Debug, PartialEq)]
pub struct DialPhase {
    pub key: &'static str,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

/// The kernel's view of the connections (`TCP_INFO`): whether the network or the server is
/// the limit.
#[derive(Clone, Debug, PartialEq)]
pub struct TcpView {
    /// The kernel's smoothed round trip: on a fast network it includes the server's delay and
    /// delayed acknowledgements, so it reads well above the network's (`min_rtt_p50_us`).
    pub rtt_p50_us: u64,
    pub rtt_p99_us: u64,
    /// The kernel's minimum round trip of the connections (p50 over the requests that
    /// recorded it): the network's own round trip, which server delay does not inflate.
    pub min_rtt_p50_us: Option<u64>,
    pub connections_count: u64,
    /// Connections that retransmitted at least once.
    pub retransmitting_connections_count: u64,
    pub retransmits_count: u64,
    /// Share of the sending time limited by the peer's receive window / the local send buffer.
    pub rwnd_limited_ratio: f64,
    pub sndbuf_limited_ratio: f64,
    /// The kernel's delivery rate estimate, p50 (0: not reported).
    pub delivery_rate_p50_bits_per_s: u64,
}

impl TcpView {
    /// `rtt p50/p99 1.2ms/3.4ms · 2 of 16 connections retransmitted · rwnd-limited 3.0% ·
    /// sndbuf-limited 0.0% · delivery p50 812 Mbit/s`.
    pub fn line(&self) -> String {
        use crate::util::fmt_ms;
        let min = self
            .min_rtt_p50_us
            .map(|m| {
                if m < 1000 {
                    format!("min rtt {m}µs · ")
                } else {
                    format!("min rtt {} · ", fmt_ms(m as f64 / 1000.0))
                }
            })
            .unwrap_or_default();
        let mut s = format!(
            "{min}smoothed rtt p50/p99 {}/{} · {} of {} connections retransmitted · rwnd-limited {:.1}% · sndbuf-limited {:.1}%",
            fmt_ms(self.rtt_p50_us as f64 / 1000.0),
            fmt_ms(self.rtt_p99_us as f64 / 1000.0),
            self.retransmitting_connections_count,
            self.connections_count,
            self.rwnd_limited_ratio * 100.0,
            self.sndbuf_limited_ratio * 100.0,
        );
        if self.delivery_rate_p50_bits_per_s > 0 {
            s += &format!(
                " · delivery p50 {}",
                fmt_bits(self.delivery_rate_p50_bits_per_s)
            );
        }
        s
    }
}

/// The measured operator's connections that the kernel reported on, from its request rows:
/// `(connections, connections that retransmitted, retransmitted segments)`. `None` when no
/// row carries the kernel's statistics.
fn measured_connections(run: &Run) -> Option<(u64, u64, u64)> {
    let mut by: std::collections::BTreeMap<u64, u64> = Default::default();
    for q in run.reqs.iter().filter(|q| q.conn_id != 0) {
        if let Some(t) = &q.tcp {
            let e = by.entry(q.conn_id).or_default();
            *e = (*e).max(t.retrans);
        }
    }
    if by.is_empty() {
        return None;
    }
    Some((
        by.len() as u64,
        by.values().filter(|r| **r > 0).count() as u64,
        by.values().sum(),
    ))
}

/// The p50 of the kernel's minimum round trip over the request rows that recorded one.
fn min_rtt_p50_us(run: &Run) -> Option<u64> {
    let mut v: Vec<u64> = run
        .reqs
        .iter()
        .filter_map(|q| q.tcp.as_ref().map(|t| t.min_rtt_us))
        .filter(|us| *us > 0)
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[(v.len() - 1) / 2])
}

/// `812 Mbit/s`.
pub fn fmt_bits(bps: u64) -> String {
    let b = bps as f64;
    if b >= 1e9 {
        format!("{:.1} Gbit/s", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} Mbit/s", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kbit/s", b / 1e3)
    } else {
        format!("{b:.0} bit/s")
    }
}

/// The TLS handshakes of the window.
#[derive(Clone, Debug, PartialEq)]
pub struct TlsView {
    pub handshakes_count: u64,
    pub resumption_ratio: f64,
    /// Handshakes that needed a HelloRetryRequest (an extra round trip).
    pub hrr_count: u64,
}

impl TlsView {
    /// `16 handshakes · 0.0% resumed · 0 HelloRetryRequest`.
    pub fn line(&self) -> String {
        format!(
            "{} handshakes · {:.1}% resumed · {} HelloRetryRequest",
            self.handshakes_count,
            self.resumption_ratio * 100.0,
            self.hrr_count
        )
    }
}

/// The run's cold start: the connections dialed at the start of the warmup and the operations
/// that waited for them.
#[derive(Clone, Debug, PartialEq)]
pub struct ColdStartView {
    pub connections_count: u64,
    pub connect_p50_ms: f64,
    pub connect_p99_ms: f64,
    pub first_ops_p50_ms: f64,
    pub first_ops_p99_ms: f64,
}

impl ColdStartView {
    /// `connect p50/p99 2.1ms/9.0ms · first operations p50/p99 …` (how many were dialed is
    /// [`ConnectionView::dials_line`]'s).
    pub fn line(&self) -> String {
        use crate::util::fmt_ms;
        format!(
            "connect p50/p99 {}/{} · first operations p50/p99 {}/{}",
            fmt_ms(self.connect_p50_ms),
            fmt_ms(self.connect_p99_ms),
            fmt_ms(self.first_ops_p50_ms),
            fmt_ms(self.first_ops_p99_ms),
        )
    }
}

/// The connections: protocol, reuse, dials (warmup included, T11) and their phases, and what
/// tells the network from the server as the limit — the kernel's TCP statistics, the TLS
/// handshakes and the cold start. The one model of the Connections section of every front end.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionView {
    /// `http/1.1 · plain`, `h2 · TLSv1.3 · …`.
    pub protocol: String,
    /// `TLSv1.3 · h2` (empty for plain connections).
    pub tls: String,
    pub requests_count: u64,
    pub http1_requests_count: u64,
    pub http2_requests_count: u64,
    /// New connections in the window.
    pub dialed_count: u64,
    pub dial_failures_count: u64,
    /// New connections during the warmup (the cold start's, or from the kept rows).
    pub warmup_dialed_count: u64,
    pub reuse_ratio: f64,
    pub peak_inflight_count: u64,
    /// The proxy the connections went through.
    pub proxy: Option<String>,
    /// `dns 1.2ms/3.4ms · tcp …` (p50/p99).
    pub phases: String,
    /// The dial phases that took time, proxy CONNECT included.
    pub dial: Vec<DialPhase>,
    pub tcp: Option<TcpView>,
    pub tls_handshakes: Option<TlsView>,
    pub cold_start: Option<ColdStartView>,
    pub remotes_count: u64,
    /// `ip:port` the requests went to.
    pub remote_addrs: Vec<String>,
}

impl ConnectionView {
    pub fn of(r: &Report, run: Option<&Run>) -> Self {
        let c = &r.connections;
        let n = &r.network;
        Self {
            protocol: super::connection_protocol(c),
            tls: [c.tls_version.as_str(), c.alpn.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · "),
            requests_count: c.requests,
            http1_requests_count: c.http1_requests,
            http2_requests_count: c.http2_requests,
            dialed_count: c.dialed,
            dial_failures_count: c.dial_failures,
            warmup_dialed_count: run
                .map_or(0, |run| {
                    run.reqs
                        .iter()
                        .filter(|q| q.t_us < 0 && !q.reused && q.connect_us() > 0)
                        .count() as u64
                })
                .max(c.cold_start.as_ref().map_or(0, |s| s.connections_count)),
            reuse_ratio: c.reuse_pct / 100.0,
            peak_inflight_count: c.peak_inflight_requests,
            proxy: c.proxy.clone(),
            phases: super::dial_phases(c),
            dial: [
                ("dns", &c.dns),
                ("tcp", &c.tcp),
                ("proxy", &c.proxy_connect),
                ("tls", &c.tls),
            ]
            .into_iter()
            .filter(|(_, q)| q.count > 0 && q.p99 > 0.0)
            .map(|(key, q)| DialPhase {
                key,
                p50_ms: q.p50,
                p99_ms: q.p99,
            })
            .collect(),
            tcp: n
                .tcp
                .kernel
                .as_ref()
                .filter(|k| k.samples_count > 0)
                .map(|k| {
                    // The measured operator's connections only (its rows), not the ones that
                    // set the dataset up: every count of this section is of the same
                    // connections (9.1 review §2 item 5).
                    let own = run.and_then(measured_connections);
                    TcpView {
                        min_rtt_p50_us: run.and_then(min_rtt_p50_us),
                        rtt_p50_us: k.rtt_p50_us,
                        rtt_p99_us: k.rtt_p99_us,
                        connections_count: own.map_or(k.connections_count, |o| o.0),
                        retransmitting_connections_count: own
                            .map_or(k.retransmitting_connections_count, |o| o.1),
                        retransmits_count: own.map_or(k.retrans_count, |o| o.2),
                        rwnd_limited_ratio: k.rwnd_limited_ratio,
                        sndbuf_limited_ratio: k.sndbuf_limited_ratio,
                        delivery_rate_p50_bits_per_s: k.delivery_rate_p50_bits_per_s,
                    }
                }),
            tls_handshakes: (n.tls.handshakes.handshakes_count > 0).then_some(TlsView {
                handshakes_count: n.tls.handshakes.handshakes_count,
                resumption_ratio: n.tls.handshakes.resumption_ratio,
                hrr_count: n.tls.handshakes.hrr_count,
            }),
            cold_start: c
                .cold_start
                .as_ref()
                .filter(|s| s.connections_count > 0)
                .map(|s| ColdStartView {
                    connections_count: s.connections_count,
                    connect_p50_ms: s.connect.p50,
                    connect_p99_ms: s.connect.p99,
                    first_ops_p50_ms: s.first_ops.p50,
                    first_ops_p99_ms: s.first_ops.p99,
                }),
            remotes_count: n
                .remotes
                .iter()
                .filter(|x| x.remote != crate::analysis::slice::NO_CONNECTION)
                .count() as u64,
            remote_addrs: c.remote_addrs.clone(),
        }
    }

    /// The connections dialed, each count with its window: `28 dialed during warm-up · 1 in
    /// the window` (the window alone when nothing was dialed during a warm-up).
    pub fn dials_line(&self) -> String {
        if self.warmup_dialed_count > 0 {
            format!(
                "{} dialed during warm-up · {} in the window",
                self.warmup_dialed_count, self.dialed_count
            )
        } else {
            format!("{} dialed in the window", self.dialed_count)
        }
    }

    /// The run went over the network (not an in-process backend).
    pub fn networked(&self) -> bool {
        self.requests_count > 0 && self.http1_requests_count + self.http2_requests_count > 0
    }
}

/// How OpenDAL wrote: the multipart part size it used (the requested `write.chunk` clamped to
/// the service's part-size limits) and the parts it uploaded — the knobs `prove` sizes for
/// `slow_multipart`.
#[derive(Clone, Debug, PartialEq)]
pub struct WritePathView {
    /// `write.chunk` as requested (0: not set).
    pub requested_chunk_bytes: u64,
    /// The part size OpenDAL wrote with.
    pub write_chunk_bytes: u64,
    pub service_min_part_bytes: Option<u64>,
    pub service_max_part_bytes: Option<u64>,
    /// OpenDAL changed the requested chunk to fit the service's limits.
    pub clamped: bool,
    /// Multipart parts uploaded, and their mean size.
    pub parts_count: u64,
    pub part_mean_bytes: Option<u64>,
    /// Median `max_part_ms / median_part_ms` across multipart writes with ≥ 2 parts.
    pub part_skew_ratio: Option<f64>,
    /// `CompleteMultipartUpload` p99 latency (ms) and share of multipart request time.
    pub complete_p99_ms: Option<f64>,
    pub complete_share_ratio: Option<f64>,
}

impl WritePathView {
    /// `None` when the run wrote nothing and recorded no write knobs.
    pub fn of(r: &Report) -> Option<Self> {
        Self::of_with(r, None)
    }

    /// [`Self::of`] enriched with per-operation part straggler skew from `run` rows.
    pub fn of_with(r: &Report, run: Option<&Run>) -> Option<Self> {
        let e = &r.effective_write;
        let parts: Vec<&crate::trace::ApiStats> = r
            .apis
            .iter()
            .filter(|a| a.api == crate::trace::Api::UploadPart && a.count > 0)
            .collect();
        let parts_count: u64 = parts.iter().map(|a| a.count).sum();
        let up: u64 = parts.iter().map(|a| a.bytes_up).sum();
        if r.bytes.logical_write == 0 && parts_count == 0 {
            return None;
        }
        if e.write_chunk_size == 0 && e.requested_chunk_size == 0 && parts_count == 0 {
            return None;
        }
        let complete = r
            .apis
            .iter()
            .find(|a| a.api == crate::trace::Api::CompleteMultipart && a.count > 0);
        let complete_p99_ms = complete.map(|c| c.stages.total.p99).filter(|v| *v > 0.0);
        let complete_share_ratio = complete.and_then(|c| {
            let mp_ms: f64 = r
                .apis
                .iter()
                .filter(|a| {
                    matches!(
                        a.api,
                        crate::trace::Api::CreateMultipart
                            | crate::trace::Api::UploadPart
                            | crate::trace::Api::CompleteMultipart
                    )
                })
                .map(|a| a.stages.total.mean * a.count as f64)
                .sum();
            let c_ms = c.stages.total.mean * c.count as f64;
            (mp_ms > 0.0).then_some(c_ms / mp_ms)
        });
        let part_skew_ratio =
            run.and_then(|run| {
                let mut by_op: std::collections::HashMap<u64, Vec<f64>> = Default::default();
                for q in run.reqs.iter().filter(|q| {
                    q.op_id != 0 && q.api == crate::trace::Api::UploadPart && !q.is_error()
                }) {
                    by_op.entry(q.op_id).or_default().push(q.total_us as f64);
                }
                let mut skews: Vec<f64> = by_op
                    .into_values()
                    .filter(|v| v.len() >= 2)
                    .filter_map(|mut v| {
                        v.sort_by(f64::total_cmp);
                        let med = crate::analysis::stats::median(&v);
                        let max = v.last().copied().unwrap_or(0.0);
                        (med > 0.0).then_some(max / med)
                    })
                    .collect();
                if skews.is_empty() {
                    None
                } else {
                    skews.sort_by(f64::total_cmp);
                    Some(crate::analysis::stats::median(&skews))
                }
            });
        Some(Self {
            requested_chunk_bytes: e.requested_chunk_size,
            write_chunk_bytes: e.write_chunk_size,
            service_min_part_bytes: e.service_min_part_size,
            service_max_part_bytes: e.service_max_part_size,
            clamped: e.write_chunk_clamped(),
            parts_count,
            part_mean_bytes: (parts_count > 0).then(|| up / parts_count),
            part_skew_ratio,
            complete_p99_ms,
            complete_share_ratio,
        })
    }

    /// Worth a line: OpenDAL wrote in parts, or changed the part size that was asked for.
    /// Without either (small writes in one request) the part size says nothing.
    pub fn shown(&self) -> bool {
        self.parts_count > 0 || self.clamped
    }

    /// `part size 8 MiB (asked 4 MiB; the service takes 5 MiB–5 GiB) · 12 parts of 8 MiB on
    /// average`.
    pub fn line(&self) -> String {
        use crate::util::{fmt_bytes, fmt_ms};
        let mut s = String::new();
        if self.write_chunk_bytes > 0 {
            s += &format!("part size {}", fmt_bytes(self.write_chunk_bytes));
            let mut why = Vec::new();
            if self.clamped {
                why.push(format!("asked {}", fmt_bytes(self.requested_chunk_bytes)));
            }
            match (self.service_min_part_bytes, self.service_max_part_bytes) {
                (Some(a), Some(b)) => why.push(format!(
                    "the service takes {}–{}",
                    fmt_bytes(a),
                    fmt_bytes(b)
                )),
                (Some(a), None) => why.push(format!("the service takes ≥ {}", fmt_bytes(a))),
                (None, Some(b)) => why.push(format!("the service takes ≤ {}", fmt_bytes(b))),
                (None, None) => {}
            }
            if !why.is_empty() {
                s += &format!(" ({})", why.join("; "));
            }
        }
        if let Some(m) = self.part_mean_bytes {
            if !s.is_empty() {
                s += " · ";
            }
            s += &format!("{} parts of {} on average", self.parts_count, fmt_bytes(m));
        }
        if let Some(sk) = self.part_skew_ratio.filter(|x| *x >= 1.25) {
            s += &format!(" · part straggler {sk:.2}×");
        }
        if let Some(ms) = self.complete_p99_ms {
            s += &format!(" · complete p99 {}", fmt_ms(ms));
        }
        s
    }
}

/// Client cache (`FoyerLayer` / hybrid cache) lookups, hits, bytes served and lookup latency.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheView {
    pub lookups_count: u64,
    pub hits_count: u64,
    pub hit_ratio: f64,
    pub hit_bytes: u64,
    pub lookup_mean_ms: f64,
}

impl CacheView {
    pub fn of(r: &Report) -> Option<Self> {
        if !r.cache.enabled && r.cache.lookups == 0 {
            return None;
        }
        Some(Self {
            lookups_count: r.cache.lookups,
            hits_count: r.cache.hits,
            hit_ratio: r.cache.hit_ratio,
            hit_bytes: r.bytes.cache_hit_bytes,
            lookup_mean_ms: r.cache.lookup_ms_mean,
        })
    }

    pub fn line(&self) -> String {
        let mut s = format!(
            "{:.1}% hit ({} of {} lookups, mean {})",
            self.hit_ratio * 100.0,
            self.hits_count,
            self.lookups_count,
            crate::util::fmt_ms(self.lookup_mean_ms)
        );
        if self.hit_bytes > 0 {
            s += &format!(
                " · {} served from cache",
                crate::util::fmt_bytes(self.hit_bytes)
            );
        }
        s
    }
}

/// `ListObjects` pagination: pages per list operation, page latency, and multi-page share.
#[derive(Clone, Debug, PartialEq)]
pub struct ListView {
    pub list_ops_count: u64,
    pub pages_count: u64,
    pub pages_per_op: f64,
    pub page_p50_ms: f64,
    pub page_p99_ms: f64,
    /// Share of list operations that needed > 1 page (when trace rows are present).
    pub paginated_ops_ratio: Option<f64>,
}

impl ListView {
    pub fn of(r: &Report, run: Option<&Run>) -> Option<Self> {
        let api = r
            .apis
            .iter()
            .find(|a| a.api == crate::trace::Api::ListObjects && a.count > 0)?;
        let list_ops: u64 = r
            .verbs
            .iter()
            .filter(|v| v.verb.starts_with("list") || v.verb == "ListObjects")
            .map(|v| v.count)
            .sum::<u64>()
            .max(1);
        // A page is a 2xx answer: a retried or throttled ListObjects request is no page.
        let pages: u64 = api
            .status
            .iter()
            .filter(|(s, _)| (200..300).contains(*s))
            .map(|(_, n)| n)
            .sum();
        let paginated_ops_ratio = run.and_then(|run| {
            let mut by_op: std::collections::HashMap<u64, u64> = Default::default();
            for q in run.reqs.iter().filter(|q| {
                q.op_id != 0
                    && q.api == crate::trace::Api::ListObjects
                    && (200..300).contains(&q.status)
            }) {
                *by_op.entry(q.op_id).or_default() += 1;
            }
            (!by_op.is_empty()).then(|| {
                let multi = by_op.values().filter(|c| **c > 1).count() as f64;
                multi / by_op.len() as f64
            })
        });
        Some(Self {
            list_ops_count: list_ops,
            pages_count: pages,
            pages_per_op: pages as f64 / list_ops as f64,
            page_p50_ms: api.stages.total.p50,
            page_p99_ms: api.stages.total.p99,
            paginated_ops_ratio,
        })
    }

    pub fn line(&self) -> String {
        use crate::util::fmt_ms;
        let mut s = format!(
            "{} pages across {} list ops ({:.2} pages/op) · page p50/p99 {}/{}",
            self.pages_count,
            self.list_ops_count,
            self.pages_per_op,
            fmt_ms(self.page_p50_ms),
            fmt_ms(self.page_p99_ms)
        );
        if let Some(r) = self.paginated_ops_ratio.filter(|r| *r > 0.0) {
            s += &format!(" · {:.1}% multi-page", r * 100.0);
        }
        s
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

/// How many slowest operations every front end shows (the profile, a range, `show`).
pub const SLOWEST_COUNT: usize = 5;

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
    pub retry: RetryView,
    pub write_path: Option<WritePathView>,
    pub cache: Option<CacheView>,
    pub list_path: Option<ListView>,
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
            slowest: run
                .map(|run| slowest(run, SLOWEST_COUNT))
                .unwrap_or_default(),
            timeline: TimelineView::of(r),
            connections: ConnectionView::of(r, run),
            operator: OperatorView::of(r),
            retry: RetryView::of(r),
            write_path: WritePathView::of_with(r, run),
            cache: CacheView::of(r),
            list_path: ListView::of(r, run),
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
    #[serde(default)]
    pub p999_ms: f64,
    /// Backend requests per second, retries included.
    #[serde(default)]
    pub requests_per_s: f64,
    #[serde(default)]
    pub retries_count: u64,
    #[serde(default)]
    pub throttled_responses_count: u64,
    pub throttled_request_ratio: f64,
    pub failed_ops_count: u64,
    pub error_ratio: f64,
    /// The top finding's code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_finding: Option<String>,
}

/// One backend API across the compared runs (one value per run, in the order of
/// [`CompareView::rows`]; 0 where a run did not call it).
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct CompareApi {
    pub api: String,
    pub requests_count: Vec<u64>,
    pub ttfb_p99_ms: Vec<f64>,
}

/// Several operators on the same workload, side by side, and how they differ.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct CompareView {
    pub rows: Vec<CompareRow>,
    /// The backend APIs any of them called, busiest (over all runs) first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<CompareApi>,
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
                    p999_ms: n.p999_ms,
                    requests_per_s: crate::util::ratio(n.requests_count as f64, r.run.window_s),
                    retries_count: r.resilience.retries,
                    throttled_responses_count: n.throttled_requests_count,
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
                let workload =
                    workload_differences(&first.spec.profile.workload, &r.spec.profile.workload);
                if !workload.is_empty() {
                    differences.push(format!(
                        "{name}: ran a different workload ({})",
                        workload.join(", ")
                    ));
                }
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
        Self {
            rows,
            apis: compare_apis(reports),
            differences,
        }
    }
}

/// The workload fields that differ between `a` and `b` (`concurrency 28 vs 16`); empty when
/// they ran the same workload.
fn workload_differences(
    a: &crate::config::WorkloadSpec,
    b: &crate::config::WorkloadSpec,
) -> Vec<String> {
    let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) =
        (serde_json::to_value(a), serde_json::to_value(b))
    else {
        return Vec::new();
    };
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let show = |v: Option<&serde_json::Value>| match v {
        None | Some(serde_json::Value::Null) => "unset".to_owned(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    };
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| format!("{k} {} vs {}", show(a.get(k)), show(b.get(k))))
        .collect()
}

/// Each API any of `reports` called, with its requests and TTFB p99 per report.
fn compare_apis(reports: &[&Report]) -> Vec<CompareApi> {
    let per: Vec<Vec<ApiRow>> = reports.iter().map(|r| apis(r)).collect();
    let mut names: Vec<(String, u64)> = Vec::new();
    for a in per.iter().flatten() {
        match names.iter_mut().find(|n| n.0 == a.api) {
            Some(n) => n.1 += a.requests_count,
            None => names.push((a.api.clone(), a.requests_count)),
        }
    }
    names.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    names
        .into_iter()
        .map(|(api, _)| {
            let of = |r: &Vec<ApiRow>| r.iter().find(|a| a.api == api).cloned();
            CompareApi {
                requests_count: per
                    .iter()
                    .map(|r| of(r).map_or(0, |a| a.requests_count))
                    .collect(),
                ttfb_p99_ms: per
                    .iter()
                    .map(|r| of(r).map_or(0.0, |a| a.ttfb_p99_ms))
                    .collect(),
                api,
            }
        })
        .collect()
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

    /// The tail of the distribution: `(latency ms, share of operations at or slower than
    /// it)`, from 1 at the fastest down to the slowest few — on log axes this is where every
    /// finding about the tail shows, which a linear CDF flattens above p90.
    pub fn tail(&self) -> Vec<(f64, f64)> {
        let mut prev = 0.0_f64;
        let mut out = Vec::with_capacity(self.pts.len());
        for (ms, f) in &self.pts {
            let slower = (1.0 - prev).max(0.0);
            if slower > 0.0 && *ms > 0.0 {
                out.push((*ms, slower));
            }
            prev = *f;
        }
        out
    }

    /// The latency at quantile `q` (0–1): the first point whose share reaches it.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        self.pts.iter().find(|p| p.1 >= q).map(|p| p.0)
    }

    /// The distribution of one verb's operations that started in `[t0, t1)` seconds, from
    /// the trace rows (weighted as a range is, [`RangeView`]); `None` when none did.
    pub fn of_range_verb(run: &Run, t0: f64, t1: f64, verb: &str) -> Option<Self> {
        let (lo, hi) = ((t0 * 1e6) as i64, (t1 * 1e6) as i64);
        let w = run.op_weights();
        let s = &run.report.sampling;
        let sampled = s.seen_ops_count > 0 && s.kept_ops_count < s.seen_ops_count;
        let mut lat: Vec<(u64, f64)> = run
            .ops
            .iter()
            .filter(|o| o.verb == verb && o.t_us >= lo && o.t_us < hi)
            .map(|o| (o.latency_us, w.get(&o.op_id).copied().unwrap_or(1.0)))
            .collect();
        (!lat.is_empty()).then(|| super::range::cdf(&mut lat, !sampled))
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
            return Self::fitted(&run.ops, r.run.window_s);
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

    /// The heatmap of one verb's operations, fitted as [`HeatmapView::of`] fits the rows;
    /// `None` when the rows are not every operation (sampled) or the verb has none.
    pub fn for_verb(r: &Report, run: &Run, verb: &str) -> Option<Self> {
        let complete = r.sampling.kept_ratio >= 1.0 || r.sampling.seen_ops_count == 0;
        if !complete {
            return None;
        }
        let ops: Vec<crate::model::OpRow> = run
            .ops
            .iter()
            .filter(|o| o.verb == verb && o.t_us >= 0)
            .cloned()
            .collect();
        if ops.is_empty() {
            return None;
        }
        Some(Self::fitted(&ops, r.run.window_s))
    }

    /// The fine heatmap of `ops`, its rows fitted to their latencies.
    fn fitted(ops: &[crate::model::OpRow], window_s: f64) -> Self {
        let mut h = crate::analysis::segments::fine_heatmap(ops, window_s, Self::MAX_COLUMNS);
        // The fitted rows end at the step at or below the slowest operation, so the last
        // row is open: close it at the next step, or a 260 ms straggler paints up to the
        // axis's next decade.
        if let Some(last) = h.latency_buckets_ms.last().copied()
            && last > 0.0
        {
            h.latency_buckets_ms.push(next_fine_step(last));
            for col in &mut h.counts {
                col.push(0);
            }
        }
        Self {
            t_buckets_s: h.t_buckets_s,
            bucket_width_s: h.bucket_width_s,
            latency_buckets_ms: h.latency_buckets_ms,
            counts: h.counts,
            fine: true,
        }
    }

    /// The largest cell (the colour scale's top; 0 when empty).
    pub fn max_count(&self) -> u64 {
        self.counts.iter().flatten().copied().max().unwrap_or(0)
    }
}

/// The fine heatmap's next row bound above `b` (1-1.5-2-3-5-7 steps per decade).
fn next_fine_step(b: f64) -> f64 {
    let decade = 10f64.powf(b.log10().floor());
    [1.0, 1.5, 2.0, 3.0, 5.0, 7.0, 10.0]
        .iter()
        .map(|m| m * decade)
        .find(|x| *x > b * 1.000_001)
        .unwrap_or(decade * 10.0)
}

/// One verb (`read.range`, `write.multipart`) with its numbers and where its time went.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct VerbView {
    pub verb: String,
    pub ops_count: u64,
    pub ops_per_s: f64,
    /// Logical bytes read and written per second.
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    /// Share of the verb's operations that saw a throttled response.
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
                mib_per_s: v.mib_per_s,
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
    /// An open-loop run's schedule: the rate asked for, the rate achieved and the lag.
    pub open_loop: Option<OpenLoopView>,
}

/// An open-loop run's arrivals against its schedule.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenLoopView {
    /// `constant`, `poisson`, `ramp`, … (empty when not recorded).
    pub arrivals: String,
    pub target_per_s: f64,
    pub achieved_per_s: f64,
    pub intended_count: u64,
    pub missed_count: u64,
    /// From an operation's intended start to its actual start.
    pub lag_p99_ms: Option<f64>,
    pub max_lag_ms: f64,
}

impl OpenLoopView {
    pub fn of(r: &Report) -> Option<Self> {
        let o = r.open_loop.as_ref()?;
        let sc = &r.schedule;
        Some(Self {
            arrivals: o.arrivals.clone(),
            target_per_s: o.target_rate,
            achieved_per_s: o.achieved_rate,
            intended_count: sc.intended_ops_count.max(o.intended_count),
            missed_count: o.missed.max(sc.missed_arrivals_count),
            lag_p99_ms: sc.schedule_stage.as_ref().map(|q| q.p99),
            max_lag_ms: o.max_schedule_lag_ms.max(sc.max_schedule_lag_ms),
        })
    }
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
            open_loop: OpenLoopView::of(r),
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
    /// Objects read back and verified, and those that came back corrupted.
    pub verified_count: u64,
    pub corrupted_count: u64,
    /// The run's checks that failed (a corrupted object, too many failed operations).
    pub breaches: Vec<String>,
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
                .map(|p| format!("{:?}", p.reason).to_lowercase()),
            verified_count: r.integrity.verified,
            corrupted_count: r.integrity.corrupted,
            breaches: r.verdict.sla.breaches.clone(),
        }
    }

    /// Something makes the numbers less certain (shown up front, not only in the details).
    pub fn concerns(&self) -> bool {
        self.client_saturated
            || self.partial.is_some()
            || self.corrupted_count > 0
            || !self.breaches.is_empty()
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

impl ApiRow {
    /// The status codes to show: the commonest answers below 400 (at most two), then every
    /// code of 400 and above and `0` (no response), each with its count.
    pub fn shown_status(&self) -> Vec<(u16, u64)> {
        let ok = self
            .status
            .iter()
            .filter(|(c, _)| *c != 0 && *c < 400)
            .take(2);
        let bad = self.status.iter().filter(|(c, _)| *c == 0 || *c >= 400);
        ok.chain(bad).copied().collect()
    }
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
    /// Hedged / cancelled straggler attempts (`TailCutLayer`).
    pub tail_cuts_count: u64,
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
            tail_cuts_count: x.tail_cuts,
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

    /// The resilience in one line, as the app's Retries section reads it: `12 retries ·
    /// 1.4s waiting · 429 ×10 · 503 ×2 · errors 5xx 2 / 4xx 0 / network 0 · 1 failed op ·
    /// classes: throttled ×10, timeout ×2`.
    pub fn line(&self) -> String {
        if self.quiet()
            && self.server_errors_count + self.client_errors_count + self.network_errors_count == 0
        {
            return "no retries, no failed operations".to_owned();
        }
        let mut s = format!(
            "{} {} · {} waiting · 429 ×{} · 503 ×{}",
            self.retries_count,
            if self.retries_count == 1 {
                "retry"
            } else {
                "retries"
            },
            crate::util::fmt_ms(self.backoff_ms),
            self.status_429_count,
            self.status_503_count,
        );
        if self.tail_cuts_count > 0 {
            s += &format!(" · {} tail-cut", self.tail_cuts_count);
        }
        if self.max_retry_after_ms > 0 {
            s += &format!(
                " (Retry-After up to {})",
                crate::util::fmt_ms(self.max_retry_after_ms as f64)
            );
        }
        s += &format!(
            " · errors 5xx {} / 4xx {} / network {} · {} failed {}",
            self.server_errors_count,
            self.client_errors_count,
            self.network_errors_count,
            self.failed_ops_count,
            if self.failed_ops_count == 1 {
                "op"
            } else {
                "ops"
            },
        );
        if !self.classes.is_empty() {
            s += &format!(
                " · classes: {}",
                self.classes
                    .iter()
                    .take(3)
                    .map(|(c, n)| format!("{c} ×{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        s
    }

    /// Nothing was retried and nothing failed.
    pub fn quiet(&self) -> bool {
        self.retries_count == 0
            && self.tail_cuts_count == 0
            && self.failed_ops_count == 0
            && self.status_429_count == 0
            && self.status_503_count == 0
    }
}

/// The capacity the run measured (9.1 review §2b gap 4): the selected limit model, and the
/// rate and in-flight ceilings fitted. Shown in Retries and ruled on the requests and
/// in-flight charts, not only inside a throttling finding.
#[derive(Clone, Debug, PartialEq)]
pub struct CapacityView {
    /// The selected hypothesis (`rate limit`, `in-flight limit`, …).
    pub hypothesis: String,
    pub confidence: Confidence,
    pub rate_ceiling_requests_per_s: Option<f64>,
    /// `(estimate, lo, hi)` requests in flight.
    pub inflight_ceiling: Option<(f64, f64, f64)>,
}

impl CapacityView {
    /// `None` when no limit was found (no hypothesis selected, or "no limit", and no ceiling).
    pub fn of(r: &Report) -> Option<Self> {
        let c = &r.capacity;
        let kind = c
            .selected()
            .map(|h| h.kind)
            .filter(|k| *k != crate::analysis::hypothesis::HypothesisKind::None);
        let inflight = c.inflight_ceiling.map(|x| (x.requests, x.lo, x.hi));
        let rate = c.rate_ceiling_requests_per_s.filter(|v| *v > 0.0);
        if kind.is_none() && inflight.is_none() && rate.is_none() {
            return None;
        }
        Some(Self {
            hypothesis: kind.map(|k| k.label().to_owned()).unwrap_or_default(),
            confidence: c.confidence,
            rate_ceiling_requests_per_s: rate,
            inflight_ceiling: inflight,
        })
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
    /// The key for people: a size bucket as its range (`64 KiB – 1 MiB`), `first read` and
    /// `re-read` for the cold and warm reads; other keys as they are.
    pub label: String,
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
    /// The median latency (remotes only).
    pub p50_ms: Option<f64>,
    pub p99_ms: Option<f64>,
}

/// The remotes in numbers ([`SpreadView::remotes_summary`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RemotesSummary {
    pub remotes_count: u64,
    /// Failed requests over all requests to the remotes.
    pub error_ratio: f64,
    /// `(lowest, highest)` of the remotes' p50 and p99.
    pub p50_ms: (f64, f64),
    pub p99_ms: (f64, f64),
}

/// A slice key for people: the object-size buckets (`0`, `<4KiB`, … `>=16MiB`) as ranges,
/// `cold` / `warm` as first read / re-read of a key.
pub fn slice_label(key: &str) -> String {
    let range = |a: &str, b: &str| format!("{a} – {b}");
    match key {
        "0" => "0 B".to_owned(),
        "<4KiB" => range("1 B", "4 KiB"),
        "<64KiB" => range("4 KiB", "64 KiB"),
        "<1MiB" => range("64 KiB", "1 MiB"),
        "<16MiB" => range("1 MiB", "16 MiB"),
        ">=16MiB" => "≥ 16 MiB".to_owned(),
        "cold" => "first read".to_owned(),
        "warm" => "re-read".to_owned(),
        k => k.to_owned(),
    }
}

/// Where the traffic went: the hottest keys, object sizes, prefixes and the server addresses.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SpreadView {
    pub hot_keys: Vec<Slice>,
    pub sizes: Vec<Slice>,
    pub prefixes: Vec<Slice>,
    /// `cold` (first read of a key) and `warm` (repeat reads of a key: not the connection).
    pub cold_warm: Vec<Slice>,
    pub remotes: Vec<Slice>,
}

impl SpreadView {
    pub const TOP: usize = 8;

    pub fn of(r: &Report) -> Self {
        let d = &r.dimensions;
        let slice = |x: &crate::model::DimensionSlice| Slice {
            key: x.key.clone(),
            label: slice_label(&x.key),
            requests_count: x.requests_count,
            share_ratio: x.share_ratio,
            throttled_ratio: Some(x.throttled_ratio),
            throttle_share_ratio: None,
            error_ratio: Some(x.error_ratio),
            p50_ms: None,
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
                    label: k.key.clone(),
                    requests_count: k.requests_count,
                    share_ratio: k.share_ratio,
                    throttled_ratio: None,
                    throttle_share_ratio: Some(k.throttle_share_ratio),
                    error_ratio: None,
                    p50_ms: None,
                    p99_ms: None,
                })
                .collect(),
            sizes: d.size_buckets.iter().take(Self::TOP).map(slice).collect(),
            prefixes: d.prefixes.iter().take(Self::TOP).map(slice).collect(),
            cold_warm: d.cold_warm.iter().take(Self::TOP).map(slice).collect(),
            remotes: r
                .network
                .remotes
                .iter()
                .take(Self::TOP)
                .map(|x| Slice {
                    key: x.remote.clone(),
                    label: x.remote.clone(),
                    requests_count: x.requests_count,
                    share_ratio: crate::util::ratio(x.requests_count as f64, total as f64),
                    throttled_ratio: None,
                    throttle_share_ratio: None,
                    error_ratio: Some(crate::util::ratio(
                        x.errors_count as f64,
                        x.requests_count as f64,
                    )),
                    p50_ms: Some(x.p50_ms),
                    p99_ms: Some(x.p99_ms),
                })
                .collect(),
        }
    }

    /// The remotes in one line's numbers (9.1 review §2b gap 8): how many answered, the share
    /// of their requests that failed, and the range of their p50 and p99. `None` without
    /// remotes.
    pub fn remotes_summary(&self) -> Option<RemotesSummary> {
        let xs: Vec<&Slice> = self
            .remotes
            .iter()
            .filter(|x| x.key != crate::analysis::slice::NO_CONNECTION)
            .collect();
        if xs.is_empty() {
            return None;
        }
        let reqs: u64 = self.remotes.iter().map(|x| x.requests_count).sum();
        let errs: f64 = self
            .remotes
            .iter()
            .map(|x| x.error_ratio.unwrap_or(0.0) * x.requests_count as f64)
            .sum();
        let range = |f: fn(&Slice) -> Option<f64>| {
            xs.iter()
                .filter_map(|x| f(x))
                .fold((f64::INFINITY, 0.0_f64), |(lo, hi), v| {
                    (lo.min(v), hi.max(v))
                })
        };
        Some(RemotesSummary {
            remotes_count: xs.len() as u64,
            error_ratio: crate::util::ratio(errs, reqs as f64),
            p50_ms: range(|x| x.p50_ms),
            p99_ms: range(|x| x.p99_ms),
        })
    }

    /// Which of the columns `throttled`, `errors`, `p99` any of `xs` has (a column no slice
    /// fills is not drawn: hot keys carry neither errors nor a p99).
    pub fn columns(xs: &[Slice]) -> (bool, bool, bool) {
        (
            xs.iter()
                .any(|x| x.throttled_ratio.or(x.throttle_share_ratio).is_some()),
            xs.iter().any(|x| x.error_ratio.is_some()),
            xs.iter().any(|x| x.p99_ms.is_some()),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.hot_keys.is_empty()
            && self.sizes.is_empty()
            && self.prefixes.is_empty()
            && self.cold_warm.is_empty()
            && self.remotes.is_empty()
    }
}

/// The stored proves of run `run_id` among `docs` (`<id>.prove.json` documents, newest
/// first): those of that run that decode, in their order.
pub fn proves_of_run(
    docs: Vec<serde_json::Value>,
    run_id: &str,
) -> Vec<crate::app::prove::ProveOutput> {
    docs.into_iter()
        .filter(|v| v.get("run_id").and_then(|x| x.as_str()) == Some(run_id))
        .filter_map(|v| {
            serde_json::to_vec(&v)
                .ok()
                .and_then(|b| crate::app::prove::ProveOutput::from_json_slice(&b).ok())
        })
        .collect()
}

/// The run a `.prove.json` stands for when its own run is not in the store (a prove shared by
/// a teammate): its operator, no rows, its run id (the prove's when it has none). A front end
/// shows the prove over it.
pub fn prove_run(p: &crate::app::prove::ProveOutput) -> Run {
    let prof = crate::config::Profile::from_operator(p.operator.redacted());
    let raw_id = if p.run_id.is_empty() {
        p.prove_id.clone()
    } else {
        p.run_id.clone()
    };
    let run_id = crate::model::RunId::parse(&raw_id).unwrap_or_default();
    let now = std::time::SystemTime::now();
    let mut run = crate::model::build_run(crate::model::ReportInput {
        run_id: &run_id,
        target_name: None,
        profile: &prof,
        knobs: &crate::config::Knobs::of(&prof),
        snapshot: crate::trace::Snapshot::default(),
        trace: crate::trace::TraceRows::default(),
        cleanup: crate::engine::CleanupSummary::default(),
        warnings: Vec::new(),
        started_at: now,
        ended_at: now,
        spec: None,
    });
    run.report.run.source = crate::model::RunSource::Trace;
    run.report.run.id = raw_id;
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A retried or throttled ListObjects request returned no page: ten single-page lists with
    /// two 503s retried are 10 pages, 1.00 per list (9.0 said 12 and 1.20).
    #[test]
    fn list_pages_count_only_successful_answers() {
        let mut run = crate::report::fixture::run();
        let list = |op_id: u64, t_us: i64, status: u16| {
            let mut q = crate::report::fixture::req(op_id, t_us, status, true);
            q.api = crate::trace::Api::ListObjects;
            q
        };
        run.reqs.clear();
        for i in 0..10u64 {
            if i < 2 {
                run.reqs.push(list(100 + i, i as i64 * 1000, 503));
            }
            run.reqs.push(list(100 + i, i as i64 * 1000 + 500, 200));
        }
        let r = &mut run.report;
        r.apis = vec![crate::trace::ApiStats {
            api: crate::trace::Api::ListObjects,
            count: 12,
            throttled: 2,
            errors: 2,
            status: [(200, 10), (503, 2)].into_iter().collect(),
            ..Default::default()
        }];
        r.verbs = vec![crate::trace::VerbStats {
            verb: "list".into(),
            count: 10,
            ..Default::default()
        }];
        let v = ListView::of(&run.report, Some(&run)).unwrap();
        assert_eq!(v.list_ops_count, 10);
        assert_eq!(v.pages_count, 10);
        assert!((v.pages_per_op - 1.0).abs() < 1e-9, "{v:?}");
        assert_eq!(v.paginated_ops_ratio, Some(0.0));
    }

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
        // The fixture's last interval starts at the window's end: it is folded into the one
        // before.
        assert_eq!(
            v.timeline.t_s.len(),
            r.timeline.iter().filter(|p| p.t_s < r.run.window_s).count()
        );
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
    fn a_comparison_has_the_tail_the_problems_and_each_api() {
        let a = crate::report::fixture::report();
        let mut b = a.clone();
        b.resilience.retries = 12;
        b.apis = vec![crate::trace::ApiStats {
            api: crate::trace::Api::HeadObject,
            count: 5,
            ..Default::default()
        }];
        let mut a = a;
        a.apis = vec![crate::trace::ApiStats {
            api: crate::trace::Api::GetObject,
            count: 7,
            ..Default::default()
        }];
        let v = CompareView::of(&[&a, &b]);
        assert_eq!(v.rows[1].retries_count, 12);
        let names: Vec<&str> = v.apis.iter().map(|x| x.api.as_str()).collect();
        assert_eq!(names, ["GetObject", "HeadObject"]);
        assert_eq!(v.apis[0].requests_count, vec![7, 0]);
        assert_eq!(v.apis[1].requests_count, vec![0, 5]);
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
    fn a_comparison_lists_a_workload_difference() {
        let a = crate::report::fixture::report();
        let mut b = a.clone();
        b.spec.profile.workload.concurrency = a.spec.profile.workload.concurrency + 12;
        let v = CompareView::of(&[&a, &b]);
        assert!(
            v.differences[0].contains("different workload")
                && v.differences[0].contains("concurrency"),
            "{:?}",
            v.differences
        );
        let same = CompareView::of(&[&a, &a]);
        assert!(
            same.differences.iter().all(|d| !d.contains("workload")),
            "{:?}",
            same.differences
        );
    }

    #[test]
    fn findings_keep_the_reports_order() {
        // A partial run: `invalid_run` (a warning) leads a critical finding; the text must
        // not re-sort what the JSON keeps (K3).
        let mut r = crate::report::fixture::report();
        let mut first = r.findings[0].clone();
        first.code = "invalid_run".into();
        first.severity = Severity::Warning;
        let mut second = r.findings[0].clone();
        second.severity = Severity::Critical;
        r.findings = vec![first, second];
        let v = findings(&r);
        assert_eq!(v[0].code, "invalid_run");
        assert_eq!(v[1].severity, Severity::Critical);
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
    fn a_waterfall_gives_the_stall_its_own_segment_and_lists_the_request_signals() {
        let mut run = crate::report::fixture::run();
        let q = &mut run.reqs[2];
        q.stall_us = 1_500;
        q.detail = Some(Box::new(crate::model::ReqDetail {
            cut: true,
            streams_count: Some(100),
            upload_acked_us: Some(900),
            send_buffered_us: Some(40),
            cache_status: Some("miss".into()),
            region: Some("eu-west-1".into()),
            error_code: Some("SlowDown".into()),
            ..Default::default()
        }));
        run.ops[1].await_tree = Some("read\n  http".into());
        let w = waterfall(&run, 2).unwrap();
        let r = &w.rows[1];
        // The stall is not in the body any more.
        assert_eq!((r.body_us, r.stall_us), (4_000, 1_500));
        let keys: Vec<&str> = r.signals.iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            vec![
                "code",
                "tail_cut",
                "streams",
                "upload_acked",
                "send_buffered",
                "cache_status",
                "region"
            ]
        );
        assert_eq!(r.signals[1].value, SignalValue::Text("hedged".into()));
        assert_eq!(r.signals[1].text(), "tail-cut hedged");
        assert!(r.signals[1].explains());
        assert!(!r.signals[6].explains(), "the region is a detail");
        assert_eq!(w.await_tree.as_deref(), Some("read\n  http"));
        // A plain request says nothing beyond its stages.
        assert!(w.rows[0].signals.is_empty());
    }

    #[test]
    fn wire_headers_stay_in_the_rows_and_out_of_the_view() {
        let mut q = crate::report::fixture::run().reqs[0].clone();
        q.detail = Some(Box::new(crate::model::ReqDetail {
            cdn_pop: Some("FRA".into()),
            age_s: Some(12),
            storage_class: Some("STANDARD_IA".into()),
            encryption: Some("aws:kms".into()),
            region: Some("eu-west-1".into()),
            ..Default::default()
        }));
        let keys: Vec<&str> = request_signals(&q).iter().map(|s| s.key).collect();
        assert_eq!(keys, vec!["region"]);
        let row = serde_json::to_value(&q).unwrap();
        assert_eq!(row["detail"]["cdn_pop"], "FRA", "{row}");
        assert_eq!(row["detail"]["storage_class"], "STANDARD_IA", "{row}");
    }

    #[test]
    fn the_connection_view_tells_the_network_from_the_server() {
        let mut r = crate::report::fixture::report();
        let c = &mut r.connections;
        c.requests = 100;
        c.http1_requests = 100;
        c.dialed = 16;
        c.proxy_connect = crate::trace::Quantiles {
            count: 16,
            p50: 1.0,
            p99: 4.0,
            ..Default::default()
        };
        c.cold_start = Some(crate::trace::ColdStartStats {
            connections_count: 16,
            connect: crate::trace::Quantiles {
                count: 16,
                p50: 2.0,
                p99: 9.0,
                ..Default::default()
            },
            first_ops: crate::trace::Quantiles {
                count: 16,
                p50: 20.0,
                p99: 40.0,
                ..Default::default()
            },
        });
        r.network.tcp.kernel = Some(crate::net::stats::TcpKernelStats {
            samples_count: 100,
            connections_count: 16,
            rtt_p50_us: 1_200,
            rtt_p99_us: 3_400,
            retransmitting_connections_count: 2,
            retrans_count: 5,
            rwnd_limited_ratio: 0.03,
            delivery_rate_p50_bits_per_s: 812_000_000,
            ..Default::default()
        });
        r.network.tls.handshakes.handshakes_count = 16;
        r.network.tls.handshakes.resumption_ratio = 0.25;
        r.network.tls.handshakes.hrr_count = 1;
        let v = ConnectionView::of(&r, None);
        assert!(v.networked());
        assert_eq!(v.dial.len(), 1);
        assert_eq!(v.dial[0].key, "proxy");
        let tcp = v.tcp.as_ref().unwrap();
        assert_eq!(
            tcp.line(),
            "smoothed rtt p50/p99 1.20ms/3.40ms · 2 of 16 connections retransmitted · rwnd-limited 3.0% · sndbuf-limited 0.0% · delivery p50 812 Mbit/s"
        );
        // One count of dials, each with its window (K18): 16 in the warm-up's cold start.
        assert_eq!(v.warmup_dialed_count, 16);
        assert_eq!(
            v.dials_line(),
            "16 dialed during warm-up · 16 in the window"
        );
        // With rows: the minimum round trip (the network's) beside the smoothed one.
        let mut run = crate::report::fixture::run();
        run.report = r.clone();
        for (i, q) in run.reqs.iter_mut().enumerate() {
            q.tcp = Some(Box::new(crate::model::TcpRow {
                rtt_us: 7_000,
                min_rtt_us: 40 + i as u64,
                ..Default::default()
            }));
        }
        let with = ConnectionView::of(&run.report, Some(&run));
        let t = with.tcp.unwrap();
        assert!(t.min_rtt_p50_us.is_some_and(|m| m < 100), "{t:?}");
        assert!(t.line().starts_with("min rtt 4"), "{}", t.line());
        assert_eq!(
            v.tls_handshakes.as_ref().unwrap().line(),
            "16 handshakes · 25.0% resumed · 1 HelloRetryRequest"
        );
        let cold = v.cold_start.as_ref().unwrap();
        assert_eq!((cold.connections_count, cold.first_ops_p99_ms), (16, 40.0));
        // Nothing recorded: nothing to say.
        let quiet = ConnectionView::of(&crate::report::fixture::report(), None);
        assert!(
            quiet.tcp.is_none() && quiet.tls_handshakes.is_none() && quiet.cold_start.is_none()
        );
        assert!(!quiet.networked());
        assert_eq!(quiet.dials_line(), "0 dialed in the window");
    }

    #[test]
    fn the_write_path_says_the_part_size_and_why() {
        let mut r = crate::report::fixture::report();
        assert_eq!(WritePathView::of(&r), None, "a run that wrote nothing");
        r.bytes.logical_write = 96 << 20;
        r.effective_write = crate::trace::EffectiveKnobs {
            requested_chunk_size: 4 << 20,
            write_chunk_size: 5 << 20,
            service_min_part_size: Some(5 << 20),
            service_max_part_size: Some(5 << 30),
        };
        r.apis = vec![crate::trace::ApiStats {
            api: crate::trace::Api::UploadPart,
            count: 12,
            bytes_up: 12 * (5 << 20),
            ..Default::default()
        }];
        let w = WritePathView::of(&r).unwrap();
        assert!(w.clamped);
        assert_eq!((w.parts_count, w.part_mean_bytes), (12, Some(5 << 20)));
        let l = w.line();
        assert!(
            l.starts_with("part size 5.0 MiB (asked 4.0 MiB; the service takes 5.0 MiB–5.00 GiB)"),
            "{l}"
        );
        assert!(l.ends_with("12 parts of 5.0 MiB on average"), "{l}");
        assert!(w.shown());
        // Small writes in one request each: the part size says nothing, no line.
        r.apis.clear();
        r.effective_write.requested_chunk_size = 0;
        let w = WritePathView::of(&r).unwrap();
        assert!(!w.shown());
        assert!(!w.line().contains("no multipart"), "{}", w.line());
    }

    #[test]
    fn the_resilience_line_reads_like_the_retries_section() {
        let mut r = crate::report::fixture::report();
        r.resilience = Default::default();
        assert_eq!(RetryView::of(&r).line(), "no retries, no failed operations");
        r.resilience.retries = 12;
        r.resilience.retry_backoff_ms = 1400.0;
        r.resilience.status_429 = 10;
        r.resilience.status_503 = 2;
        r.resilience.server_errors = 2;
        r.resilience.failed_ops = 1;
        r.resilience.error_classes = [("throttled".to_owned(), 10), ("timeout".to_owned(), 2)]
            .into_iter()
            .collect();
        let l = RetryView::of(&r).line();
        for s in [
            "12 retries",
            "429 ×10",
            "503 ×2",
            "errors 5xx 2 / 4xx 0 / network 0",
            "1 failed op",
            "classes: throttled ×10, timeout ×2",
        ] {
            assert!(l.contains(s), "{s}: {l}");
        }
    }

    #[test]
    fn cpu_is_averaged_over_the_sampled_time_when_downsampled() {
        let t = TimelineView {
            t_s: vec![0.0, 1.0, 2.0, 3.0],
            len_s: vec![1.0; 4],
            cpu_pct: vec![Some(100.0), Some(300.0), None, Some(50.0)],
            ..Default::default()
        };
        assert_eq!(t.cpu_cores(), Some(vec![1.0, 3.0, 0.0, 0.5]));
        let (d, k) = t.downsample(2);
        assert_eq!(k, 2);
        assert_eq!(d.cpu_pct, vec![Some(200.0), Some(50.0)]);
        assert_eq!(TimelineView::default().cpu_cores(), None);
        let r = crate::report::fixture::report();
        let mut r2 = r.clone();
        r2.timeline[0].cpu_pct = Some(120.0);
        assert_eq!(TimelineView::of(&r2).cpu_pct[0], Some(120.0));
    }

    #[test]
    fn the_spread_has_cold_and_warm_reads() {
        let mut r = crate::report::fixture::report();
        r.dimensions.cold_warm = vec![crate::model::DimensionSlice {
            key: "cold".into(),
            requests_count: 10,
            error_ratio: 0.1,
            ..Default::default()
        }];
        let s = SpreadView::of(&r);
        assert_eq!(s.cold_warm[0].key, "cold");
        // Cold and warm are reads of a key, not connections.
        assert_eq!(s.cold_warm[0].label, "first read");
        assert_eq!(s.cold_warm[0].error_ratio, Some(0.1));
        assert!(!s.is_empty());
    }

    #[test]
    fn size_buckets_read_as_ranges_and_empty_columns_are_dropped() {
        assert_eq!(slice_label("0"), "0 B");
        assert_eq!(slice_label("<1MiB"), "64 KiB – 1 MiB");
        assert_eq!(slice_label(">=16MiB"), "≥ 16 MiB");
        assert_eq!(slice_label("warm"), "re-read");
        assert_eq!(slice_label("data/part=00"), "data/part=00");
        let mut r = crate::report::fixture::report();
        r.dimensions.hot_keys = vec![crate::model::HotKey {
            key: "k".into(),
            requests_count: 10,
            share_ratio: 0.5,
            throttle_share_ratio: 0.2,
        }];
        let s = SpreadView::of(&r);
        // A hot key has its share of the throttling, but no errors and no p99.
        assert_eq!(SpreadView::columns(&s.hot_keys), (true, false, false));
    }

    #[test]
    fn the_timeline_ends_with_the_window_and_folds_the_drain_into_it() {
        let mut r = crate::report::fixture::report();
        // A window closed at 2.98 s; retries and failures drained for two more seconds.
        r.run.window_s = 2.98;
        let p = |t: f64, ops: u64, requests: u64, retries: u64, errors: u64| {
            crate::trace::TimelinePoint {
                t_s: t,
                len_s: 1.0,
                ops_count: ops,
                ok_ops_count: ops,
                ops_per_s: ops as f64,
                goodput_ops_per_s: ops as f64,
                requests_count: requests,
                requests_per_s: requests as f64,
                retries_count: retries,
                errors,
                mib_per_s: ops as f64 / 10.0,
                ..Default::default()
            }
        };
        r.timeline = vec![
            p(0.0, 100, 100, 0, 0),
            p(1.0, 100, 100, 0, 0),
            p(2.0, 98, 98, 0, 0),
            p(3.0, 0, 6, 6, 2),
            p(4.0, 0, 4, 4, 1),
        ];
        // The recorder's rates of the last interval are over its 0.98 s inside the window.
        r.timeline[2].goodput_ops_per_s = 100.0;
        let v = TimelineView::of(&r);
        assert_eq!(v.len(), 3, "no interval past the window");
        assert!((v.t_s[2] + v.len_s[2] - 2.98).abs() < 1e-9, "{:?}", v.len_s);
        // No drop to zero at the end: the last interval's goodput is over its 0.98 s.
        assert!((v.goodput_ops_per_s[2] - 100.0).abs() < 1e-6);
        // The drain's events are in the last interval.
        assert_eq!(v.retries_count[2], 10);
        assert_eq!(v.errors_count[2], 3);
        assert!((v.requests_per_s[2] - 108.0 / 0.98).abs() < 1e-6);
        let read = TimelineView::for_verb(&r, "x");
        assert!(read.is_none());
    }

    #[test]
    fn the_fine_heatmap_closes_its_slowest_row() {
        let run = crate::report::fixture::run();
        let h = HeatmapView::of(&run.report, Some(&run));
        let max_ms = run
            .ops
            .iter()
            .filter(|o| o.t_us >= 0)
            .map(|o| o.latency_us)
            .max()
            .unwrap() as f64
            / 1000.0;
        // The slowest operation is in a closed row whose upper bound is the next step.
        let n = h.latency_buckets_ms.len();
        let top = h.latency_buckets_ms[n - 1];
        assert!(top > max_ms, "{:?} vs {max_ms}", h.latency_buckets_ms);
        assert!(top <= max_ms * 1.5 + 1e-9, "{:?}", h.latency_buckets_ms);
        assert!(h.counts.iter().all(|c| c.len() == n && c[n - 1] == 0));
        assert_eq!(next_fine_step(200.0), 300.0);
        assert_eq!(next_fine_step(7.0), 10.0);
        assert_eq!(next_fine_step(150.0), 200.0);
    }

    #[test]
    fn evidence_values_carry_their_unit() {
        assert_eq!(EvidenceUnit::of("p50_ms", 9.9), (EvidenceUnit::Ms, 9.9));
        let (u, a) = EvidenceUnit::of("straggler_pct", 2.6);
        assert_eq!(u, EvidenceUnit::Share);
        assert!((a - 0.026).abs() < 1e-12);
        assert_eq!(
            EvidenceUnit::of("p99_to_p50_ratio", 3.0),
            (EvidenceUnit::Times, 3.0)
        );
        assert_eq!(EvidenceUnit::of("error_ratio", 0.1).0, EvidenceUnit::Share);
        assert_eq!(
            EvidenceUnit::of("fitted_bytes_per_s", 1.0).0,
            EvidenceUnit::BytesPerS
        );
        assert_eq!(
            EvidenceUnit::of("accepted_requests_per_s", 1.0).0,
            EvidenceUnit::RequestsPerS
        );
        assert_eq!(EvidenceUnit::of("window_s", 8.0).0, EvidenceUnit::Seconds);
        assert_eq!(
            EvidenceUnit::of("affected_apis_count", 3.0).0,
            EvidenceUnit::Count
        );
        let mut f = crate::report::fixture::report().findings[0].clone();
        f.evidence.values.insert("affected_apis_count".into(), 3.0);
        f.evidence.values.insert("typical_ttfb_ms".into(), 9.87);
        let v = FindingView::of(&f);
        let item = |k: &str| v.items.iter().find(|i| i.key == k).unwrap().clone();
        assert_eq!(item("affected_apis_count").label, "APIs affected");
        let t = item("typical_ttfb_ms");
        assert_eq!(t.label, "Typical time to first byte");
        assert_eq!((t.unit, t.amount), (EvidenceUnit::Ms, 9.87));
    }

    #[test]
    fn a_tail_falls_from_every_operation_to_the_slowest() {
        let c = LatencyCdf {
            pts: vec![(1.0, 0.9), (10.0, 0.99), (100.0, 1.0)],
            exact: true,
        };
        let t = c.tail();
        assert_eq!(t[0], (1.0, 1.0));
        assert!(
            (t[1].1 - 0.1).abs() < 1e-9 && (t[2].1 - 0.01).abs() < 1e-9,
            "{t:?}"
        );
        assert!(LatencyCdf::default().tail().is_empty());
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

    /// Two 1 s intervals of `(ops/s, requests/s, MiB/s)`, every operation a success.
    fn tl91(rows: &[(f64, f64, f64)]) -> TimelineView {
        let n = rows.len();
        TimelineView {
            t_s: (0..n).map(|i| i as f64).collect(),
            len_s: vec![1.0; n],
            ops_per_s: rows.iter().map(|r| r.0).collect(),
            goodput_ops_per_s: rows.iter().map(|r| r.0).collect(),
            requests_per_s: rows.iter().map(|r| r.1).collect(),
            mib_per_s: rows.iter().map(|r| r.2).collect(),
            cpu_pct: vec![Some(80.0); n],
            ..TimelineView::default()
        }
    }

    #[test]
    fn the_timeline_draws_only_the_charts_that_say_something_of_their_own() {
        // One request per operation of fixed size: requests and throughput are goodput's shape.
        let t = tl91(&[(100.0, 100.0, 1.0), (50.0, 50.0, 0.5)]);
        let l = t.layout(false);
        assert!(!l.requests);
        assert_eq!(
            l.throughput,
            ThroughputChart::OnGoodput { mib_per_op: 0.01 }
        );
        assert!(!l.cpu, "CPU only when the client was saturated");
        assert!(t.layout(true).cpu);
        // Retries (more requests than operations) and varying sizes: their own charts.
        let t = tl91(&[(100.0, 130.0, 1.0), (50.0, 50.0, 2.0)]);
        let l = t.layout(false);
        assert!(l.requests);
        assert_eq!(l.throughput, ThroughputChart::Own);
        // Nothing moved: no throughput at all.
        assert_eq!(
            tl91(&[(10.0, 10.0, 0.0)]).layout(false).throughput,
            ThroughputChart::None
        );
        // Failures: goodput is not the operations' rate, so throughput stays apart.
        let mut t = tl91(&[(100.0, 100.0, 1.0), (50.0, 50.0, 0.5)]);
        t.goodput_ops_per_s[1] = 40.0;
        assert_eq!(t.layout(false).throughput, ThroughputChart::Own);
    }

    #[test]
    fn a_finding_card_does_not_repeat_the_headline_or_its_values() {
        let run = crate::report::fixture::run();
        let r = &run.report;
        let f = FindingView::of(&r.findings[0]);
        let card = f.card(&r.verdict.headline);
        if r.verdict
            .headline
            .starts_with(f.cause.split(". ").next().unwrap())
        {
            assert_eq!(card.cause, None, "the headline already says it");
        }
        let other = f.card("Something else entirely.");
        assert_eq!(other.cause.as_deref(), Some(f.cause.as_str()));
        if !f.items.is_empty() {
            assert_eq!(card.evidence, None, "the values' table holds the numbers");
        }
        let mut bare = f.clone();
        bare.items.clear();
        bare.evidence = "words".into();
        assert_eq!(bare.card("x").evidence.as_deref(), Some("words"));
    }

    #[test]
    fn an_api_shows_every_error_code_and_its_commonest_answers() {
        let a = ApiRow {
            api: "GetObject".into(),
            requests_count: 100,
            errors_count: 6,
            throttled_count: 5,
            status: vec![(200, 80), (206, 10), (304, 2), (503, 5), (404, 2), (0, 1)],
            reused_ratio: 1.0,
            up_bytes: 0,
            down_bytes: 0,
            ttfb_p50_ms: 0.0,
            ttfb_p99_ms: 0.0,
            total_p50_ms: 0.0,
            total_p99_ms: 0.0,
        };
        assert_eq!(
            a.shown_status(),
            vec![(200, 80), (206, 10), (503, 5), (404, 2), (0, 1)]
        );
    }

    #[test]
    fn capacity_remotes_and_the_open_loop_read_the_report() {
        let mut run = crate::report::fixture::run();
        let r = &mut run.report;
        r.capacity.rate_ceiling_requests_per_s = Some(120.0);
        let c = CapacityView::of(r).expect("a rate ceiling");
        assert_eq!(c.rate_ceiling_requests_per_s, Some(120.0));
        r.capacity = Default::default();
        assert_eq!(CapacityView::of(r), None);
        r.network.remotes = vec![
            crate::model::RemoteStats {
                remote: "10.0.0.1:443".into(),
                requests_count: 90,
                errors_count: 9,
                bytes_down: 0,
                p50_ms: 2.0,
                p99_ms: 20.0,
            },
            crate::model::RemoteStats {
                remote: "10.0.0.2:443".into(),
                requests_count: 10,
                errors_count: 1,
                bytes_down: 0,
                p50_ms: 4.0,
                p99_ms: 30.0,
            },
        ];
        let s = SpreadView::of(r).remotes_summary().unwrap();
        assert_eq!(s.remotes_count, 2);
        assert!((s.error_ratio - 0.1).abs() < 1e-9);
        assert_eq!((s.p50_ms, s.p99_ms), ((2.0, 4.0), (20.0, 30.0)));
        r.integrity.corrupted = 1;
        r.integrity.verified = 9;
        let t = TrustView::of(r);
        assert!(t.concerns() && t.corrupted_count == 1);
        assert_eq!(SetupView::of(r).open_loop.is_some(), r.open_loop.is_some());
    }

    #[test]
    fn the_connection_counts_are_the_measured_operators() {
        let mut run = crate::report::fixture::run();
        for (i, q) in run.reqs.iter_mut().enumerate() {
            q.conn_id = 1 + (i as u64 % 2);
            q.tcp = Some(Box::new(crate::model::TcpRow {
                retrans: if q.conn_id == 2 { 3 } else { 0 },
                ..Default::default()
            }));
        }
        assert_eq!(measured_connections(&run), Some((2, 1, 3)));
        run.reqs.iter_mut().for_each(|q| q.tcp = None);
        assert_eq!(measured_connections(&run), None);
    }
}
