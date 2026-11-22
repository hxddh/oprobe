//! Human-oriented terminal rendering of reports.

use std::fmt::Write as _;

use super::Report;
use super::style::*;
use crate::analysis::diagnosis::Severity;
use crate::model::{OpRow, ReqRow, Run, VerdictLevel, WireProtocol};
use crate::trace::{ApiStats, VerbStats};
use crate::util::{fmt_bytes, fmt_ms, fmt_usd};

/// `HEALTHY` / `REVIEW` / `ATTENTION` / `CRITICAL` badge of a verdict level.
pub fn verdict_badge(level: VerdictLevel) -> String {
    match level {
        VerdictLevel::Ok => badge("HEALTHY", "42;30"),
        VerdictLevel::Review => badge("REVIEW", "46;30"),
        VerdictLevel::Attention => badge("ATTENTION", "43;30"),
        VerdictLevel::Critical => badge("CRITICAL", "41"),
    }
}

fn section(out: &mut String, title: &str) {
    let _ = writeln!(out, "\n{}", bold(&cyan(title)));
}

pub fn severity_icon(s: Severity) -> String {
    match s {
        Severity::Critical => red("✖"),
        Severity::Warning => yellow("▲"),
        Severity::Info => blue("●"),
    }
}

/// Render the time-share bars (used by run reports and the TUI-less live view).
pub fn time_share_lines(r: &Report) -> String {
    let time = super::view::TimeView::of(r);
    let mut out = String::new();
    let rows: Vec<(&str, f64, &str)> = time
        .stages
        .iter()
        .map(|s| (s.name, s.ratio, s.description))
        .collect();
    let max = rows.iter().map(|r| r.1).fold(0.0, f64::max).max(1e-9);
    for (name, v, desc) in rows {
        if v < 0.0005 && name != "server" {
            continue;
        }
        let b = bar(v / max, 20);
        let colored = if (v - max).abs() < 1e-12 {
            magenta(&b)
        } else {
            dim(&b)
        };
        let _ = writeln!(
            out,
            "  {:<9} {colored} {:>5.1}%  {}",
            name,
            v * 100.0,
            dim(desc)
        );
    }
    out
}

/// `✓ met (failed ops ≤ 1%, no corrupted data)` or `✖ BREACHED: …`.
pub fn sla_line(r: &Report) -> String {
    let sla = &r.sla;
    let mut terms = vec![
        format!("failed ops ≤ {}%", sla.max_error_ratio * 100.0),
        "no corrupted data".to_owned(),
    ];
    if let Some(t) = sla.max_throttle_ratio {
        terms.push(format!("throttled requests ≤ {}%", t * 100.0));
    }
    if let Some(p) = sla.p99_ms {
        terms.push(format!("p99 ≤ {}", fmt_ms(p)));
    }
    let v = &r.verdict.sla;
    let enforced = if v.enforced {
        ""
    } else {
        " · not enforced (--sla none)"
    };
    if v.passed {
        format!("{} met ({}){}", green("✓"), terms.join(", "), dim(enforced))
    } else {
        format!(
            "{} {}{}",
            red(&bold("✖ BREACHED:")),
            red(&v.breaches.join("; ")),
            dim(enforced)
        )
    }
}

