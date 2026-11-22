//! Findings (design v6 §3 "Explain", §8 "Findings → remedies"): each names a cause, shows the
//! evidence (the requests themselves) and names **one** OpenDAL remedy as an `OperatorSpec`
//! path. A finding carries no sizing number: one run varies no load, so it cannot size a
//! change (T1, T2); `oprobe prove RUN CODE` sizes and tests the remedy.
//!
//! | code | remedy path |
//! |---|---|
//! | `throttling` | `layers.concurrent_limit.http` (sized) + `layers.retry.jitter` |
//! | `rate_limit` | `layers.concurrent_limit.http` (sized from rate × latency) |
//! | `key_limit` | the workload's key layout (advice, not a layer) |
//! | `bandwidth_cap` | `layers.throttle.bandwidth` |
//! | `tail_stragglers` | `layers.tail_cut` |
//! | `hung_requests` | `layers.timeout.io` |
//! | `small_ranges` | `read.gap` / `read.concurrent` |
//! | `slow_multipart` | `write.concurrent` (with `write.chunk`) |
//! | `repeated_reads` | `layers.cache` |
//! | `cold_connections` | `http.keep_alive` / `http.version` |
//! | `client_bound` | none (reported) |
//!
//! Plus `errors` (failures that are not throttling) and `invalid_run` (the run cannot be
//! judged). One finding per code, most severe first.

use std::collections::{BTreeMap, HashMap};

use super::capacity::{self, Confidence};
use super::diagnosis::Severity;
use super::hypothesis::{Hypothesis, HypothesisKind};
use super::outliers::dominant_stage;
use super::simulate::{self, TailCutSetup};
use super::stats::quantile_us_ms;
use crate::model::{Evidence, Finding, Remedy, Report, ReqRow, Run, TimeWindow};
use crate::util::fmt_ms;

/// Every finding code (canonical, `snake_case`); [`super::catalog`] documents exactly these.
pub const CODES: &[&str] = &[
    "throttling",
    "rate_limit",
    "key_limit",
    "bandwidth_cap",
    "tail_stragglers",
    "hung_requests",
    "small_ranges",
    "slow_multipart",
    "repeated_reads",
    "cold_connections",
    "client_bound",
    "errors",
    "invalid_run",
];

/// Codes that still hold on a run that cannot be judged on performance: failures and the
/// service's limits (T10: throttle-driven failures are throttling, never hidden).
const VALID_ON_BROKEN_RUNS: &[&str] = &[
    "throttling",
    "rate_limit",
    "key_limit",
    "client_bound",
    "errors",
    "invalid_run",
];

/// The share of the effect a limit model must explain for `high` confidence (T8).
pub const HIGH_EXPLAINS_RATIO: f64 = 0.7;

/// The remedy of `code` (`variant` picks between the two paths of `small_ranges` and
/// `cold_connections`): the path, the OpenDAL API, whether prove sizes it, and the sentence.
pub fn remedy(code: &str, variant: Option<&str>) -> Remedy {
    let r = |path: Option<&str>, with: &[&str], opendal: &str, sized: bool, text: &str| Remedy {
        path: path.map(str::to_owned),
        with: with.iter().map(|s| (*s).to_owned()).collect(),
        opendal: opendal.to_owned(),
        sized,
        text: text.to_owned(),
        next: None,
    };
    match (code, variant) {
        ("throttling", _) => r(
            Some("layers.concurrent_limit.http"),
            &["layers.retry.jitter"],
            "ConcurrentLimitLayer::with_http_concurrent_limit + RetryLayer::with_jitter",
            true,
            "cap the HTTP requests in flight below the service's limit and retry with jitter",
        ),
        ("rate_limit", _) => r(
            Some("layers.concurrent_limit.http"),
            &["layers.retry.jitter"],
            "ConcurrentLimitLayer::with_http_concurrent_limit + RetryLayer::with_jitter",
            true,
            "cap the HTTP requests in flight so that requests in flight ÷ latency stays under the service's rate",
        ),
        ("key_limit", _) => r(
            None,
            &[],
            "",
            false,
            "spread the hot keys over more keys or prefixes (salting, replicas); this is the workload's key layout, not a layer",
        ),
        ("bandwidth_cap", _) => r(
            Some("layers.throttle.bandwidth"),
            &[],
            "ThrottleLayer::new",
            true,
            "shape the bandwidth on the client below the shared cap instead of adding streams",
        ),
        ("tail_stragglers", _) => r(
            Some("layers.tail_cut"),
            &["layers.retry.min_delay"],
            "TailCutLayer + RetryLayer::with_min_delay",
            false,
            "re-issue requests far slower than their peers, after a short retry backoff",
        ),
        ("hung_requests", _) => r(
            Some("layers.timeout.io"),
            &[],
            "TimeoutLayer::with_io_timeout",
            true,
            "bound every I/O call with TimeoutLayer so a hung request is retried instead of waited on",
        ),
        ("small_ranges", Some("concurrent")) => r(
            Some("read.concurrent"),
            &["read.chunk"],
            "read_with().chunk().concurrent()",
            true,
            "split each large read into concurrent chunked range requests",
        ),
        ("small_ranges", _) => r(
            Some("read.gap"),
            &[],
            "read_with().gap()",
            true,
            "coalesce nearby small ranges into fewer range requests",
        ),
        ("slow_multipart", _) => r(
            Some("write.concurrent"),
            &["write.chunk"],
            "writer_with().chunk().concurrent()",
            true,
            "upload multipart parts concurrently",
        ),
        ("repeated_reads", _) => r(
            Some("layers.cache"),
            &[],
            "FoyerLayer (in-memory)",
            false,
            "serve re-read bytes from an in-memory FoyerLayer cache",
        ),
        ("cold_connections", Some("keep_alive")) => r(
            Some("http.keep_alive"),
            &[],
            "the HttpFetch client (keep-alive)",
            false,
            "turn keep-alive on so requests reuse their connections",
        ),
        ("cold_connections", _) => r(
            Some("http.version"),
            &[],
            "the HttpFetch client (HTTP/2)",
            false,
            "multiplex requests over fewer connections with HTTP/2",
        ),
        ("client_bound", _) => r(
            None,
            &[],
            "",
            false,
            "none in storage: the client process is the limit (more cores, less per-operation work)",
        ),
        ("errors", _) => r(
            None,
            &[],
            "",
            false,
            "fix the failing requests (credentials, permissions, endpoint, service health)",
        ),
        _ => r(
            None,
            &[],
            "",
            false,
            "fix what makes the run invalid, then profile again",
        ),
    }
}

/// Evidence values; every key carries its unit like a report field.
fn values(kv: &[(&str, f64)]) -> BTreeMap<String, f64> {
    for (k, v) in kv {
        debug_assert!(
            crate::model::has_unit(k),
            "evidence value `{k}` has no unit suffix"
        );
        debug_assert!(
            !k.ends_with("_count") || v.fract() == 0.0,
            "evidence count `{k}` = {v} is not a whole number"
        );
    }
    kv.iter()
        .filter(|(_, v)| v.is_finite())
        .map(|(k, v)| ((*k).to_owned(), *v))
        .collect()
}

