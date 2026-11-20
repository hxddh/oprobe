//! `oprobe watch TARGET`: continuous low-rate probing. Each probe window runs the workload
//! open-loop at `--rate` for `--duration`, every `--interval`; a rolling window of the last
//! `--window` is checked against the SLOs (`--slo p99<200ms`, `errors<1%`, `throttle<1%`,
//! `availability>99.9%`), and SLO transitions are logged as events. `--listen ADDR` serves
//! Prometheus text on `/metrics`; `--otlp-endpoint URL` pushes OTLP metrics each window.
//! Runs until Ctrl-C (or `--count` windows); budgets apply to the whole session.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::WatchArgs;
use crate::analysis::stats::quantile_us_ms;
use crate::engine::{Cancel, RunOptions};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::events::{EventKind, LogLevel};
use crate::export::prometheus::Registry;
use crate::model::Run;
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green, red};
use crate::util::{fmt_ms, parse_duration};

/// A service level objective over the rolling window.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Slo {
    /// As given, e.g. `p99<200ms`.
    pub spec: String,
    /// `p50_ms`, `p90_ms`, `p99_ms`, `error_ratio`, `throttle_ratio`, `availability_ratio`.
    pub metric: String,
    /// The value must stay below (`true`) or above (`false`) the limit.
    pub below: bool,
    pub limit: f64,
}

impl Slo {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        let (k, v, below) = if let Some((k, v)) = t.split_once("<=").or_else(|| t.split_once('<')) {
            (k, v, true)
        } else if let Some((k, v)) = t.split_once(">=").or_else(|| t.split_once('>')) {
            (k, v, false)
        } else {
            return Err(format!(
                "invalid SLO `{s}` (examples: p99<200ms, errors<1%, availability>99.9%)"
            ));
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        let pct = |v: &str| -> Result<f64, String> {
            match v.strip_suffix('%') {
                Some(p) => p.trim().parse::<f64>().map(|x| x / 100.0),
                None => v.parse::<f64>(),
            }
            .map_err(|_| format!("invalid SLO value `{v}`"))
        };
        let (metric, limit) = match k.as_str() {
            "p50" | "p90" | "p99" => {
                let ms = parse_duration(v)
                    .map(|d| d.as_secs_f64() * 1000.0)
                    .or_else(|_| {
                        v.trim_end_matches("ms")
                            .parse::<f64>()
                            .map_err(|e| anyhow::anyhow!(e))
                    })
                    .map_err(|_| format!("invalid latency `{v}` (e.g. 200ms)"))?;
                (format!("{k}_ms"), ms)
            }
            "errors" | "error" | "error_rate" => ("error_ratio".into(), pct(v)?),
            "throttle" | "throttling" | "throttled" => ("throttle_ratio".into(), pct(v)?),
            "availability" => ("availability_ratio".into(), pct(v)?),
            _ => {
                return Err(format!(
                    "unknown SLO metric `{k}` (p50, p90, p99, errors, throttle, availability)"
                ));
            }
        };
        Ok(Self {
            spec: t.to_owned(),
            metric,
            below,
            limit,
        })
    }

    fn ok(&self, v: f64) -> bool {
        if self.below {
            v <= self.limit
        } else {
            v >= self.limit
        }
    }
}

/// One probe window's numbers, kept for the rolling window.
struct Window {
    at: Instant,
    op_lat_us: Vec<u64>,
    ops: u64,
    failed: u64,
    requests: u64,
    throttled: u64,
}

/// Metrics of the rolling window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, JsonSchema)]
pub struct Rolling {
    pub windows_count: u64,
    pub ops_count: u64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    pub throttle_ratio: f64,
    pub availability_ratio: f64,
}

