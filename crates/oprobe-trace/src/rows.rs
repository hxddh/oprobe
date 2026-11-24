//! Per-request and per-operation trace rows (`ReqRow`, `OpRow`; design §5).
//!
//! All times are microseconds relative to the start of the measurement window. Rows are what
//! a run file stores (columnar `segment` blocks of `oprobe.run/5`) and what `analyze`,
//! exporters and the app's Requests table read.

use std::borrow::Cow;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};


/// Why a request or an operation failed: the stable dotted codes a trace row carries
/// (`storage.throttled`, `net.timeout`, …). Most are the oprobe CLI's error codes and serialize
/// to the same strings, so files stay readable when the CLI adds codes; the conditional-request
/// outcomes (`storage.condition_not_match`, `storage.already_exists`, `storage.conflict`,
/// `storage.range_not_satisfied`: OpenDAL's error kinds, design v7 B6) are the trace's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum FailureCode {
    /// No response: the connection could not be made or broke.
    #[serde(rename = "net.unreachable")]
    NetUnreachable,
    /// The request or I/O timed out (including a tail-cut).
    #[serde(rename = "net.timeout")]
    NetTimeout,
    /// 401 or 403, or a permission error.
    #[serde(rename = "auth.denied")]
    AuthDenied,
    /// 404.
    #[serde(rename = "storage.not_found")]
    StorageNotFound,
    /// 429 or 503 (`SlowDown`, `TooManyRequests`, …).
    #[serde(rename = "storage.throttled")]
    StorageThrottled,
    /// The service does not support the operation.
    #[serde(rename = "storage.unsupported")]
    StorageUnsupported,
    /// A conditional request's condition did not hold: 412 (`If-Match`, `If-None-Match`;
    /// OpenDAL `ConditionNotMatch`).
    #[serde(rename = "storage.condition_not_match")]
    StorageConditionNotMatch,
    /// The object already exists (a create-only write; OpenDAL `AlreadyExists`).
    #[serde(rename = "storage.already_exists")]
    StorageAlreadyExists,
    /// 409: a conflicting concurrent change (OpenDAL `Conflict`).
    #[serde(rename = "storage.conflict")]
    StorageConflict,
    /// 416: the requested range is outside the object (OpenDAL `RangeNotSatisfied`).
    #[serde(rename = "storage.range_not_satisfied")]
    StorageRangeNotSatisfied,
    /// The operator is misconfigured.
    #[serde(rename = "config.invalid")]
    ConfigInvalid,
    /// Anything else (other 4xx/5xx, unexpected errors).
    #[serde(rename = "internal")]
    Internal,
}

impl FailureCode {
    /// The dotted string (`storage.throttled`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NetUnreachable => "net.unreachable",
            Self::NetTimeout => "net.timeout",
            Self::AuthDenied => "auth.denied",
            Self::StorageNotFound => "storage.not_found",
            Self::StorageThrottled => "storage.throttled",
            Self::StorageUnsupported => "storage.unsupported",
            Self::StorageConditionNotMatch => "storage.condition_not_match",
            Self::StorageAlreadyExists => "storage.already_exists",
            Self::StorageConflict => "storage.conflict",
            Self::StorageRangeNotSatisfied => "storage.range_not_satisfied",
            Self::ConfigInvalid => "config.invalid",
            Self::Internal => "internal",
        }
    }

    /// The code of an operation that failed with error class `class` (`rate_limited`,
    /// `timeout`, …).
    pub fn from_class(class: &str) -> Self {
        match class {
            "rate_limited" => Self::StorageThrottled,
            "permission_denied" => Self::AuthDenied,
            "not_found" => Self::StorageNotFound,
            "config_invalid" => Self::ConfigInvalid,
            "unsupported" => Self::StorageUnsupported,
            "condition_not_match" => Self::StorageConditionNotMatch,
            "already_exists" => Self::StorageAlreadyExists,
            "conflict" => Self::StorageConflict,
            "range_not_satisfied" => Self::StorageRangeNotSatisfied,
            "timeout" | "tail_cut" => Self::NetTimeout,
            "connect" | "network" => Self::NetUnreachable,
            _ => Self::Internal,
        }
    }

    /// The code of a backend request answered with HTTP `status` (0: no response); `None` for
    /// a success.
    pub fn from_status(status: u16) -> Option<Self> {
        match status {
            0 => Some(Self::NetUnreachable),
            429 | 503 => Some(Self::StorageThrottled),
            401 | 403 => Some(Self::AuthDenied),
            404 => Some(Self::StorageNotFound),
            409 => Some(Self::StorageConflict),
            412 => Some(Self::StorageConditionNotMatch),
            416 => Some(Self::StorageRangeNotSatisfied),
            400..=599 => Some(Self::Internal),
            _ => None,
        }
    }

    /// [`FailureCode::from_status`], refined by the provider's error code when it names the
    /// failure more precisely (Azure `BlobAlreadyExists` on a 409, an in-process backend's
    /// OpenDAL kind).
    pub fn of_request(status: u16, provider_code: Option<&str>) -> Option<Self> {
        let code = Self::from_status(status)?;
        Some(match provider_code {
            Some(c) if c.ends_with("AlreadyExists") => Self::StorageAlreadyExists,
            Some("ConditionNotMatch" | "ConditionNotMet" | "PreconditionFailed") => {
                Self::StorageConditionNotMatch
            }
            Some("InvalidRange" | "RangeNotSatisfied") => Self::StorageRangeNotSatisfied,
            _ => code,
        })
    }

    /// Whether OpenDAL retries a request that failed with HTTP `status` (0: no response): no
    /// response, 408, 429, 499 and 5xx except 501 are temporary; any other failure persists.
    pub fn temporary_status(status: u16) -> bool {
        matches!(status, 0 | 408 | 429 | 499 | 500 | 502 | 503 | 504)
    }
}

