//! Prometheus text exposition (format 0.0.4) for `oprobe watch --listen`: cumulative
//! histograms and counters over every probed request, plus gauges of the rolling window and
//! the SLO states. Names follow OTel semantics in Prometheus spelling.
//!
//! Every series carries a `target` label (design v5 §6, review C8), every metric has HELP
//! text, `oprobe_build_info` names the version, `oprobe_last_success_timestamp_seconds` the
//! end of the last probe window without failed operations, and [`Registry::health`] backs
//! `/healthz`.

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
    /// The `target` label of every series (empty: none).
    target: String,
    /// Unix seconds of the last probe window without failed operations.
    last_success_s: Option<f64>,
    /// Unix seconds of the last probe window.
    last_probe_s: Option<f64>,
}

/// HELP text of the gauges `watch` sets (name without the `oprobe_` prefix).
fn gauge_help(name: &str) -> &'static str {
    match name {
        "window_p50_seconds" => "Operation p50 latency over the rolling window.",
        "window_p99_seconds" => "Operation p99 latency over the rolling window.",
        "window_error_ratio" => "Share of failed operations over the rolling window.",
        "window_throttled_request_ratio" => {
            "Share of requests answered with 429/503 over the rolling window."
        }
        "slo_ok" => "1 while the SLO holds over the rolling window, else 0.",
        _ => "A gauge of oprobe watch.",
    }
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn fmt_labels(target: &str, l: &[(&'static str, String)], extra: Option<(&str, String)>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !target.is_empty() {
        parts.push(format!("target=\"{}\"", esc(target)));
    }
    parts.extend(l.iter().map(|(k, v)| format!("{k}=\"{}\"", esc(v))));
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
    /// A registry whose series carry `target="…"`.
    pub fn for_target(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            ..Self::default()
        }
    }

    /// Record the end of a probe window at `unix_s`; `ok` when no operation failed.
    pub fn probe_done(&mut self, unix_s: f64, ok: bool) {
        self.last_probe_s = Some(unix_s);
        if ok {
            self.last_success_s = Some(unix_s);
        }
    }

    /// `/healthz`: healthy when a probe window succeeded within `max_age_s` of `now_s`
    /// (before the first window: healthy, the session is starting). The text says why.
    pub fn health(&self, now_s: f64, max_age_s: f64) -> (bool, String) {
        match (self.last_success_s, self.last_probe_s) {
            (_, None) => (true, "starting: no probe window yet\n".into()),
            (Some(t), _) if now_s - t <= max_age_s => (
                true,
                format!(
                    "ok: last successful probe {:.0}s ago\n",
                    (now_s - t).max(0.0)
                ),
            ),
            (Some(t), _) => (
                false,
                format!("unhealthy: last successful probe {:.0}s ago\n", now_s - t),
            ),
            (None, Some(_)) => (false, "unhealthy: no successful probe window yet\n".into()),
        }
    }

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
        let t = self.target.as_str();
        let _ = writeln!(
            o,
            "# HELP oprobe_build_info The oprobe build serving these metrics (value 1).\n# TYPE oprobe_build_info gauge\noprobe_build_info{} 1",
            fmt_labels(
                t,
                &[("version", env!("CARGO_PKG_VERSION").to_owned())],
                None
            )
        );
        let hist = |o: &mut String, name: &str, help: &str, m: &BTreeMap<Labels, Hist>| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} histogram");
            for (l, h) in m {
                for (i, b) in DURATION_BOUNDS_S.iter().enumerate() {
                    let _ = writeln!(
                        o,
                        "{name}_bucket{} {}",
                        fmt_labels(t, l, Some(("le", b.to_string()))),
                        h.counts[i]
                    );
                }
                let _ = writeln!(
                    o,
                    "{name}_bucket{} {}",
                    fmt_labels(t, l, Some(("le", "+Inf".into()))),
                    h.count
                );
                let _ = writeln!(o, "{name}_sum{} {}", fmt_labels(t, l, None), h.sum);
                let _ = writeln!(o, "{name}_count{} {}", fmt_labels(t, l, None), h.count);
            }
        };
        let counter = |o: &mut String, name: &str, help: &str, m: &BTreeMap<Labels, u64>| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} counter");
            for (l, v) in m {
                let _ = writeln!(o, "{name}{} {v}", fmt_labels(t, l, None));
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
            "# HELP oprobe_probes_total Probe windows completed.\n# TYPE oprobe_probes_total counter\noprobe_probes_total{} {}",
            fmt_labels(t, &[], None),
            self.probes
        );
        if let Some(ts) = self.last_success_s {
            let _ = writeln!(
                o,
                "# HELP oprobe_last_success_timestamp_seconds End of the last probe window without failed operations (Unix time).\n# TYPE oprobe_last_success_timestamp_seconds gauge\noprobe_last_success_timestamp_seconds{} {ts:.3}",
                fmt_labels(t, &[], None)
            );
        }
        let mut last = "";
        for ((name, l), v) in &self.gauges {
            if *name != last {
                let _ = writeln!(
                    o,
                    "# HELP oprobe_{name} {}\n# TYPE oprobe_{name} gauge",
                    gauge_help(name)
                );
                last = name;
            }
            let _ = writeln!(o, "oprobe_{name}{} {v}", fmt_labels(t, l, None));
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
        let mut reg = Registry::for_target("s3://b/p");
        let run = straggler_run();
        reg.observe(&run);
        reg.observe(&run);
        reg.probe_done(1_800_000_000.0, true);
        reg.gauge("slo_ok", vec![("slo", "p99<200ms".into())], 0.0);
        let t = reg.render();
        assert!(t.contains("# TYPE oprobe_request_duration_seconds histogram"));
        assert!(
            t.contains(
                "oprobe_request_duration_seconds_count{target=\"s3://b/p\",api=\"GetObject\",stage=\"total\"} 2000"
            ),
            "{t}"
        );
        assert!(t.contains("le=\"+Inf\""));
        assert!(t.contains(
            "oprobe_requests_total{target=\"s3://b/p\",api=\"GetObject\",status=\"200\"} 2000"
        ));
        assert!(t.contains("oprobe_slo_ok{target=\"s3://b/p\",slo=\"p99<200ms\"} 0"));
        assert!(t.contains("oprobe_probes_total{target=\"s3://b/p\"} 2"));
        assert!(
            t.contains("oprobe_build_info{target=\"s3://b/p\",version=\""),
            "{t}"
        );
        assert!(
            t.contains("oprobe_last_success_timestamp_seconds{target=\"s3://b/p\"} 1800000000.000")
        );
        // Every metric has HELP and TYPE.
        let types = t.lines().filter(|l| l.starts_with("# TYPE")).count();
        let helps = t.lines().filter(|l| l.starts_with("# HELP")).count();
        assert_eq!(types, helps, "{t}");
    }

    #[test]
    fn health_follows_the_last_successful_window() {
        let mut reg = Registry::for_target("x");
        assert!(reg.health(100.0, 30.0).0, "starting");
        reg.probe_done(100.0, false);
        assert!(!reg.health(101.0, 30.0).0);
        reg.probe_done(110.0, true);
        assert!(reg.health(120.0, 30.0).0);
        reg.probe_done(150.0, false);
        let (ok, why) = reg.health(150.0, 30.0);
        assert!(!ok && why.contains("40s"), "{why}");
    }
}