/// Up to 3 time windows (merged seconds) where `pred` holds for a request.
fn windows_where(reqs: &[ReqRow], pred: impl Fn(&ReqRow) -> bool) -> Vec<TimeWindow> {
    let mut secs: Vec<i64> = reqs
        .iter()
        .filter(|q| q.t_us >= 0 && pred(q))
        .map(|q| q.t_us / 1_000_000)
        .collect();
    secs.sort_unstable();
    secs.dedup();
    let mut out: Vec<TimeWindow> = Vec::new();
    for s in secs {
        match out.last_mut() {
            Some(w) if w.to_s as i64 == s => w.to_s = (s + 1) as f64,
            _ => out.push(TimeWindow {
                from_s: s as f64,
                to_s: (s + 1) as f64,
            }),
        }
    }
    out.sort_by(|a, b| (b.to_s - b.from_s).total_cmp(&(a.to_s - a.from_s)));
    out.truncate(3);
    out.sort_by(|a, b| a.from_s.total_cmp(&b.from_s));
    out
}

/// The slowest of `ids` (at most 5, slowest first).
fn slowest_ops(run: &Run, ids: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let lat: HashMap<u64, u64> = run.ops.iter().map(|o| (o.op_id, o.latency_us)).collect();
    let mut v: Vec<(u64, u64)> = ids
        .into_iter()
        .filter(|id| *id != 0)
        .map(|id| (id, lat.get(&id).copied().unwrap_or(0)))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.dedup_by_key(|x| x.0);
    v.into_iter().take(5).map(|x| x.0).collect()
}

/// Where the time of the slowest operations went (T4): the real stage split of the slowest
/// 1 % of operations (at least 5), retry backoff included, as `(stage, share 0–1)` sorted by
/// share. Stages: `queue`, `connect`, `send`, `ttfb`, `body`, `backoff`, `client`.
pub fn tail_split(run: &Run) -> Vec<(&'static str, f64)> {
    let mut ops: Vec<&crate::model::OpRow> = run.ops.iter().filter(|o| o.t_us >= 0).collect();
    if ops.is_empty() {
        return Vec::new();
    }
    ops.sort_by_key(|o| std::cmp::Reverse(o.latency_us));
    let n = (ops.len() / 100).max(5).min(ops.len());
    let slow: HashMap<u64, &crate::model::OpRow> = ops[..n].iter().map(|o| (o.op_id, *o)).collect();
    let mut t: BTreeMap<&'static str, f64> = BTreeMap::new();
    for q in run.reqs.iter().filter(|q| slow.contains_key(&q.op_id)) {
        *t.entry("queue").or_default() += q.queue_us as f64;
        *t.entry("connect").or_default() += q.connect_us() as f64;
        *t.entry("send").or_default() += q.send_us as f64;
        *t.entry("ttfb").or_default() += q.ttfb_us as f64;
        *t.entry("body").or_default() += (q.body_us + q.stall_us) as f64;
    }
    for o in slow.values() {
        *t.entry("backoff").or_default() += o.breakdown.retry_us as f64;
        *t.entry("client").or_default() += (o.breakdown.client_us + o.breakdown.cache_us) as f64;
    }
    let total: f64 = t.values().sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let mut v: Vec<(&'static str, f64)> = t.into_iter().map(|(k, x)| (k, x / total)).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    v
}

struct Ctx<'a> {
    run: &'a Run,
    r: &'a Report,
    network: bool,
}

impl Ctx<'_> {
    fn spec(&self) -> &crate::operator::OperatorSpec {
        &self.r.spec.profile.operator
    }

    /// Requests in the measurement window.
    fn reqs(&self) -> impl Iterator<Item = &ReqRow> {
        self.run.reqs.iter().filter(|q| q.t_us >= 0)
    }

    fn reads(&self) -> bool {
        self.r
            .workload
            .mix
            .iter()
            .any(|m| m.weight > 0 && (m.op.contains("read") || m.op.contains("parquet")))
    }
}

/// The confidence of a limit model (T8): `high` only when it explains most of the effect
/// (≥ [`HIGH_EXPLAINS_RATIO`]) and the same model is selected on both halves of the window;
/// otherwise at most `medium` (or `low` when it explains less than half).
fn limit_confidence(c: &Ctx, h: &Hypothesis) -> Confidence {
    let mut conf = h.confidence;
    if h.explains_ratio < 0.5 {
        conf = conf.min(Confidence::Low);
    } else if h.explains_ratio < HIGH_EXPLAINS_RATIO {
        conf = conf.min(Confidence::Medium);
    }
    if conf == Confidence::High && !halves_agree(c, h.kind) {
        conf = Confidence::Medium;
    }
    conf
}

/// The limit model selected on each half of the window is `kind` (repeats within one run).
fn halves_agree(c: &Ctx, kind: HypothesisKind) -> bool {
    let w = c.r.run.window_s;
    if w <= 0.0 {
        return false;
    }
    let half = (w * 0.5 * 1e6) as i64;
    let end = (w * 1e6).ceil() as i64;
    let inputs = capacity::FitInputs {
        client_cpu_busy_ratio: Some(client_busy_ratio(c.r)),
        client_saturated: c.r.self_stats.client_saturated,
    };
    [(0, half), (half, end)].iter().all(|(a, b)| {
        let hs = capacity::fit(
            &c.run.reqs,
            &c.run.ops,
            capacity::Window {
                from_us: *a,
                to_us: *b,
            },
            &inputs,
        );
        super::hypothesis::selected(&hs).is_some_and(|s| s.kind == kind)
    })
}

/// Client CPU busy share of its cores.
pub fn client_busy_ratio(r: &Report) -> f64 {
    let p = &r.self_stats.process;
    r.self_stats
        .cpu_ratio
        .unwrap_or(p.cpu_pct / 100.0 / p.cpu_cores.max(1) as f64)
}

/// Failed operations by error class, most common first, without `rate_limited`.
fn failure_classes(r: &Report) -> Vec<(&str, u64)> {
    let mut v: Vec<(&str, u64)> = r
        .resilience
        .error_classes
        .iter()
        .filter(|(k, n)| k.as_str() != "rate_limited" && **n > 0)
        .map(|(k, n)| (k.as_str(), *n))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v
}

/// Operations that failed for a reason other than throttling (T10).
pub fn non_throttle_failures(r: &Report) -> u64 {
    let rate_limited = r
        .resilience
        .error_classes
        .get("rate_limited")
        .copied()
        .unwrap_or(0);
    r.summary.failed_ops.saturating_sub(rate_limited)
}

