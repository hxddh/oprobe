//! Bottleneck attribution and findings derived strictly from measured data.

use serde::{Deserialize, Serialize};

use crate::config::{Knobs, Sla};
use crate::trace::Snapshot;
use crate::util::{fmt_bytes, fmt_ms};

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// The layer that dominates operation latency.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Bottleneck {
    #[default]
    None,
    /// Waiting for in-flight request permits / pooled connections.
    ClientQueue,
    /// Client-side processing (CPU, OpenDAL, response consumption).
    Client,
    /// DNS / TCP / TLS / proxy handshakes.
    Connect,
    /// Uploading request bodies.
    Upload,
    /// Server time to first byte.
    ServerLatency,
    /// Downloading response bodies.
    Download,
    /// Rate limiting (429/503).
    Throttling,
    /// Sleeping between retry attempts (errors that are not throttling).
    RetryBackoff,
    /// In-process backend (memory/fs) service time.
    LocalBackend,
}

impl Bottleneck {
    pub fn label(&self) -> &'static str {
        match self {
            Bottleneck::None => "none",
            Bottleneck::ClientQueue => "client queue",
            Bottleneck::Client => "client processing",
            Bottleneck::Connect => "connection setup",
            Bottleneck::Upload => "upload transfer",
            Bottleneck::ServerLatency => "server latency (TTFB)",
            Bottleneck::Download => "download transfer",
            Bottleneck::Throttling => "throttling (429/503)",
            Bottleneck::RetryBackoff => "retry backoff",
            Bottleneck::LocalBackend => "local backend",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Finding {
    pub severity: Severity,
    pub code: String,
    pub title: String,
    pub evidence: String,
    pub action: String,
}

/// Share of total operation time spent in each layer (fractions sum to ~1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TimeShares {
    pub queue: f64,
    pub cache: f64,
    pub connect: f64,
    pub upload: f64,
    pub server: f64,
    pub download: f64,
    /// Retry layer sleeping between attempts.
    #[serde(default)]
    pub retry: f64,
    pub client: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Diagnosis {
    pub bottleneck: Bottleneck,
    pub headline: String,
    pub shares: TimeShares,
    pub findings: Vec<Finding>,
}

impl Diagnosis {
    pub fn worst(&self) -> Severity {
        self.findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or_default()
    }
}

fn finding(
    severity: Severity,
    code: &str,
    title: String,
    evidence: String,
    action: &str,
) -> Finding {
    Finding {
        severity,
        code: code.to_owned(),
        title,
        evidence,
        action: action.to_owned(),
    }
}

/// Compute where operation time went. Backend time is split into request stages in
/// proportion to the measured per-request stage totals.
pub fn time_shares(s: &Snapshot) -> TimeShares {
    let (mut q, mut c, mut b, mut rt, mut cl) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for v in &s.verbs {
        let n = v.count as f64;
        q += v.breakdown.queue_ms * n;
        c += v.breakdown.cache_ms * n;
        b += v.breakdown.backend_ms * n;
        rt += v.breakdown.retry_ms * n;
        cl += v.breakdown.client_ms * n;
    }
    let total = q + c + b + rt + cl;
    if total <= 0.0 {
        return TimeShares::default();
    }
    // Request-level stage totals (excluding queue and consumer stall).
    let (mut conn, mut up, mut srv, mut down) = (0.0, 0.0, 0.0, 0.0);
    for a in &s.apis {
        let n = a.stages.total.count as f64;
        conn += a.stages.connect.mean * a.stages.connect.count as f64;
        up += a.stages.send.mean * n;
        srv += a.stages.ttfb.mean * n;
        down += a.stages.body.mean * n;
    }
    let net = conn + up + srv + down;
    let f = |x: f64| if net > 0.0 { b * x / net / total } else { 0.0 };
    TimeShares {
        queue: q / total,
        cache: c / total,
        connect: f(conn),
        upload: f(up),
        server: if net > 0.0 { f(srv) } else { b / total },
        download: f(down),
        retry: rt / total,
        client: cl / total,
    }
}

pub fn diagnose(s: &Snapshot, knobs: &Knobs, sla: &Sla, network: bool) -> Diagnosis {
    let mut f = Vec::new();
    let shares = time_shares(s);
    let ops = s.summary.ops.max(1) as f64;
    let requests = s.connections.requests.max(1) as f64;

    // --- correctness & availability ---------------------------------------------------
    if s.integrity.corrupted > 0 {
        f.push(finding(
            Severity::Critical,
            "integrity",
            format!("{} CRC32C mismatches", s.integrity.corrupted),
            format!("{} verified OK, {} corrupted", s.integrity.verified, s.integrity.corrupted),
            "Data read back differs from what was written. Check proxies/gateways that rewrite bodies, multipart part ordering and the storage service health.",
        ));
    }
    if s.summary.failed_ops > 0 {
        let rate = s.summary.error_rate;
        let top = s
            .resilience
            .error_classes
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(k, n)| format!("most common: {k} ×{n}"))
            .unwrap_or_default();
        f.push(finding(
            if rate > sla.max_error_rate { Severity::Critical } else { Severity::Warning },
            "errors",
            format!("{:.2}% of operations failed", rate * 100.0),
            format!("{} of {} ops failed; {top}", s.summary.failed_ops, s.summary.ops),
            "Inspect `resilience.error_classes` and the slow-op request timelines; run `oprobe doctor` for credential/permission/endpoint checks.",
        ));
    }
    let thr = s.resilience.throttled_responses;
    if thr > 0 {
        f.push(finding(
            Severity::Critical,
            "throttling",
            format!("{thr} throttled responses (429/503)"),
            format!(
                "{:.2}% of requests throttled; {} retries, {} total backoff ({:.0}% of operation time spent waiting to retry), max Retry-After {} ms",
                thr as f64 / requests * 100.0,
                s.resilience.retries,
                fmt_ms(s.resilience.retry_backoff_ms),
                shares.retry * 100.0,
                s.resilience.max_retry_after_ms
            ),
            "The service is rate-limiting this request rate. Lower `concurrency` / `max_inflight_requests` (or run `oprobe tune` to find the safe ceiling), spread keys across more prefixes, and keep retries with jitter enabled.",
        ));
    }

    // --- bottleneck attribution --------------------------------------------------------
    let candidates = [
        (Bottleneck::ClientQueue, shares.queue),
        (Bottleneck::Connect, shares.connect),
        (Bottleneck::Upload, shares.upload),
        (
            if network {
                Bottleneck::ServerLatency
            } else {
                Bottleneck::LocalBackend
            },
            shares.server,
        ),
        (Bottleneck::Download, shares.download),
        (Bottleneck::RetryBackoff, shares.retry),
        (Bottleneck::Client, shares.client + shares.cache),
    ];
    let (mut bottleneck, top_share) = candidates
        .iter()
        .copied()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap_or((Bottleneck::None, 0.0));
    if s.summary.ops == 0 {
        bottleneck = Bottleneck::None;
    }
    if thr as f64 / requests > 0.01 {
        bottleneck = Bottleneck::Throttling;
    }
    let lat = &s.summary.latency;
    let stage = s.request_stage_means();
    let cores = s.client.cpu_cores.max(1) as f64;
    let cpu_busy = s.client.cpu_pct / 100.0;
    // A saturated load generator overrides any stage share (A9): on one busy core every stage
    // looks slow, and "add streams" would make it worse.
    if cpu_busy >= crate::analysis::limits::CLIENT_BOUND_RATIO * cores
        && !matches!(bottleneck, Bottleneck::Throttling | Bottleneck::None)
    {
        bottleneck = Bottleneck::Client;
    }
    let headline = match bottleneck {
        Bottleneck::None => "No operations completed in the measurement window.".to_owned(),
        Bottleneck::Throttling if shares.retry >= 0.005 => format!(
            "Throttling-bound: the service rejected {:.1}% of requests with 429/503; {:.0}% of operation time is retry backoff.",
            thr as f64 / requests * 100.0,
            shares.retry * 100.0
        ),
        Bottleneck::Throttling => format!(
            "Throttling-bound: the service rejected {:.1}% of requests with 429/503.",
            thr as f64 / requests * 100.0
        ),
        b => format!(
            "{}-bound: {:.0}% of operation time is spent in {} (p50 {} / p99 {}).",
            capitalize(b.label()),
            top_share * 100.0,
            b.label(),
            fmt_ms(lat.p50),
            fmt_ms(lat.p99)
        ),
    };

    match bottleneck {
        Bottleneck::ClientQueue => f.push(finding(
            Severity::Warning,
            "client_queue",
            "Operations wait for in-flight request permits".into(),
            format!(
                "{:.0}% of op time queued; mean request queue wait {} (cap {} in-flight requests, peak {})",
                shares.queue * 100.0,
                fmt_ms(stage.queue),
                if knobs.max_inflight_requests == 0 { "∞".to_owned() } else { knobs.max_inflight_requests.to_string() },
                s.connections.peak_inflight_requests
            ),
            "Raise `max_inflight_requests` (or set 0 = unlimited) or reduce `concurrency × io_concurrency` so requests don't queue behind each other.",
        )),
        Bottleneck::Client => {
            if cpu_busy > 0.8 * cores {
                f.push(finding(
                    Severity::Warning,
                    "client_cpu",
                    "Client CPU is saturated".into(),
                    format!("process uses {:.1} of {cores} cores", cpu_busy),
                    "The load generator itself is the bottleneck: use fewer/larger operations, disable CRC verification (`workload.verify = false`), or run on a larger machine.",
                ));
            } else if stage.stall > stage.body && stage.stall > 0.5 {
                f.push(finding(
                    Severity::Warning,
                    "consumer_stall",
                    "Response bodies wait for the consumer".into(),
                    format!("mean consumer stall {} vs network transfer {}", fmt_ms(stage.stall), fmt_ms(stage.body)),
                    "Data arrives faster than it is consumed; the application side (decode/copy) is the limit, not storage.",
                ));
            } else {
                f.push(finding(
                    Severity::Info,
                    "client_overhead",
                    "Time is spent outside network requests".into(),
                    format!("{:.0}% of op time in client processing (signing, buffering, OpenDAL, CRC)", (shares.client + shares.cache) * 100.0),
                    "Typical for in-memory/local backends or tiny objects. For remote stores, batch small operations.",
                ));
            }
        }
        Bottleneck::Connect => f.push(finding(
            Severity::Warning,
            "cold_connections",
            "Connection setup dominates".into(),
            format!(
                "{} new connections, reuse {:.0}%; DNS p50 {} · TCP p50 {} · TLS p50 {}",
                s.connections.dialed,
                s.connections.reuse_pct,
                fmt_ms(s.connections.dns.p50),
                fmt_ms(s.connections.tcp.p50),
                fmt_ms(s.connections.tls.p50)
            ),
            if knobs.keep_alive {
                "Connections are not being reused: check that responses are fully consumed, raise the idle pool, and prefer HTTP/2 where available."
            } else {
                "Enable `keep_alive = true`: every request currently pays DNS + TCP + TLS."
            },
        )),
        Bottleneck::ServerLatency => {
            let rtt = s.connections.tcp.p50;
            f.push(finding(
                Severity::Info,
                "server_latency",
                "Latency-bound on server time-to-first-byte".into(),
                format!(
                    "mean TTFB {} per request{}, {:.2} requests per op",
                    fmt_ms(stage.ttfb),
                    if rtt > 0.0 { format!(" (TCP connect ≈ {})", fmt_ms(rtt)) } else { String::new() },
                    s.bytes.request_amplification
                ),
                "Throughput scales with parallelism here: raise `concurrency` / `io_concurrency` until throttling or the SLA limit (`oprobe tune`), coalesce small ranges (`range_gap`), and run close to the bucket's region.",
            ));
        }
        Bottleneck::Download | Bottleneck::Upload => {
            let api_bw = s
                .apis
                .iter()
                .filter(|a| a.stages.body.count > 0)
                .map(|a| {
                    let bytes = if bottleneck == Bottleneck::Download { a.bytes_down } else { a.bytes_up };
                    let t = if bottleneck == Bottleneck::Download { a.stages.body.mean } else { a.stages.send.mean } * a.count as f64;
                    (bytes as f64, t)
                })
                .fold((0.0, 0.0), |acc, x| (acc.0 + x.0, acc.1 + x.1));
            let per_stream = if api_bw.1 > 0.0 { api_bw.0 / (api_bw.1 / 1000.0) / 1048576.0 } else { 0.0 };
            f.push(finding(
                Severity::Info,
                if bottleneck == Bottleneck::Download { "download_bound" } else { "upload_bound" },
                format!("Bandwidth-bound on {}", if bottleneck == Bottleneck::Download { "downloads" } else { "uploads" }),
                format!(
                    "≈{per_stream:.1} MiB/s per request stream, aggregate {:.1} MiB/s",
                    s.summary.mib_per_s
                ),
                "Use more parallel streams (`io_concurrency`, `concurrency`) with larger `chunk_size`; check NIC, proxy and cross-region links.",
            ));
        }
        Bottleneck::RetryBackoff => f.push(finding(
            Severity::Warning,
            "retries",
            "Operations spend their time waiting to retry".into(),
            format!(
                "{:.0}% of op time in retry backoff; {} retries, {} total backoff",
                shares.retry * 100.0,
                s.resilience.retries,
                fmt_ms(s.resilience.retry_backoff_ms)
            ),
            "Requests fail with temporary errors and are retried after a backoff: see `resilience.error_classes` and the slow-op request timelines for the failing requests.",
        )),
        Bottleneck::LocalBackend => f.push(finding(
            Severity::Info,
            "local_backend",
            "Local backend service time dominates".into(),
            format!("in-process backend calls take {:.0}% of op time", shares.server * 100.0),
            "Results reflect local disk/memory, not network storage.",
        )),
        _ => {}
    }

    // --- secondary findings -------------------------------------------------------------
    if network
        && s.connections.requests > 50
        && s.connections.reuse_pct < 80.0
        && bottleneck != Bottleneck::Connect
    {
        f.push(finding(
            Severity::Warning,
            "cold_connections",
            format!("Only {:.0}% of requests reused a connection", s.connections.reuse_pct),
            format!("{} dials for {} requests", s.connections.dialed, s.connections.requests),
            "Enable keep-alive and make sure response bodies are fully drained so connections return to the pool.",
        ));
    }
    if s.connections.tls.p50 > 100.0 {
        f.push(finding(
            Severity::Info,
            "cold_connections",
            "Slow TLS handshakes".into(),
            format!(
                "TLS p50 {} p99 {}",
                fmt_ms(s.connections.tls.p50),
                fmt_ms(s.connections.tls.p99)
            ),
            "Long RTT or a TLS-inspecting proxy; connection reuse matters a lot here.",
        ));
    }
    if s.bytes.overread_ratio > 1.3 && s.bytes.backend_down > 1 << 20 {
        let wasted =
            s.bytes.backend_down as f64 - s.bytes.backend_down as f64 / s.bytes.overread_ratio;
        f.push(finding(
            Severity::Warning,
            "overread",
            format!("{:.0}% of downloaded bytes were never requested", (1.0 - 1.0 / s.bytes.overread_ratio) * 100.0),
            format!("over-read {} (ratio {:.2}×) with range_gap {} / chunk {}", fmt_bytes(wasted as u64), s.bytes.overread_ratio, knobs.range_gap, knobs.chunk_size),
            "Lower `range_gap` (coalescing merges distant ranges) or `chunk_size`; over-read costs egress and bandwidth.",
        ));
    }
    if s.cache.enabled && s.cache.lookups > 100 && s.cache.hit_ratio < 0.05 {
        f.push(finding(
            Severity::Info,
            "cache_ineffective",
            format!("Read cache hit ratio is {:.1}%", s.cache.hit_ratio * 100.0),
            format!("{} lookups, {} hits", s.cache.lookups, s.cache.hits),
            "The access pattern has little reuse; disable the cache to save memory or enlarge it if the working set is close to `cache_size`.",
        ));
    }
    for v in &s.verbs {
        if v.count >= 50 && v.latency.p50 > 0.5 && v.latency.p99 > 5.0 * v.latency.p50 {
            f.push(finding(
                Severity::Warning,
                "tail_latency",
                format!("`{}` p99 is {:.1}× its median", v.verb, v.latency.p99 / v.latency.p50),
                format!("p50 {} · p99 {} · max {}", fmt_ms(v.latency.p50), fmt_ms(v.latency.p99), fmt_ms(v.latency.max)),
                "See the slowest-operation timelines for the responsible stage. Server stragglers can be cut with `tail_cut = true`; retries/backoff point to throttling or errors.",
            ));
        }
        if let Some(limit) = sla.p99_ms
            && v.count > 0
            && v.latency.p99 > limit
        {
            f.push(finding(
                    Severity::Warning,
                    "sla_p99",
                    format!("`{}` p99 {} exceeds the {} SLA", v.verb, fmt_ms(v.latency.p99), fmt_ms(limit)),
                    format!("{} ops, p50 {}", v.count, fmt_ms(v.latency.p50)),
                    "Reduce concurrency if queueing/throttling, or tune chunk/range settings (`oprobe tune`).",
                ));
        }
    }
    if let Some(ol) = &s.open_loop
        && ol.missed > 0
    {
        f.push(finding(
                Severity::Warning,
                "rate_not_sustained",
                format!("Target rate {:.0} ops/s not sustained", ol.target_rate),
                format!(
                    "achieved {:.0} ops/s; arrivals queued behind {} busy slots (max schedule lag {}); {} never started before the window ended",
                    ol.achieved_rate,
                    knobs.concurrency,
                    fmt_ms(ol.max_schedule_lag_ms),
                    ol.missed
                ),
                "Raise `concurrency` (in-flight cap) or lower the target rate. Queued arrivals still ran: their wait is included in the latency percentiles (measured from the intended start time).",
            ));
    }
    if cpu_busy > 0.8 * cores && bottleneck != Bottleneck::Client {
        f.push(finding(
            Severity::Warning,
            "client_cpu",
            "Load generator CPU is near saturation".into(),
            format!("process uses {:.1} of {cores} cores", cpu_busy),
            "Measurements may understate what the storage can do; use a bigger client or lower verification overhead.",
        ));
    }
    // Parquet-style scans that HEAD every file before reading its footer (stream B2).
    if let Some(v) = s
        .verbs
        .iter()
        .find(|v| v.verb == "parquet.scan" && v.count > 0)
    {
        let heads = s
            .apis
            .iter()
            .find(|a| a.api == crate::trace::Api::HeadObject)
            .map_or(0, |a| a.count);
        if heads > 0 && heads * 2 >= v.count {
            f.push(finding(
                Severity::Info,
                "suffix_read",
                "Use suffix reads: saves one HEAD per file".into(),
                format!(
                    "{heads} HEAD requests for {} file scans ({:.0}% of all requests): each scan asks for the size before reading the footer",
                    v.count,
                    heads as f64 / requests * 100.0
                ),
                "Read the footer with a suffix range (`Range: bytes=-N`, OpenDAL `read_with(path).range(BytesRange::suffix(n))`): the response carries the footer and the file size. Verify: `oprobe run TARGET --preset parquet-suffix`.",
            ));
        }
    }
    let _ = ops;
    f.sort_by_key(|x| std::cmp::Reverse(x.severity));
    Diagnosis {
        bottleneck,
        headline,
        shares,
        findings: f,
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Outcome of checking a run against its [`Sla`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SlaVerdict {
    /// Whether a breach fails the command (exit code 3).
    pub enforced: bool,
    pub passed: bool,
    /// One line per breached objective (empty when passed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breaches: Vec<String>,
}

/// Throttled responses (429/503) per request of a snapshot.
pub fn throttled_request_ratio(s: &Snapshot) -> f64 {
    if s.summary.throttled_request_ratio > 0.0 {
        return s.summary.throttled_request_ratio;
    }
    crate::util::ratio(
        s.resilience.throttled_responses as f64,
        s.connections.requests as f64,
    )
}

/// Check the measured result against the SLA: error rate, data integrity (always), per-verb
/// p99 and throttled requests (when set).
pub fn check_sla(s: &Snapshot, sla: &Sla) -> SlaVerdict {
    let mut breaches = Vec::new();
    if s.integrity.corrupted > 0 {
        breaches.push(format!(
            "{} corrupted objects (CRC32C mismatch); corrupted data is never acceptable",
            s.integrity.corrupted
        ));
    }
    if s.summary.ops > 0 && s.summary.error_rate > sla.max_error_rate {
        breaches.push(format!(
            "{:.2}% of operations failed ({} of {}), limit {}%",
            s.summary.error_rate * 100.0,
            s.summary.failed_ops,
            s.summary.ops,
            sla.max_error_rate * 100.0
        ));
    }
    if s.summary.ops == 0 {
        breaches.push("no operation completed in the measurement window".into());
    }
    if let Some(limit) = sla.max_throttle_rate {
        // Throttled responses per request (design v4 §4: the one throttle rate of the SLA).
        let rate = throttled_request_ratio(s);
        if rate > limit {
            breaches.push(format!(
                "{:.2}% of requests were throttled (429/503), limit {}%",
                rate * 100.0,
                limit * 100.0
            ));
        }
    }
    if let Some(limit) = sla.p99_ms {
        for v in s
            .verbs
            .iter()
            .filter(|v| v.count > 0 && v.latency.p99 > limit)
        {
            breaches.push(format!(
                "`{}` p99 {} exceeds the {} limit",
                v.verb,
                fmt_ms(v.latency.p99),
                fmt_ms(limit)
            ));
        }
    }
    SlaVerdict {
        enforced: sla.enforce,
        passed: breaches.is_empty(),
        breaches,
    }
}

#[cfg(test)]
mod sla_tests {
    use super::*;
    use crate::trace::{Quantiles, Summary, VerbStats};

    fn snap(ops: u64, failed: u64) -> Snapshot {
        Snapshot {
            summary: Summary {
                ops,
                failed_ops: failed,
                error_rate: failed as f64 / ops.max(1) as f64,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn default_sla_fails_on_errors_above_one_percent_and_any_corruption() {
        let sla = Sla::default();
        assert!(sla.enforce);
        assert!(
            check_sla(&snap(1000, 10), &sla).passed,
            "1% is within the SLA"
        );
        let v = check_sla(&snap(1000, 11), &sla);
        assert!(!v.passed && v.enforced);
        assert!(v.breaches[0].contains("failed"), "{v:?}");
        let mut s = snap(1000, 0);
        s.integrity.corrupted = 1;
        assert!(!check_sla(&s, &sla).passed);
        // Throttling absorbed by retries is not a breach unless a limit is set.
        let mut s = snap(100, 0);
        s.verbs = vec![VerbStats {
            verb: "read.full".into(),
            count: 100,
            throttled: 50,
            latency: Quantiles {
                p99: 900.0,
                ..Default::default()
            },
            ..Default::default()
        }];
        s.resilience.throttled_responses = 50;
        s.connections.requests = 400;
        assert!(check_sla(&s, &sla).passed);
        let strict = Sla {
            max_throttle_rate: Some(0.1),
            p99_ms: Some(500.0),
            ..Sla::default()
        };
        let v = check_sla(&s, &strict);
        assert_eq!(v.breaches.len(), 2);
        // The limit is on requests (50 of 400 = 12.5%), not on operations (50%).
        assert!(v.breaches[0].contains("12.50% of requests"), "{v:?}");
        let loose = Sla {
            max_throttle_rate: Some(0.2),
            ..Sla::default()
        };
        assert!(check_sla(&s, &loose).passed, "12.5% of requests < 20%");
        let off = Sla {
            enforce: false,
            ..Sla::default()
        };
        let v = check_sla(&snap(10, 10), &off);
        assert!(!v.passed && !v.enforced);
    }
}