/// One request of an operation that started at `op_t_us`.
pub fn request_line(i: usize, r: &ReqRow, op_t_us: i64) -> String {
    let t = r;
    let ms = |us: u64| fmt_ms(us as f64 / 1000.0);
    let status = if r.status == 0 {
        red("ERR")
    } else if r.status >= 400 {
        red(&r.status.to_string())
    } else {
        green(&r.status.to_string())
    };
    let conn = match r.protocol {
        WireProtocol::Inproc => "in-process".to_owned(),
        p => format!(
            "{} conn#{} {}",
            if p == WireProtocol::H2 { "h2" } else { "h1" },
            r.conn_id,
            if r.reused { "warm" } else { "cold" }
        ),
    };
    let mut parts = Vec::new();
    if t.queue_us > 0 {
        parts.push(format!("queue {}", ms(t.queue_us)));
    }
    if !r.reused && r.protocol != WireProtocol::Inproc {
        let phases: Vec<String> = [
            ("dns", t.dns_us),
            ("tcp", t.tcp_us),
            ("proxy", t.proxy_us),
            ("tls", t.tls_us),
        ]
        .iter()
        .filter(|(_, us)| *us > 0)
        .map(|(n, us)| format!("{n} {}", ms(*us)))
        .collect();
        parts.push(format!(
            "connect {} ({})",
            ms(t.connect_us()),
            phases.join(" ")
        ));
    }
    if r.protocol != WireProtocol::Inproc {
        parts.push(format!("send {}", ms(t.send_us)));
        parts.push(format!("ttfb {}", ms(t.ttfb_us)));
        parts.push(format!("transfer {}", ms(t.body_us)));
        if t.stall_us > 0 {
            parts.push(format!("stall {}", ms(t.stall_us)));
        }
    } else {
        parts.push(format!("service {}", ms(t.ttfb_us)));
    }
    let mut s = format!(
        "    #{:<2} +{:<8} {:<24} {} {}  {}  {}",
        i + 1,
        fmt_ms((r.t_us - op_t_us).max(0) as f64 / 1000.0),
        r.api,
        status,
        dim(&conn),
        parts.join(" · "),
        dim(&format!("= {}", ms(t.total_us)))
    );
    if let Some(e) = &r.error {
        let _ = write!(s, "  {}", red(e));
    }
    if let Some(id) = &r.request_id {
        let _ = write!(s, "  {}", dim(&format!("req-id {id}")));
    }
    s
}

/// An operation with its request timeline.
pub fn slow_op_block(op: &OpRow, requests: &[&ReqRow], show_tree: bool) -> String {
    let mut out = String::new();
    let b = &op.breakdown;
    let ms = |us: u64| fmt_ms(us as f64 / 1000.0);
    let _ = writeln!(
        out,
        "  {} {} at {:.1}s  {}",
        bold(&op.verb),
        bold(&ms(op.latency_us)),
        op.t_us as f64 / 1e6,
        dim(&format!(
            "queue {} · backend {} · retry {} · client {}{}",
            ms(b.queue_us),
            ms(b.backend_us),
            ms(b.retry_us),
            ms(b.client_us + b.cache_us),
            if op.attempts > 1 {
                format!(" · {} retries", op.attempts - 1)
            } else {
                String::new()
            }
        ))
    );
    if let Some(e) = &op.error {
        let _ = writeln!(out, "    {}", red(&format!("failed: {e}")));
    }
    for (i, r) in requests.iter().take(12).enumerate() {
        let _ = writeln!(out, "{}", request_line(i, r, op.t_us));
    }
    if requests.len() > 12 {
        let _ = writeln!(
            out,
            "    {}",
            dim(&format!("… {} more requests", requests.len() - 12))
        );
    }
    if show_tree && let Some(t) = &op.await_tree {
        let _ = writeln!(
            out,
            "    {}",
            dim("async call stack captured while in flight:")
        );
        for l in t.lines().take(16) {
            let _ = writeln!(out, "      {}", dim(l));
        }
    }
    out
}

/// Width the default full report fits in.
pub const WIDTH: usize = 80;

