//! The finding-code catalog: one source of truth for what every diagnosis code means, why it
//! happens, how to verify it and which knobs relate to it. Used by `oprobe explain CODE`,
//! `oprobe capabilities`, the MCP `oprobe://docs/findings` resource and `explain_run`.
//!
//! Every code `analysis::diagnosis` can emit must have an entry (a test scans the diagnosis
//! source). Lookups accept `-` and `_` interchangeably.

use schemars::JsonSchema;
use serde::Serialize;

use crate::analysis::diagnosis::Severity;

/// Where the finding codes are documented.
pub const DOCS_BASE: &str = "https://github.com/hxddh/oprobe/blob/main/docs/agents.md";

/// Documentation of one finding code.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct FindingDoc {
    /// Stable code as it appears in `report.findings[].code`.
    pub code: &'static str,
    /// Short title.
    pub title: &'static str,
    /// Severity the diagnosis usually assigns (`critical` codes can also be `warning`).
    pub typical_severity: Severity,
    /// Area: `correctness`, `availability`, `bottleneck`, `network`, `efficiency`, `sla`,
    /// `client`.
    pub category: &'static str,
    /// What the finding means.
    pub meaning: &'static str,
    /// Typical causes, most common first.
    pub causes: &'static [&'static str],
    /// How to confirm it (report fields, commands).
    pub verify: &'static [&'static str],
    /// Related knobs, flags and profile settings.
    pub knobs: &'static [&'static str],
    /// Documentation link.
    pub docs: String,
}

struct Entry {
    code: &'static str,
    title: &'static str,
    severity: Severity,
    category: &'static str,
    meaning: &'static str,
    causes: &'static [&'static str],
    verify: &'static [&'static str],
    knobs: &'static [&'static str],
}