fn errors(c: &Ctx, random_throttle: bool) -> Option<Finding> {
    let r = c.r;
    let failed = non_throttle_failures(r);
    let server = r.resilience.server_errors;
    let requests = r.connections.requests.max(1);
    let thr = r.resilience.throttled_responses;
    // Random 503s count as service errors (A5): no limit to cap.
    let retried = server + if random_throttle { thr } else { 0 };
    if failed == 0 && (retried < 3 || (retried as f64) < requests as f64 * 0.001) {
        return None;
    }
    let classes = failure_classes(r);
    let by_class: Vec<String> = classes.iter().map(|(k, n)| format!("{k} ×{n}")).collect();
    let mut text = if failed > 0 {
        format!("{failed} of {} operations failed", r.summary.ops)
    } else {
        "every operation succeeded after retries".to_owned()
    };
    if !by_class.is_empty() {
        text += &format!(" ({})", by_class.join(", "));
    }
    if retried > 0 {
        text += &format!(
            "; {retried} of {requests} requests answered 5xx{}",
            if random_throttle {
                " — 503s that follow neither the load, the rate nor the keys"
            } else {
                ""
            }
        );
    }
    let error_ratio = failed as f64 / r.summary.ops.max(1) as f64;
    let mut rem = remedy("errors", None);
    if failed == 0 {
        // Retried 5xx: keep retries with jitter.
        rem.path = Some("layers.retry.jitter".into());
        rem.opendal = "RetryLayer::with_jitter".into();
        rem.text = "keep retrying with jitter and exponential backoff; slowing down would not remove errors that do not follow the load".into();
    }
    Some(Finding {
        code: "errors".into(),
        severity: if error_ratio > r.sla.max_error_ratio {
            Severity::Critical
        } else {
            Severity::Warning
        },
        confidence: Confidence::High,
        cause: if failed > 0 {
            format!(
                "{:.2}% of operations failed for reasons other than throttling.",
                error_ratio * 100.0
            )
        } else {
            format!(
                "{:.2}% of requests failed with a server error and were retried.",
                retried as f64 * 100.0 / requests as f64
            )
        },
        evidence: Evidence {
            text,
            values: values(&[
                ("failed_ops_count", failed as f64),
                ("error_ratio", error_ratio),
                ("server_errors_count", retried as f64),
            ]),
            op_ids: slowest_ops(c.run, c.run.ops.iter().filter(|o| !o.ok).map(|o| o.op_id)),
            windows: windows_where(&c.run.reqs, |q| {
                q.is_error() && q.status != 404 && (random_throttle || !q.is_throttle())
            }),
        },
        remedy: rem,
    })
}

/// `throttling`, `rate_limit` or `key_limit`, by the selected limit model. `None` (and
/// `true`) when the throttling is random: reported as errors.
fn throttle_family(c: &Ctx) -> (Option<Finding>, bool) {
    let r = c.r;
    let thr = r.resilience.throttled_responses;
    if thr == 0 {
        return (None, false);
    }
    let requests = c.r.connections.requests.max(1);
    let req_ratio = thr as f64 / requests as f64;
    let sel = r.capacity.selected().cloned();
    let kind = sel.as_ref().map(|h| h.kind);
    if kind == Some(HypothesisKind::None)
        && sel
            .as_ref()
            .is_some_and(|h| h.confidence >= Confidence::Medium)
    {
        return (None, true);
    }
    let (code, what) = match kind {
        Some(HypothesisKind::RateLimit | HypothesisKind::TimeVarying) => (
            "rate_limit",
            "the rejections follow the request rate, not the requests in flight",
        ),
        Some(HypothesisKind::KeyLimit) => (
            "key_limit",
            "the rejections follow a few hot keys; the other keys are served",
        ),
        Some(HypothesisKind::InflightLimit) => (
            "throttling",
            "the rejections start above a number of requests in flight",
        ),
        _ => (
            "throttling",
            "no limit model explains the rejections with confidence",
        ),
    };
    let mut confidence = sel
        .as_ref()
        .map_or(Confidence::Low, |h| limit_confidence(c, h));
    if matches!(
        kind,
        None | Some(HypothesisKind::None | HypothesisKind::ClientBound)
    ) {
        confidence = Confidence::Low;
    }
    let out_of_retries = r
        .resilience
        .error_classes
        .get("rate_limited")
        .copied()
        .unwrap_or(0);
    let mut text = format!(
        "{thr} throttled responses (429/503) in {requests} requests ({:.2}%); {:.0}% of operation time in retry backoff",
        req_ratio * 100.0,
        r.stages.shares.retry_pct
    );
    if out_of_retries > 0 {
        text += &format!("; {out_of_retries} operations failed when their retries ran out");
    }
    text += &format!("; {what}");
    if let Some(h) = sel.as_ref().filter(|h| {
        h.explains_ratio > 0.0
            && matches!(
                h.kind,
                HypothesisKind::InflightLimit
                    | HypothesisKind::RateLimit
                    | HypothesisKind::TimeVarying
                    | HypothesisKind::KeyLimit
            )
    }) {
        text += &format!(
            " (the {} explains {:.0}% of the throttled requests)",
            h.kind.label(),
            h.explains_ratio * 100.0
        );
    }
    let mut vals = vec![
        ("throttled_responses_count", thr as f64),
        ("throttled_request_ratio", req_ratio),
        ("retry_pct", r.stages.shares.retry_pct),
        ("rate_limited_failures_count", out_of_retries as f64),
    ];
    // The fitted limit is evidence (JSON), not advice: prove sizes the remedy.
    if let Some(h) = &sel {
        vals.push(("explains_ratio", h.explains_ratio));
        if let Some(n) = h.params.ceiling_inflight_count {
            vals.push(("fitted_inflight_count", n as f64));
        }
        if let Some(rps) = h.params.requests_per_s {
            vals.push(("fitted_requests_per_s", rps));
        }
        if let Some(s) = h.params.key_share_ratio {
            vals.push(("hot_key_share_ratio", s));
        }
    }
    let cause = match code {
        "rate_limit" => format!(
            "The service limits the request rate and throttled {:.1}% of requests.",
            req_ratio * 100.0
        ),
        "key_limit" => format!(
            "Hot keys exceed a per-key limit: {:.1}% of requests were throttled.",
            req_ratio * 100.0
        ),
        _ => format!(
            "The service throttled {:.1}% of requests (429/503).",
            req_ratio * 100.0
        ),
    };
    let ids = c.run.ops.iter().filter(|o| o.throttled).map(|o| o.op_id);
    (
        Some(Finding {
            code: code.into(),
            severity: if req_ratio >= 0.01 || out_of_retries > 0 {
                Severity::Critical
            } else {
                Severity::Warning
            },
            confidence,
            cause,
            evidence: Evidence {
                text,
                values: values(&vals),
                op_ids: slowest_ops(c.run, ids),
                windows: windows_where(&c.run.reqs, ReqRow::is_throttle),
            },
            remedy: remedy(code, None),
        }),
        false,
    )
}

