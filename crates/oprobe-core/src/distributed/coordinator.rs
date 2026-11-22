//! The coordinator of a distributed run (design v5 §8): checks the workers (version, clock
//! offset, capabilities), splits the run, starts every worker at one instant, enforces the
//! global budgets and fans out cancellation while it streams their progress, then merges
//! their results into one run.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

use super::protocol::*;
use super::{WorkerAddr, merge};
use crate::clients::ClientChoice;
use crate::config::Profile;
use crate::engine::{Cancel, CleanupSummary, Progress, ProgressFn};
use crate::errors::{ErrorCode, OprobeError};
use crate::guard::{Budgets, Guard};
use crate::model::{DimensionSlice, Run, WorkerInfo};

/// Pings of the clock-offset exchange (the one with the shortest round trip wins).
const PINGS: usize = 8;
/// A worker whose clock differs more than this is named in a warning (start times are
/// corrected either way).
const OFFSET_WARN_MS: f64 = 100.0;
/// Start uncertainty (half the best round trip) worth a warning.
const UNCERTAINTY_WARN_MS: f64 = 5.0;
/// A worker that started its window this much after the others is named in a warning.
const LATE_WARN_MS: f64 = 50.0;

/// A checked worker.
pub struct Conn {
    /// `w1`, `w2`, …
    pub id: String,
    pub addr: String,
    pub weight: f64,
    base: String,
    http: reqwest::Client,
    token: String,
    /// The worker's clock minus the coordinator's (µs).
    pub offset_us: i64,
    /// Round trip of the best ping (µs).
    pub rtt_us: i64,
    pub hello: Hello,
    /// Loopback or TLS: credentials may be sent to it.
    pub trusted: bool,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("Conn")
            .field("id", &self.id)
            .field("addr", &self.addr)
            .field("offset_us", &self.offset_us)
            .field("rtt_us", &self.rtt_us)
            .finish_non_exhaustive()
    }
}

fn err(code: ErrorCode, msg: impl Into<String>) -> OprobeError {
    OprobeError::new(code, msg)
}