impl Rolling {
    fn of(ws: &VecDeque<Window>) -> Self {
        let mut lat: Vec<u64> = ws
            .iter()
            .flat_map(|w| w.op_lat_us.iter().copied())
            .collect();
        let ops: u64 = ws.iter().map(|w| w.ops).sum();
        let failed: u64 = ws.iter().map(|w| w.failed).sum();
        let reqs: u64 = ws.iter().map(|w| w.requests).sum();
        let thr: u64 = ws.iter().map(|w| w.throttled).sum();
        let err = failed as f64 / ops.max(1) as f64;
        Self {
            windows_count: ws.len() as u64,
            ops_count: ops,
            p50_ms: quantile_us_ms(&mut lat, 0.5),
            p90_ms: quantile_us_ms(&mut lat, 0.9),
            p99_ms: quantile_us_ms(&mut lat, 0.99),
            error_ratio: err,
            throttle_ratio: thr as f64 / reqs.max(1) as f64,
            availability_ratio: if ops == 0 { 0.0 } else { 1.0 - err },
        }
    }
    fn get(&self, metric: &str) -> f64 {
        match metric {
            "p50_ms" => self.p50_ms,
            "p90_ms" => self.p90_ms,
            "p99_ms" => self.p99_ms,
            "error_ratio" => self.error_ratio,
            "throttle_ratio" => self.throttle_ratio,
            _ => self.availability_ratio,
        }
    }
}

/// An SLO changing state.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Transition {
    /// RFC 3339.
    pub at: String,
    pub slo: String,
    /// `breached` or `recovered`.
    pub state: String,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SloState {
    pub slo: Slo,
    pub ok: bool,
    pub value: f64,
    /// Probe windows that ended with the SLO breached.
    pub breached_windows_count: u64,
}

/// Result of `oprobe watch` (when it stops).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct WatchOutput {
    pub target: String,
    pub windows_count: u64,
    pub rolling: Rolling,
    pub slos: Vec<SloState>,
    pub transitions: Vec<Transition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
}

impl CommandOutput for WatchOutput {
    fn text(&self) -> String {
        let r = &self.rolling;
        let mut o = format!(
            "{} {} · {} probe windows · rolling p50 {} · p99 {} · errors {:.2}% · throttled {:.2}%\n",
            bold("watch"),
            self.target,
            self.windows_count,
            fmt_ms(r.p50_ms),
            fmt_ms(r.p99_ms),
            r.error_ratio * 100.0,
            r.throttle_ratio * 100.0
        );
        for s in &self.slos {
            o += &format!(
                "  {} {} (now {:.4}; breached in {} windows)\n",
                if s.ok { green("ok") } else { red("BREACHED") },
                s.slo.spec,
                s.value,
                s.breached_windows_count
            );
        }
        for t in &self.transitions {
            o += &dim(&format!("  {} {} {}\n", t.at, t.slo, t.state));
        }
        o
    }
    fn exit_code(&self) -> u8 {
        if self.slos.iter().any(|s| !s.ok) {
            exit::GATE
        } else {
            exit::OK
        }
    }
}

/// Serve `/metrics` from `reg` on `addr` until the process ends.
async fn serve_metrics(addr: &str, reg: Arc<Mutex<Registry>>) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("cannot listen on {addr}: {e}"),
        )
    })?;
    let local = listener.local_addr()?.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let reg = reg.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let (status, ctype, body) = if path == "/metrics" || path.starts_with("/metrics?") {
                    let text = reg.lock().map(|r| r.render()).unwrap_or_default();
                    ("200 OK", "text/plain; version=0.0.4; charset=utf-8", text)
                } else {
                    ("404 Not Found", "text/plain", "see /metrics\n".to_owned())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    Ok(local)
}

