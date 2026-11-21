//! The regression gate of `oprobe compare` (design v4 §7): identity, then machine-grade checks.
//!
//! - **Identity first:** a gate verdict needs the same target ([`crate::store::TargetId`]) and
//!   the same workload ([`crate::model::RunSpec::workload_hash`]). Otherwise the result is
//!   `comparable: false` with the differing settings, and no regression verdict.
//! - **Checks** are `{ id, unit, baseline, candidate, delta_ratio, limit_ratio, verdict }`:
//!   stable ids (`throughput`, `latency_p99.read.range`, `throttled_request_ratio`, …), values in
//!   `unit`, relative limits as ratios.
//! - **Count metrics have absolute floors:** going from 0 to 1 throttled response (or one
//!   failed operation) is not a regression.
//!
//! [`crate::analysis::compare`] adds the statistics (significance and noise floors) on top.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::Report;
use super::style::*;
use crate::util::fmt_ms;

/// Samples a percentile needs beyond it to be gated (a p99 needs ≥ 1000 samples).
pub const MIN_TAIL_SAMPLES: u64 = 10;

/// Regression limits. Relative limits are ratios of the baseline in the "bad" direction
/// (0.2 = 20 % worse).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Thresholds {
    pub p99_increase_ratio: f64,
    pub p50_increase_ratio: f64,
    /// Latency changes smaller than this (ms) are never regressions.
    pub min_latency_delta_ms: f64,
    pub throughput_drop_ratio: f64,
    /// Max increase of the failed-operation ratio (absolute, 0.005 = 0.5 percentage points).
    pub error_increase_ratio: f64,
    /// Fewer additional failed operations than this are never a regression (count floor).
    pub min_failed_ops_count: u64,
    pub request_amp_increase_ratio: f64,
    pub read_amp_increase_ratio: f64,
    pub cost_increase_ratio: f64,
    /// Gate throttling: fail when the candidate's throttled-request ratio grows past
    /// [`Thresholds::throttle_increase_ratio`] (relative), [`THROTTLE_RATIO_FLOOR`] (absolute)
    /// and [`Thresholds::min_throttled_count`] (count floor).
    pub fail_on_new_throttling: bool,
    /// Relative growth of the throttled-request ratio that fails the gate.
    pub throttle_increase_ratio: f64,
    /// Fewer throttled responses than this in the candidate are never a regression
    /// (0 → 1 throttled response is noise).
    pub min_throttled_count: u64,
    /// Skip latency checks for operations with fewer samples.
    pub min_samples_count: u64,
    /// Compare runs of different workloads anyway (otherwise `comparable: false`).
    #[serde(default)]
    pub allow_workload_change: bool,
    /// Compare runs against different targets anyway (otherwise `comparable: false`).
    #[serde(default)]
    pub allow_target_change: bool,
    /// Runs that are not comparable are an error (exit 2) instead of a warning (exit 0).
    #[serde(default)]
    pub strict: bool,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            p99_increase_ratio: 0.20,
            p50_increase_ratio: 0.20,
            min_latency_delta_ms: 1.0,
            throughput_drop_ratio: 0.10,
            error_increase_ratio: 0.005,
            min_failed_ops_count: 5,
            request_amp_increase_ratio: 0.10,
            read_amp_increase_ratio: 0.10,
            cost_increase_ratio: 0.10,
            fail_on_new_throttling: true,
            throttle_increase_ratio: 0.25,
            min_throttled_count: 10,
            min_samples_count: 30,
            allow_workload_change: false,
            allow_target_change: false,
            strict: false,
        }
    }
}

/// Throttled-ratio changes below this (fraction of requests, 0.1 pp) are noise.
pub const THROTTLE_RATIO_FLOOR: f64 = 0.001;
/// Max increase of the open-loop missed-arrival share (absolute ratio, 1 pp).
pub const MISSED_RATIO: f64 = 0.01;

/// The verdict of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Improved,
    Fail,
    Skipped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Improved => "improved",
            Status::Fail => "fail",
            Status::Skipped => "skipped",
        }
    }
}

/// One gate check, machine-grade (design v4 §7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Check {
    /// Stable id: `throughput`, `throughput_bytes`, `latency_p50.VERB`, `latency_p99.VERB`,
    /// `present.VERB`, `error_ratio`, `throttled_request_ratio`, `open_loop_missed_ratio`,
    /// `corrupted_objects`, `requests_per_op`, `read_amplification`, `cost_per_million_ops`.
    pub id: String,
    /// Unit of `baseline`, `candidate` and `abs_limit` (`ops_per_s`, `mib_per_s`, `ms`,
    /// `ratio`, `count`, `requests_per_op`, `usd`).
    pub unit: String,
    pub baseline: f64,
    pub candidate: f64,
    /// `(candidate − baseline) / baseline`; absent when the baseline is 0 and the candidate
    /// is not.
    pub delta_ratio: Option<f64>,
    /// Relative change in the bad direction that fails the check (absent: no relative limit).
    pub limit_ratio: Option<f64>,
    /// Absolute limit or floor in `unit` (e.g. the minimum latency change in ms, the
    /// failed-ratio increase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abs_limit: Option<f64>,
    pub verdict: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Check {
    /// A human label of the check id (`p99 read.range`, `throughput`).
    pub fn label(&self) -> String {
        let (kind, verb) = self.id.split_once('.').unwrap_or((&self.id, ""));
        match kind {
            "latency_p50" => format!("{verb} p50"),
            "latency_p99" => format!("{verb} p99"),
            "present" => format!("{verb} present"),
            "throughput_bytes" => "throughput MiB/s".into(),
            "throughput" => "throughput ops/s".into(),
            "error_ratio" => "failed ops".into(),
            "throttled_request_ratio" => "throttled requests".into(),
            "open_loop_missed_ratio" => "open-loop missed".into(),
            "corrupted_objects" => "integrity (corrupted)".into(),
            "requests_per_op" => "requests per op".into(),
            "read_amplification" => "read amplification".into(),
            "cost_per_million_ops" => "cost per 1M ops".into(),
            _ => self.id.clone(),
        }
    }

    /// One value of this check, formatted for its unit.
    pub fn fmt_value(&self, v: f64) -> String {
        match self.unit.as_str() {
            "ms" => fmt_ms(v),
            "usd" => format!("${v:.4}"),
            "ratio" => format!("{:.2}%", v * 100.0),
            "count" => format!("{v:.0}"),
            _ if v.fract() == 0.0 && v.abs() < 1e12 => format!("{v:.0}"),
            _ => format!("{v:.2}"),
        }
    }

    /// The change, formatted (`+12.3%`, `+0.40pp` for ratios, `new` when the baseline is 0).
    pub fn fmt_delta(&self) -> String {
        if self.unit == "ratio" {
            return format!("{:+.2}pp", (self.candidate - self.baseline) * 100.0);
        }
        match self.delta_ratio {
            Some(d) => format!("{:+.1}%", d * 100.0),
            None => "new".into(),
        }
    }

    /// The limit, formatted (`≤ +20%`, `≥ −10%`, `≤ +0.50pp`).
    pub fn fmt_limit(&self) -> String {
        let mut parts = Vec::new();
        if let Some(l) = self.limit_ratio {
            parts.push(if self.id.starts_with("throughput") {
                format!("≥ −{:.0}%", l * 100.0)
            } else {
                format!("≤ +{:.0}%", l * 100.0)
            });
        }
        if let Some(a) = self.abs_limit {
            parts.push(match self.unit.as_str() {
                "ms" => format!("> {}", fmt_ms(a)),
                "ratio" => format!("≤ +{:.2}pp", a * 100.0),
                "count" => format!("≥ {a:.0}"),
                _ => format!("> {a}"),
            });
        }
        if parts.is_empty() {
            "informational".into()
        } else {
            parts.join(" & ")
        }
    }
}