const ENTRIES: &[Entry] = &[
    Entry {
        code: "integrity",
        title: "Data read back differs from data written",
        severity: Severity::Critical,
        category: "correctness",
        meaning: "CRC32C verification of objects oprobe wrote and read back failed: the storage path returned different bytes.",
        causes: &[
            "a proxy or gateway that rewrites or truncates bodies",
            "multipart parts assembled in the wrong order by an S3-compatible service",
            "a storage service bug or degraded node",
        ],
        verify: &[
            "report.integrity.corrupted > 0",
            "repeat with `oprobe run TARGET --preset checkpoint` and compare integrity counts",
            "run `oprobe compat TARGET` for multipart and checksum edge cases",
        ],
        knobs: &[
            "--no-verify (disables the check; never to hide this)",
            "--chunk-size",
            "--proxy",
        ],
    },
    Entry {
        code: "errors",
        title: "Operations failed",
        severity: Severity::Warning,
        category: "availability",
        meaning: "Some logical operations failed after retries; critical when the failure rate exceeds the SLA's max error rate.",
        causes: &[
            "missing permissions (403) or wrong credentials",
            "objects or buckets that do not exist (404)",
            "timeouts or connection resets on an overloaded path",
            "throttling that outlasts the retry budget",
        ],
        verify: &[
            "report.resilience.error_classes and report.summary.error_ratio",
            "`oprobe doctor TARGET` for credentials, permissions and the endpoint",
            "`oprobe analyze RUNREF --where error` / MCP analyze_run to see which requests failed",
        ],
        knobs: &["--retries", "--max-error-rate", "--concurrency"],
    },
    Entry {
        code: "throttling",
        title: "The service rate-limited requests (429/503)",
        severity: Severity::Critical,
        category: "availability",
        meaning: "The storage service answered with 429 or 503 SlowDown: the request rate is above what the bucket, prefix or account allows right now.",
        causes: &[
            "too many concurrent requests against one prefix (S3 partitions scale per prefix)",
            "a request-rate quota on the account or bucket",
            "a burst right after the bucket was created (not yet scaled out)",
        ],
        verify: &[
            "report.resilience.throttled_responses and the retry share in report.stages.shares.retry_pct",
            "re-run with half the concurrency and compare (`oprobe compare`)",
            "`oprobe tune TARGET` finds the throttling ceiling",
        ],
        knobs: &[
            "--concurrency",
            "--max-inflight",
            "--retries",
            "--max-throttle-rate",
        ],
    },
    Entry {
        code: "suffix-reads",
        title: "Use suffix reads: saves one HEAD per file",
        severity: Severity::Info,
        category: "cost",
        meaning: "File-scan operations issue a HEAD to learn the object size before reading the footer; a suffix range read returns the footer and the size in one request.",
        causes: &[
            "readers that stat a file before reading its tail (Parquet/ORC footers)",
            "a reader without suffix-range support enabled",
        ],
        verify: &[
            "report.apis: HeadObject count close to the number of file scans",
            "`oprobe run TARGET --preset parquet-suffix` and `oprobe compare` against the parquet preset",
        ],
        knobs: &["--preset parquet-suffix"],
    },
    Entry {
        code: "client-queue",
        title: "Operations wait for in-flight request permits",
        severity: Severity::Warning,
        category: "bottleneck",
        meaning: "Operations spend most of their time queued in the client before a request is even sent.",
        causes: &[
            "max_inflight_requests caps requests below concurrency × io_concurrency",
            "a connection pool that is too small",
        ],
        verify: &[
            "report.stages.shares.queue_pct is the largest share",
            "report.stages.request_p99.queue_ms",
            "re-run with `--max-inflight 0` and compare",
        ],
        knobs: &["--max-inflight", "--concurrency", "--io-concurrency"],
    },
    Entry {
        code: "client-cpu",
        title: "Load generator CPU is saturated",
        severity: Severity::Warning,
        category: "client",
        meaning: "The machine running oprobe is near its CPU limit, so measurements understate what the storage can do.",
        causes: &[
            "CRC32C verification of large objects",
            "TLS on many connections from a small machine",
            "very small operations at high concurrency",
        ],
        verify: &[
            "report.client (CPU use vs cores)",
            "re-run with `--no-verify` or fewer workers and compare throughput",
        ],
        knobs: &["--no-verify", "--concurrency", "--object-size"],
    },
    Entry {
        code: "consumer-stall",
        title: "Response bodies wait for the consumer",
        severity: Severity::Warning,
        category: "client",
        meaning: "Data arrives faster than the application side consumes it; storage is not the limit.",
        causes: &[
            "slow decoding or copying of downloaded data",
            "a CPU-bound consumer",
        ],
        verify: &[
            "report.stages.request_p99.stall_ms larger than body_ms",
            "MCP analyze_run: `stage=stall` over-represented among slow requests",
        ],
        knobs: &["--io-concurrency", "--chunk-size"],
    },
    Entry {
        code: "client-overhead",
        title: "Time is spent outside network requests",
        severity: Severity::Info,
        category: "client",
        meaning: "Most operation time is client-side processing (signing, buffering, OpenDAL, CRC), not network requests.",
        causes: &[
            "in-memory or local backends (expected)",
            "tiny objects where per-operation overhead dominates",
        ],
        verify: &["report.stages.shares.client_pct is the largest share"],
        knobs: &["--object-size", "--no-verify"],
    },
    Entry {
        code: "connect",
        title: "Connection setup dominates",
        severity: Severity::Warning,
        category: "network",
        meaning: "DNS, TCP, proxy and TLS handshakes take most of the operation time: connections are not reused.",
        causes: &[
            "keep-alive disabled",
            "response bodies not fully drained, so connections are not returned to the pool",
            "an idle pool too small for the concurrency",
            "a proxy that closes connections",
        ],
        verify: &[
            "report.connections (reuse ratio, dialed count) and report.network.tcp / tls",
            "MCP analyze_run: `connection=cold` over-represented among slow requests",
            "`oprobe probe ENDPOINT` for handshake times",
        ],
        knobs: &["--no-keep-alive (remove it)", "--http h2", "--microscope"],
    },
    Entry {
        code: "server-latency",
        title: "Latency-bound on server time-to-first-byte",
        severity: Severity::Info,
        category: "bottleneck",
        meaning: "Requests spend most of their time waiting for the server's first byte; throughput scales with parallelism.",
        causes: &[
            "network round trips to a distant region",
            "per-request service latency of the storage (normal for object stores)",
            "many small range reads that could be coalesced",
        ],
        verify: &[
            "report.stages.request_p99.ttfb_ms dominates",
            "`oprobe probe ENDPOINT` cold vs warm TTFB",
            "re-run with higher `--concurrency` and compare throughput",
        ],
        knobs: &[
            "--concurrency",
            "--io-concurrency",
            "--range-gap",
            "--region",
        ],
    },
    Entry {
        code: "download-bound",
        title: "Bandwidth-bound on downloads",
        severity: Severity::Info,
        category: "bottleneck",
        meaning: "Transferring response bodies dominates; the per-stream or aggregate bandwidth is the limit.",
        causes: &[
            "NIC, VM or proxy bandwidth caps",
            "cross-region or internet links",
            "too few parallel streams for large objects",
        ],
        verify: &[
            "report.stages.shares.download_pct is the largest share",
            "report.summary (MiB/s) against the machine's network limit",
        ],
        knobs: &["--io-concurrency", "--concurrency", "--chunk-size"],
    },
    Entry {
        code: "upload-bound",
        title: "Bandwidth-bound on uploads",
        severity: Severity::Info,
        category: "bottleneck",
        meaning: "Sending request bodies dominates; upload bandwidth is the limit.",
        causes: &[
            "NIC, VM or proxy bandwidth caps",
            "single-stream uploads of large objects",
        ],
        verify: &["report.stages.shares.upload_pct is the largest share"],
        knobs: &["--io-concurrency", "--chunk-size", "--concurrency"],
    },
    Entry {
        code: "retry-backoff",
        title: "Operations spend their time waiting to retry",
        severity: Severity::Warning,
        category: "availability",
        meaning: "Requests fail with temporary errors (not throttling) and operations sleep between retry attempts.",
        causes: &[
            "timeouts or connection resets",
            "5xx errors from an unhealthy service",
            "a too aggressive timeout",
        ],
        verify: &[
            "report.stages.shares.retry_pct and report.resilience",
            "MCP analyze_run: `attempt=retry` and `error=…` among slow requests",
        ],
        knobs: &["--retries", "--tail-cut"],
    },
    Entry {
        code: "local-backend",
        title: "Local backend service time dominates",
        severity: Severity::Info,
        category: "bottleneck",
        meaning: "An in-process backend (memory, fs) is measured: results reflect local disk or memory, not network storage.",
        causes: &["benchmarking memory:// or fs:// (expected)"],
        verify: &["report.target.scheme is memory or fs"],
        knobs: &[],
    },
    Entry {
        code: "connection-churn",
        title: "Connections are rarely reused",
        severity: Severity::Warning,
        category: "network",
        meaning: "Many requests open a new connection even though connection setup is not (yet) the main bottleneck.",
        causes: &[
            "keep-alive disabled or ignored by a proxy",
            "response bodies not drained",
            "a pool smaller than the concurrency",
        ],
        verify: &[
            "report.connections: dialed count vs requests",
            "MCP analyze_run: `connection=cold` lift among slow requests",
        ],
        knobs: &["--no-keep-alive (remove it)", "--http h2", "--max-inflight"],
    },
    Entry {
        code: "slow-tls",
        title: "Slow TLS handshakes",
        severity: Severity::Info,
        category: "network",
        meaning: "TLS handshakes take over 100 ms at the median.",
        causes: &[
            "a long round trip to the endpoint",
            "a TLS-inspecting proxy",
            "no session resumption",
        ],
        verify: &[
            "report.network.tls.handshake",
            "`oprobe probe ENDPOINT` (handshake breakdown, proxy)",
        ],
        knobs: &["--http h2", "--proxy"],
    },
    Entry {
        code: "overread",
        title: "Downloaded bytes were never requested",
        severity: Severity::Warning,
        category: "efficiency",
        meaning: "Range coalescing or chunking downloaded noticeably more bytes than the application asked for, costing egress and bandwidth.",
        causes: &[
            "a large range_gap merging distant ranges",
            "a chunk size larger than the reads",
        ],
        verify: &["report.bytes (overread ratio, backend bytes down)"],
        knobs: &["--range-gap", "--chunk-size"],
    },
    Entry {
        code: "cache-ineffective",
        title: "The read cache rarely hits",
        severity: Severity::Info,
        category: "efficiency",
        meaning: "The in-memory read cache is enabled but almost every lookup misses.",
        causes: &[
            "an access pattern with little reuse",
            "a working set larger than the cache",
        ],
        verify: &["report.cache (lookups, hits, hit ratio)"],
        knobs: &["--cache SIZE", "--access zipf:1.1"],
    },
    Entry {
        code: "tail-latency",
        title: "p99 latency is far above the median",
        severity: Severity::Warning,
        category: "bottleneck",
        meaning: "An operation's p99 is more than 5× its median: a minority of requests are much slower.",
        causes: &[
            "server stragglers",
            "cold connections or retries on a few requests",
            "throttling bursts",
            "a slow remote IP behind the endpoint",
        ],
        verify: &[
            "report.slowest op ids and their request rows",
            "`oprobe explain RUNREF` / MCP analyze_run with the slowest 1 % selected",
        ],
        knobs: &["--tail-cut", "--retries", "--concurrency"],
    },
    Entry {
        code: "sla-p99",
        title: "p99 latency exceeds the SLA",
        severity: Severity::Warning,
        category: "sla",
        meaning: "An operation's p99 is above the `--p99-ms` objective.",
        causes: &[
            "queueing or throttling at this concurrency",
            "tail latency of the service",
        ],
        verify: &["report.verbs[].latency vs report.sla.p99_ms"],
        knobs: &[
            "--p99-ms",
            "--concurrency",
            "oprobe tune --objective latency",
        ],
    },
    Entry {
        code: "rate-not-sustained",
        title: "The target arrival rate was not sustained",
        severity: Severity::Warning,
        category: "client",
        meaning: "With an open-loop `--rate`, arrivals queued behind busy workers; their wait is part of the measured latency.",
        causes: &[
            "too few workers (`--concurrency`) for the rate",
            "a rate above what the target can serve",
        ],
        verify: &["report.open_loop (target vs achieved rate, missed arrivals)"],
        knobs: &["--rate", "--concurrency"],
    },
];