pub async fn cmd_watch(a: WatchArgs, ctx: &Ctx) -> Result<u8> {
    let bad = |m: String| -> anyhow::Error {
        OprobeError::new(ErrorCode::UsageInvalidArgument, m).into()
    };
    let slos: Vec<Slo> = a
        .slo
        .iter()
        .map(|s| Slo::parse(s))
        .collect::<Result<_, _>>()
        .map_err(bad)?;
    let mut w = a.workload.clone();
    if w.duration.is_none() {
        w.duration = Some("5s".into());
    }
    let (mut profile, target_name) = super::resolve_profile(ctx, &a.target, &w, &a.knobs, &a.cost)?;
    let window = profile.workload.duration.get();
    let interval =
        parse_duration(&a.interval).map_err(|e| bad(format!("invalid --interval: {e:#}")))?;
    let rolling_for =
        parse_duration(&a.window).map_err(|e| bad(format!("invalid --window: {e:#}")))?;
    let rate = profile.workload.rate.unwrap_or(a.default_rate());
    if rate <= 0.0 {
        return Err(bad("--rate must be positive".into()));
    }
    profile.workload.rate = Some(rate);
    if a.target.target.is_none() && target_name.is_none() && profile.target.url.is_none() {
        return Err(bad("watch needs a target".into()));
    }
    let rails = super::run::rails(ctx, &profile, &a.safety)?;
    if a.safety.dry_run {
        // The plan of every window (one window when it runs until Ctrl-C).
        let mut p = profile.clone();
        let windows = a.count.unwrap_or(1).max(1);
        p.workload.duration = crate::util::HumanDuration(window * windows as u32);
        p.workload.warmup = crate::util::HumanDuration(Duration::ZERO);
        let mut plan = rails.plan(&p);
        plan.assumptions.push(match a.count {
            Some(n) => format!("{n} windows of {window:?} every {}", a.interval),
            None => format!(
                "one window of {window:?}; watch repeats it every {} until Ctrl-C (budgets cap the total)",
                a.interval
            ),
        });
        return ctx.out.finish(&super::run::PlanOutput { plan });
    }
    let guard = super::run::guard_for(ctx, &profile, &rails.budgets);
    let cancel = Cancel::new();
    super::install_ctrl_c(cancel.clone());
    let reg = Arc::new(Mutex::new(Registry::default()));
    let listen = match &a.listen {
        Some(addr) => {
            let local = serve_metrics(addr, reg.clone()).await?;
            ctx.out.info(format!(
                "serving Prometheus metrics on http://{local}/metrics"
            ));
            Some(local)
        }
        None => None,
    };
    let display = profile.target.display();
    ctx.out.info(format!(
        "{} {display} · {rate} ops/s for {} every {} · rolling {} · Ctrl-C to stop",
        bold("watching"),
        a.workload.duration.as_deref().unwrap_or("5s"),
        a.interval,
        a.window
    ));
    let quiet: crate::engine::ProgressFn = Arc::new(|_| {});
    let session = crate::engine::measure::open(
        profile,
        &rails,
        guard,
        super::trace_max_requests(ctx),
        cancel.clone(),
        quiet,
    )
    .await?;
    let knobs = session.profile.knobs.clone();
    let opts = RunOptions {
        duration: window,
        warmup: Duration::ZERO,
        max_ops: None,
        rate: Some(rate),
        trace_slow_ops: false,
    };
    let mut windows: VecDeque<Window> = VecDeque::new();
    let mut ok: Vec<bool> = vec![true; slos.len()];
    let mut breached_windows = vec![0u64; slos.len()];
    let mut transitions = Vec::new();
    let mut count = 0u64;
    let mut rolling = Rolling::default();
    let mut failure: Option<anyhow::Error> = None;
    loop {
        if cancel.is_cancelled() || a.count.is_some_and(|c| count >= c) {
            break;
        }
        let started = Instant::now();
        let started_at = SystemTime::now();
        let snap = match session.run(&knobs, &opts, None).await {
            Ok(s) => s,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        let run: Run = crate::model::build_run(crate::model::ReportInput {
            run_id: &crate::model::RunId::new(),
            target_name: target_name.clone(),
            profile: &session.profile,
            knobs: &knobs,
            snapshot: snap,
            trace: session.take_trace().unwrap_or_default(),
            cleanup: Default::default(),
            warnings: Vec::new(),
            started_at,
            ended_at: SystemTime::now(),
        });
        count += 1;
        let r = &run.report;
        windows.push_back(Window {
            at: started,
            op_lat_us: run
                .ops
                .iter()
                .filter(|o| o.ok)
                .map(|o| o.latency_us)
                .collect(),
            ops: r.summary.ops,
            failed: r.summary.failed_ops,
            requests: r.connections.requests,
            throttled: r.resilience.throttled_responses,
        });
        while windows
            .front()
            .is_some_and(|w| w.at.elapsed() > rolling_for)
            && windows.len() > 1
        {
            windows.pop_front();
        }
        rolling = Rolling::of(&windows);
        ctx.out.bus().emit(EventKind::Interval {
            t_s: count as f64,
            ops_per_s: r.summary.ops_per_s,
            mib_per_s: r.summary.mib_per_s,
            p50_ms: r.summary.latency.p50,
            p99_ms: r.summary.latency.p99,
            errors: r.summary.failed_ops,
            throttled: r.resilience.throttled_responses,
        });
        if ctx.out.format == crate::output::OutputFormat::Text {
            ctx.out.info(dim(&format!(
                "window {count}: {:.1} ops/s · p99 {} · rolling p99 {} · errors {:.2}%",
                r.summary.ops_per_s,
                fmt_ms(r.summary.latency.p99),
                fmt_ms(rolling.p99_ms),
                rolling.error_ratio * 100.0
            )));
        }
        {
            let mut g = reg.lock().unwrap_or_else(|e| e.into_inner());
            g.observe(&run);
            g.gauge("window_p50_seconds", vec![], rolling.p50_ms / 1000.0);
            g.gauge("window_p99_seconds", vec![], rolling.p99_ms / 1000.0);
            g.gauge("window_error_ratio", vec![], rolling.error_ratio);
            g.gauge("window_throttle_ratio", vec![], rolling.throttle_ratio);
            for (i, s) in slos.iter().enumerate() {
                let v = rolling.get(&s.metric);
                let now = s.ok(v);
                if !now {
                    breached_windows[i] += 1;
                }
                g.gauge(
                    "slo_ok",
                    vec![("slo", s.spec.clone())],
                    if now { 1.0 } else { 0.0 },
                );
                if now != ok[i] {
                    ok[i] = now;
                    let state = if now { "recovered" } else { "breached" };
                    let msg = format!("SLO {} {state}: {} = {v:.4}", s.spec, s.metric);
                    if now {
                        ctx.out.bus().log(LogLevel::Info, msg.clone());
                        if ctx.out.format != crate::output::OutputFormat::Ndjson {
                            ctx.out.info(green(&msg));
                        }
                    } else {
                        ctx.out.warn(msg);
                    }
                    transitions.push(Transition {
                        at: crate::util::now_rfc3339(),
                        slo: s.spec.clone(),
                        state: state.into(),
                        value: v,
                    });
                }
            }
        }
        if let Some(ep) = &a.otlp_endpoint
            && let Err(e) =
                crate::export::otlp::push(ep, "metrics", &crate::export::otlp::metrics(&run)).await
        {
            ctx.out.warn(format!("{e:#}"));
        }
        if session.guard().is_some_and(|g| g.tripped()) {
            break;
        }
        if a.count.is_some_and(|c| count >= c) {
            break;
        }
        let rest = interval.saturating_sub(started.elapsed());
        tokio::select! {
            _ = tokio::time::sleep(rest) => {}
            _ = cancel.cancelled() => {}
        }
    }
    let _ = session.close().await;
    if let Some(e) = failure {
        return Err(e);
    }
    if let Some(b) = session.guard().and_then(|g| g.breach()) {
        return Err(b.to_error());
    }
    ctx.out.finish(&WatchOutput {
        target: display,
        windows_count: count,
        slos: slos
            .iter()
            .enumerate()
            .map(|(i, s)| SloState {
                slo: s.clone(),
                ok: ok[i],
                value: rolling.get(&s.metric),
                breached_windows_count: breached_windows[i],
            })
            .collect(),
        rolling,
        transitions,
        listen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slos_parse() {
        let s = Slo::parse("p99<200ms").unwrap();
        assert_eq!(
            (s.metric.as_str(), s.limit, s.below),
            ("p99_ms", 200.0, true)
        );
        let s = Slo::parse("errors<1%").unwrap();
        assert_eq!(s.limit, 0.01);
        let s = Slo::parse("availability>=99.9%").unwrap();
        assert!(!s.below && (s.limit - 0.999).abs() < 1e-12);
        assert!(s.ok(1.0) && !s.ok(0.99));
        assert!(Slo::parse("cpu<3").is_err());
        assert!(Slo::parse("p99").is_err());
    }
}
