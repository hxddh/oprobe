//! Findings engine 3.0: diagnosis items computed from a run's aggregates *and* its trace rows.
//!
//! Each [`Finding`] has a stable `code` (documented by `oprobe explain CODE`), a severity, a
//! one-sentence cause, evidence (numbers plus references: operation ids, time windows), advice
//! and — where a knob change can test the advice — a verification with the exact
//! `oprobe run …` command. Predictions ("tail-cut (deadline 50 ms) would cut p99 from 918 ms to
//! ~140 ms") come from [`super::simulate`], which replays the recorded rows; they are stated
//! as estimates.
//!
//! One finding per code; tail latency is explained once, by its dominant cause.

use std::collections::{BTreeMap, HashMap};

use super::capacity::Capacity;
use super::hypothesis::HypothesisKind;
use super::diagnosis::{self, Severity};
use super::outliers::{Selection, attribute, dominant_stage};
use super::simulate::{self, Prediction, TailCutReplay, TailCutSetup};
use super::stats::{median, quantile_us_ms};
use super::verification::{Direction, Expectation};
use crate::model::{Evidence, Finding, Report, ReqRow, Run, TimeWindow, Verification};
use crate::util::{fmt_bytes, fmt_ms, fmt_usd};

/// Every finding code this engine can emit (canonical, `snake_case`). The 2.x diagnosis it
/// carries uses the same codes. `crate::mcp::catalog` documents exactly this set (a test
/// checks), and `oprobe capabilities` lists it.
pub const CODES: &[&str] = &[
    "integrity",
    "errors",
    "throttling",
    "retries",
    "tail_stragglers",
    "tail_latency",
    "client_queue",
    "cold_connections",
    "http1_connections",
    "remote_skew",
    "tcp_retransmits",
    "high_rtt",
    "consumer_stall",
    "overread",
    "small_objects",
    "suffix_read",
    "cache_opportunity",
    "cost_hotspot",
    "slow_start",
    "server_latency",
    "download_bound",
    "upload_bound",
    "client_cpu",
    "client_overhead",
    "local_backend",
    "cache_ineffective",
    "sla_p99",
    "rate_not_sustained",
    "run_invalid",
    "run_partial",
];

/// Throttled-request share a recommendation may leave (the tune budget's default).
pub const THROTTLE_BUDGET_RATIO: f64 = 0.001;

/// Codes that still hold on a broken run: facts about correctness, failures and the
/// service's limits. Every other finding is about performance and is suppressed when the run
/// is invalid or too short to judge (design v4 §4).
const VALID_ON_BROKEN_RUNS: &[&str] = &[
    "integrity",
    "errors",
    "throttling",
    "retries",
    "client_cpu",
    "run_invalid",
    "run_partial",
];

struct Ctx<'a> {
    run: &'a Run,
    r: &'a Report,
    network: bool,
    throttled: bool,
    /// The capacity model of the run (its own ceiling, or a remembered one).
    cap: &'a Capacity,
    /// The most workers any advice may suggest ([`Capacity::max_advisable_concurrency`]).
    safe: Option<u32>,
}

/// How a command names the run's target: the configured target name, the `lab://` scenario,
/// or the storage URL — plus `--endpoint` / `--region` only when the user gave them with a URL
/// (a `lab://` target's endpoint is an ephemeral in-process port; a named target carries its
/// own).
pub fn target_args(t: &crate::model::TargetInfo) -> Vec<String> {
    if let Some(n) = &t.name {
        return vec![n.clone()];
    }
    let mut out = vec![t.display.clone()];
    if t.display.starts_with("lab://") {
        return out;
    }
    for (opt, flag) in [("endpoint", "--endpoint"), ("region", "--region")] {
        if let Some(v) = t.options.get(opt).filter(|v| !v.contains("<redacted>")) {
            out.push(flag.into());
            out.push(v.clone());
        }
    }
    out
}

/// An `oprobe` command line: a subcommand, its positional arguments and flags (a flag set
/// twice keeps the last value, so a verification can override one knob of the run).
#[derive(Clone, Debug)]
pub struct Command {
    head: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Command {
    /// `oprobe SUBCOMMAND TARGET…` for the run's target.
    pub fn for_target(subcommand: &str, t: &crate::model::TargetInfo) -> Self {
        Self {
            head: std::iter::once(subcommand.to_owned())
                .chain(target_args(t))
                .collect(),
            flags: Vec::new(),
        }
    }

    /// Set (or replace) `flag`; `None` = a switch without a value.
    pub fn set(&mut self, flag: &str, value: Option<String>) -> &mut Self {
        match self.flags.iter_mut().find(|(f, _)| f == flag) {
            Some(slot) => slot.1 = value,
            None => self.flags.push((flag.to_owned(), value)),
        }
        self
    }

    /// Remove `flag`.
    pub fn unset(&mut self, flag: &str) -> &mut Self {
        self.flags.retain(|(f, _)| f != flag);
        self
    }

    /// Apply `extra` (`--flag value --switch …`), replacing flags already present.
    pub fn apply(&mut self, extra: &str) -> &mut Self {
        let toks: Vec<&str> = extra.split_whitespace().collect();
        let mut i = 0;
        while i < toks.len() {
            let value = toks.get(i + 1).filter(|v| !v.starts_with('-'));
            self.set(toks[i], value.map(|v| (*v).to_owned()));
            i += 1 + usize::from(value.is_some());
        }
        self
    }

    /// The arguments after `oprobe`.
    pub fn args(&self) -> Vec<String> {
        let mut v = self.head.clone();
        for (f, val) in &self.flags {
            v.push(f.clone());
            v.extend(val.clone());
        }
        v
    }
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "oprobe")?;
        for a in self.args() {
            let plain = !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,~".contains(c));
            if plain {
                write!(f, " {a}")?;
            } else {
                write!(f, " '{}'", a.replace('\'', r"'\''"))?;
            }
        }
        Ok(())
    }
}

impl Ctx<'_> {
    /// A verification of this run with `sets` (`--set` key/value pairs) applied to its spec:
    /// `oprobe run --from RUN --set …` and the self-contained replay, so nothing but the change
    /// differs. The `oprobe verify RUN CODE` command is filled in by [`derive`].
    fn verify(
        &self,
        description: impl Into<String>,
        sets: &[(&str, String)],
    ) -> Option<Verification> {
        let mut o = crate::model::Overrides::new();
        for (k, v) in sets {
            o.set(k, v).ok()?;
        }
        Some(Verification::new(
            &self.r.run.id,
            "",
            &self.r.spec,
            o,
            description,
        ))
    }

    fn concurrency(&self) -> u64 {
        self.r.knobs.requested.concurrency_count.max(1)
    }

    /// More workers, as far as the capacity model allows: twice the current concurrency,
    /// capped at the safe concurrency when a ceiling is known. `Err(safe)` when the run is
    /// already at (or above) the safe concurrency: more workers would throttle.
    fn scale_up(&self) -> Result<u64, u32> {
        let cc = self.concurrency();
        match self.safe {
            Some(safe) if u64::from(safe) <= cc => Err(safe),
            Some(safe) => Ok((cc * 2).min(u64::from(safe))),
            None => Ok(cc * 2),
        }
    }

    /// What a run at `to` workers should show: goodput from the capacity model (with its
    /// interval) and throttling within the budget.
    fn concurrency_expectations(&self, to: u64) -> Vec<Expectation> {
        let p = self.cap.predict(to.clamp(1, u64::from(u32::MAX)) as u32);
        let mut out = Vec::new();
        if p.ops_per_s > 0.0 && self.r.workload.rate_per_s.is_none() {
            out.push(Expectation {
                metric: "summary.ops_per_s".into(),
                direction: if to > self.concurrency() {
                    Direction::Increase
                } else {
                    Direction::AtLeast
                },
                predicted: p.ops_per_s,
                lo: p.ops_lo_per_s,
                hi: p.ops_hi_per_s,
                unit: "ops_per_s".into(),
            });
        }
        out.push(Expectation {
            metric: "summary.throttled_request_ratio".into(),
            direction: Direction::AtMost,
            // Under the ceiling nothing should be throttled; above it, the model's chance.
            predicted: if p.within_ceiling {
                0.0
            } else {
                p.throttle_probability_ratio
            },
            lo: 0.0,
            hi: THROTTLE_BUDGET_RATIO,
            unit: "ratio".into(),
        });
        out
    }

    /// The verification of pacing the client at `rate` operations/s: throttling within the
    /// budget, goodput near the paced rate.
    fn verify_rate(&self, rate: f64) -> Option<Verification> {
        let v = self.verify(
            format!("re-run paced at {rate:.0} operations/s: throttling should disappear"),
            &[("rate", format!("{rate:.0}"))],
        )?;
        let goodput = super::gate::good_ops_per_s(self.r);
        let mut v = v.expecting(Expectation {
            metric: "summary.throttled_request_ratio".into(),
            direction: Direction::AtMost,
            predicted: 0.0,
            lo: 0.0,
            hi: THROTTLE_BUDGET_RATIO,
            unit: "ratio".into(),
        });
        // Paced below what it achieved, the client gets what it asks for (or what it had).
        let predicted = rate.min(goodput.max(rate * 0.5));
        v = v.expecting(Expectation {
            metric: "summary.ops_per_s".into(),
            direction: Direction::AtLeast,
            predicted,
            lo: predicted * 0.8,
            hi: rate * 1.05,
            unit: "ops_per_s".into(),
        });
        Some(v)
    }

    /// The verification of running at `to` workers.
    fn verify_concurrency(&self, description: String, to: u64) -> Option<Verification> {
        let mut v = self.verify(description, &[("concurrency", to.to_string())])?;
        v.expect = self.concurrency_expectations(to);
        Some(v)
    }

    /// "… (the in-flight ceiling is N requests: at most M workers)" when one is known.
    fn ceiling_note(&self) -> String {
        match (self.cap.inflight_ceiling, self.safe) {
            (Some(c), Some(s)) => format!(
                "the service throttles above ~{:.0} requests in flight ({} per operation at the peak): at most {s} workers{}",
                c.lo,
                self.cap.per_op.peak_inflight_per_op,
                if self.cap.source == super::capacity::CapacitySource::History {
                    " (remembered from an earlier run)"
                } else {
                    ""
                }
            ),
            _ => String::new(),
        }
    }
}

/// An expectation that a latency metric falls to the replayed value.
fn latency_expectation(metric: &str, p: &Prediction) -> Expectation {
    Expectation {
        metric: metric.into(),
        direction: Direction::Decrease,
        predicted: p.predicted,
        lo: p.predicted_lo,
        hi: p.predicted_hi,
        unit: "ms".into(),
    }
}

