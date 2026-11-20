//! Repeat-aware run comparison (`oprobe compare`, `run --baseline`): the per-check gate of
//! [`crate::report::diff`] on representative runs, made statistical.
//!
//! - Either side may hold several runs (a `--repeat` group or a named baseline). Latency and
//!   throughput are then compared as medians across runs with bootstrap confidence intervals.
//! - With one run per side the operation rows are bootstrapped instead (latency), and the
//!   per-second throughput series (throughput).
//! - A latency or throughput check fails only when the change exceeds its limit *and* the 95 %
//!   interval of the change excludes zero; otherwise it passes with a "not significant" note.
//! - Findings are diffed: new, resolved and persisting codes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::stats::{self, Interval};
use crate::model::{Report, Run};
use crate::report::diff::{DiffReport, Status, Thresholds, diff};

/// One side of a comparison.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Side {
    /// What the user named (RUNREF, baseline name, `repeat group`).
    pub label: String,
    pub run_ids: Vec<String>,
    /// The run whose report the per-check table compares (the median-p99 run).
    pub representative: String,
}

/// A headline metric with confidence intervals.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MetricStat {
    /// `p50_ms`, `p99_ms`, `ops_per_s`.
    pub metric: String,
    pub baseline: Interval,
    pub candidate: Interval,
    /// Relative change of the candidate (fraction; +0.1 = 10 % higher).
    pub change: Interval,
    /// The 95 % interval of the change excludes zero.
    pub significant: bool,
    /// `runs` (medians across runs) or `samples` (operations / seconds of one run each).
    pub method: String,
}

/// A finding code on one side only, or on both.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FindingRef {
    pub code: String,
    pub severity: super::diagnosis::Severity,
    pub cause: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FindingsDiff {
    pub new: Vec<FindingRef>,
    pub resolved: Vec<FindingRef>,
    pub persisting: Vec<FindingRef>,
}

/// Result of `oprobe compare`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Comparison {
    pub passed: bool,
    pub baseline: Side,
    pub candidate: Side,
    pub stats: Vec<MetricStat>,
    pub findings: FindingsDiff,
    /// p99 (ms) of every baseline run then every candidate run, in time order.
    pub p99_trend_ms: Vec<f64>,
    /// The per-check gate (configuration changes, per-operation latency, errors, throttling,
    /// amplification, cost), with significance applied to latency and throughput checks.
    pub gate: DiffReport,
}

fn median_run(runs: &[Run]) -> &Run {
    let mut idx: Vec<usize> = (0..runs.len()).collect();
    idx.sort_by(|a, b| {
        runs[*a]
            .report
            .summary
            .latency
            .p99
            .total_cmp(&runs[*b].report.summary.latency.p99)
    });
    &runs[idx[idx.len() / 2]]
}

fn op_lat_ms(run: &Run, verb: Option<&str>) -> Vec<f64> {
    run.ops
        .iter()
        .filter(|o| o.ok && verb.is_none_or(|v| o.verb == v))
        .map(|o| o.latency_us as f64 / 1000.0)
        .collect()
}

fn per_second_ops(run: &Run) -> Vec<f64> {
    super::segments::segments(&run.ops, run.report.run.window_s)
        .per_second
        .iter()
        .map(|s| s.ops_count as f64)
        .collect()
}

/// Per-run values of `f`, or `None` when it is unavailable.
fn per_run(runs: &[Run], f: impl Fn(&Report) -> Option<f64>) -> Option<Vec<f64>> {
    runs.iter().map(|r| f(&r.report)).collect()
}

fn verb_q(r: &Report, verb: &str, p99: bool) -> Option<f64> {
    r.verbs
        .iter()
        .find(|v| v.verb == verb)
        .map(|v| if p99 { v.latency.p99 } else { v.latency.p50 })
}

