//! Multi-factor white-box bottleneck root-cause attribution engine and
//! human-friendly runtime error diagnostic translator.

use serde::{Deserialize, Serialize};

use super::{StageWaterfall, TransportAndHostSnapshot, VerbMetricsRow};

/// Diagnostic severity level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Healthy,
    Advisory,
    Warning,
    Critical,
}

impl Severity {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Healthy => "HEALTHY",
            Self::Advisory => "ADVISORY",
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
        }
    }
}

/// Automated bottleneck diagnosis and actionable prescription.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BottleneckDiagnosis {
    pub severity: Severity,
    pub primary_stage: String,
    pub headline: String,
    pub recommendation: String,
    pub findings: Vec<String>,
}

impl BottleneckDiagnosis {
    pub fn evaluate(
        wf: &StageWaterfall,
        tp: &TransportAndHostSnapshot,
        verbs: &[VerbMetricsRow],
    ) -> Self {
        let mut findings = Vec::new();
        let total = wf.total_us().max(1) as f64;

        // 1. End-to-end CRC32C data corruption is always highest priority
        if tp.crc32c_corrupted > 0 {
            return Self {
                severity: Severity::Critical,
                primary_stage: "DataIntegrityFailure".to_owned(),
                headline: format!(
                    "Detected {} end-to-end CRC32C payload mismatch(es)",
                    tp.crc32c_corrupted
                ),
                recommendation:
                    "Inspect reverse proxies, multipart part ordering, or storage gateway bit-rot immediately."
                        .to_owned(),
                findings: vec![format!(
                    "Integrity red-line: {} corrupted vs {} verified clean payloads.",
                    tp.crc32c_corrupted, tp.crc32c_verified_ok
                )],
            };
        }

        // 2. HTTP 401/403 Auth or Permission failure
        if tp.http_403_auth_count > 0 {
            return Self {
                severity: Severity::Critical,
                primary_stage: "AuthenticationOrAcl".to_owned(),
                headline: format!(
                    "Storage gateway rejected {} request(s) with HTTP 401/403",
                    tp.http_403_auth_count
                ),
                recommendation:
                    "Verify access_key_id/secret_access_key, region signature scope, and bucket IAM policy."
                        .to_owned(),
                findings: vec![format!(
                    "Observed {} auth/permission rejections out of {} HTTP calls.",
                    tp.http_403_auth_count, tp.total_http_requests
                )],
            };
        }

        // 3. Remote 429 / 503 SlowDown partition rate-limiting
        let throttled = tp.http_503_slowdown_count + tp.http_429_count;
        if throttled > 0 {
            findings.push(format!(
                "Remote endpoint returned {} HTTP 429/503 rate-limit responses ({} retries, {:.1} ms total backoff).",
                throttled, tp.retry_attempts, tp.retry_backoff_ms
            ));
            return Self {
                severity: Severity::Critical,
                primary_stage: "RemotePartitionThrottle".to_owned(),
                headline: format!(
                    "Remote 429/503 rate-limiting triggered ({throttled} throttled responses)"
                ),
                recommendation:
                    "Run `oprobe tune` to back off ConcurrentLimitLayer below the partition cliff, shard key prefixes, and keep jittered RetryLayer enabled."
                        .to_owned(),
                findings,
            };
        }

        // 4. Connection pool thrashing / missing HTTP Keep-Alive
        if tp.total_http_requests >= 4 && tp.socket_reuse_pct < 65.0 {
            findings.push(format!(
                "Only {:.1}% of HTTP requests reused warm sockets ({} cold dials across {} requests; DNS/TCP/TLS avg {:.2} ms).",
                tp.socket_reuse_pct,
                tp.cold_connections_dialed,
                tp.total_http_requests,
                wf.dns_tcp_tls_us as f64 / 1000.0
            ));
            return Self {
                severity: Severity::Warning,
                primary_stage: "SocketPoolThrashing".to_owned(),
                headline: format!(
                    "Connection pool thrashing (socket reuse {:.1}%)",
                    tp.socket_reuse_pct
                ),
                recommendation:
                    "Enable `keep_alive = true`, allow HTTP/2 multiplexing (`h2`), and size idle pool >= 2x concurrency."
                        .to_owned(),
                findings,
            };
        }

        // 5. Client concurrency semaphore starvation
        let queue_share = wf.client_queue_us as f64 / total;
        if queue_share > 0.35 && wf.client_queue_us > 1_000 {
            findings.push(format!(
                "Client semaphore queue wait consumes {:.1}% ({:.2} ms) of mean operation latency.",
                queue_share * 100.0,
                wf.client_queue_us as f64 / 1000.0
            ));
            return Self {
                severity: Severity::Warning,
                primary_stage: "ClientSemaphoreStarvation".to_owned(),
                headline: format!(
                    "Client concurrency permit starvation (queueing {:.0}%)",
                    queue_share * 100.0
                ),
                recommendation:
                    "Increase `concurrent` permits incrementally (or run `oprobe tune`) until gateway TTFB begins to inflect."
                        .to_owned(),
                findings,
            };
        }

        // 6. Multipart upload server merge/commit bottleneck
        let merge_share = wf.multipart_merge_us as f64 / total;
        if merge_share > 0.35 && wf.multipart_merge_us > 10_000 {
            findings.push(format!(
                "Multipart CompleteMultipartUpload merge barrier accounts for {:.1}% ({:.2} ms) across {} completions.",
                merge_share * 100.0,
                wf.multipart_merge_us as f64 / 1000.0,
                tp.multipart_completions
            ));
            return Self {
                severity: Severity::Advisory,
                primary_stage: "MultipartFinalizeBarrier".to_owned(),
                headline: format!(
                    "Multipart merge commit dominates write latency ({:.1} ms)",
                    wf.multipart_merge_us as f64 / 1000.0
                ),
                recommendation:
                    "Increase `chunk_bytes` (e.g., 8 MiB - 16 MiB) to reduce part count per CompleteMultipartUpload manifest."
                        .to_owned(),
                findings,
            };
        }

        // 7. Excessive read amplification from aggressive range-gap coalescing
        if tp.read_amplification > 2.10 {
            findings.push(format!(
                "Storage fetched {:.2}x more bytes than logically requested ({} KiB wasted in coalesced range gaps).",
                tp.read_amplification,
                tp.coalesced_gap_waste_bytes / 1024
            ));
            return Self {
                severity: Severity::Advisory,
                primary_stage: "HighReadAmplification".to_owned(),
                headline: format!(
                    "High sparse read amplification ({:.2}x bandwidth overfetch)",
                    tp.read_amplification
                ),
                recommendation:
                    "Tighten `range_gap_bytes` (e.g., 64 KiB - 256 KiB) to stop pulling large dead gaps between sparse Parquet columns."
                        .to_owned(),
                findings,
            };
        }

        // 8. Excessive RPC split amplification inflating Class A/B API bills
        if tp.rpc_amplification > 4.50 {
            findings.push(format!(
                "Each logical operation splits into {:.2} storage RPCs ({} Class A, {} Class B), inflating API fees & header overhead.",
                tp.rpc_amplification, tp.class_a_rpcs, tp.class_b_rpcs
            ));
            return Self {
                severity: Severity::Advisory,
                primary_stage: "HighRpcAmplification".to_owned(),
                headline: format!(
                    "High request split amplification ({:.2}x RPCs/op)",
                    tp.rpc_amplification
                ),
                recommendation:
                    "Increase `chunk_bytes` (4 MiB - 16 MiB) or widen `range_gap_bytes` moderately to coalesce adjacent column reads."
                        .to_owned(),
                findings,
            };
        }

        // 9. Severe P99 / P50 tail straggler skew or high gateway TTFB
        let worst_skew = verbs
            .iter()
            .filter(|v| v.count >= 4 && v.p50_ms >= 1.0)
            .map(|v| (v.verb.as_str(), v.p99_ms / v.p50_ms.max(0.1), v.p99_ms))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((verb_name, skew, p99)) = worst_skew {
            if skew >= 4.0 && p99 >= 20.0 {
                findings.push(format!(
                    "Verb `{verb_name}` exhibits {:.1}x P99/P50 long-tail skew (P99 {:.2} ms; TailCut cancellations: {}).",
                    skew, p99, tp.tail_cut_cancels
                ));
                return Self {
                    severity: Severity::Advisory,
                    primary_stage: "LongTailStragglers".to_owned(),
                    headline: format!(
                        "Long-tail latency skew on `{verb_name}` (P99 {:.1} ms, {:.1}x P50)",
                        p99, skew
                    ),
                    recommendation:
                        "Enable `TailCutLayer` (`enable_tail_cut = true`) with RetryLayer to cancel & hedge slow storage nodes, and enable local cache for hot keys."
                            .to_owned(),
                    findings,
                };
            }
        }

        let ttfb_share = wf.remote_ttfb_us as f64 / total;
        if ttfb_share > 0.55 && wf.remote_ttfb_us > 15_000 {
            findings.push(format!(
                "Remote gateway TTFB accounts for {:.1}% ({:.2} ms) while client queueing and socket reuse ({:.1}%) are healthy.",
                ttfb_share * 100.0,
                wf.remote_ttfb_us as f64 / 1000.0,
                tp.socket_reuse_pct
            ));
            return Self {
                severity: Severity::Advisory,
                primary_stage: "RemoteGatewayTtfb".to_owned(),
                headline: format!(
                    "Latency dominated by remote gateway TTFB ({:.1} ms)",
                    wf.remote_ttfb_us as f64 / 1000.0
                ),
                recommendation:
                    "Co-locate compute in the same cloud region, enable local `WhiteboxCacheLayer` for hot prefixes, and keep `TailCutLayer` active."
                        .to_owned(),
                findings,
            };
        }

        if tp.cache_lookups > 0 && tp.cache_hit_ratio_pct > 25.0 {
            findings.push(format!(
                "Local cache served {:.1}% of lookups ({:.2}x net wire-to-logical read ratio).",
                tp.cache_hit_ratio_pct, tp.net_wire_read_ratio
            ));
        }
        findings.push(format!(
            "Pipeline balanced: socket reuse {:.1}%, read amp {:.2}x, RPC amp {:.2}x, RSS {} MiB.",
            tp.socket_reuse_pct, tp.read_amplification, tp.rpc_amplification, tp.process_rss_mib
        ));

        Self {
            severity: Severity::Healthy,
            primary_stage: "BalancedPipeline".to_owned(),
            headline: "Storage stack pipeline balanced · no single-stage bottleneck".to_owned(),
            recommendation:
                "Configuration is production-ready; export snapshot as CI baseline for `oprobe diff`."
                    .to_owned(),
            findings,
        }
    }
}