/// Evidence values; every key carries its unit like a report field (design §6).
fn values(kv: &[(&str, f64)]) -> BTreeMap<String, f64> {
    for (k, v) in kv {
        debug_assert!(
            crate::model::has_unit(k),
            "evidence value `{k}` has no unit suffix"
        );
        // A count is a whole number (design v4 §4); fractions are `_mean` / `_ratio`.
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

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

/// Seconds of the window where `pred` holds for some request, merged into windows.
fn windows_where(reqs: &[ReqRow], pred: impl Fn(&ReqRow) -> bool) -> Vec<TimeWindow> {
    let mut secs: Vec<i64> = reqs
        .iter()
        .filter(|r| pred(r))
        .map(|r| r.t_us.div_euclid(1_000_000))
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
    out.truncate(8);
    out
}

/// Slowest operation ids among `ids` (at most 5).
fn slowest_ops(run: &Run, ids: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let lat: HashMap<u64, u64> = run.ops.iter().map(|o| (o.op_id, o.latency_us)).collect();
    let mut v: Vec<u64> = ids.into_iter().collect();
    v.sort_unstable();
    v.dedup();
    v.sort_by_key(|id| std::cmp::Reverse(lat.get(id).copied().unwrap_or(0)));
    v.truncate(5);
    v
}

fn pred_text(p: &Prediction) -> String {
    let unit = if p.metric.starts_with("ops_per_s") {
        " ops/s"
    } else {
        " ms"
    };
    format!(
        "{} would take {} from {:.0}{unit} to ~{:.0}{unit} (estimate)",
        p.setting,
        p.metric
            .split(' ')
            .next()
            .unwrap_or("it")
            .replace("_ms", ""),
        p.current,
        p.predicted
    )
}

/// Evidence values of a prediction, named after the predicted metric and its unit:
/// `current_p99_ms` / `predicted_p99_ms` (or `…_p50_ms`, `…_ops_per_s`) and the change in
/// requests per 1 000 operations (`extra_requests_per_1k_ops_mean`, negative = saved).
fn pred_values(p: &Prediction) -> [(&'static str, f64); 3] {
    let (cur, pred) = match p.metric.split(' ').next().unwrap_or("") {
        "p99_ms" => ("current_p99_ms", "predicted_p99_ms"),
        "p50_ms" => ("current_p50_ms", "predicted_p50_ms"),
        "ops_per_s" => ("current_ops_per_s", "predicted_ops_per_s"),
        _ => ("current_ms", "predicted_ms"),
    };
    [
        (cur, p.current),
        (pred, p.predicted),
        (
            "extra_requests_per_1k_ops_mean",
            p.requests_per_1k_ops_delta,
        ),
    ]
}

// ---------------------------------------------------------------------------------------

/// Failures of one error class (`resilience.error_classes`) and what to do about them.
fn class_advice(class: &str, http: crate::net::HttpVersion) -> String {
    match class {
        "permission_denied" | "config_invalid" => "credentials or permissions were refused: run `oprobe doctor` to check the credentials, the bucket policy and the endpoint".into(),
        "not_found" => "objects were missing: check the `--existing` prefix and that nothing deleted the dataset during the run".into(),
        "timeout" => "requests timed out: raise `--timeout`, or find the slow server with `oprobe analyze RUN --slowest 2 --by remote`".into(),
        "tail_cut" => "tail-cut cancelled requests until the retries ran out: raise `--retries` or turn tail-cut off".into(),
        "connect" | "network" => "connections failed or were reset: check the endpoint, proxy and network path (`oprobe doctor`, `oprobe probe ENDPOINT`)".into(),
        "server_error" => "the service answered 5xx: check its health (`oprobe analyze RUN --by status`); retries absorb short bursts".into(),
        "condition_not_match" | "already_exists" | "conflict" => "conditional writes lost a race with another writer: expected under contention on the same keys".into(),
        "range_not_satisfied" => "reads asked for ranges past the end of the objects: check the object size and the range settings".into(),
        "unsupported" => "the service does not support an operation of the mix: change the mix or the preset".into(),
        _ if http == crate::net::HttpVersion::H2 => "unclassified failures with HTTP/2 forced: the service may not speak HTTP/2 (re-run with `--http h1` or `--http auto`)".into(),
        other => format!("`{other}` failures: inspect the failing requests with `oprobe analyze RUN --by status`"),
    }
}

/// Failed operations by error class, most common first, without `rate_limited` (failures
/// that ran out of retries on 429/503 belong to the throttling finding).
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

/// Server errors (5xx other than 503) the client retried until they succeeded: reported even
/// when the run also throttles (A6), since they are a different problem.
fn retried_server_errors(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let n = r.resilience.server_errors;
    let requests = r.connections.requests.max(1);
    if n < 3 || (n as f64) < requests as f64 * 0.001 {
        return None;
    }
    let ratio = n as f64 / requests as f64;
    Some(Finding {
        code: "errors".into(),
        severity: Severity::Warning,
        cause: format!(
            "{:.2}% of requests failed with a server error (5xx) and were retried.",
            ratio * 100.0
        ),
        evidence: Evidence {
            text: format!(
                "{n} server errors (5xx other than 503) in {requests} requests; every operation succeeded after retries{}",
                if c.throttled {
                    " (the run also throttles: see throttling)"
                } else {
                    ""
                }
            ),
            values: values(&[
                ("server_errors_count", n as f64),
                ("server_error_ratio", ratio),
            ]),
            op_ids: Vec::new(),
            windows: windows_where(&c.run.reqs, |q| q.status >= 500 && q.status != 503),
        },
        advice: "The service answers some requests with 5xx: keep retries with jitter; if the share grows or persists, check the service's health and contact the provider (each retry costs a request and latency).".into(),
        verification: None,
    })
}

fn errors(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let s = &r.summary;
    if s.failed_ops == 0 {
        return retried_server_errors(c);
    }
    let rate_limited = r
        .resilience
        .error_classes
        .get("rate_limited")
        .copied()
        .unwrap_or(0);
    let classes = failure_classes(r);
    // Every failure was a throttled request out of retries: the throttling finding says so
    // (server errors among the retries are still reported).
    if rate_limited > 0 && classes.is_empty() && c.throttled {
        return retried_server_errors(c);
    }
    let failed: Vec<u64> = c
        .run
        .ops
        .iter()
        .filter(|o| !o.ok)
        .map(|o| o.op_id)
        .collect();
    let by_class: Vec<String> = classes.iter().map(|(k, n)| format!("{k} ×{n}")).collect();
    let mut text = format!("{} of {} operations failed", s.failed_ops, s.ops);
    if !by_class.is_empty() {
        text += &format!(": {}", by_class.join(", "));
    }
    if rate_limited > 0 {
        text +=
            &format!(" (and {rate_limited} throttled operations out of retries: see throttling)");
    }
    let http = r.knobs.requested.http_version;
    let advice: Vec<String> = if classes.is_empty() {
        vec![class_advice("other", http)]
    } else {
        classes
            .iter()
            .take(3)
            .map(|(k, _)| class_advice(k, http))
            .collect()
    };
    let mut advice = advice.join("; ");
    if let Some(first) = advice.get(..1) {
        advice = first.to_uppercase() + &advice[1..];
    }
    advice.push('.');
    // Forced HTTP/2 with unclassified failures: test HTTP/1.1.
    let verification = (http == crate::net::HttpVersion::H2
        && classes
            .first()
            .is_some_and(|(k, _)| matches!(*k, "unexpected" | "other" | "network")))
    .then(|| {
        c.verify(
            "re-run over HTTP/1.1: the failures should disappear",
            &[("http_version", "h1".into())],
        )
        .map(|v| {
            v.expecting(Expectation {
                metric: "summary.error_ratio".into(),
                direction: Direction::AtMost,
                predicted: 0.0,
                lo: 0.0,
                hi: r.sla.max_error_ratio,
                unit: "ratio".into(),
            })
        })
    })
    .flatten();
    Some(Finding {
        code: "errors".into(),
        severity: if s.error_rate > r.sla.max_error_ratio {
            Severity::Critical
        } else {
            Severity::Warning
        },
        cause: format!("{:.2}% of operations failed.", s.error_rate * 100.0),
        evidence: Evidence {
            text,
            values: values(&[
                ("failed_ops_count", s.failed_ops as f64),
                ("error_ratio", s.error_rate),
            ]),
            op_ids: slowest_ops(c.run, failed),
            windows: windows_where(&c.run.reqs, |q| {
                q.is_error() && !q.is_throttle() && q.status != 404
            }),
        },
        advice,
        verification,
    })
}

fn throttling(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let thr = r.resilience.throttled_responses;
    if thr == 0 {
        return None;
    }
    let (req_ratio, op_ratio) = (
        r.summary.throttled_request_ratio,
        r.summary.throttled_op_ratio,
    );
    let cc = c.concurrency();
    let out_of_retries = r
        .resilience
        .error_classes
        .get("rate_limited")
        .copied()
        .unwrap_or(0);
    let mut text = format!(
        "{thr} throttled responses ({:.2}% of requests, {:.2}% of operations); {} retries, {:.0}% of operation time in retry backoff; max Retry-After {} ms",
        req_ratio * 100.0,
        op_ratio * 100.0,
        r.resilience.retries,
        r.stages.shares.retry_pct,
        r.resilience.max_retry_after_ms
    );
    if out_of_retries > 0 {
        text += &format!("; {out_of_retries} operations failed when their retries ran out");
    }
    let mut vals = vec![
        ("throttled_responses_count", thr as f64),
        ("throttled_request_ratio", req_ratio),
        ("throttled_op_ratio", op_ratio),
        ("retry_pct", r.stages.shares.retry_pct),
    ];
    if out_of_retries > 0 {
        vals.push(("rate_limited_failures_count", out_of_retries as f64));
    }
    let advice;
    let verification;
    let mut severity = Severity::Critical;
    let cap = c.cap;
    let sel = cap.selected().cloned();
    let asserted = cap.asserted().map(|h| h.kind);
    if let Some(h) = &sel {
        text += &format!("; limit model: {}", h.summary());
        if let Some(e) = h.evidence.first() {
            text += &format!(" — {e}");
        }
    }
    let headroom = super::capacity::DEFAULT_HEADROOM_RATIO;
    let inflight = matches!(sel.as_ref().map(|h| h.kind), Some(HypothesisKind::InflightLimit) | None);
    match (cap.inflight_ceiling, c.safe) {
        (Some(ce), Some(safe)) if inflight => {
            // Always fewer workers than now: the run throttled at its own concurrency.
            let to = u64::from(safe).min(cc.saturating_sub(1)).max(1);
            let peak = cap.per_op.peak_inflight_per_op;
            text += &format!(
                "; throttling starts between {:.0} and {:.0} requests in flight, and one operation keeps up to {peak:.0} in flight",
                ce.lo, ce.hi
            );
            vals.push(("ceiling_lo_inflight_count", ce.lo));
            vals.push(("ceiling_hi_inflight_count", ce.hi));
            vals.push(("peak_inflight_per_op_count", peak));
            vals.push(("safe_concurrency_count", to as f64));
            let p = cap.predict(to as u32);
            advice = format!(
                "The service throttles above ~{:.0} requests in flight: run at concurrency ≤ {to} ({peak:.0} requests in flight per operation at the peak, {:.0}% headroom; predicted {:.0} ops/s), or cap `max_inflight_requests` at {:.0}; keep retries with jitter.",
                ce.lo,
                headroom * 100.0,
                p.ops_per_s,
                (ce.lo * (1.0 - headroom)).floor().max(1.0)
            );
            verification = c.verify_concurrency(
                format!(
                    "re-run at concurrency {to}: throttling should stay within {:.1}% of requests at ~{:.0} ops/s",
                    THROTTLE_BUDGET_RATIO * 100.0,
                    p.ops_per_s
                ),
                to,
            );
        }
        _ => match (asserted, sel.as_ref()) {
            (Some(HypothesisKind::RateLimit | HypothesisKind::TimeVarying), Some(h)) => {
                let rps = h.params.requests_per_s.unwrap_or(0.0);
                let rate = cap.safe_rate(headroom).unwrap_or(1.0).floor().max(1.0);
                let rpo = cap.clean_requests_per_op();
                vals.push(("rate_limit_requests_per_s", rps));
                vals.push(("safe_rate_ops_per_s", rate));
                advice = if h.kind == HypothesisKind::TimeVarying {
                    format!(
                        "The service's rate limit changes over time ({}): pace the client below the lowest one, ≤ {rate:.0} operations/s (`--rate {rate:.0}`; {rpo:.1} requests per operation, {:.0}% headroom), or adapt the rate to the throttling (AIMD); fewer workers alone would not help; keep retries with jitter.",
                        h.summary(),
                        headroom * 100.0
                    )
                } else {
                    format!(
                        "The service limits the request rate (≈{rps:.0} requests/s), not concurrency: pace the client at ≤ {rate:.0} operations/s (`--rate {rate:.0}`; {rpo:.1} requests per operation, {:.0}% headroom) or with a client-side rate limiter; fewer workers would not help; keep retries with jitter.",
                        headroom * 100.0
                    )
                };
                verification = c.verify_rate(rate);
            }
            (Some(HypothesisKind::KeyLimit), Some(h)) => {
                let rps = h.params.requests_per_s.unwrap_or(0.0);
                let key = h.params.key.clone().unwrap_or_default();
                let share = h.params.key_share_ratio.unwrap_or(0.0);
                vals.push(("key_limit_requests_per_s", rps));
                vals.push(("hot_key_share_ratio", share));
                let reads = c.r.workload.mix.iter().any(|m| m.op == "read" || m.op == "parquet_scan");
                let rate = cap.safe_rate(headroom).unwrap_or(1.0).floor().max(1.0);
                advice = format!(
                    "Hot keys exceed a per-key limit of ≈{rps:.0} requests/s (`{key}` takes {:.0}% of the requests): spread hot objects over more keys or prefixes (salting, replicas), cache them (`--cache 256MiB`, a CDN), or pace the whole client at ≤ {rate:.0} operations/s; adding or removing workers only moves the throttling.",
                    share * 100.0
                );
                verification = if reads && !c.r.knobs.requested.cache {
                    c.verify(
                        "cache the hot objects (256 MiB read cache): the per-key throttling should disappear",
                        &[("cache", "256MiB".into())],
                    )
                    .map(|v| {
                        v.expecting(Expectation {
                            metric: "summary.throttled_request_ratio".into(),
                            direction: Direction::AtMost,
                            predicted: 0.0,
                            lo: 0.0,
                            hi: THROTTLE_BUDGET_RATIO,
                            unit: "ratio".into(),
                        })
                        .expecting(Expectation {
                            metric: "summary.ops_per_s".into(),
                            direction: Direction::AtLeast,
                            predicted: 1.0,
                            lo: 1.0,
                            hi: 2.0,
                            unit: "baseline_ratio".into(),
                        })
                    })
                } else {
                    c.verify_rate(rate)
                };
            }
            (Some(HypothesisKind::None), Some(_)) => {
                severity = Severity::Warning;
                advice = "The throttling follows neither the load nor the request rate nor the keys (random 503s: a noisy neighbour, a gateway, a partition split): slowing down would cost throughput without removing it. Keep retries with jitter and exponential backoff; if it persists, ask the provider.".into();
                verification = None;
            }
            _ => {
                let best = sel.as_ref().map_or_else(|| "none".to_owned(), |h| h.summary());
                advice = format!(
                    "Inconclusive: no limit model explains the throttling with confidence (best: {best}). Run `oprobe tune {}` (it steps the load up and down and fits the limit) before changing the load; keep retries with jitter.",
                    target_args(&c.r.target).join(" ")
                );
                verification = None;
            }
        },
    }
    let ids = c.run.ops.iter().filter(|o| o.throttled).map(|o| o.op_id);
    Some(Finding {
        code: "throttling".into(),
        severity,
        cause: format!(
            "The service throttled {:.1}% of requests (429/503).",
            req_ratio * 100.0
        ),
        evidence: Evidence {
            text,
            values: values(&vals),
            op_ids: slowest_ops(c.run, ids),
            windows: windows_where(&c.run.reqs, ReqRow::is_throttle),
        },
        advice,
        verification,
    })
}

fn retries(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let share = r.stages.shares.retry_pct;
    if c.throttled || share < 5.0 {
        return None;
    }
    let ids = c.run.ops.iter().filter(|o| o.attempts > 1).map(|o| o.op_id);
    // With tail-cut on, the layer's cancellations are retried like failures: the backoff is
    // the price of the cut, not a service problem.
    let tail_cut = r.knobs.requested.tail_cut;
    Some(Finding {
        code: "retries".into(),
        severity: if tail_cut {
            Severity::Info
        } else {
            Severity::Warning
        },
        cause: if tail_cut {
            format!("{share:.0}% of operation time is retry backoff, mostly tail-cut re-issues.")
        } else {
            format!("{share:.0}% of operation time is retry backoff after failed requests.")
        },
        evidence: Evidence {
            text: format!(
                "{} retries, {} total backoff",
                r.resilience.retries,
                fmt_ms(r.resilience.retry_backoff_ms)
            ),
            values: values(&[
                ("retry_pct", share),
                ("retries_count", r.resilience.retries as f64),
            ]),
            op_ids: slowest_ops(c.run, ids),
            windows: windows_where(&c.run.reqs, |q| q.attempt > 1),
        },
        advice: if tail_cut {
            "Each request tail-cut cancels is re-issued after the retry backoff (50–100 ms): compare p99 with and without tail-cut (`oprobe compare`) to see whether the cut pays for its wait.".into()
        } else {
            "Requests fail with temporary errors and are retried after a backoff: inspect them with `oprobe analyze RUN --where attempt=retry --by status`.".into()
        },
        verification: None,
    })
}

/// A tail-cut must take at least this share off the affected operations' p99 to be advice.
const TAIL_CUT_MIN_GAIN: f64 = 1.0 / 3.0;

/// APIs whose replayed gain is within this share of the best one count as equally good; among
/// them the one with the most stragglers wins (then the name), so the named API does not flip
/// between runs of the same workload on noise.
const STRAGGLER_TIE_RATIO: f64 = 0.75;

/// Stragglers of one API that the measured client's tail-cut layer would cut.
struct Stragglers<'a> {
    api: String,
    /// The replay on a long-lived client (the layer always active).
    steady: TailCutReplay,
    deadline_us: u64,
    /// Successful requests slower than the layer's deadline.
    over: Vec<&'a ReqRow>,
}

/// The API whose stragglers `TailCutLayer` (the measured client's own rule, via
/// [`simulate::tail_cut`]) would cut best. `None` when no API has requests above its
/// deadline that mostly wait for the first byte, or the replay predicts no substantial gain:
/// e.g. when stragglers are more than 5 % of the requests, the layer's p95 deadline sits above
/// them and it cuts nothing.
fn straggler_candidate<'a>(run: &'a Run, setup: &TailCutSetup) -> Option<Stragglers<'a>> {
    let mut apis: Vec<&str> = run.reqs.iter().map(|q| q.api.as_ref()).collect();
    apis.sort_unstable();
    apis.dedup();
    let steady = setup.steady();
    let mut found: Vec<(f64, Stragglers)> = Vec::new();
    for api in apis {
        let Some(replay) = simulate::tail_cut(run, api, &steady) else {
            continue;
        };
        let Some(deadline_us) = replay.deadline_us else {
            continue;
        };
        let over: Vec<&ReqRow> = run
            .reqs
            .iter()
            .filter(|q| q.api == api && !q.is_error() && q.total_us > deadline_us)
            .collect();
        let ttfb_bound = over.iter().filter(|q| dominant_stage(q) == "ttfb").count();
        if over.len() < 3 || ttfb_bound * 2 < over.len() {
            continue;
        }
        let p = &replay.prediction;
        let gain = p.current - p.predicted;
        if p.current <= 0.0 || gain < TAIL_CUT_MIN_GAIN * p.current {
            continue;
        }
        found.push((
            gain,
            Stragglers {
                api: api.to_owned(),
                steady: replay,
                deadline_us,
                over,
            },
        ));
    }
    let best = found.iter().map(|f| f.0).fold(0.0, f64::max);
    found
        .into_iter()
        .filter(|f| f.0 >= STRAGGLER_TIE_RATIO * best)
        .max_by(|a, b| {
            a.1.over
                .len()
                .cmp(&b.1.over.len())
                .then_with(|| b.1.api.cmp(&a.1.api))
        })
        .map(|f| f.1)
}

