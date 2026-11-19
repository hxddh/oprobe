//! Crisp, ANSI-aware terminal report renderer for CLI output (`oprobe run`, `tune`, `diff`, `net-probe`, errors).

use std::io::IsTerminal;

use crate::net_probe::EndpointHandshakeSummary;
use crate::telemetry::{ActionableErrorHint, Severity};

use super::diff::DiffSummary;
use super::BenchmarkReport;

fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn style(text: &str, ansi_code: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{ansi_code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn bar_meter(pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0) * width as f64).round().clamp(0.0, width as f64) as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Render a white-box [`BenchmarkReport`] as an ergonomic, scannable CLI terminal summary.
pub fn render_report_terminal(rep: &BenchmarkReport) -> String {
    let c = colors_enabled();
    let mut out = String::with_capacity(4096);

    let sev_ansi = match rep.frame.diagnosis.severity {
        Severity::Healthy => "1;32",
        Severity::Advisory => "1;36",
        Severity::Warning => "1;33",
        Severity::Critical => "1;31",
    };

    out.push_str(&format!(
        "{} {} ({}) · {}\n",
        style("● oprobe v1.0.0", "1;36", c),
        style(&rep.profile.target.name, "1", c),
        rep.profile.target.scheme,
        rep.profile.workload.kind.title()
    ));
    out.push_str(&format!(
        "  [{}] {}\n  ↳ {}\n",
        style(rep.frame.diagnosis.severity.badge(), sev_ansi, c),
        style(&rep.frame.diagnosis.headline, "1", c),
        rep.frame.diagnosis.recommendation
    ));
    out.push_str(&format!(
        "  Knobs: conc={} | chunk={} KiB | gap={} KiB | cache={} | tail_cut={} | keep_alive={}\n\n",
        rep.active_knob.concurrent,
        rep.active_knob.chunk_bytes / 1024,
        rep.active_knob.range_gap_bytes / 1024,
        if rep.active_knob.enable_foyer { "on" } else { "off" },
        if rep.active_knob.enable_tail_cut { "on" } else { "off" },
        if rep.active_knob.keep_alive { "on" } else { "off" },
    ));

    // 1. 7-Stage Waterfall
    out.push_str(&format!(
        "{} (mean {:.3} ms)\n",
        style("1. Seven-Stage White-Box Latency Waterfall", "1;36", c),
        rep.frame.waterfall.total_ms()
    ));
    for (stage, ms, pct) in rep.frame.waterfall.breakdown_pct() {
        out.push_str(&format!(
            "  {:<28} {:>8.3} ms ({:>5.1}%)  {}\n",
            stage,
            ms,
            pct,
            bar_meter(pct, 24)
        ));
    }
    if rep.frame.transport.total_http_requests > 0 {
        let wf = &rep.frame.waterfall;
        out.push_str(&format!(
            "  └─ Stage 3 Wire Split: DNS {:.3} ms | TCP {:.3} ms | TLS {:.3} ms | Pool/Write {:.3} ms\n",
            wf.dns_lookup_us as f64 / 1000.0,
            wf.tcp_connect_us as f64 / 1000.0,
            wf.tls_handshake_us as f64 / 1000.0,
            wf.pool_checkout_us as f64 / 1000.0,
        ));
    }
    out.push('\n');

    // 2. Per-Verb Quantiles
    out.push_str(&format!(
        "{}\n",
        style("2. Per-Verb Isolated HDR Latency Quantiles", "1;36", c)
    ));
    out.push_str(&format!(
        "  {:<24} {:>7} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>8}\n",
        "VERB", "COUNT", "OPS/s", "P50(ms)", "P95(ms)", "P99(ms)", "MAX(ms)", "MiB/s", "429/503"
    ));
    out.push_str(&format!("  {}\n", "─".repeat(102)));
    for v in &rep.frame.verbs {
        out.push_str(&format!(
            "  {:<24} {:>7} {:>9.1} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>10.2} {:>7.1}%\n",
            v.verb,
            v.count,
            v.ops_per_sec,
            v.p50_ms,
            v.p95_ms,
            v.p99_ms,
            v.max_ms,
            v.throughput_mib_s,
            v.throttle_429_503_pct
        ));
    }
    out.push('\n');

    // 3. Transport, Amplification & Cleanup
    let tp = &rep.frame.transport;
    out.push_str(&format!(
        "{}\n",
        style("3. Transport Efficiency, Amplification & Cleanup", "1;36", c)
    ));
    out.push_str(&format!(
        "  Throughput : {:.2} MiB/s logical ({:.2} MiB/s wire | {:.2} MiB/s/core across {} cores | RSS {} MiB)\n",
        tp.aggregate_mib_s, tp.wire_mib_s, tp.mib_per_cpu_core, tp.cpu_cores, tp.process_rss_mib
    ));
    out.push_str(&format!(
        "  Connections: {:.1}% socket reuse ({} warm / {} cold) | ALPN={} TLS={} | Peak Inflight={}\n",
        tp.socket_reuse_pct,
        tp.reused_connections,
        tp.cold_connections_dialed,
        tp.negotiated_alpn,
        tp.tls_version,
        tp.peak_inflight
    ));
    out.push_str(&format!(
        "  Amplify    : Read Amp {:.2}x (Net Wire/Logical {:.2}x, Gap Waste {} KiB) | RPC Amp {:.2}x ({} A / {} B) | Cache Hit {:.1}%\n",
        tp.read_amplification,
        tp.net_wire_read_ratio,
        tp.coalesced_gap_waste_bytes / 1024,
        tp.rpc_amplification,
        tp.class_a_rpcs,
        tp.class_b_rpcs,
        tp.cache_hit_ratio_pct
    ));
    out.push_str(&format!(
        "  Resilience : 429/503 Throttles={} | Retries={} ({:.1} ms backoff) | TailCut Cancels={} | CRC32C={} ok / {} bad\n",
        tp.http_429_count + tp.http_503_slowdown_count,
        tp.retry_attempts,
        tp.retry_backoff_ms,
        tp.tail_cut_cancels,
        tp.crc32c_verified_ok,
        tp.crc32c_corrupted
    ));
    if rep.frame.cleanup.auto_cleanup_enabled {
        out.push_str(&format!(
            "  Cleanup    : {} ({}/{} objects removed under `{}` in {:.1} ms)\n",
            if rep.frame.cleanup.verified_clean {
                "VERIFIED CLEAN (0 leaked objects)"
            } else {
                "INCOMPLETE"
            },
            rep.frame.cleanup.objects_deleted,
            rep.frame.cleanup.objects_created,
            rep.frame.cleanup.run_prefix,
            rep.frame.cleanup.cleanup_ms
        ));
    }
    out.push('\n');

    // 4. Multi-Cloud FinOps
    out.push_str(&format!(
        "{} ({:.0} TB stored / {:.0} TB monthly access)\n",
        style("4. Multi-Cloud Monthly FinOps Projection", "1;36", c),
        rep.profile.pricing.baseline_stored_tb,
        rep.profile.pricing.monthly_logical_access_tb
    ));
    out.push_str(&format!(
        "  {:<22} {:>11} {:>12} {:>11} {:>13} {:>13} {:>13}\n",
        "PROVIDER", "STORAGE", "API(A+B)", "EGRESS", "TOTAL/MO", "AMP WASTE", "CACHE SAVED"
    ));
    out.push_str(&format!("  {}\n", "─".repeat(101)));
    for f in &rep.multi_cloud_finops {
        out.push_str(&format!(
            "  {:<22} {:>10.2}$ {:>11.2}$ {:>10.2}$ {:>12.2}$ {:>12.2}$ {:>12.2}$\n",
            f.provider_name,
            f.monthly_storage_usd,
            f.monthly_api_requests_usd,
            f.monthly_egress_usd,
            f.total_monthly_usd,
            f.waste_from_amplification_usd,
            f.saved_by_cache_usd
        ));
    }

    // 5. Top Slow Request Call Stack
    if let Some(slowest) = rep.frame.slowest_traces.first() {
        out.push_str(&format!(
            "\n{} (slowest observed: {:.2} ms on `{}`)\n",
            style("5. Slow-Request AwaitTree Call Stack", "1;36", c),
            slowest.observed_ms,
            slowest.root_span
        ));
        for line in slowest.tree_dump.lines().take(8) {
            out.push_str(&format!("  {line}\n"));
        }
    }

    if !rep.trials.is_empty() {
        out.push_str(&format!(
            "\n{} ({} trials, 429/503 circuit breaker: {})\n",
            style("6. Auto-Tune Sweep Summary", "1;36", c),
            rep.trials.len(),
            if rep.circuit_breaker_triggered {
                "TRIGGERED -> BACKED OFF SAFELY"
            } else {
                "NOT TRIGGERED"
            }
        ));
        for t in &rep.trials {
            let marker = if t.knob == rep.active_knob && !t.throttled_503 {
                "★"
            } else if t.throttled_503 {
                "✗"
            } else {
                " "
            };
            out.push_str(&format!(
                "  {marker} #{:<2} [{:<18}] conc={:<3} chunk={:>5}KiB gap={:>4}KiB cache={:<3} -> {:>7.1} MiB/s | P99 {:>7.2}ms | ReadAmp {:.2}x | 429/503={}\n",
                t.trial_id,
                t.phase,
                t.knob.concurrent,
                t.knob.chunk_bytes / 1024,
                t.knob.range_gap_bytes / 1024,
                if t.knob.enable_foyer { "on" } else { "off" },
                t.aggregate_mib_s,
                t.worst_p99_ms,
                t.read_amplification,
                t.throttle_events
            ));
        }
    }

    out
}

/// Render `oprobe diff` verdict cleanly for terminal / CI logs.
pub fn render_diff_terminal(diff: &DiffSummary) -> String {
    let c = colors_enabled();
    let badge = if diff.passed_gate {
        style("PASS (CI Gate Green)", "1;32", c)
    } else {
        style("FAIL (Regression Gate Breached — Exit Code 2)", "1;31", c)
    };
    let mut out = format!("● oprobe diff — {badge}\n");
    out.push_str(&format!(
        "  Throughput : {:.2} -> {:.2} MiB/s ({:.2}x)\n",
        diff.baseline_mib_s, diff.candidate_mib_s, diff.throughput_ratio
    ));
    out.push_str(&format!(
        "  Amplify    : Read Amp {:.2}x -> {:.2}x | RPC Amp {:.2}x -> {:.2}x | Monthly Bill ${:.2} -> ${:.2} ({:+.1}%)\n\n",
        diff.baseline_read_amp,
        diff.candidate_read_amp,
        diff.baseline_rpc_amp,
        diff.candidate_rpc_amp,
        diff.baseline_monthly_usd,
        diff.candidate_monthly_usd,
        diff.monthly_usd_delta_pct
    ));
    out.push_str(&format!(
        "  {:<24} {:>12} {:>12} {:>12} {:>10}\n",
        "OPERATION VERB", "BASE P99(ms)", "CAND P99(ms)", "DELTA(%)", "STATUS"
    ));
    out.push_str(&format!("  {}\n", "─".repeat(74)));
    for v in &diff.verb_diffs {
        out.push_str(&format!(
            "  {:<24} {:>12.2} {:>12.2} {:>+11.1}% {:>10}\n",
            v.verb,
            v.baseline_p99_ms,
            v.candidate_p99_ms,
            v.p99_delta_pct,
            if v.regressed { "REGRESSED" } else { "OK" }
        ));
    }
    if !diff.reasons.is_empty() {
        out.push_str("\n  Breach Reasons:\n");
        for r in &diff.reasons {
            out.push_str(&format!("    - {r}\n"));
        }
    }
    out
}

/// Render `oprobe net-probe` cold-vs-warm socket & TLS handshake summary in terminal.
pub fn render_handshake_terminal(hs: &EndpointHandshakeSummary) -> String {
    let c = colors_enabled();
    let mut out = format!(
        "{} {} ({}:{})\n",
        style("● oprobe net-probe", "1;36", c),
        style(&hs.endpoint, "1", c),
        hs.host,
        hs.port
    );
    out.push_str(&format!(
        "  Protocol : ALPN `{}` | TLS `{}` | Cipher `{}` | Resolved IPs: {:?}\n",
        hs.negotiated_alpn, hs.tls_version, hs.cipher_suite, hs.resolved_ips
    ));
    out.push_str(&format!(
        "  Cold Dial: DNS {:.2}/{:.2} ms (P50/P99) | TCP RTT {:.2}/{:.2} ms | TLS {:.2}/{:.2} ms | Cold HEAD TTFB {:.2}/{:.2} ms\n",
        hs.dns_p50_ms,
        hs.dns_p99_ms,
        hs.tcp_rtt_p50_ms,
        hs.tcp_rtt_p99_ms,
        hs.tls_p50_ms,
        hs.tls_p99_ms,
        hs.head_ttfb_p50_ms,
        hs.head_ttfb_p99_ms
    ));
    out.push_str(&format!(
        "  Reuse Win: Cold Total {:.2} ms vs Warm Reused HEAD TTFB {:.2} ms -> {:.1}x faster with Keep-Alive!\n\n",
        hs.cold_total_ms, hs.warm_ttfb_p50_ms, hs.cold_vs_warm_ratio
    ));
    for s in &hs.samples {
        out.push_str(&format!(
            "  #{} [{} -> HTTP {}] dns={:.2}ms tcp={:.2}ms tls={:.2}ms cold_ttfb={:.2}ms warm_ttfb={:.2}ms ({})\n",
            s.sample_index,
            s.remote_addr,
            s.http_status,
            s.dns_lookup_us as f64 / 1000.0,
            s.tcp_connect_us as f64 / 1000.0,
            s.tls_handshake_us as f64 / 1000.0,
            s.http_head_ttfb_us as f64 / 1000.0,
            s.warm_reused_ttfb_us as f64 / 1000.0,
            s.alpn_protocol
        ));
    }
    out
}

/// Format a structured runtime failure with actionable diagnostic hints on stderr.
pub fn render_error_terminal(err: &anyhow::Error, hint: &ActionableErrorHint) -> String {
    let c = std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal();
    let mut out = format!(
        "\n{} {}\n",
        style("✗ oprobe diagnostic error:", "1;31", c),
        style(&hint.category, "1", c)
    );
    out.push_str(&format!("  Cause   : {err:#}\n"));
    out.push_str(&format!("  Insight : {}\n", hint.likely_cause));
    out.push_str("  How to fix:\n");
    for (idx, step) in hint.remediation_steps.iter().enumerate() {
        out.push_str(&format!("    {}. {}\n", idx + 1, step));
    }
    out
}
