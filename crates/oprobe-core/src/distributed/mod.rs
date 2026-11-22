//! Distributed load (design v5 §8). Owned by stream SCALE.
//!
//! `oprobe worker --listen ADDR --token-file F` serves the worker protocol ([`protocol`],
//! [`worker`]); `oprobe run … --workers host:port,…` (or `local:N`, [`local`]) makes this
//! process the coordinator ([`coordinator`]): it checks the workers (version, clock offset,
//! capabilities), gives each a share of the run (concurrency, rate, dataset, budgets) under its
//! own prefix, starts them at one instant, enforces the global budgets and fans out
//! cancellation, then merges their snapshots and trace rows ([`merge`]) into one run with
//! `report.workers[]` and a per-worker dimension.

pub mod coordinator;
pub mod local;
pub mod merge;
pub mod protocol;
pub mod worker;

use std::path::PathBuf;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::app::{Ctx, Resolved, RunRequest};
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError, exit};

/// One remote worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkerAddr {
    /// `host:port` of `oprobe worker --listen` (`https://host:port` for a TLS worker).
    pub addr: String,
    /// Share of the load (concurrency, rate and dataset partitions), relative to the others.
    #[serde(default = "unit")]
    pub weight: f64,
}

fn unit() -> f64 {
    1.0
}

/// The workers of a distributed run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkerSet {
    #[serde(default)]
    pub workers: Vec<WorkerAddr>,
    /// `local:N`: spawn N local worker processes instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<u32>,
    /// The shared token (`--worker-token-file`; else `[workers].token_file`, else
    /// `$OPROBE_WORKER_TOKEN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<PathBuf>,
    /// `--workers config`: the workers of the config file's `[workers]` table.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_config: bool,
}

/// `[workers]` in the config file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkersConfig {
    /// `host:port[*WEIGHT]` of each `oprobe worker` (`https://host:port` for TLS workers).
    #[serde(default)]
    pub addrs: Vec<String>,
    /// The token file the workers were started with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<PathBuf>,
}

/// Most local workers one coordinator starts.
pub const MAX_LOCAL_WORKERS: u32 = 64;