/// The one tail finding: p99 ≫ median, explained by the dominant stage of the slow requests.
fn tail(c: &Ctx, have: &[Finding]) -> Option<Finding> {
    let run = c.run;
    if run.ops.len() < 50 || run.reqs.len() < 50 {
        return None;
    }
    let mut lat: Vec<u64> = run.ops.iter().map(|o| o.latency_us).collect();
    let (p50, p99) = (
        quantile_us_ms(&mut lat, 0.5),
        quantile_us_ms(&mut lat, 0.99),
    );
    if p50 <= 0.5 || p99 < 5.0 * p50 {
        return None;
    }
    // The slowest 2 % of requests: where did their time go?
    let sel = Selection::slowest(&run.reqs, 2.0);
    let min = sel.min_total_us.unwrap_or(0);
    let slow: Vec<&ReqRow> = run.reqs.iter().filter(|q| q.total_us >= min).collect();
    let mut by_stage: HashMap<&'static str, u64> = HashMap::new();
    for q in &slow {
        *by_stage.entry(dominant_stage(q)).or_default() += 1;
    }
    // Most common stage; ties broken by name, so the cause is stable.
    let (stage, _) = by_stage
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))?;
    // A cause that already has its own finding explains the tail there.
    let owner = match *stage {
        "queue" => Some("client_queue"),
        "connect" => Some("cold_connections"),
        "stall" => Some("consumer_stall"),
        _ => None,
    };
    if owner.is_some_and(|o| have.iter().any(|f| f.code == o))
        || (c.throttled
            && slow
                .iter()
                .filter(|q| q.attempt > 1 || q.is_throttle())
                .count()
                * 2
                > slow.len())
    {
        return None;
    }
    let attr = attribute(&run.reqs, &sel);
    let factors: Vec<String> = attr
        .factors
        .iter()
        .take(3)
        .map(|f| {
            format!(
                "{}={} ({:.0}% of slow vs {:.0}% of the rest)",
                f.attribute,
                f.value,
                f.selected_ratio * 100.0,
                f.rest_ratio * 100.0
            )
        })
        .collect();
    let slow_ids = slowest_ops(run, slow.iter().map(|q| q.op_id).filter(|id| *id != 0));
    let mut vals = vec![
        ("p50_ms", p50),
        ("p99_ms", p99),
        ("p99_to_p50_ratio", p99 / p50),
    ];
    let mut ev_text = format!("operation p50 {} · p99 {}", fmt_ms(p50), fmt_ms(p99));
    if !factors.is_empty() {
        ev_text += &format!(
            "; slow requests are over-represented in {}",
            factors.join(", ")
        );
    }
    // Stragglers: requests of one API far slower than that API's own typical request, waiting
    // for the server's first byte, that the measured client's tail-cut layer would cut. The
    // API is the one where the replay saves the most (near-ties go to the one with the most
    // stragglers), not the one most common among the slowest requests.
    let tail_cut_on = c.r.knobs.requested.tail_cut;
    let setup = TailCutSetup::of(c.r);
    if let Some(st) = setup.as_ref().and_then(|s| straggler_candidate(run, s)) {
        let setup = setup.expect("candidate implies a setup");
        let api = &st.api;
        let pred = &st.steady.prediction;
        let all_api = run
            .reqs
            .iter()
            .filter(|q| q.api == *api && !q.is_error())
            .count();
        let share = st.over.len() as f64 * 100.0 / all_api.max(1) as f64;
        let mut ttfb: Vec<u64> = st.over.iter().map(|q| q.ttfb_us).collect();
        let wait = quantile_us_ms(&mut ttfb, 0.5);
        let deadline = fmt_ms(ms(st.deadline_us));
        let window = c.r.run.window_s;
        let blind_s = setup.blind_s(window);
        let blind = blind_s > 0.25 * window;
        vals.push(("straggler_pct", share));
        vals.push(("straggler_ttfb_ms", wait));
        vals.extend(pred_values(pred));
        vals.push(("tail_cut_deadline_ms", ms(st.deadline_us)));
        if blind {
            vals.push(("tail_cut_blind_s", blind_s));
        }
        ev_text += &format!(
            "; {} — about {:.0} extra requests per 1 000 operations",
            pred.note, pred.requests_per_1k_ops_delta
        );
        // The layer is blind for its first 10 s: a run that short needs a longer window to
        // show the effect, so the verification lengthens it.
        let longer = format!(
            "{:.0}s",
            (window.max(10.0) + blind_s).ceil()
                + crate::engine::stack::TAIL_CUT_BLIND_START.as_secs_f64()
        );
        let expectation = (pred.replayed_ops_count * 10 >= c.r.summary.ops * 9)
            .then(|| latency_expectation("summary.latency.p99_ms", pred));
        let (cause, advice, verification) = if tail_cut_on {
            let live = simulate::tail_cut(run, api, &setup);
            let cut = live.as_ref().map_or(0, |l| l.cut_count);
            ev_text += &format!(
                "; with tail-cut on, {} of {} stragglers started while the layer was still blind",
                live.as_ref().map_or(0, |l| l.blind_count),
                st.over.len()
            );
            let cause = if blind {
                format!(
                    "Tail-cut is on, yet {share:.1}% of {api} requests still wait ~{} for the first byte: the layer cuts nothing during its first 10 s, which covered {blind_s:.1} s of this {window:.1} s window.",
                    fmt_ms(wait)
                )
            } else {
                format!(
                    "Tail-cut is on, yet {share:.1}% of {api} requests still wait ~{} for the first byte (deadline {deadline}; ~{cut} would be cut).",
                    fmt_ms(wait)
                )
            };
            let advice = if blind {
                format!(
                    "OpenDAL's TailCutLayer counts no latencies during its first 10 s, so a short run is never cut (a long-lived client is unaffected): measure with a longer run (`--duration {longer}`) to see its effect."
                )
            } else {
                "Tail-cut is already on: check the slow requests' remotes (`oprobe analyze RUN --slowest 2 --by remote`) and the service's health.".into()
            };
            let verification = blind
                .then(|| {
                    c.verify(
                        format!(
                            "measure long enough for the layer to engage: p99 should fall to ~{}",
                            fmt_ms(pred.predicted)
                        ),
                        &[("duration", longer.clone())],
                    )
                })
                .flatten()
                .map(|mut v| {
                    v.expect.extend(expectation.clone());
                    v
                });
            (cause, advice, verification)
        } else {
            let cause = format!(
                "{share:.1}% of {api} requests wait ~{} for the server's first byte (stragglers); tail-cut (deadline {deadline}) would cut p99 from {} to ~{}.",
                fmt_ms(wait),
                fmt_ms(pred.current),
                fmt_ms(pred.predicted)
            );
            let mut sets = vec![("tail_cut", "on".to_owned())];
            if blind {
                sets.push(("duration", longer.clone()));
            }
            let verification = c
                .verify(
                    format!(
                        "enable tail-cut and compare p99 (predicted ~{}){}",
                        fmt_ms(pred.predicted),
                        if blind {
                            "; the run is lengthened because the layer is blind for its first 10 s"
                        } else {
                            ""
                        }
                    ),
                    &sets,
                )
                .map(|mut v| {
                    v.expect.extend(expectation.clone());
                    v
                });
            (
                cause,
                "Cut server stragglers with `tail_cut = true` (OpenDAL TailCutLayer re-issues requests slower than p95 × 1.5, at least 50 ms; it starts cutting 10 s after the client is built); the extra requests cost a little.".into(),
                verification,
            )
        };
        return Some(Finding {
            code: "tail_stragglers".into(),
            severity: Severity::Warning,
            cause,
            evidence: Evidence {
                text: ev_text,
                values: values(&vals),
                op_ids: slowest_ops(run, st.over.iter().map(|q| q.op_id).filter(|id| *id != 0)),
                windows: Vec::new(),
            },
            advice,
            verification,
        });
    }
    Some(Finding {
        code: "tail_latency".into(),
        severity: Severity::Warning,
        cause: format!(
            "p99 is {:.1}× the median; the slowest requests spend most of their time in {stage}.",
            p99 / p50
        ),
        evidence: Evidence {
            text: ev_text,
            values: values(&vals),
            op_ids: slow_ids,
            windows: Vec::new(),
        },
        advice: if tail_cut_on && *stage == "ttfb" {
            format!(
                "Tail-cut is on but cuts little here: its deadline (p95 × 1.5) sits above the slow requests when they are more than ~5% of them. Look at the slowest operations (`oprobe analyze RUN --slowest 2 --by stage`); the {stage} stage is where the tail comes from."
            )
        } else {
            format!(
                "Look at the slowest operations (`oprobe analyze RUN --slowest 2 --by stage`); the {stage} stage is where the tail comes from."
            )
        },
        verification: None,
    })
}