/// What part of the setup a [`ConfigChange`] is in.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// What was run (preset, mix, dataset, access pattern, rate…): results are not comparable.
    Workload,
    /// Where it ran (scheme, bucket/prefix, endpoint, backend options).
    Target,
    /// Client configuration: the legitimate subject of an A/B comparison.
    Knobs,
    /// The machine, network path or oprobe version that measured.
    Host,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Workload => "workload",
            ChangeKind::Target => "target",
            ChangeKind::Knobs => "knobs",
            ChangeKind::Host => "host",
        }
    }
}

/// One setting that differs between baseline and candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConfigChange {
    pub kind: ChangeKind,
    /// Dotted path, e.g. `knobs.concurrency`, `workload.mix`.
    pub field: String,
    pub baseline: String,
    pub candidate: String,
}

/// Whether two runs may be gated against each other (design v4 §7).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Identity {
    /// Same target identity and workload hash (or the difference was allowed).
    pub comparable: bool,
    pub same_target: bool,
    pub same_workload: bool,
    pub baseline_target: String,
    pub candidate_target: String,
    pub baseline_workload_hash: String,
    pub candidate_workload_hash: String,
    /// Why the runs are not comparable (empty when they are).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

/// The gate's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GateVerdict {
    /// Comparable and no check failed.
    Passed,
    /// Comparable and at least one check failed (exit 3).
    Regression,
    /// Different target or workload: no regression verdict (exit 0 with a warning, exit 2
    /// with `--strict`).
    NotComparable,
}

impl GateVerdict {
    pub fn label(self) -> &'static str {
        match self {
            GateVerdict::Passed => "passed",
            GateVerdict::Regression => "regression",
            GateVerdict::NotComparable => "not comparable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffReport {
    pub verdict: GateVerdict,
    /// The verdict is `passed`.
    pub passed: bool,
    pub comparable: bool,
    pub baseline: String,
    pub candidate: String,
    pub thresholds: Thresholds,
    pub identity: Identity,
    /// Every workload / target / knob / host setting that differs between the two runs.
    pub config_changes: Vec<ConfigChange>,
    pub checks: Vec<Check>,
    pub warnings: Vec<String>,
}

impl DiffReport {
    /// Recompute `verdict` and `passed` from the identity and the checks.
    pub fn settle(&mut self) {
        self.verdict = if !self.comparable {
            GateVerdict::NotComparable
        } else if self.checks.iter().any(|c| c.verdict == Status::Fail) {
            GateVerdict::Regression
        } else {
            GateVerdict::Passed
        };
        self.passed = self.verdict == GateVerdict::Passed;
    }

    /// The exit code of this verdict (3 on a regression; 2 for non-comparable runs under
    /// `--strict`).
    pub fn exit_code(&self) -> u8 {
        match self.verdict {
            GateVerdict::Passed => crate::errors::exit::OK,
            GateVerdict::Regression => crate::errors::exit::GATE,
            GateVerdict::NotComparable if self.thresholds.strict => crate::errors::exit::USAGE,
            GateVerdict::NotComparable => crate::errors::exit::OK,
        }
    }

    pub fn check(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }
}

/// Flatten a JSON value into `prefix.a.b = value` leaves (arrays are compared whole).
fn flatten(
    prefix: &str,
    v: &serde_json::Value,
    out: &mut std::collections::BTreeMap<String, String>,
) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                flatten(&format!("{prefix}.{k}"), x, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix.to_owned(), s.clone());
        }
        other => {
            out.insert(prefix.to_owned(), other.to_string());
        }
    }
}

fn changes_of<T: Serialize>(
    kind: ChangeKind,
    prefix: &str,
    b: &T,
    c: &T,
    ignore: &[&str],
    out: &mut Vec<ConfigChange>,
) {
    let (mut bm, mut cm) = (Default::default(), Default::default());
    flatten(
        prefix,
        &serde_json::to_value(b).unwrap_or_default(),
        &mut bm,
    );
    flatten(
        prefix,
        &serde_json::to_value(c).unwrap_or_default(),
        &mut cm,
    );
    let keys: std::collections::BTreeSet<&String> = bm.keys().chain(cm.keys()).collect();
    for k in keys {
        if ignore.iter().any(|i| k == &format!("{prefix}.{i}")) {
            continue;
        }
        let (bv, cv) = (bm.get(k), cm.get(k));
        if bv != cv {
            let show = |v: Option<&String>| v.cloned().unwrap_or_else(|| "(unset)".into());
            out.push(ConfigChange {
                kind,
                field: k.clone(),
                baseline: show(bv),
                candidate: show(cv),
            });
        }
    }
}

