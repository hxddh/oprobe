//! Rendering of the regression gate (`oprobe compare`): text and Markdown tables. The gate
//! itself — identity, checks, verdicts — is [`crate::analysis::gate`] (re-exported here).

use std::fmt::Write as _;

pub use crate::analysis::gate::*;

#[cfg(test)]
use super::Report;
use super::style::*;
#[allow(unused_imports)]
use crate::util::fmt_ms;

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
            ..Default::default()
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