fn client_queue(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let share = r.stages.shares.queue_pct;
    if share < 10.0 {
        return None;
    }
    let pred = simulate::uncapped_inflight(c.run);
    let cap = r.knobs.requested.max_inflight_requests_count;
    let mut text = format!(
        "{share:.0}% of operation time queued; request queue p99 {} (cap {} in-flight, peak {})",
        fmt_ms(r.stages.request_p99.queue_ms),
        if cap == 0 {
            "none".to_owned()
        } else {
            cap.to_string()
        },
        r.connections.peak_inflight_requests
    );
    let mut vals = vec![
        ("queue_pct", share),
        ("queue_p99_ms", r.stages.request_p99.queue_ms),
    ];
    if let Some(p) = &pred {
        text += &format!("; {}", pred_text(p));
        vals.extend(pred_values(p));
    }
    let ids = c
        .run
        .ops
        .iter()
        .filter(|o| o.breakdown.queue_us * 2 > o.latency_us)
        .map(|o| o.op_id);
    // The cap to test: none (0) unless the uncapped peak would cross the in-flight ceiling,
    // then the most the ceiling allows (and nothing when that is no more than now).
    let uncap: Option<u64> = (cap > 0)
        .then(|| match c.cap.inflight_ceiling {
            Some(ce) => {
                let allowed =
                    (ce.lo * (1.0 - super::capacity::DEFAULT_HEADROOM_RATIO)).floor() as u64;
                let uncapped =
                    c.concurrency() as f64 * c.r.knobs.requested.io_concurrency_count.max(1) as f64;
                if uncapped <= allowed as f64 {
                    Some(0)
                } else {
                    (allowed > cap).then_some(allowed)
                }
            }
            None => Some(0),
        })
        .flatten();
    Some(Finding {
        code: "client_queue".into(),
        severity: Severity::Warning,
        cause: "Operations wait for in-flight request permits before anything is sent.".into(),
        evidence: Evidence {
            text,
            values: values(&vals),
            op_ids: slowest_ops(c.run, ids),
            windows: Vec::new(),
        },
        advice: match uncap {
            Some(0) | None => "Raise `max_inflight_requests` (0 = unlimited) or lower `concurrency × io_concurrency` so requests stop queueing behind each other.".into(),
            Some(n) => format!("Raise `max_inflight_requests` to {n} (not unlimited: {}), or lower `concurrency × io_concurrency` so requests stop queueing behind each other.", c.ceiling_note()),
        },
        verification: uncap
            .and_then(|n| {
                let what = if n == 0 {
                    "remove the in-flight cap and compare p99".to_owned()
                } else {
                    format!("raise the in-flight cap to {n} and compare p99")
                };
                c.verify(what, &[("max_inflight_requests", n.to_string())])
                    .map(|mut v| {
                        if let Some(p) = &pred {
                            v.expect.push(latency_expectation("summary.latency.p99_ms", p));
                        }
                        v
                    })
            }),
    })
}

fn cold_connections(c: &Ctx) -> Option<Finding> {
    if !c.network {
        return None;
    }
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
    let reuse = c.r.connections.reuse_pct;
    if share < 10.0 && !(reuse < 80.0 && reqs.len() > 50) {
        return None;
    }
    let p50 = |f: &dyn Fn(&ReqRow) -> u64| {
        let mut v: Vec<u64> = cold.iter().map(|q| f(q)).filter(|x| *x > 0).collect();
        quantile_us_ms(&mut v, 0.5)
    };
    let (dns, tcp, tls) = (p50(&|q| q.dns_us), p50(&|q| q.tcp_us), p50(&|q| q.tls_us));
    let keep_alive = c.r.knobs.requested.keep_alive;
    let resumed = cold
        .iter()
        .filter(|q| q.tls.as_ref().is_some_and(|t| t.resumed))
        .count();
    let mut text = format!(
        "{} of {} requests opened a connection (reuse {reuse:.0}%); DNS p50 {} · TCP p50 {} · TLS p50 {}",
        cold.len(),
        reqs.len(),
        fmt_ms(dns),
        fmt_ms(tcp),
        fmt_ms(tls)
    );
    if tls > 0.0 {
        text += &format!("; {resumed} TLS resumptions");
    }
    let slowest = if tls >= dns.max(tcp) {
        "TLS"
    } else if dns >= tcp {
        "DNS"
    } else {
        "TCP"
    };
    Some(Finding {
        code: "cold_connections".into(),
        severity: if share >= 10.0 {
            Severity::Warning
        } else {
            Severity::Info
        },
        cause: format!(
            "Connection setup takes {share:.0}% of request time ({} new connections; {slowest} is the slowest step).",
            cold.len()
        ),
        evidence: Evidence {
            text,
            values: values(&[
                ("connect_pct", share),
                ("cold_requests_count", cold.len() as f64),
                ("reuse_pct", reuse),
                ("dns_p50_ms", dns),
                ("tcp_p50_ms", tcp),
                ("tls_p50_ms", tls),
            ]),
            op_ids: slowest_ops(c.run, cold.iter().map(|q| q.op_id).filter(|i| *i != 0)),
            windows: windows_where(reqs, |q| !q.reused && q.connect_us() > 0),
        },
        advice: if keep_alive {
            "Reuse connections: drain response bodies so connections return to the pool, keep the pool large enough for the concurrency, and prefer HTTP/2 where the service offers it.".into()
        } else {
            "Turn keep-alive back on: every request currently pays DNS + TCP + TLS.".into()
        },
        verification: if keep_alive {
            c.verify(
                "multiplex on fewer connections",
                &[("http_version", "h2".into())],
            )
        } else {
            c.verify("re-run with keep-alive", &[("keep_alive", "on".into())])
        },
    })
}

fn http1(c: &Ctx) -> Option<Finding> {
    if !c.network {
        return None;
    }
    let reqs = &c.run.reqs;
    let h1 = reqs
        .iter()
        .filter(|q| q.protocol == crate::model::WireProtocol::H1)
        .count();
    if reqs.len() < 100 || h1 * 10 < reqs.len() * 9 {
        return None;
    }
    let mut conns: Vec<u64> = reqs.iter().map(|q| q.conn_id).collect();
    conns.sort_unstable();
    conns.dedup();
    let queue = c.r.stages.shares.queue_pct;
    if conns.len() < 16 && queue < 5.0 {
        return None;
    }
    if c.r.knobs.requested.http_version == crate::net::HttpVersion::H1 {
        return None;
    }
    Some(Finding {
        code: "http1_connections".into(),
        severity: Severity::Info,
        cause: format!(
            "Requests use HTTP/1.1: {} connections for {} requests, one per in-flight request.",
            conns.len(),
            reqs.len()
        ),
        evidence: Evidence {
            text: format!(
                "{:.0}% of requests on HTTP/1.1, {} distinct connections, {queue:.0}% of op time queued",
                h1 as f64 * 100.0 / reqs.len() as f64,
                conns.len()
            ),
            values: values(&[("connections_count", conns.len() as f64), ("queue_pct", queue)]),
            ..Default::default()
        },
        advice: "If the service speaks HTTP/2, `--http h2` multiplexes requests over a few connections (fewer handshakes, no per-connection pool limits). Compare both: some S3-compatible services are faster on HTTP/1.1.".into(),
        verification: c.verify("measure the same workload over HTTP/2", &[("http_version", "h2".into())]),
    })
}

fn remote_skew(c: &Ctx) -> Option<Finding> {
    let mut by: HashMap<String, Vec<u64>> = HashMap::new();
    for q in c.run.reqs.iter().filter(|q| !q.is_throttle()) {
        if let Some(a) = q.remote {
            by.entry(a.ip().to_string())
                .or_default()
                .push(q.ttfb_us + q.body_us);
        }
    }
    by.retain(|_, v| v.len() >= 20);
    if by.len() < 2 {
        return None;
    }
    let mut p50s: Vec<(String, f64, usize)> = by
        .into_iter()
        .map(|(ip, mut v)| {
            let n = v.len();
            (ip, quantile_us_ms(&mut v, 0.5), n)
        })
        .collect();
    p50s.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (worst, wp50, n) = p50s[0].clone();
    let others = median(&p50s[1..].iter().map(|x| x.1).collect::<Vec<_>>());
    if others <= 0.0 || wp50 < 3.0 * others || wp50 - others < 5.0 {
        return None;
    }
    let ids = c
        .run
        .reqs
        .iter()
        .filter(|q| q.remote.is_some_and(|a| a.ip().to_string() == worst))
        .map(|q| q.op_id)
        .filter(|i| *i != 0);
    Some(Finding {
        code: "remote_skew".into(),
        severity: Severity::Warning,
        cause: format!(
            "One front-end ({worst}) answers {:.1}× slower than the other {} remotes.",
            wp50 / others,
            p50s.len() - 1
        ),
        evidence: Evidence {
            text: format!(
                "{worst}: server+transfer p50 {} over {n} requests; others p50 {}",
                fmt_ms(wp50),
                fmt_ms(others)
            ),
            values: values(&[
                ("worst_remote_p50_ms", wp50),
                ("other_remotes_p50_ms", others),
                ("worst_remote_requests_count", n as f64),
            ]),
            op_ids: slowest_ops(c.run, ids),
            windows: Vec::new(),
        },
        advice: "A single slow server behind DNS: report it to the provider with the request ids (`oprobe analyze RUN --where remote=IP`); shorter DNS caching or more connections spread load away from it.".into(),
        verification: None,
    })
}

