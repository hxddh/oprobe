//! The concise text result of `oprobe run` (design §9 "Text output default"): at most 20
//! lines that fit 80 columns — verdict, top findings (cause · evidence · advice · next
//! command), key numbers, and where the full report is.
//!
//! Layout: 1 verdict line, up to 3 findings × (cause, evidence, advice, next command), a line
//! for more findings, 2–4 lines of numbers and SLA breaches, the pointer to the full report.

use super::Report;
use super::style::*;
use crate::util::{fmt_ms, fmt_usd};

const WIDTH: usize = 80;

/// Cut plain text to `max` characters (with an ellipsis).
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Render the brief result of a run saved as `run_id`.
pub fn render(r: &Report, run_id: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let badge = super::terminal::verdict_badge(r.verdict.level);
    let badge_w = visible_width(&badge) + 1;
    lines.push(format!(
        "{badge} {}",
        bold(&cut(&r.verdict.headline, WIDTH - badge_w))
    ));
    if r.run.interrupted {
        lines.push(yellow("  partial: the run was stopped early"));
    }
    let mut findings: Vec<_> = r.findings.iter().collect();
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    for f in findings.iter().take(3) {
        lines.push(format!(
            "  {} {}",
            super::terminal::severity_icon(f.severity),
            cut(&f.cause, WIDTH - 4)
        ));
        if !f.evidence.text.is_empty() {
            lines.push(format!("    {}", dim(&cut(&f.evidence.text, WIDTH - 4))));
        }
        lines.push(format!("    → {}", cut(&f.advice, WIDTH - 6)));
        if let Some(c) = f.verification.as_ref().and_then(|v| v.command.as_ref()) {
            lines.push(format!("    {} {}", dim("next:"), cut(c, WIDTH - 10)));
        }
    }
    if r.findings.len() > 3 {
        lines.push(dim(&format!(
            "  … {} more findings: oprobe explain {run_id}",
            r.findings.len() - 3
        )));
    }
    let s = &r.summary;
    lines.push(format!(
        "  {:.1} ops/s · {:.1} MiB/s · p50 {} · p99 {} · errors {:.2}%",
        s.ops_per_s,
        s.mib_per_s,
        fmt_ms(s.latency.p50),
        fmt_ms(s.latency.p99),
        s.error_rate * 100.0
    ));
    let mut extra = Vec::new();
    if r.resilience.throttled_responses > 0 {
        extra.push(format!("{} throttled", r.resilience.throttled_responses));
    }
    if r.cost.applicable {
        extra.push(format!("{} /month", fmt_usd(r.cost.selected.total_usd)));
    }
    let v = &r.verdict.sla;
    extra.push(if v.passed {
        "SLA met".to_owned()
    } else if v.enforced {
        "SLA BREACHED".to_owned()
    } else {
        "SLA breached (not enforced)".to_owned()
    });
    lines.push(format!("  {}", cut(&extra.join(" · "), WIDTH - 2)));
    if !v.passed {
        for b in v.breaches.iter().take(2) {
            lines.push(format!("    {}", red(&cut(b, WIDTH - 4))));
        }
    }
    lines.truncate(19);
    lines.push(dim(&format!("full report: oprobe runs show {run_id}")));
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Finding;

    #[test]
    fn brief_fits_20_lines_of_80_columns() {
        set(false);
        let mut r = crate::model::Report {
            schema: crate::model::REPORT_SCHEMA.into(),
            run: Default::default(),
            target: Default::default(),
            workload: Default::default(),
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
        r.verdict.headline = "x".repeat(200);
        r.verdict.sla.passed = false;
        r.verdict.sla.breaches = vec!["y".repeat(300); 5];
        for _ in 0..10 {
            r.findings.push(Finding {
                cause: "c".repeat(120),
                advice: "a".repeat(120),
                verification: Some(crate::model::Verification {
                    description: "d".into(),
                    command: Some("oprobe run x ".repeat(20)),
                }),
                ..Default::default()
            });
        }
        let out = render(&r, "20261120-143005-a1b2");
        assert!(out.lines().count() <= 20, "{out}");
        for l in out.lines() {
            assert!(visible_width(l) <= 80, "{} > 80: {l}", visible_width(l));
        }
        assert!(out.contains("oprobe runs show 20261120-143005-a1b2"));
    }
}