/// Change interval for a latency quantile (`q`) of `verb` (all operations when `None`).
fn latency_change(
    base: &[Run],
    cand: &[Run],
    verb: Option<&str>,
    q: f64,
) -> Option<(Interval, Interval, Interval, &'static str)> {
    let seed = (q * 1000.0) as u64 ^ verb.map_or(0, |v| crate::model::ids::fnv1a(v.as_bytes()));
    if base.len() > 1 || cand.len() > 1 {
        let get = |r: &Report| match verb {
            Some(v) => verb_q(r, v, q > 0.9),
            None => Some(if q > 0.9 {
                r.summary.latency.p99
            } else {
                r.summary.latency.p50
            }),
        };
        let (a, b) = (per_run(base, get)?, per_run(cand, get)?);
        return Some((
            stats::bootstrap(&a, stats::med, seed),
            stats::bootstrap(&b, stats::med, seed + 1),
            stats::bootstrap_rel_change(&a, &b, stats::med, seed + 2),
            "runs",
        ));
    }
    let (a, b) = (op_lat_ms(&base[0], verb), op_lat_ms(&cand[0], verb));
    if a.len() < 10 || b.len() < 10 {
        return None;
    }
    Some((
        stats::bootstrap(&a, stats::p(q), seed),
        stats::bootstrap(&b, stats::p(q), seed + 1),
        stats::bootstrap_rel_change(&a, &b, stats::p(q), seed + 2),
        "samples",
    ))
}

fn throughput_change(
    base: &[Run],
    cand: &[Run],
) -> Option<(Interval, Interval, Interval, &'static str)> {
    if base.len() > 1 || cand.len() > 1 {
        let get = |r: &Report| Some(r.summary.ops_per_s);
        let (a, b) = (per_run(base, get)?, per_run(cand, get)?);
        return Some((
            stats::bootstrap(&a, stats::med, 11),
            stats::bootstrap(&b, stats::med, 12),
            stats::bootstrap_rel_change(&a, &b, stats::med, 13),
            "runs",
        ));
    }
    let (a, b) = (per_second_ops(&base[0]), per_second_ops(&cand[0]));
    if a.len() < 3 || b.len() < 3 {
        return None;
    }
    let mean = |v: &mut [f64]| stats::mean(v);
    Some((
        stats::bootstrap(&a, mean, 11),
        stats::bootstrap(&b, mean, 12),
        stats::bootstrap_rel_change(&a, &b, mean, 13),
        "samples",
    ))
}

fn majority_codes(runs: &[Run]) -> BTreeMap<String, FindingRef> {
    let mut n: BTreeMap<String, (usize, FindingRef)> = BTreeMap::new();
    for r in runs {
        for f in &r.report.findings {
            let e = n.entry(f.code.clone()).or_insert_with(|| {
                (
                    0,
                    FindingRef {
                        code: f.code.clone(),
                        severity: f.severity,
                        cause: f.cause.clone(),
                    },
                )
            });
            e.0 += 1;
        }
    }
    n.into_iter()
        .filter(|(_, (k, _))| *k * 2 > runs.len())
        .map(|(c, (_, f))| (c, f))
        .collect()
}

fn findings_diff(base: &[Run], cand: &[Run]) -> FindingsDiff {
    let (b, c) = (majority_codes(base), majority_codes(cand));
    FindingsDiff {
        new: c
            .iter()
            .filter(|(k, _)| !b.contains_key(*k))
            .map(|(_, f)| f.clone())
            .collect(),
        resolved: b
            .iter()
            .filter(|(k, _)| !c.contains_key(*k))
            .map(|(_, f)| f.clone())
            .collect(),
        persisting: c
            .iter()
            .filter(|(k, _)| b.contains_key(*k))
            .map(|(_, f)| f.clone())
            .collect(),
    }
}

fn fmt_ci(i: &Interval) -> String {
    format!(
        "{:+.1}% [{:+.1}%, {:+.1}%]",
        i.estimate * 100.0,
        i.low * 100.0,
        i.high * 100.0
    )
}

