//! Markdown rendering (PR comments, wikis, job summaries).

use std::fmt::Write as _;

use super::Report;
use super::view::ReportView;
use crate::analysis::diagnosis::Severity;
use crate::model::{OpRow, ReqRow, Run};
use crate::util::{fmt_bytes, fmt_ms, fmt_usd};

fn icon(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "🔴",
        Severity::Warning => "🟠",
        Severity::Info => "🔵",
    }
}

/// A report without trace rows as Markdown.
pub fn render_report(r: &Report) -> String {
    render_with(r, None)
}

/// A run as Markdown (PR comments, job summaries, wikis), with the slowest operation's
/// request timeline when the run has trace rows.
pub fn render(run: &Run) -> String {
    let slow = run
        .report
        .slowest
        .first()
        .and_then(|id| run.op(*id))
        .map(|op| (op, run.requests_of(op.op_id)));
    render_with(&run.report, slow)
}

fn render_with(r: &Report, slow: Option<(&OpRow, Vec<&ReqRow>)>) -> String {
    let s = r;
    let view = ReportView::of(r);
    let mut md = String::new();
    let _ = writeln!(md, "## oprobe report — `{}`\n", r.target.display);
    let _ = writeln!(
        md,
        "**{}** · preset `{}` · {:.1}s measured · {} · oprobe {} · {}\n",
        r.verdict.headline,
        r.workload.preset,
        r.run.window_s,
        r.knobs.requested.to_knobs().summary(),
        r.run.oprobe_version,
        r.run.started_at
    );
    if view.partial_note {
        let _ = writeln!(
            md,
            "> ⚠️ **Partial** — the run was stopped early; results cover a shorter window than configured.\n"
        );
    }
    for w in view.warnings {
        let _ = writeln!(md, "> ⚠️ {w}\n");
    }
    let sm = &view.summary;
    let _ = writeln!(
        md,
        "| good ops/s | MiB/s | p50 | p99 | p99.9 | failed ops | throttled requests |\n|---:|---:|---:|---:|---:|---:|---:|"
    );
    let _ = writeln!(
        md,
        "| {:.1} | {:.1} | {} | {} | {} | {} ({:.2}%) | {} ({:.2}%) |\n",
        sm.good_ops_per_s,
        sm.mib_per_s,
        fmt_ms(sm.p50_ms),
        fmt_ms(sm.p99_ms),
        fmt_ms(sm.p999_ms),
        sm.failed_ops_count,
        sm.error_ratio * 100.0,
        sm.throttled_requests_count,
        sm.throttled_request_ratio * 100.0
    );

    let v = &view.sla;
    let _ = writeln!(
        md,
        "**SLA:** {}{}\n",
        if v.passed {
            "✅ met".to_owned()
        } else {
            format!("❌ breached — {}", v.breaches.join("; "))
        },
        if v.enforced { "" } else { " (not enforced)" }
    );

    let _ = writeln!(
        md,
        "### Where the time goes\n\n| layer | share |\n|---|---:|"
    );
    for st in &view.time.stages {
        let _ = writeln!(md, "| {} | {:.1}% |", st.title, st.ratio * 100.0);
    }

    let _ = writeln!(
        md,
        "\n### Operations\n\n| operation | count | ops/s | MiB/s | p50 | p90 | p99 | max | req/op | errors |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );
    for v in &s.verbs {
        let _ = writeln!(
            md,
            "| `{}` | {} | {:.1} | {:.1} | {} | {} | {} | {} | {:.2} | {} |",
            v.verb,
            v.count,
            v.ops_per_s,
            v.mib_per_s,
            fmt_ms(v.latency.p50),
            fmt_ms(v.latency.p90),
            fmt_ms(v.latency.p99),
            fmt_ms(v.latency.max),
            v.requests_per_op,
            v.errors
        );
    }

    if !s.apis.is_empty() {
        let _ = writeln!(
            md,
            "\n### Backend requests (mean per request)\n\n| request | count | warm | queue | connect | send | ttfb | transfer | stall | p99 |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
        );
        for a in &s.apis {
            let st = &a.stages;
            let _ = writeln!(
                md,
                "| {} | {} | {:.0}% | {} | {} | {} | {} | {} | {} | {} |",
                a.api.label(),
                a.count,
                a.reused_pct,
                fmt_ms(st.queue.mean),
                if st.connect.count > 0 {
                    fmt_ms(st.connect.mean)
                } else {
                    "-".into()
                },
                fmt_ms(st.send.mean),
                fmt_ms(st.ttfb.mean),
                fmt_ms(st.body.mean),
                fmt_ms(st.stall.mean),
                fmt_ms(st.total.p99)
            );
        }
    }

    let c = &view.cost;
    let _ = writeln!(
        md,
        "\n### Bytes & cost\n\n- read amplification **{:.2}×**, over-read {:.2}×, **{:.2} requests/op**; downloaded {}, uploaded {}",
        c.read_amplification,
        c.overread_ratio,
        c.requests_per_op,
        fmt_bytes(c.downloaded_bytes),
        fmt_bytes(c.uploaded_bytes)
    );
    if c.applicable {
        let _ = writeln!(
            md,
            "- {}: **{} per 1M ops** ({} requests + {} egress per month at {:.0}M ops; storage {} for {:.0} GiB{})",
            c.provider,
            fmt_usd(c.per_million_ops_usd),
            fmt_usd(c.requests_usd),
            fmt_usd(c.egress_usd),
            c.monthly_ops / 1e6,
            fmt_usd(c.storage_usd),
            c.stored_gib,
            match c.storage_label.as_str() {
                "" => String::new(),
                l => format!(" {l}"),
            }
        );
    } else {
        let _ = writeln!(md, "- cost: not applicable — {}", c.not_applicable_reason);
    }

    if !view.findings.is_empty() {
        let _ = writeln!(md, "\n### Findings\n");
        for f in &view.findings {
            let _ = writeln!(
                md,
                "- {} **{}** — {}  \n  → {}",
                icon(f.severity),
                f.cause,
                f.evidence,
                f.advice
            );
            let kv: Vec<String> = f
                .items
                .iter()
                .map(|e| format!("{} **{}**", e.label, e.value))
                .collect();
            if !kv.is_empty() {
                let _ = writeln!(md, "  {}", kv.join(" · "));
            }
        }
    }

    if let Some((op, reqs)) = slow {
        let _ = writeln!(
            md,
            "\n<details><summary>Slowest operation: <code>{}</code> {}</summary>\n\n```text",
            op.verb,
            fmt_ms(op.latency_us as f64 / 1000.0)
        );
        for (i, req) in reqs.iter().take(20).enumerate() {
            let t = req;
            let _ = writeln!(
                md,
                "#{:<2} +{:>8} {:<24} {:>3} {} queue={} connect={} send={} ttfb={} transfer={} total={}",
                i + 1,
                fmt_ms((req.t_us - op.t_us).max(0) as f64 / 1000.0),
                req.api,
                req.status,
                if req.reused { "warm" } else { "cold" },
                fmt_ms(t.queue_us as f64 / 1000.0),
                fmt_ms(t.connect_us() as f64 / 1000.0),
                fmt_ms(t.send_us as f64 / 1000.0),
                fmt_ms(t.ttfb_us as f64 / 1000.0),
                fmt_ms(t.body_us as f64 / 1000.0),
                fmt_ms(t.total_us as f64 / 1000.0)
            );
        }
        if let Some(tree) = &op.await_tree {
            let _ = writeln!(md, "\n{tree}");
        }
        let _ = writeln!(md, "```\n</details>");
    }
    md
}
