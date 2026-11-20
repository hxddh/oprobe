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
/// `http://localhost:4318`; `/v1/traces` or `/v1/metrics` is appended unless present).
pub async fn push(endpoint: &str, signal: &str, body: &Value) -> anyhow::Result<()> {
    let url = signal_url(endpoint, signal);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(body)?)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("OTLP push to {url} failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "OTLP collector at {url} answered {status}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
    Ok(())
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