/// Compare `cand` runs against `base` runs (each non-empty).
pub fn compare(
    base: &[Run],
    cand: &[Run],
    base_label: &str,
    cand_label: &str,
    t: &Thresholds,
) -> Comparison {
    assert!(
        !base.is_empty() && !cand.is_empty(),
        "compare needs runs on both sides"
    );
    let (rb, rc) = (median_run(base), median_run(cand));
    let mut gate = diff(&rb.report, &rc.report, t);
    let repeated = base.len() > 1 || cand.len() > 1;
    // Significance for latency and throughput checks.
    for chk in gate.checks.iter_mut() {
        let interval = if let Some(verb) = chk.name.strip_suffix(" p99 ms") {
            latency_change(base, cand, Some(verb), 0.99)
        } else if let Some(verb) = chk.name.strip_suffix(" p50 ms") {
            latency_change(base, cand, Some(verb), 0.50)
        } else if chk.name == "throughput ops/s" {
            throughput_change(base, cand)
        } else {
            continue;
        };
        let Some((b, c, change, method)) = interval else {
            continue;
        };
        if repeated {
            chk.baseline = b.estimate;
            chk.candidate = c.estimate;
            chk.change_pct = change.estimate * 100.0;
        }
        let ci = fmt_ci(&change);
        if chk.status == Status::Fail && !change.excludes_zero() {
            chk.status = Status::Pass;
            chk.note = format!("not significant: 95% CI of the change {ci} ({method})");
        } else if chk.status == Status::Fail || chk.status == Status::Improved {
            chk.note = format!("95% CI {ci} ({method})");
        }
    }
    gate.passed = !gate.checks.iter().any(|c| c.status == Status::Fail);
    let mut stats_out = Vec::new();
    for (metric, iv) in [
        ("p50_ms", latency_change(base, cand, None, 0.50)),
        ("p99_ms", latency_change(base, cand, None, 0.99)),
        ("ops_per_s", throughput_change(base, cand)),
    ] {
        if let Some((b, c, change, method)) = iv {
            stats_out.push(MetricStat {
                metric: metric.into(),
                baseline: b,
                candidate: c,
                significant: change.excludes_zero(),
                change,
                method: method.into(),
            });
        }
    }
    let mut trend: Vec<(&str, f64)> = base
        .iter()
        .chain(cand.iter())
        .map(|r| {
            (
                r.report.run.started_at.as_str(),
                r.report.summary.latency.p99,
            )
        })
        .collect();
    trend.sort_by(|a, b| a.0.cmp(b.0));
    let side = |label: &str, runs: &[Run], rep: &Run| Side {
        label: label.to_owned(),
        run_ids: runs.iter().map(|r| r.id().to_owned()).collect(),
        representative: rep.id().to_owned(),
    };
    Comparison {
        passed: gate.passed,
        baseline: side(base_label, base, rb),
        candidate: side(cand_label, cand, rc),
        stats: stats_out,
        findings: findings_diff(base, cand),
        p99_trend_ms: trend.into_iter().map(|x| x.1).collect(),
        gate,
    }
}

impl Comparison {
    fn stat_rows(&self) -> Vec<Vec<String>> {
        self.stats
            .iter()
            .map(|s| {
                let v = |i: &Interval| {
                    if (i.high - i.low).abs() < 1e-9 {
                        format!("{:.2}", i.estimate)
                    } else {
                        format!("{:.2} [{:.2}–{:.2}]", i.estimate, i.low, i.high)
                    }
                };
                vec![
                    s.metric.clone(),
                    v(&s.baseline),
                    v(&s.candidate),
                    fmt_ci(&s.change),
                    if s.significant {
                        "yes".into()
                    } else {
                        "no".into()
                    },
                ]
            })
            .collect()
    }

    pub fn render_terminal(&self) -> String {
        use crate::report::style::*;
        let mut o = self.gate.render_terminal();
        let _ = writeln!(
            o,
            "\n{} baseline {} run(s) · candidate {} run(s) · p99 trend {}",
            bold(&cyan("STATISTICS")),
            self.baseline.run_ids.len(),
            self.candidate.run_ids.len(),
            stats::sparkline(&self.p99_trend_ms, 24)
        );
        o += &table(
            &[
                "metric",
                "baseline",
                "candidate",
                "change (95% CI)",
                "significant",
            ],
            &self.stat_rows(),
            2,
        );
        let f = &self.findings;
        for (label, list) in [("new", &f.new), ("resolved", &f.resolved)] {
            for x in list {
                let _ = writeln!(
                    o,
                    "  {} {} — {}",
                    if label == "new" {
                        red("new finding")
                    } else {
                        green("resolved")
                    },
                    x.code,
                    x.cause
                );
            }
        }
        o
    }

    /// Markdown for a sticky PR comment (starts with a stable HTML marker to find and update).
    pub fn render_markdown(&self) -> String {
        let mut md = String::from("<!-- oprobe-compare -->\n");
        md += &self.gate.render_markdown();
        let _ = writeln!(
            md,
            "\n**Statistics** — baseline {} run(s), candidate {} run(s) · p99 trend `{}`\n",
            self.baseline.run_ids.len(),
            self.candidate.run_ids.len(),
            stats::sparkline(&self.p99_trend_ms, 24)
        );
        let _ = writeln!(
            md,
            "| metric | baseline | candidate | change (95% CI) | significant |\n|---|---:|---:|---:|---|"
        );
        for r in self.stat_rows() {
            let _ = writeln!(md, "| {} |", r.join(" | "));
        }
        let f = &self.findings;
        if !f.new.is_empty() || !f.resolved.is_empty() {
            md += "\n**Findings**\n\n";
            for x in &f.new {
                let _ = writeln!(md, "- 🆕 `{}` ({}) {}", x.code, x.severity.label(), x.cause);
            }
            for x in &f.resolved {
                let _ = writeln!(md, "- ✅ resolved `{}` {}", x.code, x.cause);
            }
        }
        md
    }

