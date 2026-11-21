//! Background runs and tunes started by MCP tools: one [`Job`] per run id, executed on the
//! server's runtime (at most `max_concurrent_runs` at a time), observable through its
//! [`EventBus`] (progress notifications, `oprobe://runs/{id}/events`) and cancellable.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::{Semaphore, watch};

use crate::config::Profile;
use crate::engine::Cancel;
use crate::errors::{ErrorBody, ErrorCode};
use crate::events::{Event, EventBus, EventKind, LogLevel};
use crate::guard::{Budgets, Plan};
use crate::model::RunId;
use crate::store::RunStore;
use crate::trace::Phase;

/// Events kept per job for `oprobe://runs/{id}/events`.
const MAX_EVENTS: usize = 5000;

/// What a job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Run,
    Tune,
}

/// Lifecycle of a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Waiting for a run slot.
    Queued,
    Running,
    Succeeded,
    /// Ended with an error (a budget breach still saves the partial run).
    Failed,
    Cancelled,
    /// Not started by this server: a stored run.
    Stored,
}

impl JobStatus {
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// Live progress of a job.
#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub struct JobProgress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// Seconds into the current phase's window (measurement window while measuring).
    pub elapsed_s: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_s: Option<f64>,
    pub ops_count: u64,
    pub requests_count: u64,
    /// Tuner trials finished so far.
    pub trials_count: u64,
}

struct Inner {
    progress: JobProgress,
    error: Option<ErrorBody>,
    path: Option<String>,
    events: VecDeque<Event>,
    user_cancelled: bool,
    ended_s: Option<f64>,
    /// The final status, set by [`Job::finish`] and published with the final event.
    ending: Option<JobStatus>,
    recommendation: Option<Box<crate::tune::recommend::Recommendation>>,
}

/// One background run or tune.
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub plan: Plan,
    pub budgets: Budgets,
    pub cancel: Cancel,
    pub bus: EventBus,
    started: Instant,
    inner: Mutex<Inner>,
    status: watch::Sender<JobStatus>,
}

/// A snapshot of a job.
pub struct JobView {
    pub status: JobStatus,
    pub progress: JobProgress,
    pub error: Option<ErrorBody>,
    pub path: Option<String>,
    pub wall_s: f64,
    /// The tuner's recommendation (finished tune jobs).
    pub recommendation: Option<Box<crate::tune::recommend::Recommendation>>,
}

impl Job {
    fn new(id: String, kind: JobKind, plan: Plan, budgets: Budgets) -> Arc<Self> {
        let (status, _) = watch::channel(JobStatus::Queued);
        let job = Arc::new(Self {
            id,
            kind,
            plan,
            budgets,
            cancel: Cancel::new(),
            bus: EventBus::new(),
            started: Instant::now(),
            inner: Mutex::new(Inner {
                progress: JobProgress::default(),
                error: None,
                path: None,
                events: VecDeque::new(),
                user_cancelled: false,
                ended_s: None,
                ending: None,
                recommendation: None,
            }),
            status,
        });
        let weak = Arc::downgrade(&job);
        job.bus.subscribe(Arc::new(move |e| {
            if let Some(j) = weak.upgrade() {
                j.record(e);
            }
        }));
        job.bus.emit_event(&Event::version());
        job
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, e: &Event) {
        let mut i = self.lock();
        match &e.kind {
            // `done` is the job's, not the engine's: the run still cleans up and saves after
            // its window closes, and `get_run` must not read `done` while it is `running`.
            EventKind::Phase { phase } if *phase == Phase::Done && i.ending.is_none() => {
                i.progress.phase = Some(Phase::Cleanup)
            }
            EventKind::Phase { phase } => i.progress.phase = Some(*phase),
            EventKind::Progress {
                elapsed_s,
                total_s,
                ops,
                requests,
            } => {
                // Never past the planned window.
                i.progress.elapsed_s = match total_s {
                    Some(t) if *t > 0.0 => elapsed_s.min(*t),
                    _ => *elapsed_s,
                };
                i.progress.total_s = *total_s;
                i.progress.ops_count = *ops;
                i.progress.requests_count = *requests;
            }
            EventKind::Trial { .. } => i.progress.trials_count += 1,
            _ => {}
        }
        if i.events.len() >= MAX_EVENTS {
            i.events.pop_front();
        }
        i.events.push_back(e.clone());
        // The final event and the final status are published under the same lock: a reader
        // sees both or neither (AG-6).
        let last = matches!(e.kind, EventKind::Result { .. } | EventKind::Error { .. });
        if last && let Some(status) = i.ending {
            if i.progress.phase.is_some() {
                i.progress.phase = Some(Phase::Done);
            }
            i.ended_s = Some(self.started.elapsed().as_secs_f64());
            self.status.send_replace(status);
        }
    }

