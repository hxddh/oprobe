//! Findings engine 3.0: diagnosis items computed from a run's aggregates *and* its trace rows.
//!
//! Each [`Finding`] has a stable `code` (documented by `oprobe explain CODE`), a severity, a
//! one-sentence cause, evidence (numbers plus references: operation ids, time windows), advice
//! and — where a knob change can test the advice — a verification with the exact
//! `oprobe run …` command. Predictions ("tail-cut at 150 ms would cut p99 from 918 ms to
//! ~140 ms") come from [`super::simulate`], which replays the recorded rows; they are stated
//! as estimates.
//!
//! One finding per code; tail latency is explained once, by its dominant cause.

use std::collections::{BTreeMap, HashMap};

use super::diagnosis::{self, Severity};
use super::outliers::{Selection, attribute, dominant_stage};
use super::simulate::{self, Prediction};
use super::stats::{median, quantile_us_ms};
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
];

struct Ctx<'a> {
    run: &'a Run,
    r: &'a Report,
    network: bool,
    throttled: bool,
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
}

/// Evidence values; every key carries its unit like a report field (design §6).
fn values(kv: &[(&str, f64)]) -> BTreeMap<String, f64> {
    for (k, _) in kv {
        debug_assert!(
            crate::model::has_unit(k),
            "evidence value `{k}` has no unit suffix"
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
/// requests per 1 000 operations (`predicted_extra_requests_per_1k_ops_count`, negative =
/// saved).
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
            "predicted_extra_requests_per_1k_ops_count",
            p.requests_per_1k_ops_delta,
        ),
    ]
}

// ---------------------------------------------------------------------------------------

fn errors(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let s = &r.summary;
    if s.failed_ops == 0 {
        return None;
    }
    let top = r
        .resilience
        .error_classes
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(k, n)| format!("; most common: {k} ×{n}"))
        .unwrap_or_default();
    let failed: Vec<u64> = c
        .run
        .ops
        .iter()
        .filter(|o| !o.ok)
        .map(|o| o.op_id)
        .collect();
    Some(Finding {
        code: "errors".into(),
        severity: if s.error_rate > r.sla.max_error_ratio {
            Severity::Critical
        } else {
            Severity::Warning
        },
        cause: format!("{:.2}% of operations failed.", s.error_rate * 100.0),
        evidence: Evidence {
            text: format!("{} of {} operations failed{top}", s.failed_ops, s.ops),
            values: values(&[
                ("failed_ops_count", s.failed_ops as f64),
                ("error_ratio", s.error_rate),
            ]),
            op_ids: slowest_ops(c.run, failed),
            windows: windows_where(&c.run.reqs, |q| q.is_error() && !q.is_throttle() && q.status != 404),
        },
        advice: "Inspect the failing requests (`oprobe analyze RUN --where status=…`) and run `oprobe doctor` for credential, permission and endpoint checks.".into(),
        verification: None,
    })
}

/// Requests per second the service accepted while it was throttling (median over throttled
/// seconds), and the in-flight requests that rate corresponds to.
fn throttle_ceiling(reqs: &[ReqRow]) -> Option<(f64, f64)> {
    let mut per: BTreeMap<i64, (u64, u64, u64)> = BTreeMap::new();
    for q in reqs {
        let e = per.entry(q.t_us.div_euclid(1_000_000)).or_default();
        if q.is_throttle() {
            e.0 += 1;
        } else {
            e.1 += 1;
            e.2 += q.total_us;
        }
    }
    let rates: Vec<f64> = per
        .values()
        .filter(|e| e.0 > 0)
        .map(|e| e.1 as f64)
        .collect();
    if rates.is_empty() {
        return None;
    }
    let ok: Vec<&ReqRow> = reqs.iter().filter(|q| !q.is_throttle()).collect();
    let mean_s = ok.iter().map(|q| q.total_us).sum::<u64>() as f64 / ok.len().max(1) as f64 / 1e6;
    let rps = median(&rates);
    Some((rps, rps * mean_s))
}