fn bandwidth_cap(c: &Ctx) -> Option<Finding> {
    let h = c.r.capacity.asserted()?;
    if h.kind != HypothesisKind::BandwidthCap {
        return None;
    }
    let bps = h.params.bytes_per_s.unwrap_or(0.0);
    let body: f64 = c.reqs().map(|q| q.body_us as f64).sum();
    let total: f64 = c.reqs().map(|q| q.total_us as f64).sum::<f64>().max(1.0);
    Some(Finding {
        code: "bandwidth_cap".into(),
        severity: Severity::Warning,
        confidence: limit_confidence(c, h),
        cause: "Every response stream shares one bandwidth cap: more streams only split it.".into(),
        evidence: Evidence {
            text: format!(
                "{:.0}% of request time is the response body; a stream's rate falls as more run beside it, the aggregate stays flat at {:.1} MiB/s",
                body * 100.0 / total,
                c.r.summary.mib_per_s
            ),
            values: values(&[
                ("body_pct", body * 100.0 / total),
                ("fitted_bytes_per_s", bps),
                ("explains_ratio", h.explains_ratio),
            ]),
            op_ids: slowest_ops(c.run, c.reqs().map(|q| q.op_id)),
            windows: Vec::new(),
        },
        remedy: remedy("bandwidth_cap", None),
    })
}

/// A tail-cut setup with the re-issue backoff the remedy uses (a short retry delay), so the
/// detection does not depend on the current retry backoff (OpenDAL's default waits 1 s).
fn remedy_setup(r: &Report) -> TailCutSetup {
    let op = &r.spec.profile.operator;
    let params = op
        .layers
        .tail_cut
        .clone()
        .unwrap_or_default()
        .params(op.op_timeout());
    TailCutSetup {
        params,
        backoff_us: TAIL_CUT_REISSUE_US,
        blind_until_us: i64::MIN,
    }
}

/// The retry backoff prove pairs with `layers.tail_cut` (µs): a cut request is re-issued at
/// once, not after OpenDAL's default 1 s.
pub const TAIL_CUT_REISSUE_US: u64 = 20_000;

fn tail_stragglers(c: &Ctx) -> Option<Finding> {
    let run = c.run;
    if c.spec().layers.tail_cut.is_some() || run.reqs.len() < 50 {
        return None;
    }
    let mut lat: Vec<u64> = run.ops.iter().map(|o| o.latency_us).collect();
    let (p50, p99) = (
        quantile_us_ms(&mut lat, 0.5),
        quantile_us_ms(&mut lat, 0.99),
    );
    if p50 <= 0.0 || p99 < 3.0 * p50 {
        return None;
    }
    // T4: the requests themselves must wait for the first byte; a tail of retry backoff or
    // queueing has no stragglers to cut (the replay below only counts TTFB-bound requests).
    let setup = remedy_setup(c.r);
    let mut apis: Vec<&str> = run.reqs.iter().map(|q| q.api.as_ref()).collect();
    apis.sort_unstable();
    apis.dedup();
    let mut best: Option<(f64, String, Vec<&ReqRow>, simulate::TailCutReplay)> = None;
    for api in apis {
        let Some(replay) = simulate::tail_cut(run, api, &setup) else {
            continue;
        };
        let Some(deadline) = replay.deadline_us else {
            continue;
        };
        let over: Vec<&ReqRow> = run
            .reqs
            .iter()
            .filter(|q| q.api == api && !q.is_error() && q.total_us > deadline)
            .collect();
        let ttfb_bound = over.iter().filter(|q| dominant_stage(q) == "ttfb").count();
        if over.len() < 3 || ttfb_bound * 2 < over.len() {
            continue;
        }
        let p = &replay.prediction;
        let gain = p.current - p.predicted;
        if p.current <= 0.0 || gain < p.current / 3.0 {
            continue;
        }
        if best.as_ref().is_none_or(|b| gain > b.0) {
            best = Some((gain, api.to_owned(), over, replay));
        }
    }
    let (_, api, over, replay) = best?;
    let all = run
        .reqs
        .iter()
        .filter(|q| q.api == api && !q.is_error())
        .count();
    let share = over.len() as f64 * 100.0 / all.max(1) as f64;
    let mut ttfb: Vec<u64> = over.iter().map(|q| q.ttfb_us).collect();
    let wait = quantile_us_ms(&mut ttfb, 0.5);
    let mut typical: Vec<u64> = run
        .reqs
        .iter()
        .filter(|q| q.api == api && !q.is_error())
        .map(|q| q.ttfb_us)
        .collect();
    let typ = quantile_us_ms(&mut typical, 0.5);
    Some(Finding {
        code: "tail_stragglers".into(),
        severity: Severity::Warning,
        confidence: if over.len() >= 20 {
            Confidence::High
        } else {
            Confidence::Medium
        },
        cause: format!(
            "{share:.1}% of {api} requests wait far longer for the server's first byte than their peers (stragglers)."
        ),
        evidence: Evidence {
            text: format!(
                "{} stragglers wait ~{} for the first byte against ~{} for a typical {api}; operation p50 {} · p99 {}",
                over.len(),
                fmt_ms(wait),
                fmt_ms(typ),
                fmt_ms(p50),
                fmt_ms(p99)
            ),
            values: values(&[
                ("straggler_pct", share),
                ("straggler_ttfb_ms", wait),
                ("typical_ttfb_ms", typ),
                ("p50_ms", p50),
                ("p99_ms", p99),
                ("replayed_p99_ms", replay.prediction.predicted),
            ]),
            op_ids: slowest_ops(run, over.iter().map(|q| q.op_id)),
            windows: Vec::new(),
        },
        remedy: remedy("tail_stragglers", None),
    })
}

/// A request is hung when it takes this long and ≥ 20× the median request.
const HUNG_MIN_US: u64 = 2_000_000;

fn hung_requests(c: &Ctx) -> Option<Finding> {
    let mut totals: Vec<u64> = c.reqs().map(|q| q.total_us).collect();
    if totals.len() < 20 {
        return None;
    }
    let median_ms = quantile_us_ms(&mut totals, 0.5);
    let bound = HUNG_MIN_US.max((median_ms * 1000.0 * 20.0) as u64);
    let timeouts =
        c.r.resilience
            .error_classes
            .get("timeout")
            .copied()
            .unwrap_or(0);
    let hung: Vec<&ReqRow> = c
        .reqs()
        .filter(|q| {
            q.total_us >= bound && matches!(dominant_stage(q), "ttfb" | "body" | "stall" | "send")
        })
        .collect();
    if hung.is_empty() && timeouts == 0 {
        return None;
    }
    let has_io = c.spec().layers.timeout.is_some();
    let mut worst: Vec<u64> = hung.iter().map(|q| q.total_us).collect();
    let worst_ms = worst.iter().copied().max().unwrap_or(0) as f64 / 1000.0;
    worst.clear();
    Some(Finding {
        code: "hung_requests".into(),
        severity: Severity::Warning,
        confidence: if hung.len() >= 3 || timeouts > 0 {
            Confidence::High
        } else {
            Confidence::Medium
        },
        cause: format!(
            "{} requests hung far beyond the median{}.",
            hung.len().max(timeouts as usize),
            if has_io {
                " and the I/O timeout did not cut them early"
            } else {
                ""
            }
        ),
        evidence: Evidence {
            text: format!(
                "{} requests took ≥ {} (median request {}; the slowest {}){}",
                hung.len(),
                fmt_ms(bound as f64 / 1000.0),
                fmt_ms(median_ms),
                fmt_ms(worst_ms),
                if timeouts > 0 {
                    format!("; {timeouts} operations failed with a timeout")
                } else {
                    String::new()
                }
            ),
            values: values(&[
                ("hung_requests_count", hung.len() as f64),
                ("median_request_ms", median_ms),
                ("slowest_request_ms", worst_ms),
                ("timeouts_count", timeouts as f64),
            ]),
            op_ids: slowest_ops(c.run, hung.iter().map(|q| q.op_id)),
            windows: windows_where(&c.run.reqs, |q| q.total_us >= bound),
        },
        remedy: remedy("hung_requests", None),
    })
}