impl WorkerSet {
    /// Parse `--workers`: `local:N`, `config` (the config file's `[workers]`) or
    /// `host:port[*WEIGHT][,host:port…]`.
    pub fn parse(v: &str) -> Result<Self, OprobeError> {
        let v = v.trim();
        let bad = |m: String| {
            OprobeError::new(ErrorCode::UsageInvalidArgument, m)
                .with_hint("use host:port,host:port, local:N or config")
        };
        if v == "config" {
            return Ok(Self {
                from_config: true,
                ..Self::default()
            });
        }
        if let Some(n) = v.strip_prefix("local:") {
            let n: u32 = n
                .trim()
                .parse()
                .ok()
                .filter(|n| (1..=MAX_LOCAL_WORKERS).contains(n))
                .ok_or_else(|| {
                    bad(format!(
                        "invalid worker count in `{v}` (1 to {MAX_LOCAL_WORKERS})"
                    ))
                })?;
            return Ok(Self {
                local: Some(n),
                ..Self::default()
            });
        }
        let workers: Vec<WorkerAddr> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|a| {
                let (a, weight) = match a.rsplit_once('*') {
                    Some((a, w)) => (
                        a,
                        w.parse::<f64>()
                            .ok()
                            .filter(|w| w.is_finite() && *w > 0.0)
                            .ok_or_else(|| bad(format!("invalid worker weight in `{a}*{w}`")))?,
                    ),
                    None => (a, 1.0),
                };
                let hostport = a.strip_prefix("https://").unwrap_or(a);
                if hostport
                    .rsplit_once(':')
                    .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok())
                {
                    Ok(WorkerAddr {
                        addr: a.to_owned(),
                        weight,
                    })
                } else {
                    Err(bad(format!("invalid worker address `{a}`")))
                }
            })
            .collect::<Result<_, _>>()?;
        if workers.is_empty() {
            return Err(bad("no workers given".into()));
        }
        Ok(Self {
            workers,
            ..Self::default()
        })
    }

    /// Check a worker set before a run.
    pub fn check_supported(&self) -> Result<(), OprobeError> {
        if self.local.is_none() && self.workers.is_empty() && !self.from_config {
            return Err(
                OprobeError::new(ErrorCode::UsageInvalidArgument, "no workers given")
                    .with_hint("use host:port,host:port or local:N"),
            );
        }
        Ok(())
    }

    /// Fill a `--workers config` set (and a missing token file) from `[workers]`.
    pub fn with_config(mut self, cfg: Option<&WorkersConfig>) -> Result<Self> {
        if self.from_config {
            let cfg = cfg.filter(|c| !c.addrs.is_empty()).ok_or_else(|| {
                OprobeError::new(
                    ErrorCode::ConfigInvalid,
                    "--workers config needs a [workers] table with addrs in the config file",
                )
                .with_hint("add [workers] addrs = [\"host:port\", …] to the config file (`oprobe config edit`)")
            })?;
            self.workers = Self::parse(&cfg.addrs.join(","))?.workers;
            self.from_config = false;
        }
        if self.token_file.is_none() && self.local.is_none() {
            self.token_file = cfg.and_then(|c| c.token_file.clone());
        }
        Ok(self)
    }

    /// The token remote workers expect.
    fn token(&self) -> Result<String> {
        let t = match &self.token_file {
            Some(f) => std::fs::read_to_string(f)
                .with_context(|| format!("cannot read the worker token file `{}`", f.display()))?,
            None => std::env::var("OPROBE_WORKER_TOKEN").map_err(|_| {
                OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    "remote workers need their token",
                )
                .with_hint("pass --worker-token-file FILE (the file the workers were started with) or set OPROBE_WORKER_TOKEN")
            })?,
        };
        let t = t.trim().to_owned();
        anyhow::ensure!(
            !t.is_empty(),
            OprobeError::new(ErrorCode::UsageInvalidArgument, "the worker token is empty")
        );
        Ok(t)
    }
}

