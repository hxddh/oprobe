//! OpenTelemetry metrics (feature `otel`), following the HTTP semantic conventions.
//!
//! [`OtelExporter`] records every request into the `http.client.request.duration` histogram
//! (seconds), once per stage: `oprobe.stage` is `total` (the whole request, what the semantic
//! convention means) or one of `queue`, `connect`, `send`, `ttfb`, `body`, `stall` (stages
//! that took no time are skipped). Attributes: `oprobe.stage`, `oprobe.api` (`GetObject`, …),
//! `http.request.method` and `http.response.status_code` (absent when no response arrived).
//!
//! ```no_run
//! let meter = opentelemetry::global::meter("my-app");
//! let (layer, handle) = oprobe_trace::layer(oprobe_trace::otel::config(&meter));
//! # let _ = (layer, handle);
//! ```

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, Meter};

use crate::record::RequestRecord;
use crate::recorder::Exporter;

/// The metric name (HTTP semantic conventions).
pub const REQUEST_DURATION: &str = "http.client.request.duration";

/// Emits [`REQUEST_DURATION`] per request and stage.
pub struct OtelExporter {
    duration: Histogram<f64>,
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
            let mut attrs = vec![
                KeyValue::new("oprobe.stage", stage),
                KeyValue::new("oprobe.api", api),
                KeyValue::new("http.request.method", r.method.clone()),
            ];
            if r.status > 0 {
                attrs.push(KeyValue::new(
                    "http.response.status_code",
                    i64::from(r.status),
                ));
            }
            self.duration.record(us as f64 / 1e6, &attrs);
        }
    }
}

/// A [`crate::Config`] that exports to `meter`.
pub fn config(meter: &Meter) -> crate::Config {
    crate::Config::default().exporter(Arc::new(OtelExporter::new(meter)))
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
                            }
                        }
                    }
                }
            }
        }
        assert!(points >= 2, "{points} data points");
        assert!(stages.contains("total"), "{stages:?}");
    }
}
