//! The full profile of `oprobe show` (`--detail` adds every section), rendered from the
//! presentation model ([`super::view`]).

use std::fmt::Write as _;

use super::Report;
use super::style::*;
use crate::analysis::diagnosis::Severity;
use crate::model::{OpRow, ReqRow, Run, VerdictLevel, WireProtocol};
use crate::trace::{ApiStats, VerbStats};
use crate::util::{fmt_bytes, fmt_ms};

/// `OK` / `Review` / `Attention` / `Critical` badge of a verdict level.
pub fn verdict_badge(level: VerdictLevel) -> String {
    let color = match level {
        VerdictLevel::Ok => "42;30",
        VerdictLevel::Review => "46;30",
        VerdictLevel::Attention => "43;30",
        VerdictLevel::Critical => "41",
    };
    badge(level.label(), color)
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

/// Bars of stage shares: `(key, title, ratio)`, the largest highlighted.
fn share_bars(rows: &[super::view::StageShare]) -> String {
    let mut out = String::new();
    let max = rows.iter().map(|r| r.ratio).fold(0.0, f64::max).max(1e-9);
    for r in rows {
        if r.ratio < 0.0005 && r.key != "server" {
            continue;
        }
        let b = bar(r.ratio / max, 20);
        let colored = if (r.ratio - max).abs() < 1e-12 {
            magenta(&b)
        } else {
            dim(&b)
        };
        let _ = writeln!(out, "  {:<14} {colored} {:>5.1}%", r.title, r.ratio * 100.0);
    }
    out
}

/// The time-share bars of every operation.
pub fn time_share_lines(r: &Report) -> String {
    share_bars(&super::view::TimeView::of(r, None).stages)
}

/// One request of an operation that started at `op_t_us`, with what it recorded beyond its
/// stages: the signals that explain a slow request on their own (error code, `Retry-After`,
/// the HTTP/2 stream wait and flow-control stall, the CDN cache), and with `detail` every
/// other one (round trip, retransmits, `Server-Timing`, the kernel's upload timing, region).
pub fn request_line(i: usize, r: &ReqRow, op_t_us: i64, detail: bool) -> String {
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
        r.api_name(),
        status,
        dim(&conn),
        parts.join(" · "),
        dim(&format!("= {}", ms(t.total_us)))
    );
    if let Some(e) = &r.error {
        let _ = write!(s, "  {}", red(e));
    }
    let signals: Vec<String> = super::view::request_signals(r)
        .iter()
        .filter(|x| detail || x.explains())
        .map(|x| x.text())
        .collect();
    if !signals.is_empty() {
        let _ = write!(s, "  {}", yellow(&signals.join(" · ")));
    }
    if let Some(id) = &r.request_id {
        let _ = write!(s, "  {}", dim(&format!("req-id {id}")));
    }
    s
}

