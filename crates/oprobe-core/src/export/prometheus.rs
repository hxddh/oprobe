//! Prometheus text exposition (format 0.0.4) for `oprobe watch --listen`: cumulative
//! histograms and counters over every probed request, plus gauges of the rolling window and
//! the SLO states. Names follow OTel semantics in Prometheus spelling.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::otlp::DURATION_BOUNDS_S;
use crate::model::Run;

#[derive(Clone, Debug, Default)]
struct Hist {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Hist {
    fn observe(&mut self, v: f64) {
        if self.counts.is_empty() {
            self.counts = vec![0; DURATION_BOUNDS_S.len()];
        }
        for (i, b) in DURATION_BOUNDS_S.iter().enumerate() {
            if v <= *b {
                self.counts[i] += 1;
            }
        }
        self.sum += v;
        self.count += 1;
    }
}

type Labels = Vec<(&'static str, String)>;

/// Accumulated metrics of a `watch` session.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    req: BTreeMap<Labels, Hist>,
    ops: BTreeMap<Labels, Hist>,
    requests: BTreeMap<Labels, u64>,
    errors: BTreeMap<Labels, u64>,
    throttled: BTreeMap<Labels, u64>,
    gauges: BTreeMap<(&'static str, Labels), f64>,
    probes: u64,
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn fmt_labels(l: &[(&'static str, String)], extra: Option<(&str, String)>) -> String {
    let mut parts: Vec<String> = l
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", esc(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{v}\""));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

impl Registry {
    /// Add every request and operation of one probe window.
    pub fn observe(&mut self, run: &Run) {
        self.probes += 1;
        for q in run.reqs.iter().filter(|q| q.t_us >= 0) {
            let api = q.api.to_string();
            for (stage, us, keep) in [
                ("total", q.total_us, true),
                ("queue", q.queue_us, true),
                ("connect", q.connect_us(), !q.reused && q.connect_us() > 0),
                ("send", q.send_us, true),
                ("ttfb", q.ttfb_us, true),
                ("body", q.body_us, true),
            ] {
                if keep {
                    self.req
                        .entry(vec![("api", api.clone()), ("stage", stage.into())])
                        .or_default()
                        .observe(us as f64 / 1e6);
                }
            }
            *self
                .requests
                .entry(vec![("api", api.clone()), ("status", q.status.to_string())])
                .or_default() += 1;
            if q.is_error() && !q.is_throttle() && q.status != 404 {
                *self.errors.entry(vec![("api", api.clone())]).or_default() += 1;
            }
            if q.is_throttle() {
                *self.throttled.entry(vec![("api", api)]).or_default() += 1;
            }
        }
        for o in run.ops.iter().filter(|o| o.t_us >= 0) {
            self.ops
                .entry(vec![("verb", o.verb.to_string())])
                .or_default()
                .observe(o.latency_us as f64 / 1e6);
        }
    }

    /// Set a gauge (`name` without the `oprobe_` prefix).
    pub fn gauge(&mut self, name: &'static str, labels: Labels, v: f64) {
        self.gauges.insert((name, labels), v);
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        let hist = |o: &mut String, name: &str, help: &str, m: &BTreeMap<Labels, Hist>| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} histogram");
            for (l, h) in m {
                for (i, b) in DURATION_BOUNDS_S.iter().enumerate() {
                    let _ = writeln!(
                        o,
                        "{name}_bucket{} {}",
                        fmt_labels(l, Some(("le", b.to_string()))),
                        h.counts[i]
                    );
                }
                let _ = writeln!(
                    o,
                    "{name}_bucket{} {}",
                    fmt_labels(l, Some(("le", "+Inf".into()))),
                    h.count
                );
                let _ = writeln!(o, "{name}_sum{} {}", fmt_labels(l, None), h.sum);
                let _ = writeln!(o, "{name}_count{} {}", fmt_labels(l, None), h.count);
            }
        };
        let counter = |o: &mut String, name: &str, help: &str, m: &BTreeMap<Labels, u64>| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} counter");
            for (l, v) in m {
                let _ = writeln!(o, "{name}{} {v}", fmt_labels(l, None));
            }
        };
        hist(
            &mut o,
            "oprobe_request_duration_seconds",
            "Backend request time by API and stage (stage=total: the whole request).",
            &self.req,
        );
        hist(
            &mut o,
            "oprobe_operation_duration_seconds",
            "Logical operation latency by verb.",
            &self.ops,
        );
        counter(
            &mut o,
            "oprobe_requests_total",
            "Backend requests by API and HTTP status (0: no response).",
            &self.requests,
        );
        counter(
            &mut o,
            "oprobe_request_errors_total",
            "Failed requests (excluding throttling and 404).",
            &self.errors,
        );
        counter(
            &mut o,
            "oprobe_requests_throttled_total",
            "Requests answered with 429/503.",
            &self.throttled,
        );
        let _ = writeln!(
            o,
            "# HELP oprobe_probes_total Probe windows completed.\n# TYPE oprobe_probes_total counter\noprobe_probes_total {}",
            self.probes
        );
        let mut last = "";
        for ((name, l), v) in &self.gauges {
            if *name != last {
                let _ = writeln!(o, "# TYPE oprobe_{name} gauge");
                last = name;
            }
            let _ = writeln!(o, "oprobe_{name}{} {v}", fmt_labels(l, None));
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;

    #[test]
    fn exposition_is_cumulative_and_labelled() {
        let mut reg = Registry::default();
        let run = straggler_run();
        reg.observe(&run);
        reg.observe(&run);
        reg.gauge("slo_ok", vec![("slo", "p99<200ms".into())], 0.0);
        let t = reg.render();
        assert!(t.contains("# TYPE oprobe_request_duration_seconds histogram"));
        assert!(
            t.contains(
                "oprobe_request_duration_seconds_count{api=\"GetObject\",stage=\"total\"} 2000"
            ),
            "{t}"
        );
        assert!(t.contains("le=\"+Inf\""));
        assert!(t.contains("oprobe_requests_total{api=\"GetObject\",status=\"200\"} 2000"));
        assert!(t.contains("oprobe_slo_ok{slo=\"p99<200ms\"} 0"));
        assert!(t.contains("oprobe_probes_total 2"));
    }
}