/// Whether the run measured an in-process `lab://` server (by URL, or as `oprobe demo` names it).
fn is_lab(r: &Report) -> bool {
    r.target.display.starts_with("lab://")
        || r.target
            .name
            .as_deref()
            .is_some_and(|n| n.starts_with("lab://"))
}

/// Settings that differ between two reports. The measurement window (duration, warmup,
/// max_ops), the cleanup policy and the dataset's persistence do not change what is measured.
pub fn config_changes(base: &Report, cand: &Report) -> Vec<ConfigChange> {
    let mut out = Vec::new();
    changes_of(
        ChangeKind::Workload,
        "workload",
        &base.workload,
        &cand.workload,
        &[
            "duration_s",
            "warmup_s",
            "max_ops_count",
            "cleanup",
            "dataset.persist",
        ],
        &mut out,
    );
    let target = |r: &Report| {
        // A `lab://` target is identified by its scenario: its endpoint is the ephemeral port
        // of an in-process server and differs on every run.
        let (endpoint, options) = if is_lab(r) {
            let mut o = r.target.options.clone();
            o.remove("endpoint");
            (None, o)
        } else {
            (r.target.endpoint.clone(), r.target.options.clone())
        };
        serde_json::json!({
            "scheme": r.target.scheme,
            "display": crate::util::display_url(&r.target.display),
            "endpoint": endpoint,
            "options": options,
        })
    };
    changes_of(
        ChangeKind::Target,
        "target",
        &target(base),
        &target(cand),
        &[],
        &mut out,
    );
    changes_of(
        ChangeKind::Knobs,
        "knobs.requested",
        &base.knobs.requested,
        &cand.knobs.requested,
        &[],
        &mut out,
    );
    let effective = |r: &Report| {
        let e = &r.knobs.effective;
        serde_json::json!({
            "write_chunk_bytes": e.write_chunk_bytes,
            "service_min_part_bytes": e.service_min_part_bytes,
            "service_max_part_bytes": e.service_max_part_bytes,
        })
    };
    changes_of(
        ChangeKind::Knobs,
        "knobs.effective",
        &effective(base),
        &effective(cand),
        &[],
        &mut out,
    );
    let host = |r: &Report| {
        serde_json::json!({
            "os": r.run.host.os,
            "arch": r.run.host.arch,
            "cpus_count": r.run.host.cpus_count,
            "proxy": r.target.proxy,
            "oprobe_version": r.run.oprobe_version,
        })
    };
    changes_of(
        ChangeKind::Host,
        "host",
        &host(base),
        &host(cand),
        &[],
        &mut out,
    );
    out
}

/// The target identity of a report: its spec's [`crate::store::TargetId`], or the displayed
/// target when the spec's target does not resolve.
pub fn target_identity(r: &Report) -> String {
    match r.spec.target_id() {
        Some(id) => id.to_string(),
        None => crate::util::display_url(&r.target.display),
    }
}

/// Same target identity and workload hash (design v4 §7), with the allowed differences.
pub fn identity(
    base: &Report,
    cand: &Report,
    changes: &[ConfigChange],
    t: &Thresholds,
) -> Identity {
    let (bt, ct) = (target_identity(base), target_identity(cand));
    let (bw, cw) = (base.spec.workload_hash(), cand.spec.workload_hash());
    let same_target = bt == ct;
    let same_workload = bw == cw;
    let mut reasons = Vec::new();
    let fields = |k: ChangeKind| {
        changes
            .iter()
            .filter(|c| c.kind == k)
            .map(|c| c.field.clone())
            .collect::<Vec<_>>()
    };
    if !same_target && !t.allow_target_change {
        reasons.push(format!("different targets: {bt} vs {ct}"));
    }
    if !same_workload && !t.allow_workload_change {
        let keys = fields(ChangeKind::Workload);
        let (bwk, cwk) = (&base.spec.profile.workload, &cand.spec.profile.workload);
        let mut window = Vec::new();
        if bwk.duration != cwk.duration {
            window.push(format!("duration {} vs {}", bwk.duration, cwk.duration));
        }
        if bwk.warmup != cwk.warmup {
            window.push(format!("warmup {} vs {}", bwk.warmup, cwk.warmup));
        }
        if bwk.max_ops != cwk.max_ops {
            window.push("max_ops".to_owned());
        }
        let mut what: Vec<String> = keys;
        what.extend(window);
        reasons.push(if what.is_empty() {
            format!("different workloads ({bw} vs {cw})")
        } else {
            format!("different workloads: {}", what.join(", "))
        });
    }
    Identity {
        comparable: reasons.is_empty(),
        same_target,
        same_workload,
        baseline_target: bt,
        candidate_target: ct,
        baseline_workload_hash: bw,
        candidate_workload_hash: cw,
        reasons,
    }
}

/// `(c − b) / b`, `None` when only the baseline is 0.
fn rel(b: f64, c: f64) -> Option<f64> {
    if b.abs() < 1e-12 {
        if c.abs() < 1e-12 { Some(0.0) } else { None }
    } else {
        Some((c - b) / b)
    }
}

struct Builder {
    checks: Vec<Check>,
}

impl Builder {
    fn push(&mut self, c: Check) {
        self.checks.push(c);
    }

    /// Higher-is-worse metric with a relative limit and an absolute noise floor.
    #[allow(clippy::too_many_arguments)]
    fn higher_worse(
        &mut self,
        id: String,
        unit: &str,
        b: f64,
        c: f64,
        limit: f64,
        min_abs: f64,
        show_floor: bool,
    ) {
        let d = rel(b, c);
        let up = d.unwrap_or(f64::INFINITY);
        let verdict = if c - b > min_abs && up > limit {
            Status::Fail
        } else if b - c > min_abs && -up > limit {
            Status::Improved
        } else {
            Status::Pass
        };
        self.push(Check {
            id,
            unit: unit.into(),
            baseline: b,
            candidate: c,
            delta_ratio: d,
            limit_ratio: Some(limit),
            abs_limit: show_floor.then_some(min_abs),
            verdict,
            note: String::new(),
        });
    }

