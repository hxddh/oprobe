//! Bridge between the UI and the `oprobe` engine. Benchmarks run on a Tokio runtime of their
//! own (the engine spawns Tokio tasks and owns sockets); the UI polls a shared, `Send` state
//! and only ever reads snapshots of it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::config::Profile;
use crate::engine::measure::{Measured, Mode, Rails, Request, Stage};
use crate::engine::{Cancel, Progress, ProgressFn};
use crate::guard::Budgets;
use crate::model::Run;
use crate::net::probe::{ProbeOptions, ProbeReport, probe_endpoint};
use crate::store::RunStore;
use crate::trace::Snapshot;
use crate::tune::{Trial, TuneEvent, TuneOptions, codegen};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("oprobe-engine")
            .build()
            .expect("tokio runtime")
    })
}

/// Run blocking work (loading a run file, deriving its views) on the engine's threads.
pub async fn off_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = futures::channel::oneshot::channel();
    runtime().spawn_blocking(move || {
        let _ = tx.send(f());
    });
    rx.await.ok()
}

/// The finished result of a job.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// A measurement (or the tuner's confirmation run), saved to the run store when it has one.
    Run {
        run: Box<Run>,
        path: Option<PathBuf>,
        /// The tuner's extras (its winner as a profile and as Rust code).
        tuned: Option<Box<TunedExtras>>,
    },
    Probe(Box<ProbeReport>),
    Failed {
        error: String,
        hint: Option<(String, Vec<String>)>,
    },
}

/// What the tuner adds to its confirmation run.
#[derive(Clone, Debug, PartialEq)]
pub struct TunedExtras {
    pub knobs: crate::config::Knobs,
    pub profile_toml: String,
    pub rust: String,
}

/// What a run job measures and where its result goes.
#[derive(Clone)]
pub struct RunRequest {
    pub profile: Profile,
    pub target_name: Option<String>,
    /// Shown instead of the profile's target (`lab://demo`).
    pub display: Option<String>,
    pub label: Option<String>,
    /// Start an in-process lab with these faults and point the profile at it.
    pub lab: Option<crate::lab::LabConfig>,
    pub store: Option<RunStore>,
    pub trace_max_requests: usize,
    /// Budgets of the run (`[defaults.budgets]`, as for the CLI).
    pub budgets: Budgets,
}

#[derive(Default)]
struct State {
    snapshot: Option<Snapshot>,
    log: Vec<String>,
    trials: Vec<Trial>,
    outcome: Option<Outcome>,
}

/// What changed since the last [`Job::take`].
#[derive(Default)]
pub struct Update {
    pub snapshot: Option<Snapshot>,
    pub log: Vec<String>,
    pub trials: Option<Vec<Trial>>,
    pub outcome: Option<Outcome>,
}

