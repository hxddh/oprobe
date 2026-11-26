//! OpenTelemetry metrics (feature `otel`), following the HTTP semantic conventions.
//!
//! [`OtelExporter`] records every HTTP request the operator issues into two histograms
//! (seconds):
//!
//! - **`http.client.request.duration`** ([`REQUEST_DURATION`]): the standard metric, recorded
//!   exactly once per request with the whole request's duration and only attributes of the
//!   semantic conventions: `http.request.method`, `server.address` and `server.port` (the
//!   host the request was addressed to), `http.response.status_code` and
//!   `network.protocol.version` when a response arrived, and `error.type` when the request
//!   failed (the status code, or `cancelled` / `_OTHER` when there was no response). A count
//!   or a sum over it is the number or the time of the requests, as any HTTP dashboard assumes.
//! - **`oprobe.http.client.request.stage.duration`** ([`REQUEST_STAGE_DURATION`]): where the
//!   request's time went, once per stage that took time. `oprobe.stage` is one of `queue`,
//!   `connect`, `send`, `ttfb`, `body`, `stall`; the stages of a request add up to its
//!   duration. Besides `oprobe.stage` it carries `oprobe.api` (`GetObject`, …),
//!   `http.request.method`, `http.response.status_code` and OpenDAL's own metric labels
//!   (`opendal-layer-otelmetrics`), so both layers' series join: `scheme`, `namespace` (the
//!   bucket or container) and `root` of the service of the operator that issued the request
//!   (a layer applied to several operators labels each one's requests with its own), and
//!   `operation` (`read`, `write`, `stat`, …) when the service attached it.
//!
//! The backend calls of in-process services (`memory`, `fs`) are not HTTP requests and are in
//! neither; operation durations are OpenDAL's own metrics (`opendal-layer-otelmetrics`).
//!
//! ```no_run
//! let meter = opentelemetry::global::meter("my-app");
//! let (layer, handle) = oprobe_trace::layer(oprobe_trace::otel::config(&meter));
//! # let _ = (layer, handle);
//! ```

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, Meter};

use crate::record::{Protocol, RequestRecord};
use crate::recorder::Exporter;

/// The standard request metric (HTTP semantic conventions): one observation per request.
pub const REQUEST_DURATION: &str = "http.client.request.duration";

/// oprobe's breakdown of a request by stage (`oprobe.stage`).
pub const REQUEST_STAGE_DURATION: &str = "oprobe.http.client.request.stage.duration";

/// The bucket boundaries the HTTP semantic conventions advise for request durations.
const DURATION_BOUNDARIES_S: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// A stage is often far shorter than a request: the same boundaries, extended down to 100 µs.
const STAGE_BOUNDARIES_S: [f64; 19] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75,
    1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Emits [`REQUEST_DURATION`] and [`REQUEST_STAGE_DURATION`]. Operation-level metrics are
/// OpenDAL's own (`opendal-layer-otelmetrics`): oprobe adds what only it sees, the requests
/// and their stages.
pub struct OtelExporter {
    duration: Histogram<f64>,
    stage_duration: Histogram<f64>,
}

impl std::fmt::Debug for OtelExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtelExporter")
    }
}

impl OtelExporter {
    pub fn new(meter: &Meter) -> Self {
        Self {
            duration: meter
                .f64_histogram(REQUEST_DURATION)
                .with_unit("s")
                .with_description("Duration of HTTP client requests.")
                .with_boundaries(DURATION_BOUNDARIES_S.to_vec())
                .build(),
            stage_duration: meter
                .f64_histogram(REQUEST_STAGE_DURATION)
                .with_unit("s")
                .with_description("Duration of the stages of HTTP client requests.")
                .with_boundaries(STAGE_BOUNDARIES_S.to_vec())
                .build(),
        }
    }
}