fn norm(code: &str) -> String {
    code.trim().to_ascii_lowercase().replace('_', "-")
}

fn doc(e: &Entry) -> FindingDoc {
    FindingDoc {
        code: e.code,
        title: e.title,
        typical_severity: e.severity,
        category: e.category,
        meaning: e.meaning,
        causes: e.causes,
        verify: e.verify,
        knobs: e.knobs,
        docs: format!("{DOCS_BASE}#finding-{}", e.code),
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

/// Codes close to `code` (for "did you mean").
pub fn suggest(code: &str) -> Vec<&'static str> {
    let n = norm(code);
    ENTRIES
        .iter()
        .filter(|e| {
            let c = norm(e.code);
            let common = c.chars().zip(n.chars()).take_while(|(a, b)| a == b).count();
            !n.is_empty()
                && (c.contains(&n)
                    || n.contains(&c)
                    || common >= 5
                    || c.split('-').any(|p| p.len() > 3 && n.contains(p)))
        })
        .map(|e| e.code)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding codes the diagnosis source can emit: the first lowercase identifier literals
    /// passed to `finding(` before its formatted title.
    fn emitted_codes() -> Vec<String> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/analysis");
        let mut out = Vec::new();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "rs"))
            .collect();
        files.sort();
        for f in files {
            let src = std::fs::read_to_string(&f).unwrap();
            for (i, _) in src.match_indices("finding(") {
                // Skip the definition `fn finding(`.
                if src[..i].ends_with("fn ") {
                    continue;
                }
                let rest = &src[i + "finding(".len()..];
                let end = ["format!", ".into()", ".to_owned()", ".to_string()"]
                    .iter()
                    .filter_map(|m| rest.find(m))
                    .min()
                    .unwrap_or(rest.len().min(400));
                for lit in rest[..end].split('"').skip(1).step_by(2) {
                    if !lit.is_empty()
                        && lit
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
                    {
                        out.push(lit.to_owned());
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn every_emitted_code_is_documented() {
        let emitted = emitted_codes();
        assert!(emitted.len() >= 15, "scan found too few codes: {emitted:?}");
        for c in &emitted {
            assert!(get(c).is_some(), "finding code `{c}` has no catalog entry");
        }
    }

    #[test]
    fn catalog_is_unique_and_complete() {
        let mut seen = std::collections::BTreeSet::new();
        for d in all() {
            assert!(seen.insert(norm(d.code)), "duplicate {}", d.code);
            assert!(!d.meaning.is_empty() && !d.causes.is_empty() && !d.verify.is_empty());
        }
        assert_eq!(get("CLIENT_QUEUE").unwrap().code, "client-queue");
        assert!(get("nope").is_none());
        assert!(suggest("throttle").contains(&"throttling"));
    }
}