    pub fn render_junit(&self) -> String {
        let mut x = self.gate.render_junit();
        // One test case per new finding of warning severity or worse (informational ones pass).
        let extra: String = self
            .findings
            .new
            .iter()
            .map(|f| {
                let esc = f.cause.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
                if f.severity >= super::diagnosis::Severity::Warning {
                    format!("  <testcase classname=\"oprobe.findings\" name=\"new finding {}\"><failure message=\"{esc}\"/></testcase>\n", f.code)
                } else {
                    format!("  <testcase classname=\"oprobe.findings\" name=\"new finding {}\"/>\n", f.code)
                }
            })
            .collect();
        if let Some(i) = x.rfind("</testsuite>") {
            x.insert_str(i, &extra);
        }
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;
    use crate::trace::{Quantiles, VerbStats};

    fn run(id: &str, scale: f64, ops_per_s: f64) -> Run {
        let mut r = straggler_run();
        r.report.run.id = id.into();
        r.report.run.started_at = format!("2026-11-20T00:00:{id}Z");
        r.report.run.window_s = 5.0;
        for o in &mut r.ops {
            o.latency_us = (o.latency_us as f64 * scale) as u64;
            o.verb = "read.range".into();
        }
        let mut lat: Vec<f64> = r.ops.iter().map(|o| o.latency_us as f64 / 1000.0).collect();
        let q = Quantiles {
            count: 1000,
            p50: stats::quantile_mut(&mut lat, 0.5),
            p99: stats::quantile_mut(&mut lat, 0.99),
            ..Default::default()
        };
        r.report.summary.latency = q;
        r.report.summary.ops_per_s = ops_per_s;
        r.report.verbs = vec![VerbStats {
            verb: "read.range".into(),
            count: 1000,
            latency: q,
            ops_per_s,
            ..Default::default()
        }];
        r
    }

    #[test]
    fn a_real_regression_fails_and_noise_passes() {
        let t = Thresholds::default();
        let base = vec![run("01", 1.0, 200.0)];
        let worse = vec![run("02", 1.6, 200.0)];
        let c = compare(&base, &worse, "a", "b", &t);
        assert!(!c.passed, "{:#?}", c.gate.checks);
        let p50 = c.stats.iter().find(|s| s.metric == "p50_ms").unwrap();
        assert!(p50.significant && p50.change.estimate > 0.5);
        // Repeats: per-run medians; a 25 % p99 change in one noisy run of five is not significant.
        let bs: Vec<Run> = (0..5)
            .map(|i| run(&format!("1{i}"), 1.0 + i as f64 * 0.05, 200.0))
            .collect();
        let cs: Vec<Run> = (0..5)
            .map(|i| {
                run(
                    &format!("2{i}"),
                    if i == 0 { 1.25 } else { 1.0 + i as f64 * 0.05 },
                    200.0,
                )
            })
            .collect();
        let c = compare(&bs, &cs, "base", "cand", &t);
        assert!(c.passed, "{:#?}", c.gate.checks);
        assert_eq!(c.stats[0].method, "runs");
        assert_eq!(c.p99_trend_ms.len(), 10);
        let md = c.render_markdown();
        assert!(md.starts_with("<!-- oprobe-compare -->"));
        assert!(md.contains("p99 trend"));
        assert!(c.render_junit().contains("</testsuite>"));
    }

    #[test]
    fn findings_are_diffed() {
        let mut a = run("01", 1.0, 200.0);
        let mut b = run("02", 1.0, 200.0);
        a.report.findings = vec![crate::model::Finding {
            code: "throttling".into(),
            ..Default::default()
        }];
        b.report.findings = vec![crate::model::Finding {
            code: "cold_connections".into(),
            severity: crate::analysis::diagnosis::Severity::Warning,
            ..Default::default()
        }];
        let c = compare(&[a], &[b], "a", "b", &Thresholds::default());
        assert_eq!(c.findings.new[0].code, "cold_connections");
        assert_eq!(c.findings.resolved[0].code, "throttling");
        assert!(c.render_junit().contains("new finding cold_connections"));
    }
}