    fn lower_worse(&mut self, id: String, unit: &str, b: f64, c: f64, limit: f64) {
        let d = rel(b, c);
        let ch = d.unwrap_or(f64::INFINITY);
        let verdict = if -ch > limit {
            Status::Fail
        } else if ch > limit {
            Status::Improved
        } else {
            Status::Pass
        };
        self.push(Check {
            id,
            unit: unit.into(),
            baseline: b,
            candidate: c,
            delta_ratio: d,
            limit_ratio: Some(limit),
            abs_limit: None,
            verdict,
            note: String::new(),
        });
    }
}

/// Throttled responses / requests.
pub fn throttled_request_ratio(r: &Report) -> f64 {
    crate::util::ratio(
        r.resilience.throttled_responses as f64,
        r.connections.requests as f64,
    )
}

/// Successful operations per second (failed operations never count).
pub fn good_ops_per_s(r: &Report) -> f64 {
    let s = &r.summary;
    if r.run.window_s > 0.0 {
        s.ops.saturating_sub(s.failed_ops) as f64 / r.run.window_s
    } else {
        s.ops_per_s
    }
}

/// Share of open-loop arrivals that never started.
fn missed_ratio(s: &Report) -> Option<f64> {
    let ol = s.open_loop.as_ref()?;
    let issued = ol.achieved_rate * s.run.window_s;
    Some(crate::util::ratio(
        ol.missed as f64,
        issued + ol.missed as f64,
    ))
}