impl std::fmt::Display for FailureCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Wire protocol of one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub enum WireProtocol {
    #[default]
    #[serde(rename = "h1")]
    H1,
    #[serde(rename = "h2")]
    H2,
    /// In-process backends (memory, fs): no network.
    #[serde(rename = "inproc")]
    Inproc,
}

impl From<crate::Protocol> for WireProtocol {
    fn from(p: crate::Protocol) -> Self {
        match p {
            crate::Protocol::Http1 => Self::H1,
            crate::Protocol::Http2 => Self::H2,
            crate::Protocol::InProcess => Self::Inproc,
        }
    }
}

/// TLS details of the connection a request used (when the transport reports them).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct TlsRow {
    pub resumed: bool,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    /// The handshake needed a HelloRetryRequest.
    pub hrr: bool,
    /// Negotiated key-exchange group, e.g. `X25519MLKEM768`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Kernel TCP statistics of the connection at request end (Linux `TCP_INFO`, macOS
/// `TCP_CONNECTION_INFO`). Counters are totals over the connection's life so far.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct TcpRow {
    /// Smoothed round-trip time.
    pub rtt_us: u64,
    pub rttvar_us: u64,
    /// Retransmitted segments (whole connection).
    pub retrans: u64,
    /// Congestion window, segments.
    pub cwnd: u64,
    /// Most recent delivery rate estimate, bits per second (Linux).
    pub delivery_rate_bps: u64,
    /// Time busy sending data (Linux ≥ 4.10).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub busy_us: u64,
    /// Time sending was limited by the receiver's window (Linux ≥ 4.10).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rwnd_limited_us: u64,
    /// Time sending was limited by the send buffer (Linux ≥ 4.10).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sndbuf_limited_us: u64,
    /// Minimum RTT the kernel observed: the network's RTT, which server delay does not
    /// inflate (unlike the smoothed `rtt_us`; Linux).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub min_rtt_us: u64,
    /// Bytes written but not yet sent (Linux).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub notsent_bytes: u64,
    /// Segments sent and not yet acknowledged (Linux).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unacked_count: u64,
    /// Segments considered lost (Linux).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lost_count: u64,
    /// Bytes acknowledged by the peer over the connection's life (Linux).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes_acked: u64,
    /// Pacing rate, bits per second (Linux).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pacing_rate_bps: u64,
    /// Bytes received over the connection's life (Linux `tcpi_bytes_received`, macOS
    /// `rxbytes`): with `bytes_acked`, the bytes on the wire.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes_received: u64,
}