fn small_ranges(c: &Ctx, have_cap: bool) -> Option<Finding> {
    if !c.reads() {
        return None;
    }
    let reads: Vec<&ReqRow> = c
        .reqs()
        .filter(|q| q.api == "GetObject" && !q.is_error())
        .collect();
    if reads.len() < 5 {
        return None;
    }
    let ops = c.r.summary.ops.max(1) as f64;
    let read_ops = c
        .run
        .ops
        .iter()
        .filter(|o| o.t_us >= 0 && (o.verb.starts_with("read") || o.verb.contains("parquet")))
        .count()
        .max(1) as f64;
    let per_op = reads.len() as f64 / read_ops.min(ops).max(1.0);
    let bytes: f64 = reads.iter().map(|q| q.bytes_down as f64).sum();
    let mean = bytes / reads.len() as f64;
    let total: f64 = reads
        .iter()
        .map(|q| q.total_us as f64)
        .sum::<f64>()
        .max(1.0);
    let ttfb: f64 = reads.iter().map(|q| (q.send_us + q.ttfb_us) as f64).sum();
    let body: f64 = reads.iter().map(|q| q.body_us as f64).sum();
    let gap_set = c.spec().read.gap.is_some();
    let concurrent = c.spec().read_concurrent();
    // Several ranges per operation fetched one after another, each waiting for its first
    // byte: fetch them concurrently.
    if reads.len() >= 50 && per_op >= 3.0 && ttfb / total >= 0.5 && concurrent <= 1 {
        let mut rem = remedy("small_ranges", Some("concurrent"));
        rem.with.clear();
        return Some(Finding {
            code: "small_ranges".into(),
            severity: Severity::Warning,
            confidence: Confidence::Medium,
            cause:
                "Each read fetches several ranges one after another, each paying a full round trip."
                    .into(),
            evidence: Evidence {
                text: format!(
                    "{per_op:.1} range requests per read operation, {} on average, fetched sequentially (read.concurrent is 1); {:.0}% of their time waits for the first byte",
                    crate::util::fmt_bytes(mean as u64),
                    ttfb * 100.0 / total
                ),
                values: values(&[
                    ("requests_per_op_mean", per_op),
                    ("mean_request_bytes", mean),
                    ("ttfb_pct", ttfb * 100.0 / total),
                ]),
                ..Default::default()
            },
            remedy: rem,
        });
    }
    // Many small ranges per operation, waiting for the first byte: coalesce.
    if reads.len() >= 50
        && per_op >= 3.0
        && mean < 256.0 * 1024.0
        && ttfb / total >= 0.5
        && !gap_set
    {
        return Some(Finding {
            code: "small_ranges".into(),
            severity: Severity::Warning,
            confidence: Confidence::Medium,
            cause: "Each read is many small range requests, each paying a full round trip.".into(),
            evidence: Evidence {
                text: format!(
                    "{per_op:.1} range requests per read operation, {} on average; {:.0}% of their time waits for the first byte",
                    crate::util::fmt_bytes(mean as u64),
                    ttfb * 100.0 / total
                ),
                values: values(&[
                    ("requests_per_op_mean", per_op),
                    ("mean_request_bytes", mean),
                    ("ttfb_pct", ttfb * 100.0 / total),
                ]),
                ..Default::default()
            },
            remedy: remedy("small_ranges", Some("gap")),
        });
    }
    // Large reads streamed through one request each: split them.
    if !have_cap && concurrent <= 1 && mean >= 1024.0 * 1024.0 && body / total >= 0.5 {
        let mut rates: Vec<f64> = reads
            .iter()
            .filter(|q| q.body_us > 0)
            .map(|q| q.bytes_down as f64 / (q.body_us as f64 / 1e6))
            .collect();
        rates.sort_by(f64::total_cmp);
        let per_stream = super::stats::median(&rates);
        return Some(Finding {
            code: "small_ranges".into(),
            severity: Severity::Warning,
            confidence: Confidence::Medium,
            cause: "Each large read streams through one request at one stream's rate.".into(),
            evidence: Evidence {
                text: format!(
                    "reads of {} on average spend {:.0}% of their time on the body at ~{:.1} MiB/s per stream; reads are not split (read.concurrent is 1)",
                    crate::util::fmt_bytes(mean as u64),
                    body * 100.0 / total,
                    per_stream / (1024.0 * 1024.0)
                ),
                values: values(&[
                    ("mean_request_bytes", mean),
                    ("body_pct", body * 100.0 / total),
                    ("stream_bytes_per_s", per_stream),
                ]),
                ..Default::default()
            },
            remedy: remedy("small_ranges", Some("concurrent")),
        });
    }
    None
}

fn slow_multipart(c: &Ctx) -> Option<Finding> {
    let parts: Vec<&ReqRow> = c
        .reqs()
        .filter(|q| q.api == "UploadPart" && !q.is_error())
        .collect();
    if parts.len() < 10 || c.spec().write_concurrent() > 1 {
        return None;
    }
    let writes: Vec<&crate::model::OpRow> = c
        .run
        .ops
        .iter()
        .filter(|o| o.t_us >= 0 && o.verb.contains("multipart"))
        .collect();
    if writes.is_empty() {
        return None;
    }
    let per_op = parts.len() as f64 / writes.len() as f64;
    if per_op < 2.0 {
        return None;
    }
    let up: f64 = parts.iter().map(|q| q.send_us as f64).sum();
    let total: f64 = parts
        .iter()
        .map(|q| q.total_us as f64)
        .sum::<f64>()
        .max(1.0);
    Some(Finding {
        code: "slow_multipart".into(),
        severity: Severity::Warning,
        confidence: Confidence::Medium,
        cause: "Multipart writes upload their parts one after another.".into(),
        evidence: Evidence {
            text: format!(
                "{per_op:.1} parts per write uploaded sequentially (write.concurrent is 1); {:.0}% of part time is the upload",
                up * 100.0 / total
            ),
            values: values(&[
                ("parts_per_op_mean", per_op),
                ("upload_pct", up * 100.0 / total),
            ]),
            op_ids: slowest_ops(c.run, writes.iter().map(|o| o.op_id)),
            windows: Vec::new(),
        },
        remedy: remedy("slow_multipart", None),
    })
}