    pub fn status(&self) -> JobStatus {
        *self.status.borrow()
    }

    /// Wait until the job has ended.
    pub async fn wait(&self) -> JobStatus {
        let mut rx = self.status.subscribe();
        let r = rx.wait_for(|s| s.is_final()).await.map(|s| *s);
        r.unwrap_or_else(|_| self.status())
    }

    /// A receiver of status changes.
    pub fn watch(&self) -> watch::Receiver<JobStatus> {
        self.status.subscribe()
    }

    pub fn view(&self) -> JobView {
        let i = self.lock();
        JobView {
            status: self.status(),
            progress: i.progress.clone(),
            error: i.error.clone(),
            path: i.path.clone(),
            wall_s: i
                .ended_s
                .unwrap_or_else(|| self.started.elapsed().as_secs_f64()),
            recommendation: i.recommendation.clone(),
        }
    }

    /// Events recorded so far.
    pub fn events(&self) -> Vec<Event> {
        self.lock().events.iter().cloned().collect()
    }

    /// Ask the job to stop (gracefully: cleanup still runs and the partial run is saved).
    pub fn request_cancel(&self) {
        self.lock().user_cancelled = true;
        self.cancel.cancel();
    }

    /// End the job: the `done` phase, then its last event (`result`, or `error` when `error`
    /// is set); recording that event flips the status (see [`Job::record`]).
    fn finish(
        &self,
        status: JobStatus,
        error: Option<ErrorBody>,
        path: Option<String>,
        run_id: Option<String>,
    ) {
        {
            let mut i = self.lock();
            i.error = error.clone();
            if path.is_some() {
                i.path = path.clone();
            }
            i.ending = Some(status);
        }
        self.bus.emit(EventKind::Phase { phase: Phase::Done });
        match error {
            Some(error) => self.bus.emit(EventKind::Error { error }),
            None => self.bus.emit(EventKind::Result {
                run_id,
                path,
                report: None,
                data: None,
            }),
        }
        if !self.status().is_final() {
            let mut i = self.lock();
            i.ended_s = Some(self.started.elapsed().as_secs_f64());
            self.status.send_replace(status);
        }
    }
}

/// Everything a background run needs.
pub struct JobSpec {
    pub kind: JobKind,
    pub run_id: RunId,
    pub run_prefix: String,
    pub profile: Profile,
    pub target_name: Option<String>,
    pub budgets: Budgets,
    pub plan: Plan,
    pub trace_max_requests: usize,
    pub store: RunStore,
    pub label: Option<String>,
    pub tune: Option<crate::tune::TuneOptions>,
}

/// The server's jobs.
pub struct Jobs {
    map: Mutex<BTreeMap<String, Arc<Job>>>,
    slots: Arc<Semaphore>,
}

impl Jobs {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            map: Mutex::new(BTreeMap::new()),
            slots: Arc::new(Semaphore::new(max_concurrent.max(1))),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// Jobs, oldest first.
    pub fn all(&self) -> Vec<Arc<Job>> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Cancel every running job (server shutdown).
    pub fn cancel_all(&self) {
        for j in self.all() {
            if !j.status().is_final() {
                j.request_cancel();
            }
        }
    }