/// `oprobe run --workers …`: coordinate the run, save it, apply `--fail-on` and the report
/// files, like a local run.
pub async fn run(
    ctx: &Ctx,
    r: Resolved,
    req: &RunRequest,
    set: &WorkerSet,
    cancel: Cancel,
) -> Result<crate::app::run::RunOutput> {
    if req.repeat > 1 || req.baseline.is_some() {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            "--repeat and --baseline do not combine with --workers",
        )
        .with_hint("run the distributed run once per repetition")
        .into());
    }
    let set = &set.clone().with_config(ctx.config()?.workers.as_ref())?;
    let profile = r.profile;
    profile.validate()?;
    let rails = crate::app::run::rails(ctx, &profile, &req.safety)?;
    let guard = crate::app::run::guard_for(ctx, &profile, &rails.budgets);
    if let Some(g) = &guard {
        g.arm(cancel.clone());
    }
    for w in crate::guard::workload_warnings(&profile) {
        ctx.out.warn(w);
    }

    // The workers: local processes, or the given addresses.
    let (local, addrs, token) = match set.local {
        Some(n) => {
            let lw = local::spawn(n).await?;
            let addrs = lw
                .addrs
                .iter()
                .map(|a| WorkerAddr {
                    addr: a.clone(),
                    weight: 1.0,
                })
                .collect();
            let token = lw.token.clone();
            (Some(lw), addrs, token)
        }
        None => (None, set.workers.clone(), set.token()?),
    };
    let conns = futures::future::join_all(
        addrs
            .iter()
            .enumerate()
            .map(|(i, a)| coordinator::Conn::connect(i, a, &token)),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    let n = conns.len();
    ctx.out.info(format!(
        "oprobe {} · run {} · {n} workers ({})",
        profile.target.display(),
        rails.run_id,
        conns
            .iter()
            .map(|c| format!("{} {:+.1} ms", c.addr, c.offset_us as f64 / 1000.0))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let total_s = profile.workload.duration.get().as_secs_f64();
    let (progress, line) = crate::app::run::progress_for(ctx, total_s);
    let out = ctx.out.clone();
    let info = move |m: String| out.info(m);
    let res = coordinator::coordinate(coordinator::Plan {
        conns: &conns,
        profile: &profile,
        target_name: r.target_name.clone(),
        client: &req.client,
        rails: &rails,
        guard: guard.clone(),
        trace_max_requests: crate::app::profile::trace_max_requests(ctx),
        cancel: &cancel,
        progress: Some(progress),
        info: &info,
    })
    .await;
    if let Some(l) = &line {
        l.finish();
    }
    drop(local);
    let mut c = res?;
    if c.breach.is_some() {
        c.run.report.verdict.exit_code = exit::BUDGET;
        crate::model::build::mark_partial(&mut c.run, crate::model::PartialReason::Budget);
    } else if c.cancelled && c.run.report.run.partial.is_none() {
        crate::model::build::mark_partial(&mut c.run, crate::model::PartialReason::Interrupted);
    }
    if let Some(level) = req.output.fail_on
        && crate::app::run::fail_on_gate(&mut c.run.report, level)
    {
        ctx.out.warn(format!(
            "`--fail-on`: the diagnosis has {} findings (exit {})",
            c.run.report.worst_severity().label(),
            exit::GATE
        ));
    }
    let run = c.run;
    let path = if req.output.no_save {
        None
    } else {
        Some(ctx.store().save(&run).map_err(|e| {
            e.context("cannot save the run (set OPROBE_DATA_DIR or pass --no-save)")
        })?)
    };
    crate::app::run::save_outputs(&run.report, &req.output)?;
    if let Some(b) = &c.breach {
        if path.is_some() {
            ctx.out.warn(format!(
                "the partial run was saved: oprobe runs show {}",
                run.id()
            ));
        }
        return Err(b.to_error());
    }
    if c.cancelled {
        ctx.out
            .warn("interrupted — partial results: they cover only the time before Ctrl-C");
    }
    Ok(crate::app::run::RunOutput {
        run_id: run.id().to_owned(),
        path: path.map(|p| p.display().to_string()),
        report: run.report.clone(),
        full: Some(Box::new(run)),
    })
}

#[doc(hidden)]
pub use coordinator::Conn;

/// What `oprobe worker` serves (printed once it listens).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct WorkerOutput {
    /// The bound `host:port` (with `--listen …:0`, the port chosen).
    pub listening: String,
    /// `oprobe.worker/1`.
    pub protocol: String,
    pub version: String,
    pub tls: bool,
    /// Clients coordinators may use here (`exec` only with `--allow-exec`).
    pub clients: Vec<String>,
}

impl crate::output::CommandOutput for WorkerOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        let scheme = if self.tls { "https://" } else { "" };
        vec![crate::output::NextStep::new(
            "run load from it",
            format!(
                "oprobe run TARGET --workers {scheme}{} --worker-token-file TOKEN_FILE",
                self.listening
            ),
        )]
    }

    fn text(&self) -> String {
        format!(
            "{} oprobe worker listening on {} ({}{}; clients: {})\n",
            crate::report::style::green("●"),
            self.listening,
            self.protocol,
            if self.tls { ", TLS" } else { ", h2c" },
            self.clients.join(", ")
        )
    }
}

