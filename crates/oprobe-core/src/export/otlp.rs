//! OTLP over HTTP with JSON encoding (the protobuf JSON mapping of
//! `opentelemetry.proto.collector.{trace,metrics}.v1.Export*ServiceRequest`), written directly
//! with serde — no SDK.
//!
//! Traces: one trace per operation; the operation is the root span (`INTERNAL`), each backend
//! request a child span (`CLIENT`) with OTel HTTP semantic-convention attributes, and the
//! request stages are span events (`oprobe.stage.*`) plus `oprobe.*_us` attributes.
//! Metrics: `http.client.request.duration` (histogram, seconds, by method/status/server/api),
//! `oprobe.request.stage.duration` (by api and stage), `oprobe.operation.duration` (by verb)
//! and request/throttle counters.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{abs_ns, bucket, object_url, ops_by_id, window_start_unix_ns};
use crate::model::{OpRow, ReqRow, Run, WireProtocol};

/// OTel's default explicit bucket bounds for `http.client.request.duration` (seconds).
pub const DURATION_BOUNDS_S: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

fn s(v: impl Into<String>) -> Value {
    json!({ "stringValue": v.into() })
}
fn i(v: i64) -> Value {
    json!({ "intValue": v.to_string() })
}
fn b(v: bool) -> Value {
    json!({ "boolValue": v })
}
fn kv(key: &str, value: Value) -> Value {
    json!({ "key": key, "value": value })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fnv(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for byte in *p {
            h ^= *byte as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h.max(1)
}

/// Deterministic ids: a re-export of the same run yields the same trace.
fn trace_id(run_id: &str, op_id: u64) -> String {
    let a = fnv(&[run_id.as_bytes(), &op_id.to_le_bytes(), b"trace-a"]);
    let b = fnv(&[run_id.as_bytes(), &op_id.to_le_bytes(), b"trace-b"]);
    hex(&[a.to_be_bytes(), b.to_be_bytes()].concat())
}
fn span_id(run_id: &str, op_id: u64, idx: u64) -> String {
    hex(&fnv(&[run_id.as_bytes(), &op_id.to_le_bytes(), &idx.to_le_bytes()]).to_be_bytes())
}

fn resource(run: &Run) -> Value {
    let r = &run.report;
    let mut attrs = vec![
        kv("service.name", s("oprobe")),
        kv("service.version", s(r.run.oprobe_version.clone())),
        kv("oprobe.run.id", s(r.run.id.clone())),
        kv(
            "oprobe.target",
            s(crate::util::display_url(&r.target.display)),
        ),
        kv("oprobe.workload.preset", s(r.workload.preset.clone())),
        kv("host.arch", s(r.run.host.arch.clone())),
        kv("os.type", s(r.run.host.os.clone())),
    ];
    if let Some(l) = &r.run.label {
        attrs.push(kv("oprobe.run.label", s(l.clone())));
    }
    if let Some(p) = &r.run.env.provider {
        attrs.push(kv("cloud.provider", s(p.clone())));
    }
    if let Some(reg) = &r.run.env.region {
        attrs.push(kv("cloud.region", s(reg.clone())));
    }
    json!({ "attributes": attrs })
}

fn scope() -> Value {
    json!({ "name": "oprobe", "version": env!("CARGO_PKG_VERSION") })
}

fn protocol_version(p: WireProtocol) -> Option<&'static str> {
    match p {
        WireProtocol::H1 => Some("1.1"),
        WireProtocol::H2 => Some("2"),
        WireProtocol::Inproc => None,
    }
}

fn op_span(run: &Run, o: &OpRow, start_ns: u128) -> Value {
    let r = &run.report;
    let t0 = abs_ns(start_ns, o.t_us);
    let mut attrs = vec![
        kv("oprobe.op.id", i(o.op_id as i64)),
        kv("oprobe.op.verb", s(o.verb.to_string())),
        kv("oprobe.op.key", s(o.key.clone())),
        kv("oprobe.op.bytes", i(o.bytes as i64)),
        kv("oprobe.op.attempts", i(o.attempts as i64)),
        kv("oprobe.op.requests", i(o.requests as i64)),
        kv("oprobe.queue_us", i(o.breakdown.queue_us as i64)),
        kv("oprobe.cache_us", i(o.breakdown.cache_us as i64)),
        kv("oprobe.backend_us", i(o.breakdown.backend_us as i64)),
        kv("oprobe.retry_us", i(o.breakdown.retry_us as i64)),
        kv("oprobe.client_us", i(o.breakdown.client_us as i64)),
    ];
    if o.throttled {
        attrs.push(kv("oprobe.op.throttled", b(true)));
    }
    if let Some(bk) = bucket(r).filter(|_| r.target.scheme == "s3") {
        attrs.push(kv("aws.s3.bucket", s(bk)));
        attrs.push(kv("aws.s3.key", s(o.key.clone())));
    }
    if let Some(e) = &o.error_code {
        attrs.push(kv("error.type", s(e.as_str())));
    }
    let mut span = json!({
        "traceId": trace_id(&r.run.id, o.op_id),
        "spanId": span_id(&r.run.id, o.op_id, 0),
        "name": o.verb.to_string(),
        "kind": 1,
        "startTimeUnixNano": t0.to_string(),
        "endTimeUnixNano": (t0 + o.latency_us as u128 * 1000).to_string(),
        "attributes": attrs,
        "status": { "code": if o.ok { 1 } else { 2 } },
    });
    if let Some(msg) = o.error.as_deref().filter(|_| !o.ok) {
        span["status"]["message"] = json!(msg);
    }
    span
}

fn req_span(run: &Run, q: &ReqRow, idx: u64, op: Option<&OpRow>, start_ns: u128) -> Value {
    let r = &run.report;
    let t0 = abs_ns(start_ns, q.t_us);
    let mut attrs = vec![
        kv("http.request.method", s(q.method.to_string())),
        kv("oprobe.api", s(q.api.to_string())),
        kv("oprobe.attempt", i(q.attempt as i64)),
        kv("oprobe.connection.reused", b(q.reused)),
        kv("oprobe.connection.id", i(q.conn_id as i64)),
        kv("http.request.body.size", i(q.bytes_up as i64)),
        kv("http.response.body.size", i(q.bytes_down as i64)),
    ];
    if q.status > 0 {
        attrs.push(kv("http.response.status_code", i(q.status as i64)));
    }
    if let Some(v) = protocol_version(q.protocol) {
        attrs.push(kv("network.protocol.name", s("http")));
        attrs.push(kv("network.protocol.version", s(v)));
    }
    if let Some(a) = q.remote {
        attrs.push(kv("network.peer.address", s(a.ip().to_string())));
        attrs.push(kv("network.peer.port", i(a.port() as i64)));
    }
    if let Some(ep) = r
        .target
        .endpoint
        .as_deref()
        .and_then(|e| url::Url::parse(e).ok())
    {
        if let Some(h) = ep.host_str() {
            attrs.push(kv("server.address", s(h)));
        }
        if let Some(p) = ep.port_or_known_default() {
            attrs.push(kv("server.port", i(p as i64)));
        }
    }
    if let Some(o) = op {
        attrs.push(kv("url.full", s(object_url(r, &o.key))));
        if let Some(bk) = bucket(r).filter(|_| r.target.scheme == "s3") {
            attrs.push(kv("aws.s3.bucket", s(bk)));
            attrs.push(kv("aws.s3.key", s(o.key.clone())));
        }
    }
    if let Some(id) = &q.request_id {
        attrs.push(kv("aws.request_id", s(id.clone())));
    }
    if let Some(ra) = q.retry_after_ms {
        attrs.push(kv("oprobe.retry_after_ms", i(ra as i64)));
    }
    if let Some(e) = &q.error_code {
        attrs.push(kv("error.type", s(e.as_str())));
    } else if q.is_error() && q.status > 0 {
        attrs.push(kv("error.type", s(q.status.to_string())));
    }
    if let Some(t) = &q.tls {
        attrs.push(kv("tls.protocol.version", s(t.version.clone())));
        attrs.push(kv("tls.resumed", b(t.resumed)));
        if let Some(c) = &t.cipher {
            attrs.push(kv("tls.cipher", s(c.clone())));
        }
    }
    if let Some(t) = &q.tcp {
        attrs.push(kv("oprobe.tcp.rtt_us", i(t.rtt_us as i64)));
        attrs.push(kv("oprobe.tcp.retrans", i(t.retrans as i64)));
    }
    // Stages, in wire order: an event at the start of each, its duration as an attribute.
    let mut events = Vec::new();
    let mut at = t0;
    for (name, us) in [
        ("queue", q.queue_us),
        ("dns", q.dns_us),
        ("tcp", q.tcp_us),
        ("proxy", q.proxy_us),
        ("tls", q.tls_us),
        ("send", q.send_us),
        ("ttfb", q.ttfb_us),
        ("body", q.body_us),
        ("stall", q.stall_us),
    ] {
        attrs.push(kv(&format!("oprobe.{name}_us"), i(us as i64)));
        if us > 0 {
            events.push(json!({
                "timeUnixNano": at.to_string(),
                "name": format!("oprobe.stage.{name}"),
                "attributes": [kv("oprobe.duration_us", i(us as i64))],
            }));
            at += us as u128 * 1000;
        }
    }
    json!({
        "traceId": trace_id(&r.run.id, q.op_id),
        "spanId": span_id(&r.run.id, q.op_id, idx),
        "parentSpanId": if q.op_id != 0 { span_id(&r.run.id, q.op_id, 0) } else { String::new() },
        "name": format!("{} {}", q.method, q.api),
        "kind": 3,
        "startTimeUnixNano": t0.to_string(),
        "endTimeUnixNano": (t0 + q.total_us as u128 * 1000).to_string(),
        "attributes": attrs,
        "events": events,
        "status": { "code": if q.is_error() && q.status != 404 { 2 } else { 0 } },
    })
}

/// `ExportTraceServiceRequest` of a run (every kept operation and request row).
pub fn traces(run: &Run) -> Value {
    let start = window_start_unix_ns(&run.report);
    let ops = ops_by_id(run);
    let mut spans: Vec<Value> = run.ops.iter().map(|o| op_span(run, o, start)).collect();
    let mut idx: BTreeMap<u64, u64> = BTreeMap::new();
    for q in &run.reqs {
        let n = idx.entry(q.op_id).or_insert(0);
        *n += 1;
        spans.push(req_span(run, q, *n, ops.get(&q.op_id).copied(), start));
    }
    json!({
        "resourceSpans": [{
            "resource": resource(run),
            "scopeSpans": [{ "scope": scope(), "spans": spans }],
        }]
    })
}

#[derive(Default)]
struct Hist {
    counts: Vec<u64>,
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Hist {
    fn observe(&mut self, v: f64, bounds: &[f64]) {
        if self.counts.is_empty() {
            self.counts = vec![0; bounds.len() + 1];
            self.min = v;
            self.max = v;
        }
        self.counts[bounds.partition_point(|b| *b < v)] += 1;
        self.count += 1;
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }
}

fn hist_points(
    m: BTreeMap<Vec<(String, String)>, Hist>,
    bounds: &[f64],
    t0: u128,
    t1: u128,
) -> Vec<Value> {
    m.into_iter()
        .map(|(labels, h)| {
            json!({
                "attributes": labels.iter().map(|(k, v)| {
                    if k == "http.response.status_code" {
                        kv(k, i(v.parse().unwrap_or(0)))
                    } else {
                        kv(k, s(v.clone()))
                    }
                }).collect::<Vec<_>>(),
                "startTimeUnixNano": t0.to_string(),
                "timeUnixNano": t1.to_string(),
                "count": h.count.to_string(),
                "sum": h.sum,
                "min": h.min,
                "max": h.max,
                "bucketCounts": h.counts.iter().map(u64::to_string).collect::<Vec<_>>(),
                "explicitBounds": bounds,
            })
        })
        .collect()
}

/// `ExportMetricsServiceRequest` of a run: histograms and counters over its rows.
pub fn metrics(run: &Run) -> Value {
    let r = &run.report;
    let t0 = window_start_unix_ns(r);
    let t1 = t0 + (r.run.window_s.max(0.0) * 1e9) as u128;
    let server = r
        .target
        .endpoint
        .as_deref()
        .and_then(|e| url::Url::parse(e).ok())
        .and_then(|u| u.host_str().map(str::to_owned));
    let mut req: BTreeMap<Vec<(String, String)>, Hist> = BTreeMap::new();
    let mut stage: BTreeMap<Vec<(String, String)>, Hist> = BTreeMap::new();
    let mut ops: BTreeMap<Vec<(String, String)>, Hist> = BTreeMap::new();
    let (mut throttled, mut total) = (
        BTreeMap::<String, u64>::new(),
        BTreeMap::<String, u64>::new(),
    );
    for q in run.reqs.iter().filter(|q| q.t_us >= 0) {
        let mut labels = vec![
            ("http.request.method".to_owned(), q.method.to_string()),
            ("oprobe.api".to_owned(), q.api.to_string()),
        ];
        if q.status > 0 {
            labels.push(("http.response.status_code".into(), q.status.to_string()));
        }
        if let Some(h) = &server {
            labels.push(("server.address".into(), h.clone()));
        }
        req.entry(labels)
            .or_default()
            .observe(q.total_us as f64 / 1e6, &DURATION_BOUNDS_S);
        for (name, us) in [
            ("queue", q.queue_us),
            ("connect", q.connect_us()),
            ("send", q.send_us),
            ("ttfb", q.ttfb_us),
            ("body", q.body_us),
            ("stall", q.stall_us),
        ] {
            if name == "connect" && (q.reused || us == 0) {
                continue;
            }
            stage
                .entry(vec![
                    ("oprobe.api".into(), q.api.to_string()),
                    ("oprobe.stage".into(), name.into()),
                ])
                .or_default()
                .observe(us as f64 / 1e6, &DURATION_BOUNDS_S);
        }
        *total.entry(q.api.to_string()).or_default() += 1;
        if q.is_throttle() {
            *throttled.entry(q.api.to_string()).or_default() += 1;
        }
    }
    for o in run.ops.iter().filter(|o| o.t_us >= 0) {
        ops.entry(vec![
            ("oprobe.op.verb".into(), o.verb.to_string()),
            ("oprobe.op.ok".into(), o.ok.to_string()),
        ])
        .or_default()
        .observe(o.latency_us as f64 / 1e6, &DURATION_BOUNDS_S);
    }
    let sum = |name: &str, desc: &str, m: BTreeMap<String, u64>| {
        json!({
            "name": name,
            "description": desc,
            "unit": "{request}",
            "sum": {
                "aggregationTemporality": 2,
                "isMonotonic": true,
                "dataPoints": m.into_iter().map(|(api, n)| json!({
                    "attributes": [kv("oprobe.api", s(api))],
                    "startTimeUnixNano": t0.to_string(),
                    "timeUnixNano": t1.to_string(),
                    "asInt": n.to_string(),
                })).collect::<Vec<_>>(),
            }
        })
    };
    let hist = |name: &str, desc: &str, m| {
        json!({
            "name": name,
            "description": desc,
            "unit": "s",
            "histogram": {
                "aggregationTemporality": 2,
                "dataPoints": hist_points(m, &DURATION_BOUNDS_S, t0, t1),
            }
        })
    };
    json!({
        "resourceMetrics": [{
            "resource": resource(run),
            "scopeMetrics": [{
                "scope": scope(),
                "metrics": [
                    hist("http.client.request.duration", "Duration of HTTP client requests.", req),
                    hist("oprobe.request.stage.duration", "Time requests spent in each stage.", stage),
                    hist("oprobe.operation.duration", "Duration of logical storage operations.", ops),
                    sum("oprobe.requests", "Backend requests sent.", total),
                    sum("oprobe.requests.throttled", "Requests answered with 429/503.", throttled),
                ],
            }]
        }]
    })
}

/// The OTLP/HTTP URL of `signal` at `endpoint` (a base URL, or one already ending in
/// `/v1/traces` or `/v1/metrics`, which is replaced).
pub fn signal_url(endpoint: &str, signal: &str) -> String {
    let mut base = endpoint.trim_end_matches('/');
    for s in ["/v1/traces", "/v1/metrics"] {
        if let Some(b) = base.strip_suffix(s) {
            base = b;
        }
    }
    format!("{base}/v1/{signal}")
}

/// POST an OTLP/JSON body to `endpoint` (a collector base URL such as
/// `http://localhost:4318`; `/v1/traces` or `/v1/metrics` is appended unless present), with
/// the headers of `OTEL_EXPORTER_OTLP_HEADERS`.
pub async fn push(endpoint: &str, signal: &str, body: &Value) -> anyhow::Result<()> {
    let headers = env_headers(signal);
    post(
        endpoint,
        signal,
        "application/json",
        serde_json::to_vec(body)?,
        &headers,
    )
    .await
}

async fn post(
    endpoint: &str,
    signal: &str,
    content_type: &str,
    body: Vec<u8>,
    headers: &[(String, String)],
) -> anyhow::Result<()> {
    let url = signal_url(endpoint, signal);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut req = client.post(&url).header("content-type", content_type);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.body(body).send().await.map_err(|e| {
        let code = if e.is_timeout() {
            crate::errors::ErrorCode::NetTimeout
        } else {
            crate::errors::ErrorCode::NetUnreachable
        };
        crate::errors::OprobeError::new(code, format!("OTLP push to {url} failed: {e}"))
            .with_hint("check that the collector runs and --endpoint is its base URL")
    })?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(crate::errors::OprobeError::new(
            crate::errors::ErrorCode::from_api_status(status.as_u16(), false),
            format!(
                "OTLP collector at {url} answered {status}: {}",
                text.chars().take(200).collect::<String>()
            ),
        )
        .with_hint(
            "check the collector's logs, OTEL_EXPORTER_OTLP_HEADERS (authentication) and \
             OTEL_EXPORTER_OTLP_PROTOCOL (http/protobuf or http/json)",
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// `oprobe watch --otlp-endpoint` (design v5 §6, review C7): one cumulative metric stream per
// watch session. The resource carries a stable `service.instance.id` and the target, never a
// run id (no series churn per window); counters and histograms are cumulative from the start
// of the session; headers come from `OTEL_EXPORTER_OTLP_HEADERS`; the body is protobuf unless
// `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`.
// ---------------------------------------------------------------------------------------

/// An attribute value of the format-independent metrics model.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

/// `key = value`.
pub type Attr = (String, AttrValue);

/// A histogram data point (explicit bounds; `bucket_counts.len() == bounds.len() + 1`).
#[derive(Clone, Debug, PartialEq)]
pub struct HistPoint {
    pub attrs: Vec<Attr>,
    pub start_ns: u64,
    pub time_ns: u64,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub bucket_counts: Vec<u64>,
    pub bounds: Vec<f64>,
}

/// A number data point.
#[derive(Clone, Debug, PartialEq)]
pub struct NumPoint {
    pub attrs: Vec<Attr>,
    pub start_ns: u64,
    pub time_ns: u64,
    pub value: f64,
    /// Encode as an integer (`asInt`) rather than a double.
    pub int: bool,
}

/// The data of a metric.
#[derive(Clone, Debug, PartialEq)]
pub enum MetricData {
    /// Cumulative histogram.
    Histogram(Vec<HistPoint>),
    /// Cumulative monotonic sum.
    Counter(Vec<NumPoint>),
    Gauge(Vec<NumPoint>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Metric {
    pub name: &'static str,
    pub description: &'static str,
    pub unit: &'static str,
    pub data: MetricData,
}

/// An `ExportMetricsServiceRequest` with one resource and one scope.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricsDoc {
    pub resource: Vec<Attr>,
    pub metrics: Vec<Metric>,
}

fn attr_json((k, v): &Attr) -> Value {
    let value = match v {
        AttrValue::Str(x) => s(x.clone()),
        AttrValue::Int(x) => i(*x),
        AttrValue::Bool(x) => b(*x),
    };
    kv(k, value)
}

impl MetricsDoc {
    /// The OTLP/JSON encoding.
    pub fn to_json(&self) -> Value {
        let metrics: Vec<Value> = self
            .metrics
            .iter()
            .map(|m| {
                let num = |p: &NumPoint| {
                    let mut v = json!({
                        "attributes": p.attrs.iter().map(attr_json).collect::<Vec<_>>(),
                        "startTimeUnixNano": p.start_ns.to_string(),
                        "timeUnixNano": p.time_ns.to_string(),
                    });
                    if p.int {
                        v["asInt"] = json!((p.value as i64).to_string());
                    } else {
                        v["asDouble"] = json!(p.value);
                    }
                    v
                };
                let mut v = json!({ "name": m.name, "description": m.description, "unit": m.unit });
                match &m.data {
                    MetricData::Histogram(points) => {
                        v["histogram"] = json!({
                            "aggregationTemporality": 2,
                            "dataPoints": points.iter().map(|p| json!({
                                "attributes": p.attrs.iter().map(attr_json).collect::<Vec<_>>(),
                                "startTimeUnixNano": p.start_ns.to_string(),
                                "timeUnixNano": p.time_ns.to_string(),
                                "count": p.count.to_string(),
                                "sum": p.sum,
                                "min": p.min,
                                "max": p.max,
                                "bucketCounts": p.bucket_counts.iter().map(u64::to_string).collect::<Vec<_>>(),
                                "explicitBounds": p.bounds,
                            })).collect::<Vec<_>>(),
                        })
                    }
                    MetricData::Counter(points) => {
                        v["sum"] = json!({
                            "aggregationTemporality": 2,
                            "isMonotonic": true,
                            "dataPoints": points.iter().map(num).collect::<Vec<_>>(),
                        })
                    }
                    MetricData::Gauge(points) => {
                        v["gauge"] = json!({ "dataPoints": points.iter().map(num).collect::<Vec<_>>() })
                    }
                }
                v
            })
            .collect();
        json!({
            "resourceMetrics": [{
                "resource": { "attributes": self.resource.iter().map(attr_json).collect::<Vec<_>>() },
                "scopeMetrics": [{ "scope": scope(), "metrics": metrics }],
            }]
        })
    }

    /// The OTLP protobuf encoding (`opentelemetry.proto.collector.metrics.v1`), hand-written:
    /// no code generator or protobuf runtime in the binary.
    pub fn to_protobuf(&self) -> Vec<u8> {
        use pb::*;
        let attr = |a: &Attr| {
            let mut val = Vec::new();
            match &a.1 {
                AttrValue::Str(x) => string(&mut val, 1, x),
                AttrValue::Bool(x) => varint_field(&mut val, 2, u64::from(*x)),
                AttrValue::Int(x) => varint_field(&mut val, 3, *x as u64),
            }
            let mut kvb = Vec::new();
            string(&mut kvb, 1, &a.0);
            message(&mut kvb, 2, &val);
            kvb
        };
        let num = |p: &NumPoint| {
            let mut d = Vec::new();
            for a in &p.attrs {
                message(&mut d, 7, &attr(a));
            }
            fixed64_field(&mut d, 2, p.start_ns);
            fixed64_field(&mut d, 3, p.time_ns);
            if p.int {
                fixed64_field(&mut d, 6, p.value as i64 as u64);
            } else {
                fixed64_field(&mut d, 4, p.value.to_bits());
            }
            d
        };
        let mut metrics = Vec::new();
        for m in &self.metrics {
            let mut mb = Vec::new();
            string(&mut mb, 1, m.name);
            string(&mut mb, 2, m.description);
            string(&mut mb, 3, m.unit);
            match &m.data {
                MetricData::Histogram(points) => {
                    let mut h = Vec::new();
                    for p in points {
                        let mut d = Vec::new();
                        for a in &p.attrs {
                            message(&mut d, 9, &attr(a));
                        }
                        fixed64_field(&mut d, 2, p.start_ns);
                        fixed64_field(&mut d, 3, p.time_ns);
                        fixed64_field(&mut d, 4, p.count);
                        fixed64_field(&mut d, 5, p.sum.to_bits());
                        packed_fixed64(&mut d, 6, p.bucket_counts.iter().copied());
                        packed_fixed64(&mut d, 7, p.bounds.iter().map(|b| b.to_bits()));
                        fixed64_field(&mut d, 11, p.min.to_bits());
                        fixed64_field(&mut d, 12, p.max.to_bits());
                        message(&mut h, 1, &d);
                    }
                    varint_field(&mut h, 2, 2); // AGGREGATION_TEMPORALITY_CUMULATIVE
                    message(&mut mb, 9, &h);
                }
                MetricData::Counter(points) => {
                    let mut sm = Vec::new();
                    for p in points {
                        message(&mut sm, 1, &num(p));
                    }
                    varint_field(&mut sm, 2, 2);
                    varint_field(&mut sm, 3, 1); // is_monotonic
                    message(&mut mb, 7, &sm);
                }
                MetricData::Gauge(points) => {
                    let mut g = Vec::new();
                    for p in points {
                        message(&mut g, 1, &num(p));
                    }
                    message(&mut mb, 5, &g);
                }
            }
            metrics.push(mb);
        }
        let mut sc = Vec::new();
        string(&mut sc, 1, "oprobe");
        string(&mut sc, 2, env!("CARGO_PKG_VERSION"));
        let mut scope_metrics = Vec::new();
        message(&mut scope_metrics, 1, &sc);
        for m in &metrics {
            message(&mut scope_metrics, 2, m);
        }
        let mut res = Vec::new();
        for a in &self.resource {
            message(&mut res, 1, &attr(a));
        }
        let mut rm = Vec::new();
        message(&mut rm, 1, &res);
        message(&mut rm, 2, &scope_metrics);
        let mut out = Vec::new();
        message(&mut out, 1, &rm);
        out
    }
}

/// The few protobuf wire-format primitives OTLP metrics need.
mod pb {
    pub fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    fn key(out: &mut Vec<u8>, field: u32, wire: u8) {
        varint(out, (u64::from(field) << 3) | u64::from(wire));
    }
    pub fn varint_field(out: &mut Vec<u8>, field: u32, v: u64) {
        key(out, field, 0);
        varint(out, v);
    }
    pub fn fixed64_field(out: &mut Vec<u8>, field: u32, v: u64) {
        key(out, field, 1);
        out.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
        key(out, field, 2);
        varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }
    pub fn string(out: &mut Vec<u8>, field: u32, s: &str) {
        bytes(out, field, s.as_bytes());
    }
    pub fn message(out: &mut Vec<u8>, field: u32, m: &[u8]) {
        bytes(out, field, m);
    }
    pub fn packed_fixed64(out: &mut Vec<u8>, field: u32, vals: impl Iterator<Item = u64>) {
        let mut b = Vec::new();
        for v in vals {
            b.extend_from_slice(&v.to_le_bytes());
        }
        bytes(out, field, &b);
    }
}

/// A stable `service.instance.id` for a watch of `target` on this host: the same host and
/// target give the same id across restarts (UUID-shaped, from a hash).
pub fn instance_id(host: &str, target: &str) -> String {
    let a = fnv(&[host.as_bytes(), target.as_bytes(), b"oprobe-watch-a"]);
    let b = fnv(&[host.as_bytes(), target.as_bytes(), b"oprobe-watch-b"]);
    let h = hex(&[a.to_be_bytes(), b.to_be_bytes()].concat());
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

type Key = Vec<(String, String)>;

/// Cumulative metrics of one `oprobe watch` session (one series per label set for the whole
/// session; see the section comment).
#[derive(Default)]
pub struct WatchMetrics {
    resource: Vec<Attr>,
    start_ns: u64,
    req: BTreeMap<Key, Hist>,
    stage: BTreeMap<Key, Hist>,
    ops: BTreeMap<Key, Hist>,
    requests: BTreeMap<String, u64>,
    throttled: BTreeMap<String, u64>,
    windows: u64,
    last_success_s: Option<f64>,
}

impl WatchMetrics {
    /// A session watching `target` (its display URL) from `host` (an identifier such as the
    /// report's host name hash; it only feeds the instance id), started at `start_ns` (Unix
    /// nanoseconds).
    pub fn new(target: &str, host: &str, start_ns: u64) -> Self {
        let resource = vec![
            ("service.name".to_owned(), AttrValue::Str("oprobe".into())),
            (
                "service.version".to_owned(),
                AttrValue::Str(env!("CARGO_PKG_VERSION").into()),
            ),
            (
                "service.instance.id".to_owned(),
                AttrValue::Str(instance_id(host, target)),
            ),
            (
                "oprobe.target".to_owned(),
                AttrValue::Str(target.to_owned()),
            ),
        ];
        Self {
            resource,
            start_ns,
            ..Self::default()
        }
    }

    /// Add one probe window (its kept request and operation rows), ended at `unix_s`.
    pub fn observe(&mut self, run: &Run, unix_s: f64) {
        self.windows += 1;
        if run.report.summary.failed_ops == 0 {
            self.last_success_s = Some(unix_s);
        }
        for q in run.reqs.iter().filter(|q| q.t_us >= 0) {
            let mut labels = vec![
                ("http.request.method".to_owned(), q.method.to_string()),
                ("oprobe.api".to_owned(), q.api.to_string()),
            ];
            if q.status > 0 {
                labels.push(("http.response.status_code".into(), q.status.to_string()));
            }
            self.req
                .entry(labels)
                .or_default()
                .observe(q.total_us as f64 / 1e6, &DURATION_BOUNDS_S);
            for (name, us) in [
                ("queue", q.queue_us),
                ("connect", q.connect_us()),
                ("send", q.send_us),
                ("ttfb", q.ttfb_us),
                ("body", q.body_us),
            ] {
                if name == "connect" && (q.reused || us == 0) {
                    continue;
                }
                self.stage
                    .entry(vec![
                        ("oprobe.api".into(), q.api.to_string()),
                        ("oprobe.stage".into(), name.into()),
                    ])
                    .or_default()
                    .observe(us as f64 / 1e6, &DURATION_BOUNDS_S);
            }
            *self.requests.entry(q.api.to_string()).or_default() += 1;
            if q.is_throttle() {
                *self.throttled.entry(q.api.to_string()).or_default() += 1;
            }
        }
        for o in run.ops.iter().filter(|o| o.t_us >= 0) {
            self.ops
                .entry(vec![
                    ("oprobe.op.verb".into(), o.verb.to_string()),
                    ("oprobe.op.ok".into(), o.ok.to_string()),
                ])
                .or_default()
                .observe(o.latency_us as f64 / 1e6, &DURATION_BOUNDS_S);
        }
    }

    /// The cumulative state at `now_ns`.
    pub fn doc(&self, now_ns: u64) -> MetricsDoc {
        let start = self.start_ns;
        let attrs = |k: &Key| -> Vec<Attr> {
            k.iter()
                .map(|(n, v)| {
                    let val = if n == "http.response.status_code" {
                        AttrValue::Int(v.parse().unwrap_or(0))
                    } else {
                        AttrValue::Str(v.clone())
                    };
                    (n.clone(), val)
                })
                .collect()
        };
        let hist = |m: &BTreeMap<Key, Hist>| {
            MetricData::Histogram(
                m.iter()
                    .map(|(k, h)| HistPoint {
                        attrs: attrs(k),
                        start_ns: start,
                        time_ns: now_ns,
                        count: h.count,
                        sum: h.sum,
                        min: h.min,
                        max: h.max,
                        bucket_counts: h.counts.clone(),
                        bounds: DURATION_BOUNDS_S.to_vec(),
                    })
                    .collect(),
            )
        };
        let counter = |m: &BTreeMap<String, u64>| {
            MetricData::Counter(
                m.iter()
                    .map(|(api, n)| NumPoint {
                        attrs: vec![("oprobe.api".into(), AttrValue::Str(api.clone()))],
                        start_ns: start,
                        time_ns: now_ns,
                        value: *n as f64,
                        int: true,
                    })
                    .collect(),
            )
        };
        let mut metrics = vec![
            Metric {
                name: "http.client.request.duration",
                description: "Duration of HTTP client requests.",
                unit: "s",
                data: hist(&self.req),
            },
            Metric {
                name: "oprobe.request.stage.duration",
                description: "Time requests spent in each stage.",
                unit: "s",
                data: hist(&self.stage),
            },
            Metric {
                name: "oprobe.operation.duration",
                description: "Duration of logical storage operations.",
                unit: "s",
                data: hist(&self.ops),
            },
            Metric {
                name: "oprobe.requests",
                description: "Backend requests sent.",
                unit: "{request}",
                data: counter(&self.requests),
            },
            Metric {
                name: "oprobe.requests.throttled",
                description: "Requests answered with 429/503.",
                unit: "{request}",
                data: counter(&self.throttled),
            },
            Metric {
                name: "oprobe.watch.windows",
                description: "Probe windows completed.",
                unit: "{window}",
                data: MetricData::Counter(vec![NumPoint {
                    attrs: vec![],
                    start_ns: start,
                    time_ns: now_ns,
                    value: self.windows as f64,
                    int: true,
                }]),
            },
        ];
        if let Some(t) = self.last_success_s {
            metrics.push(Metric {
                name: "oprobe.watch.last_success",
                description: "End of the last probe window without failed operations (Unix time).",
                unit: "s",
                data: MetricData::Gauge(vec![NumPoint {
                    attrs: vec![],
                    start_ns: start,
                    time_ns: now_ns,
                    value: t,
                    int: false,
                }]),
            });
        }
        MetricsDoc {
            resource: self.resource.clone(),
            metrics,
        }
    }
}

/// `OTEL_EXPORTER_OTLP_{signal}_HEADERS`, else `OTEL_EXPORTER_OTLP_HEADERS`: `k=v` pairs
/// separated by commas, values percent-decoded (the OTel specification's format).
pub fn env_headers(signal: &str) -> Vec<(String, String)> {
    let var = format!("OTEL_EXPORTER_OTLP_{}_HEADERS", signal.to_ascii_uppercase());
    let raw = std::env::var(&var)
        .ok()
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_HEADERS").ok())
        .unwrap_or_default();
    parse_headers(&raw)
}

/// Parse `k=v,k2=v2` (values percent-decoded; malformed pairs skipped).
pub fn parse_headers(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| {
                let v = url::form_urlencoded::parse(
                    format!("v={}", v.trim().replace('+', "%2B")).as_bytes(),
                )
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
                (k.to_owned(), v)
            })
        })
        .collect()
}

/// The OTLP/HTTP encoding to push `signal` with: `OTEL_EXPORTER_OTLP_{signal}_PROTOCOL`, else
/// `OTEL_EXPORTER_OTLP_PROTOCOL`; `http/protobuf` (the specification's default) unless it says
/// `http/json`. Only metrics have a protobuf encoder; traces are always JSON.
pub fn env_protobuf(signal: &str) -> bool {
    let var = format!(
        "OTEL_EXPORTER_OTLP_{}_PROTOCOL",
        signal.to_ascii_uppercase()
    );
    let p = std::env::var(&var)
        .ok()
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL").ok())
        .unwrap_or_default();
    signal == "metrics" && !p.trim().eq_ignore_ascii_case("http/json")
}

/// POST watch metrics to `endpoint` with the environment's headers and protocol.
pub async fn push_metrics(endpoint: &str, doc: &MetricsDoc) -> anyhow::Result<()> {
    let headers = env_headers("metrics");
    if env_protobuf("metrics") {
        post(
            endpoint,
            "metrics",
            "application/x-protobuf",
            doc.to_protobuf(),
            &headers,
        )
        .await
    } else {
        post(
            endpoint,
            "metrics",
            "application/json",
            serde_json::to_vec(&doc.to_json())?,
            &headers,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_signal_posts_to_its_own_path() {
        for ep in [
            "http://c:4318",
            "http://c:4318/",
            "http://c:4318/v1/traces",
            "http://c:4318/v1/metrics",
        ] {
            assert_eq!(signal_url(ep, "traces"), "http://c:4318/v1/traces");
            assert_eq!(signal_url(ep, "metrics"), "http://c:4318/v1/metrics");
        }
    }
    use crate::analysis::simulate::tests::straggler_run;

    #[test]
    fn watch_metrics_are_cumulative_with_a_stable_identity() {
        let run = straggler_run();
        let mut w = WatchMetrics::new("s3://b/p", "host-1", 1_000);
        w.observe(&run, 10.0);
        w.observe(&run, 20.0);
        let d = w.doc(5_000);
        // No run id anywhere; a stable instance id; the target.
        let res: Vec<&str> = d.resource.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!res.contains(&"oprobe.run.id"), "{res:?}");
        assert!(res.contains(&"service.instance.id") && res.contains(&"oprobe.target"));
        assert_eq!(
            instance_id("host-1", "s3://b/p"),
            instance_id("host-1", "s3://b/p")
        );
        assert_ne!(
            instance_id("host-1", "s3://b/p"),
            instance_id("host-2", "s3://b/p")
        );
        assert_eq!(instance_id("h", "t").len(), 36);
        let MetricData::Histogram(points) = &d.metrics[0].data else {
            panic!()
        };
        assert_eq!(points[0].count, 2000, "two windows accumulate");
        assert_eq!((points[0].start_ns, points[0].time_ns), (1_000, 5_000));
        let MetricData::Counter(c) = &d.metrics[3].data else {
            panic!()
        };
        assert!(c.iter().all(|p| p.int && p.start_ns == 1_000));
        let j = d.to_json();
        assert_eq!(
            j["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["histogram"]["aggregationTemporality"],
            2
        );
        assert_eq!(
            j["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][3]["sum"]["isMonotonic"],
            true
        );
    }

    /// A minimal protobuf reader: (field, wire type, payload) of one message level.
    fn fields(mut b: &[u8]) -> Vec<(u32, u8, Vec<u8>)> {
        fn varint(b: &mut &[u8]) -> u64 {
            let (mut v, mut shift) = (0u64, 0);
            loop {
                let x = b[0];
                *b = &b[1..];
                v |= u64::from(x & 0x7f) << shift;
                if x < 0x80 {
                    return v;
                }
                shift += 7;
            }
        }
        let mut out = Vec::new();
        while !b.is_empty() {
            let k = varint(&mut b);
            let (field, wire) = ((k >> 3) as u32, (k & 7) as u8);
            let payload = match wire {
                0 => varint(&mut b).to_le_bytes().to_vec(),
                1 => {
                    let p = b[..8].to_vec();
                    b = &b[8..];
                    p
                }
                2 => {
                    let n = varint(&mut b) as usize;
                    let p = b[..n].to_vec();
                    b = &b[n..];
                    p
                }
                w => panic!("wire type {w}"),
            };
            out.push((field, wire, payload));
        }
        out
    }

    #[test]
    fn protobuf_follows_the_otlp_schema() {
        let mut w = WatchMetrics::new("s3://b/p", "h", 7);
        w.observe(&straggler_run(), 1.0);
        let pb = w.doc(9).to_protobuf();
        let top = fields(&pb);
        assert_eq!(top.len(), 1);
        assert_eq!((top[0].0, top[0].1), (1, 2), "resource_metrics");
        let rm = fields(&top[0].2);
        let resource = fields(&rm.iter().find(|f| f.0 == 1).unwrap().2);
        let keys: Vec<String> = resource
            .iter()
            .map(|kv| String::from_utf8(fields(&kv.2)[0].2.clone()).unwrap())
            .collect();
        assert!(keys.contains(&"service.instance.id".to_owned()), "{keys:?}");
        let sm = fields(&rm.iter().find(|f| f.0 == 2).unwrap().2);
        let scope = fields(&sm[0].2);
        assert_eq!(scope[0].2, b"oprobe");
        let metric = fields(&sm[1].2);
        assert_eq!(metric[0].2, b"http.client.request.duration");
        let hist = fields(&metric.iter().find(|f| f.0 == 9).expect("histogram = 9").2);
        let temporality = hist.iter().find(|f| f.0 == 2).unwrap();
        assert_eq!(temporality.2[0], 2, "cumulative");
        let dp = fields(&hist[0].2);
        let get = |n: u32| dp.iter().find(|f| f.0 == n).unwrap().2.clone();
        assert_eq!(u64::from_le_bytes(get(2).try_into().unwrap()), 7, "start");
        assert_eq!(u64::from_le_bytes(get(3).try_into().unwrap()), 9, "time");
        assert_eq!(
            u64::from_le_bytes(get(4).try_into().unwrap()),
            1000,
            "count"
        );
        assert_eq!(
            get(6).len(),
            (DURATION_BOUNDS_S.len() + 1) * 8,
            "packed bucket counts"
        );
        assert_eq!(get(7).len(), DURATION_BOUNDS_S.len() * 8, "packed bounds");
    }

    #[test]
    fn otlp_headers_follow_the_specification_format() {
        assert_eq!(
            parse_headers("api-key=abc%3D%3D, x-tenant = t1 ,bad,=x"),
            vec![
                ("api-key".to_owned(), "abc==".to_owned()),
                ("x-tenant".to_owned(), "t1".to_owned())
            ]
        );
        assert_eq!(
            parse_headers("authorization=Basic a+b"),
            vec![("authorization".to_owned(), "Basic a+b".to_owned())]
        );
        assert!(parse_headers("").is_empty());
    }

    #[test]
    fn traces_have_op_roots_request_children_and_stage_events() {
        let mut run = straggler_run();
        run.report.run.id = "20261120-143005-a1b2".into();
        run.report.run.ended_at = "2026-11-20T14:30:10Z".into();
        run.report.run.window_s = 5.0;
        let v = traces(&run);
        let spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), run.ops.len() + run.reqs.len());
        let root = &spans[0];
        let child = &spans[run.ops.len()];
        assert_eq!(child["traceId"], root["traceId"]);
        assert_eq!(child["parentSpanId"], root["spanId"]);
        assert_eq!(root["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(child["spanId"].as_str().unwrap().len(), 16);
        assert_eq!(child["kind"], 3);
        let keys: Vec<&str> = child["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        for k in [
            "http.request.method",
            "http.response.status_code",
            "network.protocol.version",
            "url.full",
            "aws.s3.bucket",
            "oprobe.ttfb_us",
        ] {
            assert!(keys.contains(&k), "{k} missing: {keys:?}");
        }
        assert!(
            child["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "oprobe.stage.ttfb")
        );
        // Start 14:30:05 (window end minus 5 s).
        assert_eq!(root["startTimeUnixNano"], "1795185005000000000");
        let m = metrics(&run);
        let ms = m["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        assert_eq!(ms[0]["name"], "http.client.request.duration");
        let dp = &ms[0]["histogram"]["dataPoints"][0];
        assert_eq!(dp["count"], "1000");
        assert_eq!(
            dp["bucketCounts"].as_array().unwrap().len(),
            DURATION_BOUNDS_S.len() + 1
        );
    }
}