fn repeated_reads(c: &Ctx) -> Option<Finding> {
    if c.spec().layers.cache.is_some() || !c.reads() {
        return None;
    }
    // A generated dataset read uniformly says nothing about real reuse (A15): only real
    // objects, or a stated skew, count.
    let ds = &c.r.workload.dataset;
    if ds.existing_prefix.is_none() && c.r.workload.access == "uniform" {
        return None;
    }
    let p = simulate::cache(c.run, 256 << 20)?;
    if p.requests_per_1k_ops_delta > -300.0 {
        return None;
    }
    Some(Finding {
        code: "repeated_reads".into(),
        severity: Severity::Info,
        confidence: Confidence::Medium,
        cause: "The same bytes are read again and again.".into(),
        evidence: Evidence {
            text: format!(
                "replaying the reads through an in-memory cache would save {:.0} of every 1 000 read requests",
                -p.requests_per_1k_ops_delta
            ),
            values: values(&[(
                "saved_requests_per_1k_ops_mean",
                -p.requests_per_1k_ops_delta,
            )]),
            ..Default::default()
        },
        remedy: remedy("repeated_reads", None),
    })
}

fn cold_connections(c: &Ctx) -> Option<Finding> {
    if !c.network {
        return None;
    }
    // Dials during warmup count too (T11): rows before the window are kept for this.
    let reqs = &c.run.reqs;
    let cold: Vec<&ReqRow> = reqs
        .iter()
        .filter(|q| !q.reused && q.connect_us() > 0)
        .collect();
    if reqs.len() < 20 || cold.is_empty() {
        return None;
    }
    let total: u64 = reqs.iter().map(|q| q.total_us).sum();
    let connect: u64 = cold.iter().map(|q| q.connect_us()).sum();
    let share = connect as f64 * 100.0 / total.max(1) as f64;
    let reuse = (reqs.len() - cold.len()) as f64 * 100.0 / reqs.len() as f64;
    if share < 10.0 && !(reuse < 80.0 && reqs.len() > 50) {
        return None;
    }
    let spec = c.spec();
    let variant = if !spec.http.keep_alive {
        "keep_alive"
    } else if spec.http.version == crate::net::HttpVersion::H2 {
        return None;
    } else {
        "version"
    };
    let p50 = |f: &dyn Fn(&ReqRow) -> u64| {
        let mut v: Vec<u64> = cold.iter().map(|q| f(q)).filter(|x| *x > 0).collect();
        quantile_us_ms(&mut v, 0.5)
    };
    let (dns, tcp, tls) = (p50(&|q| q.dns_us), p50(&|q| q.tcp_us), p50(&|q| q.tls_us));
    Some(Finding {
        code: "cold_connections".into(),
        severity: if share >= 10.0 {
            Severity::Warning
        } else {
            Severity::Info
        },
        confidence: Confidence::High,
        cause: format!(
            "Connection setup takes {share:.0}% of request time ({} new connections).",
            cold.len()
        ),
        evidence: Evidence {
            text: format!(
                "{} of {} requests opened a connection (reuse {reuse:.0}%, warmup included); DNS p50 {} · TCP p50 {} · TLS p50 {}",
                cold.len(),
                reqs.len(),
                fmt_ms(dns),
                fmt_ms(tcp),
                fmt_ms(tls)
            ),
            values: values(&[
                ("connect_pct", share),
                ("cold_requests_count", cold.len() as f64),
                ("reuse_pct", reuse),
                ("dns_p50_ms", dns),
                ("tcp_p50_ms", tcp),
                ("tls_p50_ms", tls),
            ]),
            op_ids: slowest_ops(c.run, cold.iter().map(|q| q.op_id)),
            windows: windows_where(reqs, |q| !q.reused && q.connect_us() > 0),
        },
        remedy: remedy("cold_connections", Some(variant)),
    })
}

/// `client_bound`: the client's CPU or its scheduler set the pace (A9, T7), or an open-loop
/// schedule fell behind.
fn client_bound(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let cpu = client_busy_ratio(r);
    let lag = r.self_stats.sched_lag_p99_ms.unwrap_or(0.0);
    let saturated = r.self_stats.client_saturated;
    let cpu_bound = cpu >= super::limits::CLIENT_BOUND_RATIO;
    let backlog = r.open_loop.as_ref().filter(|ol| {
        ol.intended_count > 0
            && ol.missed as f64 / ol.intended_count as f64 > crate::model::SCHEDULE_BACKLOG_RATIO
    });
    if !cpu_bound && !saturated && backlog.is_none() {
        return None;
    }
    let mut text = format!(
        "client CPU {:.0}% of its cores; scheduling lag p99 {}",
        cpu * 100.0,
        fmt_ms(lag)
    );
    if let Some(ol) = backlog {
        text += &format!(
            "; {} of {} scheduled arrivals never started (they count in the latency tail as lower bounds)",
            ol.missed, ol.intended_count
        );
    }
    Some(Finding {
        code: "client_bound".into(),
        severity: Severity::Warning,
        confidence: if cpu >= 0.95 || lag > 20.0 {
            Confidence::High
        } else {
            Confidence::Medium
        },
        cause: if backlog.is_some() && !cpu_bound && !saturated {
            "The client fell behind its arrival schedule: throughput is the client's, not the service's.".into()
        } else {
            "The client process, not the service, set the pace: its numbers understate the service."
                .into()
        },
        evidence: Evidence {
            text,
            values: values(&[
                ("client_cpu_ratio", cpu),
                ("sched_lag_p99_ms", lag),
                (
                    "missed_arrivals_count",
                    backlog.map_or(0.0, |ol| ol.missed as f64),
                ),
            ]),
            ..Default::default()
        },
        remedy: remedy("client_bound", None),
    })
}

/// How many times the client's scheduling lag (p99) a latency finding's excess delay must be
/// to survive a saturated client.
const LAG_DWARF_FACTOR: f64 = 10.0;

/// The delay a latency finding attributes to the server, over its typical request: what a
/// saturated client's scheduling lag would have to explain. `None` for other findings.
fn excess_latency_ms(f: &Finding) -> Option<f64> {
    let v = |k: &str| f.evidence.values.get(k).copied();
    match f.code.as_str() {
        "tail_stragglers" => Some(v("straggler_ttfb_ms")? - v("typical_ttfb_ms")?),
        "hung_requests" => Some(v("slowest_request_ms")? - v("median_request_ms")?),
        _ => None,
    }
}