pub fn diff(base: &Report, cand: &Report, t: &Thresholds) -> DiffReport {
    let mut warnings = Vec::new();
    for (side, r) in [("baseline", base), ("candidate", cand)] {
        if r.run.interrupted {
            warnings.push(format!(
                "the {side} run was interrupted (stopped early): its results cover a shorter window and may not be comparable; re-run it to completion"
            ));
        }
    }
    match (base.workload.rate_per_s, cand.workload.rate_per_s) {
        (None, Some(_)) | (Some(_), None) => warnings.push(
            "comparing a closed-loop run with an open-loop (fixed-rate) run: throughput and latency mean different things in each mode".into(),
        ),
        (Some(a), Some(b)) if (a - b).abs() > 0.01 * a.max(b) => warnings.push(format!(
            "open-loop target rates differ ({a} vs {b} ops/s)"
        )),
        _ => {}
    }
    let changes = config_changes(base, cand);
    let ident = identity(base, cand, &changes, t);
    for r in &ident.reasons {
        warnings.push(format!(
            "not comparable — {r}: no regression verdict (replay the baseline with `oprobe run --from {}` for a comparable candidate{})",
            base.run.id,
            if t.strict { "" } else { "; --strict makes this exit 2" }
        ));
    }
    let fields = |k: ChangeKind| {
        changes
            .iter()
            .filter(|c| c.kind == k)
            .map(|c| {
                format!(
                    "{} {} → {}",
                    c.field,
                    summarize(&c.baseline, 32),
                    summarize(&c.candidate, 32)
                )
            })
            .collect::<Vec<_>>()
    };
    for (kind, allowed, same) in [
        (
            ChangeKind::Workload,
            t.allow_workload_change,
            ident.same_workload,
        ),
        (ChangeKind::Target, t.allow_target_change, ident.same_target),
    ] {
        let diffs = fields(kind);
        if allowed && (!same || !diffs.is_empty()) {
            warnings.push(format!(
                "the {} differs{}: compared anyway (allowed), results may not be comparable",
                kind.label(),
                if diffs.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", diffs.join("; "))
                }
            ));
        }
    }
    let knob_diffs = fields(ChangeKind::Knobs);
    if !knob_diffs.is_empty() {
        warnings.push(format!(
            "client knobs differ ({}): an A/B comparison of the configuration — changes below are caused by it",
            knob_diffs.join("; ")
        ));
    }
    let host_diffs = fields(ChangeKind::Host);
    if !host_diffs.is_empty() {
        warnings.push(format!(
            "measured from a different host or oprobe ({}): differences may come from the client machine, not the storage",
            host_diffs.join("; ")
        ));
    }
    let (bs, cs) = (base, cand);
    let mut b = Builder { checks: Vec::new() };
    b.lower_worse(
        "throughput".into(),
        "ops_per_s",
        good_ops_per_s(bs),
        good_ops_per_s(cs),
        t.throughput_drop_ratio,
    );
    if bs.summary.mib_per_s > 0.01 {
        b.lower_worse(
            "throughput_bytes".into(),
            "mib_per_s",
            bs.summary.mib_per_s,
            cs.summary.mib_per_s,
            t.throughput_drop_ratio,
        );
    }
    for bv in &bs.verbs {
        let Some(cv) = cs.verbs.iter().find(|v| v.verb == bv.verb) else {
            b.push(Check {
                id: format!("present.{}", bv.verb),
                unit: "count".into(),
                baseline: bv.count as f64,
                candidate: 0.0,
                delta_ratio: Some(-1.0),
                limit_ratio: None,
                abs_limit: None,
                verdict: Status::Fail,
                note: format!("operation `{}` is missing from the candidate", bv.verb),
            });
            continue;
        };
        let skipped = |id: String, b0: f64, c0: f64, limit: f64, needs: u64, what: &str| Check {
            id,
            unit: "ms".into(),
            baseline: b0,
            candidate: c0,
            delta_ratio: rel(b0, c0),
            limit_ratio: Some(limit),
            abs_limit: None,
            verdict: Status::Skipped,
            note: format!(
                "{} / {} samples (a stable {what} needs ≥ {needs})",
                bv.count, cv.count
            ),
        };
        if bv.count < t.min_samples_count || cv.count < t.min_samples_count {
            b.push(skipped(
                format!("latency_p50.{}", bv.verb),
                bv.latency.p50,
                cv.latency.p50,
                t.p50_increase_ratio,
                t.min_samples_count,
                "p50",
            ));
        } else {
            b.higher_worse(
                format!("latency_p50.{}", bv.verb),
                "ms",
                bv.latency.p50,
                cv.latency.p50,
                t.p50_increase_ratio,
                t.min_latency_delta_ms,
                true,
            );
        }
        // A p99 is only as stable as the samples above it: with fewer than
        // `MIN_TAIL_SAMPLES` beyond the 99th percentile it is essentially the maximum, and one
        // straggler more or less flips it (17 ms → 262 ms between two identical runs).
        let p99_needs = t
            .min_samples_count
            .max((MIN_TAIL_SAMPLES as f64 / (1.0 - 0.99)).round() as u64);
        if bv.count < p99_needs || cv.count < p99_needs {
            b.push(skipped(
                format!("latency_p99.{}", bv.verb),
                bv.latency.p99,
                cv.latency.p99,
                t.p99_increase_ratio,
                p99_needs,
                "p99",
            ));
        } else {
            b.higher_worse(
                format!("latency_p99.{}", bv.verb),
                "ms",
                bv.latency.p99,
                cv.latency.p99,
                t.p99_increase_ratio,
                t.min_latency_delta_ms,
                true,
            );
        }
    }
    // Failed operations: an absolute ratio limit and a count floor.
    let (be, ce) = (bs.summary.error_rate, cs.summary.error_rate);
    let (bf, cf) = (bs.summary.failed_ops, cs.summary.failed_ops);
    let more_failed = cf.saturating_sub(bf);
    b.push(Check {
        id: "error_ratio".into(),
        unit: "ratio".into(),
        baseline: be,
        candidate: ce,
        delta_ratio: rel(be, ce),
        limit_ratio: None,
        abs_limit: Some(t.error_increase_ratio),
        verdict: if ce - be > t.error_increase_ratio && more_failed >= t.min_failed_ops_count {
            Status::Fail
        } else {
            Status::Pass
        },
        note: if ce - be > t.error_increase_ratio && more_failed < t.min_failed_ops_count {
            format!(
                "{bf} → {cf} failed operations: below the floor of {} more",
                t.min_failed_ops_count
            )
        } else {
            format!("{bf} → {cf} failed operations")
        },
    });
    // Throttling: relative growth, an absolute ratio floor and a count floor.
    let (bt, ct) = (
        bs.resilience.throttled_responses,
        cs.resilience.throttled_responses,
    );
    let (btr, ctr) = (throttled_request_ratio(bs), throttled_request_ratio(cs));
    let grew = ctr - btr > THROTTLE_RATIO_FLOOR
        && rel(btr, ctr).is_none_or(|d| d > t.throttle_increase_ratio);
    let enough = ct >= t.min_throttled_count;
    let shrank = bt >= t.min_throttled_count
        && btr - ctr > THROTTLE_RATIO_FLOOR
        && rel(btr, ctr).is_some_and(|d| -d > t.throttle_increase_ratio);
    b.push(Check {
        id: "throttled_request_ratio".into(),
        unit: "ratio".into(),
        baseline: btr,
        candidate: ctr,
        delta_ratio: rel(btr, ctr),
        limit_ratio: t
            .fail_on_new_throttling
            .then_some(t.throttle_increase_ratio),
        abs_limit: t.fail_on_new_throttling.then_some(THROTTLE_RATIO_FLOOR),
        verdict: if t.fail_on_new_throttling && grew && enough {
            Status::Fail
        } else if shrank {
            Status::Improved
        } else {
            Status::Pass
        },
        note: if grew && !enough {
            format!(
                "{bt} → {ct} throttled responses: below the floor of {} responses",
                t.min_throttled_count
            )
        } else {
            format!("{bt} → {ct} throttled responses")
        },
    });
    if let (Some(bm), Some(cm)) = (missed_ratio(bs), missed_ratio(cs)) {
        b.push(Check {
            id: "open_loop_missed_ratio".into(),
            unit: "ratio".into(),
            baseline: bm,
            candidate: cm,
            delta_ratio: rel(bm, cm),
            limit_ratio: None,
            abs_limit: Some(MISSED_RATIO),
            verdict: if cm - bm > MISSED_RATIO {
                Status::Fail
            } else {
                Status::Pass
            },
            note: String::new(),
        });
    }
    b.push(Check {
        id: "corrupted_objects".into(),
        unit: "count".into(),
        baseline: bs.integrity.corrupted as f64,
        candidate: cs.integrity.corrupted as f64,
        delta_ratio: rel(bs.integrity.corrupted as f64, cs.integrity.corrupted as f64),
        limit_ratio: None,
        abs_limit: None,
        verdict: if cs.integrity.corrupted > 0 {
            Status::Fail
        } else {
            Status::Pass
        },
        note: "any corrupted object fails".into(),
    });
    b.higher_worse(
        "requests_per_op".into(),
        "requests_per_op",
        bs.bytes.request_amplification,
        cs.bytes.request_amplification,
        t.request_amp_increase_ratio,
        0.01,
        false,
    );
    if bs.bytes.logical_read > 0 {
        b.higher_worse(
            "read_amplification".into(),
            "times",
            bs.bytes.read_amplification,
            cs.bytes.read_amplification,
            t.read_amp_increase_ratio,
            0.01,
            false,
        );
    }
    if base.cost.applicable && cand.cost.applicable {
        b.higher_worse(
            "cost_per_million_ops".into(),
            "usd",
            base.cost.selected.per_million_ops_usd,
            cand.cost.selected.per_million_ops_usd,
            t.cost_increase_ratio,
            0.0001,
            false,
        );
    }
    let mut d = DiffReport {
        verdict: GateVerdict::Passed,
        passed: true,
        comparable: ident.comparable,
        baseline: format!(
            "{} ({} {})",
            crate::util::display_url(&base.target.display),
            base.run.id,
            base.run.started_at
        ),
        candidate: format!(
            "{} ({} {})",
            crate::util::display_url(&cand.target.display),
            cand.run.id,
            cand.run.started_at
        ),
        thresholds: t.clone(),
        identity: ident,
        config_changes: changes,
        checks: b.checks,
        warnings,
    };
    d.settle();
    d
}