/// The stages of `r` in µs: they add up to the request's duration.
fn stages(r: &RequestRecord) -> [(&'static str, u64); 6] {
    let t = &r.timing;
    [
        ("queue", t.queue_us),
        ("connect", t.connect_us()),
        ("send", t.send_us),
        ("ttfb", t.ttfb_us),
        ("body", t.body_us),
        ("stall", t.stall_us),
    ]
}

/// `http.request.method`: one of the known methods, else `_OTHER` (semantic conventions).
fn method(m: &str) -> &'static str {
    match m {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "PUT" => "PUT",
        "POST" => "POST",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "OPTIONS" => "OPTIONS",
        "CONNECT" => "CONNECT",
        "TRACE" => "TRACE",
        _ => "_OTHER",
    }
}

/// `error.type` of a failed request: the status code of an error response, else a
/// low-cardinality reason. `None` for a success.
fn error_type(r: &RequestRecord) -> Option<String> {
    match r.status {
        0 if r.detail.as_deref().is_some_and(|d| d.aborted) => Some("cancelled".to_owned()),
        0 => Some("_OTHER".to_owned()),
        s if s >= 400 => Some(s.to_string()),
        // A response that broke off part-way.
        _ => r.error.as_ref().map(|_| "_OTHER".to_owned()),
    }
}

impl Exporter for OtelExporter {
    fn on_request(&self, r: &RequestRecord) {
        // A backend call of `memory` or `fs` is not an HTTP request.
        if r.protocol == Protocol::InProcess {
            return;
        }
        let method = method(&r.method);
        let status = (r.status > 0).then(|| i64::from(r.status));

        // The standard metric: once, with standard attributes only.
        let mut attrs = Vec::with_capacity(6);
        attrs.push(KeyValue::new("http.request.method", method));
        if let Some((address, port)) = &r.server {
            attrs.push(KeyValue::new("server.address", address.to_string()));
            attrs.push(KeyValue::new("server.port", i64::from(*port)));
        }
        if let Some(status) = status {
            attrs.push(KeyValue::new("http.response.status_code", status));
            let version = match r.protocol {
                Protocol::Http2 => "2",
                Protocol::Http1 | Protocol::InProcess => "1.1",
            };
            attrs.push(KeyValue::new("network.protocol.version", version));
        }
        if let Some(error) = error_type(r) {
            attrs.push(KeyValue::new("error.type", error));
        }
        self.duration.record(r.timing.total_us as f64 / 1e6, &attrs);

        // oprobe's own: the stages, with what tells them apart and OpenDAL's labels.
        let mut attrs = Vec::with_capacity(9);
        attrs.extend([
            KeyValue::new("oprobe.stage", ""),
            KeyValue::new("oprobe.api", r.service_op.unwrap_or_else(|| r.api.label())),
            KeyValue::new("http.request.method", method),
        ]);
        // The service of the operator that issued the request (9.2 labelled every request
        // of a layer with the first operator it was applied to).
        if let Some(s) = &r.service {
            attrs.extend([
                KeyValue::new("scheme", s.scheme.clone()),
                KeyValue::new("namespace", s.namespace.clone()),
                KeyValue::new("root", s.root.clone()),
            ]);
        }
        if let Some(op) = r.operation {
            attrs.push(KeyValue::new("operation", op));
        }
        if let Some(status) = status {
            attrs.push(KeyValue::new("http.response.status_code", status));
        }
        for (stage, us) in stages(r) {
            if us == 0 {
                continue;
            }
            attrs[0] = KeyValue::new("oprobe.stage", stage);
            self.stage_duration.record(us as f64 / 1e6, &attrs);
        }
    }
}

