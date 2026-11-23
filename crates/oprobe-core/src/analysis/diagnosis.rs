//! Bottleneck attribution (where operation time went), the stage shares and the SLA check.
//! Findings are [`super::findings`].

use serde::{Deserialize, Serialize};

use crate::config::Sla;
use crate::trace::Snapshot;
use crate::util::fmt_ms;

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

/// Where a run's operation time went: the dominant layer and a one-line headline.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Diagnosis {
    pub bottleneck: Bottleneck,
    pub headline: String,
    pub shares: TimeShares,
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

/// The bottleneck of a snapshot: the layer with the largest share of operation time
/// (throttling when more than 1 % of requests were throttled, the client when its CPU is
/// saturated) and a headline saying so.
pub fn diagnose(s: &Snapshot, network: bool) -> Diagnosis {
    let shares = time_shares(s);
    let requests = s.connections.requests.max(1) as f64;
    let thr = s.resilience.throttled_responses;
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
    let cores = s.client.cpu_cores.max(1) as f64;
    let cpu_busy = s.client.cpu_pct / 100.0;
    // A saturated load generator overrides any stage share (A9).
    if cpu_busy >= crate::analysis::limits::CLIENT_BOUND_RATIO * cores
        && !matches!(bottleneck, Bottleneck::Throttling | Bottleneck::None)
    {
        bottleneck = Bottleneck::Client;
    }
    let lat = &s.summary.latency;
    let headline = match bottleneck {
        Bottleneck::None => "No operations completed in the measurement window.".to_owned(),
        Bottleneck::Throttling => format!(
            "Throttling-bound: {:.0}% of operation time is retry backoff after 429/503 ({:.1}% of requests rejected).",
            shares.retry * 100.0,
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
    Diagnosis {
        bottleneck,
        headline,
        shares,
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
