//! The brief result of `oprobe run` (design v6 §8): one screen — the verdict line, at most
//! three findings (cause · evidence · remedy path · `next: oprobe prove RUN CODE`), the key
//! numbers, and where the full profile is. No sizing numbers: prove sizes the remedy.
//!
//! The brief wraps to the terminal width (or 100 columns off a terminal, `COLUMNS` when set;
//! [`style::term_width`]) and never truncates: prose wraps at words, and `next` commands are
//! printed whole on their own line so they can be copied. Everything it says comes from the
//! presentation model ([`super::view`]).

use super::Report;
use super::style::*;
use super::view::{FindingView, NumbersView, VerdictView};
use crate::util::fmt_ms;

/// Render the brief result of a run, for this terminal.
pub fn render(r: &Report) -> String {
    render_width(r, term_width())
}

fn confidence(c: crate::analysis::capacity::Confidence) -> &'static str {
    match c {
        crate::analysis::capacity::Confidence::Low => "low confidence",
        crate::analysis::capacity::Confidence::Medium => "medium confidence",
        crate::analysis::capacity::Confidence::High => "high confidence",
    }
}

/// The key numbers on one line.
pub fn numbers_line(n: &NumbersView) -> String {
    let mut s = format!(
        "{:.1} ops/s · {:.1} MiB/s · p50 {} · p99 {} · errors {:.2}%",
        n.goodput_ops_per_s,
        n.mib_per_s,
        fmt_ms(n.p50_ms),
        fmt_ms(n.p99_ms),
        n.error_ratio * 100.0
    );
    if n.throttled_requests_count > 0 {
        s += &format!(
            " · {:.2}% of requests throttled",
            n.throttled_request_ratio * 100.0
        );
    }
    s
}

/// [`render`] for `width` columns.
pub fn render_width(r: &Report, width: usize) -> String {
    let width = width.max(40);
    let v = VerdictView::of(r);
    let findings: Vec<FindingView> = super::view::findings(r);
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
    let badge = super::terminal::verdict_badge(v.level);
    let badge_pad = " ".repeat(visible_width(&badge) + 1);
    for (i, l) in wrap_text(&v.headline, width, &badge_pad, "  ")
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            lines.push(format!("{badge} {}", bold(l.trim_start())));
        } else {
            lines.push(format!("  {}", bold(l.trim_start())));
        }
    }
    prose(
        &mut lines,
        &format!(
            "{} · {} · {:.1} s · run {}",
            v.operator, v.preset, v.window_s, v.run_id
        ),
        "  ",
        "  ",
        dim,
    );
    if let Some(p) = &v.partial {
        lines.push(yellow(&format!("  partial: {p}")));
    }
    for (i, f) in findings.iter().take(3).enumerate() {
        let icon = super::terminal::severity_icon(f.severity);
        prose(
            &mut lines,
            &format!(
                "{} {} ({}) — {}",
                i + 1,
                f.code,
                confidence(f.confidence),
                f.cause
            ),
            &format!("  {icon} "),
            "      ",
            |s| s.to_owned(),
        );
        if !f.evidence.is_empty() {
            prose(&mut lines, &f.evidence, "      ", "      ", dim);
        }
        let remedy = match &f.remedy_path {
            Some(_) => format!(
                "remedy: {} — {} ({})",
                f.remedy_label(),
                f.remedy,
                f.remedy_opendal
            ),
            None => format!("remedy: {}", f.remedy),
        };
        prose(&mut lines, &remedy, "      ", "        ", |s| s.to_owned());
        if let Some(c) = &f.next {
            // Whole, on its own line: a command is copied, never cut or wrapped.
            lines.push(format!("      {} {c}", dim("next:")));
        }
    }
    if findings.len() > 3 {
        lines.push(dim(&format!(
            "  … {} more findings: oprobe show {} --detail",
            findings.len() - 3,
            v.run_id
        )));
    }
    prose(
        &mut lines,
        &numbers_line(&NumbersView::of(r)),
        "  ",
        "  ",
        |s| s.to_owned(),
    );
    if !v.sla_breaches.is_empty() {
        prose(
            &mut lines,
            &format!("✖ {}", v.sla_breaches.join("; ")),
            "  ",
            "    ",
            red,
        );
    }
    lines.push(dim(&format!(
        "  full profile: oprobe show {} --detail",
        v.run_id
    )));
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Finding;

    #[test]
    fn brief_wraps_to_the_width_and_never_truncates() {
        set(false);
        let mut r = crate::report::fixture::report();
        r.run.id = "20261120-143005-a1b2".into();
        r.verdict.headline = "x".repeat(200);
        r.verdict.sla.passed = false;
        r.verdict.sla.breaches = vec!["y".repeat(300); 2];
        for _ in 0..10 {
            r.findings.push(Finding {
                code: "throttling".into(),
                cause: "c".repeat(120),
                remedy: crate::model::Remedy {
                    path: Some("layers.concurrent_limit.http".into()),
                    text: "a".repeat(120),
                    next: Some("oprobe prove 20261120-143005-a1b2 throttling".into()),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        for width in [40, 80, 100, 160] {
            let out = render_width(&r, width);
            for l in out.lines() {
                if l.contains("oprobe ") {
                    continue;
                }
                assert!(
                    display_width(l) <= width,
                    "{} > {width}: {l}",
                    display_width(l)
                );
            }
            let flat: String = out.split_whitespace().collect();
            assert!(flat.contains(&"x".repeat(200)), "{out}");
            assert!(flat.contains(&"a".repeat(120)), "{out}");
        }
        let out = render_width(&r, 100);
        assert!(out.contains("next: oprobe prove 20261120-143005-a1b2 throttling"));
        assert!(out.contains("oprobe show 20261120-143005-a1b2 --detail"));
        // At most three findings.
        assert_eq!(out.matches("remedy:").count(), 3, "{out}");
    }

    #[test]
    fn the_brief_names_the_remedy_path_and_no_sizing_number() {
        set(false);
        let r = crate::report::fixture::report();
        let out = render_width(&r, 100);
        assert!(
            out.contains("remedy: layers.concurrent_limit.http + layers.retry.jitter"),
            "{out}"
        );
        let remedy = out.lines().find(|l| l.contains("remedy:")).unwrap();
        assert!(!remedy.chars().any(|c| c.is_ascii_digit()), "{remedy}");
        assert!(out.lines().count() <= 24, "{out}");
    }
}
