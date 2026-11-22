//! The finding-code catalog: one source of truth for what every diagnosis code means, why it
//! happens, how to verify it and which knobs relate to it. Used by `oprobe explain CODE`,
//! `oprobe capabilities`, the MCP `oprobe://docs/findings` resource and `explain_run`.
//!
//! Codes are `snake_case`. The catalog lists exactly the codes the findings engine can emit
//! ([`crate::analysis::findings::CODES`]; a test also scans both engines' sources for code
//! literals). Lookups accept `-` and `_` interchangeably.

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
    /// Area: `correctness`, `availability`, `bottleneck`, `network`, `efficiency`, `cost`,
    /// `sla`, `client`.
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
            "report.integrity (corrupted objects > 0)",
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
            "`oprobe analyze RUNREF --by status` (then `--where status=CODE`) / MCP analyze_run to see which requests failed",
        ],
        knobs: &["--retries", "--max-error-rate", "--concurrency"],
    },
    Entry {
        code: "throttling",
        title: "The service rate-limited requests (429/503)",
        severity: Severity::Critical,
        category: "availability",
        meaning: "The storage service answered with 429 or 503 SlowDown: the request rate is above what the bucket, prefix or account allows right now. The evidence names the in-flight level where throttling starts (the capacity model's ceiling, report.capacity) and the safe concurrency below it; when throttling does not follow concurrency, the request rate the service accepted and a rate to pace at.",
        causes: &[
            "too many concurrent requests against one prefix (S3 partitions scale per prefix)",
            "a request-rate quota on the account or bucket",
            "a burst right after the bucket was created (not yet scaled out)",
        ],
        verify: &[
            "report.resilience.throttled_responses and the retry share in report.stages.shares.retry_pct",
            "the finding's verification command re-runs at the suggested concurrency",
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
        code: "retries",
        title: "Operations spend their time waiting to retry",
        severity: Severity::Warning,
        category: "availability",
        meaning: "Requests fail with temporary errors (not throttling) and operations sleep between retry attempts: at least 5% of operation time is retry backoff.",
        causes: &[
            "timeouts or connection resets",
            "5xx errors from an unhealthy service",
            "a too aggressive timeout",
        ],
        verify: &[
            "report.stages.shares.retry_pct and report.resilience",
            "`oprobe analyze RUNREF --where attempt=retry --by status`",
        ],
        knobs: &["--retries", "--tail-cut"],
    },
    Entry {
        code: "tail_stragglers",
        title: "A few requests wait far longer for the first byte",
        severity: Severity::Warning,
        category: "bottleneck",
        meaning: "The slowest requests spend most of their time waiting for the server's first byte while the rest answer quickly: server-side stragglers. The evidence includes a replayed estimate of what a tail-cut (re-issuing slow requests) would do to p99.",
        causes: &[
            "server stragglers (slow disks, garbage collection, hot partitions)",
            "one slow front-end behind the endpoint",
            "background load on a shared service",
        ],
        verify: &[
            "the finding's evidence values (current and predicted p99 in ms)",
            "the verification command re-runs with `--tail-cut`; compare p99 with `oprobe compare`",
            "`oprobe analyze RUNREF --slowest 2 --by remote`",
        ],
        knobs: &["--tail-cut", "--retries", "--concurrency"],
    },
    Entry {
        code: "tail_latency",
        title: "p99 latency is far above the median",
        severity: Severity::Warning,
        category: "bottleneck",
        meaning: "Operation p99 is more than 5× the median; the finding names the request stage the slowest requests spend most of their time in.",
        causes: &[
            "cold connections or retries on a few requests",
            "throttling bursts",
            "a slow remote IP behind the endpoint",
            "large transfers on a few operations",
        ],
        verify: &[
            "report.slowest op ids and their request rows",
            "`oprobe analyze RUNREF --slowest 2 --by stage`",
        ],
        knobs: &["--tail-cut", "--retries", "--concurrency"],
    },
    Entry {
        code: "client_queue",
        title: "Operations wait for in-flight request permits",
        severity: Severity::Warning,
        category: "bottleneck",
        meaning: "Operations spend a large share of their time (at least 10%) queued in the client before a request is even sent.",
        causes: &[
            "max_inflight_requests caps requests below concurrency × io_concurrency",
            "a connection pool that is too small",
        ],
        verify: &[
            "report.stages.shares.queue_pct",
            "report.stages.request_p99.queue_ms",
            "re-run with `--max-inflight 0` and compare",
        ],
        knobs: &["--max-inflight", "--concurrency", "--io-concurrency"],
    },
    Entry {
        code: "cold_connections",
        title: "Connection setup costs a large share of request time",
        severity: Severity::Warning,
        category: "network",
        meaning: "Requests that open a new connection pay DNS, TCP, proxy and TLS handshakes; together they take at least 10% of request time, or connections are rarely reused. The evidence names the slowest handshake step.",
        causes: &[
            "keep-alive disabled",
            "response bodies not fully drained, so connections are not returned to the pool",
            "an idle pool too small for the concurrency",
            "a proxy that closes connections",
            "slow TLS handshakes (long round trip, TLS-inspecting proxy, no resumption)",
        ],
        verify: &[
            "report.connections (reuse_pct, dialed connections) and report.network.tcp / tls",
            "`oprobe analyze RUNREF --by conn`",
            "`oprobe probe ENDPOINT` for handshake times",
        ],
        knobs: &["--no-keep-alive (remove it)", "--http h2", "--microscope"],
    },
    Entry {
        code: "http1_connections",
        title: "HTTP/1.1 needs one connection per in-flight request",
        severity: Severity::Info,
        category: "network",
        meaning: "Nearly all requests use HTTP/1.1, so every in-flight request needs its own connection; HTTP/2 would multiplex them over a few connections.",
        causes: &[
            "the HTTP version left on auto against a service that negotiates HTTP/1.1",
            "a proxy that only speaks HTTP/1.1",
        ],
        verify: &[
            "report.connections (distinct connections vs requests)",
            "re-run with `--http h2` and `oprobe compare` (some services are faster on HTTP/1.1)",
        ],
        knobs: &["--http h1|h2", "--max-inflight"],
    },
    Entry {
        code: "remote_skew",
        title: "One remote address is much slower than the others",
        severity: Severity::Warning,
        category: "network",
        meaning: "Requests to one front-end IP take at least 3× longer (server + transfer time) than requests to the other remotes behind the same endpoint.",
        causes: &[
            "a degraded or overloaded front-end server",
            "a remote in another zone or region",
            "long DNS caching pinning connections to one address",
        ],
        verify: &[
            "report.network.remotes",
            "`oprobe analyze RUNREF --by remote` and `--where remote=IP`",
        ],
        knobs: &["--concurrency", "--tail-cut"],
    },
    Entry {
        code: "tcp_retransmits",
        title: "Packets were lost while requests were in flight",
        severity: Severity::Warning,
        category: "network",
        meaning: "The kernel's TCP statistics show retransmissions on a connection between consecutive samples, i.e. while a request was running on it. Loopback connections are not counted.",
        causes: &[
            "a congested NIC, proxy or VPN",
            "a lossy cross-region or internet link",
            "a receive window or buffer too small for the bandwidth",
        ],
        verify: &[
            "the finding's evidence: requests with retransmissions and the retransmitted segments",
            "`oprobe analyze RUNREF --by conn`; compare from another host or region",
        ],
        knobs: &["--region", "--proxy", "--concurrency"],
    },
    Entry {
        code: "high_rtt",
        title: "The network round trip is long",
        severity: Severity::Info,
        category: "network",
        meaning: "The kernel-measured TCP round-trip time to the endpoint is at least 50 ms at the median; every request pays at least one round trip.",
        causes: &[
            "the client runs far from the bucket's region",
            "a proxy or VPN detour",
        ],
        verify: &[
            "report.network.tcp (round-trip time)",
            "`oprobe probe ENDPOINT`",
        ],
        knobs: &["--region", "--concurrency", "--proxy"],
    },
    Entry {
        code: "consumer_stall",
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
            "`oprobe analyze RUNREF --slowest 2 --by stage` (stage=stall)",
        ],
        knobs: &["--io-concurrency", "--chunk-size"],
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
        code: "small_objects",
        title: "Small requests: round trips, not bandwidth, bound throughput",
        severity: Severity::Info,
        category: "bottleneck",
        meaning: "The mean request moves less than 128 KiB and most request time is spent waiting for the server's answer; throughput grows with parallelism, not bandwidth.",
        causes: &[
            "many small objects or small range reads",
            "too little parallelism for the per-request latency",
        ],
        verify: &[
            "report.apis (bytes per request) and report.stages.shares",
            "the verification command doubles the concurrency; compare throughput",
        ],
        knobs: &["--concurrency", "--range-gap", "--object-size"],
    },
    Entry {
        code: "suffix_read",
        title: "Use suffix reads: saves one HEAD per file",
        severity: Severity::Info,
        category: "efficiency",
        meaning: "File-scan operations issue a HEAD to learn the object size before reading the footer; a suffix range read returns the footer and the size in one request.",
        causes: &[
            "readers that stat a file before reading its tail (Parquet/ORC footers)",
            "a reader without suffix-range support enabled",
        ],
        verify: &[
            "report.apis: HeadObject requests close to the number of file scans",
            "`oprobe run TARGET --preset parquet-suffix` and `oprobe compare` against the parquet preset",
        ],
        knobs: &["--preset parquet-suffix"],
    },
    Entry {
        code: "cache_opportunity",
        title: "The same objects are read repeatedly",
        severity: Severity::Info,
        category: "efficiency",
        meaning: "Replaying the run through a 256 MiB read cache would save at least 200 requests per 1 000 operations: the working set is re-read.",
        causes: &[
            "a skewed (zipf, hot-set) access pattern",
            "repeated reads of the same footers or metadata",
        ],
        verify: &[
            "the finding's predicted request savings",
            "the verification command re-runs with `--cache 256MiB`",
        ],
        knobs: &["--cache SIZE", "--access"],
    },
    Entry {
        code: "cost_hotspot",
        title: "One API or wasted requests dominate the bill",
        severity: Severity::Info,
        category: "cost",
        meaning: "One API is at least 60% of the projected request spend, or at least 10% of the projected monthly bill buys nothing (failed or throttled requests, over-read egress).",
        causes: &[
            "many small requests of one kind (HEAD, LIST, small GETs)",
            "throttled or failed requests that are billed anyway",
            "over-read downloads",
        ],
        verify: &[
            "report.cost.by_api and report.cost.waste_usd",
            "`oprobe run TARGET --provider NAME --monthly-ops N` for your own volume",
        ],
        knobs: &["--range-gap", "--chunk-size", "--concurrency", "--provider"],
    },
    Entry {
        code: "slow_start",
        title: "The first seconds are much slower than the steady state",
        severity: Severity::Info,
        category: "sla",
        meaning: "The start segment of the run has at least twice the median latency of the steady segment; short runs are skewed by it.",
        causes: &[
            "cold connections, DNS and TLS at the start",
            "server-side scaling or caches warming up",
        ],
        verify: &[
            "`oprobe analyze RUNREF --by second`",
            "the verification command re-runs with a longer `--warmup`",
        ],
        knobs: &["--warmup", "--duration"],
    },
    Entry {
        code: "server_latency",
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
        code: "download_bound",
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
        code: "upload_bound",
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
        code: "client_cpu",
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
        code: "client_overhead",
        title: "Time is spent outside network requests",
        severity: Severity::Info,
        category: "client",
        meaning: "Most operation time is client-side processing (signing, buffering, OpenDAL, CRC), not network requests. Reported only when nothing more specific explains the run.",
        causes: &[
            "in-memory or local backends (expected)",
            "tiny objects where per-operation overhead dominates",
        ],
        verify: &["report.stages.shares.client_pct is the largest share"],
        knobs: &["--object-size", "--no-verify"],
    },
    Entry {
        code: "local_backend",
        title: "Local backend service time dominates",
        severity: Severity::Info,
        category: "bottleneck",
        meaning: "An in-process backend (memory, fs) is measured: results reflect local disk or memory, not network storage.",
        causes: &["benchmarking memory:// or fs:// (expected)"],
        verify: &["report.target.scheme is memory or fs"],
        knobs: &[],
    },
    Entry {
        code: "cache_ineffective",
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
        code: "sla_p99",
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
        code: "rate_not_sustained",
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
    Entry {
        code: "schedule_backlog",
        title: "The open-loop schedule fell behind",
        severity: Severity::Warning,
        category: "validity",
        meaning: "More than 1% of the open-loop arrivals never started before the window ended: their latency is unknown and counted in the tail as a lower bound (censored), so percentiles are \"≥\" values and throughput is what the client managed, not the target (no coordinated omission).",
        causes: &[
            "too few in-flight slots (`--concurrency`) for the rate or shape",
            "a rate above what the target serves",
            "a saturated load generator (report.self.client_saturated)",
        ],
        verify: &["report.schedule (intended, missed_arrivals_ratio, schedule_stage)"],
        knobs: &["--rate", "--shape", "--concurrency"],
    },
    Entry {
        code: "run_invalid",
        title: "The run cannot be judged on performance",
        severity: Severity::Warning,
        category: "validity",
        meaning: "No operation completed, more operations failed than the SLA allows, or the measurement window was shorter than the warmup: latency and throughput would describe failures or noise, so performance findings (latency, scaling, small objects, HTTP, cache, tail, cost) are suppressed.",
        causes: &[
            "failing requests (credentials, missing objects, an unsupported HTTP version)",
            "a measurement window shorter than the warmup",
        ],
        verify: &[
            "report.summary.error_ratio against report.sla.max_error_ratio",
            "report.run.window_s against report.workload.warmup_s",
        ],
        knobs: &["--duration", "--warmup", "--http"],
    },
    Entry {
        code: "run_partial",
        title: "The run stopped early",
        severity: Severity::Warning,
        category: "validity",
        meaning: "The run was interrupted (Ctrl-C, Stop), stopped by a safety budget or by an error before its configured end (report.run.partial). Interrupted runs and windows under a second have their performance findings suppressed; a partial run is never accepted as a baseline.",
        causes: &[
            "Ctrl-C or a cancelled job",
            "a budget (--max-requests, --max-bytes, --max-duration, --max-cost-usd)",
        ],
        verify: &[
            "report.run.partial (reason, measured_s) and report.warnings",
            "re-run with `oprobe run --from RUNREF`",
        ],
        knobs: &["--duration", "--max-requests", "--max-cost-usd"],
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
                    || c.split('_').any(|p| p.len() > 3 && n.contains(p)))
        })
        .map(|e| e.code)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// String literals in `src` that look like finding codes, found after `marker`
    /// (`finding(` calls in the 2.x diagnosis, `code: "…"` in the 3.0 engine).
    fn literals_after(src: &str, marker: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, _) in src.match_indices(marker) {
            if src[..i].ends_with("fn ") {
                continue;
            }
            let rest = &src[i + marker.len()..];
            let end = ["format!", ".into()", ".to_owned()", ".to_string()"]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .unwrap_or(rest.len().min(400));
            for lit in rest[..end].split('"').skip(1).step_by(2) {
                if !lit.is_empty()
                    && lit.chars().all(|c| {
                        c.is_ascii_lowercase() || c == '-' || c == '_' || c.is_ascii_digit()
                    })
                {
                    out.push(lit.to_owned());
                }
            }
        }
        out
    }

    /// Every code literal either engine can emit, scanned from the sources.
    fn scanned_codes() -> BTreeSet<String> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/analysis");
        let diagnosis = std::fs::read_to_string(dir.join("diagnosis.rs")).unwrap();
        let findings = std::fs::read_to_string(dir.join("findings.rs")).unwrap();
        let findings = &findings[..findings.find("#[cfg(test)]").unwrap_or(findings.len())];
        let mut out: BTreeSet<String> =
            literals_after(&diagnosis, "finding(").into_iter().collect();
        for m in ["code: \"", "code: String::from(\""] {
            for (i, _) in findings.match_indices(m) {
                let rest = &findings[i + m.len()..];
                out.insert(rest[..rest.find('"').unwrap()].to_owned());
            }
        }
        out
    }

    #[test]
    fn catalog_lists_exactly_the_codes_the_engines_emit() {
        let catalog: BTreeSet<&str> = codes().into_iter().collect();
        let engine: BTreeSet<&str> = crate::analysis::findings::CODES.iter().copied().collect();
        assert_eq!(catalog, engine, "catalog vs findings::CODES");
        let scanned = scanned_codes();
        assert!(scanned.len() >= 20, "scan found too few codes: {scanned:?}");
        for c in &scanned {
            assert!(
                engine.contains(c.as_str()),
                "engine source emits `{c}`, which is not canonical (findings::CODES) or not documented"
            );
        }
        for c in &engine {
            assert!(
                c.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit()),
                "`{c}` is not snake_case"
            );
        }
    }

    #[test]
    fn catalog_is_unique_and_complete() {
        let mut seen = BTreeSet::new();
        for d in all() {
            assert!(seen.insert(norm(d.code)), "duplicate {}", d.code);
            assert!(!d.meaning.is_empty() && !d.causes.is_empty() && !d.verify.is_empty());
        }
        assert_eq!(get("CLIENT_QUEUE").unwrap().code, "client_queue");
        assert_eq!(get("tail-stragglers").unwrap().code, "tail_stragglers");
        assert_eq!(get("tail_stragglers").unwrap().code, "tail_stragglers");
        assert!(get("nope").is_none());
        assert!(suggest("throttle").contains(&"throttling"));
    }
}