/// A [`crate::Config`] that exports to `meter`. Every operator its layers are applied to has
/// its own `scheme`, `namespace` and `root` attributes, whether each has a layer of its own or
/// they share a clone of one.
pub fn config(meter: &Meter) -> crate::Config {
    crate::Config::default().exporter(Arc::new(OtelExporter::new(meter)))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::net::testserver;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    /// What a histogram collected: its observations, and every attribute with its values.
    #[derive(Debug, Default)]
    struct Collected {
        count: u64,
        attributes: BTreeMap<String, BTreeSet<String>>,
        /// How many observations carry each attribute.
        with: BTreeMap<String, u64>,
    }

    struct Meters {
        exporter: InMemoryMetricExporter,
        provider: SdkMeterProvider,
    }

    impl Meters {
        fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .build();
            Self { exporter, provider }
        }

        fn meter(&self) -> Meter {
            self.provider.meter("oprobe-trace-test")
        }

        fn histogram(&self, name: &str) -> Collected {
            // The totals so far, once: not on top of what an earlier call collected.
            self.exporter.reset();
            self.provider.force_flush().unwrap();
            let mut out = Collected::default();
            for rm in &self.exporter.get_finished_metrics().unwrap() {
                for m in rm.scope_metrics().flat_map(|sm| sm.metrics()) {
                    if m.name() != name {
                        continue;
                    }
                    assert_eq!(m.unit(), "s");
                    let AggregatedMetrics::F64(MetricData::Histogram(h)) = m.data() else {
                        panic!("{name} is not a histogram");
                    };
                    for p in h.data_points() {
                        out.count += p.count();
                        for kv in p.attributes() {
                            out.attributes
                                .entry(kv.key.to_string())
                                .or_default()
                                .insert(kv.value.to_string());
                            *out.with.entry(kv.key.to_string()).or_default() += p.count();
                        }
                    }
                }
            }
            out
        }
    }

    fn values<'a>(c: &'a Collected, key: &str) -> Vec<&'a str> {
        c.attributes
            .get(key)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// N requests are N observations of the standard metric, with standard attributes only;
    /// the stages are in oprobe's own histogram (9.1 recorded every request up to seven
    /// times into `http.client.request.duration`, once per stage).
    #[tokio::test]
    async fn every_http_request_is_one_observation_of_the_standard_metric() {
        let server = testserver::serve(|req| async move {
            let status = if req.uri().path().ends_with("/missing") {
                404
            } else {
                200
            };
            testserver::respond(status, "hello")
        })
        .await;
        let meters = Meters::new();
        let cfg = config(&meters.meter()).https(false).proxy(None);
        let (layer, handle) = crate::layer(cfg);
        let op = opendal_core::Operator::new(
            opendal_service_http::Http::default().endpoint(&format!("http://{}", server.addr)),
        )
        .unwrap()
        .layer(layer);
        for _ in 0..5 {
            assert_eq!(op.read("k").await.unwrap().to_vec(), b"hello");
        }
        op.stat("k").await.unwrap();
        assert!(op.read("missing").await.is_err());

        let requests: u64 = handle.snapshot().apis.iter().map(|a| a.count).sum();
        assert!(requests >= 7, "{requests} requests");
        let standard = meters.histogram(REQUEST_DURATION);
        assert_eq!(standard.count, requests, "{standard:?}");
        let allowed = [
            "http.request.method",
            "http.response.status_code",
            "server.address",
            "server.port",
            "network.protocol.version",
            "error.type",
        ];
        for key in standard.attributes.keys() {
            assert!(allowed.contains(&key.as_str()), "{key}: {standard:?}");
        }
        assert_eq!(values(&standard, "http.request.method"), ["GET", "HEAD"]);
        assert_eq!(
            values(&standard, "http.response.status_code"),
            ["200", "404"]
        );
        assert_eq!(values(&standard, "server.address"), ["127.0.0.1"]);
        let port = server.addr.port().to_string();
        assert_eq!(values(&standard, "server.port"), [port.as_str()]);
        assert_eq!(values(&standard, "network.protocol.version"), ["1.1"]);
        assert_eq!(values(&standard, "error.type"), ["404"]);

        // The stages: every request spent time waiting for its response, and none is `total`.
        let stages = meters.histogram(REQUEST_STAGE_DURATION);
        assert!(stages.count >= requests, "{stages:?}");
        let known = ["queue", "connect", "send", "ttfb", "body", "stall"];
        let seen = values(&stages, "oprobe.stage");
        assert!(seen.contains(&"ttfb"), "{stages:?}");
        assert!(seen.iter().all(|s| known.contains(s)), "{stages:?}");
        // OpenDAL's own metric labels, so oprobe's series join opendal-layer-otelmetrics'.
        assert_eq!(values(&stages, "scheme"), ["http"]);
        for l in ["namespace", "root", "operation", "oprobe.api"] {
            assert!(stages.attributes.contains_key(l), "{l}: {stages:?}");
        }
    }

    /// A backend call of an in-process service is not an HTTP request: it is in neither
    /// histogram (9.1 put it into `http.client.request.duration`).
    #[tokio::test]
    async fn in_process_backend_calls_are_not_http_requests() {
        let meters = Meters::new();
        let (layer, handle) = crate::layer(config(&meters.meter()));
        let op = opendal_core::Operator::new(opendal_core::services::Memory::default())
            .unwrap()
            .layer(layer);
        op.write("a", vec![1u8; 64]).await.unwrap();
        let _ = op.read("a").await.unwrap();
        assert_eq!(handle.snapshot().summary.ops, 2);
        assert_eq!(meters.histogram(REQUEST_DURATION).count, 0);
        assert_eq!(meters.histogram(REQUEST_STAGE_DURATION).count, 0);
    }

    /// Each traced operator's stage series carries its own service labels.
    #[tokio::test]
    async fn cloned_otel_config_keeps_each_operators_service_labels() {
        let server = testserver::serve(|_| async { testserver::respond(200, "x") }).await;
        let meters = Meters::new();
        let cfg = config(&meters.meter()).https(false).proxy(None);
        for root in ["/alpha", "/beta"] {
            let (layer, _handle) = crate::layer(cfg.clone());
            let endpoint = format!("http://{}", server.addr);
            let http = opendal_service_http::Http::default()
                .endpoint(&endpoint)
                .root(root);
            let op = opendal_core::Operator::new(http).unwrap().layer(layer);
            op.read("k").await.unwrap();
        }
        let stages = meters.histogram(REQUEST_STAGE_DURATION);
        assert_eq!(values(&stages, "root"), ["/alpha/", "/beta/"]);
    }

    /// One layer, cloned onto two operators (a `TraceLayer` is `Clone`, and one profile of
    /// several operators is what sharing it is for): each operator's requests carry its own
    /// service labels (9.2 labelled all of them with the operator the layer met first).
    #[tokio::test]
    async fn a_layer_cloned_onto_two_operators_labels_each_ones_requests() {
        let server = testserver::serve(|_| async { testserver::respond(200, "x") }).await;
        let meters = Meters::new();
        let cfg = config(&meters.meter()).https(false).proxy(None);
        let (layer, handle) = crate::layer(cfg);
        for root in ["/alpha", "/beta"] {
            let endpoint = format!("http://{}", server.addr);
            let http = opendal_service_http::Http::default()
                .endpoint(&endpoint)
                .root(root);
            let op = opendal_core::Operator::new(http)
                .unwrap()
                .layer(layer.clone());
            op.read("k").await.unwrap();
            // A layer added afterwards replays the trace layer: the labels stay the operator's.
            let op = op.layer(handle.queue_layer());
            op.read("k").await.unwrap();
        }
        let stages = meters.histogram(REQUEST_STAGE_DURATION);
        assert_eq!(values(&stages, "root"), ["/alpha/", "/beta/"]);
        assert_eq!(values(&stages, "scheme"), ["http"]);
        // Every observation has a root: none is left unlabelled.
        let requests: u64 = handle.snapshot().apis.iter().map(|a| a.count).sum();
        assert_eq!(requests, 4);
        assert!(stages.count >= requests, "{stages:?}");
        assert_eq!(stages.with["root"], stages.count, "{stages:?}");
    }
}