/// Segments each request saw retransmitted *while it ran*: the growth of its connection's
/// cumulative retransmit counter since the previous sample on that connection (requests are
/// ordered by end time). The first sample of a fresh connection counts in full (its handshake
/// belongs to the request); the first sample of a reused connection has no baseline and counts
/// zero. Requests to loopback addresses are skipped: local servers are not a network path.
fn retransmit_deltas(reqs: &[ReqRow]) -> Vec<(&ReqRow, u64)> {
    let mut rows: Vec<&ReqRow> = reqs
        .iter()
        .filter(|q| q.tcp.is_some() && !q.remote.is_some_and(|a| a.ip().is_loopback()))
        .collect();
    rows.sort_by_key(|q| (q.conn_id, q.t_us + q.total_us as i64));
    let mut last: HashMap<u64, u64> = HashMap::new();
    rows.into_iter()
        .map(|q| {
            let now = q.tcp.as_ref().map_or(0, |t| t.retrans);
            let delta = match last.insert(q.conn_id, now) {
                Some(prev) => now.saturating_sub(prev),
                None if !q.reused => now,
                None => 0,
            };
            (q, delta)
        })
        .collect()
}

/// Minimum share of requests (percent) that must see retransmissions while running.
const RETRANSMIT_MIN_PCT: f64 = 1.0;
/// Minimum number of such requests: a handful of lost packets is not a finding.
const RETRANSMIT_MIN_REQUESTS: usize = 5;

fn tcp(c: &Ctx) -> Vec<Finding> {
    let deltas = retransmit_deltas(&c.run.reqs);
    let mut out = Vec::new();
    if deltas.len() >= 20 {
        let hit: Vec<&(&ReqRow, u64)> = deltas.iter().filter(|(_, d)| *d > 0).collect();
        let share = hit.len() as f64 * 100.0 / deltas.len() as f64;
        let segments: u64 = hit.iter().map(|(_, d)| d).sum();
        if share >= RETRANSMIT_MIN_PCT && hit.len() >= RETRANSMIT_MIN_REQUESTS {
            let hit_ids: std::collections::HashSet<(u64, i64)> =
                hit.iter().map(|(q, _)| (q.conn_id, q.t_us)).collect();
            out.push(Finding {
                code: "tcp_retransmits".into(),
                severity: if share >= 5.0 { Severity::Warning } else { Severity::Info },
                cause: format!("{share:.1}% of requests lost packets while they ran (TCP retransmissions)."),
                evidence: Evidence {
                    text: format!(
                        "{} of {} requests with TCP statistics saw {segments} segments retransmitted during the request",
                        hit.len(),
                        deltas.len()
                    ),
                    values: values(&[
                        ("retransmit_requests_pct", share),
                        ("retransmit_requests_count", hit.len() as f64),
                        ("retransmitted_segments_count", segments as f64),
                    ]),
                    op_ids: slowest_ops(c.run, hit.iter().map(|(q, _)| q.op_id).filter(|i| *i != 0)),
                    windows: windows_where(&c.run.reqs, |q| hit_ids.contains(&(q.conn_id, q.t_us))),
                },
                advice: "Packet loss on the path (congested NIC, proxy, VPN or cross-region link) stretches transfers and the tail; compare from another host or region.".into(),
                verification: None,
            });
        }
    }
    let with: Vec<&ReqRow> = c.run.reqs.iter().filter(|q| q.tcp.is_some()).collect();
    if with.len() < 20 {
        return out;
    }
    let mut rtt: Vec<u64> = with
        .iter()
        .filter_map(|q| q.tcp.as_ref().map(|t| t.rtt_us))
        .collect();
    let mut rtt_p50 = quantile_us_ms(&mut rtt, 0.5);
    // A kernel RTT inflated by the server's delay is no network round trip: use the TCP
    // handshake's (a clean sample) instead, or say nothing.
    if super::outliers::rtt_tracks_server(&c.run.reqs) {
        let mut hs: Vec<u64> = c
            .run
            .reqs
            .iter()
            .filter(|q| !q.reused && q.tcp_us > 0)
            .map(|q| q.tcp_us)
            .collect();
        rtt_p50 = if hs.len() >= 5 {
            quantile_us_ms(&mut hs, 0.5)
        } else {
            0.0
        };
    }
    if rtt_p50 >= 50.0 {
        out.push(Finding {
            code: "high_rtt".into(),
            severity: Severity::Info,
            cause: format!("The network round trip to the endpoint is {} (TCP RTT p50).", fmt_ms(rtt_p50)),
            evidence: Evidence {
                text: format!("kernel-measured RTT p50 {} over {} requests", fmt_ms(rtt_p50), with.len()),
                values: values(&[("rtt_p50_ms", rtt_p50)]),
                ..Default::default()
            },
            advice: "Every request pays at least one round trip: run close to the bucket's region, or hide the latency with more parallelism and fewer, larger requests.".into(),
            verification: None,
        });
    }
    out
}

