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

/// One trial of a Prove: baseline (A) or candidate (B), and its objective value.
#[derive(Clone, Debug, PartialEq)]
pub struct Trial {
    pub candidate: bool,
    pub value: f64,
    /// Operations it counted (the objective's resolution).
    pub ops: u64,
}

/// The verdict of a Prove with its evidence and the proven diff.
#[derive(Clone, Debug, PartialEq)]
pub struct Proved {
    pub prove_id: String,
    pub run_id: String,
    pub objective: crate::app::ProveObjective,
    pub verdict: crate::app::ProveVerdict,
    pub delta: (f64, f64, f64),
    pub significant: bool,
    pub pairs: u64,
    /// The objective of each A/B pair, `(baseline, candidate, fewest operations)`, in order.
    pub ab: Vec<(f64, f64, u64)>,
    /// Last completed A/B pair's stored `(baseline_run_id, candidate_run_id)`, for Compare.
    pub trial_ids: Option<(String, String)>,
    /// The other metrics (shown, never decisive).
    pub secondary: Vec<Secondary>,
    /// The proven change as `PATH=VALUE` pairs (sized values included).
    pub change: Vec<String>,
    pub rust: String,
    pub toml: String,
}

impl Proved {
    /// Convert a [`crate::app::ProveOutput`] (live or loaded from `<id>.prove.json`) into the
    /// UI's [`Proved`] summary.
    pub fn from_output(p: crate::app::ProveOutput) -> Self {
        use crate::app::prove::TrialKind;
        let p99 = p.objective == crate::app::ProveObjective::P99;
        let trial_ids = (0..p.pairs_count).rev().find_map(|pair| {
            let a = p
                .trials
                .iter()
                .find(|t| t.kind == TrialKind::Baseline && t.pair == Some(pair))?;
            let b = p
                .trials
                .iter()
                .find(|t| t.kind == TrialKind::Candidate && t.pair == Some(pair))?;
            (!a.run_id.is_empty() && !b.run_id.is_empty())
                .then(|| (a.run_id.clone(), b.run_id.clone()))
        });
        Self {
            prove_id: p.prove_id,
            run_id: p.run_id,
            objective: p.objective,
            verdict: p.verdict,
            delta: (
                p.objective_delta.estimate,
                p.objective_delta.low,
                p.objective_delta.high,
            ),
            significant: p.objective_delta.low > 0.0 || p.objective_delta.high < 0.0,
            pairs: p.pairs_count,
            ab: ab_pairs(&p.trials, p99),
            trial_ids,
            secondary: p.secondary.iter().map(Secondary::of).collect(),
            change: p.change,
            rust: p.diff.rust,
            toml: p.diff.toml,
        }
    }
}

/// A secondary metric of a Prove: a relative change, or for a share (`throttled`, `errors`) a
/// difference in percentage points.
#[derive(Clone, Debug, PartialEq)]
pub struct Secondary {
    /// `p99_ms`, `goodput_ops_per_s`, `throttled_request_ratio`, `error_ratio`.
    pub metric: String,
    /// Relative change (`0.1` = +10 %), or the difference of two shares (`0.005` = +0.5 pp).
    pub change: f64,
    /// `change` is a difference of shares, shown in percentage points.
    pub points: bool,
}

impl Secondary {
    fn of(s: &crate::app::prove::SecondaryMetric) -> Self {
        Self {
            metric: s.metric.clone(),
            change: s.change.estimate,
            points: s.metric.ends_with("_ratio"),
        }
    }

    /// The metric's name in the app's language.
    pub fn label(&self) -> String {
        let key = match self.metric.as_str() {
            "p99_ms" => "metric.p99",
            "goodput_ops_per_s" => "metric.goodput",
            "throttled_request_ratio" => "metric.throttled",
            "error_ratio" => "metric.errors",
            other => return other.to_owned(),
        };
        super::i18n::tr(key).to_owned()
    }

    /// `p99 −12.0 %`, `Throttled +0.500 pp`.
    pub fn text(&self) -> String {
        let v = if self.points {
            super::fmt::points(self.change)
        } else {
            super::fmt::delta(self.change)
        };
        format!("{} {v}", self.label())
    }
}