/// Cut a line to `max` visible columns (ANSI escapes are kept; an ellipsis marks the cut).
pub fn fit(line: &str, max: usize) -> String {
    if visible_width(line) <= max {
        return line.to_owned();
    }
    let mut out = String::new();
    let mut w = 0;
    let mut in_esc = false;
    for c in line.chars() {
        if in_esc {
            out.push(c);
            if c == 'm' {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
            out.push(c);
        } else if w + 1 < max {
            out.push(c);
            w += 1;
        } else {
            break;
        }
    }
    out.push('…');
    if enabled() {
        out.push_str("\x1b[0m");
    }
    out
}

/// Operations: compact columns by default, every column with `detail`.
pub fn verbs_table(verbs: &[VerbStats], detail: bool) -> String {
    let rows: Vec<Vec<String>> = verbs
        .iter()
        .map(|v| {
            let errors = if v.errors > 0 {
                red(&v.errors.to_string())
            } else {
                "0".into()
            };
            if detail {
                vec![
                    v.verb.clone(),
                    v.count.to_string(),
                    format!("{:.1}", v.ops_per_s),
                    format!("{:.1}", v.mib_per_s),
                    fmt_ms(v.latency.p50),
                    fmt_ms(v.latency.p90),
                    fmt_ms(v.latency.p99),
                    fmt_ms(v.latency.p999),
                    fmt_ms(v.latency.max),
                    format!("{:.2}", v.requests_per_op),
                    v.retries.to_string(),
                    errors,
                ]
            } else {
                vec![
                    v.verb.clone(),
                    v.count.to_string(),
                    format!("{:.1}", v.ops_per_s),
                    fmt_ms(v.latency.p50),
                    fmt_ms(v.latency.p99),
                    fmt_ms(v.latency.max),
                    format!("{:.2}", v.requests_per_op),
                    errors,
                ]
            }
        })
        .collect();
    if detail {
        table(
            &[
                "operation",
                "count",
                "ops/s",
                "MiB/s",
                "p50",
                "p90",
                "p99",
                "p99.9",
                "max",
                "req/op",
                "retries",
                "errors",
            ],
            &rows,
            2,
        )
    } else {
        table(
            &[
                "operation",
                "count",
                "ops/s",
                "p50",
                "p99",
                "max",
                "req/op",
                "errors",
            ],
            &rows,
            2,
        )
    }
}

fn status_summary(a: &ApiStats) -> String {
    a.status
        .iter()
        .map(|(c, n)| {
            let label = if *c == 0 {
                "err".to_owned()
            } else {
                c.to_string()
            };
            let t = format!("{label}×{n}");
            if *c == 0 || *c >= 400 { red(&t) } else { t }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Backend requests: compact columns by default (mean ttfb, p99, statuses), every stage with
/// `detail`.
pub fn apis_table(apis: &[ApiStats], detail: bool) -> String {
    let rows: Vec<Vec<String>> = apis
        .iter()
        .map(|a| {
            let st = &a.stages;
            if detail {
                vec![
                    a.api.label().to_owned(),
                    a.count.to_string(),
                    format!("{:.0}%", a.reused_pct),
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
                    fmt_ms(st.total.p99),
                    status_summary(a),
                ]
            } else {
                vec![
                    a.api.label().to_owned(),
                    a.count.to_string(),
                    format!("{:.0}%", a.reused_pct),
                    fmt_ms(st.ttfb.mean),
                    fmt_ms(st.body.mean),
                    fmt_ms(st.total.p99),
                    status_summary(a),
                ]
            }
        })
        .collect();
    if detail {
        table(
            &[
                "request", "count", "warm", "queue", "connect", "send", "ttfb", "transfer",
                "stall", "p99", "status",
            ],
            &rows,
            2,
        )
    } else {
        table(
            &[
                "request", "count", "warm", "ttfb", "transfer", "p99", "status",
            ],
            &rows,
            2,
        )
    }
}

/// The full report of a report without trace rows.
pub fn render(r: &Report) -> String {
    render_with(r, None, false)
}

/// The full report of a run (`oprobe runs show`), fitting 80 columns.
pub fn render_run(run: &Run) -> String {
    render_with(&run.report, Some(run), false)
}

/// The full report; `detail` adds every column, per-stage and per-verb breakdowns, remote
/// addresses, the per-second timeline, effective knobs and the five slowest operations.
pub fn render_run_with(run: &Run, detail: bool) -> String {
    render_with(&run.report, Some(run), detail)
}

fn kv(out: &mut String, k: &str, v: &str) {
    let _ = writeln!(out, "  {} {v}", dim(&format!("{k:<11}")));
}

fn render_with(r: &Report, run: Option<&Run>, detail: bool) -> String {
    let mut out = String::new();
    let sm = &r.summary;

    // Identity.
    let _ = writeln!(
        out,
        "{} {} {}",
        bold("oprobe"),
        dim(&r.run.oprobe_version),
        dim(&format!(
            "· run {}{}",
            r.run.id,
            r.run
                .label
                .as_ref()
                .map(|l| format!(" · {l}"))
                .unwrap_or_default()
        ))
    );
    let _ = writeln!(
        out,
        "{} {}",
        bold(&r.target.display),
        dim(&r
            .target
            .name
            .as_ref()
            .map(|n| format!("(target {n})"))
            .unwrap_or_default())
    );
    let _ = writeln!(
        out,
        "{}",
        dim(&format!(
            "{} · {:.1}s measured · {} · client {} · {}",
            r.workload.preset,
            r.run.window_s,
            if let Some(rate) = r.workload.rate_per_s {
                format!("open loop {rate} ops/s")
            } else {
                format!("{} workers", r.knobs.requested.concurrency_count)
            },
            match r.knobs.requested.transport {
                crate::config::TransportMode::Opendal => "OpenDAL reqwest",
                crate::config::TransportMode::Microscope => "microscope",
            },
            r.run.started_at
        ))
    );
    if let Some(ep) = &r.target.endpoint {
        let _ = writeln!(
            out,
            "{}",
            dim(&format!(
                "endpoint {ep}{}",
                r.target
                    .proxy
                    .as_ref()
                    .map(|p| format!(" via proxy {p}"))
                    .unwrap_or_default()
            ))
        );
    }
    if r.is_partial() {
        let reason = r
            .run
            .partial
            .as_ref()
            .map_or("interrupted", |p| p.reason.as_str());
        let _ = writeln!(
            out,
            "{} {}",
            badge(&format!("PARTIAL · {reason}"), "43;30"),
            yellow("stopped early: a shorter window than configured")
        );
    }
    for w in &r.warnings {
        let _ = writeln!(out, "{} {}", yellow("warning:"), w);
    }
    let _ = writeln!(
        out,
        "\n{} {}",
        verdict_badge(r.verdict.level),
        bold(&r.verdict.headline)
    );

    section(&mut out, "KEY NUMBERS");
    kv(
        &mut out,
        "throughput",
        &format!(
            "{:.1} ops/s · {:.1} MiB/s {}",
            sm.ops_per_s,
            sm.mib_per_s,
            dim(&if sm.failed_ops > 0 {
                format!(
                    "(successful ops; {:.1} attempted/s · read {:.1} / write {:.1})",
                    sm.attempted_ops_per_s, sm.read_mib_per_s, sm.write_mib_per_s
                )
            } else {
                format!(
                    "(read {:.1} / write {:.1})",
                    sm.read_mib_per_s, sm.write_mib_per_s
                )
            })
        ),
    );
    kv(
        &mut out,
        "latency",
        &format!(
            "p50 {} · p99 {} · p99.9 {} · max {}",
            fmt_ms(sm.latency.p50),
            fmt_ms(sm.latency.p99),
            fmt_ms(sm.latency.p999),
            fmt_ms(sm.latency.max)
        ),
    );
    kv(
        &mut out,
        "errors",
        &format!(
            "{} failed ops ({:.2}%) · {}",
            if sm.failed_ops > 0 {
                red(&sm.failed_ops.to_string())
            } else {
                "0".into()
            },
            sm.error_rate * 100.0,
            if r.resilience.throttled_responses > 0 {
                red(&format!(
                    "{} throttled ({:.2}% of requests, {:.2}% of ops)",
                    r.resilience.throttled_responses,
                    sm.throttled_request_ratio * 100.0,
                    sm.throttled_op_ratio * 100.0
                ))
            } else {
                "0 throttled".into()
            }
        ),
    );
    kv(&mut out, "sla", &sla_line(r));
    if r.cost.applicable && sm.ops > 0 {
        kv(
            &mut out,
            "cost",
            &format!(
                "{} per 1M ops · {} /month ({}){}",
                fmt_usd(r.cost.selected.per_million_ops_usd),
                fmt_usd(r.cost.selected.total_usd),
                r.cost.selected.provider,
                match crate::analysis::cost::storage_label(&r.cost) {
                    l if l.is_empty() => l,
                    l => format!(" {}", dim(&l)),
                }
            ),
        );
    }
    if let Some(ol) = &r.open_loop {
        kv(
            &mut out,
            "open loop",
            &format!(
                "target {:.0} ops/s · achieved {:.0} · missed {} · lag {}",
                ol.target_rate,
                ol.achieved_rate,
                ol.missed,
                fmt_ms(ol.max_schedule_lag_ms)
            ),
        );
    }

    if !r.findings.is_empty() {
        section(&mut out, "FINDINGS");
        for f in super::view::findings_by_severity(r) {
            let _ = writeln!(
                out,
                "  {} {} {}",
                severity_icon(f.severity),
                bold(f.cause),
                dim(&format!("[{}]", f.code))
            );
            for l in wrap_text(f.evidence, WIDTH - 4) {
                let _ = writeln!(out, "    {}", dim(&l));
            }
            if detail {
                let kv: Vec<String> = f
                    .items
                    .iter()
                    .map(|e| format!("{} {}", e.label, e.value))
                    .collect();
                for l in wrap_text(&kv.join(" · "), WIDTH - 4) {
                    let _ = writeln!(out, "    {}", dim(&l));
                }
            }
            for (i, l) in wrap_text(f.advice, WIDTH - 6).iter().enumerate() {
                let _ = writeln!(out, "    {} {l}", if i == 0 { "→" } else { " " });
            }
            if let Some(rerun) = f.rerun {
                let _ = writeln!(out, "    {} {}", dim("try:"), rerun);
            }
        }
    }

    section(
        &mut out,
        "WHERE THE TIME GOES  (share of operation wall time)",
    );
    out.push_str(&time_share_lines(r));
    if detail {
        let p = &r.stages.request_p99;
        let _ = writeln!(
            out,
            "  {}",
            dim(&format!(
                "request p99: queue {} · connect {} · send {} · ttfb {} · body {} · stall {} · total {}",
                fmt_ms(p.queue_ms),
                fmt_ms(p.connect_ms),
                fmt_ms(p.send_ms),
                fmt_ms(p.ttfb_ms),
                fmt_ms(p.body_ms),
                fmt_ms(p.stall_ms),
                fmt_ms(p.total_ms)
            ))
        );
        if !r.stages.per_verb.is_empty() {
            let rows: Vec<Vec<String>> = r
                .stages
                .per_verb
                .iter()
                .map(|v| {
                    let b = &v.breakdown;
                    vec![
                        v.verb.clone(),
                        fmt_ms(b.queue_ms),
                        fmt_ms(b.cache_ms),
                        fmt_ms(b.backend_ms),
                        fmt_ms(b.retry_ms),
                        fmt_ms(b.client_ms),
                    ]
                })
                .collect();
            out.push_str(&table(
                &[
                    "mean per op",
                    "queue",
                    "cache",
                    "backend",
                    "retry",
                    "client",
                ],
                &rows,
                2,
            ));
        }
    }

    section(&mut out, "OPERATIONS");
    out.push_str(&verbs_table(&r.verbs, detail));

    if !r.apis.is_empty() {
        section(
            &mut out,
            "BACKEND REQUESTS  (mean per request; p99 of the total)",
        );
        out.push_str(&apis_table(&r.apis, detail));
        if detail && r.connections.http2_requests > 0 {
            let _ = writeln!(
                out,
                "  {}",
                dim(
                    "h2: `send` ends when the body is handed to the stream; flow control shows up in ttfb"
                )
            );
        }
    }

    let c = &r.connections;
    if c.requests > 0 && (c.http1_requests + c.http2_requests) > 0 {
        section(&mut out, "CONNECTIONS");
        let _ = writeln!(
            out,
            "  {}{}",
            super::connection_protocol(c),
            c.proxy
                .as_ref()
                .map(|p| format!(" · via {p}"))
                .unwrap_or_default()
        );
        let _ = writeln!(
            out,
            "  {} new · {:.1}% of requests warm · peak {} in flight{}",
            c.dialed,
            c.reuse_pct,
            c.peak_inflight_requests,
            if c.dial_failures > 0 {
                red(&format!(" · {} failed dials", c.dial_failures))
            } else {
                String::new()
            }
        );
        let phases = super::dial_phases(c);
        if !phases.is_empty() {
            let _ = writeln!(out, "  dial p50/p99 {phases}");
        }
        if detail && !r.network.remotes.is_empty() {
            let rows: Vec<Vec<String>> = r
                .network
                .remotes
                .iter()
                .map(|x| {
                    vec![
                        x.remote.clone(),
                        x.requests_count.to_string(),
                        x.errors_count.to_string(),
                        fmt_bytes(x.bytes_down),
                        fmt_ms(x.p50_ms),
                        fmt_ms(x.p99_ms),
                    ]
                })
                .collect();
            out.push_str(&table(
                &["remote", "requests", "errors", "down", "p50", "p99"],
                &rows,
                2,
            ));
        } else if !c.remote_addrs.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                dim(&format!("remote {}", c.remote_addrs.join(", ")))
            );
        }
    }

    section(&mut out, "BYTES & AMPLIFICATION");
    let b = &r.bytes;
    let _ = writeln!(
        out,
        "  read {} · write {} · backend down {} · up {}",
        fmt_bytes(b.logical_read),
        fmt_bytes(b.logical_write),
        fmt_bytes(b.backend_down),
        fmt_bytes(b.backend_up),
    );
    let _ = writeln!(
        out,
        "  read amplification {:.2}× · over-read {:.2}× · {:.2} requests/op",
        b.read_amplification, b.overread_ratio, b.request_amplification,
    );
    if b.wire_rx + b.wire_tx > 0 {
        let _ = writeln!(
            out,
            "  wire rx {} · tx {} {}",
            fmt_bytes(b.wire_rx),
            fmt_bytes(b.wire_tx),
            dim("(--microscope)")
        );
    }
    if r.cache.enabled {
        let _ = writeln!(
            out,
            "  cache {:.1}% hit ({} of {} lookups, mean {})",
            r.cache.hit_ratio * 100.0,
            r.cache.hits,
            r.cache.lookups,
            fmt_ms(r.cache.lookup_ms_mean)
        );
    }
    if r.integrity.verified + r.integrity.corrupted > 0 {
        let _ = writeln!(
            out,
            "  integrity {} {} verified{}",
            if r.integrity.corrupted == 0 {
                green("✓")
            } else {
                red("✖")
            },
            r.integrity.verified,
            if r.integrity.corrupted > 0 {
                red(&format!(", {} CORRUPTED", r.integrity.corrupted))
            } else {
                String::new()
            }
        );
    }

    if r.cost.applicable && sm.ops > 0 {
        let cst = &r.cost;
        section(
            &mut out,
            &format!(
                "COST  ({} · {:.0}M ops/month · egress {:.0}%)",
                cst.selected.provider,
                cst.monthly_ops / 1e6,
                cst.egress_fraction * 100.0
            ),
        );
        let sel = &cst.selected;
        let _ = writeln!(
            out,
            "  {} per 1M ops → {} /month",
            bold(&fmt_usd(sel.per_million_ops_usd)),
            bold(&fmt_usd(sel.total_usd))
        );
        let _ = writeln!(
            out,
            "  requests {} · egress {} · storage {} for {:.0} GiB{}",
            fmt_usd(sel.requests_usd),
            fmt_usd(sel.egress_usd),
            fmt_usd(sel.storage_usd),
            cst.stored_gb,
            match crate::analysis::cost::storage_label(cst) {
                l if l.is_empty() => l,
                l => format!(" {}", dim(&l)),
            }
        );
        if sel.failed_requests_usd > 0.005
            || sel.overread_usd > 0.005
            || sel.cache_savings_usd > 0.005
        {
            let _ = writeln!(
                out,
                "  waste: failed {} · over-read {} · cache saved {}",
                fmt_usd(sel.failed_requests_usd),
                fmt_usd(sel.overread_usd),
                fmt_usd(sel.cache_savings_usd)
            );
        }
        if detail {
            let u = &cst.per_op;
            let _ = writeln!(
                out,
                "  {}",
                dim(&format!(
                    "per op: {:.2} write · {:.2} list · {:.2} read requests · {} down · {} up",
                    u.write_requests,
                    u.list_requests,
                    u.read_requests,
                    fmt_bytes(u.download_bytes as u64),
                    fmt_bytes(u.upload_bytes as u64)
                ))
            );
        }
        let mut cmp: Vec<_> = cst.comparison.iter().collect();
        cmp.sort_by(|a, b| a.total_usd.total_cmp(&b.total_usd));
        if !cmp.is_empty() {
            let line: Vec<String> = cmp
                .iter()
                .map(|c| format!("{} {}", c.key, fmt_usd(c.total_usd)))
                .collect();
            if detail {
                let _ = writeln!(
                    out,
                    "  {}",
                    dim(&format!("elsewhere: {}", line.join(" · ")))
                );
            } else {
                for chunk in line.chunks(3) {
                    let _ = writeln!(out, "  {}", dim(&chunk.join(" · ")));
                }
            }
        }
    }

    if detail && !r.timeline.is_empty() {
        section(&mut out, "TIMELINE  (per second)");
        let rows: Vec<Vec<String>> = r
            .timeline
            .iter()
            .map(|p| {
                vec![
                    format!("{:.0}s", p.t_s),
                    format!("{:.1}", p.ops_per_s),
                    format!("{:.1}", p.mib_per_s),
                    fmt_ms(p.p50_ms),
                    fmt_ms(p.p99_ms),
                    p.errors.to_string(),
                    p.throttled.to_string(),
                ]
            })
            .collect();
        out.push_str(&table(
            &["t", "ops/s", "MiB/s", "p50", "p99", "errors", "throttled"],
            &rows,
            2,
        ));
    }

    if let Some(run) = run {
        let n = if detail { 5 } else { 1 };
        let slow: Vec<&OpRow> = r
            .slowest
            .iter()
            .filter_map(|id| run.op(*id))
            .take(n)
            .collect();
        if !slow.is_empty() {
            section(
                &mut out,
                if n > 1 {
                    "SLOWEST OPERATIONS"
                } else {
                    "SLOWEST OPERATION"
                },
            );
            for op in slow {
                let reqs = run.requests_of(op.op_id);
                out.push_str(&slow_op_block(op, &reqs, detail));
            }
        }
    }

    if detail {
        section(&mut out, "CONFIGURATION");
        let k = &r.knobs.requested;
        let _ = writeln!(out, "  knobs      {}", k.to_knobs().summary());
        let e = &r.knobs.effective;
        if e.write_chunk_clamped() {
            let _ = writeln!(
                out,
                "  effective  write chunk {} (requested {})",
                fmt_bytes(e.write_chunk_bytes),
                fmt_bytes(k.chunk_bytes)
            );
        }
        let w = &r.workload;
        let _ = writeln!(
            out,
            "  workload   {} · {} objects × {} · access {} · seed {}",
            w.preset,
            w.dataset.objects_count,
            fmt_bytes(w.dataset.object_size_bytes),
            w.access,
            w.seed
        );
        for (k, v) in &r.target.options {
            let _ = writeln!(out, "  option     {k}={v}");
        }
        let h = &r.run.host;
        let _ = writeln!(
            out,
            "  host       {}/{} · {} CPUs · {}",
            h.os, h.arch, h.cpus_count, h.hostname_hash
        );
        let t = &r.run.trace;
        let _ = writeln!(
            out,
            "  trace      {} of {} requests, {} of {} ops kept{}",
            t.requests_kept_count,
            t.requests_seen_count,
            t.ops_kept_count,
            t.ops_seen_count,
            if r.run.sampled { " (sampled)" } else { "" }
        );
    }

    section(&mut out, "CLEANUP");
    let cl = &r.cleanup;
    if !cl.enabled {
        let _ = writeln!(
            out,
            "  {} test data kept under {} (remove with `oprobe cleanup`)",
            yellow("●"),
            cl.prefix
        );
    } else if cl.verified && cl.deleted == 0 && cl.kept_dataset.is_some() && cl.duration_ms == 0.0 {
        let _ = writeln!(out, "  {} read-only run: nothing was written", green("✓"));
    } else if cl.verified {
        let _ = writeln!(
            out,
            "  {} removed {} objects under {} {}",
            green("✓"),
            cl.deleted,
            cl.prefix,
            dim(&format!("({:.0} ms, verified)", cl.duration_ms))
        );
    } else {
        let _ = writeln!(
            out,
            "  {} {} objects remain under {}{} — run `oprobe cleanup`",
            red("✖"),
            cl.remaining,
            cl.prefix,
            cl.error
                .as_ref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        );
    }
    if let Some(ds) = &cl.kept_dataset {
        let _ = writeln!(out, "  {}", dim(&format!("dataset left in place: {ds}")));
    }

    if !r.run.id.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            dim(&format!(
                "more: oprobe runs show {} --detail · -o html > report.html",
                r.run.id
            ))
        );
    }
    if detail {
        out
    } else {
        out.lines()
            .map(|l| fit(l, WIDTH))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }
}