/// The parallelism part of a latency-bound finding (`server_latency`, `small_objects`), from
/// the capacity model: how many workers to try (never past the safe concurrency), what that
/// should give, and the verification; or, at the ceiling, that more workers would throttle.
struct ScaleAdvice {
    text: String,
    vals: Vec<(&'static str, f64)>,
    advice: String,
    verification: Option<Verification>,
}

/// More workers cannot help when the selected limit model is a queue, a shared bandwidth
/// cap or the client itself: the advice then says what does (fixes A8, A9; Little's law on the
/// fitted model instead of linear scaling, A3).
fn saturation_advice(c: &Ctx) -> Option<ScaleAdvice> {
    let h = c.cap.asserted()?;
    let cc = c.concurrency();
    match h.kind {
        HypothesisKind::Queueing => {
            let servers = h.params.servers_count.unwrap_or(1).max(1);
            let service = h.params.service_ms.unwrap_or(0.0);
            let per_op = c.cap.per_op.peak_inflight_per_op.max(1.0);
            // Enough workers to keep every server busy, with one in line each.
            let to = ((servers as f64 * 1.5 / per_op).ceil() as u64).max(1);
            let max_ops = c.cap.predict(to as u32).ops_per_s;
            let mut out = ScaleAdvice {
                text: format!(
                    "the service queues: {servers} requests are served at once (≈{service:.1} ms each), so throughput saturates near {max_ops:.0} ops/s and extra in-flight requests only wait"
                ),
                vals: vec![
                    ("servers_count", servers as f64),
                    ("service_ms", service),
                ],
                advice: format!(
                    "Do not add workers: the service serves {servers} requests at once and the rest wait in line (latency grows with concurrency, throughput does not). Run at ≈{to} workers for the same throughput at a lower latency"
                ),
                verification: None,
            };
            if to < cc {
                out.verification = c
                    .verify(
                        format!("re-run at concurrency {to}: the same goodput at a lower p99"),
                        &[("concurrency", to.to_string())],
                    )
                    .map(|v| {
                        v.expecting(Expectation {
                            metric: "summary.ops_per_s".into(),
                            direction: Direction::AtLeast,
                            predicted: 0.9,
                            lo: 0.9,
                            hi: 1.1,
                            unit: "baseline_ratio".into(),
                        })
                        .expecting(Expectation {
                            metric: "p99_ms".into(),
                            direction: Direction::Decrease,
                            ..Default::default()
                        })
                    });
            }
            Some(out)
        }
        HypothesisKind::BandwidthCap => {
            let bps = h.params.bytes_per_s.unwrap_or(0.0);
            let to = (cc / 2).max(1);
            let mut out = ScaleAdvice {
                text: format!(
                    "every stream shares ≈{:.1} MiB/s: a stream's rate falls as more run beside it",
                    bps / (1024.0 * 1024.0)
                ),
                vals: vec![("bandwidth_cap_bytes_per_s", bps)],
                advice: format!(
                    "Do not add streams: the path is capped at ≈{:.1} MiB/s in total (a NIC, proxy, VPC endpoint or bucket limit), so more parallelism only splits it. Move fewer bytes (narrower ranges, compression), run closer to the data, or raise the cap; fewer streams keep the same throughput at a lower latency",
                    bps / (1024.0 * 1024.0)
                ),
                verification: None,
            };
            if to < cc {
                out.verification = c
                    .verify(
                        format!("re-run with {to} workers: the same MiB/s at a lower p99"),
                        &[("concurrency", to.to_string())],
                    )
                    .map(|v| {
                        v.expecting(Expectation {
                            metric: "summary.mib_per_s".into(),
                            direction: Direction::AtLeast,
                            predicted: 0.9,
                            lo: 0.9,
                            hi: 1.1,
                            unit: "baseline_ratio".into(),
                        })
                        .expecting(Expectation {
                            metric: "p99_ms".into(),
                            direction: Direction::Decrease,
                            ..Default::default()
                        })
                    });
            }
            Some(out)
        }
        HypothesisKind::ClientBound => Some(ScaleAdvice {
            text: h.evidence.first().cloned().unwrap_or_default(),
            vals: Vec::new(),
            advice: "Do not add workers: the load generator's CPU is saturated, so its numbers understate the service. Use a bigger client or several (`--workers`), or cut client work (`workload.verify = false`)".into(),
            verification: None,
        }),
        _ => None,
    }
}

fn scale_advice(c: &Ctx) -> ScaleAdvice {
    if let Some(s) = saturation_advice(c) {
        return s;
    }
    match c.scale_up() {
        Ok(to) => {
            let pred = simulate::concurrency(c.run, to, c.cap);
            let mut out = ScaleAdvice {
                text: String::new(),
                vals: Vec::new(),
                advice: match c.safe {
                    Some(_) => format!(
                        "Raise `concurrency` to {to}, the most the in-flight ceiling allows ({})",
                        c.ceiling_note()
                    ),
                    None => format!(
                        "Raise `concurrency` (try {to}) until throttling or the latency SLA stops you (`oprobe tune` finds the ceiling)"
                    ),
                },
                verification: None,
            };
            if let Some(p) = &pred {
                out.text = pred_text(p);
                out.vals.extend(pred_values(p));
                out.verification = c.verify_concurrency(
                    format!(
                        "raise the concurrency to {to}: ~{:.0} ops/s predicted",
                        p.predicted
                    ),
                    to,
                );
            }
            out
        }
        Err(safe) => ScaleAdvice {
            text: format!(
                "the run already uses {} workers: {}",
                c.concurrency(),
                c.ceiling_note()
            ),
            vals: vec![("safe_concurrency_count", f64::from(safe))],
            advice: "Do not add workers: the service throttles beyond the current parallelism. Cut round trips instead".into(),
            verification: None,
        },
    }
}

fn small_objects(c: &Ctx) -> Option<Finding> {
    let reqs = &c.run.reqs;
    if !c.network || c.throttled || reqs.len() < 100 {
        return None;
    }
    let bytes: u64 = reqs.iter().map(|q| q.bytes_down + q.bytes_up).sum();
    let mean_bytes = bytes / reqs.len() as u64;
    let total: u64 = reqs.iter().map(|q| q.total_us).sum();
    let ttfb: u64 = reqs.iter().map(|q| q.ttfb_us + q.send_us).sum();
    let share = ttfb as f64 * 100.0 / total.max(1) as f64;
    if mean_bytes >= 128 * 1024 || share < 60.0 {
        return None;
    }
    let scale = scale_advice(c);
    let mut text = format!(
        "mean {} per request; {share:.0}% of request time waiting for the server's answer",
        fmt_bytes(mean_bytes)
    );
    if !scale.text.is_empty() {
        text += &format!("; {}", scale.text);
    }
    let mut vals = vec![
        ("mean_request_bytes", mean_bytes as f64),
        ("ttfb_pct", share),
    ];
    vals.extend(scale.vals);
    Some(Finding {
        code: "small_objects".into(),
        severity: Severity::Info,
        cause: "Requests are small: throughput is bound by request round trips, not bandwidth."
            .into(),
        evidence: Evidence {
            text,
            values: values(&vals),
            ..Default::default()
        },
        advice: format!(
            "{}; batch or pack small objects, and coalesce neighbouring ranges (`range_gap`).",
            scale.advice
        ),
        verification: scale.verification,
    })
}

fn suffix_read(c: &Ctx) -> Option<Finding> {
    let p = simulate::suffix_read(c.run)?;
    if p.change_ratio() > -0.1 {
        return None;
    }
    Some(Finding {
        code: "suffix_read".into(),
        severity: Severity::Info,
        cause: "Operations HEAD each object before reading it; a suffix range read needs no HEAD.".into(),
        evidence: Evidence {
            text: format!(
                "{}; saves {:.0} requests per 1 000 operations",
                pred_text(&p),
                -p.requests_per_1k_ops_delta
            ),
            values: values(&pred_values(&p)),
            ..Default::default()
        },
        advice: "Read footers with a suffix range (`bytes=-N`, OpenDAL `read_with(path).range(..)` from the end) or pass the known size instead of calling stat first.".into(),
        verification: None,
    })
}

fn cache_opportunity(c: &Ctx) -> Option<Finding> {
    if c.r.knobs.requested.cache {
        return None;
    }
    // A generated dataset smaller than the data the user keeps (`cost.stored_gib`) says
    // nothing about the real hit ratio (A15): only real objects, or a stated skew, count.
    let ds = &c.r.workload.dataset;
    let generated_bytes = ds.objects_count.saturating_mul(ds.object_size_bytes) as f64;
    let stated_bytes = c.r.cost.stored_gb * (1u64 << 30) as f64;
    let skewed = c.r.workload.access != "uniform";
    if ds.existing_prefix.is_none() && !skewed && generated_bytes < stated_bytes {
        return None;
    }
    let p = simulate::cache(c.run, 256 << 20)?;
    if p.requests_per_1k_ops_delta > -200.0 {
        return None;
    }
    Some(Finding {
        code: "cache_opportunity".into(),
        severity: Severity::Info,
        cause: "The same objects are read again and again; a read cache would save requests.".into(),
        evidence: Evidence {
            text: format!(
                "{}; saves {:.0} requests per 1 000 reads — {}",
                pred_text(&p),
                -p.requests_per_1k_ops_delta,
                p.note
            ),
            values: values(&pred_values(&p)),
            ..Default::default()
        },
        advice: "Enable the read cache (`--cache 256MiB`) when the working set is re-read; in production, a cache in front of OpenDAL (or `CacheLayer`) does the same.".into(),
        verification: c.verify("measure with a 256 MiB read cache", &[("cache", "256MiB".into())]),
    })
}

fn cost_hotspot(c: &Ctx) -> Option<Finding> {
    let cost = &c.r.cost;
    if !cost.applicable || cost.selected.total_usd <= 0.0 {
        return None;
    }
    let top = cost.by_api.first();
    let waste_pct = cost.waste_usd * 100.0 / cost.selected.total_usd;
    let top_hot = top.filter(|t| t.share_pct >= 60.0 && t.monthly_usd > 0.0);
    if top_hot.is_none() && waste_pct < 10.0 {
        return None;
    }
    let mut parts = Vec::new();
    let mut vals = vec![
        ("monthly_usd", cost.selected.total_usd),
        ("waste_usd", cost.waste_usd),
        ("waste_pct", waste_pct),
    ];
    if let Some(t) = top_hot {
        parts.push(format!(
            "{} is {:.0}% of request spend ({}/month, {:.2} requests per operation)",
            t.api,
            t.share_pct,
            fmt_usd(t.monthly_usd),
            t.requests_per_op
        ));
        vals.push(("top_api_share_pct", t.share_pct));
    }
    if waste_pct >= 10.0 {
        parts.push(format!(
            "{} of {} per month buys nothing (failed/throttled requests {}, over-read egress {})",
            fmt_usd(cost.waste_usd),
            fmt_usd(cost.selected.total_usd),
            fmt_usd(cost.selected.failed_requests_usd),
            fmt_usd(cost.selected.overread_usd)
        ));
    }
    Some(Finding {
        code: "cost_hotspot".into(),
        severity: if waste_pct >= 25.0 { Severity::Warning } else { Severity::Info },
        cause: match top_hot {
            Some(t) if waste_pct < 10.0 => format!("{} requests dominate the projected bill.", t.api),
            _ => format!("{waste_pct:.0}% of the projected bill is waste."),
        },
        evidence: Evidence {
            text: format!(
                "{} at {:.0} operations/month ({} prices, {})",
                parts.join("; "),
                cost.monthly_ops,
                cost.provider.as_deref().unwrap_or("selected"),
                cost.pricing_as_of
            ),
            values: values(&vals),
            ..Default::default()
        },
        advice: "Fewer, larger requests cut request spend (coalesce ranges, bigger parts, skip HEADs); stop throttling and over-reading to cut waste.".into(),
        verification: None,
    })
}

fn slow_start(c: &Ctx) -> Option<Finding> {
    let seg = super::segments::segments(&c.run.ops, c.r.run.window_s);
    let (start, steady) = (seg.start?, seg.steady?);
    if start.p50_ms < 2.0 * steady.p50_ms.max(0.001) || start.p50_ms - steady.p50_ms < 2.0 {
        return None;
    }
    Some(Finding {
        code: "slow_start".into(),
        severity: Severity::Info,
        cause: format!(
            "The first {:.0} s are much slower than the steady state (p50 {} vs {}).",
            start.to_s,
            fmt_ms(start.p50_ms),
            fmt_ms(steady.p50_ms)
        ),
        evidence: Evidence {
            text: format!(
                "start: {:.1} ops/s, p99 {}; steady ({:.0}–{:.0} s): {:.1} ops/s, p99 {}",
                start.ops_per_s,
                fmt_ms(start.p99_ms),
                steady.from_s,
                steady.to_s,
                steady.ops_per_s,
                fmt_ms(steady.p99_ms)
            ),
            values: values(&[
                ("start_p50_ms", start.p50_ms),
                ("steady_p50_ms", steady.p50_ms),
                ("start_ops_per_s", start.ops_per_s),
                ("steady_ops_per_s", steady.ops_per_s),
            ]),
            op_ids: Vec::new(),
            windows: vec![start.window()],
        },
        advice: "Cold connections, DNS, TLS or server-side scaling at the start skew short runs: use a longer `--warmup` or judge by the steady segment (`oprobe analyze RUN --by second`).".into(),
        verification: c.verify("re-run with a longer warmup", &[("warmup", format!("{:.0}s", start.to_s.max(2.0)))]),
    })
}

/// Convert a 2.x-style diagnosis item (aggregate-only checks this engine keeps as they are).
fn carry(c: &Ctx, f: &diagnosis::Finding) -> Finding {
    let code = f.code.clone();
    let mut out = Finding::from(f);
    if !out.cause.ends_with('.') {
        out.cause.push('.');
    }
    match code.as_str() {
        "server_latency" if c.throttled && c.safe.is_none() => {
            out.advice = "Latency-bound, but the service throttles this load: do not add workers; coalesce small ranges (`range_gap`) and run close to the bucket's region.".into();
        }
        "server_latency" => {
            let scale = scale_advice(c);
            if !scale.text.is_empty() {
                out.evidence.text += &format!("; {}", scale.text);
            }
            out.evidence.values = values(&scale.vals);
            out.advice = format!(
                "{}; coalesce small ranges (`range_gap`), and run close to the bucket's region.",
                scale.advice
            );
            out.verification = scale.verification;
        }
        "download_bound" | "upload_bound"
            if c.cap.asserted().is_some_and(|h| {
                matches!(
                    h.kind,
                    HypothesisKind::BandwidthCap | HypothesisKind::ClientBound
                )
            }) =>
        {
            let s = saturation_advice(c).expect("asserted saturation");
            out.evidence.text += &format!("; {}", s.text);
            out.evidence.values.extend(values(&s.vals));
            out.advice = format!("{}.", s.advice);
            out.verification = s.verification;
        }
        "download_bound" | "upload_bound" if c.safe.is_some() => {
            out.advice = format!(
                "{} Stay within the in-flight ceiling: {}.",
                out.advice,
                c.ceiling_note()
            );
        }
        "rate_not_sustained" if c.scale_up().is_err() => {
            out.advice = format!(
                "Lower the target rate: more in-flight slots would cross the service's ceiling ({}). Queued arrivals still ran: their wait is included in the latency percentiles.",
                c.ceiling_note()
            );
        }
        "overread" => {
            out.evidence.values = values(&[("overread_ratio", c.r.bytes.overread_ratio)]);
            out.verification = c.verify(
                "halve the range gap",
                &[(
                    "range_gap",
                    crate::util::ByteSize(c.r.knobs.requested.range_gap_bytes / 2).to_string(),
                )],
            );
        }
        "consumer_stall" => {
            out.evidence.op_ids = slowest_ops(
                c.run,
                c.run
                    .reqs
                    .iter()
                    .filter(|q| q.stall_us > q.body_us)
                    .map(|q| q.op_id)
                    .filter(|i| *i != 0),
            );
        }
        _ => {}
    }
    out
}

/// Codes the 2.x diagnosis emits that this engine replaces with row-based findings (the
/// diagnosis' connection-setup, connection-churn and slow-TLS checks all emit
/// `cold_connections`, its retry-backoff check `retries`).
const REPLACED: &[&str] = &[
    "errors",
    "throttling",
    "retries",
    "client_queue",
    "cold_connections",
    "tail_latency",
    "suffix_read",
];

/// All findings of a run: row-based checks plus the aggregate checks of `legacy` (the
/// [`diagnosis::diagnose`] result for the same window), one per code, most severe first.
pub fn derive(run: &Run, legacy: &[diagnosis::Finding]) -> Vec<Finding> {
    let r = &run.report;
    let c = Ctx {
        run,
        r,
        network: r.target.scheme != "memory" && r.target.scheme != "fs",
        throttled: r.resilience.throttled_responses > 0,
        cap: &r.capacity,
        safe: r.capacity.max_advisable_concurrency(),
    };
    let mut out: Vec<Finding> = Vec::new();
    let push = |f: Option<Finding>, out: &mut Vec<Finding>| {
        if let Some(f) = f
            && !out.iter().any(|x| x.code == f.code)
        {
            out.push(f);
        }
    };
    for f in legacy.iter().filter(|f| f.code == "integrity") {
        push(Some(carry(&c, f)), &mut out);
    }
    push(errors(&c), &mut out);
    push(throttling(&c), &mut out);
    push(retries(&c), &mut out);
    push(client_queue(&c), &mut out);
    push(cold_connections(&c), &mut out);
    push(remote_skew(&c), &mut out);
    for f in tcp(&c) {
        push(Some(f), &mut out);
    }
    let legacy_kept: Vec<Finding> = {
        let mut v: Vec<Finding> = Vec::new();
        for f in legacy
            .iter()
            .filter(|f| !REPLACED.contains(&f.code.as_str()) && f.code != "integrity")
        {
            let mut g = carry(&c, f);
            // Several verbs over the SLA: one finding naming them all.
            if let Some(prev) = v.iter_mut().find(|x| x.code == g.code) {
                if g.code == "sla_p99" {
                    prev.cause = prev.cause.trim_end_matches('.').to_owned()
                        + "; "
                        + g.cause.trim_end_matches('.')
                        + ".";
                }
                continue;
            }
            if g.code == "consumer_stall" && out.iter().any(|x| x.code == "consumer_stall") {
                continue;
            }
            g.evidence.text = g.evidence.text.trim().to_owned();
            v.push(g);
        }
        v
    };
    let have: Vec<Finding> = out.iter().chain(legacy_kept.iter()).cloned().collect();
    push(tail(&c, &have), &mut out);
    for f in legacy_kept {
        push(Some(f), &mut out);
    }
    push(http1(&c), &mut out);
    // Server-latency-bound runs already carry the same parallelism estimate.
    if !out.iter().any(|f| f.code == "server_latency") {
        push(small_objects(&c), &mut out);
    }
    push(suffix_read(&c), &mut out);
    push(cache_opportunity(&c), &mut out);
    push(cost_hotspot(&c), &mut out);
    push(slow_start(&c), &mut out);
    // Bottleneck items restate the headline: keep them only when nothing more specific
    // explains the run.
    let specific = out.iter().any(|f| f.severity >= Severity::Warning);
    out.retain(|f| !(specific && matches!(f.code.as_str(), "client_overhead" | "local_backend")));
    // Stable order: severity, then the order above.
    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    for f in &mut out {
        if let Some(v) = &mut f.verification {
            v.command = crate::model::spec::command_line(&[
                "verify".into(),
                r.run.id.clone(),
                f.code.clone(),
            ]);
        }
    }
    let (kept, _) = validity(r, out);
    kept
}

/// Why a run cannot be judged on performance (design v4 §4): it is invalid (no operation
/// completed, or more failed than the SLA allows), or it is partial (interrupted, stopped by a
/// budget or an error) and too short to judge, or its window is shorter than its warmup.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Validity {
    pub invalid: Option<String>,
    pub partial: Option<String>,
    /// Performance findings are suppressed.
    pub suppress: bool,
}