/// The secondary metrics on one line, captioned as not decisive (shares in percentage
/// points), in the app's language.
pub fn secondary_line(s: &[Secondary]) -> String {
    if s.is_empty() {
        return String::new();
    }
    let items: Vec<String> = s.iter().map(Secondary::text).collect();
    super::i18n::trf("operator.secondary", &[&items.join(" · ")])
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
    /// Prove's trials as they finish.
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

    fn start(total_s: f64, is_prove: bool) -> (Self, EventSink) {
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
                    // Prove streams discrete completed A/B trials via `EventKind::Trial`;
                    // skipping 100 ms snapshot churn avoids re-rendering the convergence plot
                    // ten times per second while a trial is measuring.
                    if !is_prove && snap.phase == Phase::Measure {
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
                Live::Finished { .. } => {
                    s.done_count += 1;
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

    fn finish(
        state: &Arc<Mutex<State>>,
        cancel: &Cancel,
        scheme: &str,
        res: anyhow::Result<Outcome>,
    ) {
        let outcome = match res {
            Ok(o) => o,
            Err(_) if cancel.is_cancelled() => Outcome::Stopped,
            Err(e) => {
                let body = crate::errors::classify(&e, scheme);
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
        let scheme = req
            .operators
            .first()
            .and_then(|op| {
                if let Ok(cfg) = app.config()
                    && let Ok((spec, _)) = crate::operator::resolve::resolve(op, &cfg)
                {
                    crate::config::TargetSpec::from_url(&spec.url)
                        .ok()
                        .map(|t| t.scheme)
                } else {
                    crate::config::TargetSpec::from_url(op)
                        .ok()
                        .map(|t| t.scheme)
                }
            })
            .unwrap_or_else(|| "s3".into());
        let total = req
            .workload
            .duration
            .as_deref()
            .and_then(|d| crate::util::parse_duration(d).ok())
            .unwrap_or(Duration::from_secs(10))
            .as_secs_f64();
        let (job, sink) = Self::start(total, false);
        let (state, cancel) = (job.state.clone(), job.cancel.clone());
        runtime().spawn(async move {
            let res = app
                .run(req, sink, cancel.clone())
                .await
                .and_then(|o| match o {
                    RunOutcome::Run(set) => match set.error {
                        // A spent budget is the job's error, as before (the partial run is
                        // saved).
                        Some(e) => Err(crate::errors::OprobeError::new(e.code, e.message).into()),
                        None => Ok(Outcome::Ran(
                            set.runs.into_iter().map(|r| r.run_id).collect(),
                        )),
                    },
                    RunOutcome::Plan(_) => Ok(Outcome::Ran(Vec::new())),
                });
            Self::finish(&state, &cancel, &scheme, res);
        });
        job
    }

    /// `App::prove`: A/B-test a change of `base` (the run's operator).
    pub fn prove(app: App, req: ProveRequest, base: OperatorSpec) -> Self {
        let p99 = req.objective == crate::app::ProveObjective::P99;
        let scheme = crate::config::TargetSpec::from_url(&base.url)
            .ok()
            .map(|t| t.scheme)
            .unwrap_or_else(|| "s3".into());
        let (job, sink) = Self::start(0.0, true);
        let st_trials = job.state.clone();
        sink.bus()
            .subscribe(Arc::new(move |e: &crate::events::Event| {
                use crate::app::prove::TrialKind;
                if let crate::events::EventKind::Trial { trial } = &e.kind {
                    let candidate = match trial.kind {
                        TrialKind::Baseline => false,
                        TrialKind::Candidate => true,
                        // Sizing sweeps are never A/B pairs.
                        TrialKind::Sizing => return,
                    };
                    let value = if p99 {
                        trial.p99_ms
                    } else {
                        trial.goodput_ops_per_s
                    };
                    let mut s = st_trials.lock().unwrap_or_else(|e| e.into_inner());
                    s.trials.push(Trial {
                        candidate,
                        value,
                        ops: trial.ops_count,
                    });
                }
            }));
        let (state, cancel) = (job.state.clone(), job.cancel.clone());
        runtime().spawn(async move {
            let res = app.prove(req, sink, cancel.clone()).await.map(|o| match o {
                ProveOutcome::Proved(p) => Outcome::Proved(Box::new(Proved::from_output(*p))),
                ProveOutcome::Plan(_) => Outcome::Stopped,
            });
            Self::finish(&state, &cancel, &scheme, res);
        });
        job
    }
}

/// The objective of each complete A/B pair of a prove's trials, by pair number.
pub fn ab_pairs(trials: &[crate::app::prove::ProveTrial], p99: bool) -> Vec<(f64, f64, u64)> {
    use crate::app::prove::TrialKind;
    let v = |t: &crate::app::prove::ProveTrial| if p99 { t.p99_ms } else { t.goodput_ops_per_s };
    type Side = Option<(f64, u64)>;
    let mut by: std::collections::BTreeMap<u64, (Side, Side)> = Default::default();
    for t in trials {
        let Some(pair) = t.pair else { continue };
        let e = by.entry(pair).or_default();
        match t.kind {
            TrialKind::Baseline => e.0 = Some((v(t), t.ops_count)),
            TrialKind::Candidate => e.1 = Some((v(t), t.ops_count)),
            TrialKind::Sizing => {}
        }
    }
    by.into_values()
        .filter_map(|(a, b)| {
            let ((a, na), (b, nb)) = (a?, b?);
            Some((a, b, na.min(nb)))
        })
        .collect()
}

/// Prove's pairs so far as `(baseline, candidate, fewest operations)`.
pub fn ab_live(trials: &[Trial]) -> Vec<(f64, f64, u64)> {
    let mut out = Vec::new();
    let mut last_a = None;
    for t in trials {
        if t.candidate {
            if let Some((a, na)) = last_a.take() {
                out.push((a, t.value, t.ops.min(na)));
            }
        } else {
            last_a = Some((t.value, t.ops));
        }
    }
    out
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
    fn secondary_shares_read_as_percentage_points_in_both_languages() {
        let s = vec![
            Secondary {
                metric: "p99_ms".into(),
                change: -0.12,
                points: false,
            },
            Secondary {
                metric: "throttled_request_ratio".into(),
                change: 0.005,
                points: true,
            },
            Secondary {
                metric: "error_ratio".into(),
                change: -0.0002,
                points: true,
            },
        ];
        // Each test runs on its own thread: the language is per thread.
        super::super::i18n::set(super::super::i18n::Lang::En);
        let en = secondary_line(&s);
        assert!(en.contains("p99 −12.0 %"), "{en}");
        assert!(en.contains("Throttled +0.500 pp"), "{en}");
        assert!(en.contains("Errors −0.0200 pp"), "{en}");
        assert!(en.contains("percentage points"), "{en}");
        super::super::i18n::set(super::super::i18n::Lang::Zh);
        let zh = secondary_line(&s);
        assert!(zh.contains("被限流 +0.500 pp"), "{zh}");
        assert!(zh.contains("百分点"), "{zh}");
        super::super::i18n::set(super::super::i18n::Lang::En);
    }

    #[test]
    fn pairs_take_the_baseline_before_each_candidate() {
        let t = |candidate, value| Trial {
            candidate,
            value,
            ops: 100,
        };
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
