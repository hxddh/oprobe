//! Long work off the UI thread: `App::run` and `App::prove` on a Tokio runtime of their own,
//! drawing their progress from the sink's live stream (`EventBus::subscribe_live`). The UI
//! polls a job's shared state ([`Job::peek`]) and only ever reads copies of it. Loading a run
//! ([`off_thread`]) uses the same runtime.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::app::{App, Cancel, EventSink, ProveOutcome, ProveRequest, RunOutcome, RunRequest};
use crate::engine::Progress;
use crate::events::Live;
use crate::operator::OperatorSpec;
use crate::trace::Phase;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("oprobe-app")
            .thread_stack_size(8 * 1024 * 1024)
            .build()
            .expect("tokio runtime")
    })
}

/// Run blocking work (loading a run file) on the engine's threads; `None` if it panicked.
pub async fn off_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = futures::channel::oneshot::channel();
    runtime().spawn_blocking(move || {
        let _ = tx.send(f());
    });
    rx.await.ok()
}

/// One replay of a Prove: baseline (A) or candidate (B), and its objective value.
#[derive(Clone, Debug, PartialEq)]
pub struct Trial {
    pub candidate: bool,
    pub value: f64,
}

/// The verdict of a Prove with its evidence and the proven diff.
#[derive(Clone, Debug, PartialEq)]
pub struct Proved {
    pub verdict: crate::app::ProveVerdict,
    pub delta: (f64, f64, f64),
    pub significant: bool,
    pub pairs: u64,
    /// The other metrics: `(metric, relative change)`.
    pub secondary: Vec<(String, f64)>,
    pub rust: String,
    pub toml: String,
}

/// How a job ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The runs it saved, in order.
    Ran(Vec<String>),
    Proved(Box<Proved>),
    Failed {
        message: String,
        hint: Option<String>,
    },
    Stopped,
}

/// What the UI reads of a running job.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub phase: Option<Phase>,
    pub elapsed_s: f64,
    pub total_s: f64,
    /// Goodput over the run so far (the live sparkline).
    pub spark: Vec<f64>,
    pub ops_per_s: f64,
    pub p99_ms: f64,
    pub errors: u64,
    pub throttled: u64,
    pub cleaning: bool,
    /// Prove's replays as they finish.
    pub trials: Vec<Trial>,
    /// Operators measured so far (a run of several operators).
    pub done_count: usize,
    pub outcome: Option<Outcome>,
}

const SPARK_POINTS: usize = 240;

/// A running `run` or `prove`.
#[derive(Clone)]
pub struct Job {
    state: Arc<Mutex<State>>,
    cancel: Cancel,
}

