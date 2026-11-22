//! The finding-code catalog (design v6 §8): what every finding code means, why it happens,
//! and its one remedy. It documents exactly the codes the findings engine can emit
//! ([`crate::analysis::findings::CODES`]: the eleven remedy codes plus `errors` and
//! `invalid_run`); a test also scans the engine's source for code literals. Lookups accept `-`
//! and `_` interchangeably.

use schemars::JsonSchema;
use serde::Serialize;

use crate::analysis::diagnosis::Severity;
use crate::model::Remedy;

/// Documentation of one finding code.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct FindingDoc {
    /// Stable code as it appears in `report.findings[].code`.
    pub code: &'static str,
    pub title: &'static str,
    /// Severity the engine usually assigns.
    pub typical_severity: Severity,
    /// What the finding means.
    pub meaning: &'static str,
    /// Typical causes, most common first.
    pub causes: &'static [&'static str],
    /// The one remedy ([`crate::analysis::findings::remedy`]).
    pub remedy: Remedy,
}

struct Entry {
    code: &'static str,
    title: &'static str,
    severity: Severity,
    meaning: &'static str,
    causes: &'static [&'static str],
}

const ENTRIES: &[Entry] = &[
    Entry {
        code: "throttling",
        title: "The service throttles above a number of requests in flight",
        severity: Severity::Critical,
        meaning: "The service answered 429/503 and the rejections follow the requests in flight (the limit model is in the report's capacity section). OpenDAL's default retry then waits a full second per rejection.",
        causes: &[
            "more concurrent requests than a bucket, prefix or gateway accepts",
            "several operators or processes sharing one limit",
        ],
    },
    Entry {
        code: "rate_limit",
        title: "The service limits the request rate",
        severity: Severity::Critical,
        meaning: "The rejections follow the request rate (a token bucket), not the requests in flight: fewer workers alone do not help unless they lower the rate.",
        causes: &[
            "a request-rate quota on the account, bucket or prefix",
            "a rate that changes over time (partition scaling)",
        ],
    },
    Entry {
        code: "key_limit",
        title: "Hot keys exceed a per-key limit",
        severity: Severity::Critical,
        meaning: "A few keys or prefixes take most of the requests and are throttled while the other keys are served.",
        causes: &[
            "a skewed (zipf) access pattern",
            "every writer or reader using one prefix",
        ],
    },
    Entry {
        code: "bandwidth_cap",
        title: "Every stream shares one bandwidth cap",
        severity: Severity::Warning,
        meaning: "The aggregate throughput is flat while a stream's rate falls as more streams run beside it: a shared cap (a NIC, proxy, endpoint or bucket limit).",
        causes: &[
            "a network link, NAT gateway or proxy at its capacity",
            "a provider's per-bucket or per-connection bandwidth limit",
        ],
    },
    Entry {
        code: "tail_stragglers",
        title: "A few requests wait far longer for the first byte",
        severity: Severity::Warning,
        meaning: "A small share of requests waits much longer for the server's first byte than their peers; a replay of the requests through TailCutLayer's rule shows the cut would shorten the tail.",
        causes: &[
            "a slow or overloaded storage node",
            "server-side garbage collection or cold metadata",
        ],
    },
    Entry {
        code: "hung_requests",
        title: "Requests hang far beyond the median",
        severity: Severity::Warning,
        meaning: "Some requests took seconds and twenty times the median request or more, or failed with a timeout: an I/O timeout would retry them instead of waiting.",
        causes: &[
            "a stalled connection or a lost response",
            "a server that accepts a request and never answers",
        ],
    },
    Entry {
        code: "small_ranges",
        title: "Reads are many small ranges, or one serial stream",
        severity: Severity::Warning,
        meaning: "Either each read is several small range requests that each pay a round trip (coalesce them with `read.gap`), or each large read streams through one request at one stream's rate (split it with `read.concurrent`).",
        causes: &[
            "columnar formats reading many column chunks (parquet, ORC)",
            "large objects read without a chunked concurrent reader",
        ],
    },
    Entry {
        code: "slow_multipart",
        title: "Multipart parts upload one after another",
        severity: Severity::Warning,
        meaning: "Multipart writes upload several parts sequentially (`write.concurrent` is 1), so a write takes the sum of its parts.",
        causes: &["the writer's default concurrency of one"],
    },
    Entry {
        code: "repeated_reads",
        title: "The same bytes are read again and again",
        severity: Severity::Info,
        meaning: "Replaying the reads through an in-memory cache saves a large share of the read requests (judged only on real objects or a stated skew, never on a generated uniform dataset).",
        causes: &["hot objects (metadata, footers, small lookups) re-read by every task"],
    },
    Entry {
        code: "cold_connections",
        title: "Connection setup takes a large share of request time",
        severity: Severity::Warning,
        meaning: "Requests open new connections (DNS, TCP, TLS), warmup included: keep-alive is off, or HTTP/1.1 needs one connection per request in flight.",
        causes: &[
            "keep-alive switched off",
            "response bodies not drained, so connections are not reused",
            "HTTP/1.1 with many requests in flight",
        ],
    },
    Entry {
        code: "client_bound",
        title: "The client, not the service, sets the pace",
        severity: Severity::Warning,
        meaning: "The load generator's CPU or its scheduler is saturated (scheduling lag), or an open-loop schedule fell behind: its latency and throughput understate the service, and latency findings are dropped.",
        causes: &[
            "too few cores for the concurrency",
            "per-operation client work (checksums, decoding)",
        ],
    },
    Entry {
        code: "errors",
        title: "Requests or operations failed",
        severity: Severity::Warning,
        meaning: "Operations failed for reasons other than throttling, or the service answered 5xx (including 503s that follow no load pattern) that retries absorbed.",
        causes: &[
            "missing permissions or wrong credentials",
            "objects or buckets that do not exist",
            "an unhealthy service or a noisy neighbour",
        ],
    },
    Entry {
        code: "invalid_run",
        title: "The run cannot be judged on performance",
        severity: Severity::Warning,
        meaning: "No operation completed, more operations failed (for reasons other than throttling) than the SLA allows, or the window is shorter than the warmup. Performance findings are suppressed; throttling and errors are kept.",
        causes: &[
            "wrong credentials or endpoint",
            "a window too short for the workload",
        ],
    },
];