fn throttling(c: &Ctx) -> Option<Finding> {
    let r = c.r;
    let thr = r.resilience.throttled_responses;
    if thr == 0 {
        return None;
    }
    let requests = r.connections.requests.max(1) as f64;
    let ceiling = throttle_ceiling(&c.run.reqs);
    let amp = r.bytes.request_amplification.max(1.0);
    let mut text = format!(
        "{thr} throttled responses ({:.2}% of requests); {} retries, {:.0}% of operation time in retry backoff; max Retry-After {} ms",
        thr as f64 * 100.0 / requests,
        r.resilience.retries,
        r.stages.shares.retry_pct,
        r.resilience.max_retry_after_ms
    );
    let mut vals = vec![
        ("throttled_responses_count", thr as f64),
        ("throttle_ratio", thr as f64 / requests),
        ("retry_pct", r.stages.shares.retry_pct),
    ];
    let mut advice = "The service is rate-limiting this request rate: lower `concurrency` / `max_inflight_requests` below the ceiling, spread keys across more prefixes, keep retries with jitter.".to_owned();
    let verification;
    if let Some((rps, inflight)) = ceiling {
        let safe_c = ((inflight / amp).floor() as u64).max(1);
        text += &format!(
            "; while throttled the service accepted ≈{rps:.0} requests/s (≈{inflight:.0} in flight)"
        );
        vals.push(("ceiling_requests_per_s", rps));
        vals.push(("ceiling_inflight_count", inflight));
        advice = format!(
            "The service accepts ≈{rps:.0} requests/s here: run at concurrency ≤ {safe_c} (or cap `max_inflight_requests` near {:.0}), spread keys across more prefixes, keep retries with jitter.",
            inflight.max(1.0)
        );
        verification = c.verify(
            format!(
                "re-run at concurrency {safe_c}: throttling should disappear at a similar throughput"
            ),
            &[("concurrency", safe_c.to_string())],
        );
    } else {
        let half = (r.knobs.requested.concurrency_count / 2).max(1);
        verification = c.verify(
            format!("re-run at half the concurrency ({half}); `oprobe tune` finds the ceiling"),
            &[("concurrency", half.to_string())],
        );
    }
    let ids = c.run.ops.iter().filter(|o| o.throttled).map(|o| o.op_id);
    Some(Finding {
        code: "throttling".into(),
        severity: Severity::Critical,
        cause: format!(
            "The service throttled {:.1}% of requests (429/503).",
            thr as f64 * 100.0 / requests
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
    Some(Finding {
        code: "retries".into(),
        severity: Severity::Warning,
        cause: format!("{share:.0}% of operation time is retry backoff after failed requests."),
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
        advice: "Requests fail with temporary errors and are retried after a backoff: inspect them with `oprobe analyze RUN --where attempt=retry --by status`.".into(),
        verification: None,
    })
}

/// A tail-cut must take at least this share off the affected operations' p99 to be advice.
const TAIL_CUT_MIN_GAIN: f64 = 1.0 / 3.0;

/// The API whose stragglers a tail-cut would cut best: its threshold, the replayed prediction
/// and the successful requests above the threshold. `None` when no API has requests above its
/// threshold that mostly wait for the first byte, or the replay predicts no substantial gain.
fn straggler_candidate(run: &Run) -> Option<(String, u64, Prediction, Vec<&ReqRow>)> {
    let mut apis: Vec<&str> = run.reqs.iter().map(|q| q.api.as_ref()).collect();
    apis.sort_unstable();
    apis.dedup();
    let mut best: Option<(String, u64, Prediction, Vec<&ReqRow>)> = None;
    for api in apis {
        let Some(cut) = simulate::tail_cut_threshold_us(&run.reqs, api) else {
            continue;
        };
        let over: Vec<&ReqRow> = run
            .reqs
            .iter()
            .filter(|q| q.api == api && !q.is_error() && q.total_us > cut)
            .collect();
        let ttfb_bound = over.iter().filter(|q| dominant_stage(q) == "ttfb").count();
        if over.len() < 3 || ttfb_bound * 2 < over.len() {
            continue;
        }
        let Some(pred) = simulate::tail_cut(run, api, cut) else {
            continue;
        };
        let gain = pred.current - pred.predicted;
        if pred.current <= 0.0 || gain < TAIL_CUT_MIN_GAIN * pred.current {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|b| gain > b.2.current - b.2.predicted)
        {
            best = Some((api.to_owned(), cut, pred, over));
        }
    }
    best
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
    let (stage, _) = by_stage.iter().max_by_key(|(_, n)| **n)?;
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
    // for the server's first byte, where a tail-cut would take a large part off the p99. The
    // API is the one where the replay saves the most, not the one most common among the
    // slowest requests (slow writes on a slow machine can outnumber a few GET stragglers).
    if let Some((api, cut, pred, over)) = straggler_candidate(run) {
        let all_api = run
            .reqs
            .iter()
            .filter(|q| q.api == api && !q.is_error())
            .count();
        let share = over.len() as f64 * 100.0 / all_api.max(1) as f64;
        let mut ttfb: Vec<u64> = over.iter().map(|q| q.ttfb_us).collect();
        let wait = quantile_us_ms(&mut ttfb, 0.5);
        let cause = format!(
            "{share:.1}% of {api} requests wait ~{} for the server's first byte (stragglers); tail-cut at {} ms would cut p99 from {} to ~{}.",
            fmt_ms(wait),
            cut / 1000,
            fmt_ms(pred.current),
            fmt_ms(pred.predicted)
        );
        vals.push(("straggler_pct", share));
        vals.push(("straggler_ttfb_ms", wait));
        vals.extend(pred_values(&pred));
        vals.push(("tail_cut_threshold_ms", ms(cut)));
        ev_text += &format!(
            "; {} — about {:.0} extra requests per 1 000 operations",
            pred.note, pred.requests_per_1k_ops_delta
        );
        let verification = if c.r.knobs.requested.tail_cut {
            None
        } else {
            c.verify(
                format!(
                    "enable tail-cut and compare p99 (predicted ~{})",
                    fmt_ms(pred.predicted)
                ),
                &[("tail_cut", "on".into())],
            )
        };
        return Some(Finding {
            code: "tail_stragglers".into(),
            severity: Severity::Warning,
            cause,
            evidence: Evidence {
                text: ev_text,
                values: values(&vals),
                op_ids: slowest_ops(run, over.iter().map(|q| q.op_id).filter(|id| *id != 0)),
                windows: Vec::new(),
            },
            advice: if c.r.knobs.requested.tail_cut {
                "Tail-cut is already on: check the slow requests' remotes (`oprobe analyze RUN --slowest 2 --by remote`) and the service's health.".into()
            } else {
                "Cut server stragglers with `tail_cut = true` (OpenDAL TailCutLayer re-issues requests slower than the threshold); the extra requests cost a little.".into()
            },
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
        advice: format!(
            "Look at the slowest operations (`oprobe analyze RUN --slowest 2 --by stage`); the {stage} stage is where the tail comes from."
        ),
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
        advice: "Raise `max_inflight_requests` (0 = unlimited) or lower `concurrency × io_concurrency` so requests stop queueing behind each other.".into(),
        verification: (cap > 0)
            .then(|| c.verify("remove the in-flight cap and compare p99", &[("max_inflight_requests", "0".into())]))
            .flatten(),
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
    let rtt_p50 = quantile_us_ms(&mut rtt, 0.5);
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
    let cc = c.r.knobs.requested.concurrency_count;
    let pred = simulate::concurrency(c.run, cc * 2, None);
    let mut text = format!(
        "mean {} per request; {share:.0}% of request time waiting for the server's answer",
        fmt_bytes(mean_bytes)
    );
    let mut vals = vec![
        ("mean_request_bytes", mean_bytes as f64),
        ("ttfb_pct", share),
    ];
    if let Some(p) = &pred {
        text += &format!("; {}", pred_text(p));
        vals.extend(pred_values(p));
    }
    Some(Finding {
        code: "small_objects".into(),
        severity: Severity::Info,
        cause: "Requests are small: throughput is bound by request round trips, not bandwidth.".into(),
        evidence: Evidence {
            text,
            values: values(&vals),
            ..Default::default()
        },
        advice: "Raise parallelism (`concurrency`) until throttling or the latency SLA stops you, batch or pack small objects, and coalesce neighbouring ranges (`range_gap`).".into(),
        verification: pred
            .is_some()
            .then(|| c.verify(format!("double the concurrency to {}", cc * 2), &[("concurrency", (cc * 2).to_string())]))
            .flatten(),
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
        "server_latency" if !c.throttled => {
            let cc = c.r.knobs.requested.concurrency_count;
            if let Some(p) = simulate::concurrency(c.run, cc * 2, None) {
                out.evidence.text += &format!("; {}", pred_text(&p));
                out.evidence.values = values(&pred_values(&p));
                out.verification = c.verify(
                    format!("double the concurrency to {}", cc * 2),
                    &[("concurrency", (cc * 2).to_string())],
                );
            }
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
    out
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
            t.cause.contains("GetObject") && t.cause.contains("tail-cut at"),
            "{}",
            t.cause
        );
        assert!(t.evidence.values["predicted_p99_ms"] < 100.0, "{t:#?}");
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
        run.report.knobs.requested.concurrency_count = 32;
        run.report.bytes.request_amplification = 1.0;
        for (i, q) in run.reqs.iter_mut().enumerate() {
            if i % 10 == 0 {
                q.status = 503;
            }
        }
        let f = derive(&run, &[]);
        let t = f.iter().find(|x| x.code == "throttling").unwrap();
        assert_eq!(f[0].code, "throttling");
        assert!(
            t.evidence.values.contains_key("ceiling_requests_per_s"),
            "{t:#?}"
        );
        assert!(!t.evidence.windows.is_empty());
        let v = t.verification.as_ref().unwrap();
        assert_ne!(v.overrides.knobs["concurrency"], "32", "{v:?}");
        // Throttling explains the run: no separate tail item about retries.
        assert!(f.iter().all(|x| x.code != "retries"));
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

    /// Parse `cmd` (as printed) with the CLI parser; the `run` arguments.
    fn parse_run(cmd: &str) -> crate::cli::args::RunArgs {
        use clap::Parser;
        let words: Vec<&str> = cmd.split_whitespace().collect();
        assert_eq!(words[0], "oprobe");
        let cli =
            crate::cli::args::Cli::try_parse_from(&words).unwrap_or_else(|e| panic!("{cmd}: {e}"));
        match cli.command {
            Some(crate::cli::args::Command::Run(a)) => a,
            other => panic!("not a run: {other:?}"),
        }
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
            // The self-contained replay builds the run's profile with only the change applied.
            let a = parse_run(&v.replay);
            assert_eq!(
                a.target.target.as_deref(),
                Some("s3://bkt/x"),
                "{}",
                v.replay
            );
            let mut got =
                crate::cli::build_profile(&a.target, &a.workload, &a.knobs, &a.cost).unwrap();
            let mut label = None;
            crate::model::Overrides::parse(&a.set)
                .unwrap()
                .apply(&mut got, &mut label)
                .unwrap();
            let want = run.report.spec.with(&v.overrides).resolved_profile();
            assert_eq!(got.workload, want.workload, "{}", v.replay);
            assert_eq!(got.knobs, want.knobs, "{}", v.replay);
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
        assert!(v["tail_cut_threshold_ms"] < 300.0, "{v:?}");
        assert!(v["predicted_p99_ms"] < v["current_p99_ms"] / 3.0, "{v:?}");

        // No stragglers anywhere: the rule says nothing rather than something empty.
        for q in run.reqs.iter_mut().filter(|q| q.api == "GetObject") {
            q.total_us = 5_000;
            q.ttfb_us = 4_990;
        }
        let f = derive(&run, &[]);
        assert!(f.iter().all(|x| x.code != "tail_stragglers"), "{f:#?}");
    }
}
