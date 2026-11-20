//! Per-request and per-operation trace rows (`ReqRow`, `OpRow`; design §5).
//!
//! All times are microseconds relative to the start of the measurement window. Rows are what
//! a run file stores after its report (`{"kind":"op",…}` / `{"kind":"req",…}`) and what
//! `analyze`, exporters and the app's Requests table read.

use std::borrow::Cow;
use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::ErrorCode;

fn is_zero(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Wire protocol of one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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

impl From<crate::trace::Protocol> for WireProtocol {
    fn from(p: crate::trace::Protocol) -> Self {
        match p {
            crate::trace::Protocol::Http1 => Self::H1,
            crate::trace::Protocol::Http2 => Self::H2,
            crate::trace::Protocol::InProcess => Self::Inproc,
        }
    }
}

/// TLS details of the connection a request used (when the transport reports them).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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
}

/// One backend request (an HTTP exchange, or an in-process backend call).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    #[schemars(with = "Option<String>")]
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
    pub error_code: Option<ErrorCode>,
    /// Error detail (transport or backend message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<Box<TlsRow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<Box<TcpRow>>,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpBreakdownUs {
    pub queue_us: u64,
    pub cache_us: u64,
    pub backend_us: u64,
    pub retry_us: u64,
    pub client_us: u64,
}

/// One measured logical operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    pub error_code: Option<ErrorCode>,
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