/// Serve a worker in this process on `listener` (tests and `oprobe worker`).
pub async fn serve_worker(
    listener: tokio::net::TcpListener,
    cfg: worker::WorkerConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    worker::serve(listener, cfg, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, EventSink, RunOutcome};
    use crate::lab::{LabConfig, LabServer};

    struct Workers {
        addrs: Vec<String>,
        token_file: PathBuf,
        stops: Vec<tokio::sync::oneshot::Sender<()>>,
    }

    /// `n` workers served in this process (what `oprobe worker` runs).
    async fn workers(n: usize, dir: &std::path::Path) -> Workers {
        let token = local::random_token();
        let token_file = dir.join("token");
        std::fs::write(&token_file, &token).unwrap();
        let mut w = Workers {
            addrs: Vec::new(),
            token_file,
            stops: Vec::new(),
        };
        for _ in 0..n {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            w.addrs.push(l.local_addr().unwrap().to_string());
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            w.stops.push(tx);
            let cfg = worker::WorkerConfig {
                token: token.clone(),
                allow_exec: false,
                tls: None,
                paths: crate::store::Paths {
                    config_file: dir.join("worker.toml"),
                    data_dir: dir.join("worker-data"),
                },
            };
            tokio::spawn(serve_worker(l, cfg, async {
                let _ = rx.await;
            }));
        }
        w
    }

    fn request(lab: &crate::lab::LabHandle, w: &Workers, secs: f64) -> RunRequest {
        let mut p = lab.profile("bkt");
        p.workload.duration = crate::util::HumanDuration(std::time::Duration::from_secs_f64(secs));
        p.workload.warmup = crate::util::HumanDuration(std::time::Duration::ZERO);
        p.knobs.concurrency = 6;
        let mut req = RunRequest::new(Resolved {
            profile: p,
            target_name: None,
        });
        req.workers = Some(WorkerSet {
            workers: w
                .addrs
                .iter()
                .map(|a| WorkerAddr {
                    addr: a.clone(),
                    weight: 1.0,
                })
                .collect(),
            token_file: Some(w.token_file.clone()),
            local: None,
            from_config: false,
        });
        req
    }

    async fn states(w: &Workers) -> Vec<protocol::State> {
        let token = std::fs::read_to_string(&w.token_file).unwrap();
        let mut out = Vec::new();
        for (i, a) in w.addrs.iter().enumerate() {
            let c = coordinator::Conn::connect(
                i,
                &WorkerAddr {
                    addr: a.clone(),
                    weight: 1.0,
                },
                &token,
            )
            .await
            .unwrap();
            out.push(c.hello.busy);
        }
        out.into_iter()
            .map(|busy| {
                if busy {
                    protocol::State::Running
                } else {
                    protocol::State::Done
                }
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn three_workers_merge_into_one_run() {
        let dir = tempfile::tempdir().unwrap();
        let lab = LabServer::spawn("127.0.0.1:0", LabConfig::default())
            .await
            .unwrap();
        let w = workers(3, dir.path()).await;
        let app = App::new(crate::store::Paths {
            config_file: dir.path().join("config.toml"),
            data_dir: dir.path().join("data"),
        });
        let out = match app
            .run(request(&lab, &w, 1.5), &EventSink::silent(), Cancel::new())
            .await
            .unwrap()
        {
            RunOutcome::Run(o) => o,
            _ => panic!("one run"),
        };
        let r = &out.report;
        assert_eq!(r.workers.len(), 3);
        // The merged counts are the sum of the workers'.
        let ops: u64 = r.workers.iter().map(|w| w.ops_count).sum();
        let reqs: u64 = r.workers.iter().map(|w| w.requests_count).sum();
        assert!(ops > 0);
        assert_eq!(r.summary.ops, ops);
        assert_eq!(r.apis.iter().map(|a| a.count).sum::<u64>(), reqs);
        assert!(r.workers.iter().all(|w| w.ops_count > 0), "{:?}", r.workers);
        assert_eq!(r.dimensions.workers.len(), 3);
        let share: f64 = r.dimensions.workers.iter().map(|d| d.share_ratio).sum();
        assert!((share - 1.0).abs() < 1e-9);
        // Every worker's rows, with unique operation ids, reach the run.
        let full = out.full.as_ref().unwrap();
        let mut ids: Vec<u64> = full.ops.iter().map(|o| o.op_id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n);
        let per_worker: std::collections::BTreeSet<_> = full
            .ops
            .iter()
            .filter_map(|o| merge::worker_of(o.op_id))
            .collect();
        assert_eq!(per_worker.len(), 3);
        // The capacity model is fitted on the merged rows: 6 slots over 3 workers of 2, so
        // more than one worker's 2 requests were in flight at once.
        assert!(
            r.capacity.observed.peak_inflight_count > 2,
            "{:?}",
            r.capacity.observed
        );
        // Each worker cleaned up its own prefix: nothing is left.
        assert!(
            lab.stored_keys().iter().all(|k| !k.contains(&r.run.id)),
            "{:?}",
            lab.stored_keys()
        );
        assert!(out.path.is_some());
        drop(w);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_budget_breach_and_a_cancel_stop_every_worker() {
        let dir = tempfile::tempdir().unwrap();
        let lab = LabServer::spawn("127.0.0.1:0", LabConfig::default())
            .await
            .unwrap();
        let w = workers(3, dir.path()).await;
        let app = App::new(crate::store::Paths {
            config_file: dir.path().join("config.toml"),
            data_dir: dir.path().join("data"),
        });
        // A global request budget far below what 3 workers send in 30 s.
        let mut req = request(&lab, &w, 30.0);
        req.safety.max_requests = Some(400);
        let t0 = std::time::Instant::now();
        let e = app
            .run(req, &EventSink::silent(), Cancel::new())
            .await
            .expect_err("budget breach");
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::SafetyBudgetExceeded
        );
        assert!(t0.elapsed() < std::time::Duration::from_secs(20));
        assert!(states(&w).await.iter().all(|s| *s == protocol::State::Done));

        // Cancelling the coordinator cancels every worker; the partial run is kept.
        let cancel = Cancel::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
            c2.cancel();
        });
        let t0 = std::time::Instant::now();
        let out = match app
            .run(request(&lab, &w, 30.0), &EventSink::silent(), cancel)
            .await
            .unwrap()
        {
            RunOutcome::Run(o) => o,
            _ => panic!("one run"),
        };
        assert!(t0.elapsed() < std::time::Duration::from_secs(20));
        assert!(out.report.run.interrupted);
        assert!(out.report.summary.ops > 0);
        assert!(states(&w).await.iter().all(|s| *s == protocol::State::Done));
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let w = workers(1, dir.path()).await;
        let e = coordinator::Conn::connect(
            0,
            &WorkerAddr {
                addr: w.addrs[0].clone(),
                weight: 1.0,
            },
            "wrong",
        )
        .await
        .expect_err("refused");
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::AuthDenied
        );
    }

    #[test]
    fn parses_worker_sets() {
        assert_eq!(WorkerSet::parse("local:3").unwrap().local, Some(3));
        let w = WorkerSet::parse("a:1, b:2*2.5, https://c:3").unwrap();
        assert_eq!(w.workers.len(), 3);
        assert_eq!(w.workers[1].weight, 2.5);
        assert_eq!(w.workers[2].addr, "https://c:3");
        assert!(WorkerSet::parse("nope").is_err());
        assert!(WorkerSet::parse("local:0").is_err());
        assert!(WorkerSet::parse("a:1*0").is_err());
        assert!(w.check_supported().is_ok());
        assert!(WorkerSet::default().check_supported().is_err());
        // `config`: the [workers] table, its token file unless one was given.
        let c = WorkersConfig {
            addrs: vec!["h1:7070".into(), "h2:7070*3".into()],
            token_file: Some("/t".into()),
        };
        let set = WorkerSet::parse("config")
            .unwrap()
            .with_config(Some(&c))
            .unwrap();
        assert_eq!(set.workers.len(), 2);
        assert_eq!(set.workers[1].weight, 3.0);
        assert_eq!(set.token_file.as_deref(), Some(std::path::Path::new("/t")));
        assert!(
            WorkerSet::parse("config")
                .unwrap()
                .with_config(None)
                .is_err()
        );
    }
}