/// Set once the job's task has ended (after its cleanup), however it ended.
#[derive(Default)]
struct Done {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Done {
    fn set(&self) {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }
    fn is_set(&self) -> bool {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Wait until set or `deadline`; whether it was set.
    fn wait_until(&self, deadline: Instant) -> bool {
        let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        while !*g {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = self
                .cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

/// Every job that has not ended yet, so quitting the app can stop them and wait for their
/// cleanup (the engine's runtime dies with the process, abandoning test data otherwise).
static LIVE: Mutex<Vec<(Cancel, Arc<Done>)>> = Mutex::new(Vec::new());
/// The app is quitting: jobs were asked to stop.
static QUITTING: AtomicBool = AtomicBool::new(false);
/// The user insisted (a second Ctrl+C): don't wait for cleanup.
static ABANDONED: AtomicBool = AtomicBool::new(false);

fn live() -> std::sync::MutexGuard<'static, Vec<(Cancel, Arc<Done>)>> {
    let mut l = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    l.retain(|(_, d)| !d.is_set());
    l
}

/// Whether any job is still running (including its cleanup).
pub fn busy() -> bool {
    !live().is_empty()
}

/// Whether the app asked the running jobs to stop because it is quitting.
pub fn quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}

/// Ask every running job to stop (each still cleans up its test data).
pub fn begin_shutdown() {
    QUITTING.store(true, Ordering::SeqCst);
    for (c, _) in live().iter() {
        c.cancel();
    }
}

/// Don't wait for cleanup any more (the user pressed Ctrl+C again).
pub fn abandon() {
    ABANDONED.store(true, Ordering::SeqCst);
}

/// Whether [`abandon`] was called.
pub fn abandoned() -> bool {
    ABANDONED.load(Ordering::SeqCst)
}

/// Stop every running job and block until each has cleaned up, `timeout` at most.
/// Returns whether all of them finished. Quick when nothing runs.
pub fn shutdown(timeout: Duration) -> bool {
    if !busy() {
        return true;
    }
    begin_shutdown();
    if ABANDONED.load(Ordering::SeqCst) {
        return false;
    }
    let deadline = Instant::now() + timeout;
    let pending: Vec<Arc<Done>> = live().iter().map(|(_, d)| d.clone()).collect();
    pending.iter().all(|d| d.wait_until(deadline))
}

/// How long quitting waits for a stopped job's cleanup.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// A running benchmark, tuning session or probe.
pub struct Job {
    state: Arc<Mutex<State>>,
    cancel: Cancel,
    done: Arc<Done>,
}

fn failed(err: &anyhow::Error, scheme: &str) -> Outcome {
    let chain = format!("{err:#}");
    let hint = crate::analysis::hints::hint_for(&chain, scheme).map(|h| (h.title, h.steps));
    Outcome::Failed { error: chain, hint }
}

/// Describe a job task that did not return (a panic, or the runtime shutting down).
fn join_error(e: tokio::task::JoinError) -> String {
    if !e.is_panic() {
        return "the job was aborted".into();
    }
    let p = e.into_panic();
    let msg = p
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into());
    format!(
        "internal error: the engine panicked: {msg}\n(test data may be left behind; remove it with `oprobe cleanup`)"
    )
}

/// One app measurement through [`crate::engine::measure`]: the rails and budgets of the CLI
/// (`[defaults.budgets]` from the config file). A budget breach keeps the saved partial run
/// and fails the job with the breach.
#[allow(clippy::too_many_arguments)]
async fn measure_in_app(
    profile: Profile,
    target_name: Option<String>,
    budgets: &Budgets,
    trace_max_requests: usize,
    store: Option<RunStore>,
    cancel: Cancel,
    progress: ProgressFn,
    mode: Mode,
    finalize: crate::engine::measure::FinalizeFn,
) -> anyhow::Result<Measured> {
    let rails = Rails::new(&profile, false, None, budgets.clone())?;
    let guard = rails.guard(&profile);
    let p2 = progress.clone();
    let p3 = progress.clone();
    let mut req = Request::new(profile, rails, cancel, progress);
    req.target_name = target_name;
    req.guard = guard;
    req.trace_max_requests = trace_max_requests;
    req.store = store;
    req.mode = mode;
    req.finalize = Some(finalize);
    req.on_stage = Some(Arc::new(move |s| {
        if s == Stage::CleaningUp {
            p2(Progress::Message("cleaning up test data…".into()));
        }
    }));
    let m = crate::engine::measure::measure(req).await?;
    for w in &m.warnings {
        p3(Progress::Message(format!("warning: {w}")));
    }
    if let Some(b) = &m.breach {
        let mut e = b.to_error();
        if let Some(p) = &m.path {
            e = e.context(format!("the partial run was saved: {}", p.display()));
        }
        return Err(e);
    }
    Ok(m)
}

impl Job {
    fn spawn<F, Fut>(f: F) -> Arc<Self>
    where
        F: FnOnce(Arc<Mutex<State>>, Cancel) -> Fut,
        Fut: std::future::Future<Output = Outcome> + Send + 'static,
    {
        let state = Arc::new(Mutex::new(State::default()));
        let cancel = Cancel::new();
        let done = Arc::new(Done::default());
        live().push((cancel.clone(), done.clone()));
        if quitting() {
            cancel.cancel();
        }
        let fut = f(state.clone(), cancel.clone());
        let st = state.clone();
        let handle = runtime().spawn(fut);
        let done2 = done.clone();
        // Awaiting the handle turns an engine panic into a failure the UI shows, instead of a
        // job that runs forever and blocks every new one.
        runtime().spawn(async move {
            let outcome = match handle.await {
                Ok(o) => o,
                Err(e) => Outcome::Failed {
                    error: join_error(e),
                    hint: None,
                },
            };
            st.lock().unwrap_or_else(|e| e.into_inner()).outcome = Some(outcome);
            done2.set();
        });
        Arc::new(Self {
            state,
            cancel,
            done,
        })
    }

    fn progress(state: &Arc<Mutex<State>>) -> ProgressFn {
        let st = state.clone();
        Arc::new(move |p| {
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            match p {
                Progress::Message(m) => s.log.push(m),
                Progress::Snapshot(snap) => s.snapshot = Some(*snap),
            }
        })
    }

