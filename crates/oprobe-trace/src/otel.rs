//! OpenTelemetry metrics (feature `otel`), following the HTTP semantic conventions.
//!
//! [`OtelExporter`] records every request into the `http.client.request.duration` histogram
//! (seconds), once per stage: `oprobe.stage` is `total` (the whole request, what the semantic
//! convention means) or one of `queue`, `connect`, `send`, `ttfb`, `body`, `stall` (stages
//! that took no time are skipped). Attributes: `oprobe.stage`, `oprobe.api` (`GetObject`, …),
//! `http.request.method` and `http.response.status_code` (absent when no response arrived),
//! and OpenDAL's own metric labels (`opendal-layer-otelmetrics`), so both layers' series join:
//! `scheme`, `namespace` (the bucket or container) and `root` of the service, and `operation`
//! (`read`, `write`, `stat`, …) when the service attached it.
//!
//! ```no_run
//! let meter = opentelemetry::global::meter("my-app");
//! let (layer, handle) = oprobe_trace::layer(oprobe_trace::otel::config(&meter));
//! # let _ = (layer, handle);
//! ```

use std::sync::{Arc, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, Meter};

use crate::record::RequestRecord;
use crate::recorder::Exporter;

/// The request metric name (HTTP semantic conventions).
pub const REQUEST_DURATION: &str = "http.client.request.duration";

/// Emits [`REQUEST_DURATION`]. Operation-level metrics are OpenDAL's own
/// (`opendal-layer-otelmetrics`): oprobe adds what only it sees, the request's stages.
pub struct OtelExporter {
    duration: Histogram<f64>,
    /// `scheme`, `namespace`, `root` of the service (once the layer is applied).
    service: OnceLock<[KeyValue; 3]>,
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
                .with_description("Duration of HTTP client requests, by oprobe stage.")
                .build(),
            service: OnceLock::new(),
        }
    }
}

/// The stages of `r` in µs: the whole request first.
fn stages(r: &RequestRecord) -> [(&'static str, u64); 7] {
    let t = &r.timing;
    [
        ("total", t.total_us),
        ("queue", t.queue_us),
        ("connect", t.connect_us()),
        ("send", t.send_us),
        ("ttfb", t.ttfb_us),
        ("body", t.body_us),
        ("stall", t.stall_us),
    ]
}

impl Exporter for OtelExporter {
    fn on_request(&self, r: &RequestRecord) {
        let api = r.service_op.unwrap_or_else(|| r.api.label());
        for (stage, us) in stages(r) {
            if us == 0 && stage != "total" {
                continue;
            }
            let mut attrs = Vec::with_capacity(9);
            attrs.extend([
                KeyValue::new("oprobe.stage", stage),
                KeyValue::new("oprobe.api", api),
                KeyValue::new("http.request.method", r.method.clone()),
            ]);
            if let Some(s) = self.service.get() {
                attrs.extend(s.iter().cloned());
            }
            if let Some(op) = r.operation {
                attrs.push(KeyValue::new("operation", op));
            }
            if r.status > 0 {
                attrs.push(KeyValue::new(
                    "http.response.status_code",
                    i64::from(r.status),
                ));
            }
            self.duration.record(us as f64 / 1e6, &attrs);
        }
    }

    fn on_service(&self, scheme: &str, name: &str, root: &str) {
        let _ = self.service.set([
            KeyValue::new("scheme", scheme.to_owned()),
            KeyValue::new("namespace", name.to_owned()),
            KeyValue::new("root", root.to_owned()),
        ]);
    }
}

/// A [`crate::Config`] that exports to `meter` (cloning it gives each traced operator its own
/// `scheme`, `namespace` and `root` attributes).
pub fn config(meter: &Meter) -> crate::Config {
    let meter = meter.clone();
    crate::Config::default().exporter_factory(move || Arc::new(OtelExporter::new(&meter)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    #[tokio::test]
    async fn requests_land_in_the_duration_histogram() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let meter = provider.meter("oprobe-trace-test");
        let (layer, _handle) = crate::layer(config(&meter));
        let op = opendal_core::Operator::new(opendal_core::services::Memory::default())
            .unwrap()
            .layer(layer);
        op.write("a", vec![1u8; 64]).await.unwrap();
        let _ = op.read("a").await.unwrap();
        provider.force_flush().unwrap();
        let metrics = exporter.get_finished_metrics().unwrap();
        let mut points = 0;
        let mut stages = std::collections::BTreeSet::new();
        let mut labels = std::collections::BTreeMap::new();
        for rm in &metrics {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() != REQUEST_DURATION {
                        continue;
                    }
                    assert_eq!(m.unit(), "s");
                    if let AggregatedMetrics::F64(MetricData::Histogram(h)) = m.data() {
                        for p in h.data_points() {
                            points += p.count();
                            for kv in p.attributes() {
                                if kv.key.as_str() == "oprobe.stage" {
                                    stages.insert(kv.value.to_string());
                                }
                                labels.insert(kv.key.to_string(), kv.value.to_string());
                            }
                        }
                    }
                }
            }
        }
        assert!(points >= 2, "{points} data points");
        assert!(stages.contains("total"), "{stages:?}");
        // OpenDAL's own metric labels, so oprobe's series join opendal-layer-otelmetrics'.
        assert_eq!(
            labels.get("scheme").map(String::as_str),
            Some("memory"),
            "{labels:?}"
        );
        for l in ["namespace", "root", "operation"] {
            assert!(labels.contains_key(l), "{l}: {labels:?}");
        }
    }

    #[tokio::test]
    async fn cloned_otel_config_keeps_each_operators_service_labels() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let meter = provider.meter("oprobe-trace-multi");
        let cfg = config(&meter);
        let (l1, _h1) = crate::layer(cfg.clone());
        let (l2, _h2) = crate::layer(cfg);
        let op1 =
            opendal_core::Operator::new(opendal_core::services::Memory::default().root("/alpha"))
                .unwrap()
                .layer(l1);
        let op2 =
            opendal_core::Operator::new(opendal_core::services::Memory::default().root("/beta"))
                .unwrap()
                .layer(l2);
        op1.write("k", "1").await.unwrap();
        op2.write("k", "2").await.unwrap();
        provider.force_flush().unwrap();
        let metrics = exporter.get_finished_metrics().unwrap();
        let mut roots = std::collections::BTreeSet::new();
        for rm in &metrics {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if let AggregatedMetrics::F64(MetricData::Histogram(h)) = m.data() {
                        for p in h.data_points() {
                            for kv in p.attributes() {
                                if kv.key.as_str() == "root" {
                                    roots.insert(kv.value.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(roots.contains("/alpha/"), "{roots:?}");
        assert!(roots.contains("/beta/"), "{roots:?}");
    }
}