impl Job {
    /// The state now (a copy).
    pub fn peek(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Ask the job to stop (it still removes its test data).
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    pub fn finished(&self) -> bool {
        self.peek().outcome.is_some()
    }

    fn start(total_s: f64, p99_objective: bool, base: Option<OperatorSpec>) -> (Self, EventSink) {
        let state = Arc::new(Mutex::new(State {
            total_s,
            ..Default::default()
        }));
        let sink = EventSink::silent();
        let st = state.clone();
        sink.bus().subscribe_live(Arc::new(move |e: &Live| {
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            match e {
                Live::Progress(Progress::Snapshot(snap)) => {
                    s.phase = Some(snap.phase);
                    s.cleaning = false;
                    if snap.phase == Phase::Measure {
                        s.elapsed_s = snap.elapsed_s;
                        s.ops_per_s = snap.summary.ops_per_s;
                        s.p99_ms = snap.summary.latency.p99;
                        s.errors = snap.summary.failed_ops;
                        s.throttled = snap.resilience.throttled_responses;
                        s.spark.push(snap.summary.ops_per_s);
                        if s.spark.len() > SPARK_POINTS {
                            // Halve the resolution rather than drop the start.
                            s.spark = s
                                .spark
                                .chunks(2)
                                .map(|c| c.iter().sum::<f64>() / c.len() as f64)
                                .collect();
                        }
                    }
                }
                Live::Progress(Progress::Message(_)) => {}
                Live::CleaningUp => s.cleaning = true,
                Live::Finished { run, .. } => {
                    s.done_count += 1;
                    if let Some(base) = &base {
                        let r = &run.report;
                        let value = if p99_objective {
                            r.summary.latency.p99
                        } else {
                            r.summary.ops_per_s
                        };
                        s.trials.push(Trial {
                            candidate: r.spec.profile.operator != *base,
                            value,
                        });
                    }
                    s.spark.clear();
                }
            }
        }));
        (
            Self {
                state,
                cancel: Cancel::new(),
            },
            sink,
        )
    }

    fn finish(state: &Arc<Mutex<State>>, cancel: &Cancel, res: anyhow::Result<Outcome>) {
        let outcome = match res {
            Ok(o) => o,
            Err(_) if cancel.is_cancelled() => Outcome::Stopped,
            Err(e) => {
                let body = crate::errors::classify(&e, "s3");
                Outcome::Failed {
                    message: body.message,
                    hint: body.hint,
                }
            }
        };
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.cleaning = false;
        s.outcome = Some(outcome);
    }

    /// `App::run`: measure the operators of `req` one after another.
    pub fn run(app: App, req: RunRequest) -> Self {
        let total = req
            .workload
            .duration
            .as_deref()
            .and_then(|d| crate::util::parse_duration(d).ok())
            .unwrap_or(Duration::from_secs(10))
            .as_secs_f64();
        let (job, sink) = Self::start(total, false, None);
        let (state, cancel) = (job.state.clone(), job.cancel.clone());
        runtime().spawn(async move {
            let res = app.run(req, sink, cancel.clone()).await.map(|o| match o {
                RunOutcome::Run(set) => {
                    Outcome::Ran(set.runs.into_iter().map(|r| r.run_id).collect())
                }
                RunOutcome::Plan(_) => Outcome::Ran(Vec::new()),
            });
            Self::finish(&state, &cancel, res);
        });
        job
    }

    /// `App::prove`: A/B-test a change of `base` (the run's operator).
    pub fn prove(app: App, req: ProveRequest, base: OperatorSpec) -> Self {
        let p99 = req.objective == crate::app::ProveObjective::P99;
        let (job, sink) = Self::start(0.0, p99, Some(base));
        let (state, cancel) = (job.state.clone(), job.cancel.clone());
        runtime().spawn(async move {
            let res = app.prove(req, sink, cancel.clone()).await.map(|o| match o {
                ProveOutcome::Proved(p) => Outcome::Proved(Box::new(Proved {
                    verdict: p.verdict,
                    delta: (
                        p.objective_delta.estimate,
                        p.objective_delta.low,
                        p.objective_delta.high,
                    ),
                    significant: p.objective_delta.low > 0.0 || p.objective_delta.high < 0.0,
                    pairs: p.pairs_count,
                    secondary: p
                        .secondary
                        .iter()
                        .map(|s| (s.metric.clone(), s.change.estimate))
                        .collect(),
                    rust: p.diff.rust,
                    toml: p.diff.toml,
                })),
                ProveOutcome::Plan(_) => Outcome::Stopped,
            });
            Self::finish(&state, &cancel, res);
        });
        job
    }
}

/// Prove's pairs so far: each candidate with the baseline just before it, and the change of
/// the objective (candidate / baseline − 1).
pub fn pairs(trials: &[Trial]) -> Vec<(f64, f64, f64)> {
    let mut out = Vec::new();
    let mut last_a = None;
    for t in trials {
        if t.candidate {
            if let Some(a) = last_a.take() {
                let d = if a > 0.0 { t.value / a - 1.0 } else { 0.0 };
                out.push((a, t.value, d));
            }
        } else {
            last_a = Some(t.value);
        }
    }
    out
}

/// Every job that has not ended, so quitting can stop them and wait for their cleanup.
static LIVE: Mutex<Vec<Job>> = Mutex::new(Vec::new());

/// Track `job` until it ends.
pub fn track(job: &Job) {
    let mut l = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    l.retain(|j| !j.finished());
    l.push(job.clone());
}

/// Whether any job still runs (its cleanup included).
pub fn busy() -> bool {
    let mut l = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    l.retain(|j| !j.finished());
    !l.is_empty()
}

/// Stop every job and wait (at most `timeout`) until each has cleaned up; whether all did.
pub fn shutdown(timeout: Duration) -> bool {
    let jobs: Vec<Job> = LIVE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for j in &jobs {
        j.stop();
    }
    let deadline = std::time::Instant::now() + timeout;
    while jobs.iter().any(|j| !j.finished()) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_take_the_baseline_before_each_candidate() {
        let t = |candidate, value| Trial { candidate, value };
        let p = pairs(&[
            t(false, 100.0),
            t(false, 100.0),
            t(true, 120.0),
            t(false, 90.0),
            t(true, 99.0),
        ]);
        assert_eq!(p.len(), 2);
        assert!((p[0].2 - 0.2).abs() < 1e-9);
        assert!((p[1].2 - 0.1).abs() < 1e-9);
    }
}