impl Conn {
    /// Connect to `addr` (`host:port`, or `https://host:port` for a TLS worker) and measure
    /// its clock offset.
    pub async fn connect(index: usize, w: &WorkerAddr, token: &str) -> Result<Self> {
        let (base, tls) = if let Some(rest) = w.addr.strip_prefix("https://") {
            (format!("https://{}", rest.trim_end_matches('/')), true)
        } else {
            (
                format!(
                    "http://{}",
                    w.addr.trim_start_matches("http://").trim_end_matches('/')
                ),
                false,
            )
        };
        let mut b = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60));
        if tls {
            if let Some(ca) = std::env::var_os("OPROBE_WORKER_CA") {
                let pem = std::fs::read(&ca).with_context(|| {
                    format!("cannot read OPROBE_WORKER_CA `{}`", ca.to_string_lossy())
                })?;
                b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
            }
        } else {
            b = b.http2_prior_knowledge();
        }
        let host = w
            .addr
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .rsplit_once(':')
            .map(|(h, _)| h.trim_matches(['[', ']']).to_owned())
            .unwrap_or_default();
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        let mut c = Self {
            id: format!("w{}", index + 1),
            addr: w.addr.clone(),
            weight: if w.weight > 0.0 { w.weight } else { 1.0 },
            base,
            http: b.build()?,
            token: token.to_owned(),
            offset_us: 0,
            rtt_us: i64::MAX,
            hello: Hello {
                protocol: String::new(),
                version: String::new(),
                clock_unix_us: 0,
                cpu_cores_count: 0,
                clients: Vec::new(),
                busy: false,
            },
            trusted: loopback || tls,
        };
        for _ in 0..PINGS {
            let t0 = now_unix_us();
            let h: Hello = c.get("/v1/hello").await?;
            let t3 = now_unix_us();
            let rtt = t3 - t0;
            if rtt < c.rtt_us {
                c.rtt_us = rtt;
                c.offset_us = h.clock_unix_us - (t0 + t3) / 2;
                c.hello = h;
            }
        }
        let h = &c.hello;
        if h.protocol
            .split('.')
            .take(2)
            .ne(PROTOCOL.split('.').take(2))
        {
            return Err(err(
                ErrorCode::UsageInvalidArgument,
                format!(
                    "worker {} speaks `{}`, not `{PROTOCOL}`",
                    c.addr, h.protocol
                ),
            )
            .into());
        }
        if h.version != env!("CARGO_PKG_VERSION") {
            return Err(err(
                ErrorCode::UsageInvalidArgument,
                format!(
                    "worker {} runs oprobe {}, the coordinator {}",
                    c.addr,
                    h.version,
                    env!("CARGO_PKG_VERSION")
                ),
            )
            .with_hint("run the same oprobe version on every machine")
            .into());
        }
        if h.busy {
            return Err(err(
                ErrorCode::UsageInvalidArgument,
                format!("worker {} is busy with another run", c.addr),
            )
            .into());
        }
        Ok(c)
    }

    async fn check<T: DeserializeOwned>(&self, path: &str, resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let body = resp.bytes().await?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(err(
                ErrorCode::AuthDenied,
                format!("worker {} refused the token", self.addr),
            )
            .with_hint("use the token file the worker was started with (--worker-token-file)")
            .into());
        }
        if !status.is_success() {
            let why = serde_json::from_slice::<Ack>(&body)
                .ok()
                .and_then(|a| a.error)
                .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
            anyhow::bail!("worker {} ({}): {path}: {why}", self.id, self.addr);
        }
        serde_json::from_slice(&body)
            .with_context(|| format!("worker {} answered {path} with invalid JSON", self.addr))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| self.net_error(path, e))?;
        self.check(path, resp).await
    }

    async fn post<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(body)?)
            .send()
            .await
            .map_err(|e| self.net_error(path, e))?;
        self.check(path, resp).await
    }

    fn net_error(&self, path: &str, e: reqwest::Error) -> anyhow::Error {
        let mut s = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(c) = src {
            s.push_str(": ");
            s.push_str(&c.to_string());
            src = c.source();
        }
        err(
            ErrorCode::NetUnreachable,
            format!("cannot reach worker {} ({path}): {s}", self.addr),
        )
        .with_hint("check that `oprobe worker` runs there and the address is right")
        .into()
    }

    async fn result(&self) -> Result<Option<WorkerResult>> {
        let resp = self
            .http
            .get(format!("{}/v1/result", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| self.net_error("/v1/result", e))?;
        if resp.status() == reqwest::StatusCode::ACCEPTED {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let a: Ack = self.check("/v1/result", resp).await?;
            anyhow::bail!("worker {}: {}", self.id, a.error.unwrap_or_default());
        }
        let b = resp.bytes().await?;
        Ok(Some(super::worker::decode(&b)?))
    }

    async fn cancel(&self) {
        let _ = self.post::<_, Ack>("/v1/cancel", &()).await;
    }
}

/// Integer shares of `total` by weight (largest remainder), each at least `min`.
pub fn shares(total: u64, weights: &[f64], min: u64) -> Vec<u64> {
    let sum: f64 = weights.iter().sum::<f64>().max(f64::MIN_POSITIVE);
    let exact: Vec<f64> = weights.iter().map(|w| total as f64 * w / sum).collect();
    let mut out: Vec<u64> = exact.iter().map(|x| x.floor() as u64).collect();
    let mut left = total.saturating_sub(out.iter().sum());
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&a, &b| (exact[b] - exact[b].floor()).total_cmp(&(exact[a] - exact[a].floor())));
    for &i in order.iter().cycle().take(weights.len() * 2) {
        if left == 0 {
            break;
        }
        out[i] += 1;
        left -= 1;
    }
    out.iter().map(|&x| x.max(min)).collect()
}

