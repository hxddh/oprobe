//! `oprobe diff`: compare a candidate report against a baseline and gate regressions.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::Report;
use super::style::*;
use crate::util::fmt_ms;

/// Regression limits. Percentages are relative changes in the "bad" direction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Fail when the candidate is throttled and the baseline was not.
    pub fail_on_new_throttling: bool,
    /// Skip latency checks for operations with fewer samples.
    pub min_samples: u64,
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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Improved,
    Fail,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffReport {
    pub passed: bool,
    pub baseline: String,
    pub candidate: String,
    pub thresholds: Thresholds,
    pub checks: Vec<Check>,
    pub warnings: Vec<String>,
}

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

pub fn diff(base: &Report, cand: &Report, t: &Thresholds) -> DiffReport {
    let mut warnings = Vec::new();
    if base.target.display != cand.target.display {
        warnings.push(format!(
            "targets differ: `{}` vs `{}`",
            base.target.display, cand.target.display
        ));
    }
    if base.workload.preset != cand.workload.preset || base.workload.mix != cand.workload.mix {
        warnings.push("workloads differ; comparisons may not be meaningful".into());
    }
    let (bs, cs) = (&base.result, &cand.result);
    let mut b = Builder { checks: Vec::new() };
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
        let Some(cv) = cs.verb(&bv.verb) else {
            warnings.push(format!("operation `{}` missing from candidate", bv.verb));
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
    b.checks.push(Check {
        name: "throttled responses".into(),
        baseline: bt as f64,
        candidate: ct as f64,
        change_pct: rel(bt as f64, ct as f64),
        limit: if t.fail_on_new_throttling {
            "no new throttling".into()
        } else {
            "informational".into()
        },
        status: if t.fail_on_new_throttling && bt == 0 && ct > 0 {
            Status::Fail
        } else {
            Status::Pass
        },
        note: String::new(),
    });
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
    b.higher_worse(
        "cost per 1M ops USD".into(),
        base.cost.selected.per_million_ops_usd,
        cand.cost.selected.per_million_ops_usd,
        t.cost_pct,
        0.0001,
        "",
    );
    let passed = !b.checks.iter().any(|c| c.status == Status::Fail);
    DiffReport {
        passed,
        baseline: format!("{} ({})", base.target.display, base.generated_at),
        candidate: format!("{} ({})", cand.target.display, cand.generated_at),
        thresholds: t.clone(),
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
            bold("oprobe diff"),
            dim("baseline "),
            self.baseline,
            dim("candidate"),
            self.candidate
        );
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
            "<testsuite name=\"oprobe diff\" tests=\"{}\" failures=\"{failures}\" skipped=\"{skipped}\">",
            self.checks.len()
        );
        for c in &self.checks {
            let _ = write!(
                x,
                "  <testcase classname=\"oprobe.gate\" name=\"{}\">",
                esc(&c.name)
            );
            match c.status {
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