fn norm(code: &str) -> String {
    code.trim().to_ascii_lowercase().replace('-', "_")
}

fn doc(e: &Entry) -> FindingDoc {
    FindingDoc {
        code: e.code,
        title: e.title,
        typical_severity: e.severity,
        meaning: e.meaning,
        causes: e.causes,
        remedy: crate::analysis::findings::remedy(e.code, None),
    }
}

/// Every documented finding code, in catalog order.
pub fn all() -> Vec<FindingDoc> {
    ENTRIES.iter().map(doc).collect()
}

/// The codes, in catalog order.
pub fn codes() -> Vec<&'static str> {
    ENTRIES.iter().map(|e| e.code).collect()
}

/// Look up a finding code (`-` and `_` are interchangeable, case-insensitive).
pub fn get(code: &str) -> Option<FindingDoc> {
    let n = norm(code);
    ENTRIES.iter().find(|e| norm(e.code) == n).map(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every `code: "…"` literal of the findings engine (outside its tests).
    fn scanned_codes() -> BTreeSet<String> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/analysis/findings.rs");
        let src = std::fs::read_to_string(path).unwrap();
        let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        let mut out = BTreeSet::new();
        for (i, _) in src.match_indices("code: \"") {
            let rest = &src[i + 7..];
            out.insert(rest[..rest.find('"').unwrap()].to_owned());
        }
        out
    }

    #[test]
    fn catalog_lists_exactly_the_codes_the_engine_emits() {
        let catalog: BTreeSet<&str> = codes().into_iter().collect();
        let engine: BTreeSet<&str> = crate::analysis::findings::CODES.iter().copied().collect();
        assert_eq!(catalog, engine, "catalog vs findings::CODES");
        let scanned = scanned_codes();
        assert!(scanned.len() >= 10, "scan found too few codes: {scanned:?}");
        for c in &scanned {
            assert!(
                engine.contains(c.as_str()),
                "the engine emits `{c}`, which is not documented"
            );
        }
        // The design's table: the eleven remedy codes plus `errors` and `invalid_run`.
        assert_eq!(engine.len(), 13);
    }

    #[test]
    fn catalog_is_unique_and_names_remedies() {
        let mut seen = BTreeSet::new();
        for d in all() {
            assert!(seen.insert(norm(d.code)), "duplicate {}", d.code);
            assert!(!d.meaning.is_empty() && !d.causes.is_empty());
            assert!(!d.remedy.text.is_empty());
        }
        assert_eq!(get("TAIL-STRAGGLERS").unwrap().code, "tail_stragglers");
        assert_eq!(
            get("throttling").unwrap().remedy.path.as_deref(),
            Some("layers.concurrent_limit.http")
        );
        assert!(get("key_limit").unwrap().remedy.path.is_none());
        assert!(get("client_queue").is_none());
    }
}