/// Worker `i`'s profile: its share of the concurrency, the rate, the operation cap and a
/// generated dataset (an existing dataset is split by key range on the workers).
pub fn split_profile(p: &Profile, weights: &[f64]) -> Vec<Profile> {
    let n = weights.len();
    let conc = shares(p.knobs.concurrency.max(1) as u64, weights, 1);
    let sum: f64 = weights.iter().sum();
    let max_ops = p.workload.max_ops.map(|m| shares(m, weights, 1));
    let ds = p.workload.effective_dataset();
    let objects = shares(ds.objects.max(1) as u64, weights, 1);
    (0..n)
        .map(|i| {
            let mut q = p.clone();
            q.knobs.concurrency = conc[i] as usize;
            q.workload.rate = p.workload.rate.map(|r| r * weights[i] / sum);
            q.workload.max_ops = max_ops.as_ref().map(|m| m[i]);
            if ds.existing_prefix.is_none() {
                let mut d = ds.clone();
                d.objects = objects[i] as usize;
                q.workload.dataset = Some(d);
                // Distinct objects per worker (and distinct persisted datasets).
                q.workload.seed = p.workload.seed.wrapping_add(i as u64);
            }
            q
        })
        .collect()
}

/// Worker `i`'s share of the budgets (durations are not divided).
pub fn split_budgets(b: &Budgets, weights: &[f64]) -> Vec<Budgets> {
    let sum: f64 = weights.iter().sum();
    weights
        .iter()
        .map(|w| {
            let f = w / sum;
            Budgets {
                max_cost_usd: b.max_cost_usd.map(|c| c * f),
                max_requests_count: b
                    .max_requests_count
                    .map(|r| ((r as f64 * f).ceil() as u64).max(1)),
                max_bytes: b.max_bytes.map(|r| ((r as f64 * f).ceil() as u64).max(1)),
                max_duration_s: b.max_duration_s,
            }
        })
        .collect()
}

/// What [`coordinate`] needs.
pub struct Plan<'a> {
    pub conns: &'a [Conn],
    pub profile: &'a Profile,
    pub target_name: Option<String>,
    pub client: &'a ClientChoice,
    pub rails: &'a crate::engine::measure::Rails,
    pub guard: Option<Arc<Guard>>,
    pub trace_max_requests: usize,
    pub cancel: &'a Cancel,
    pub progress: Option<ProgressFn>,
    pub info: &'a (dyn Fn(String) + Send + Sync),
}

/// What [`coordinate`] produced.
pub struct Coordinated {
    pub run: Run,
    pub breach: Option<crate::guard::BudgetBreach>,
    pub cancelled: bool,
}

async fn all<T>(
    futs: impl IntoIterator<Item = impl std::future::Future<Output = Result<T>>>,
) -> Result<Vec<T>> {
    futures::future::join_all(futs).await.into_iter().collect()
}

async fn cancel_all(conns: &[Conn]) {
    futures::future::join_all(conns.iter().map(|c| c.cancel())).await;
}

