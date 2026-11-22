//! `oprobe worker`: serves the worker protocol ([`super::protocol`]) for a coordinator
//! (design v5 §8).
//!
//! One job at a time: `prepare` opens the session (preflight, the worker's dataset share under
//! its own prefix), `start` runs the window at the coordinator's instant, the result (snapshot
//! and trace rows) waits for `result`. Every request needs the token; a coordinator can make a
//! worker run an `exec:` command only when the worker was started with `--allow-exec`.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoServerBuilder;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::protocol::*;
use crate::clients::ClientChoice;
use crate::engine::measure::Rails;
use crate::engine::{Cancel, Progress, ProgressFn, RunOptions};
use crate::errors::{ErrorCode, OprobeError};
use crate::store::Paths;

/// How a worker serves.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// The shared secret every request must carry.
    pub token: String,
    /// Let coordinators run `--client exec:CMD` here.
    pub allow_exec: bool,
    /// PEM certificate and key: serve TLS (h2 by ALPN) instead of h2c.
    pub tls: Option<(String, String)>,
    /// Where credentials of a redacted spec are resolved (config file, data dir).
    pub paths: Paths,
}

/// The current job.
#[derive(Default)]
struct Slot {
    state: State,
    cancel: Cancel,
    start_tx: Option<oneshot::Sender<i64>>,
    started: Option<Instant>,
    snapshot: Option<Box<crate::trace::Snapshot>>,
    guard: Option<Arc<crate::guard::Guard>>,
    result: Option<Bytes>,
    error: Option<String>,
}

struct Shared {
    cfg: WorkerConfig,
    slot: Mutex<Slot>,
}

impl Shared {
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Clients this worker can drive.
pub fn clients(allow_exec: bool) -> Vec<String> {
    let mut c = vec!["opendal".to_owned()];
    if cfg!(feature = "aws-sdk") {
        c.push("aws_sdk".into());
    }
    if allow_exec {
        c.push("exec".into());
    }
    c
}

/// Constant-time token comparison.
fn token_ok(given: &str, want: &str) -> bool {
    let (a, b) = (given.as_bytes(), want.as_bytes());
    if a.len() != b.len() || b.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn json<T: serde::Serialize>(status: StatusCode, v: &T) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(v).unwrap_or_default(),
        )))
        .unwrap_or_default()
}

fn ack(status: StatusCode, error: Option<String>) -> Response<Full<Bytes>> {
    json(
        status,
        &Ack {
            ok: error.is_none(),
            error,
        },
    )
}

/// Serve on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    cfg: WorkerConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    anyhow::ensure!(
        !cfg.token.trim().is_empty(),
        OprobeError::new(ErrorCode::UsageInvalidArgument, "the worker token is empty")
            .with_hint("write a long random secret to the token file")
    );
    let acceptor = match &cfg.tls {
        Some((cert, key)) => Some(crate::net::tls::build_tls_acceptor(cert, key, true)?),
        None => None,
    };
    let shared = Arc::new(Shared {
        cfg,
        slot: Mutex::new(Slot::default()),
    });
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((tcp, _)) = accepted else { continue };
                let _ = tcp.set_nodelay(true);
                let sh = shared.clone();
                let acc = acceptor.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let sh = sh.clone();
                        async move { Ok::<_, Infallible>(handle(sh, req).await) }
                    });
                    let builder = AutoServerBuilder::new(TokioExecutor::new());
                    match acc {
                        Some(acc) => {
                            if let Ok(tls) = acc.accept(tcp).await {
                                let _ = builder.serve_connection(TokioIo::new(tls), svc).await;
                            }
                        }
                        None => {
                            let _ = builder.serve_connection(TokioIo::new(tcp), svc).await;
                        }
                    }
                });
            }
        }
    }
    // Stop a job still running (its own cleanup runs as the window ends).
    shared.slot().cancel.cancel();
    Ok(())
}

