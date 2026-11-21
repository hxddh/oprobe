//! `oprobe compare`: compare a candidate run against a baseline and gate regressions.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::Report;
use super::style::*;
use crate::util::fmt_ms;

/// Regression limits. Percentages are relative changes in the "bad" direction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Thresholds {
    pub p99_pct: f64,
    pub p50_pct: f64,
    /// Latency changes smaller than this (ms) are never regressions (noise floor).
    pub min_latency_delta_ms: f64,
    pub throughput_drop_pct: f64,
    /// Max increase of the failed-operation rate in percentage points.
    pub error_rate_pp: f64,
    pub request_amp_pct: f64,
    pub read_amp_pct: f64,
    pub cost_pct: f64,
    /// Gate throttling: fail when the candidate is throttled and the baseline was not, or when
    /// its throttle rate (throttled responses per request) grew by more than
    /// [`THROTTLE_RATE_PCT`] and [`THROTTLE_RATE_FLOOR`].
    pub fail_on_new_throttling: bool,
    /// Skip latency checks for operations with fewer samples.
    #[serde(rename = "min_samples_count", alias = "min_samples")]
    pub min_samples: u64,
    /// Compare runs of different workloads without failing (otherwise a gate failure).
    #[serde(default)]
    pub allow_workload_change: bool,
    /// Compare runs against different targets without failing (otherwise a gate failure).
    #[serde(default)]
    pub allow_target_change: bool,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            p99_pct: 20.0,
            p50_pct: 20.0,
            min_latency_delta_ms: 1.0,
            throughput_drop_pct: 10.0,
            error_rate_pp: 0.5,
            request_amp_pct: 10.0,
            read_amp_pct: 10.0,
            cost_pct: 10.0,
            fail_on_new_throttling: true,
            min_samples: 30,
            allow_workload_change: false,
            allow_target_change: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Improved,
    Fail,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Check {
    pub name: String,
    pub baseline: f64,
    pub candidate: f64,
    /// Relative change in percent (positive = higher).
    pub change_pct: f64,
    pub limit: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffReport {
    pub passed: bool,
    pub baseline: String,
    pub candidate: String,
    pub thresholds: Thresholds,
    /// Every workload / target / knob / host setting that differs between the two runs.
    pub config_changes: Vec<ConfigChange>,
    pub checks: Vec<Check>,
    pub warnings: Vec<String>,
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

/// Relative throttle-rate increase (percent) that fails the gate when the baseline was throttled.
pub const THROTTLE_RATE_PCT: f64 = 25.0;
/// Throttle-rate changes below this (fraction of requests, 0.1 pp) are noise.
pub const THROTTLE_RATE_FLOOR: f64 = 0.001;
/// Max increase of the open-loop missed-arrival share, in percentage points.
pub const MISSED_PP: f64 = 1.0;

fn rel(b: f64, c: f64) -> f64 {
    if b.abs() < 1e-12 {
        if c.abs() < 1e-12 { 0.0 } else { 100.0 }
    } else {
        (c - b) / b * 100.0
    }
}

struct Builder {
    checks: Vec<Check>,
}

impl Builder {
    /// Higher-is-worse metric with relative limit and absolute noise floor.
    fn higher_worse(
        &mut self,
        name: String,
        b: f64,
        c: f64,
        limit_pct: f64,
        min_abs: f64,
        unit: &str,
    ) {
        let ch = rel(b, c);
        let status = if c - b > min_abs && ch > limit_pct {
            Status::Fail
        } else if b - c > min_abs && -ch > limit_pct {
            Status::Improved
        } else {
            Status::Pass
        };
        self.checks.push(Check {
            name,
            baseline: b,
            candidate: c,
            change_pct: ch,
            limit: format!("≤ +{limit_pct}%{unit}"),
            status,
            note: String::new(),
        });
    }

    fn lower_worse(&mut self, name: String, b: f64, c: f64, limit_pct: f64) {
        let ch = rel(b, c);
        let status = if -ch > limit_pct {
            Status::Fail
        } else if ch > limit_pct {
            Status::Improved
        } else {
            Status::Pass
        };
        self.checks.push(Check {
            name,
            baseline: b,
            candidate: c,
            change_pct: ch,
            limit: format!("≥ -{limit_pct}%"),
            status,
            note: String::new(),
        });
    }
}

fn throttle_rate(s: &Report) -> f64 {
    crate::util::ratio(
        s.resilience.throttled_responses as f64,
        s.connections.requests as f64,
    )
}

/// Share of open-loop arrivals that never started (percent).
fn missed_pct(s: &Report) -> Option<f64> {
    let ol = s.open_loop.as_ref()?;
    let issued = ol.achieved_rate * s.run.window_s;
    Some(crate::util::pct(
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
    let (bd, cd) = (base.run.window_s, cand.run.window_s);
    if bd > 0.0 && cd > 0.0 && (bd / cd > 2.0 || cd / bd > 2.0) {
        warnings.push(format!(
            "measurement windows differ a lot ({bd:.1}s vs {cd:.1}s): short windows are noisier and may miss slow effects"
        ));
    }
    let changes = config_changes(base, cand);
    let fields = |k: ChangeKind| {
        changes
            .iter()
            .filter(|c| c.kind == k)
            .map(|c| format!("{} {} → {}", c.field, c.baseline, c.candidate))
            .collect::<Vec<_>>()
    };
    let mut b = Builder { checks: Vec::new() };
    for (kind, allowed, flag) in [
        (
            ChangeKind::Workload,
            t.allow_workload_change,
            "--allow-workload-change",
        ),
        (
            ChangeKind::Target,
            t.allow_target_change,
            "--allow-target-change",
        ),
    ] {
        let diffs = fields(kind);
        b.checks.push(Check {
            name: format!("same {}", kind.label()),
            baseline: 0.0,
            candidate: diffs.len() as f64,
            change_pct: 0.0,
            limit: if allowed {
                format!("informational ({flag})")
            } else {
                "identical".into()
            },
            status: if diffs.is_empty() || allowed {
                Status::Pass
            } else {
                Status::Fail
            },
            note: diffs.join("; "),
        });
        if !diffs.is_empty() {
            warnings.push(if allowed {
                format!(
                    "the {} differs ({}): results may not be comparable",
                    kind.label(),
                    diffs.join("; ")
                )
            } else {
                format!(
                    "the {} differs ({}): the runs are not comparable, so the gate fails; pass {flag} to compare anyway",
                    kind.label(),
                    diffs.join("; ")
                )
            });
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
    let floor = format!(" & > {}ms", t.min_latency_delta_ms);

    b.lower_worse(
        "throughput ops/s".into(),
        bs.summary.ops_per_s,
        cs.summary.ops_per_s,
        t.throughput_drop_pct,
    );
    if bs.summary.mib_per_s > 0.01 {
        b.lower_worse(
            "throughput MiB/s".into(),
            bs.summary.mib_per_s,
            cs.summary.mib_per_s,
            t.throughput_drop_pct,
        );
    }
    for bv in &bs.verbs {
        let Some(cv) = cs.verbs.iter().find(|v| v.verb == bv.verb) else {
            b.checks.push(Check {
                name: format!("{} present", bv.verb),
                baseline: bv.count as f64,
                candidate: 0.0,
                change_pct: -100.0,
                limit: "operation must run".into(),
                status: Status::Fail,
                note: format!("operation `{}` is missing from the candidate", bv.verb),
            });
            continue;
        };
        if bv.count < t.min_samples || cv.count < t.min_samples {
            b.checks.push(Check {
                name: format!("{} p99", bv.verb),
                baseline: bv.latency.p99,
                candidate: cv.latency.p99,
                change_pct: rel(bv.latency.p99, cv.latency.p99),
                limit: format!("needs ≥ {} samples", t.min_samples),
                status: Status::Skipped,
                note: format!("{} / {} samples", bv.count, cv.count),
            });
            continue;
        }
        b.higher_worse(
            format!("{} p50 ms", bv.verb),
            bv.latency.p50,
            cv.latency.p50,
            t.p50_pct,
            t.min_latency_delta_ms,
            &floor,
        );
        b.higher_worse(
            format!("{} p99 ms", bv.verb),
            bv.latency.p99,
            cv.latency.p99,
            t.p99_pct,
            t.min_latency_delta_ms,
            &floor,
        );
    }
    // error rate (percentage points)
    let (be, ce) = (bs.summary.error_rate * 100.0, cs.summary.error_rate * 100.0);
    b.checks.push(Check {
        name: "failed ops %".into(),
        baseline: be,
        candidate: ce,
        change_pct: ce - be,
        limit: format!("≤ +{}pp", t.error_rate_pp),
        status: if ce - be > t.error_rate_pp {
            Status::Fail
        } else {
            Status::Pass
        },
        note: String::new(),
    });
    let (bt, ct) = (
        bs.resilience.throttled_responses,
        cs.resilience.throttled_responses,
    );
    let (btr, ctr) = (throttle_rate(bs) * 100.0, throttle_rate(cs) * 100.0);
    let new_throttling = bt == 0 && ct > 0;
    let more_throttling =
        bt > 0 && (ctr - btr) / 100.0 > THROTTLE_RATE_FLOOR && rel(btr, ctr) > THROTTLE_RATE_PCT;
    b.checks.push(Check {
        name: "throttled requests %".into(),
        baseline: btr,
        candidate: ctr,
        change_pct: rel(btr, ctr),
        limit: if t.fail_on_new_throttling {
            format!("no new throttling, ≤ +{THROTTLE_RATE_PCT}%")
        } else {
            "informational".into()
        },
        status: if t.fail_on_new_throttling && (new_throttling || more_throttling) {
            Status::Fail
        } else if bt > 0
            && (btr - ctr) / 100.0 > THROTTLE_RATE_FLOOR
            && -rel(btr, ctr) > THROTTLE_RATE_PCT
        {
            Status::Improved
        } else {
            Status::Pass
        },
        note: format!("{bt} → {ct} throttled responses"),
    });
    if let (Some(bm), Some(cm)) = (missed_pct(bs), missed_pct(cs)) {
        b.checks.push(Check {
            name: "open-loop missed %".into(),
            baseline: bm,
            candidate: cm,
            change_pct: cm - bm,
            limit: format!("≤ +{MISSED_PP}pp"),
            status: if cm - bm > MISSED_PP {
                Status::Fail
            } else {
                Status::Pass
            },
            note: String::new(),
        });
    }
    b.checks.push(Check {
        name: "integrity (corrupted)".into(),
        baseline: bs.integrity.corrupted as f64,
        candidate: cs.integrity.corrupted as f64,
        change_pct: 0.0,
        limit: "= 0".into(),
        status: if cs.integrity.corrupted > 0 {
            Status::Fail
        } else {
            Status::Pass
        },
        note: String::new(),
    });
    b.higher_worse(
        "requests per op".into(),
        bs.bytes.request_amplification,
        cs.bytes.request_amplification,
        t.request_amp_pct,
        0.01,
        "",
    );
    if bs.bytes.logical_read > 0 {
        b.higher_worse(
            "read amplification".into(),
            bs.bytes.read_amplification,
            cs.bytes.read_amplification,
            t.read_amp_pct,
            0.01,
            "",
        );
    }
    if base.cost.applicable && cand.cost.applicable {
        b.higher_worse(
            "cost per 1M ops USD".into(),
            base.cost.selected.per_million_ops_usd,
            cand.cost.selected.per_million_ops_usd,
            t.cost_pct,
            0.0001,
            "",
        );
    }
    let passed = !b.checks.iter().any(|c| c.status == Status::Fail);
    DiffReport {
        passed,
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
        config_changes: changes,
        checks: b.checks,
        warnings,
    }
}

fn fmt_val(name: &str, v: f64) -> String {
    if name.ends_with(" ms") {
        fmt_ms(v)
    } else if name.contains("USD") {
        format!("${v:.4}")
    } else if v.fract() == 0.0 && v.abs() < 1e12 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

impl DiffReport {
    pub fn render_terminal(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{} {}\n  {} {}\n  {} {}",
            if self.passed {
                badge("PASS", "42;30")
            } else {
                badge("REGRESSION", "41")
            },
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
                        c.baseline.clone(),
                        c.candidate.clone(),
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
            let _ = writeln!(out, "  {} {w}", yellow("warning:"));
        }
        let rows: Vec<Vec<String>> = self
            .checks
            .iter()
            .map(|c| {
                let st = match c.status {
                    Status::Pass => green("pass"),
                    Status::Improved => cyan("improved"),
                    Status::Fail => red("FAIL"),
                    Status::Skipped => dim("skipped"),
                };
                vec![
                    c.name.clone(),
                    fmt_val(&c.name, c.baseline),
                    fmt_val(&c.name, c.candidate),
                    format!("{:+.1}%", c.change_pct),
                    c.limit.clone(),
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
            "## {} oprobe storage gate\n\nBaseline: {}  \nCandidate: {}\n",
            if self.passed { "✅" } else { "❌" },
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
                    c.baseline,
                    c.candidate
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
            let st = match c.status {
                Status::Pass => "✅ pass",
                Status::Improved => "🚀 improved",
                Status::Fail => "❌ **FAIL**",
                Status::Skipped => "⏭ skipped",
            };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {:+.1}% | {} | {} |",
                c.name,
                fmt_val(&c.name, c.baseline),
                fmt_val(&c.name, c.candidate),
                c.change_pct,
                c.limit,
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
        let failures = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count();
        let skipped = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Skipped)
            .count();
        let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(
            x,
            "<testsuite name=\"oprobe compare\" tests=\"{}\" failures=\"{failures}\" skipped=\"{skipped}\">",
            self.checks.len()
        );
        for c in &self.checks {
            let _ = write!(
                x,
                "  <testcase classname=\"oprobe.gate\" name=\"{}\">",
                esc(&c.name)
            );
            match c.status {
                Status::Fail if c.name.starts_with("same ") => {
                    let _ = write!(
                        x,
                        "<failure message=\"{}\">{}</failure>",
                        esc(&format!("{}: differs", c.name)),
                        esc(&c.note)
                    );
                }
                Status::Fail => {
                    let _ = write!(
                        x,
                        "<failure message=\"{}\">baseline={} candidate={} change={:+.2}% limit={}</failure>",
                        esc(&format!("{} regressed {:+.1}%", c.name, c.change_pct)),
                        c.baseline,
                        c.candidate,
                        c.change_pct,
                        esc(&c.limit)
                    );
                }
                Status::Skipped => {
                    let _ = write!(x, "<skipped message=\"{}\"/>", esc(&c.note));
                }
                _ => {}
            }
            let _ = writeln!(x, "</testcase>");
        }
        x.push_str("</testsuite>\n");
        x
    }
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
            spec: None,
        })
    }

    fn status(d: &DiffReport, name: &str) -> Status {
        d.checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}: {:?}", d.checks))
            .status
    }

    #[test]
    fn configuration_changes_are_reported_and_gate_workload_and_target() {
        let b = report(&["stat"], 0, 1000);
        // Knobs differ: legitimate A/B, passes with a prominent warning.
        let mut c = b.clone();
        c.knobs.requested.concurrency_count = 64;
        let d = diff(&b, &c, &Thresholds::default());
        assert!(d.passed, "{}", d.render_terminal());
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
        assert!(
            d.render_markdown()
                .contains("`knobs.requested.concurrency_count`")
        );

        // Workload differs: fails unless allowed.
        let mut c = b.clone();
        c.workload.access = "zipf:1.1".into();
        c.workload.duration_s = 99.0;
        let d = diff(&b, &c, &Thresholds::default());
        assert!(!d.passed);
        assert_eq!(status(&d, "same workload"), Status::Fail);
        assert_eq!(
            d.config_changes.len(),
            1,
            "duration is not a workload change"
        );
        assert!(d.render_junit().contains("same workload: differs"));
        let allow = Thresholds {
            allow_workload_change: true,
            ..Thresholds::default()
        };
        assert!(diff(&b, &c, &allow).passed);

        // Target differs: fails unless allowed.
        let mut c = b.clone();
        c.target.display = "s3://other".into();
        let d = diff(&b, &c, &Thresholds::default());
        assert_eq!(status(&d, "same target"), Status::Fail);
        let allow = Thresholds {
            allow_target_change: true,
            ..Thresholds::default()
        };
        assert!(diff(&b, &c, &allow).passed);

        // Host differs: warning only.
        let mut c = b.clone();
        c.run.host.cpus_count += 1;
        let d = diff(&b, &c, &Thresholds::default());
        assert!(d.passed);
        assert!(
            d.warnings.iter().any(|w| w.contains("host.cpus_count")),
            "{:?}",
            d.warnings
        );
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
        assert_eq!(status(&d, "same target"), Status::Pass);
        // `oprobe demo` names its lab target.
        let (mut b2, mut c2) = (b.clone(), c.clone());
        for r in [&mut b2, &mut c2] {
            r.target.display = "s3://demo".into();
            r.target.name = Some("lab://demo".into());
        }
        assert!(
            diff(&b2, &c2, &Thresholds::default())
                .config_changes
                .is_empty()
        );
        // A different scenario is still a different target.
        c.target.display = "lab://throttle".into();
        let d = diff(&b, &c, &Thresholds::default());
        assert_eq!(status(&d, "same target"), Status::Fail);
        // So is a different endpoint of a real target.
        let (mut b3, mut c3) = (b.clone(), b.clone());
        b3.target.display = "s3://bkt".into();
        c3.target.display = "s3://bkt".into();
        c3.target.endpoint = Some("http://127.0.0.1:1".into());
        assert_eq!(
            status(&diff(&b3, &c3, &Thresholds::default()), "same target"),
            Status::Fail
        );
    }

    #[test]
    fn missing_operation_fails_the_gate() {
        let b = report(&["read.full", "write"], 0, 1000);
        let c = report(&["read.full"], 0, 1000);
        let d = diff(&b, &c, &Thresholds::default());
        assert!(!d.passed);
        assert_eq!(status(&d, "write present"), Status::Fail);
    }

    #[test]
    fn throttling_gates_on_rate_growth_with_noise_floor() {
        let t = Thresholds::default();
        // baseline already throttled: 2% -> 4% fails
        let d = diff(
            &report(&["stat"], 20, 1000),
            &report(&["stat"], 40, 1000),
            &t,
        );
        assert_eq!(status(&d, "throttled requests %"), Status::Fail);
        // 2% -> 2.05% is noise
        let d = diff(
            &report(&["stat"], 200, 10_000),
            &report(&["stat"], 205, 10_000),
            &t,
        );
        assert_eq!(status(&d, "throttled requests %"), Status::Pass);
        // tiny absolute change below the floor even if relatively large
        let d = diff(
            &report(&["stat"], 1, 100_000),
            &report(&["stat"], 5, 100_000),
            &t,
        );
        assert_eq!(status(&d, "throttled requests %"), Status::Pass);
        // new throttling still fails
        let d = diff(&report(&["stat"], 0, 1000), &report(&["stat"], 1, 1000), &t);
        assert_eq!(status(&d, "throttled requests %"), Status::Fail);
    }

    #[test]
    fn mode_and_duration_mismatches_warn_and_missed_growth_fails() {
        let b = report(&["stat"], 0, 1000);
        let mut c = report(&["stat"], 0, 1000);
        c.workload.rate_per_s = Some(100.0);
        c.run.window_s = 60.0;
        let d = diff(&b, &c, &Thresholds::default());
        assert!(
            d.warnings.iter().any(|w| w.contains("closed-loop")),
            "{:?}",
            d.warnings
        );
        assert!(
            d.warnings.iter().any(|w| w.contains("windows differ")),
            "{:?}",
            d.warnings
        );

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
        assert_eq!(status(&d, "open-loop missed %"), Status::Fail);
    }
}