    /// Run one workload window, build the run (report + trace rows) and save it — through the
    /// same pipeline as `oprobe run` (rails, budgets, preflight, cleanup).
    pub fn run(req: RunRequest) -> Arc<Self> {
        Self::spawn(move |state, cancel| async move {
            let scheme = req.profile.target.scheme.clone();
            let progress = Self::progress(&state);
            let res: anyhow::Result<Outcome> = async {
                let mut profile = req.profile.clone();
                // The lab lives as long as this task: until the session is closed.
                let _lab = match req.lab.clone() {
                    Some(faults) => {
                        progress(Progress::Message("starting the lab server…".into()));
                        let handle = crate::lab::LabServer::spawn("127.0.0.1:0", faults).await?;
                        let mut p = handle.profile("demo");
                        p.workload = profile.workload.clone();
                        p.knobs = profile.knobs.clone();
                        p.sla = profile.sla.clone();
                        p.name = profile.name.clone();
                        profile = p;
                        Some(handle)
                    }
                    None => None,
                };
                if let Some(l) = &req.label {
                    profile.name = Some(l.clone());
                }
                let display = req.display.clone();
                let m = measure_in_app(
                    profile,
                    req.target_name.clone(),
                    &req.budgets,
                    req.trace_max_requests,
                    req.store.clone(),
                    cancel,
                    progress,
                    Mode::Run,
                    Box::new(move |run| {
                        if let Some(d) = display {
                            run.report.target.display = d;
                        }
                    }),
                )
                .await?;
                Ok(Outcome::Run {
                    run: Box::new(m.run),
                    path: m.path,
                    tuned: None,
                })
            }
            .await;
            res.unwrap_or_else(|e| failed(&e, &scheme))
        })
    }

    /// Run the closed-loop tuner; its confirmation run is saved like any run.
    pub fn tune(
        profile: Profile,
        target_name: Option<String>,
        opts: TuneOptions,
        budgets: Budgets,
        store: Option<RunStore>,
    ) -> Arc<Self> {
        Self::spawn(move |state, cancel| async move {
            let scheme = profile.target.scheme.clone();
            let progress = Self::progress(&state);
            let res: anyhow::Result<Outcome> = async {
                let st = state.clone();
                let on_event: Arc<dyn Fn(TuneEvent) + Send + Sync> = Arc::new(move |ev| {
                    let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
                    match ev {
                        TuneEvent::Started { id, stage, knobs } => s
                            .log
                            .push(format!("trial {id} · {stage} · {}", knobs.summary())),
                        TuneEvent::Finished(t) => s.trials.push(t),
                        TuneEvent::Message(m) => s.log.push(m),
                    }
                });
                let m = measure_in_app(
                    profile,
                    target_name,
                    &budgets,
                    crate::trace::DEFAULT_MAX_REQUESTS,
                    store,
                    cancel,
                    progress,
                    Mode::Tune { opts, on_event },
                    Box::new(|run| run.report.run.label = Some("tuned".into())),
                )
                .await?;
                let tuned = m.tuned.context("the tuner returned no result")?;
                let header = format!(
                    "objective {} · {:+.0}% vs baseline",
                    tuned.summary.objective,
                    tuned.summary.improvement * 100.0
                );
                Ok(Outcome::Run {
                    tuned: Some(Box::new(TunedExtras {
                        profile_toml: codegen::profile_toml(
                            &tuned.profile,
                            &tuned.winner,
                            &header,
                        )?,
                        rust: codegen::rust_snippet(
                            &tuned.profile.target,
                            &tuned.winner,
                            &tuned.capability,
                        ),
                        knobs: tuned.winner.clone(),
                    })),
                    run: Box::new(m.run),
                    path: m.path,
                })
            }
            .await;
            res.unwrap_or_else(|e| failed(&e, &scheme))
        })
    }

    /// Probe an endpoint's handshakes.
    pub fn probe(endpoint: String, samples: usize) -> Arc<Self> {
        Self::spawn(move |_state, _cancel| async move {
            let opts = ProbeOptions {
                samples,
                ..Default::default()
            };
            match probe_endpoint(&endpoint, &opts).await {
                Ok(r) => Outcome::Probe(Box::new(r)),
                Err(e) => failed(&e, "s3"),
            }
        })
    }

    /// Drain what changed since the last call.
    pub fn take(&self) -> Update {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Update {
            snapshot: s.snapshot.take(),
            log: std::mem::take(&mut s.log),
            trials: (!s.trials.is_empty()).then(|| s.trials.clone()),
            outcome: s.outcome.take(),
        }
    }

    /// Ask the job to stop; test data is still cleaned up.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether the job has ended (cleanup included).
    pub fn is_done(&self) -> bool {
        self.done.is_set()
    }

    /// Stop the job and block until it has cleaned up, `timeout` at most; whether it ended.
    pub fn stop_and_wait(&self, timeout: Duration) -> bool {
        self.cancel();
        self.done.wait_until(Instant::now() + timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_job_fails_instead_of_running_forever() {
        let job = Job::spawn(|_, _| async {
            panic!("boom");
        });
        assert!(job.stop_and_wait(Duration::from_secs(10)));
        match job.take().outcome {
            Some(Outcome::Failed { error, .. }) => {
                assert!(error.contains("panicked: boom"), "{error}")
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stop_and_wait_waits_for_cleanup() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let c2 = cleaned.clone();
        let job = Job::spawn(move |_, cancel| async move {
            while !cancel.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.store(true, Ordering::SeqCst);
            Outcome::Failed {
                error: "stopped".into(),
                hint: None,
            }
        });
        assert!(!job.is_done());
        assert!(job.stop_and_wait(Duration::from_secs(10)));
        assert!(cleaned.load(Ordering::SeqCst));
    }
}
