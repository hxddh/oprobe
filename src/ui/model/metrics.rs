//! Report metrics the app shows by name, read one way everywhere (tiles, verdicts, verify).
//!
//! `throttled_request_ratio` is design v4 §4's definition (throttled responses / requests);
//! until the report carries the field itself it is derived here from the same counters.

use crate::model::Report;
use crate::trace::{ConnStats, ResilienceStats};

/// Throttled responses / requests (0–1).
pub fn throttled_request_ratio_of(res: &ResilienceStats, conn: &ConnStats) -> f64 {
    if conn.requests == 0 {
        0.0
    } else {
        res.throttled_responses as f64 / conn.requests as f64
    }
}

/// Retries / requests (0–1): the share of requests that repeated an earlier attempt.
pub fn retry_share_of(res: &ResilienceStats, conn: &ConnStats) -> f64 {
    if conn.requests == 0 {
        0.0
    } else {
        (res.retries as f64 / conn.requests as f64).min(1.0)
    }
}

pub fn throttled_request_ratio(r: &Report) -> f64 {
    throttled_request_ratio_of(&r.resilience, &r.connections)
}

pub fn retry_share(r: &Report) -> f64 {
    retry_share_of(&r.resilience, &r.connections)
}

/// Whether the cost projection assumes the stored volume (the built-in default, not a
/// `--stored-gib` / `--set stored_gib=` the user gave).
pub fn storage_assumed(r: &Report) -> bool {
    let default = crate::config::defaults().cost.stored_gib;
    (r.spec.profile.cost.stored_gib - default).abs() < 1e-9
}

/// "(assumed 1 TiB stored — set --stored-gib)", or empty when the volume was given.
pub fn storage_note(r: &Report) -> String {
    if storage_assumed(r) {
        format!(
            "(assumed {} stored — set --stored-gib)",
            super::fmt::bytes(r.cost.stored_gb * 1_073_741_824.0)
        )
    } else {
        String::new()
    }
}

/// A report metric by the name an [`crate::analysis::verification::Expectation`] uses, in
/// that expectation's unit (`ratio` 0–1, `ms`, `ops_per_s`, `mib_per_s`).
pub fn metric(r: &Report, name: &str) -> Option<f64> {
    let n = name.trim().trim_start_matches("summary.");
    Some(match n {
        "ops_per_s" | "goodput" | "goodput_ops_per_s" => r.summary.ops_per_s,
        "mib_per_s" => r.summary.mib_per_s,
        "p50_ms" | "latency.p50" | "p50" => r.summary.latency.p50,
        "p99_ms" | "latency.p99" | "p99" => r.summary.latency.p99,
        "error_ratio" | "error_rate" => r.summary.error_rate,
        "throttled_request_ratio" | "throttle_ratio" | "throttled_ratio" => {
            throttled_request_ratio(r)
        }
        "retry_ratio" | "retry_share" => retry_share(r),
        "request_amplification" | "requests_per_op" => r.bytes.request_amplification,
        "cost_per_month_usd" | "cost.total_usd" => r.cost.selected.total_usd,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_reads_requests_not_ops() {
        let mut r = crate::report::fixture::run().report;
        r.connections.requests = 200;
        r.resilience.throttled_responses = 10;
        r.resilience.retries = 30;
        assert!((throttled_request_ratio(&r) - 0.05).abs() < 1e-12);
        assert!((retry_share(&r) - 0.15).abs() < 1e-12);
        assert_eq!(metric(&r, "throttled_request_ratio"), Some(0.05));
        assert_eq!(metric(&r, "summary.ops_per_s"), Some(r.summary.ops_per_s));
        assert_eq!(metric(&r, "nonsense"), None);
        r.connections.requests = 0;
        assert_eq!(throttled_request_ratio(&r), 0.0);
    }
}