/// An operation with its request timeline; `detail` adds every request signal and the async
/// call stack.
pub fn slow_op_block(op: &OpRow, requests: &[&ReqRow], detail: bool) -> String {
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
        let _ = writeln!(out, "{}", request_line(i, r, op.t_us, detail));
    }
    if requests.len() > 12 {
        let _ = writeln!(
            out,
            "    {}",
            dim(&format!("… {} more requests", requests.len() - 12))
        );
    }
    if detail && let Some(t) = &op.await_tree {
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

/// Wrap a line to `max` visible columns at spaces (ANSI escapes are kept). Nothing is ever
/// cut: a continuation is indented two columns past the line's own indent, and a word longer
/// than the width (a URL, a command's argument) stays whole on its own line.
pub fn wrap_line(line: &str, max: usize) -> Vec<String> {
    if visible_width(line) <= max {
        return vec![line.to_owned()];
    }
    let indent = line.chars().take_while(|c| *c == ' ').count();
    let cont = " ".repeat(indent + 2);
    let mut out: Vec<String> = Vec::new();
    let mut cur = " ".repeat(indent);
    let mut cur_w = indent;
    let mut fresh = true;
    for word in line[indent..].split(' ') {
        let w = visible_width(word);
        if !fresh && cur_w + 1 + w > max {
            out.push(std::mem::replace(&mut cur, cont.clone()));
            cur_w = cont.len();
            fresh = true;
        }
        if !fresh {
            cur.push(' ');
            cur_w += 1;
        }
        cur.push_str(word);
        cur_w += w;
        fresh = false;
    }
    out.push(cur);
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
                    format!(
                        "{:.1}%",
                        crate::util::ratio(v.throttled as f64, v.count as f64) * 100.0
                    ),
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
                "throttled",
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

fn status_summary(status: &[(u16, u64)]) -> String {
    status
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

/// `p50/p99` of two values in ms.
fn p50_p99(p50: f64, p99: f64) -> String {
    format!("{}/{}", fmt_ms(p50), fmt_ms(p99))
}

/// Backend requests: compact columns by default (the app's: ttfb and total p50/p99, bytes
/// down/up, statuses), every stage's p50/p99 with `detail`.
pub fn apis_table(r: &Report, detail: bool) -> String {
    if detail {
        // Busiest first, as the compact table (`view::apis`).
        let mut apis: Vec<&ApiStats> = r.apis.iter().filter(|a| a.count > 0).collect();
        apis.sort_by_key(|a| std::cmp::Reverse(a.count));
        let rows: Vec<Vec<String>> = apis
            .into_iter()
            .map(|a| {
                let st = &a.stages;
                let q = |q: &crate::trace::Quantiles| p50_p99(q.p50, q.p99);
                let mut status: Vec<(u16, u64)> = a.status.iter().map(|(k, v)| (*k, *v)).collect();
                status.sort_by_key(|s| std::cmp::Reverse(s.1));
                vec![
                    a.api.label().to_owned(),
                    a.count.to_string(),
                    format!("{:.0}%", a.reused_pct),
                    q(&st.queue),
                    if st.connect.count > 0 {
                        q(&st.connect)
                    } else {
                        "-".into()
                    },
                    q(&st.send),
                    q(&st.ttfb),
                    q(&st.body),
                    q(&st.stall),
                    q(&st.total),
                    format!("{}/{}", fmt_bytes(a.bytes_down), fmt_bytes(a.bytes_up)),
                    status_summary(&status),
                ]
            })
            .collect();
        return table(
            &[
                "request", "count", "warm", "queue", "connect", "send", "ttfb", "transfer",
                "stall", "total", "down/up", "status",
            ],
            &rows,
            2,
        );
    }
    let rows: Vec<Vec<String>> = super::view::apis(r)
        .iter()
        .map(|a| {
            vec![
                a.api.clone(),
                a.requests_count.to_string(),
                format!("{:.0}%", a.reused_ratio * 100.0),
                p50_p99(a.ttfb_p50_ms, a.ttfb_p99_ms),
                p50_p99(a.total_p50_ms, a.total_p99_ms),
                format!("{}/{}", fmt_bytes(a.down_bytes), fmt_bytes(a.up_bytes)),
                status_summary(&a.status),
            ]
        })
        .collect();
    table(
        &[
            "request", "count", "warm", "ttfb", "total", "down/up", "status",
        ],
        &rows,
        2,
    )
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
        dim(&format!("· run {}", r.run.id))
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
            "{} · {:.1}s {} · {} · {}",
            r.workload_label(),
            r.run.window_s,
            if r.is_trace() { "recorded" } else { "measured" },
            if r.is_trace() {
                format!("{} requests", r.connections.requests)
            } else if let Some(rate) = r.workload.rate_per_s {
                format!("open loop {rate} ops/s")
            } else {
                format!("{} workers", r.spec.profile.workload.concurrency)
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
    let view = super::view::ProfileView::of(r, run);
    // Resilience: the app's Retries section in one line.
    kv(&mut out, "resilience", &{
        let line = view.retry.line();
        if view.retry.quiet() { dim(&line) } else { line }
    });
    // A corrupted object or too many failed operations (the run's checks).
    if !r.verdict.sla.breaches.is_empty() {
        kv(
            &mut out,
            "checks",
            &red(&format!("✖ {}", r.verdict.sla.breaches.join("; "))),
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

    if !view.findings.is_empty() {
        section(&mut out, "FINDINGS");
        for f in &view.findings {
            let _ = writeln!(
                out,
                "  {} {} {}",
                severity_icon(f.severity),
                bold(&f.cause),
                dim(&format!("[{}]", f.code))
            );
            for l in wrap_text(&f.evidence, WIDTH - 4) {
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
            for (i, l) in wrap_text(&format!("remedy: {}", f.remedy_line()), WIDTH - 6)
                .iter()
                .enumerate()
            {
                let _ = writeln!(out, "    {} {l}", if i == 0 { "→" } else { " " });
            }
            if let Some(next) = &f.next {
                let _ = writeln!(out, "    {} {next}", dim("next:"));
            }
        }
    }

    section(
        &mut out,
        "WHERE THE TIME GOES  (share of operation wall time)",
    );
    out.push_str(&share_bars(&view.time.stages));
    if !view.time.tail.is_empty() {
        let _ = writeln!(out, "  {}", dim("the slowest 1% of operations:"));
        out.push_str(&share_bars(&view.time.tail));
    }
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
        section(&mut out, "BACKEND REQUESTS  (p50/p99 per request)");
        out.push_str(&apis_table(r, detail));
        if let Some(w) = view.write_path.as_ref().filter(|w| w.shown()) {
            let _ = writeln!(out, "  {} {}", dim("writes"), w.line());
        }
        if let Some(lp) = &view.list_path {
            let _ = writeln!(out, "  {} {}", dim("lists"), lp.line());
        }
        if detail && r.connections.http2_requests > 0 {
            let _ = writeln!(
                out,
                "  {}",
                dim(
                    "h2: `send` ends when the body is handed to the stream; any flow-control stall sits inside ttfb"
                )
            );
        }
    }

    let c = &view.connections;
    if c.networked() {
        section(&mut out, "CONNECTIONS");
        let _ = writeln!(
            out,
            "  {}{}",
            c.protocol,
            c.proxy
                .as_ref()
                .map(|p| format!(" · via {p}"))
                .unwrap_or_default()
        );
        let _ = writeln!(
            out,
            "  {} · {:.1}% of requests warm · peak {} in flight{}",
            c.dials_line(),
            c.reuse_ratio * 100.0,
            c.peak_inflight_count,
            if c.dial_failures_count > 0 {
                red(&format!(" · {} failed dials", c.dial_failures_count))
            } else {
                String::new()
            }
        );
        if !c.phases.is_empty() {
            let _ = writeln!(out, "  dial p50/p99 {}", c.phases);
        }
        // The kernel's view: whether the network or the server is the limit.
        if let Some(t) = &c.tcp {
            let _ = writeln!(out, "  {} {}", dim("tcp"), t.line());
        }
        if detail {
            if let Some(t) = &c.tls_handshakes {
                let _ = writeln!(out, "  {} {}", dim("tls"), t.line());
            }
            if let Some(s) = &c.cold_start {
                let _ = writeln!(out, "  {} {}", dim("cold start"), s.line());
            }
        }
        let remote_addrs = &c.remote_addrs;
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
        } else if !remote_addrs.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                dim(&format!("remote {}", remote_addrs.join(", ")))
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
            "  wire rx {} · tx {}",
            fmt_bytes(b.wire_rx),
            fmt_bytes(b.wire_tx),
        );
    }
    if let Some(cv) = &view.cache {
        let _ = writeln!(out, "  cache {}", cv.line());
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

    if detail && !r.timeline.is_empty() {
        out.push_str(&timeline_section(&super::view::TimelineView::of(r)));
    }
    if detail {
        out.push_str(&spread_section(&super::view::SpreadView::of(r)));
    }

    if let Some(run) = run {
        let n = super::view::SLOWEST_COUNT;
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
        section(
            &mut out,
            "OPERATOR  (layers innermost first; ← the findings they remedy)",
        );
        let _ = writeln!(out, "  {}", view.operator.url);
        let layers: &[_] = if r.is_trace() {
            let _ = writeln!(
                out,
                "  {}",
                dim(
                    "layers and options not recorded: the trace saw the requests, not the application's operator"
                )
            );
            &[]
        } else {
            &view.operator.layers
        };
        for l in layers {
            let mark = if l.remedy_of.is_empty() {
                String::new()
            } else {
                yellow(&format!("  ← {}", l.remedy_of.join(", ")))
            };
            match &l.rust {
                Some(code) => {
                    let _ = writeln!(out, "  {} {code}{mark}", green("●"));
                }
                None => {
                    let _ = writeln!(out, "  {} {}{mark}", dim("○"), dim(&l.name));
                }
            }
        }
        let opts: Vec<String> = view
            .operator
            .options
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .filter(|_| !r.is_trace())
            .collect();
        for l in wrap_text(&opts.join(" · "), WIDTH - 4) {
            let _ = writeln!(out, "  {}", dim(&l));
        }
        section(&mut out, "CONFIGURATION");
        let w = &r.workload;
        if r.is_trace() {
            let _ = writeln!(
                out,
                "  workload   {} · the application's own traffic (oprobe drove nothing; written by oprobe-trace {})",
                r.workload_label(),
                r.run.oprobe_version
            );
        } else {
            let _ = writeln!(
                out,
                "  workload   {} · {} objects × {} · access {} · seed {}",
                w.preset,
                w.dataset.objects_count,
                fmt_bytes(w.dataset.object_size_bytes),
                w.access,
                w.seed
            );
        }
        if let Some(w) = &view.write_path {
            let _ = writeln!(out, "  writes     {}", w.line());
        }
        for (k, v) in &r.target.options {
            let _ = writeln!(out, "  option     {k}={v}");
        }
        let h = &r.run.host;
        if !h.os.is_empty() {
            let _ = writeln!(
                out,
                "  host       {}/{} · {} CPUs · {}",
                h.os, h.arch, h.cpus_count, h.hostname_hash
            );
        }
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

    // A production trace wrote nothing: there is no cleanup to report.
    if !r.is_trace() {
        cleanup_section(&mut out, &r.cleanup);
    }

    if !r.run.id.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            dim(&format!(
                "more: oprobe show {0} --detail · oprobe show {0} --by stage|api|conn",
                crate::model::spec::shell_quote(r.show_ref())
            ))
        );
    }
    if detail {
        out
    } else {
        out.lines()
            .flat_map(|l| wrap_line(l, WIDTH))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }
}

/// Rows the detailed timeline shows at most: a long run is merged into fewer, longer rows.
pub const TIMELINE_ROWS: usize = 60;

/// `1 s`, `10 s`, `2 min`.
fn interval_label(s: f64) -> String {
    if s >= 60.0 && (s / 60.0).fract().abs() < 1e-6 {
        format!("{:.0} min", s / 60.0)
    } else if s.fract().abs() < 1e-6 {
        format!("{s:.0} s")
    } else {
        format!("{s:.1} s")
    }
}

/// A count of a series the timeline may not carry (`-` when it does not).
fn cell_u64(v: &[u64], i: usize) -> String {
    v.get(i).map_or("-".to_owned(), u64::to_string)
}

/// The TIMELINE section of `show --detail`: one row per interval (its real length), at most
/// [`TIMELINE_ROWS`] rows.
pub fn timeline_section(t: &super::view::TimelineView) -> String {
    let mut out = String::new();
    if t.is_empty() {
        return out;
    }
    let (rows_v, merged) = t.downsample(TIMELINE_ROWS);
    // The interval length of the rows (the last one may be shorter: the window's end).
    let mut lens: Vec<f64> = t.len_s.clone();
    lens.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    let base = interval_label(lens.first().copied().unwrap_or(1.0));
    let title = if merged > 1 {
        format!(
            "TIMELINE  ({} rows of {merged} intervals of {base}; p99, p99.9 and max: the worst interval)",
            rows_v.len()
        )
    } else if lens.len() > 1 {
        format!(
            "TIMELINE  (intervals of {base}, then {})",
            interval_label(lens[1])
        )
    } else {
        format!("TIMELINE  (intervals of {base})")
    };
    section(&mut out, &title);
    let rows: Vec<Vec<String>> = (0..rows_v.len())
        .map(|i| {
            vec![
                format!("{:.0}s", rows_v.t_s[i]),
                format!("{:.1}", rows_v.ops_per_s[i]),
                format!("{:.1}", rows_v.mib_per_s[i]),
                fmt_ms(rows_v.p50_ms[i]),
                fmt_ms(rows_v.p99_ms[i]),
                fmt_ms(rows_v.p999_ms[i]),
                fmt_ms(rows_v.max_ms[i]),
                rows_v.errors_count[i].to_string(),
                rows_v.throttled_count[i].to_string(),
                cell_u64(&rows_v.retries_count, i),
                cell_u64(&rows_v.new_connections_count, i),
                rows_v
                    .cpu_pct
                    .get(i)
                    .copied()
                    .flatten()
                    .map_or("-".to_owned(), |c| format!("{:.0}%", c)),
            ]
        })
        .collect();
    out.push_str(&table(
        &[
            "t",
            "ops/s",
            "MiB/s",
            "p50",
            "p99",
            "p99.9",
            "max",
            "errors",
            "throttled",
            "retries",
            "new conns",
            "cpu",
        ],
        &rows,
        2,
    ));
    let _ = writeln!(
        out,
        "  {}",
        dim(
            "errors, throttled, retries, new conns: counts in each row, not rates · cpu: % of one core"
        )
    );
    out
}

/// The SPREAD section of `show --detail`: where the traffic went (hot keys, object sizes,
/// prefixes, remotes).
pub fn spread_section(s: &super::view::SpreadView) -> String {
    let mut out = String::new();
    if s.is_empty() {
        return out;
    }
    section(&mut out, "SPREAD  (where the requests went)");
    let pct = |x: Option<f64>| x.map_or("-".to_owned(), |v| format!("{:.1}%", v * 100.0));
    let ms = |x: Option<f64>| x.map_or("-".to_owned(), fmt_ms);
    let groups: [(&str, &str, &[super::view::Slice]); 5] = [
        ("hot key", "of throttling", &s.hot_keys),
        ("object size", "throttled", &s.sizes),
        ("prefix", "throttled", &s.prefixes),
        ("first read / re-read", "throttled", &s.cold_warm),
        ("remote", "throttled", &s.remotes),
    ];
    for (title, thr, slices) in groups {
        if slices.is_empty() {
            continue;
        }
        // Columns no slice fills (a hot key's errors and p99) are left out.
        let (has_thr, has_err, has_p99) = super::view::SpreadView::columns(slices);
        let rows: Vec<Vec<String>> = slices
            .iter()
            .map(|x| {
                let mut row = vec![
                    x.label.clone(),
                    x.requests_count.to_string(),
                    format!("{:.1}%", x.share_ratio * 100.0),
                ];
                if has_thr {
                    // Hot keys carry their share of all throttling, the others their own.
                    row.push(pct(x.throttled_ratio.or(x.throttle_share_ratio)));
                }
                if has_err {
                    row.push(pct(x.error_ratio));
                }
                if has_p99 {
                    row.push(ms(x.p99_ms));
                }
                row
            })
            .collect();
        let mut head = vec![title, "requests", "share"];
        if has_thr {
            head.push(thr);
        }
        if has_err {
            head.push("errors");
        }
        if has_p99 {
            head.push("p99");
        }
        out.push_str(&table(&head, &rows, 2));
    }
    let _ = writeln!(
        out,
        "  {}",
        dim(
            "share: of all requests · hot keys: share of all throttling · others: share of the slice's own requests"
        )
    );
    out
}

fn cleanup_section(out: &mut String, cl: &crate::engine::CleanupSummary) {
    section(out, "CLEANUP");
    if !cl.enabled {
        let _ = writeln!(
            out,
            "  {} test data kept under {} (cleanup was off)",
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
            "  {} {} objects remain under {}{} — remove them with your storage tools",
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

/// `2.0–6.0 s`, with the digits a short range needs to show its length (`0.000–0.001 s`,
/// not `0.0–0.0 s`): one decimal, and as many more as the length's first digit lies after.
pub fn range_span(since_s: f64, until_s: f64) -> String {
    let span = until_s - since_s;
    let decimals = if span > 0.0 {
        ((-span.log10() - 1e-9).ceil() as usize).clamp(1, 6)
    } else {
        6
    };
    format!("{since_s:.decimals$}–{until_s:.decimals$} s")
}

/// `show --since/--until`: one time range of a run, recomputed from its trace rows
/// ([`super::range::RangeView`]): the numbers, where the time went, the verbs, the requests by
/// API and the slowest operations of the range.
pub fn range_section(run: &Run, v: &super::range::RangeView) -> String {
    range_section_with(run, v, false)
}

pub fn range_section_with(run: &Run, v: &super::range::RangeView, detail: bool) -> String {
    let r = &run.report;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {} {}",
        bold("oprobe"),
        dim(&r.run.oprobe_version),
        dim(&format!("· run {}", r.run.id))
    );
    let _ = writeln!(
        out,
        "{} {}",
        bold(&r.target.display),
        dim(&format!(
            "· {} of the {:.1} s window",
            range_span(v.since_s, v.until_s),
            r.run.window_s
        ))
    );
    if v.sampled {
        let _ = writeln!(
            out,
            "{}",
            yellow(&format!(
                "rows were sampled ({:.0}% kept): counts are estimates, each row weighted by the operations it stands for",
                v.kept_ratio * 100.0
            ))
        );
    }
    if v.is_empty() {
        let _ = writeln!(out, "\n  no operation started in this range");
        if v.missed_arrivals_count > 0 {
            let _ = writeln!(
                out,
                "  {} open-loop arrivals due in it never started",
                v.missed_arrivals_count
            );
        }
        return out;
    }
    section(&mut out, "Range");
    kv(
        &mut out,
        "goodput",
        &format!(
            "{:.1} ops/s · {:.1} attempted · {:.2} MiB/s",
            v.goodput_ops_per_s, v.ops_per_s, v.mib_per_s
        ),
    );
    // With arrivals that never started the percentiles are lower bounds, as the whole
    // run's are: "at least".
    let at_least = if v.latency_at_least { "≥ " } else { "" };
    kv(
        &mut out,
        "latency",
        &format!(
            "p50 {at_least}{} · p90 {at_least}{} · p99 {at_least}{} · p99.9 {at_least}{} · max {}",
            fmt_ms(v.p50_ms),
            fmt_ms(v.p90_ms),
            fmt_ms(v.p99_ms),
            fmt_ms(v.p999_ms),
            fmt_ms(v.max_ms)
        ),
    );
    kv(
        &mut out,
        "operations",
        &format!(
            "{} · {} failed ({:.2}%)",
            v.ops_count,
            v.failed_ops_count,
            v.error_ratio * 100.0
        ),
    );
    if v.missed_arrivals_count > 0 {
        kv(
            &mut out,
            "missed",
            &format!(
                "{} open-loop arrivals due in this range never started: the percentiles count each at the least it waited, so they are \"at least\" values (max is of the operations that started)",
                v.missed_arrivals_count
            ),
        );
    }
    kv(
        &mut out,
        "requests",
        &format!(
            "{} · {} retries · {} throttled ({:.2}%)",
            v.requests_count,
            v.retries_count,
            v.throttled_requests_count,
            v.throttled_request_ratio * 100.0
        ),
    );
    if !v.stages.is_empty() {
        section(&mut out, "Where the time went");
        out += &share_bars(&v.stages);
    }
    if v.verbs.len() > 1 {
        section(&mut out, "Verbs");
        let rows: Vec<Vec<String>> = v
            .verbs
            .iter()
            .map(|x| {
                vec![
                    x.verb.clone(),
                    x.ops_count.to_string(),
                    format!("{:.1}", x.ops_per_s),
                    fmt_ms(x.p50_ms),
                    fmt_ms(x.p99_ms),
                    format!("{:.2}%", x.error_ratio * 100.0),
                ]
            })
            .collect();
        out += &table(&["verb", "ops", "ops/s", "p50", "p99", "errors"], &rows, 2);
    }
    if !v.apis.is_empty() {
        section(&mut out, "Requests by API");
        let rows: Vec<Vec<String>> = v
            .apis
            .iter()
            .map(|a| {
                vec![
                    a.api.clone(),
                    a.requests_count.to_string(),
                    a.status
                        .iter()
                        .take(3)
                        .map(|(c, n)| format!("{c}×{n}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    format!("{}/{}", fmt_ms(a.ttfb_p50_ms), fmt_ms(a.ttfb_p99_ms)),
                    fmt_ms(a.total_p99_ms),
                    a.throttled_count.to_string(),
                ]
            })
            .collect();
        out += &table(
            &[
                "api",
                "requests",
                "status",
                "ttfb p50/p99",
                "p99",
                "throttled",
            ],
            &rows,
            2,
        );
    }
    let slow: Vec<&OpRow> = v
        .slowest_op_ids
        .iter()
        .filter_map(|id| run.op(*id))
        .collect();
    if !slow.is_empty() {
        section(&mut out, "Slowest operations in the range");
        let n = if detail {
            slow.len()
        } else {
            super::view::SLOWEST_COUNT
        };
        for op in slow.iter().take(n) {
            out += &slow_op_block(op, &run.requests_of(op.op_id), detail);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_spread_shows_a_hot_keys_share_of_throttling() {
        use super::super::view::{Slice, SpreadView};
        let s = SpreadView {
            hot_keys: vec![Slice {
                key: "hot/a".into(),
                label: "hot/a".into(),
                requests_count: 120,
                share_ratio: 0.4,
                throttled_ratio: None,
                throttle_share_ratio: Some(0.75),
                error_ratio: None,
                p50_ms: None,
                p99_ms: None,
            }],
            ..Default::default()
        };
        let out = spread_section(&s);
        assert!(out.contains("75.0%"), "{out}");
        // A hot key has no errors and no p99: those columns are left out.
        assert!(!out.contains("errors") && !out.contains("p99"), "{out}");
    }

    use super::*;

    /// `show --until 1ms` printed "0.0–0.0 s of the 10.0 s window": a short range reads
    /// with the digits it needs.
    #[test]
    fn a_short_range_reads_as_a_range() {
        assert_eq!(range_span(2.0, 6.0), "2.0–6.0 s");
        assert_eq!(range_span(0.1, 0.2), "0.1–0.2 s");
        assert_eq!(range_span(0.0, 0.001), "0.000–0.001 s");
        assert_eq!(range_span(0.0, 0.05), "0.00–0.05 s");
        assert_eq!(range_span(1.25, 1.5), "1.2–1.5 s");
        assert_eq!(range_span(0.0, 0.00000001), "0.000000–0.000000 s");
        set(false);
        let run = super::super::fixture::run();
        let v = super::super::range::RangeView::of(&run, 0.0, 0.001);
        let text = range_section(&run, &v);
        assert!(
            text.contains("0.000–0.001 s of the 10.0 s window"),
            "{text}"
        );
    }

    #[test]
    fn full_report_fits_80_columns_and_detail_adds_sections() {
        set(false);
        let mut run = super::super::fixture::run();
        run.report.findings[0].remedy.text = "word ".repeat(60);
        run.report.target.display = format!("s3://{}", "b".repeat(120));
        let text = render_run(&run);
        // Lines wrap at 80 columns; nothing is cut, and a word longer than that stays whole.
        assert!(!text.lines().any(|l| l.ends_with('…')), "{text}");
        assert!(text.contains(&run.report.target.display), "{text}");
        for l in text.lines() {
            assert!(
                visible_width(l) <= WIDTH || !l.trim().contains(' '),
                "{} > 80: {l}",
                visible_width(l)
            );
        }
        for s in [
            "KEY NUMBERS",
            "FINDINGS",
            "WHERE THE TIME GOES",
            "OPERATIONS",
            "SLOWEST OPERATION",
            "CLEANUP",
            "next: oprobe prove 20261120-143005-a1b2 throttling",
            "remedy: layers.concurrent_limit.http",
            "--detail",
        ] {
            assert!(text.contains(s), "{s}\n{text}");
        }
        run.report.dimensions.size_buckets = vec![crate::model::DimensionSlice {
            key: "64KiB–1MiB".into(),
            requests_count: 1100,
            share_ratio: 1.0,
            throttled_ratio: 0.01,
            p99_ms: 110.0,
            ..Default::default()
        }];
        let detail = render_run_with(&run, true);
        for s in [
            "TIMELINE  (intervals of 1 s)",
            "p99.9",
            "counts in each row",
            "SPREAD",
            "64KiB–1MiB",
            "CONFIGURATION",
            "OPERATOR",
            "SLOWEST OPERATIONS",
        ] {
            assert!(detail.contains(s), "{s}\n{detail}");
        }
        // No SLA line, no label.
        assert!(!text.contains("sla") && !detail.contains("sla "), "{text}");
    }

    #[test]
    fn the_report_shows_resilience_connections_writes_and_request_signals() {
        set(false);
        let mut run = super::super::fixture::run();
        let r = &mut run.report;
        r.resilience.retries = 3;
        r.resilience.status_503 = 12;
        r.connections.requests = 1100;
        r.connections.http1_requests = 1100;
        r.network.tcp.kernel = Some(crate::net::stats::TcpKernelStats {
            samples_count: 10,
            connections_count: 4,
            rtt_p50_us: 1_000,
            rtt_p99_us: 2_000,
            ..Default::default()
        });
        r.network.tls.handshakes.handshakes_count = 4;
        r.bytes.logical_write = 1 << 20;
        r.effective_write.write_chunk_size = 8 << 20;
        r.apis = vec![crate::trace::ApiStats {
            api: crate::trace::Api::GetObject,
            count: 1100,
            reused_pct: 99.0,
            bytes_down: 64 << 20,
            status: [(200, 1088), (503, 12)].into_iter().collect(),
            stages: crate::trace::StageStats {
                ttfb: crate::trace::Quantiles {
                    count: 1100,
                    p50: 3.0,
                    p99: 12.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }];
        // Written in parts: the writes line says the part size and the parts.
        r.apis.push(crate::trace::ApiStats {
            api: crate::trace::Api::UploadPart,
            count: 2,
            bytes_up: 16 << 20,
            ..Default::default()
        });
        r.connections.dialed = 1;
        r.connections.cold_start = Some(crate::trace::ColdStartStats {
            connections_count: 28,
            ..Default::default()
        });
        run.reqs[2].detail = Some(Box::new(crate::model::ReqDetail {
            aborted: true,
            region: Some("eu-west-1".into()),
            ..Default::default()
        }));
        run.report.slowest = vec![2];
        let text = render_run(&run);
        // Lines wrap at 80 columns: compare words.
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for s in [
            "resilience 3 retries",
            "503 ×12",
            "BACKEND REQUESTS (p50/p99 per request)",
            "3.00ms/12.0ms",
            "64.0 MiB/0 B",
            "writes part size 8.0 MiB",
            "tcp smoothed rtt p50/p99 1.00ms/2.00ms",
            "2 parts of 8.0 MiB on average",
            // One count of dials, with its window (K18).
            "28 dialed during warm-up · 1 in the window",
            "body incomplete",
        ] {
            assert!(words.contains(s), "{s}\n{text}");
        }
        // The long tail waits for --detail: the region of a request, the TLS handshakes.
        assert!(!text.contains("region eu-west-1"), "{text}");
        assert!(!text.contains("handshakes"), "{text}");
        let detail = render_run_with(&run, true);
        for s in [
            "region eu-west-1",
            "tls 4 handshakes",
            "writes     part size 8.0 MiB",
            "new conns",
            "throttled",
        ] {
            assert!(detail.contains(s), "{s}\n{detail}");
        }
    }

    #[test]
    fn long_lines_wrap_and_are_never_cut() {
        let s = format!("  \x1b[31m{}\x1b[0m tail", "x".repeat(100));
        let w = wrap_line(&s, 10);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].starts_with("  \x1b[31m"));
        assert_eq!(visible_width(&w[0]), 102);
        assert_eq!(w[1], "    tail");
        let cmd = "  more: oprobe show 20261120-143005-a1b2 --detail · oprobe show 20261120-143005-a1b2 --by stage|api|conn";
        let w = wrap_line(cmd, 40);
        assert!(w.iter().all(|l| visible_width(l) <= 40), "{w:?}");
        assert_eq!(
            w.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" "),
            cmd.trim()
        );
        assert_eq!(wrap_text("a bb ccc dddd", 6), vec!["a bb", "ccc", "dddd"]);
    }
}
