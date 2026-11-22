//! The concise text result of `oprobe run` (design §9 "Text output default", v5 §6 "Brief
//! output"): verdict, top findings (cause · evidence · advice · next command), key numbers,
//! and where the full report is.
//!
//! The brief wraps to the terminal width (or 100 columns off a terminal, `COLUMNS` when set;
//! [`style::term_width`]) and never truncates: prose wraps at words, and `next` commands are
//! printed whole on their own line so they can be copied. Labels, finding causes and advice
//! follow `--lang` ([`crate::i18n`]); numbers and commands do not change.

use super::Report;
use super::style::*;
use crate::i18n::{self, t, tf};
use crate::util::{fmt_ms, fmt_usd};

/// Render the brief result of a run saved as `run_id`, for this terminal.
pub fn render(r: &Report, run_id: &str) -> String {
    render_width(r, run_id, term_width())
}

/// [`render`] for `width` columns.
pub fn render_width(r: &Report, run_id: &str, width: usize) -> String {
    let width = width.max(40);
    let mut lines: Vec<String> = Vec::new();
    let prose = |lines: &mut Vec<String>,
                 text: &str,
                 first: &str,
                 rest: &str,
                 paint: fn(&str) -> String| {
        for (i, l) in wrap_text(text, width, first, rest).into_iter().enumerate() {
            let indent = if i == 0 { first } else { rest };
            let body = l.strip_prefix(indent).unwrap_or(&l);
            lines.push(format!("{indent}{}", paint(body)));
        }
    };
    let badge = super::terminal::verdict_badge(r.verdict.level);
    let badge_pad = " ".repeat(visible_width(&badge) + 1);
    for (i, l) in wrap_text(&r.verdict.headline, width, &badge_pad, "  ")
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            lines.push(format!("{badge} {}", bold(l.trim_start())));
        } else {
            lines.push(format!("  {}", bold(l.trim_start())));
        }
    }
    if r.run.interrupted {
        lines.push(yellow(&format!("  {}", t("brief.partial"))));
    }
    let mut findings: Vec<_> = r.findings.iter().collect();
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    for f in findings.iter().take(3) {
        let (cause, advice) = match i18n::finding(&f.code) {
            Some((c, a)) => (c.to_owned(), a.to_owned()),
            None => (f.cause.clone(), f.advice.clone()),
        };
        let icon = super::terminal::severity_icon(f.severity);
        for (i, l) in wrap_text(&cause, width, "    ", "    ")
            .into_iter()
            .enumerate()
        {
            let body = l.trim_start().to_owned();
            lines.push(if i == 0 {
                format!("  {icon} {body}")
            } else {
                format!("    {body}")
            });
        }
        if !f.evidence.text.is_empty() {
            prose(&mut lines, &f.evidence.text, "    ", "    ", dim);
        }
        prose(
            &mut lines,
            &format!("→ {advice}"),
            "    ",
            "      ",
            |s| s.to_owned(),
        );
        if let Some(c) = f.verification.as_ref().map(|v| &v.rerun) {
            // Whole, on its own line: a command is copied, never cut or wrapped.
            lines.push(format!("    {} {c}", dim(t("label.next"))));
        }
    }
    if r.findings.len() > 3 {
        let n = r.findings.len() - 3;
        with_command(
            &mut lines,
            width,
            "  ",
            "brief.more_findings",
            &[("n", &n)],
            &format!("oprobe explain {run_id}"),
        );
    }
    let s = &r.summary;
    prose(
        &mut lines,
        &tf(
            "brief.numbers",
            &[
                ("ops", &format!("{:.1}", s.ops_per_s)),
                ("mib", &format!("{:.1}", s.mib_per_s)),
                ("p50", &fmt_ms(s.latency.p50)),
                ("p99", &fmt_ms(s.latency.p99)),
                ("errors", &format!("{:.2}", s.error_rate * 100.0)),
            ],
        ),
        "  ",
        "  ",
        |s| s.to_owned(),
    );
    let mut extra = Vec::new();
    let throttled = r.resilience.throttled_responses;
    if throttled > 0 {
        let ratio = crate::util::ratio(throttled as f64, r.connections.requests as f64) * 100.0;
        extra.push(tf(
            "brief.throttled",
            &[("n", &throttled), ("ratio", &format!("{ratio:.2}"))],
        ));
    }
    if r.cost.applicable {
        let label = crate::analysis::cost::storage_label(&r.cost);
        let usd = tf(
            "brief.per_month",
            &[("usd", &fmt_usd(r.cost.selected.total_usd))],
        );
        extra.push(if label.is_empty() {
            usd
        } else {
            format!("{usd} {label}")
        });
    }
    let v = &r.verdict.sla;
    extra.push(
        if v.passed {
            // "1105 throttled · SLA met" read as if throttling had been checked (review C5).
            if throttled > 0 && r.sla.max_throttle_ratio.is_none() {
                t("brief.sla_met_unchecked_throttle")
            } else {
                t("brief.sla_met")
            }
        } else if v.enforced {
            t("brief.sla_breached")
        } else {
            t("brief.sla_breached_soft")
        }
        .to_owned(),
    );
    prose(&mut lines, &extra.join(" · "), "  ", "  ", |s| {
        s.to_owned()
    });
    if !v.passed {
        for b in v.breaches.iter().take(3) {
            prose(&mut lines, b, "    ", "      ", red);
        }
    }
    with_command(
        &mut lines,
        width,
        "",
        "brief.full_report",
        &[],
        &format!("oprobe runs show {run_id}"),
    );
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// A dimmed message ending in a command (`{cmd}`): on one line when it fits, else the message,
/// then the command whole on the next line.
fn with_command(
    lines: &mut Vec<String>,
    width: usize,
    indent: &str,
    key: &'static str,
    args: &[(&str, &dyn std::fmt::Display)],
    cmd: &str,
) {
    let mut all: Vec<(&str, &dyn std::fmt::Display)> = args.to_vec();
    all.push(("cmd", &cmd));
    let one = format!("{indent}{}", tf(key, &all));
    if display_width(&one) <= width {
        lines.push(dim(&one));
        return;
    }
    all.pop();
    all.push(("cmd", &""));
    lines.push(dim(&format!("{indent}{}", tf(key, &all).trim_end())));
    lines.push(dim(&format!("{indent}  {cmd}")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Finding;

    #[test]
    fn brief_wraps_to_the_width_and_never_truncates() {
        set(false);
        let mut r = crate::model::Report {
            spec: Default::default(),
            capacity: Default::default(),
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
        r.verdict.headline = "x".repeat(200);
        r.verdict.sla.passed = false;
        r.verdict.sla.breaches = vec!["y".repeat(300); 5];
        for _ in 0..10 {
            r.findings.push(Finding {
                cause: "c".repeat(120),
                advice: "a".repeat(120),
                verification: Some(crate::model::Verification {
                    description: "d".into(),
                    rerun: "oprobe run x ".repeat(20),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        for width in [40, 80, 100, 160] {
            let out = render_width(&r, "20261120-143005-a1b2", width);
            for l in out.lines() {
                // Commands are printed whole; every other line fits.
                if l.trim_start().starts_with("oprobe ") {
                    continue;
                }
                if l.contains("next: oprobe") {
                    assert!(
                        l.trim_end().ends_with("oprobe run x oprobe run x"),
                        "cut command: {l}"
                    );
                    continue;
                }
                assert!(
                    display_width(l) <= width,
                    "{} > {width}: {l}",
                    display_width(l)
                );
            }
            // Nothing is truncated: the whole headline and advice are there.
            let flat: String = out.split_whitespace().collect();
            assert!(flat.contains(&"x".repeat(200)), "{out}");
            assert!(flat.contains(&"a".repeat(120)), "{out}");
            assert!(!out.contains('…') || out.contains("more findings"), "{out}");
        }
        let out = render_width(&r, "20261120-143005-a1b2", 80);
        assert!(out.contains("oprobe runs show 20261120-143005-a1b2"));
    }
}