/// Details of one request beyond the stage timings (design v5 §3): what the server said
/// (M10), kernel upload timing (M4), HTTP/2 stream waits (M5) and dial waits. Sparse: only
/// requests that have any of them carry it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct ReqDetail {
    /// The provider's error code from the error body or header (S3 `SlowDown`,
    /// `InternalError`; GCS `rateLimitExceeded`; Azure `ServerBusy`), read from the first
    /// 4 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// S3 `x-amz-id-2` (extended request id), alongside the request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// The bucket's region (`x-amz-bucket-region`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The `Server-Timing` header (capped at 256 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_timing: Option<String>,
    /// CDN cache status (`x-cache`, `cf-cache-status`, `x-cache-status`): `hit`, `miss`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<String>,
    /// From the request start until the kernel saw the server acknowledge the last request
    /// byte (Linux `TCP_INFO`): the end of the `upload` phase. `send_us` then counts until the
    /// acknowledgement and `ttfb_us` from it (the `server` phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_acked_us: Option<u64>,
    /// Time until the request body reached the kernel's send buffer (what `send_us` measured
    /// before the acknowledgement was known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_buffered_us: Option<u64>,
    /// HTTP/2: time waiting for a stream slot (`max_concurrent_streams`) on a connection
    /// (the `stream_wait` stage; part of `queue_us`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_wait_us: Option<u64>,
    /// HTTP/2: time the request body waited for flow-control window (window at 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_stall_us: Option<u64>,
    /// HTTP/2: streams open on the connection when the request started (itself included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streams_count: Option<u32>,
    /// The request waited for a connection another request was dialing: it counts as cold and
    /// the wait is in `connect` (fidelity transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dial_wait_us: Option<u64>,
    /// Retransmitted segments on the connection during this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrans_count: Option<u64>,
    /// The length of the byte range the request asked for (`Range: bytes=a-b`); absent for a
    /// whole object or an open-ended range. Against `bytes_down` it shows over-read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_bytes: Option<u64>,
    /// A failed request: whether the failure is temporary (OpenDAL retries it: no response,
    /// 408, 429, 5xx) or persistent (any other 4xx; a precondition or conflict). Absent for a
    /// success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary: Option<bool>,
}

impl ReqDetail {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// One backend request (an HTTP exchange, or an in-process backend call).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct ReqRow {
    /// Start, µs from the measurement window start (negative: before the window).
    pub t_us: i64,
    /// The logical operation that issued it (0: issued outside any operation).
    pub op_id: u64,
    /// 1 for the first attempt of this API call within the operation, 2+ for retries.
    pub attempt: u32,
    /// Storage API, e.g. `GetObject` (OpenDAL `ServiceOperation`; URL heuristic fallback).
    pub api: Cow<'static, str>,
    pub method: Cow<'static, str>,
    /// HTTP status; 0 when no response arrived.
    pub status: u16,
    pub protocol: WireProtocol,
    /// The request reused a pooled connection.
    pub reused: bool,
    pub conn_id: u64,
    /// Remote `ip:port` (the proxy's when tunnelling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub remote: Option<SocketAddr>,
    pub queue_us: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dns_us: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tcp_us: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub proxy_us: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tls_us: u64,
    pub send_us: u64,
    pub ttfb_us: u64,
    pub body_us: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stall_us: u64,
    pub total_us: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<FailureCode>,
    /// Error detail (transport or backend message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<Box<TlsRow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<Box<TcpRow>>,
    /// Server signals, kernel upload timing, HTTP/2 stream waits ([`ReqDetail`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Box<ReqDetail>>,
}

impl ReqRow {
    pub fn connect_us(&self) -> u64 {
        self.dns_us + self.tcp_us + self.proxy_us + self.tls_us
    }
    pub fn is_throttle(&self) -> bool {
        matches!(self.status, 429 | 503)
    }
    pub fn is_error(&self) -> bool {
        self.status == 0 || self.status >= 400
    }
}

/// Where the wall time of one operation went (µs; the parts sum to `latency_us`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct OpBreakdownUs {
    pub queue_us: u64,
    pub cache_us: u64,
    pub backend_us: u64,
    pub retry_us: u64,
    pub client_us: u64,
}

/// One measured logical operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct OpRow {
    pub op_id: u64,
    /// Workload verb, e.g. `read.range`, `write.multipart`.
    pub verb: Cow<'static, str>,
    /// Object key (or listed prefix).
    pub key: String,
    /// Start (the intended start in open-loop runs), µs from the window start.
    pub t_us: i64,
    pub latency_us: u64,
    /// Logical bytes read + written.
    pub bytes: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<FailureCode>,
    /// Error class / detail of a failed operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// At least one request of the operation was throttled (429/503).
    #[serde(default, skip_serializing_if = "is_false")]
    pub throttled: bool,
    /// 1 + retries.
    pub attempts: u32,
    pub breakdown: OpBreakdownUs,
    /// Backend requests issued (exact, even when request rows were capped).
    pub requests: u32,
    /// Await-tree call stack captured while the operation was slow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_tree: Option<String>,
}

/// What the run file's trace rows cover.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(not(feature = "__internal"), non_exhaustive)]
pub struct TraceInfo {
    pub max_requests_count: u64,
    pub ops_seen_count: u64,
    pub ops_kept_count: u64,
    pub requests_seen_count: u64,
    pub requests_kept_count: u64,
    /// Kept / seen request rows (1 when nothing was sampled away).
    pub sample_ratio: f64,
}