/// A setting value shortened for a table cell: JSON arrays and objects become a summary
/// (`[2 ops: read.range×80, stat×20]`, `{3 keys}`), long strings are cut with `…` (CMP-1).
pub fn summarize(v: &str, max: usize) -> String {
    let t = v.trim();
    if t.chars().count() <= max {
        return t.to_owned();
    }
    if let Ok(j) = serde_json::from_str::<serde_json::Value>(t) {
        match &j {
            serde_json::Value::Array(a) => {
                let names: Vec<String> = a
                    .iter()
                    .map(|x| match x {
                        serde_json::Value::Object(m) => {
                            let kind = m
                                .get("op")
                                .and_then(|o| o.get("kind").or(Some(o)))
                                .and_then(|k| k.as_str().map(str::to_owned))
                                .or_else(|| {
                                    m.get("op")
                                        .and_then(|o| o.as_object())
                                        .and_then(|o| o.keys().next().cloned())
                                })
                                .unwrap_or_else(|| "item".into());
                            match m.get("weight").and_then(|w| w.as_u64()) {
                                Some(w) => format!("{kind}×{w}"),
                                None => kind,
                            }
                        }
                        other => other.to_string(),
                    })
                    .collect();
                let s = format!("[{} items: {}]", a.len(), names.join(", "));
                return cut(&s, max);
            }
            serde_json::Value::Object(m) => {
                return format!("{{{} keys}}", m.len());
            }
            _ => {}
        }
    }
    cut(t, max)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
    o.push('…');
    o
}

/// Width of the setting-value columns of the text compare.
const VALUE_WIDTH: usize = 36;

impl DiffReport {
    pub fn render_terminal(&self) -> String {
        let mut out = String::new();
        let badge_txt = match self.verdict {
            GateVerdict::Passed => badge("PASS", "42;30"),
            GateVerdict::Regression => badge("REGRESSION", "41"),
            GateVerdict::NotComparable => badge("NOT COMPARABLE", "43;30"),
        };
        let _ = writeln!(
            out,
            "{} {}\n  {} {}\n  {} {}",
            badge_txt,
            bold("oprobe compare"),
            dim("baseline "),
            self.baseline,
            dim("candidate"),
            self.candidate
        );
        if !self.config_changes.is_empty() {
            let _ = writeln!(out, "\n{}", bold(&cyan("CONFIGURATION CHANGES")));
            let rows: Vec<Vec<String>> = self
                .config_changes
                .iter()
                .map(|c| {
                    let kind = match c.kind {
                        ChangeKind::Workload | ChangeKind::Target => red(c.kind.label()),
                        _ => yellow(c.kind.label()),
                    };
                    vec![
                        kind,
                        c.field.clone(),
                        summarize(&c.baseline, VALUE_WIDTH),
                        summarize(&c.candidate, VALUE_WIDTH),
                    ]
                })
                .collect();
            out.push_str(&table(
                &["kind", "setting", "baseline", "candidate"],
                &rows,
                2,
            ));
        }
        for w in &self.warnings {
            let _ = writeln!(out, "  {} {}", yellow("warning:"), wrap(w, 100, 11));
        }
        let rows: Vec<Vec<String>> = self
            .checks
            .iter()
            .map(|c| {
                let st = match c.verdict {
                    Status::Pass => green("pass"),
                    Status::Improved => cyan("improved"),
                    Status::Fail if self.comparable => red("FAIL"),
                    Status::Fail => yellow("fail (n/a)"),
                    Status::Skipped => dim("skipped"),
                };
                vec![
                    c.label(),
                    c.fmt_value(c.baseline),
                    c.fmt_value(c.candidate),
                    c.fmt_delta(),
                    c.fmt_limit(),
                    st,
                ]
            })
            .collect();
        out.push('\n');
        out.push_str(&table(
            &[
                "check",
                "baseline",
                "candidate",
                "change",
                "limit",
                "result",
            ],
            &rows,
            2,
        ));
        out
    }

    pub fn render_markdown(&self) -> String {
        let mut md = String::new();
        let _ = writeln!(
            md,
            "## {} oprobe storage gate — {}\n\nBaseline: {}  \nCandidate: {}\n",
            match self.verdict {
                GateVerdict::Passed => "✅",
                GateVerdict::Regression => "❌",
                GateVerdict::NotComparable => "⚠️",
            },
            self.verdict.label(),
            self.baseline,
            self.candidate
        );
        if !self.config_changes.is_empty() {
            let _ = writeln!(
                md,
                "**Configuration changes**\n\n| kind | setting | baseline | candidate |\n|---|---|---|---|"
            );
            for c in &self.config_changes {
                let _ = writeln!(
                    md,
                    "| {} | `{}` | {} | {} |",
                    c.kind.label(),
                    c.field,
                    summarize(&c.baseline, 60).replace('|', "\\|"),
                    summarize(&c.candidate, 60).replace('|', "\\|")
                );
            }
            md.push('\n');
        }
        for w in &self.warnings {
            let _ = writeln!(md, "> ⚠️ {w}\n");
        }
        let _ = writeln!(
            md,
            "| check | baseline | candidate | change | limit | result |\n|---|---:|---:|---:|---|---|"
        );
        for c in &self.checks {
            let st = match c.verdict {
                Status::Pass => "✅ pass",
                Status::Improved => "🚀 improved",
                Status::Fail => "❌ **FAIL**",
                Status::Skipped => "⏭ skipped",
            };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} | {} |",
                c.label(),
                c.fmt_value(c.baseline),
                c.fmt_value(c.candidate),
                c.fmt_delta(),
                c.fmt_limit(),
                st
            );
        }
        md
    }

    /// JUnit XML (one test case per check) for CI systems that render test reports.
    pub fn render_junit(&self) -> String {
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let failing = |c: &Check| self.comparable && c.verdict == Status::Fail;
        let failures = self.checks.iter().filter(|c| failing(c)).count();
        let skipped = self
            .checks
            .iter()
            .filter(|c| c.verdict == Status::Skipped)
            .count();
        let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(
            x,
            "<testsuite name=\"oprobe compare\" tests=\"{}\" failures=\"{failures}\" skipped=\"{skipped}\">",
            self.checks.len() + 1
        );
        let _ = write!(
            x,
            "  <testcase classname=\"oprobe.gate\" name=\"comparable\">"
        );
        if !self.comparable {
            let _ = write!(
                x,
                "<skipped message=\"{}\"/>",
                esc(&self.identity.reasons.join("; "))
            );
        }
        let _ = writeln!(x, "</testcase>");
        for c in &self.checks {
            let _ = write!(
                x,
                "  <testcase classname=\"oprobe.gate\" name=\"{}\">",
                esc(&c.id)
            );
            if failing(c) {
                let _ = write!(
                    x,
                    "<failure message=\"{}\">baseline={} candidate={} unit={} delta_ratio={} limit={}</failure>",
                    esc(&format!("{} regressed {}", c.label(), c.fmt_delta())),
                    c.baseline,
                    c.candidate,
                    esc(&c.unit),
                    c.delta_ratio
                        .map_or("null".to_owned(), |d| format!("{d:.4}")),
                    esc(&c.fmt_limit())
                );
            } else if c.verdict == Status::Skipped {
                let _ = write!(x, "<skipped message=\"{}\"/>", esc(&c.note));
            }
            let _ = writeln!(x, "</testcase>");
        }
        x.push_str("</testsuite>\n");
        x
    }
}