/// Run the plan end to end and merge the result.
pub async fn coordinate(p: Plan<'_>) -> Result<Coordinated> {
    let conns = p.conns;
    let n = conns.len();
    let weights: Vec<f64> = conns.iter().map(|c| c.weight).collect();
    for c in conns {
        if !c.hello.clients.iter().any(|k| k == p.client.kind()) {
            return Err(err(
                ErrorCode::UsageInvalidArgument,
                format!(
                    "worker {} ({}) cannot drive the `{}` client",
                    c.id,
                    c.addr,
                    p.client.kind()
                ),
            )
            .with_hint(if matches!(p.client, ClientChoice::Exec { .. }) {
                "start the workers with --allow-exec"
            } else {
                "use a worker build with this client"
            })
            .into());
        }
    }
    let profiles = split_profile(p.profile, &weights);
    let budgets = split_budgets(&p.rails.budgets, &weights);
    let spec_of = |q: &Profile| crate::engine::measure::spec_of(q, p.target_name.as_deref());
    let started_at = SystemTime::now();

    // 1. Prepare (sessions, datasets) everywhere.
    let jobs = conns.iter().enumerate().map(|(i, c)| {
        let job = Job {
            run_id: p.rails.run_id.to_string(),
            index: i as u32,
            count: n as u32,
            profile: c.trusted.then(|| profiles[i].clone()),
            spec: (!c.trusted).then(|| spec_of(&profiles[i])),
            client: p.client.clone(),
            run_prefix: format!("{}{}/", p.rails.run_prefix, c.id),
            budgets: budgets[i].clone(),
            read_only: p.rails.read_only,
            trace_max_requests_count: p.trace_max_requests,
        };
        async move { c.post::<_, Ack>("/v1/prepare", &job).await }
    });
    if let Err(e) = all(jobs).await {
        cancel_all(conns).await;
        return Err(e);
    }
    (p.info)(format!("{n} workers preparing…"));
    loop {
        if p.cancel.is_cancelled() {
            cancel_all(conns).await;
            anyhow::bail!(err(
                ErrorCode::RunInterrupted,
                "the distributed run was cancelled before it started"
            ));
        }
        let st = match all(conns.iter().map(|c| c.get::<Status>("/v1/status"))).await {
            Ok(s) => s,
            Err(e) => {
                cancel_all(conns).await;
                return Err(e);
            }
        };
        if let Some((c, s)) = conns
            .iter()
            .zip(&st)
            .find(|(_, s)| s.state == State::Failed)
        {
            cancel_all(conns).await;
            anyhow::bail!(
                "worker {} ({}) failed to prepare: {}",
                c.id,
                c.addr,
                s.error.clone().unwrap_or_default()
            );
        }
        if st.iter().all(|s| s.state == State::Ready) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 2. Start everyone at one instant (each on its own clock).
    let lead_us = 300_000 + conns.iter().map(|c| c.rtt_us).max().unwrap_or(0) * 4;
    let start_coord = now_unix_us() + lead_us;
    if let Err(e) = all(conns.iter().map(|c| async move {
        let start = Start {
            at_unix_us: start_coord + c.offset_us,
        };
        c.post::<_, Ack>("/v1/start", &start).await
    }))
    .await
    {
        cancel_all(conns).await;
        return Err(e);
    }

    // 3. Stream progress, enforce the global budgets, fan out cancellation.
    let mut used = vec![(0u64, 0u64); n];
    let mut cancelled_sent = false;
    let mut failure: Option<String> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let st =
            futures::future::join_all(conns.iter().map(|c| c.get::<Status>("/v1/status"))).await;
        let mut live = Vec::new();
        let mut done = true;
        for (i, s) in st.iter().enumerate() {
            match s {
                Ok(s) => {
                    if let Some(g) = &p.guard {
                        let (r, b) = (s.requests_count, s.bytes);
                        let (dr, db) = (r.saturating_sub(used[i].0), b.saturating_sub(used[i].1));
                        used[i] = (used[i].0.max(r), used[i].1.max(b));
                        g.add(dr, db);
                    }
                    if s.state == State::Failed && failure.is_none() {
                        failure = Some(format!(
                            "worker {} failed: {}",
                            conns[i].id,
                            s.error.clone().unwrap_or_default()
                        ));
                    }
                    if !matches!(s.state, State::Done | State::Failed) {
                        done = false;
                    }
                    if let Some(sn) = &s.snapshot {
                        live.push((**sn).clone());
                    }
                }
                Err(e) => {
                    if failure.is_none() {
                        failure = Some(format!("{e:#}"));
                    }
                }
            }
        }
        if let Some(g) = &p.guard {
            g.tick();
        }
        if (p.cancel.is_cancelled() || failure.is_some()) && !cancelled_sent {
            cancel_all(conns).await;
            cancelled_sent = true;
        }
        if let (Some(f), false) = (&p.progress, live.is_empty()) {
            f(Progress::Snapshot(Box::new(merge::merge_snapshots(&live))));
        }
        if done
            || (failure.is_some()
                && st.iter().all(|s| {
                    s.is_err()
                        || s.as_ref().is_ok_and(|s| {
                            matches!(s.state, State::Done | State::Failed | State::Idle)
                        })
                }))
        {
            break;
        }
    }
    if let Some(f) = failure {
        anyhow::bail!(f);
    }

    // 4. Collect.
    let mut results = Vec::with_capacity(n);
    for c in conns {
        let r = loop {
            match c.result().await? {
                Some(r) => break r,
                None => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        };
        results.push(r);
    }
    let ended_at = SystemTime::now();
    let run = merge_results(&p, &results, started_at, ended_at);
    let breach = p.guard.as_ref().and_then(|g| g.breach()).or_else(|| {
        results.iter().find_map(|r| {
            r.breach.as_ref().map(|b| crate::guard::BudgetBreach {
                budget: b.clone(),
                limit: 0.0,
                observed: 0.0,
            })
        })
    });
    Ok(Coordinated {
        run,
        breach,
        cancelled: p.cancel.is_cancelled() || results.iter().any(|r| r.cancelled),
    })
}

fn merge_results(
    p: &Plan<'_>,
    results: &[WorkerResult],
    started_at: SystemTime,
    ended_at: SystemTime,
) -> Run {
    let conns = p.conns;
    // Window starts on the coordinator's clock, relative to the earliest.
    let starts: Vec<i64> = results
        .iter()
        .zip(conns)
        .map(|(r, c)| r.window_start_unix_us - c.offset_us)
        .collect();
    let first = starts.iter().copied().min().unwrap_or(0);
    let snapshot = merge::merge_snapshots(
        &results
            .iter()
            .map(|r| r.snapshot.clone())
            .collect::<Vec<_>>(),
    );
    let trace = merge::merge_rows(
        results
            .iter()
            .zip(&starts)
            .map(|(r, s)| (r.rows.clone(), s - first))
            .collect(),
    );
    let cleanup = CleanupSummary {
        enabled: results.iter().any(|r| r.cleanup.enabled),
        prefix: p.rails.run_prefix.clone(),
        deleted: results.iter().map(|r| r.cleanup.deleted).sum(),
        remaining: results.iter().map(|r| r.cleanup.remaining).sum(),
        duration_ms: results
            .iter()
            .map(|r| r.cleanup.duration_ms)
            .fold(0.0, f64::max),
        verified: results.iter().all(|r| r.cleanup.verified),
        kept_dataset: results.iter().find_map(|r| r.cleanup.kept_dataset.clone()),
        ..results
            .first()
            .map(|r| r.cleanup.clone())
            .unwrap_or_default()
    };
    // Warnings every worker gave once; the others with their worker.
    let mut warnings: Vec<String> = Vec::new();
    for (r, c) in results.iter().zip(conns) {
        for w in &r.warnings {
            let everyone = results.iter().all(|o| o.warnings.contains(w));
            let w = if everyone {
                w.clone()
            } else {
                format!("{}: {w}", c.id)
            };
            if !warnings.contains(&w) {
                warnings.push(w);
            }
        }
    }
    // The run as a whole: the normalized target of the workers, the total concurrency.
    let mut profile = results
        .first()
        .map(|r| r.profile.clone())
        .unwrap_or_else(|| p.profile.clone());
    profile.knobs = p.profile.knobs.clone();
    profile.workload = p.profile.workload.clone();
    profile.client = p.client.clone();
    let knobs = profile.knobs.clone();
    let mut run = crate::model::build::build_run_with(
        crate::model::ReportInput {
            run_id: &p.rails.run_id,
            target_name: p.target_name.clone(),
            profile: &profile,
            knobs: &knobs,
            snapshot,
            trace,
            cleanup,
            warnings,
            started_at,
            ended_at,
            spec: Some(crate::engine::measure::spec_of(
                p.profile,
                p.target_name.as_deref(),
            )),
        },
        None,
    );
    let r = &mut run.report;
    r.client = results
        .iter()
        .find_map(|x| x.client.clone())
        .unwrap_or_else(|| p.client.info());
    crate::clients::suppress_unsupported(r);
    // Per worker: summary, clock, skew.
    let total_requests: u64 = results
        .iter()
        .map(|x| x.snapshot.apis.iter().map(|a| a.count).sum::<u64>())
        .sum();
    r.workers = results
        .iter()
        .zip(conns)
        .zip(&starts)
        .map(|((x, c), s)| {
            let offset_ms = c.offset_us as f64 / 1000.0;
            let late_ms = (s - first) as f64 / 1000.0;
            let mut w = Vec::new();
            if offset_ms.abs() > OFFSET_WARN_MS {
                w.push(format!(
                    "clock differs from the coordinator's by {offset_ms:.0} ms (start time corrected)"
                ));
            }
            let unc = c.rtt_us as f64 / 2000.0;
            if unc > UNCERTAINTY_WARN_MS {
                w.push(format!("clock offset uncertain by ±{unc:.1} ms (round trip {:.1} ms)", c.rtt_us as f64 / 1000.0));
            }
            if late_ms > LATE_WARN_MS {
                w.push(format!("started its window {late_ms:.0} ms after the first worker"));
            }
            WorkerInfo {
                id: c.id.clone(),
                addr: c.addr.clone(),
                clock_offset_ms: offset_ms,
                ops_count: x.snapshot.summary.ops,
                requests_count: x.snapshot.apis.iter().map(|a| a.count).sum(),
                ops_per_s: x.snapshot.summary.ops_per_s,
                warnings: w,
            }
        })
        .collect();
    for w in &r.workers {
        for m in &w.warnings {
            r.warnings
                .push(format!("worker {} ({}): {m}", w.id, w.addr));
        }
    }
    r.dimensions.workers = results
        .iter()
        .zip(conns)
        .map(|(x, c)| {
            let s = &x.snapshot;
            let req: u64 = s.apis.iter().map(|a| a.count).sum();
            let thr: u64 = s.apis.iter().map(|a| a.throttled).sum();
            DimensionSlice {
                key: c.id.clone(),
                ops_count: s.summary.ops,
                requests_count: req,
                share_ratio: crate::trace::util::ratio(req as f64, total_requests as f64),
                throttled_ratio: crate::trace::util::ratio(thr as f64, req as f64),
                error_ratio: s.summary.error_rate,
                p99_ms: s.summary.latency.p99,
            }
        })
        .collect();
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_split_by_weight_and_keep_the_total() {
        assert_eq!(shares(10, &[1.0, 1.0, 1.0], 1), vec![4, 3, 3]);
        assert_eq!(shares(10, &[1.0, 1.0, 1.0], 1).iter().sum::<u64>(), 10);
        assert_eq!(shares(6, &[2.0, 1.0], 1), vec![4, 2]);
        // Every worker gets at least one.
        assert_eq!(shares(2, &[1.0, 1.0, 1.0], 1), vec![1, 1, 1]);
    }

    #[test]
    fn profiles_and_budgets_split() {
        let mut p = Profile::from_target(crate::config::TargetSpec::from_url("memory://").unwrap());
        p.knobs.concurrency = 8;
        p.workload.rate = Some(90.0);
        let ps = split_profile(&p, &[1.0, 2.0]);
        assert_eq!(ps.iter().map(|q| q.knobs.concurrency).sum::<usize>(), 8);
        assert!((ps[1].workload.rate.unwrap() - 60.0).abs() < 1e-9);
        assert_ne!(ps[0].workload.seed, ps[1].workload.seed);
        let total = p.workload.effective_dataset().objects;
        let split: usize = ps
            .iter()
            .map(|q| q.workload.effective_dataset().objects)
            .sum();
        assert_eq!(split, total.max(2));
        let b = split_budgets(
            &Budgets {
                max_requests_count: Some(100),
                max_duration_s: Some(5.0),
                ..Default::default()
            },
            &[1.0, 1.0],
        );
        assert_eq!(b[0].max_requests_count, Some(50));
        assert_eq!(b[1].max_duration_s, Some(5.0));
    }
}