/// All findings of a run, one per code, most severe first, with each remedy's next command.
pub fn derive(run: &Run) -> Vec<Finding> {
    let r = &run.report;
    let c = Ctx {
        run,
        r,
        network: r.target.scheme != "memory" && r.target.scheme != "fs",
    };
    let mut out: Vec<Finding> = Vec::new();
    let client = client_bound(&c);
    let (thr, random) = throttle_family(&c);
    out.extend(errors(&c, random));
    out.extend(thr);
    let cap = bandwidth_cap(&c);
    let have_cap = cap.is_some();
    out.extend(cap);
    out.extend(hung_requests(&c));
    out.extend(tail_stragglers(&c));
    out.extend(small_ranges(&c, have_cap));
    out.extend(slow_multipart(&c));
    out.extend(cold_connections(&c));
    out.extend(repeated_reads(&c));
    // A saturated client makes latency findings unreliable (T7): it leads, and the findings
    // that read the server's timing are dropped — unless the delay they report dwarfs what the
    // client's own scheduling lag can add (a 300 ms straggler is not a 7 ms poll delay).
    if let Some(cb) = client {
        let lag_ms = r.self_stats.sched_lag_p99_ms.unwrap_or(0.0);
        out.retain(|f| {
            matches!(
                f.code.as_str(),
                "errors" | "throttling" | "rate_limit" | "key_limit"
            ) || excess_latency_ms(f).is_some_and(|ms| ms > LAG_DWARF_FACTOR * lag_ms.max(1.0))
        });
        out.insert(0, cb);
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    for f in &mut out {
        f.remedy.next = Some(next_command(&r.run.id, f));
    }
    let (kept, _) = validity(r, out);
    kept
}

/// The next command of a finding: `oprobe prove RUN CODE` when the remedy is a change of the
/// operator (key_limit proves the client-side cap as a stopgap), else where to look.
pub fn next_command(run_id: &str, f: &Finding) -> String {
    match f.code.as_str() {
        "errors" if f.remedy.path.is_none() => format!("oprobe show {run_id} --by api"),
        "client_bound" | "invalid_run" => format!("oprobe show {run_id} --detail"),
        code => format!("oprobe prove {run_id} {code}"),
    }
}

/// Why a run cannot be judged on performance: it is invalid (no operation completed, more
/// failed than the SLA allows for reasons other than throttling — T10), or it is partial and
/// too short to judge.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Validity {
    pub invalid: Option<String>,
    /// Performance findings are suppressed.
    pub suppress: bool,
}

/// The run's [`Validity`].
pub fn run_validity(r: &Report) -> Validity {
    let s = &r.summary;
    let window = r.run.window_s;
    let failed = non_throttle_failures(r);
    let error_ratio = failed as f64 / s.ops.max(1) as f64;
    let invalid = if s.ops == 0 {
        Some("no operation completed in the measurement window".to_owned())
    } else if error_ratio > r.sla.max_error_ratio {
        Some(format!(
            "{:.1}% of operations failed for reasons other than throttling (the SLA allows {}%), so latency and throughput describe failures",
            error_ratio * 100.0,
            r.sla.max_error_ratio * 100.0
        ))
    } else if r.workload.warmup_s > 0.0 && window < r.workload.warmup_s {
        Some(format!(
            "the measurement window ({window:.1} s) is shorter than the warmup ({:.1} s)",
            r.workload.warmup_s
        ))
    } else {
        None
    };
    let too_short = window < r.workload.warmup_s.max(1.0);
    let suppress = invalid.is_some()
        || r.run
            .partial
            .as_ref()
            .is_some_and(|p| p.reason != crate::model::PartialReason::Budget || too_short);
    Validity { invalid, suppress }
}