/// Word-wrap plain text to `width` columns.
pub fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let wl = word.chars().count();
        let cl = cur.chars().count();
        if cl > 0 && cl + 1 + wl > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_report_fits_80_columns_and_detail_adds_sections() {
        set(false);
        let mut run = super::super::fixture::run();
        run.report.findings[0].advice = "word ".repeat(60);
        run.report.target.display = format!("s3://{}", "b".repeat(120));
        let text = render_run(&run);
        for l in text.lines() {
            assert!(visible_width(l) <= WIDTH, "{} > 80: {l}", visible_width(l));
        }
        for s in [
            "KEY NUMBERS",
            "FINDINGS",
            "WHERE THE TIME GOES",
            "OPERATIONS",
            "SLOWEST OPERATION",
            "CLEANUP",
            "try: oprobe run --from 20261120-143005-a1b2 --set concurrency=16",
            "--detail",
        ] {
            assert!(text.contains(s), "{s}\n{text}");
        }
        let detail = render_run_with(&run, true);
        for s in ["TIMELINE", "CONFIGURATION", "SLOWEST OPERATIONS", "p99.9"] {
            assert!(detail.contains(s), "{s}");
        }
    }

    #[test]
    fn fit_keeps_escapes_and_cuts_visible_text() {
        let s = format!("\x1b[31m{}\x1b[0m tail", "x".repeat(100));
        let f = fit(&s, 10);
        assert_eq!(visible_width(&f), 10);
        assert!(f.starts_with("\x1b[31m"));
        assert_eq!(wrap_text("a bb ccc dddd", 6), vec!["a bb", "ccc", "dddd"]);
    }
}
