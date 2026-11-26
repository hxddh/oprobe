//! Findings (design v6 §3 "Explain", §8 "Findings → remedies"): each names a cause, shows the
//! evidence (the requests themselves) and names **one** OpenDAL remedy as an `OperatorSpec`
//! path. A finding carries no sizing number: one run varies no load, so it cannot size a
//! change (T1, T2); `oprobe prove RUN CODE` sizes and tests the remedy.
//!
//! | code | remedy path |
//! |---|---|
//! | `throttling` | `layers.concurrent_limit.http` (sized) + `layers.retry.jitter`: the service rejects (429/503) above a number of requests in flight, or queues them without a rejection |
//! | `rate_limit` | `layers.concurrent_limit.http` (sized from rate × latency) |
//! | `key_limit` | the workload's key layout (advice, not a layer) |
//! | `bandwidth_cap` | `layers.throttle.bandwidth` on writes (a capped uplink, or a capped link the workload also writes over); on reads not a layer |
//! | `tail_stragglers` | `layers.tail_cut` |
//! | `hung_requests` | `layers.timeout.io` |
//! | `small_ranges` | `read.gap` / `read.concurrent` |
//! | `slow_multipart` | `write.concurrent` (with `write.chunk`) |
//! | `repeated_reads` | `layers.cache` |
//! | `cold_connections` | `http.keep_alive` / `http.version` |
//! | `client_bound` | none (reported) |
//!
//! Plus `errors` (failures that are not throttling: `layers.retry.min_delay` (sized) +
//! `layers.retry.jitter` when retries recovered every operation, else not a layer) and
//! `invalid_run` (the run cannot be judged). One finding per code, most severe first.

use std::collections::{BTreeMap, HashMap};