async fn handle(sh: Arc<Shared>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let auth = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    if !token_ok(&auth, &sh.cfg.token) {
        return ack(
            StatusCode::UNAUTHORIZED,
            Some("invalid or missing token".into()),
        );
    }
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return ack(StatusCode::BAD_REQUEST, Some(e.to_string())),
    };
    match (method, path.as_str()) {
        (Method::GET, "/v1/hello") => json(
            StatusCode::OK,
            &Hello {
                protocol: PROTOCOL.into(),
                version: env!("CARGO_PKG_VERSION").into(),
                clock_unix_us: now_unix_us(),
                cpu_cores_count: std::thread::available_parallelism().map_or(1, |n| n.get()),
                clients: clients(sh.cfg.allow_exec),
                busy: matches!(
                    sh.slot().state,
                    State::Preparing | State::Ready | State::Running
                ),
            },
        ),
        (Method::POST, "/v1/prepare") => {
            let job: Job = match serde_json::from_slice(&body) {
                Ok(j) => j,
                Err(e) => return ack(StatusCode::BAD_REQUEST, Some(format!("invalid job: {e}"))),
            };
            if matches!(job.client, ClientChoice::Exec { .. }) && !sh.cfg.allow_exec {
                return ack(
                    StatusCode::FORBIDDEN,
                    Some(
                        "this worker does not run exec clients (start it with --allow-exec)".into(),
                    ),
                );
            }
            if let Err(e) = job.client.check_supported() {
                return ack(StatusCode::BAD_REQUEST, Some(e.to_string()));
            }
            let (start_tx, start_rx) = oneshot::channel();
            let cancel = Cancel::new();
            {
                let mut s = sh.slot();
                if matches!(s.state, State::Preparing | State::Ready | State::Running) {
                    return ack(StatusCode::CONFLICT, Some("the worker is busy".into()));
                }
                *s = Slot {
                    state: State::Preparing,
                    cancel: cancel.clone(),
                    start_tx: Some(start_tx),
                    ..Slot::default()
                };
            }
            tokio::spawn(job_task(sh.clone(), job, start_rx, cancel));
            ack(StatusCode::OK, None)
        }
        (Method::POST, "/v1/start") => {
            let start: Start = match serde_json::from_slice(&body) {
                Ok(s) => s,
                Err(e) => return ack(StatusCode::BAD_REQUEST, Some(format!("invalid start: {e}"))),
            };
            let tx = {
                let mut s = sh.slot();
                if s.state != State::Ready {
                    return ack(
                        StatusCode::CONFLICT,
                        Some(format!("the worker is {:?}, not ready", s.state).to_lowercase()),
                    );
                }
                s.start_tx.take()
            };
            match tx.map(|t| t.send(start.at_unix_us)) {
                Some(Ok(())) => ack(StatusCode::OK, None),
                _ => ack(StatusCode::CONFLICT, Some("the job is gone".into())),
            }
        }
        (Method::GET, "/v1/status") => {
            let s = sh.slot();
            json(StatusCode::OK, &status_of(&s))
        }
        (Method::POST, "/v1/cancel") => {
            let mut s = sh.slot();
            s.cancel.cancel();
            // A job waiting for `start` stops at once.
            s.start_tx.take();
            ack(StatusCode::OK, None)
        }
        (Method::GET, "/v1/result") => {
            let s = sh.slot();
            match (s.state, &s.result) {
                (State::Done, Some(b)) => Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/zstd")
                    .body(Full::new(b.clone()))
                    .unwrap_or_default(),
                (State::Failed, _) => ack(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Some(s.error.clone().unwrap_or_else(|| "the job failed".into())),
                ),
                (State::Idle, _) => ack(StatusCode::NOT_FOUND, Some("no job".into())),
                _ => ack(StatusCode::ACCEPTED, Some("not done".into())),
            }
        }
        _ => ack(
            StatusCode::NOT_FOUND,
            Some(format!("unknown request {path}")),
        ),
    }
}

fn status_of(s: &Slot) -> Status {
    let snap = s.snapshot.as_deref();
    let (requests, bytes) = match (&s.guard, snap) {
        // A guard accounts every request and byte exactly (what budgets count).
        (Some(g), _) => g.usage(),
        (None, Some(sn)) => (
            sn.apis.iter().map(|a| a.count).sum(),
            sn.bytes.wire_rx + sn.bytes.wire_tx,
        ),
        (None, None) => (0, 0),
    };
    Status {
        state: s.state,
        elapsed_s: s.started.map_or(0.0, |t| t.elapsed().as_secs_f64()),
        ops_count: snap.map_or(0, |x| x.summary.ops),
        failed_ops_count: snap.map_or(0, |x| x.summary.failed_ops),
        requests_count: requests,
        bytes,
        ops_per_s: snap.map_or(0.0, |x| x.summary.ops_per_s),
        snapshot: s.snapshot.clone(),
        error: s.error.clone(),
    }
}

async fn job_task(sh: Arc<Shared>, job: Job, start_rx: oneshot::Receiver<i64>, cancel: Cancel) {
    let res = run_job(&sh, job, start_rx, cancel).await;
    let mut s = sh.slot();
    match res.and_then(|r| encode(&r)) {
        Ok(b) => {
            s.result = Some(b);
            s.state = State::Done;
        }
        Err(e) => {
            s.error = Some(format!("{e:#}"));
            s.state = State::Failed;
        }
    }
}

/// zstd-compressed JSON (results hold up to a million trace rows).
pub fn encode(r: &WorkerResult) -> Result<Bytes> {
    let json = serde_json::to_vec(r)?;
    Ok(Bytes::from(zstd::encode_all(&json[..], 3)?))
}

