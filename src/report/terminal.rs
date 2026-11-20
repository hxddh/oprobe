//! Human-oriented terminal rendering of reports.

use std::fmt::Write as _;

use super::Report;
use super::style::*;
use crate::analysis::diagnosis::Severity;
use crate::trace::{Protocol, RequestRecord, SlowOp, Snapshot};
use crate::util::{fmt_bytes, fmt_ms, fmt_usd};

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
    let sh = &r.diagnosis.shares;
    let mut out = String::new();
    let rows = [
        (
            "queue",
            sh.queue,
            "waiting for in-flight permits / connections",
        ),
        (
            "connect",
            sh.connect,
            "DNS + TCP + proxy + TLS on fresh connections",
        ),
        ("upload", sh.upload, "sending request bodies"),
        (
            if r.target.endpoint.is_some() {
                "server"
            } else {
                "backend"
            },
            sh.server,
            "time to first byte (server processing + RTT)",
        ),
        ("download", sh.download, "receiving response bodies"),
        ("retry", sh.retry, "backoff sleeps between retry attempts"),
        ("cache", sh.cache, "local cache lookups"),
        ("client", sh.client, "OpenDAL / signing / CRC / consumer"),
    ];
    let max = rows.iter().map(|r| r.1).fold(0.0, f64::max).max(1e-9);
    for (name, v, desc) in rows {
        if v < 0.0005 && name != "server" {
            continue;
        }
        let b = bar(v / max, 28);
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
        format!("failed ops ≤ {}%", sla.max_error_rate * 100.0),
        "no corrupted data".to_owned(),
    ];
    if let Some(t) = sla.max_throttle_rate {
        terms.push(format!("throttled ops ≤ {}%", t * 100.0));
    }
    if let Some(p) = sla.p99_ms {
        terms.push(format!("p99 ≤ {}", fmt_ms(p)));
    }
    let v = &r.sla_verdict;
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