    /// Start a job in the background; returns at once.
    pub fn spawn(&self, spec: JobSpec) -> Arc<Job> {
        let job = Job::new(
            spec.run_id.to_string(),
            spec.kind,
            spec.plan.clone(),
            spec.budgets.clone(),
        );
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job.id.clone(), job.clone());
        let slots = self.slots.clone();
        let j = job.clone();
        tokio::spawn(async move {
            let permit = tokio::select! {
                p = slots.acquire_owned() => p.ok(),
                _ = j.cancel.cancelled() => None,
            };
            let Some(_permit) = permit else {
                j.finish(JobStatus::Cancelled, None, None, None);
                return;
            };
            j.status.send_replace(JobStatus::Running);
            let scheme = spec.profile.target.scheme.clone();
            let run_id = spec.run_id.to_string();
            let res = execute(&j, spec).await;
            let cancelled = j.lock().user_cancelled;
            match res {
                Ok(o) => {
                    let (status, error) = match o.breach {
                        Some(b) => (
                            JobStatus::Failed,
                            Some(crate::errors::classify(&b.to_error(), &scheme)),
                        ),
                        None if cancelled => (JobStatus::Cancelled, None),
                        None => (JobStatus::Succeeded, None),
                    };
                    j.lock().recommendation = o.recommendation;
                    j.finish(status, error, Some(o.path), Some(run_id));
                }
                Err(e) => {
                    let mut body = crate::errors::classify(&e, &scheme);
                    if cancelled {
                        body.code = ErrorCode::RunInterrupted;
                        body.retryable = true;
                    }
                    let status = if cancelled {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Failed
                    };
                    j.finish(status, Some(body), None, Some(run_id));
                }
            }
        });
        job
    }
}

struct Outcome {
    path: String,
    breach: Option<crate::guard::BudgetBreach>,
    recommendation: Option<Box<crate::tune::recommend::Recommendation>>,
}

async fn execute(job: &Arc<Job>, spec: JobSpec) -> anyhow::Result<Outcome> {
    use crate::engine::measure::{self, Mode, Rails, Request, Stage};
    let JobSpec {
        kind: _,
        run_id,
        run_prefix,
        profile,
        target_name,
        budgets,
        plan,
        trace_max_requests,
        store,
        label,
        tune,
    } = spec;
    let mut profile = profile;
    if label.is_some() {
        profile.name = label;
    }
    let total_s = match &tune {
        Some(t) => (t.trial.duration + t.trial.warmup).as_secs_f64() * t.max_trials as f64,
        None => profile.workload.duration.get().as_secs_f64(),
    };
    // The engine reports `done` when the window closes; cleanup and saving follow. The job's
    // stream says `cleaning up` then, and `done` only with its final event (`Job::finish`).
    let inner = job.bus.progress_fn(Some(total_s));
    let progress: crate::engine::ProgressFn = Arc::new(move |p| match p {
        crate::engine::Progress::Snapshot(mut s) if s.phase == Phase::Done => {
            s.phase = Phase::Cleanup;
            inner(crate::engine::Progress::Snapshot(s))
        }
        other => inner(other),
    });
    let rails = Rails {
        run_id: run_id.clone(),
        run_prefix,
        read_only: plan.read_only,
        budgets,
    };
    let guard = rails.guard(&profile);
    let mode = match tune {
        None => Mode::Run,
        Some(opts) => {
            let bus = job.bus.clone();
            Mode::Tune {
                opts,
                on_event: Arc::new(move |ev| match ev {
                    crate::tune::TuneEvent::Finished(t) => {
                        bus.emit(EventKind::Trial { trial: (&t).into() })
                    }
                    crate::tune::TuneEvent::Message(m) => bus.log(LogLevel::Info, m),
                    crate::tune::TuneEvent::Started { .. } => {}
                }),
            }
        }
    };
    let bus = job.bus.clone();
    let mut req = Request::new(profile, rails, job.cancel.clone(), progress);
    req.target_name = target_name;
    req.guard = guard;
    req.trace_max_requests = trace_max_requests;
    req.store = Some(store);
    req.mode = mode;
    req.on_stage = Some(Arc::new(move |s| {
        if s == Stage::CleaningUp {
            bus.log(LogLevel::Info, "cleaning up");
        }
    }));
    let m = measure::measure(req).await?;
    for w in &m.warnings {
        job.bus.log(LogLevel::Warn, w.clone());
    }
    let path = m.path.map(|p| p.display().to_string()).unwrap_or_default();
    // The tuner's answer as a recommendation (AG-1): the winner's knobs, profile, snippet and
    // deltas, in `start_tune` / `get_run` structured output.
    let recommendation = m.tuned.as_ref().and_then(|t| {
        match crate::tune::recommend::Recommendation::from_tune(
            &t.profile,
            &t.winner,
            &t.summary,
            &t.capability,
        ) {
            Ok(r) => Some(Box::new(r)),
            Err(e) => {
                job.bus
                    .log(LogLevel::Warn, format!("no recommendation: {e:#}"));
                None
            }
        }
    });
    let _ = run_id;
    Ok(Outcome {
        path,
        breach: m.breach,
        recommendation,
    })
}