/// Wrap `s` at `width` columns, continuation lines indented by `indent` spaces.
pub fn wrap(s: &str, width: usize, indent: usize) -> String {
    let mut out = String::new();
    let mut line = 0usize;
    for word in s.split_whitespace() {
        let w = word.chars().count();
        if line > 0 && line + 1 + w > width {
            out.push('\n');
            out.push_str(&" ".repeat(indent));
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Profile, TargetSpec};
    use crate::trace::{OpenLoopStats, Quantiles, Snapshot, VerbStats};

    fn report(verbs: &[&str], throttled: u64, requests: u64) -> Report {
        let p = Profile::from_target(TargetSpec::from_url("memory://").unwrap());
        let mut s = Snapshot {
            elapsed_s: 10.0,
            ..Default::default()
        };
        s.summary.ops = 1000;
        s.summary.ops_per_s = 100.0;
        s.connections.requests = requests;
        s.resilience.throttled_responses = throttled;
        s.verbs = verbs
            .iter()
            .map(|v| VerbStats {
                verb: (*v).to_owned(),
                count: 500,
                latency: Quantiles {
                    p50: 10.0,
                    p99: 20.0,
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        let id = crate::model::RunId::new();
        crate::model::build_report(crate::model::ReportInput {
            run_id: &id,
            target_name: None,
            profile: &p,
            knobs: &p.knobs,
            snapshot: s,
            trace: Default::default(),
            cleanup: Default::default(),
            warnings: vec![],
            started_at: std::time::SystemTime::now(),
            ended_at: std::time::SystemTime::now(),
            spec: Some(crate::model::RunSpec::new(&p, "memory://")),
        })
    }

    fn verdict(d: &DiffReport, id: &str) -> Status {
        d.check(id)
            .unwrap_or_else(|| panic!("no check {id}: {:?}", d.checks))
            .verdict
    }

    #[test]
    fn a_p99_of_few_samples_is_not_gated() {
        // Two identical lab runs: 66 stats each, one straggler more lands in the candidate.
        let mut b = report(&["stat"], 0, 1000);
        let mut c = b.clone();
        b.verbs[0].count = 66;
        c.verbs[0].count = 66;
        b.verbs[0].latency.p99 = 17.2;
        c.verbs[0].latency.p99 = 262.4;
        let d = diff(&b, &c, &Thresholds::default());
        assert_eq!(verdict(&d, "latency_p99.stat"), Status::Skipped);
        assert_eq!(verdict(&d, "latency_p50.stat"), Status::Pass);
        assert_ne!(d.verdict, GateVerdict::Regression, "{:?}", d.checks);
        // With enough samples the same jump is a regression.
        b.verbs[0].count = 5000;
        c.verbs[0].count = 5000;
        let d = diff(&b, &c, &Thresholds::default());
        assert_eq!(verdict(&d, "latency_p99.stat"), Status::Fail);
    }

    #[test]
    fn checks_are_machine_grade() {
        let b = report(&["stat"], 0, 1000);
        let mut c = b.clone();
        c.summary.ops = 500;
        let d = diff(&b, &c, &Thresholds::default());
        let t = d.check("throughput").unwrap();
        assert_eq!(t.unit, "ops_per_s");
        assert!((t.delta_ratio.unwrap() + 0.5).abs() < 1e-9, "{t:?}");
        assert_eq!(t.limit_ratio, Some(0.10));
        assert_eq!(t.verdict, Status::Fail);
        assert_eq!(d.verdict, GateVerdict::Regression);
        assert_eq!(d.exit_code(), crate::errors::exit::GATE);
        let v = serde_json::to_value(t).unwrap();
        for k in [
            "id",
            "unit",
            "baseline",
            "candidate",
            "delta_ratio",
            "limit_ratio",
            "verdict",
        ] {
            assert!(v.get(k).is_some(), "{k} missing: {v}");
        }
        assert!(v.get("name").is_none());
        assert!(d.check("latency_p99.stat").is_some());
    }

    #[test]
    fn knob_changes_are_an_ab_comparison() {
        let b = report(&["stat"], 0, 1000);
        let mut c = b.clone();
        c.knobs.requested.concurrency_count = 64;
        let d = diff(&b, &c, &Thresholds::default());
        assert!(d.passed, "{}", d.render_terminal());
        assert!(d.comparable);
        assert_eq!(
            d.config_changes,
            vec![ConfigChange {
                kind: ChangeKind::Knobs,
                field: "knobs.requested.concurrency_count".into(),
                baseline: "16".into(),
                candidate: "64".into(),
            }]
        );
        assert!(
            d.warnings
                .iter()
                .any(|w| w.contains("knobs.requested.concurrency_count 16 → 64"))
        );
        assert!(d.render_terminal().contains("CONFIGURATION CHANGES"));
        // Host differs: warning only.
        let mut c = b.clone();
        c.run.host.cpus_count += 1;
        let d = diff(&b, &c, &Thresholds::default());
        assert!(d.passed);
        assert!(d.warnings.iter().any(|w| w.contains("host.cpus_count")));
    }

    #[test]
    fn different_workload_or_target_is_not_comparable() {
        let b = report(&["stat"], 0, 1000);
        let mut c = report(&["stat"], 0, 1000);
        c.spec.profile.workload.access = crate::config::Access::Zipf(1.1);
        c.summary.ops = 10; // would be a regression if it were comparable
        let d = diff(&b, &c, &Thresholds::default());
        assert!(!d.comparable);
        assert_eq!(d.verdict, GateVerdict::NotComparable);
        assert!(!d.passed);
        assert_eq!(d.exit_code(), crate::errors::exit::OK);
        assert!(!d.identity.same_workload && d.identity.same_target);
        assert!(!d.identity.reasons.is_empty());
        assert!(d.warnings.iter().any(|w| w.contains("not comparable")));
        assert!(!d.render_junit().contains("<failure"));
        let strict = Thresholds {
            strict: true,
            ..Thresholds::default()
        };
        assert_eq!(
            diff(&b, &c, &strict).exit_code(),
            crate::errors::exit::USAGE
        );
        let allow = Thresholds {
            allow_workload_change: true,
            ..Thresholds::default()
        };
        let d = diff(&b, &c, &allow);
        assert!(d.comparable && d.verdict == GateVerdict::Regression);

        let mut c = report(&["stat"], 0, 1000);
        c.spec.profile.target = TargetSpec::from_url("s3://other/x").unwrap();
        let d = diff(&b, &c, &Thresholds::default());
        assert!(!d.identity.same_target);
        assert_eq!(d.verdict, GateVerdict::NotComparable);
    }

    #[test]
    fn lab_targets_compare_by_scenario_not_ephemeral_port() {
        let mut b = report(&["stat"], 0, 1000);
        b.target.scheme = "s3".into();
        b.target.display = "lab://demo".into();
        b.target.endpoint = Some("http://127.0.0.1:43373".into());
        b.target
            .options
            .insert("endpoint".into(), "http://127.0.0.1:43373".into());
        let mut c = b.clone();
        c.target.endpoint = Some("http://127.0.0.1:39831".into());
        c.target
            .options
            .insert("endpoint".into(), "http://127.0.0.1:39831".into());
        let d = diff(&b, &c, &Thresholds::default());
        assert!(d.config_changes.is_empty(), "{:?}", d.config_changes);
        assert!(d.comparable);
    }

    #[test]
    fn missing_operation_fails_the_gate() {
        let b = report(&["read.full", "write"], 0, 1000);
        let c = report(&["read.full"], 0, 1000);
        let d = diff(&b, &c, &Thresholds::default());
        assert!(!d.passed);
        assert_eq!(verdict(&d, "present.write"), Status::Fail);
    }

    #[test]
    fn throttling_gates_on_rate_growth_with_floors() {
        let t = Thresholds::default();
        // baseline already throttled: 2% -> 4% fails
        let d = diff(
            &report(&["stat"], 20, 1000),
            &report(&["stat"], 40, 1000),
            &t,
        );
        assert_eq!(verdict(&d, "throttled_request_ratio"), Status::Fail);
        // 2% -> 2.05% is noise
        let d = diff(
            &report(&["stat"], 200, 10_000),
            &report(&["stat"], 205, 10_000),
            &t,
        );
        assert_eq!(verdict(&d, "throttled_request_ratio"), Status::Pass);
        // tiny absolute change below the ratio floor
        let d = diff(
            &report(&["stat"], 1, 100_000),
            &report(&["stat"], 5, 100_000),
            &t,
        );
        assert_eq!(verdict(&d, "throttled_request_ratio"), Status::Pass);
        // 0 -> 1 throttled response is not a regression (count floor) …
        let d = diff(&report(&["stat"], 0, 1000), &report(&["stat"], 1, 1000), &t);
        assert_eq!(verdict(&d, "throttled_request_ratio"), Status::Pass);
        assert!(d.passed, "{}", d.render_terminal());
        let d = diff(&report(&["stat"], 0, 1000), &report(&["stat"], 3, 1000), &t);
        assert_eq!(verdict(&d, "throttled_request_ratio"), Status::Pass);
        assert!(
            d.check("throttled_request_ratio")
                .unwrap()
                .note
                .contains("floor")
        );
        // … but real new throttling is.
        let d = diff(
            &report(&["stat"], 0, 1000),
            &report(&["stat"], 50, 1000),
            &t,
        );
        assert_eq!(verdict(&d, "throttled_request_ratio"), Status::Fail);
    }

    #[test]
    fn one_more_failed_op_is_below_the_count_floor() {
        let b = report(&["stat"], 0, 1000);
        let mut c = b.clone();
        c.summary.failed_ops = 1;
        c.summary.error_rate = 0.01;
        let d = diff(&b, &c, &Thresholds::default());
        assert_eq!(verdict(&d, "error_ratio"), Status::Pass);
        c.summary.failed_ops = 10;
        let d = diff(&b, &c, &Thresholds::default());
        assert_eq!(verdict(&d, "error_ratio"), Status::Fail);
    }

    #[test]
    fn mode_mismatch_warns_and_missed_growth_fails() {
        let b = report(&["stat"], 0, 1000);
        let mut c = report(&["stat"], 0, 1000);
        c.workload.rate_per_s = Some(100.0);
        let d = diff(&b, &c, &Thresholds::default());
        assert!(d.warnings.iter().any(|w| w.contains("closed-loop")));

        let mut b = report(&["stat"], 0, 1000);
        let mut c = b.clone();
        let ol = |missed| OpenLoopStats {
            target_rate: 100.0,
            achieved_rate: 100.0,
            missed,
            max_schedule_lag_ms: 0.0,
        };
        b.open_loop = Some(ol(0));
        c.open_loop = Some(ol(100));
        let d = diff(&b, &c, &Thresholds::default());
        assert_eq!(verdict(&d, "open_loop_missed_ratio"), Status::Fail);
    }

    #[test]
    fn long_json_values_are_summarized() {
        let mix = r#"[{"weight":80,"op":{"kind":"read","size":"1MiB"}},{"weight":20,"op":{"kind":"stat"}}]"#;
        let s = summarize(mix, 36);
        assert!(s.chars().count() <= 36, "{s}");
        assert!(s.starts_with("[2 items: read×80"), "{s}");
        assert_eq!(summarize("short", 36), "short");
        assert!(summarize(&"x".repeat(100), 10).ends_with('…'));
        let mut b = report(&["stat"], 0, 1000);
        let mut c = b.clone();
        b.workload.preset = mix.into();
        c.workload.preset = "small-files".into();
        let d = diff(&b, &c, &Thresholds::default());
        let width = d
            .render_terminal()
            .lines()
            .map(crate::report::style::visible_width)
            .max()
            .unwrap();
        assert!(width < 160, "table is {width} columns wide");
        assert!(wrap(&"word ".repeat(60), 40, 2).lines().count() > 3);
    }
}