pub fn decode(b: &[u8]) -> Result<WorkerResult> {
    let json = zstd::decode_all(b).context("cannot decompress the worker's result")?;
    serde_json::from_slice(&json).context("invalid worker result")
}

async fn run_job(
    sh: &Arc<Shared>,
    job: Job,
    start_rx: oneshot::Receiver<i64>,
    cancel: Cancel,
) -> Result<WorkerResult> {
    let mut profile = match (job.profile, job.spec) {
        (Some(p), _) => p,
        (None, Some(spec)) => {
            // Credentials come from this machine, as for `oprobe run --from`.
            let app = crate::app::App::new(sh.cfg.paths.clone());
            app.profile_of(&crate::app::RunRequest::from_spec(spec))?
                .profile
        }
        (None, None) => anyhow::bail!("the job has neither a profile nor a spec"),
    };
    profile.client = job.client.clone();
    let mut rails = Rails::new(&profile, job.read_only, None, job.budgets.clone())?;
    rails.run_prefix = job.run_prefix.clone();
    let guard = rails.guard(&profile);
    sh.slot().guard = guard.clone();
    let progress: ProgressFn = {
        let sh = sh.clone();
        Arc::new(move |p| {
            if let Progress::Snapshot(s) = p {
                sh.slot().snapshot = Some(s);
            }
        })
    };
    let mut session = crate::engine::measure::open(
        profile,
        &rails,
        guard.clone(),
        job.trace_max_requests_count,
        cancel.clone(),
        progress.clone(),
    )
    .await?;
    // A replayed log is split by key: each worker replays its own keys.
    session.shard_replay(&job.run_prefix, job.index, job.count);
    // An existing dataset is listed by every worker: each reads its own key range.
    if session.dataset.read_only && job.count > 1 {
        let d = &session.dataset;
        let mut objects = d.objects.clone();
        objects.sort_by(|a, b| a.path.cmp(&b.path));
        let n = objects.len();
        let (i, c) = (job.index as usize, job.count as usize);
        let part: Vec<_> = objects[n * i / c..n * (i + 1) / c].to_vec();
        if !part.is_empty() {
            session.dataset = Arc::new(crate::engine::dataset::Dataset {
                prefix: d.prefix.clone(),
                objects: part,
                partitions: d.partitions.clone(),
                preexisting: d.preexisting,
                read_only: d.read_only,
            });
        }
    }
    sh.slot().state = State::Ready;
    // Wait for the common instant (or a cancel, or the coordinator going away).
    let at = tokio::select! {
        r = start_rx => r.ok(),
        _ = cancel.cancelled() => None,
        _ = tokio::time::sleep(Duration::from_secs(600)) => None,
    };
    let Some(at) = at else {
        let _ = session.close().await;
        anyhow::bail!("the run was cancelled before it started");
    };
    let wait_us = at - now_unix_us();
    if wait_us > 0 {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_micros(wait_us as u64)) => {}
            _ = cancel.cancelled() => {}
        }
    }
    let run_started = now_unix_us();
    {
        let mut s = sh.slot();
        s.state = State::Running;
        s.started = Some(Instant::now());
    }
    let knobs = session.profile.knobs.clone();
    let opts = RunOptions::from_profile(&session.profile);
    let res = session.run(&knobs, &opts, Some(progress)).await;
    let (ok, bad) = if res.is_ok() && session.profile.workload.verify {
        session.verify_writes(None).await
    } else {
        (0, 0)
    };
    let cleanup = session.close().await;
    let mut snapshot = res?;
    snapshot.integrity.verified += ok;
    snapshot.integrity.corrupted += bad;
    if guard.as_ref().is_some_and(|g| g.tripped()) {
        snapshot.interrupted = true;
    }
    let trace = session.take_trace().unwrap_or_default();
    Ok(WorkerResult {
        snapshot,
        rows: Rows {
            ops: trace.ops,
            reqs: trace.reqs,
            info: trace.info,
            sampling: trace.sampling.into(),
        },
        cleanup,
        warnings: session.warnings.clone(),
        client: session.client().map(|c| c.info()),
        profile: session.profile.clone(),
        window_start_unix_us: run_started + opts.warmup.as_micros() as i64,
        start_lateness_ms: (run_started - at) as f64 / 1000.0,
        breach: guard.and_then(|g| g.breach()).map(|b| b.budget.to_string()),
        cancelled: cancel.is_cancelled(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_exactly() {
        assert!(token_ok("abc", "abc"));
        assert!(!token_ok("abd", "abc"));
        assert!(!token_ok("ab", "abc"));
        assert!(!token_ok("", ""));
    }
}