pub fn request_line(i: usize, r: &RequestRecord) -> String {
    let t = &r.timing;
    let ms = |us: u64| fmt_ms(us as f64 / 1000.0);
    let status = if r.status == 0 {
        red("ERR")
    } else if r.status >= 400 {
        red(&r.status.to_string())
    } else {
        green(&r.status.to_string())
    };
    let conn = match r.protocol {
        Protocol::InProcess => "in-process".to_owned(),
        p => format!(
            "{} conn#{} {}",
            if p == Protocol::Http2 { "h2" } else { "h1" },
            r.conn_id,
            if r.reused { "warm" } else { "cold" }
        ),
    };
    let mut parts = Vec::new();
    if t.queue_us > 0 {
        parts.push(format!("queue {}", ms(t.queue_us)));
    }
    if !r.reused && r.protocol != Protocol::InProcess {
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
    if r.protocol != Protocol::InProcess {
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
        fmt_ms(r.start_offset_us as f64 / 1000.0),
        r.api.label(),
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

pub fn slow_op_block(op: &SlowOp, show_tree: bool) -> String {
    let mut out = String::new();
    let b = &op.breakdown;
    let _ = writeln!(
        out,
        "  {} {} at {:.1}s  {}",
        bold(&op.verb),
        bold(&fmt_ms(op.latency_ms)),
        op.at_s,
        dim(&format!(
            "queue {} · backend {} · retry {} · client {}{}",
            fmt_ms(b.queue_ms),
            fmt_ms(b.backend_ms),
            fmt_ms(b.retry_ms),
            fmt_ms(b.client_ms + b.cache_ms),
            if op.retries > 0 {
                format!(" · {} retries", op.retries)
            } else {
                String::new()
            }
        ))
    );
    if let Some(e) = &op.error {
        let _ = writeln!(out, "    {}", red(&format!("failed: {e}")));
    }
    for (i, r) in op.requests.iter().take(12).enumerate() {
        let _ = writeln!(out, "{}", request_line(i, r));
    }
    if op.requests.len() > 12 {
        let _ = writeln!(
            out,
            "    {}",
            dim(&format!("… {} more requests", op.requests.len() - 12))
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

pub fn verbs_table(s: &Snapshot) -> String {
    let rows: Vec<Vec<String>> = s
        .verbs
        .iter()
        .map(|v| {
            vec![
                v.verb.clone(),
                v.count.to_string(),
                format!("{:.1}", v.ops_per_s),
                format!("{:.1}", v.mib_per_s),
                fmt_ms(v.latency.p50),
                fmt_ms(v.latency.p90),
                fmt_ms(v.latency.p99),
                fmt_ms(v.latency.max),
                format!("{:.2}", v.requests_per_op),
                if v.errors > 0 {
                    red(&v.errors.to_string())
                } else {
                    "0".into()
                },
            ]
        })
        .collect();
    table(
        &[
            "operation",
            "count",
            "ops/s",
            "MiB/s",
            "p50",
            "p90",
            "p99",
            "max",
            "req/op",
            "errors",
        ],
        &rows,
        2,
    )
}

pub fn apis_table(s: &Snapshot) -> String {
    let rows: Vec<Vec<String>> = s
        .apis
        .iter()
        .map(|a| {
            let st = &a.stages;
            let status: Vec<String> = a
                .status
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
                .collect();
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
                status.join(" "),
            ]
        })
        .collect();
    table(
        &[
            "request", "count", "warm", "queue", "connect", "send", "ttfb", "transfer", "stall",
            "p99", "status",
        ],
        &rows,
        2,
    )
}

pub fn render(r: &Report) -> String {
    let s = &r.result;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {} {} {}",
        bold("oprobe"),
        dim(&r.oprobe_version),
        bold(&r.target.display),
        dim(&format!(
            "· {} · {:.1}s measured · {} · client {}",
            r.workload.preset.slug(),
            s.elapsed_s,
            if let Some(rate) = r.workload.rate {
                format!("open-loop {rate} ops/s")
            } else {
                format!("{} workers", r.knobs.concurrency)
            },
            match r.knobs.transport {
                crate::config::TransportMode::Opendal => "OpenDAL reqwest",
                crate::config::TransportMode::Microscope => "oprobe microscope",
            }
        ))
    );
    if r.interrupted {
        let _ = writeln!(
            out,
            "{} {}",
            badge("INTERRUPTED", "43;30"),
            yellow("run stopped early: results cover a shorter window than configured")
        );
    }
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
    for w in &r.warnings {
        let _ = writeln!(out, "{} {}", yellow("warning:"), w);
    }

    let worst = r.diagnosis.worst();
    let verdict = match worst {
        Severity::Critical => badge("ATTENTION", "41"),
        Severity::Warning => badge("REVIEW", "43;30"),
        Severity::Info => badge("HEALTHY", "42;30"),
    };
    let _ = writeln!(out, "\n{verdict} {}", bold(&r.diagnosis.headline));

    let sm = &s.summary;
    let _ = writeln!(
        out,
        "\n  {} {:.1} ops/s · {:.1} MiB/s {}   {} p50 {} · p99 {} · p99.9 {}   {} {} · {}",
        dim("throughput"),
        sm.ops_per_s,
        sm.mib_per_s,
        dim(&format!(
            "(read {:.1} / write {:.1})",
            sm.read_mib_per_s, sm.write_mib_per_s
        )),
        dim("latency"),
        fmt_ms(sm.latency.p50),
        fmt_ms(sm.latency.p99),
        fmt_ms(sm.latency.p999),
        dim("errors"),
        if sm.failed_ops > 0 {
            red(&sm.failed_ops.to_string())
        } else {
            "0".into()
        },
        if s.resilience.throttled_responses > 0 {
            red(&format!("{} throttled", s.resilience.throttled_responses))
        } else {
            "0 throttled".into()
        },
    );
    let _ = writeln!(out, "  {} {}", dim("sla       "), sla_line(r));
    if let Some(ol) = &s.open_loop {
        let _ = writeln!(
            out,
            "  {} target {:.0} ops/s · achieved {:.0} · missed {} · max schedule lag {}",
            dim("open loop"),
            ol.target_rate,
            ol.achieved_rate,
            ol.missed,
            fmt_ms(ol.max_schedule_lag_ms)
        );
    }

    section(
        &mut out,
        "WHERE THE TIME GOES  (share of operation wall time)",
    );
    out.push_str(&time_share_lines(r));

    section(&mut out, "OPERATIONS");
    out.push_str(&verbs_table(s));

    if !s.apis.is_empty() {
        section(
            &mut out,
            "BACKEND REQUESTS  (mean per request; p99 of total)",
        );
        out.push_str(&apis_table(s));
        if s.connections.http2_requests > 0 {
            let _ = writeln!(
                out,
                "  {}",
                dim(
                    "h2: `send` ends when the body is handed to the stream; flow-controlled transmission shows up in ttfb"
                )
            );
        }
    }

    let c = &s.connections;
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
            "  {} new in window · {:.1}% of requests on warm connections · peak {} in-flight requests{}",
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
            let _ = writeln!(out, "  dial p50/p99   {phases}");
        }
        if !c.remote_addrs.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                dim(&format!("remote {}", c.remote_addrs.join(", ")))
            );
        }
    }

    section(&mut out, "BYTES & AMPLIFICATION");
    let b = &s.bytes;
    let _ = writeln!(
        out,
        "  logical read {} · write {} · backend down {} · up {} · cache hits {}",
        fmt_bytes(b.logical_read),
        fmt_bytes(b.logical_write),
        fmt_bytes(b.backend_down),
        fmt_bytes(b.backend_up),
        fmt_bytes(b.cache_hit_bytes)
    );
    let _ = writeln!(
        out,
        "  read amplification {:.2}× · over-read {:.2}× · {:.2} requests/op{}",
        b.read_amplification,
        b.overread_ratio,
        b.request_amplification,
        // Socket-level bytes are only visible to the microscope transport.
        if b.wire_rx + b.wire_tx > 0 {
            format!(
                " · wire rx {} tx {}",
                fmt_bytes(b.wire_rx),
                fmt_bytes(b.wire_tx)
            )
        } else {
            String::new()
        }
    );
    if s.cache.enabled {
        let _ = writeln!(
            out,
            "  cache {:.1}% hit ({} of {} lookups, mean lookup {})",
            s.cache.hit_ratio * 100.0,
            s.cache.hits,
            s.cache.lookups,
            fmt_ms(s.cache.lookup_ms_mean)
        );
    }
    if s.integrity.verified + s.integrity.corrupted > 0 {
        let _ = writeln!(
            out,
            "  integrity {} {} verified{}",
            if s.integrity.corrupted == 0 {
                green("✓")
            } else {
                red("✖")
            },
            s.integrity.verified,
            if s.integrity.corrupted > 0 {
                red(&format!(", {} CORRUPTED", s.integrity.corrupted))
            } else {
                String::new()
            }
        );
    }

    if r.cost.applicable && s.summary.ops > 0 {
        let cst = &r.cost;
        section(
            &mut out,
            &format!(
                "COST  ({} at {:.0}M ops/month, egress {:.0}%)",
                cst.selected.provider,
                cst.monthly_ops / 1e6,
                cst.egress_fraction * 100.0
            ),
        );
        let sel = &cst.selected;
        let _ = writeln!(
            out,
            "  {} per 1M ops · requests {} · egress {} · storage {} → {} /month",
            bold(&fmt_usd(sel.per_million_ops_usd)),
            fmt_usd(sel.requests_usd),
            fmt_usd(sel.egress_usd),
            fmt_usd(sel.storage_usd),
            bold(&fmt_usd(sel.total_usd))
        );
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
        if sel.failed_requests_usd > 0.005
            || sel.overread_usd > 0.005
            || sel.cache_savings_usd > 0.005
        {
            let _ = writeln!(
                out,
                "  waste: failed/throttled requests {} · over-read egress {} · cache saved {}",
                fmt_usd(sel.failed_requests_usd),
                fmt_usd(sel.overread_usd),
                fmt_usd(sel.cache_savings_usd)
            );
        }
        let mut cmp: Vec<_> = cst.comparison.iter().collect();
        cmp.sort_by(|a, b| a.total_usd.total_cmp(&b.total_usd));
        let line: Vec<String> = cmp
            .iter()
            .map(|c| format!("{} {}", c.key, fmt_usd(c.total_usd)))
            .collect();
        let _ = writeln!(
            out,
            "  {}",
            dim(&format!("same workload elsewhere: {}", line.join(" · ")))
        );
    }

    if !r.diagnosis.findings.is_empty() {
        section(&mut out, "FINDINGS");
        for f in &r.diagnosis.findings {
            let _ = writeln!(out, "  {} {}", severity_icon(f.severity), bold(&f.title));
            let _ = writeln!(out, "    {}", dim(&f.evidence));
            let _ = writeln!(out, "    → {}", f.action);
        }
    }

    if let Some(op) = s.slow_ops.first() {
        section(&mut out, "SLOWEST OPERATION");
        out.push_str(&slow_op_block(op, true));
    }

    section(&mut out, "CLEANUP");
    let cl = &r.cleanup;
    if !cl.enabled {
        let _ = writeln!(
            out,
            "  {} test data kept under {} (remove later with `oprobe cleanup`)",
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
            dim(&format!("({:.0} ms, verified empty)", cl.duration_ms))
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
    out
}
