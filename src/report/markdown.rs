//! Markdown rendering (PR comments, wikis, job summaries).

use std::fmt::Write as _;

use super::Report;
use crate::analysis::diagnosis::Severity;
use crate::util::{fmt_bytes, fmt_ms, fmt_usd};

fn icon(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "🔴",
        Severity::Warning => "🟠",
        Severity::Info => "🔵",
    }
}

pub fn render(r: &Report) -> String {
    let s = &r.result;
    let mut md = String::new();
    let _ = writeln!(md, "## oprobe report — `{}`\n", r.target.display);
    let _ = writeln!(
        md,
        "**{}** · preset `{}` · {:.1}s measured · {} · oprobe {} · {}\n",
        r.diagnosis.headline,
        r.workload.preset.slug(),
        s.elapsed_s,
        r.knobs.summary(),
        r.oprobe_version,
        r.generated_at
    );
    if r.interrupted {
        let _ = writeln!(
            md,
            "> ⚠️ **Interrupted** — the run was stopped early; results cover a shorter window than configured.\n"
        );
    }
    for w in &r.warnings {
        let _ = writeln!(md, "> ⚠️ {w}\n");
    }
    let sm = &s.summary;
    let _ = writeln!(
        md,
        "| ops/s | MiB/s | p50 | p99 | p99.9 | failed ops | throttled |\n|---:|---:|---:|---:|---:|---:|---:|"
    );
    let _ = writeln!(
        md,
        "| {:.1} | {:.1} | {} | {} | {} | {} | {} |\n",
        sm.ops_per_s,
        sm.mib_per_s,
        fmt_ms(sm.latency.p50),
        fmt_ms(sm.latency.p99),
        fmt_ms(sm.latency.p999),
        sm.failed_ops,
        s.resilience.throttled_responses
    );

    let v = &r.sla_verdict;
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

    let sh = &r.diagnosis.shares;
    let _ = writeln!(
        md,
        "### Where the time goes\n\n| layer | share |\n|---|---:|"
    );
    for (n, v) in [
        ("queue", sh.queue),
        ("connect", sh.connect),
        ("upload", sh.upload),
        ("server (TTFB)", sh.server),
        ("download", sh.download),
        ("retry backoff", sh.retry),
        ("cache", sh.cache),
        ("client", sh.client),
    ] {
        let _ = writeln!(md, "| {n} | {:.1}% |", v * 100.0);
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

    let b = &s.bytes;
    let _ = writeln!(
        md,
        "\n### Bytes & cost\n\n- read amplification **{:.2}×**, over-read {:.2}×, **{:.2} requests/op**; downloaded {}, uploaded {}",
        b.read_amplification,
        b.overread_ratio,
        b.request_amplification,
        fmt_bytes(b.backend_down),
        fmt_bytes(b.backend_up)
    );
    let c = &r.cost.selected;
    if r.cost.applicable {
        let _ = writeln!(
            md,
            "- {}: **{} per 1M ops** ({} requests + {} egress per month at {:.0}M ops; storage {})",
            c.provider,
            fmt_usd(c.per_million_ops_usd),
            fmt_usd(c.requests_usd),
            fmt_usd(c.egress_usd),
            r.cost.monthly_ops / 1e6,
            fmt_usd(c.storage_usd)
        );
    } else {
        let _ = writeln!(
            md,
            "- cost: not applicable — {}",
            r.cost.not_applicable_reason.as_deref().unwrap_or_default()
        );
    }

    if !r.diagnosis.findings.is_empty() {
        let _ = writeln!(md, "\n### Findings\n");
        for f in &r.diagnosis.findings {
            let _ = writeln!(
                md,
                "- {} **{}** — {}  \n  → {}",
                icon(f.severity),
                f.title,
                f.evidence,
                f.action
            );
        }
    }

    if let Some(op) = s.slow_ops.first() {
        let _ = writeln!(
            md,
            "\n<details><summary>Slowest operation: <code>{}</code> {}</summary>\n\n```text",
            op.verb,
            fmt_ms(op.latency_ms)
        );
        for (i, req) in op.requests.iter().take(20).enumerate() {
            let t = &req.timing;
            let _ = writeln!(
                md,
                "#{:<2} +{:>8} {:<24} {:>3} {} queue={} connect={} send={} ttfb={} transfer={} total={}",
                i + 1,
                fmt_ms(req.start_offset_us as f64 / 1000.0),
                req.api.label(),
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
