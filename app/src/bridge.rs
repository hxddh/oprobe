//! Bridge between the UI and the `oprobe` engine. Benchmarks run on a Tokio runtime of their
//! own (the engine spawns Tokio tasks and owns sockets); the UI polls a shared, `Send` state.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use oprobe::config::Profile;
use oprobe::engine::{Cancel, Progress, ProgressFn, RunOptions, Session};
use oprobe::net::probe::{ProbeOptions, ProbeReport, probe_endpoint};
use oprobe::report::{Report, TuneSummary};
use oprobe::trace::Snapshot;
use oprobe::tune::{Objective, Trial, TuneEvent, TuneOptions, codegen, tune};

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

/// The finished result of a job.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Report(Box<Report>),
    Tuned {
        report: Box<Report>,
        summary: Box<TuneSummary>,
        profile_toml: String,
        rust: String,
    },
    Probe(Box<ProbeReport>),
    Failed {
        error: String,
        hint: Option<(String, Vec<String>)>,
    },
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

/// A running benchmark, tuning session or probe.
pub struct Job {
    state: Arc<Mutex<State>>,
    cancel: Cancel,
}

fn failed(err: &anyhow::Error, scheme: &str) -> Outcome {
    let chain = format!("{err:#}");
    let hint = oprobe::analysis::hints::hint_for(&chain, scheme).map(|h| (h.title, h.steps));
    Outcome::Failed { error: chain, hint }
}

impl Job {
    fn spawn<F, Fut>(f: F) -> Arc<Self>
    where
        F: FnOnce(Arc<Mutex<State>>, Cancel) -> Fut,
        Fut: std::future::Future<Output = Outcome> + Send + 'static,
    {
        let state = Arc::new(Mutex::new(State::default()));
        let cancel = Cancel::new();
        let fut = f(state.clone(), cancel.clone());
        let st = state.clone();
        runtime().spawn(async move {
            let outcome = fut.await;
            st.lock().unwrap_or_else(|e| e.into_inner()).outcome = Some(outcome);
        });
        Arc::new(Self { state, cancel })
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

    /// Run one workload window and build its report.
    pub fn run(profile: Profile) -> Arc<Self> {
        Self::spawn(move |state, cancel| async move {
            let scheme = profile.target.scheme.clone();
            let progress = Self::progress(&state);
            let res: anyhow::Result<Report> = async {
                let session = Session::open(profile, cancel, progress.clone()).await?;
                for w in &session.warnings {
                    progress(Progress::Message(format!("warning: {w}")));
                }
                let knobs = session.profile.knobs.clone();
                let run = session
                    .run(
                        &knobs,
                        &RunOptions::from_profile(&session.profile),
                        Some(progress.clone()),
                    )
                    .await;
                let (ok, bad) = if run.is_ok() && session.profile.workload.verify {
                    session.verify_writes(None).await
                } else {
                    (0, 0)
                };
                progress(Progress::Message("cleaning up test data…".into()));
                let cleanup = session.close().await;
                let mut snap = run?;
                snap.integrity.verified += ok;
                snap.integrity.corrupted += bad;
                Ok(Report::build(
                    &session.profile,
                    &knobs,
                    snap,
                    cleanup,
                    session.warnings.clone(),
                ))
            }
            .await;
            match res {
                Ok(r) => Outcome::Report(Box::new(r)),
                Err(e) => failed(&e, &scheme),
            }
        })
    }

    /// Run the closed-loop tuner.
    pub fn tune(
        profile: Profile,
        objective: Objective,
        trial: Duration,
        max_trials: usize,
    ) -> Arc<Self> {
        Self::spawn(move |state, cancel| async move {
            let scheme = profile.target.scheme.clone();
            let progress = Self::progress(&state);
            let res: anyhow::Result<Outcome> = async {
                let session = Session::open(profile, cancel, progress.clone()).await?;
                let opts = TuneOptions {
                    objective,
                    trial: RunOptions {
                        duration: trial,
                        warmup: Duration::from_millis(500),
                        max_ops: None,
                        rate: session.profile.workload.rate,
                        trace_slow_ops: false,
                    },
                    max_concurrency: 256,
                    max_trials,
                    cooldown: Duration::from_secs(3),
                };
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
                let out = tune(&session, &opts, on_event).await;
                progress(Progress::Message("cleaning up test data…".into()));
                let cleanup = session.close().await;
                let out = out?;
                let mut report = Report::build(
                    &session.profile,
                    &out.winner,
                    out.winner_snapshot.clone(),
                    cleanup,
                    session.warnings.clone(),
                );
                report.tune = Some(out.summary.clone());
                let header = format!(
                    "objective {} · {:+.0}% vs baseline",
                    out.summary.objective,
                    out.summary.improvement * 100.0
                );
                Ok(Outcome::Tuned {
                    profile_toml: codegen::profile_toml(&session.profile, &out.winner, &header)?,
                    rust: codegen::rust_snippet(&session.profile.target.scheme, &out.winner),
                    summary: Box::new(out.summary),
                    report: Box::new(report),
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
}