use super::capacity::{self, Confidence};
use super::diagnosis::Severity;
use super::hypothesis::{Hypothesis, HypothesisKind};
use super::outliers::dominant_stage;
use super::simulate::{self, TailCutSetup};
use super::stats::weighted_quantile_us_ms;
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
        ("throttling", Some("queueing")) => r(
            Some("layers.concurrent_limit.http"),
            &["layers.retry.jitter"],
            "ConcurrentLimitLayer::with_http_concurrent_limit + RetryLayer::with_jitter",
            true,
            "cap the HTTP requests in flight at what the service serves at once: the requests beyond it only wait in its queue",
        ),
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
        ("bandwidth_cap", Some("reads")) => r(
            None,
            &[],
            "",
            false,
            "the cap is not a layer: OpenDAL's ThrottleLayer shapes writes only, so on reads keep the streams to what the cap carries, or raise the cap itself (a wider link, another endpoint)",
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
            "reader_with().gap()",
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
        ("client_bound", Some("workers")) => r(
            None,
            &[],
            "",
            false,
            "none in storage: the workers, not the client's CPU, are the bound; raise the workers (`-c`) or lower `--rate`",
        ),
        ("client_bound", _) => r(
            None,
            &[],
            "",
            false,
            "none in storage: the client process is the limit (more cores, less per-operation work)",
        ),
        ("errors", Some("retried")) => r(
            Some("layers.retry.min_delay"),
            &["layers.retry.jitter"],
            "RetryLayer::with_min_delay + RetryLayer::with_jitter",
            true,
            "retry transient failures after a short first backoff, with jitter: they do not follow the load, so a long backoff only adds latency",
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
    // Compared as printed (whole percent): a ratio a rounding error away from a threshold
    // never flips the confidence between runs of the same rows (U9).
    let explains = (h.explains_ratio * 100.0).round() / 100.0;
    if explains < 0.5 {
        conf = conf.min(Confidence::Low);
    } else if explains < HIGH_EXPLAINS_RATIO {
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

/// Fewer than this many failures (or under [`MIN_ERROR_SHARE`] of their population) raise no
/// `errors` finding: one 404 is not a finding.
const MIN_ERRORS: u64 = 3;
/// The smallest share of requests (or, in a production trace, of operations) whose failures
/// raise an `errors` finding.
const MIN_ERROR_SHARE: f64 = 0.001;

/// `n` failures of `of` are too few to report.
fn too_few_errors(n: u64, of: u64) -> bool {
    n < MIN_ERRORS || (n as f64) < of.max(1) as f64 * MIN_ERROR_SHARE
}

fn errors(c: &Ctx, random_throttle: bool) -> Option<Finding> {
    let r = c.r;
    let failed = non_throttle_failures(r);
    let server = r.resilience.server_errors;
    // Requests that got no response (connection resets, refused or dropped connections).
    let network = r.resilience.network_errors;
    let requests = r.connections.requests.max(1);
    let thr = r.resilience.throttled_responses;
    // Random 503s count as service errors (A5): no limit to cap.
    let random = if random_throttle { thr } else { 0 };
    let retried = server + network + random;
    // A production trace's failed operations meet the same minimum share as a run's retried
    // requests (K14): an application that stats a missing key fails one operation.
    let trace = r.run.source == crate::model::RunSource::Trace;
    let failed_counts = failed > 0 && !(trace && too_few_errors(failed, r.summary.ops));
    if !failed_counts && too_few_errors(retried, requests) {
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
    if server + random > 0 {
        text += &format!(
            "; {} of {requests} requests answered 5xx{}",
            server + random,
            if random_throttle {
                " — 503s that follow neither the load, the rate nor the keys"
            } else {
                ""
            }
        );
    }
    if network > 0 {
        text +=
            &format!("; {network} of {requests} requests lost their connection before a response");
    }
    if failed == 0 && retried > 0 {
        text += &format!(
            "; {:.0}% of operation time in retry backoff",
            r.stages.shares.retry_pct
        );
    }
    let error_ratio = failed as f64 / r.summary.ops.max(1) as f64;
    // Every operation succeeded after retries: the failures are transient, and what they cost
    // is the retry backoff (OpenDAL waits 1 s first). The remedy is a shorter first backoff,
    // with jitter, sized by prove (K1).
    let rem = remedy("errors", (failed == 0).then_some("retried"));
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
                "{:.2}% of requests failed with a server error or a lost connection and were retried: the time goes to the retry backoff.",
                retried as f64 * 100.0 / requests as f64
            )
        },
        evidence: Evidence {
            text,
            values: values(&[
                ("failed_ops_count", failed as f64),
                ("error_ratio", error_ratio),
                ("server_errors_count", (server + random) as f64),
                ("network_errors_count", network as f64),
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
    let backoff_share = r.stages.shares.retry_pct / 100.0;
    let failed_ratio = out_of_retries as f64 / r.summary.ops.max(1) as f64;
    let mut vals = vec![
        ("throttled_responses_count", thr as f64),
        ("throttled_request_ratio", req_ratio),
        ("retry_pct", r.stages.shares.retry_pct),
        ("rate_limited_failures_count", out_of_retries as f64),
        (COST_KEY, backoff_share),
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
    // The cause leads with what the throttling costs (U4): time in retry backoff, then
    // operations that failed; the share of throttled requests alone understates it.
    let what = match code {
        "rate_limit" => format!(
            "the service limits the request rate and throttled {:.1}% of requests",
            req_ratio * 100.0
        ),
        "key_limit" => format!(
            "hot keys exceed a per-key limit ({:.1}% of requests throttled)",
            req_ratio * 100.0
        ),
        _ => format!(
            "the service throttled {:.1}% of requests",
            req_ratio * 100.0
        ),
    };
    let failed = match out_of_retries {
        0 => String::new(),
        1 => "; 1 operation failed when its retries ran out".to_owned(),
        n => format!("; {n} operations failed when their retries ran out"),
    };
    let cause = if r.stages.shares.retry_pct >= 0.5 {
        format!(
            "{:.0}% of operation time is spent in retry backoff after 429/503: {what}{failed}.",
            r.stages.shares.retry_pct
        )
    } else {
        let mut s = what;
        if let Some(c) = s.get_mut(..1) {
            c.make_ascii_uppercase();
        }
        if failed.is_empty() {
            format!("{s} (429/503); retries absorbed them at under 1% of operation time.")
        } else {
            format!("{s} (429/503){failed}.")
        }
    };
    let ids = c.run.ops.iter().filter(|o| o.throttled).map(|o| o.op_id);
    (
        Some(Finding {
            code: code.into(),
            severity: throttle_severity(failed_ratio, backoff_share, r.sla.max_error_ratio),
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

/// Evidence value of the share of operation time a finding costs (retry backoff for the
/// throttle family, the stragglers' wait beyond a typical request for `tail_stragglers`): it
/// orders findings of one severity.
pub const COST_KEY: &str = "time_cost_ratio";

/// Backoff share of operation time from which throttling is critical.
const CRITICAL_BACKOFF_RATIO: f64 = 0.5;
/// Backoff share of operation time from which throttling is worth a warning.
const WARNING_BACKOFF_RATIO: f64 = 0.05;

/// The severity of throttling by its cost, not its presence (6.3): critical when more
/// operations failed out of retries than the run allows, or when half the operation time is
/// retry backoff; a warning when some operations failed or the backoff is a noticeable share;
/// otherwise informational (a few 429/503 that retries absorbed at no cost).
pub fn throttle_severity(failed_ratio: f64, backoff_share: f64, max_error_ratio: f64) -> Severity {
    if failed_ratio > max_error_ratio || backoff_share >= CRITICAL_BACKOFF_RATIO {
        Severity::Critical
    } else if failed_ratio > 0.0 || backoff_share >= WARNING_BACKOFF_RATIO {
        Severity::Warning
    } else {
        Severity::Info
    }
}

/// The share of operation time `f` costs ([`COST_KEY`]; 0 when not estimated).
fn cost_ratio(f: &Finding) -> f64 {
    f.evidence
        .values
        .get(COST_KEY)
        .copied()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// Most severe first; within one severity, a saturated client first (it makes the others'
/// timing unreliable), then the costliest (a stable sort: findings that estimate no cost keep
/// their order).
pub fn rank(findings: &mut [Finding]) {
    let client = |f: &Finding| f.code == "client_bound";
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(client(b).cmp(&client(a)))
            .then(cost_ratio(b).total_cmp(&cost_ratio(a)))
    });
}

/// The shortest mean wait in a service's queue (per answered request) that is a finding: 1 ms.
/// Below it the growth of the time to first byte with the requests in flight cannot be told
/// from the client's own scheduling.
const MIN_QUEUE_WAIT_US: f64 = 1000.0;

/// `throttling` with no rejection: the selected limit model is queueing, asserted with medium
/// confidence or more — the service serves a number of requests at once and the rest wait
/// for their first byte. The limit is the same kind as a 429/503 in-flight limit, and so is
/// its remedy; prove sizes the cap and says what it gains. 9.1 fitted the model and dropped
/// it: such a run had no finding, or only the hung requests and stragglers the queue made.
fn queueing(c: &Ctx) -> Option<Finding> {
    let h = c.r.capacity.asserted()?;
    if h.kind != HypothesisKind::Queueing || c.r.resilience.throttled_responses > 0 {
        return None;
    }
    // No rejection backs this limit: it is a finding only while the model explains at least
    // half of the wait at the busiest level.
    let confidence = limit_confidence(c, h);
    if confidence < Confidence::Medium {
        return None;
    }
    let servers = h.params.servers_count?;
    let service_ms = h.params.service_ms?;
    // What the queue costs: the wait for the first byte beyond the service time, as a share
    // of all request time.
    let service_us = service_ms * 1000.0;
    let waited = |q: &ReqRow| (q.ttfb_us as f64 - service_us).max(0.0);
    let wait: f64 = c.reqs().filter(|q| !q.is_error()).map(waited).sum();
    let total: f64 = c.reqs().map(|q| q.total_us as f64).sum::<f64>().max(1.0);
    // A wait this short is not a service's queue: it is what the client's own scheduling and
    // the loopback add when more requests are in flight (Windows CI: a time to first byte of
    // 0.5 ms at 1 in flight and 1.1 ms at 15 on `lab://clean` fitted "7 served at once").
    let answered = c.reqs().filter(|q| !q.is_error()).count().max(1) as f64;
    if wait / answered < MIN_QUEUE_WAIT_US {
        return None;
    }
    let mut text = h.evidence.first().cloned().unwrap_or_default();
    text += "; no request was rejected (429/503)";
    Some(Finding {
        code: "throttling".into(),
        severity: Severity::Warning,
        confidence,
        cause: format!(
            "The service serves about {servers} requests at once and queues the rest: {:.0}% of request time waits in its queue (no request was rejected).",
            wait * 100.0 / total
        ),
        evidence: Evidence {
            text,
            values: values(&[
                ("fitted_inflight_count", servers as f64),
                ("typical_ttfb_ms", service_ms),
                ("explains_ratio", h.explains_ratio),
                (COST_KEY, wait / total),
            ]),
            op_ids: slowest_ops(
                c.run,
                c.reqs().filter(|q| waited(q) > service_us).map(|q| q.op_id),
            ),
            windows: Vec::new(),
        },
        remedy: remedy("throttling", Some("queueing")),
    })
}

fn bandwidth_cap(c: &Ctx) -> Option<Finding> {
    let h = c.r.capacity.asserted()?;
    if h.kind != HypothesisKind::BandwidthCap {
        return None;
    }
    let bps = h.params.bytes_per_s.unwrap_or(0.0);
    // The cap was fitted on the request bodies: the uplink is what is shared.
    let upload = super::limits::is_upload_cap(h);
    // ThrottleLayer shapes writes only (K16): on a workload that writes nothing it cannot
    // help, and the finding says the cap is not a layer. A capped uplink is always writes.
    let writes = upload
        || c.r
            .workload
            .mix
            .iter()
            .any(|m| m.weight > 0 && m.op.contains("write"));
    let moving: f64 = c
        .reqs()
        .map(|q| if upload { q.send_us } else { q.body_us } as f64)
        .sum();
    let total: f64 = c.reqs().map(|q| q.total_us as f64).sum::<f64>().max(1.0);
    let (cause, stage, key) = if upload {
        (
            "Every upload stream shares one bandwidth cap: more streams only split it.",
            "sending the request body",
            "upload_pct",
        )
    } else {
        (
            "Every response stream shares one bandwidth cap: more streams only split it.",
            "the response body",
            "body_pct",
        )
    };
    Some(Finding {
        code: "bandwidth_cap".into(),
        severity: Severity::Warning,
        confidence: limit_confidence(c, h),
        cause: cause.into(),
        evidence: Evidence {
            text: format!(
                "{:.0}% of request time is {stage}; a stream's rate falls as more run beside it, the aggregate stays flat at {:.1} MiB/s",
                moving * 100.0 / total,
                if upload {
                    bps / (1024.0 * 1024.0)
                } else {
                    c.r.summary.mib_per_s
                }
            ),
            values: values(&[
                (key, moving * 100.0 / total),
                ("fitted_bytes_per_s", bps),
                ("explains_ratio", h.explains_ratio),
            ]),
            op_ids: slowest_ops(c.run, c.reqs().map(|q| q.op_id)),
            windows: Vec::new(),
        },
        remedy: remedy("bandwidth_cap", (!writes).then_some("reads")),
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
        .params(op.call_timeout());
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
    // Sampled rows over-weigh the tail (the slowest, failed and throttled operations are
    // always kept): the run's exact distribution, else the rows weighted by what they stand
    // for.
    let w = run.op_weights();
    let weight = |id: u64| w.get(&id).copied().unwrap_or(1.0);
    let s = &c.r.summary.latency;
    let (p50, p99) = if s.count > 0 && s.p50 > 0.0 {
        (s.p50, s.p99)
    } else {
        let mut lat: Vec<(u64, f64)> = run
            .ops
            .iter()
            .filter(|o| o.t_us >= 0)
            .map(|o| (o.latency_us, weight(o.op_id)))
            .collect();
        (
            weighted_quantile_us_ms(&mut lat, 0.5),
            weighted_quantile_us_ms(&mut lat, 0.99),
        )
    };
    if p50 <= 0.0 || p99 < 3.0 * p50 {
        return None;
    }
    // T4: the requests themselves must wait for the first byte; a tail of retry backoff or
    // queueing has no stragglers to cut (the replay below only counts TTFB-bound requests).
    let setup = remedy_setup(c.r);
    let mut apis: Vec<crate::trace::Api> = run.reqs.iter().map(|q| q.api).collect();
    apis.sort_by_key(|a| a.label());
    apis.dedup();
    // Every API with stragglers the tail cut would remove (largest gain first).
    let mut affected: Vec<(
        f64,
        crate::trace::Api,
        Vec<&ReqRow>,
        simulate::TailCutReplay,
    )> = Vec::new();
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
        affected.push((gain, api, over, replay));
    }
    if affected.is_empty() {
        return None;
    }
    affected.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.label().cmp(b.1.label())));
    // Per API: (api, stragglers, share %, straggler TTFB p50, typical TTFB p50), weighted.
    let per_api: Vec<(&str, usize, f64, f64, f64)> = affected
        .iter()
        .map(|(_, api, over, _)| {
            let ok: Vec<&ReqRow> = run
                .reqs
                .iter()
                .filter(|q| q.api == *api && !q.is_error())
                .collect();
            let all_w: f64 = ok.iter().map(|q| weight(q.op_id)).sum();
            let over_w: f64 = over.iter().map(|q| weight(q.op_id)).sum();
            let mut ttfb: Vec<(u64, f64)> =
                over.iter().map(|q| (q.ttfb_us, weight(q.op_id))).collect();
            let mut typical: Vec<(u64, f64)> =
                ok.iter().map(|q| (q.ttfb_us, weight(q.op_id))).collect();
            (
                api.label(),
                over.len(),
                over_w * 100.0 / all_w.max(1.0),
                weighted_quantile_us_ms(&mut ttfb, 0.5),
                weighted_quantile_us_ms(&mut typical, 0.5),
            )
        })
        .collect();
    let (_, _, share, wait, typ) = per_api[0];
    let stragglers: usize = per_api.iter().map(|a| a.1).sum();
    let shares: Vec<String> = per_api
        .iter()
        .map(|(api, _, share, _, _)| format!("{share:.1}% of {api}"))
        .collect();
    let mut text = per_api
        .iter()
        .map(|(api, n, _, wait, typ)| {
            format!(
                "{n} {api} stragglers wait ~{} for the first byte against ~{} for a typical {api}",
                fmt_ms(*wait),
                fmt_ms(*typ)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    text += &format!("; operation p50 {} · p99 {}", fmt_ms(p50), fmt_ms(p99));
    // What the stragglers cost: the wait beyond a typical request of every request that waits
    // for its first byte past its API's deadline, as a share of all request time.
    let mut ttfb_all: Vec<(u64, f64)> = run
        .reqs
        .iter()
        .filter(|q| !q.is_error())
        .map(|q| (q.ttfb_us, weight(q.op_id)))
        .collect();
    let typ_all_us = weighted_quantile_us_ms(&mut ttfb_all, 0.5) * 1000.0;
    let excess: f64 = affected
        .iter()
        .flat_map(|a| a.2.iter())
        .filter(|q| dominant_stage(q) == "ttfb")
        .map(|q| (q.total_us as f64 - typ_all_us).max(0.0))
        .sum();
    let total: f64 = run
        .reqs
        .iter()
        .map(|q| q.total_us as f64)
        .sum::<f64>()
        .max(1.0);
    Some(Finding {
        code: "tail_stragglers".into(),
        severity: Severity::Warning,
        confidence: if stragglers >= 20 {
            Confidence::High
        } else {
            Confidence::Medium
        },
        cause: format!(
            "{} requests wait far longer for the server's first byte than their peers (stragglers).",
            join_and(&shares)
        ),
        evidence: Evidence {
            text,
            values: values(&[
                ("straggler_pct", share),
                ("straggler_ttfb_ms", wait),
                ("typical_ttfb_ms", typ),
                ("p50_ms", p50),
                ("p99_ms", p99),
                ("replayed_p99_ms", affected[0].3.prediction.predicted),
                ("affected_apis_count", per_api.len() as f64),
                (COST_KEY, excess / total),
            ]),
            op_ids: slowest_ops(
                run,
                affected
                    .iter()
                    .flat_map(|a| a.2.iter().map(|q| q.op_id))
                    .collect::<Vec<_>>(),
            ),
            windows: Vec::new(),
        },
        remedy: remedy("tail_stragglers", None),
    })
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// A request is hung when it takes this long and ≥ 20× the median request.
const HUNG_MIN_US: u64 = 2_000_000;

/// `capped_uploads`: the uplink is at a shared cap (`bandwidth_cap` on the request bodies),
/// so a request that spends its time sending its body is slow like every other stream, not
/// hung: a timeout would only send the same bytes again.
fn hung_requests(c: &Ctx, capped_uploads: bool) -> Option<Finding> {
    // Weighted: sampled rows over-weigh the slow requests.
    let w = c.run.op_weights();
    let mut totals: Vec<(u64, f64)> = c
        .reqs()
        .map(|q| (q.total_us, w.get(&q.op_id).copied().unwrap_or(1.0)))
        .collect();
    if totals.len() < 20 {
        return None;
    }
    let median_ms = weighted_quantile_us_ms(&mut totals, 0.5);
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
            q.total_us >= bound
                && match dominant_stage(q) {
                    "send" => !capped_uploads,
                    stage => matches!(stage, "ttfb" | "body" | "stall"),
                }
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
        .filter(|q| q.api == crate::trace::Api::GetObject && !q.is_error())
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
    let small_gap = c.spec().read.gap.is_some_and(|g| g.0 < 256 * 1024);
    let concurrent = c.spec().read_concurrent();
    // Many tiny uncoalesced ranges per operation (or already concurrent small ranges without
    // coalescing), waiting for the first byte: coalesce adjacent ranges first so `read.gap`
    // is not shadowed by `read.concurrent = 1`.
    let coalesce_first =
        mean < 64.0 * 1024.0 || small_gap || (concurrent > 1 && mean < 256.0 * 1024.0);
    if reads.len() >= 50
        && per_op >= 3.0
        && mean < 256.0 * 1024.0
        && ttfb / total >= 0.5
        && coalesce_first
        && (!gap_set || small_gap)
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
        .filter(|q| q.api == crate::trace::Api::UploadPart && !q.is_error())
        .collect();
    if parts.len() < 10 || c.spec().write_concurrent() > 1 {
        return None;
    }
    let part_ops: std::collections::HashSet<u64> = parts
        .iter()
        .filter(|q| q.op_id != 0)
        .map(|q| q.op_id)
        .collect();
    let writes: Vec<&crate::model::OpRow> = c
        .run
        .ops
        .iter()
        .filter(|o| {
            o.t_us >= 0
                && (o.verb.contains("multipart")
                    || (o.verb.starts_with("write") && part_ops.contains(&o.op_id)))
        })
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
                "replaying the reads through an in-memory cache would save {} requests per {} successful read operations",
                crate::model::group_thousands(-p.requests_per_1k_ops_delta, 0),
                crate::model::group_thousands(1000.0, 0)
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
    // Weighted by what each sampled row stands for (warmup rows are never sampled).
    let w = c.run.op_weights();
    let weight = |q: &ReqRow| {
        if q.t_us < 0 {
            1.0
        } else {
            w.get(&q.op_id).copied().unwrap_or(1.0)
        }
    };
    let total: f64 = reqs.iter().map(|q| q.total_us as f64 * weight(q)).sum();
    let connect: f64 = cold.iter().map(|q| q.connect_us() as f64 * weight(q)).sum();
    let share = connect * 100.0 / total.max(1.0);
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
        let mut v: Vec<(u64, f64)> = cold
            .iter()
            .map(|q| (f(q), weight(q)))
            .filter(|x| x.0 > 0)
            .collect();
        weighted_quantile_us_ms(&mut v, 0.5)
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
    // The arrivals queued behind the workers while the CPU had room (K15): `--rate` asks for
    // more than `-c` workers carry at the operations' latency.
    let workers = backlog.is_some() && !cpu_bound && !saturated;
    let conc = r.spec.profile.workload.concurrency.max(1);
    if let (true, Some(ol)) = (workers, backlog) {
        text += &format!(
            "; the arrivals queued behind the {conc} workers: {:.0}/s scheduled, {:.0}/s carried",
            ol.target_rate, ol.achieved_rate
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
        cause: if workers {
            format!(
                "The {conc} workers set the pace, not the service: the arrivals queued behind them, and the client's CPU had room."
            )
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
        remedy: remedy("client_bound", workers.then_some("workers")),
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
    // One finding per code: the queueing form of `throttling` only when nothing was rejected.
    if !out.iter().any(|f| f.code == "throttling") {
        out.extend(queueing(&c));
    }
    let cap = bandwidth_cap(&c);
    let have_cap = cap.is_some();
    let capped_uploads = cap
        .as_ref()
        .is_some_and(|f| f.evidence.values.contains_key("upload_pct"));
    out.extend(cap);
    out.extend(hung_requests(&c, capped_uploads));
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
    rank(&mut out);
    for f in &mut out {
        f.remedy.next = Some(next_command(&r.run.id, f));
    }
    let (kept, _) = validity(r, out);
    kept
}

/// The objective the remedy of finding `code` is proved on, when it is not prove's default
/// (goodput): `p99` for an in-flight or rate limit. Capping the requests in flight takes the
/// throttled operations' retry backoffs out of the tail, but cannot raise goodput while spare
/// workers already keep the service at its limit.
pub fn proof_objective(code: &str) -> Option<&'static str> {
    matches!(code, "throttling" | "rate_limit").then_some("p99")
}

/// The next command of a finding: `oprobe prove RUN CODE` (with the objective it is proved
/// on) when the remedy is a change of the operator, else where to look. A remedy that is not a
/// layer (`key_limit`: the key layout; a bandwidth cap on reads) has nothing to prove: the
/// hot keys and the cap are in `show --detail`.
pub fn next_command(run_id: &str, f: &Finding) -> String {
    match f.code.as_str() {
        "errors" if f.remedy.path.is_none() => format!("oprobe show {run_id} --by api"),
        _ if !f.remedy.provable() => format!("oprobe show {run_id} --detail"),
        code => match proof_objective(code) {
            Some(obj) => format!("oprobe prove {run_id} {code} --objective {obj}"),
            None => format!("oprobe prove {run_id} {code}"),
        },
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

impl Validity {
    /// Why the run cannot be judged on performance (invalid, or partial and too short), or
    /// `None` for a run that can: `prove` refuses such a run, and no finding suggests it.
    pub fn unjudged(&self, r: &Report) -> Option<String> {
        if let Some(why) = &self.invalid {
            return Some(why.clone());
        }
        let p = r.run.partial.as_ref().filter(|_| self.suppress)?;
        Some(format!(
            "it is partial ({} after {:.1} s of the {:.1} s window)",
            p.reason.describe(),
            p.measured_s,
            r.workload.duration_s
        ))
    }
}

/// The run's [`Validity`].
pub fn run_validity(r: &Report) -> Validity {
    let s = &r.summary;
    let window = r.run.window_s;
    let failed = non_throttle_failures(r);
    let error_ratio = failed as f64 / s.ops.max(1) as f64;
    let invalid = if s.ops == 0 {
        Some("no operation completed in the measurement window".to_owned())
    } else if !r.is_trace() && error_ratio > r.sla.max_error_ratio {
        Some(format!(
            "{:.1}% of operations failed for reasons other than throttling (the SLA allows {}%), so latency and throughput describe failures",
            error_ratio * 100.0,
            r.sla.max_error_ratio * 100.0
        ))
    } else if !r.is_trace() && r.workload.warmup_s > 0.0 && window < r.workload.warmup_s {
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
/// cannot be judged, and `invalid_run` says why. It leads, and no finding suggests `prove`
/// (which refuses such a run): a limit seen in a broken run is a lead, not a remedy to size.
/// Idempotent.
pub fn validity(r: &Report, findings: Vec<Finding>) -> (Vec<Finding>, Validity) {
    let v = run_validity(r);
    let mut out: Vec<Finding> = findings
        .into_iter()
        .filter(|f| f.code != "invalid_run")
        .filter(|f| !v.suppress || VALID_ON_BROKEN_RUNS.contains(&f.code.as_str()))
        .collect();
    rank(&mut out);
    let Some(why) = v.unjudged(r) else {
        return (out, v);
    };
    // The reason the run cannot be judged comes first; nothing points at `prove`.
    for f in &mut out {
        if f.remedy
            .next
            .as_deref()
            .is_some_and(|n| n.starts_with("oprobe prove"))
        {
            f.remedy.next = None;
        }
    }
    let mut rem = remedy("invalid_run", None);
    rem.next = Some(format!("oprobe show {} --detail", r.run.id));
    out.insert(
        0,
        Finding {
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
        },
    );
    (out, v)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;

    /// Throttling is judged by what it costs: failed operations and retry backoff, not the
    /// mere presence of 429/503 (the demo's 0.3 % throttled requests absorbed by retries).
    #[test]
    fn throttling_severity_follows_its_cost() {
        let max = 0.01;
        assert_eq!(throttle_severity(0.0, 0.01, max), Severity::Info);
        assert_eq!(throttle_severity(0.0, 0.2, max), Severity::Warning);
        assert_eq!(throttle_severity(0.001, 0.0, max), Severity::Warning);
        assert_eq!(throttle_severity(0.02, 0.0, max), Severity::Critical);
        assert_eq!(throttle_severity(0.0, 0.7, max), Severity::Critical);
    }

    #[test]
    fn findings_of_one_severity_rank_by_cost() {
        let f = |code: &str, sev, cost: Option<f64>| Finding {
            code: code.into(),
            severity: sev,
            evidence: Evidence {
                values: cost.map(|c| values(&[(COST_KEY, c)])).unwrap_or_default(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut v = vec![
            f("errors", Severity::Warning, None),
            f("throttling", Severity::Warning, Some(0.15)),
            f("tail_stragglers", Severity::Warning, Some(0.4)),
            f("cold_connections", Severity::Critical, None),
        ];
        rank(&mut v);
        let codes: Vec<&str> = v.iter().map(|f| f.code.as_str()).collect();
        assert_eq!(
            codes,
            [
                "cold_connections",
                "tail_stragglers",
                "throttling",
                "errors"
            ]
        );
    }

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
            latency_histogram: Default::default(),
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
    fn stragglers_name_every_affected_api() {
        let mut run = base();
        // Alternate blocks of 40 requests between two APIs: both get their stragglers.
        for (i, q) in run.reqs.iter_mut().enumerate() {
            if (i / 40) % 2 == 1 {
                q.api = crate::trace::Api::HeadObject;
            }
        }
        let f = derive(&run);
        let t = f
            .iter()
            .find(|x| x.code == "tail_stragglers")
            .unwrap_or_else(|| panic!("{f:#?}"));
        assert!(
            t.cause.contains("GetObject") && t.cause.contains("HeadObject"),
            "{}",
            t.cause
        );
        assert!(t.cause.contains(" and "), "{}", t.cause);
        assert!(
            t.evidence.text.contains("GetObject stragglers")
                && t.evidence.text.contains("HeadObject stragglers"),
            "{}",
            t.evidence.text
        );
        assert_eq!(t.evidence.values["affected_apis_count"], 2.0);
    }

    #[test]
    fn sampled_rows_are_weighted_by_what_they_stand_for() {
        // 1 000 kept rows of 10 000 operations: the 25 slowest kept always, the rest a sample.
        let mut run = base();
        run.report.sampling = crate::model::Sampling {
            seen_ops_count: 10_000,
            kept_ops_count: 1000,
            kept_ratio: 0.1,
            always_kept_ops_count: 25,
            ..Default::default()
        };
        let w = run.op_weights();
        // op 8 is a straggler (row 7): it stands for itself.
        assert!(!w.contains_key(&8));
        assert!((w[&1] - 9975.0 / 975.0).abs() < 1e-9, "{}", w[&1]);
        // With the timeline's exact counts: per interval (2 000 operations, 200 rows, 5 of
        // them stragglers).
        run.report.timeline = (0..5)
            .map(|i| crate::trace::TimelinePoint {
                t_s: f64::from(i),
                len_s: 1.0,
                ops_count: 2000,
                ..Default::default()
            })
            .collect();
        let w = run.op_weights();
        assert!((w[&1] - 1995.0 / 195.0).abs() < 1e-9, "{}", w[&1]);
        // Weighted, the stragglers are 0.25 % of the operations, not 2.5 % of the rows: the
        // p99 is a typical operation, and there is no tail to cut.
        assert!(!derive(&run).iter().any(|x| x.code == "tail_stragglers"));
    }

    #[test]
    fn repeated_reads_count_requests_per_thousand_reads() {
        let mut run = base();
        run.report.workload.access = "zipf(1.2)".into();
        run.report.workload.mix = vec![crate::model::MixEntry {
            weight: 1,
            op: "read".into(),
            ..Default::default()
        }];
        let f = derive(&run);
        let rr = f
            .iter()
            .find(|x| x.code == "repeated_reads")
            .unwrap_or_else(|| panic!("{f:#?}"));
        let saved = rr.evidence.values["saved_requests_per_1k_ops_mean"];
        // The text and the labelled value name the same unit and print the same number.
        let n = crate::model::group_thousands(saved, 0);
        assert!(
            rr.evidence.text.contains(&format!(
                "{n} requests per 1\u{202f}000 successful read operations"
            )),
            "{}",
            rr.evidence.text
        );
        let l = rr.evidence.labelled();
        assert_eq!(
            l[0],
            ("Requests saved per 1\u{202f}000 reads".to_owned(), n)
        );
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
        // Throttling that costs: most of the operation time is retry backoff (critical).
        run.report.stages.shares.retry_pct = 60.0;
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

    /// The cause (the headline) leads with what throttling costs, not the share of requests.
    #[test]
    fn the_throttling_cause_leads_with_its_cost() {
        let mut run = throttled(HypothesisKind::InflightLimit, 0.95, Confidence::High);
        run.report.connections.requests = 10_000;
        let f = derive(&run);
        assert!(
            f[0].cause
                .starts_with("60% of operation time is spent in retry backoff after 429/503: "),
            "{}",
            f[0].cause
        );
        assert!(
            f[0].cause.contains("throttled 1.0% of requests"),
            "{}",
            f[0].cause
        );
        // Absorbed at no cost: said so.
        run.report.stages.shares.retry_pct = 0.1;
        let f = derive(&run);
        let t = f.iter().find(|x| x.code == "throttling").unwrap();
        assert_eq!(
            t.cause,
            "The service throttled 1.0% of requests (429/503); retries absorbed them at under 1% of operation time."
        );
    }

    #[test]
    fn high_confidence_needs_the_model_to_explain_most_of_it() {
        let run = throttled(HypothesisKind::InflightLimit, 0.45, Confidence::High);
        assert_eq!(derive(&run)[0].confidence, Confidence::Low);
        let run = throttled(HypothesisKind::InflightLimit, 0.6, Confidence::High);
        assert_eq!(derive(&run)[0].confidence, Confidence::Medium);
        // A rounding error below a threshold is the threshold (no flips between runs).
        let run = throttled(HypothesisKind::InflightLimit, 0.5 - 1e-12, Confidence::High);
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
        assert_eq!(e.remedy.path.as_deref(), Some("layers.retry.min_delay"));
        assert_eq!(e.remedy.with, ["layers.retry.jitter"]);
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

    /// 6.3.0: Ctrl-C after 4 s ranked a throttling finding first with `next: oprobe prove`.
    /// A run that cannot be judged leads with `invalid_run` and suggests no prove.
    #[test]
    fn an_unjudged_run_leads_with_invalid_run_and_suggests_no_prove() {
        use crate::model::PartialReason;
        for reason in [PartialReason::Interrupted, PartialReason::Error] {
            let mut run = throttled(HypothesisKind::InflightLimit, 0.9, Confidence::High);
            run.report.findings = derive(&run);
            assert!(
                run.report.findings[0]
                    .remedy
                    .next
                    .as_deref()
                    .unwrap()
                    .starts_with("oprobe prove")
            );
            crate::model::build::mark_partial(&mut run, reason);
            let f = &run.report.findings;
            assert_eq!(f[0].code, "invalid_run", "{f:#?}");
            assert!(f[0].cause.contains("partial"), "{}", f[0].cause);
            assert!(f.iter().any(|x| x.code == "throttling"), "{f:#?}");
            for x in f {
                let next = x.remedy.next.as_deref().unwrap_or_default();
                assert!(!next.starts_with("oprobe prove"), "{}: {next}", x.code);
            }
            assert!(
                crate::app::next::after_run(&run.report)
                    .iter()
                    .all(|n| !n.cli.starts_with("oprobe prove"))
            );
        }
        // An invalid run (failures that are not throttling) leads with invalid_run too.
        let mut run = throttled(HypothesisKind::InflightLimit, 0.9, Confidence::High);
        run.report.summary.failed_ops = 50;
        run.report.summary.error_rate = 0.05;
        run.report
            .resilience
            .error_classes
            .insert("permission".into(), 50);
        let f = derive(&run);
        assert_eq!(f[0].code, "invalid_run", "{f:#?}");
        assert!(f.iter().all(|x| {
            !x.remedy
                .next
                .as_deref()
                .unwrap_or_default()
                .starts_with("oprobe prove")
        }));
    }

    /// 6.3.0: `--max-requests 500` gave `[Critical] Run invalid`; the docs say a partial run is
    /// Review.
    #[test]
    fn a_partial_run_is_review() {
        use crate::model::{PartialReason, VerdictLevel};
        for reason in [PartialReason::Budget, PartialReason::Interrupted] {
            let mut run = throttled(HypothesisKind::InflightLimit, 0.9, Confidence::High);
            run.report.findings = derive(&run);
            assert_eq!(run.report.worst_severity(), Severity::Critical);
            crate::model::build::mark_partial(&mut run, reason);
            assert_eq!(run.report.verdict.level, VerdictLevel::Review, "{reason:?}");
        }
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

    /// Retried resets alone are an `errors` finding (K4), and failures retries recovered name
    /// the retry backoff, sized by prove, with jitter (K1: 7.0 named jitter alone, and prove
    /// came out worse).
    #[test]
    fn retried_transient_failures_name_the_retry_backoff() {
        let mut run = base();
        run.report.resilience.network_errors = 20;
        let f = derive(&run);
        let e = f
            .iter()
            .find(|x| x.code == "errors")
            .unwrap_or_else(|| panic!("{f:#?}"));
        assert_eq!(e.remedy.path.as_deref(), Some("layers.retry.min_delay"));
        assert_eq!(e.remedy.with, ["layers.retry.jitter"]);
        assert!(e.remedy.sized);
        assert_eq!(e.evidence.values["network_errors_count"], 20.0);
        assert!(
            e.evidence.text.contains("lost their connection"),
            "{}",
            e.evidence.text
        );
        assert_eq!(e.remedy.next.as_deref(), Some("oprobe prove r1 errors"));
        // Too few to matter: nothing.
        run.report.resilience.network_errors = 2;
        assert!(!derive(&run).iter().any(|x| x.code == "errors"));
        // Failures that reached the application: not a layer.
        run.report.summary.failed_ops = 1;
        run.report.summary.ops = 1000;
        let f = derive(&run);
        let e = f.iter().find(|x| x.code == "errors").unwrap();
        assert!(e.remedy.path.is_none());
    }

    /// In a production trace, one failed operation (a stat of a missing key) is not a finding:
    /// the same minimum share as a run's retried requests (K14).
    #[test]
    fn one_failure_in_a_trace_is_not_an_errors_finding() {
        let mut run = base();
        run.report.run.source = crate::model::RunSource::Trace;
        run.report.summary.ops = 1000;
        run.report.summary.failed_ops = 1;
        assert!(!derive(&run).iter().any(|x| x.code == "errors"));
        run.report.summary.failed_ops = 5;
        assert!(derive(&run).iter().any(|x| x.code == "errors"));
    }

    /// An open-loop run whose arrivals queue behind the workers names the workers, not the
    /// CPU (K15).
    #[test]
    fn a_rate_above_the_workers_names_the_workers() {
        let mut run = base();
        run.report.self_stats.cpu_ratio = Some(0.1);
        run.report.spec.profile.workload.concurrency = 2;
        run.report.summary.latency.mean = 10.0;
        run.report.open_loop = Some(crate::trace::OpenLoopStats {
            target_rate: 1000.0,
            achieved_rate: 200.0,
            missed: 4000,
            intended_count: 5000,
            ..Default::default()
        });
        let f = derive(&run);
        let c = f
            .iter()
            .find(|x| x.code == "client_bound")
            .unwrap_or_else(|| panic!("{f:#?}"));
        assert!(c.cause.contains("2 workers"), "{}", c.cause);
        assert!(
            !c.cause.contains("CPU, not") && !c.remedy.text.contains("more cores"),
            "{c:#?}"
        );
        assert!(c.remedy.text.contains("`-c`"), "{}", c.remedy.text);
        assert!(
            c.evidence.text.contains("1000/s scheduled, 200/s carried"),
            "{}",
            c.evidence.text
        );
    }

    /// `run` with the limit models fitted to its rows, as the report builder fits them.
    fn with_capacity(mut run: Run, reqs: Vec<ReqRow>, ops: Vec<crate::model::OpRow>) -> Run {
        run.report.summary.ops = ops.len() as u64;
        run.report.connections.requests = reqs.len() as u64;
        let last = reqs
            .iter()
            .map(|q| q.t_us + q.total_us as i64)
            .max()
            .unwrap();
        run.report.run.window_s = last as f64 / 1e6;
        run.report.capacity = capacity::estimate_with(
            &reqs,
            &ops,
            capacity::Window::measured(run.report.run.window_s),
            &capacity::FitInputs::default(),
        );
        (run.reqs, run.ops) = (reqs, ops);
        run
    }

    /// A saturated uplink: uploads that share 8 MiB/s take seconds beside each other, among
    /// quick small requests. 9.1 fitted no cap on request bodies and called the slow uploads
    /// hung (a timeout that would only send the same bytes again).
    #[test]
    fn a_saturated_uplink_is_a_bandwidth_cap_not_hung_requests() {
        let (mut reqs, mut ops) = crate::analysis::limits::tests::capped_link(true);
        // Uploads of 4 MiB: up to 4 s when eight share the link.
        for q in &mut reqs {
            q.t_us *= 4;
            q.send_us *= 4;
            q.bytes_up *= 4;
            q.total_us = q.send_us + q.ttfb_us;
        }
        // Quick metadata requests all along: the median request is fast.
        let end = reqs.iter().map(|q| q.t_us).max().unwrap();
        for i in 0..600u64 {
            let id = 10_000 + i;
            let t = end / 600 * i as i64;
            let mut q = crate::analysis::outliers::test_req(t, 8_000);
            q.op_id = id;
            reqs.push(q);
            ops.push(crate::analysis::simulate::tests::op(id, t, 8_000));
        }
        let run = with_capacity(base(), reqs, ops);
        let slow = run
            .reqs
            .iter()
            .filter(|q| q.total_us >= HUNG_MIN_US && dominant_stage(q) == "send")
            .count();
        assert!(
            slow >= 20,
            "the trace has uploads slow enough to read as hung: {slow}"
        );
        let f = derive(&run);
        let cap = f
            .iter()
            .find(|x| x.code == "bandwidth_cap")
            .unwrap_or_else(|| panic!("{f:#?}\n{:#?}", run.report.capacity.hypotheses));
        assert_eq!(
            cap.remedy.path.as_deref(),
            Some("layers.throttle.bandwidth")
        );
        assert_eq!(
            cap.remedy.next.as_deref(),
            Some("oprobe prove r1 bandwidth_cap")
        );
        assert!(cap.cause.contains("upload"), "{}", cap.cause);
        assert!(cap.evidence.values["upload_pct"] > 50.0, "{cap:#?}");
        let mib_s = cap.evidence.values["fitted_bytes_per_s"] / (1u64 << 20) as f64;
        assert!((7.0..9.0).contains(&mib_s), "{mib_s}");
        assert!(!f.iter().any(|x| x.code == "hung_requests"), "{f:#?}");
        // A request that hangs waiting for the server is still hung beside the cap.
        let mut run = run;
        for q in run.reqs.iter_mut().filter(|q| q.op_id >= 10_000).take(3) {
            q.ttfb_us = 9_000_000;
            q.total_us = 9_000_010;
        }
        let f = derive(&run);
        assert!(f.iter().any(|x| x.code == "bandwidth_cap"), "{f:#?}");
        let hung = f.iter().find(|x| x.code == "hung_requests").unwrap();
        assert_eq!(hung.evidence.values["hung_requests_count"], 3.0);
    }

    /// A service that queues instead of rejecting: 9.1 fitted the queueing model and no
    /// finding read it. It is the in-flight limit of `throttling`, with the same remedy.
    #[test]
    fn queueing_is_an_inflight_limit_without_rejections() {
        let (reqs, ops) = crate::analysis::limits::tests::queueing_bursts(|b| 2 + b % 14);
        let run = with_capacity(base(), reqs, ops);
        let sel = run.report.capacity.asserted().expect("asserted");
        assert_eq!(sel.kind, HypothesisKind::Queueing);
        let f = derive(&run);
        let q = f
            .iter()
            .find(|x| x.code == "throttling")
            .unwrap_or_else(|| panic!("{f:#?}"));
        assert_eq!(q.severity, Severity::Warning);
        assert!(q.confidence >= Confidence::Medium);
        assert_eq!(
            q.remedy.path.as_deref(),
            Some("layers.concurrent_limit.http")
        );
        assert!(q.remedy.sized);
        assert!(q.remedy.text.contains("queue"), "{}", q.remedy.text);
        assert_eq!(
            q.remedy.next.as_deref(),
            Some("oprobe prove r1 throttling --objective p99")
        );
        let servers = q.evidence.values["fitted_inflight_count"];
        assert!((3.0..=5.0).contains(&servers), "{servers}");
        assert!(q.evidence.values[COST_KEY] > 0.2, "{q:#?}");
        assert!(q.cause.contains("queues the rest") && q.cause.contains("no request was rejected"));
        // No sizing advice in words (N2): the limit is evidence, prove sizes the cap.
        assert!(!q.cause.contains("concurrency") && !q.cause.contains("run at"));
        assert!(!q.evidence.op_ids.is_empty());
        assert_eq!(f.iter().filter(|x| x.code == "throttling").count(), 1);
        // The same curve at a twentieth of the time (0.5 ms of service, waits of a fraction of
        // a millisecond) is the client's own scheduling, not a service's queue: no finding.
        let (reqs, ops) = crate::analysis::limits::tests::queueing_bursts_of(0.5, |b| 2 + b % 14);
        let tiny = with_capacity(base(), reqs, ops);
        let f = derive(&tiny);
        assert!(!f.iter().any(|x| x.code == "throttling"), "{f:#?}");
        // The same service under a load that never varied asserts nothing: no finding.
        let (reqs, ops) = crate::analysis::limits::tests::queueing_closed_loops(&[12, 14, 16, 19]);
        let steady = with_capacity(base(), reqs, ops);
        let sel = steady.report.capacity.selected().expect("selected");
        assert_eq!(sel.kind, HypothesisKind::Queueing);
        let f = derive(&steady);
        assert!(!f.iter().any(|x| x.code == "throttling"), "{f:#?}");
        // And real rejections keep the one `throttling` finding they always made.
        let mut thr = run;
        thr.report.resilience.throttled_responses = 100;
        let f = derive(&thr);
        let t: Vec<&Finding> = f.iter().filter(|x| x.code == "throttling").collect();
        assert_eq!(t.len(), 1);
        assert!(t[0].evidence.text.contains("throttled responses"), "{t:#?}");
    }

    /// A bandwidth cap names ThrottleLayer only where it applies: on writes (K16).
    #[test]
    fn a_bandwidth_cap_on_reads_is_not_a_layer() {
        let reads = remedy("bandwidth_cap", Some("reads"));
        assert!(reads.path.is_none());
        assert!(reads.text.contains("not a layer"), "{}", reads.text);
        let writes = remedy("bandwidth_cap", None);
        assert_eq!(writes.path.as_deref(), Some("layers.throttle.bandwidth"));
        let f = Finding {
            code: "bandwidth_cap".into(),
            remedy: reads,
            ..Default::default()
        };
        assert_eq!(next_command("r1", &f), "oprobe show r1 --detail");
    }

    /// 9.0 suggested `prove … key_limit` for a remedy it calls "not a layer" (W9), and the
    /// throttle family's `next` omitted the objective it is proved on (W8).
    #[test]
    fn next_proves_only_layers_on_their_objective() {
        let f = |code: &str, variant: Option<&str>| Finding {
            code: code.into(),
            remedy: remedy(code, variant),
            ..Default::default()
        };
        assert_eq!(
            next_command("r1", &f("key_limit", None)),
            "oprobe show r1 --detail"
        );
        for code in ["throttling", "rate_limit"] {
            assert_eq!(
                next_command("r1", &f(code, None)),
                format!("oprobe prove r1 {code} --objective p99")
            );
        }
        assert_eq!(
            next_command("r1", &f("tail_stragglers", None)),
            "oprobe prove r1 tail_stragglers"
        );
        // Every `prove` a finding suggests is one prove accepts: a layer.
        for c in CODES {
            for v in [None, Some("reads"), Some("retried"), Some("workers")] {
                let x = f(c, v);
                if next_command("r1", &x).starts_with("oprobe prove") {
                    assert!(x.remedy.provable(), "{c} {v:?}");
                }
            }
        }
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