/// Apply the run's [`Validity`] to `findings`: performance findings are dropped when the run
/// cannot be judged, and `invalid_run` says why. Idempotent.
pub fn validity(r: &Report, findings: Vec<Finding>) -> (Vec<Finding>, Validity) {
    let v = run_validity(r);
    let mut out: Vec<Finding> = findings
        .into_iter()
        .filter(|f| f.code != "invalid_run")
        .filter(|f| !v.suppress || VALID_ON_BROKEN_RUNS.contains(&f.code.as_str()))
        .collect();
    if let Some(why) = &v.invalid {
        let mut rem = remedy("invalid_run", None);
        rem.next = Some(format!("oprobe show {} --detail", r.run.id));
        out.push(Finding {
            code: "invalid_run".into(),
            severity: Severity::Warning,
            confidence: Confidence::High,
            cause: format!("The run cannot be judged on performance: {why}."),
            evidence: Evidence {
                text: format!(
                    "{} operations, {} failed ({} throttled out of retries), {:.1} s measured after a {:.1} s warmup",
                    r.summary.ops,
                    r.summary.failed_ops,
                    r.summary.failed_ops - non_throttle_failures(r),
                    r.run.window_s,
                    r.workload.warmup_s
                ),
                values: values(&[
                    ("error_ratio", r.summary.error_rate),
                    ("window_s", r.run.window_s),
                ]),
                ..Default::default()
            },
            remedy: rem,
        });
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    (out, v)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;

    /// A complete, empty report as JSON (every field present).
    pub(crate) fn empty_report_json() -> serde_json::Value {
        let r = crate::model::Report {
            schema: crate::model::REPORT_SCHEMA.into(),
            run: Default::default(),
            spec: Default::default(),
            capacity: Default::default(),
            target: crate::model::TargetInfo {
                scheme: "s3".into(),
                display: "s3://bkt/x".into(),
                ..Default::default()
            },
            workload: crate::model::WorkloadInfo {
                preset: "range-reads".into(),
                duration_s: 5.0,
                ..Default::default()
            },
            effective_write: Default::default(),
            sla: Default::default(),
            verdict: Default::default(),
            summary: Default::default(),
            stages: Default::default(),
            apis: vec![],
            verbs: vec![],
            connections: Default::default(),
            network: Default::default(),
            resilience: Default::default(),
            bytes: Default::default(),
            cache: Default::default(),
            integrity: Default::default(),
            self_stats: Default::default(),
            schedule: Default::default(),
            dimensions: Default::default(),
            sampling: Default::default(),
            open_loop: None,
            timeline: vec![],
            heatmap: Default::default(),
            findings: vec![],
            slowest: vec![],
            cleanup: Default::default(),
            warnings: vec![],
        };
        serde_json::to_value(r).unwrap()
    }

    fn base() -> Run {
        let mut run = straggler_run();
        run.report.run.id = "r1".into();
        run.report.run.window_s = 5.0;
        run.report.sla.max_error_ratio = 0.01;
        run.report.connections.requests = 1000;
        run
    }

    #[test]
    fn stragglers_name_tail_cut_and_no_number() {
        let run = base();
        let f = derive(&run);
        let t = f
            .iter()
            .find(|x| x.code == "tail_stragglers")
            .unwrap_or_else(|| panic!("{f:#?}"));
        assert_eq!(t.remedy.path.as_deref(), Some("layers.tail_cut"));
        assert_eq!(
            t.remedy.next.as_deref(),
            Some("oprobe prove r1 tail_stragglers")
        );
        assert!(!t.evidence.op_ids.is_empty());
        assert!(!t.remedy.text.chars().any(|c| c.is_ascii_digit()), "{t:#?}");
    }

    #[test]
    fn stragglers_are_found_with_opendal_default_retry() {
        // OpenDAL's default backoff (1 s, no jitter) would make a cut request slower: the
        // detection uses the remedy's own short backoff.
        let mut run = base();
        run.report.spec.profile.operator.layers.retry = Some(crate::operator::Retry::default());
        assert!(derive(&run).iter().any(|x| x.code == "tail_stragglers"));
    }

    #[test]
    fn a_backoff_tail_is_not_stragglers() {
        let mut run = base();
        for o in run.ops.iter_mut().filter(|o| o.latency_us > 500_000) {
            o.breakdown.retry_us = o.latency_us;
        }
        for q in run.reqs.iter_mut().filter(|q| q.total_us > 500_000) {
            q.total_us = 10_000;
            q.ttfb_us = 9_990;
        }
        assert!(!derive(&run).iter().any(|x| x.code == "tail_stragglers"));
        let split = tail_split(&run);
        assert_eq!(split[0].0, "backoff", "{split:?}");
    }

    fn throttled(kind: HypothesisKind, explains: f64, conf: Confidence) -> Run {
        let mut run = base();
        run.report.resilience.throttled_responses = 100;
        run.report.capacity.hypotheses = vec![Hypothesis {
            kind,
            explains_ratio: explains,
            confidence: conf,
            selected: true,
            params: super::super::hypothesis::HypothesisParams {
                ceiling_inflight_count: Some(24),
                ..Default::default()
            },
            ..Default::default()
        }];
        run
    }

    #[test]
    fn throttling_names_the_concurrent_limit_and_its_model() {
        let run = throttled(HypothesisKind::InflightLimit, 0.95, Confidence::High);
        let f = derive(&run);
        assert_eq!(f[0].code, "throttling", "{f:#?}");
        assert_eq!(
            f[0].remedy.path.as_deref(),
            Some("layers.concurrent_limit.http")
        );
        assert_eq!(f[0].remedy.with, vec!["layers.retry.jitter"]);
        assert!(f[0].remedy.sized);
        assert_eq!(f[0].evidence.values["fitted_inflight_count"], 24.0);
        assert!(!f[0].cause.contains("24"));
    }

    #[test]
    fn high_confidence_needs_the_model_to_explain_most_of_it() {
        let run = throttled(HypothesisKind::InflightLimit, 0.45, Confidence::High);
        assert_eq!(derive(&run)[0].confidence, Confidence::Low);
        let run = throttled(HypothesisKind::InflightLimit, 0.6, Confidence::High);
        assert_eq!(derive(&run)[0].confidence, Confidence::Medium);
    }

    #[test]
    fn rate_and_key_limits_get_their_own_codes() {
        let run = throttled(HypothesisKind::RateLimit, 0.9, Confidence::High);
        assert_eq!(derive(&run)[0].code, "rate_limit");
        let run = throttled(HypothesisKind::KeyLimit, 0.9, Confidence::High);
        let f = derive(&run);
        assert_eq!(f[0].code, "key_limit");
        assert!(f[0].remedy.path.is_none());
    }

    #[test]
    fn random_throttling_is_errors_not_a_limit() {
        let mut run = throttled(HypothesisKind::None, 0.0, Confidence::Medium);
        run.report.resilience.server_errors = 20;
        let f = derive(&run);
        assert!(!f.iter().any(|x| x.code == "throttling"), "{f:#?}");
        let e = f.iter().find(|x| x.code == "errors").unwrap();
        assert_eq!(e.remedy.path.as_deref(), Some("layers.retry.jitter"));
    }

    #[test]
    fn throttle_driven_failures_keep_the_run_valid() {
        let mut run = throttled(HypothesisKind::InflightLimit, 0.9, Confidence::High);
        run.report.summary.failed_ops = 50;
        run.report.summary.error_rate = 0.05;
        run.report
            .resilience
            .error_classes
            .insert("rate_limited".into(), 50);
        let f = derive(&run);
        assert!(!f.iter().any(|x| x.code == "invalid_run"), "{f:#?}");
        assert_eq!(f[0].code, "throttling");
        // Other failures do make it invalid.
        run.report
            .resilience
            .error_classes
            .insert("rate_limited".into(), 0);
        run.report
            .resilience
            .error_classes
            .insert("permission".into(), 50);
        let f = derive(&run);
        assert!(f.iter().any(|x| x.code == "invalid_run"));
        assert!(f.iter().any(|x| x.code == "throttling"));
        assert!(!f.iter().any(|x| x.code == "tail_stragglers"));
    }

    #[test]
    fn a_saturated_client_leads_and_drops_latency_findings() {
        let mut run = base();
        run.report.self_stats.client_saturated = true;
        // A lag as long as the stragglers themselves: it could explain them.
        run.report.self_stats.sched_lag_p99_ms = Some(1000.0);
        let f = derive(&run);
        assert_eq!(f[0].code, "client_bound", "{f:#?}");
        assert!(f[0].remedy.path.is_none());
        assert!(!f.iter().any(|x| x.code == "tail_stragglers"));
    }

    #[test]
    fn stragglers_that_dwarf_the_clients_lag_survive_a_saturated_client() {
        let mut run = base();
        let straggler = derive(&run)
            .into_iter()
            .find(|x| x.code == "tail_stragglers")
            .expect("the base run has stragglers");
        let excess = excess_latency_ms(&straggler).unwrap();
        // A small scheduling lag (a busy CI machine) cannot explain them: they stay.
        run.report.self_stats.client_saturated = true;
        run.report.self_stats.sched_lag_p99_ms = Some(excess / (LAG_DWARF_FACTOR * 2.0));
        let f = derive(&run);
        assert_eq!(f[0].code, "client_bound", "{f:#?}");
        assert!(f.iter().any(|x| x.code == "tail_stragglers"), "{f:#?}");
        // A lag of the same order could: they go.
        run.report.self_stats.sched_lag_p99_ms = Some(excess);
        assert!(!derive(&run).iter().any(|x| x.code == "tail_stragglers"));
    }

    #[test]
    fn every_code_has_a_remedy_and_evidence_counts_are_whole() {
        for c in CODES {
            let r = remedy(c, None);
            assert!(!r.text.is_empty(), "{c}");
            let words = r.text.replace("HTTP/2", "HTTP");
            assert!(
                !words.chars().any(|ch| ch.is_ascii_digit()),
                "{c}: {}",
                r.text
            );
        }
        let run = throttled(HypothesisKind::InflightLimit, 0.9, Confidence::High);
        for f in derive(&run) {
            for (k, v) in &f.evidence.values {
                assert!(!k.ends_with("_count") || v.fract() == 0.0, "{k}={v}");
            }
        }
    }
}