/// Structured, user-friendly diagnostic explanation when a CLI command fails at runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionableErrorHint {
    pub category: String,
    pub summary: String,
    pub likely_cause: String,
    pub remediation_steps: Vec<String>,
}

/// Classify raw OpenDAL / Hyper / TLS / DNS errors into clear operator guidance.
pub fn diagnose_runtime_error(err_chain: &str, scheme: &str) -> ActionableErrorHint {
    let lower = err_chain.to_ascii_lowercase();

    if lower.contains("certificate")
        || lower.contains("unknownissuer")
        || lower.contains("invalidpeer")
        || lower.contains("tls")
    {
        return ActionableErrorHint {
            category: "TLS / Certificate Verification Error".to_owned(),
            summary: "TLS handshake with the storage endpoint failed certificate validation.".to_owned(),
            likely_cause:
                "Target uses a private/self-signed CA (e.g., MinIO, Ceph RGW, enterprise proxy) or hostname does not match certificate SAN."
                    .to_owned(),
            remediation_steps: vec![
                "Pass `--ca-cert /path/to/ca.pem` (or `ca_cert_path` in profile `[target.options]`) to trust your private CA.".to_owned(),
                "For local development/testing only, pass `--insecure` (or `allow_insecure_tls = \"true\"`).".to_owned(),
                "Run `oprobe net-probe <endpoint> --insecure` to inspect negotiated TLS version, cipher, and certificate chain.".to_owned(),
            ],
        };
    }

    if lower.contains("403")
        || lower.contains("signaturedoesnotmatch")
        || lower.contains("invalidaccesskeyid")
        || lower.contains("accessdenied")
        || lower.contains("permissiondenied")
    {
        return ActionableErrorHint {
            category: "Storage Authentication / IAM Denied (HTTP 401/403)".to_owned(),
            summary: format!("Backend `{scheme}` rejected credentials or bucket/prefix permissions."),
            likely_cause:
                "Missing/incorrect access key, wrong signing `region`, virtual-host vs path-style mismatch, or missing `s3:ListBucket`/`s3:PutObject` IAM permission."
                    .to_owned(),
            remediation_steps: vec![
                "Check `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (or `[target.options]` keys `access_key_id`, `secret_access_key`, `region`).".to_owned(),
                "For S3-compatible gateways (MinIO, Ceph, local labs), set `enable_virtual_host_style = \"false\"` in `[target.options]`.".to_owned(),
                "Ensure the target prefix allows read, write, list, and delete for benchmark & auto-cleanup.".to_owned(),
            ],
        };
    }

    if lower.contains("nosuchbucket") || (lower.contains("404") && lower.contains("bucket")) {
        return ActionableErrorHint {
            category: "Target Bucket Not Found (HTTP 404)".to_owned(),
            summary: "The configured bucket or container does not exist on the endpoint.".to_owned(),
            likely_cause:
                "Bucket name typo, wrong endpoint URL, or missing `enable_virtual_host_style = \"false\"` on IP-based endpoints."
                    .to_owned(),
            remediation_steps: vec![
                "Create the bucket first or verify `bucket = \"...\"` in your profile / `--url s3://<bucket>/prefix`.".to_owned(),
                "If connecting to an IP:port endpoint, use path-style addressing (`enable_virtual_host_style = \"false\"`).".to_owned(),
            ],
        };
    }

    if lower.contains("429")
        || lower.contains("503")
        || lower.contains("slowdown")
        || lower.contains("ratelimited")
    {
        return ActionableErrorHint {
            category: "Remote Partition Rate Limit Exhausted (HTTP 429/503)".to_owned(),
            summary: "Remote object storage throttled requests faster than RetryLayer could recover.".to_owned(),
            likely_cause:
                "Concurrency (`--concurrent`) exceeds the storage partition QPS/bandwidth quota."
                    .to_owned(),
            remediation_steps: vec![
                "Run `oprobe tune` — the auto-tuner automatically detects 429/503 cliffs and converges on the highest unthrottled concurrency.".to_owned(),
                "Or lower `--concurrent` (e.g., `--concurrent 8`) and increase `--chunk-kib` to reduce QPS.".to_owned(),
            ],
        };
    }

    if lower.contains("dns")
        || lower.contains("failed to resolve")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
    {
        return ActionableErrorHint {
            category: "DNS Resolution Failure".to_owned(),
            summary: "Could not resolve the storage endpoint hostname.".to_owned(),
            likely_cause:
                "Invalid endpoint hostname, virtual-host bucket prefix prepended to an unresolvable domain, or offline DNS."
                    .to_owned(),
            remediation_steps: vec![
                "Run `oprobe net-probe <endpoint>` to isolate DNS lookup latency and IP resolution.".to_owned(),
                "If OpenDAL prepended `<bucket>.` to a custom endpoint, set `enable_virtual_host_style = \"false\"`.".to_owned(),
            ],
        };
    }

    if lower.contains("connection refused") || lower.contains("timed out") || lower.contains("connect") {
        return ActionableErrorHint {
            category: "Network Connectivity / Endpoint Unreachable".to_owned(),
            summary: "TCP connection to the storage endpoint timed out or was refused.".to_owned(),
            likely_cause:
                "Endpoint service is down, port/firewall blocked, or wrong `endpoint` port."
                    .to_owned(),
            remediation_steps: vec![
                "Verify endpoint reachability with `oprobe net-probe <endpoint>`.".to_owned(),
                "For offline local evaluation, run `oprobe run` (in-memory) or start a local S3 chaos lab with `oprobe serve-s3`.".to_owned(),
            ],
        };
    }

    ActionableErrorHint {
        category: "Configuration / Storage Execution Error".to_owned(),
        summary: "Operation aborted due to a configuration or backend error.".to_owned(),
        likely_cause: "Invalid profile TOML syntax, unsupported backend option, or transient I/O failure."
            .to_owned(),
        remediation_steps: vec![
            "Inspect the underlying error cause printed above.".to_owned(),
            "Compare your TOML configuration against `profiles/s3-prod.toml` or run `oprobe --help`.".to_owned(),
        ],
    }
}