/// The run's [`Validity`].
pub fn run_validity(r: &Report) -> Validity {
    let s = &r.summary;
    let window = r.run.window_s;
    let invalid = if s.ops == 0 {
        Some("no operation completed in the measurement window".to_owned())
    } else if s.error_rate > r.sla.max_error_ratio {
        Some(format!(
            "{:.1}% of operations failed (the SLA allows {}%), so latency and throughput describe failures",
            s.error_rate * 100.0,
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
    let partial = r.run.partial.as_ref().map(|p| {
        format!(
            "the run was {} after {:.1} s of measurement",
            p.reason.describe(),
            p.measured_s
        )
    });
    // A budget stop with a usable window still describes the workload; an interrupted or
    // failed run, or any window under a second (or under the warmup), does not.
    let too_short = window < r.workload.warmup_s.max(1.0);
    let suppress = invalid.is_some()
        || r.run
            .partial
            .as_ref()
            .is_some_and(|p| p.reason != crate::model::PartialReason::Budget || too_short);
    Validity {
        invalid,
        partial,
        suppress,
    }
}

/// Apply the run's [`Validity`] to `findings`: performance findings are dropped when the run
/// cannot be judged, and `run_invalid` / `run_partial` say why. Idempotent. Returns the kept
/// findings (most severe first) and the validity.
pub fn validity(r: &Report, findings: Vec<Finding>) -> (Vec<Finding>, Validity) {
    let v = run_validity(r);
    let mut out: Vec<Finding> = findings
        .into_iter()
        .filter(|f| !matches!(f.code.as_str(), "run_invalid" | "run_partial"))
        .filter(|f| !v.suppress || VALID_ON_BROKEN_RUNS.contains(&f.code.as_str()))
        .collect();
    let suppressed = if v.suppress {
        " Performance findings (latency, scaling, tail, cache, cost) are suppressed."
    } else {
        ""
    };
    if let Some(why) = &v.invalid {
        out.push(Finding {
            code: "run_invalid".into(),
            severity: Severity::Warning,
            cause: format!("The run cannot be judged on performance: {why}."),
            evidence: Evidence {
                text: format!(
                    "{} operations, {} failed ({:.2}%), {:.1} s measured after a {:.1} s warmup",
                    r.summary.ops,
                    r.summary.failed_ops,
                    r.summary.error_rate * 100.0,
                    r.run.window_s,
                    r.workload.warmup_s
                ),
                values: values(&[
                    ("error_ratio", r.summary.error_rate),
                    ("window_s", r.run.window_s),
                ]),
                ..Default::default()
            },
            advice: format!(
                "Fix the failures (see the errors finding) or lengthen the run, then measure again.{suppressed}"
            ),
            verification: None,
        });
    }
    if let (Some(why), Some(p)) = (&v.partial, &r.run.partial) {
        out.push(Finding {
            code: "run_partial".into(),
            severity: if v.suppress {
                Severity::Warning
            } else {
                Severity::Info
            },
            cause: format!("Partial run: {why}."),
            evidence: Evidence {
                text: format!(
                    "{:.1} s of {:.1} s measured; {} operations",
                    p.measured_s,
                    r.workload.duration_s,
                    r.summary.ops
                ),
                values: values(&[
                    ("measured_s", p.measured_s),
                    ("configured_s", r.workload.duration_s),
                ]),
                ..Default::default()
            },
            advice: format!(
                "Re-run the full workload (`oprobe run --from {}`); a partial run is not accepted as a baseline.{suppressed}",
                r.run.id
            ),
            verification: None,
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
                preset: "parquet".into(),
                duration_s: 5.0,
                ..Default::default()
            },
            knobs: Default::default(),
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
            client: Default::default(),
            self_stats: Default::default(),
            schedule: Default::default(),
            workers: Default::default(),
            dimensions: Default::default(),
            sampling: Default::default(),
            open_loop: None,
            timeline: vec![],
            heatmap: Default::default(),
            findings: vec![],
            cost: Default::default(),
            slowest: vec![],
            cleanup: Default::default(),
            warnings: vec![],
            tune: None,
        };
        serde_json::to_value(r).unwrap()
    }

    #[test]
    fn stragglers_are_explained_once_with_a_tail_cut_prediction() {
        let mut run = straggler_run();
        run.report.knobs.requested.keep_alive = true;
        let legacy = vec![diagnosis::Finding {
            severity: Severity::Warning,
            code: "tail_latency".into(),
            title: "p99 is 80x".into(),
            evidence: String::new(),
            action: String::new(),
        }];
        let f = derive(&run, &legacy);
        let tails: Vec<&Finding> = f.iter().filter(|x| x.code.starts_with("tail")).collect();
        assert_eq!(tails.len(), 1, "{f:#?}");
        let t = tails[0];
        assert_eq!(t.code, "tail_stragglers");
        assert!(
            t.cause.contains("GetObject") && t.cause.contains("tail-cut (deadline"),
            "{}",
            t.cause
        );
        assert!(t.evidence.values["predicted_p99_ms"] < 200.0, "{t:#?}");
        assert!(!t.evidence.op_ids.is_empty());
        let v = t.verification.as_ref().unwrap();
        assert!(v.replay.contains("--tail-cut"), "{v:?}");
        assert!(v.rerun.contains("--set tail_cut=on"), "{v:?}");
        assert_eq!(v.overrides.knobs["tail_cut"], "on");
        assert!(CODES.contains(&"tail_stragglers"));
    }

    #[test]
    fn throttling_reports_the_ceiling_and_a_concurrency_to_try() {
        let mut run = straggler_run();
        run.report.resilience.throttled_responses = 100;
        run.report.connections.requests = 1000;
        run.report.summary.throttled_request_ratio = 0.1;
        run.report.knobs.requested.concurrency_count = 32;
        run.report.bytes.request_amplification = 1.0;
        for (i, q) in run.reqs.iter_mut().enumerate() {
            if i % 10 == 0 {
                q.status = 503;
            }
        }
        // The capacity model knows the ceiling: 24 requests in flight, 1 per operation.
        run.report.capacity = capacity_24();
        let f = derive(&run, &[]);
        let t = f.iter().find(|x| x.code == "throttling").unwrap();
        assert_eq!(f[0].code, "throttling");
        assert_eq!(t.evidence.values["ceiling_lo_inflight_count"], 24.0);
        assert_eq!(t.evidence.values["safe_concurrency_count"], 20.0);
        assert_eq!(t.evidence.values["throttled_request_ratio"], 0.1);
        assert!(!t.evidence.values.contains_key("throttle_ratio"));
        assert!(!t.evidence.windows.is_empty());
        assert!(t.advice.contains("concurrency ≤ 20"), "{}", t.advice);
        let v = t.verification.as_ref().unwrap();
        assert_eq!(v.overrides.knobs["concurrency"], "20", "{v:?}");
        let thr = v
            .expect
            .iter()
            .find(|e| e.metric == "summary.throttled_request_ratio")
            .expect("throttle expectation");
        assert_eq!(thr.direction, Direction::AtMost);
        assert!(thr.hi <= THROTTLE_BUDGET_RATIO);
        let ops = v
            .expect
            .iter()
            .find(|e| e.metric == "summary.ops_per_s")
            .expect("goodput expectation");
        assert!(ops.lo < ops.predicted && ops.predicted < ops.hi, "{ops:?}");
        // Throttling explains the run: no separate tail item about retries.
        assert!(f.iter().all(|x| x.code != "retries"));

        // Throttling that follows the request rate (a token bucket, design v5 §4): pace the
        // request rate instead.
        run.report.capacity.inflight_ceiling = None;
        run.report.capacity.hypotheses = vec![crate::analysis::hypothesis::Hypothesis {
            kind: HypothesisKind::RateLimit,
            params: crate::analysis::hypothesis::HypothesisParams {
                requests_per_s: Some(1000.0),
                ..Default::default()
            },
            confidence: crate::analysis::capacity::Confidence::High,
            selected: true,
            ..Default::default()
        }];
        let want = run.report.capacity.safe_rate(0.15).unwrap().floor();
        let f = derive(&run, &[]);
        let t = f.iter().find(|x| x.code == "throttling").unwrap();
        let v = t.verification.as_ref().unwrap();
        assert_eq!(v.overrides.workload["rate"], format!("{want:.0}"), "{v:?}");
        assert!(t.advice.contains("not concurrency"), "{}", t.advice);
        // Random throttling: no slow-down advice at all (A5).
        run.report.capacity.hypotheses[0].kind = HypothesisKind::None;
        let f = derive(&run, &[]);
        let t = f.iter().find(|x| x.code == "throttling").unwrap();
        assert!(t.verification.is_none(), "{:?}", t.verification);
        assert!(t.advice.contains("random"), "{}", t.advice);
        // Nothing known with confidence: inconclusive, nothing to replay.
        run.report.capacity = Default::default();
        let f = derive(&run, &[]);
        let t = f.iter().find(|x| x.code == "throttling").unwrap();
        assert!(t.verification.is_none(), "{:?}", t.verification);
        assert!(t.advice.starts_with("Inconclusive"), "{}", t.advice);
    }

    /// A capacity estimate with a ceiling of 24 requests in flight, one per operation, from a
    /// run of 20 workers at 2 000 ops/s.
    pub(crate) fn capacity_24() -> Capacity {
        let mut c = Capacity {
            inflight_ceiling: Some(super::super::capacity::Ceiling {
                requests: 24.0,
                lo: 24.0,
                hi: 26.0,
            }),
            ..Default::default()
        };
        c.per_op.peak_inflight_per_op = 1.0;
        c.per_op.mean_inflight_per_op = 1.0;
        c.per_op.requests_per_op = 1.0;
        c.observed.ops_per_s = 2000.0;
        c.observed.ops_inflight_mean = 20.0;
        c.observed.peak_inflight_count = 20;
        c.confidence = super::super::capacity::Confidence::High;
        c
    }

    #[test]
    fn latency_findings_never_suggest_more_workers_than_the_ceiling_allows() {
        let mut run = straggler_run();
        run.report.target.scheme = "s3".into();
        run.report.summary.ops_per_s = 2000.0;
        for q in &mut run.reqs {
            q.total_us = 9_000;
            q.ttfb_us = 8_990;
        }
        let legacy = vec![diagnosis::Finding {
            severity: Severity::Info,
            code: "server_latency".into(),
            title: "Latency-bound on server time-to-first-byte".into(),
            evidence: "mean TTFB 9 ms".into(),
            action: "raise concurrency".into(),
        }];
        // No ceiling known: double (a hypothesis verify will test).
        run.report.knobs.requested.concurrency_count = 12;
        run.report.capacity = capacity_24();
        run.report.capacity.inflight_ceiling = None;
        let f = derive(&run, &legacy);
        let s = f.iter().find(|x| x.code == "server_latency").unwrap();
        assert_eq!(
            s.verification.as_ref().unwrap().overrides.knobs["concurrency"],
            "24"
        );
        // A ceiling of 24 in flight: at most 20 workers.
        run.report.capacity = capacity_24();
        let f = derive(&run, &legacy);
        let s = f.iter().find(|x| x.code == "server_latency").unwrap();
        let v = s.verification.as_ref().unwrap();
        assert_eq!(v.overrides.knobs["concurrency"], "20", "{s:#?}");
        assert!(s.advice.contains("at most 20 workers"), "{}", s.advice);
        assert!(v.expect.iter().any(|e| e.metric == "summary.ops_per_s"));
        // Already at the safe concurrency: no more workers, and no verification.
        run.report.knobs.requested.concurrency_count = 20;
        let f = derive(&run, &legacy);
        let s = f.iter().find(|x| x.code == "server_latency").unwrap();
        assert!(s.verification.is_none(), "{s:#?}");
        assert!(s.advice.starts_with("Do not add workers"), "{}", s.advice);
        for f in &f {
            if let Some(v) = &f.verification
                && let Some(c) = v.overrides.knobs.get("concurrency")
            {
                assert!(c.parse::<u64>().unwrap() <= 20, "{f:#?}");
            }
        }
    }

    #[test]
    fn broken_runs_get_no_performance_findings() {
        let mut run = straggler_run();
        run.report.run.id = "20261121-101010-abcd".into();
        run.report.run.window_s = 5.0;
        run.report.workload.duration_s = 10.0;
        let legacy = vec![diagnosis::Finding {
            severity: Severity::Warning,
            code: "tail_latency".into(),
            title: "p99 is 80x".into(),
            evidence: String::new(),
            action: String::new(),
        }];
        assert!(
            derive(&run, &legacy)
                .iter()
                .any(|f| f.code.starts_with("tail"))
        );
        // Interrupted: only the run_partial finding explains why nothing else is said.
        run.report.run.partial = Some(crate::model::PartialInfo {
            reason: crate::model::PartialReason::Interrupted,
            measured_s: 0.5,
        });
        let f = derive(&run, &legacy);
        let codes: Vec<&str> = f.iter().map(|x| x.code.as_str()).collect();
        assert_eq!(codes, ["run_partial"], "{f:#?}");
        assert!(f[0].advice.contains("--from 20261121-101010-abcd"));
        // A budget stop with a usable window keeps its findings (and says it is partial).
        run.report.run.partial = Some(crate::model::PartialInfo {
            reason: crate::model::PartialReason::Budget,
            measured_s: 5.0,
        });
        let f = derive(&run, &legacy);
        assert!(f.iter().any(|x| x.code.starts_with("tail")), "{f:#?}");
        assert!(
            f.iter()
                .any(|x| x.code == "run_partial" && x.severity == Severity::Info)
        );
        // Every op failed: errors stay, performance goes, run_invalid says why.
        run.report.run.partial = None;
        run.report.summary.failed_ops = 1000;
        run.report.summary.error_rate = 1.0;
        run.report.sla.max_error_ratio = 0.01;
        run.report
            .resilience
            .error_classes
            .insert("unexpected".into(), 1000);
        run.report.knobs.requested.http_version = crate::net::HttpVersion::H2;
        for o in &mut run.ops {
            o.ok = false;
        }
        let f = derive(&run, &legacy);
        let codes: Vec<&str> = f.iter().map(|x| x.code.as_str()).collect();
        assert!(
            codes.contains(&"errors") && codes.contains(&"run_invalid"),
            "{codes:?}"
        );
        assert!(!codes.iter().any(|c| c.starts_with("tail")), "{codes:?}");
        // AN-6: forced HTTP/2 with unclassified failures → test HTTP/1.1.
        let e = f.iter().find(|x| x.code == "errors").unwrap();
        assert!(e.advice.contains("HTTP/2"), "{}", e.advice);
        assert_eq!(
            e.verification.as_ref().unwrap().overrides.knobs["http_version"],
            "h1"
        );
        // Idempotent: validity applied twice changes nothing.
        let (again, v) = validity(&run.report, f.clone());
        assert_eq!(again, f);
        assert!(v.suppress && v.invalid.is_some());
    }

    #[test]
    fn error_advice_follows_the_error_class() {
        let mut run = straggler_run();
        run.report.summary.failed_ops = 23;
        run.report.summary.ops = 1000;
        run.report.summary.error_rate = 0.023;
        run.report.sla.max_error_ratio = 0.5;
        run.report.resilience.throttled_responses = 200;
        run.report.connections.requests = 1000;
        // AN-9: failures that ran out of retries on 429/503 are throttling, not credentials.
        run.report
            .resilience
            .error_classes
            .insert("rate_limited".into(), 23);
        let f = derive(&run, &[]);
        assert!(f.iter().all(|x| x.code != "errors"), "{f:#?}");
        let t = f.iter().find(|x| x.code == "throttling").unwrap();
        assert!(t.evidence.text.contains("23 operations failed"), "{t:#?}");
        assert!(f.iter().all(|x| !x.advice.contains("doctor")), "{f:#?}");
        // Credentials advice only for auth classes.
        run.report
            .resilience
            .error_classes
            .insert("permission_denied".into(), 5);
        let f = derive(&run, &[]);
        let e = f.iter().find(|x| x.code == "errors").unwrap();
        assert!(e.advice.contains("oprobe doctor"), "{}", e.advice);
        assert!(e.evidence.text.contains("permission_denied ×5"), "{e:#?}");
        run.report.resilience.error_classes.clear();
        run.report
            .resilience
            .error_classes
            .insert("timeout".into(), 23);
        let f = derive(&run, &[]);
        let e = f.iter().find(|x| x.code == "errors").unwrap();
        assert!(
            !e.advice.contains("doctor") && e.advice.contains("--timeout"),
            "{}",
            e.advice
        );
    }

    #[test]
    fn evidence_counts_are_whole_numbers() {
        let mut run = straggler_run();
        run.report.capacity = capacity_24();
        run.report.resilience.throttled_responses = 100;
        run.report.connections.requests = 1000;
        run.report.knobs.requested.concurrency_count = 32;
        for f in derive(&run, &[]) {
            for (k, v) in &f.evidence.values {
                if k.ends_with("_count") {
                    assert_eq!(v.fract(), 0.0, "{}: {k} = {v}", f.code);
                }
            }
        }
    }
    #[test]
    fn cold_connections_and_remote_skew_from_rows() {
        let mut run = straggler_run();
        run.report.knobs.requested.keep_alive = true;
        for (i, q) in run.reqs.iter_mut().enumerate() {
            q.total_us = 10_000;
            q.ttfb_us = 5_000;
            if i % 3 == 0 {
                q.reused = false;
                q.dns_us = 1_000;
                q.tcp_us = 1_000;
                q.tls_us = 3_000;
            }
            if i % 4 == 0 {
                q.remote = Some("10.0.0.9:443".parse().unwrap());
                q.ttfb_us = 80_000;
            }
        }
        let f = derive(&run, &[]);
        let cold = f
            .iter()
            .find(|x| x.code == "cold_connections")
            .expect("cold");
        assert!(cold.cause.contains("TLS"), "{}", cold.cause);
        let skew = f.iter().find(|x| x.code == "remote_skew").expect("skew");
        assert!(skew.cause.contains("10.0.0.9"));
    }

    #[test]
    fn verification_commands_reproduce_the_run_except_the_change() {
        let mut run = straggler_run();
        run.report.run.id = "20261121-101010-abcd".into();
        let mut t = crate::config::TargetSpec::from_url("s3://bkt/x").unwrap();
        t.options
            .insert("endpoint".into(), "https://minio.local:9000".into());
        t.options.insert("region".into(), "eu-west-1".into());
        let mut p = crate::config::Profile::from_target(t);
        p.workload.preset = crate::config::Preset::SmallFiles;
        p.workload.duration = crate::util::HumanDuration::secs(12);
        p.workload.warmup = crate::util::HumanDuration(std::time::Duration::from_millis(500));
        p.workload.seed = 9;
        p.workload.max_ops = Some(5000);
        p.knobs.concurrency = 24;
        p.knobs.io_concurrency = 3;
        p.knobs.http_version = crate::net::HttpVersion::H1;
        p.knobs.max_inflight_requests = 16;
        p.knobs.keep_alive = false;
        run.report.spec = crate::model::RunSpec::new(&p, "s3://bkt/x");
        let found = derive(&run, &[]);
        assert!(found.iter().any(|f| f.verification.is_some()), "{found:#?}");
        for f in &found {
            let Some(v) = &f.verification else { continue };
            assert_eq!(
                v.command,
                format!("oprobe verify 20261121-101010-abcd {}", f.code)
            );
            assert!(
                v.rerun
                    .starts_with("oprobe run --from 20261121-101010-abcd --set "),
                "{}",
                v.rerun
            );
            assert!(!v.overrides.is_empty(), "{v:?}");
            // The self-contained replay is the run's spec with only the change applied (the
            // CLI parses `RunSpec::command` back to the same profile: `oprobe`'s cli tests).
            let changed = run.report.spec.with(&v.overrides);
            assert_eq!(v.replay, changed.command());
            assert_eq!(changed.target_ref, "s3://bkt/x", "{}", v.replay);
            let got = changed.resolved_profile();
            assert_eq!(got.target.options["endpoint"], "https://minio.local:9000");
            assert_eq!(got.workload.seed, 9);
            assert_eq!(got.workload.warmup.get().as_millis(), 500);
        }
    }

    #[test]
    fn tcp_retransmits_count_losses_during_requests_not_connection_history() {
        let tcp = |retrans: u64| {
            Some(Box::new(crate::model::TcpRow {
                rtt_us: 1_000,
                retrans,
                ..Default::default()
            }))
        };
        // One early retransmit on a long-lived connection taints nothing after it.
        let mut run = straggler_run();
        for (i, q) in run.reqs.iter_mut().enumerate() {
            q.conn_id = (i % 4) as u64;
            q.reused = i >= 4;
            q.tcp = tcp(u64::from(q.conn_id == 0));
        }
        let codes =
            |run: &Run| -> Vec<String> { derive(run, &[]).into_iter().map(|f| f.code).collect() };
        assert!(!codes(&run).contains(&"tcp_retransmits".to_owned()));
        // Losses that keep growing while requests run are a finding.
        for (i, q) in run.reqs.iter_mut().enumerate() {
            q.tcp = tcp(i as u64 / 20);
        }
        let f = derive(&run, &[]);
        let t = f
            .iter()
            .find(|x| x.code == "tcp_retransmits")
            .expect("finding");
        assert!(
            t.evidence.values["retransmit_requests_count"] >= 40.0,
            "{t:#?}"
        );
        // Loopback remotes (a local lab) are never a network path.
        for q in &mut run.reqs {
            q.remote = Some("127.0.0.1:9000".parse().unwrap());
        }
        assert!(!codes(&run).contains(&"tcp_retransmits".to_owned()));
    }

    #[test]
    fn stragglers_are_found_on_their_own_api_even_when_slower_writes_dominate_the_tail() {
        use crate::analysis::outliers::test_req;
        use crate::analysis::simulate::tests::op;
        let mut run = straggler_run();
        run.ops.clear();
        run.reqs.clear();
        for i in 0..2000u64 {
            let t = i as i64 * 2_000;
            let (api, lat) = if i % 2 == 0 {
                // GETs: 5 ms, 1 in 25 waits 300 ms more for the first byte.
                (
                    "GetObject",
                    if (i / 2) % 25 == 7 {
                        305_000
                    } else {
                        5_000 + i % 7 * 100
                    },
                )
            } else {
                // Writes: spread from 5 ms to 400 ms, the slowest requests of the run.
                ("PutObject", 5_000 + (i * 7919 % 1000) * 400)
            };
            run.ops.push(op(i + 1, t, lat));
            let mut q = test_req(t, lat);
            q.op_id = i + 1;
            q.api = std::borrow::Cow::Borrowed(api);
            run.reqs.push(q);
        }
        let f = derive(&run, &[]);
        let t = f
            .iter()
            .find(|x| x.code == "tail_stragglers")
            .expect("stragglers");
        assert!(t.cause.starts_with("4.0% of GetObject"), "{}", t.cause);
        let v = &t.evidence.values;
        assert!(v["tail_cut_deadline_ms"] < 300.0, "{v:?}");
        assert!(v["predicted_p99_ms"] < v["current_p99_ms"] / 2.0, "{v:?}");

        // No stragglers anywhere: the rule says nothing rather than something empty.
        for q in run.reqs.iter_mut().filter(|q| q.api == "GetObject") {
            q.total_us = 5_000;
            q.ttfb_us = 4_990;
        }
        let f = derive(&run, &[]);
        assert!(f.iter().all(|x| x.code != "tail_stragglers"), "{f:#?}");
    }
}
