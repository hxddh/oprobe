//! App state: one `Copy` store of signals, made in the root and read through context.

use std::sync::Arc;
use std::time::Duration;

use crate::analysis::diagnosis::{Diagnosis, diagnose};
use crate::cli::args::{CostArgs, KnobArgs, TargetArgs, WorkloadArgs};
use crate::config::{Knobs, Profile};
use crate::net::probe::ProbeReport;
use crate::report::{Report, TuneSummary};
use crate::trace::Snapshot;
use crate::tune::{Objective, Trial};
use nsh_core::prelude::*;

use crate::ui::bridge::{Job, Outcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Workbench,
    Tune,
    Probe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
    Tuning,
    Probing,
    Done,
    Failed,
}

impl Status {
    pub fn busy(self) -> bool {
        matches!(self, Status::Running | Status::Tuning | Status::Probing)
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "Ready",
            Status::Running => "Running",
            Status::Tuning => "Tuning",
            Status::Probing => "Probing",
            Status::Done => "Finished",
            Status::Failed => "Failed",
        }
    }
}

/// One point of the live charts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub t: f64,
    pub ops: f64,
    pub mib: f64,
    pub p99: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub error: String,
    pub hint: Option<(String, Vec<String>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tuned {
    pub summary: TuneSummary,
    pub profile_toml: String,
    pub rust: String,
}

/// The run form.
#[derive(Clone, Copy)]
pub struct Form {
    pub target: Signal<String>,
    pub endpoint: Signal<String>,
    pub region: Signal<String>,
    pub existing: Signal<String>,
    pub preset: Signal<Option<SharedString>>,
    pub duration: Signal<String>,
    pub concurrency: Signal<String>,
    pub io_concurrency: Signal<String>,
    pub chunk: Signal<String>,
    pub anonymous: Signal<bool>,
    pub insecure: Signal<bool>,
    pub cache: Signal<bool>,
    pub keep_alive: Signal<bool>,
}

#[derive(Clone, Copy)]
pub struct Store {
    pub page: Signal<Page>,
    pub form: Form,
    pub status: Signal<Status>,
    pub snapshot: Signal<Option<Snapshot>>,
    pub history: Signal<Vec<Sample>>,
    pub report: Signal<Option<Report>>,
    pub knobs: Signal<Knobs>,
    pub profile: Signal<Option<Profile>>,
    pub log: Signal<Vec<SharedString>>,
    pub failure: Signal<Option<Failure>>,
    pub tab: Signal<SharedString>,
    pub objective: Signal<Option<SharedString>>,
    pub trial_secs: Signal<String>,
    pub max_trials: Signal<String>,
    pub trials: Signal<Vec<Trial>>,
    pub tuned: Signal<Option<Tuned>>,
    pub probe_endpoint: Signal<String>,
    pub probe: Signal<Option<ProbeReport>>,
    pub notice: Signal<Option<SharedString>>,
    job: Stored<Option<Arc<Job>>>,
    last: Stored<Option<(f64, u64)>>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        let s = Self {
            page: signal(Page::Workbench),
            form: Form {
                target: signal("memory://".to_owned()),
                endpoint: signal(String::new()),
                region: signal(String::new()),
                existing: signal(String::new()),
                preset: signal(Some(SharedString::from("mixed"))),
                duration: signal("10s".to_owned()),
                concurrency: signal("16".to_owned()),
                io_concurrency: signal("4".to_owned()),
                chunk: signal("8MiB".to_owned()),
                anonymous: signal(false),
                insecure: signal(false),
                cache: signal(false),
                keep_alive: signal(true),
            },
            status: signal(Status::Idle),
            snapshot: signal(None),
            history: signal(Vec::new()),
            report: signal(None),
            knobs: signal(Knobs::default()),
            profile: signal(None),
            log: signal(Vec::new()),
            failure: signal(None),
            tab: signal(SharedString::from("operations")),
            objective: signal(Some(SharedString::from("throughput"))),
            trial_secs: signal("4".to_owned()),
            max_trials: signal("16".to_owned()),
            trials: signal(Vec::new()),
            tuned: signal(None),
            probe_endpoint: signal("https://s3.us-east-1.amazonaws.com".to_owned()),
            probe: signal(None),
            notice: signal(None),
            job: stored(None),
            last: stored(None),
        };
        let l = crate::ui::launch();
        batch(|| {
            if let Some(v) = l.target {
                s.form.target.set(v);
            }
            if let Some(v) = l.endpoint {
                s.form.endpoint.set(v);
            }
            if let Some(v) = l.preset {
                s.form.preset.set(Some(v.into()));
            }
            if let Some(v) = l.duration {
                s.form.duration.set(v);
            }
            if let Some(v) = l.concurrency {
                s.form.concurrency.set(v);
            }
            s.form.insecure.set(l.insecure);
            s.form.anonymous.set(l.anonymous);
        });
        // Poll the engine; signals are written only here, on the engine thread.
        set_interval(Duration::from_millis(200), move || s.pull());
        s
    }

    /// Build a profile from the form exactly as the CLI would from flags.
    pub fn build_profile(&self) -> Result<Profile, String> {
        let f = self.form;
        let opt = |s: Signal<String>| {
            let v = s.get_untracked();
            let v = v.trim();
            (!v.is_empty()).then(|| v.to_owned())
        };
        let target = TargetArgs {
            target: opt(f.target),
            endpoint: opt(f.endpoint),
            region: opt(f.region),
            anonymous: f.anonymous.get_untracked(),
            insecure: f.insecure.get_untracked(),
            ..Default::default()
        };
        let workload = WorkloadArgs {
            preset: f.preset.get_untracked().map(|p| p.to_string()),
            duration: opt(f.duration),
            warmup: Some("2s".into()),
            existing: opt(f.existing),
            ..Default::default()
        };
        let parse_n = |s: Signal<String>, what: &str| -> Result<Option<usize>, String> {
            match opt(s) {
                None => Ok(None),
                Some(v) => v
                    .parse()
                    .map(Some)
                    .map_err(|_| format!("{what} must be a number")),
            }
        };
        let knobs = KnobArgs {
            concurrency: parse_n(f.concurrency, "Concurrency")?,
            io_concurrency: parse_n(f.io_concurrency, "I/O concurrency")?,
            chunk_size: opt(f.chunk),
            cache: f.cache.get_untracked().then(|| "256MiB".into()),
            no_keep_alive: !f.keep_alive.get_untracked(),
            ..Default::default()
        };
        crate::cli::build_profile(&target, &workload, &knobs, &CostArgs::default())
            .map_err(|e| format!("{e:#}"))
    }

    fn start(&self, job: Arc<Job>, status: Status) {
        batch(|| {
            self.status.set(status);
            self.failure.set(None);
            self.notice.set(None);
        });
        self.last.replace(None);
        self.job.replace(Some(job));
    }

    fn reject(&self, error: String) {
        let hint = crate::analysis::hints::hint_for(&error, "s3").map(|h| (h.title, h.steps));
        batch(|| {
            self.failure.set(Some(Failure { error, hint }));
            self.status.set(Status::Failed);
        });
    }

    pub fn run(&self) {
        if self.status.get_untracked().busy() {
            return;
        }
        match self.build_profile() {
            Ok(p) => {
                batch(|| {
                    self.knobs.set(p.knobs.clone());
                    self.profile.set(Some(p.clone()));
                    self.snapshot.set(None);
                    self.report.set(None);
                    self.history.set(Vec::new());
                    self.log.set(vec![
                        format!("run {} · {}", p.target.display(), p.knobs.summary()).into(),
                    ]);
                });
                self.start(Job::run(p), Status::Running);
            }
            Err(e) => self.reject(e),
        }
    }

    pub fn tune(&self) {
        if self.status.get_untracked().busy() {
            return;
        }
        let objective = match self.objective.get_untracked().as_deref() {
            Some("latency") => Objective::Latency,
            Some("cost") => Objective::Cost,
            _ => Objective::Throughput,
        };
        let secs: f64 = self
            .trial_secs
            .get_untracked()
            .trim()
            .parse()
            .unwrap_or(4.0);
        let trials: usize = self.max_trials.get_untracked().trim().parse().unwrap_or(16);
        match self.build_profile() {
            Ok(p) => {
                batch(|| {
                    self.trials.set(Vec::new());
                    self.tuned.set(None);
                    self.log.set(vec![
                        format!("tuning {} for {}", p.target.display(), objective.label()).into(),
                    ]);
                });
                self.start(
                    Job::tune(
                        p,
                        objective,
                        Duration::from_secs_f64(secs.max(1.0)),
                        trials.max(3),
                    ),
                    Status::Tuning,
                );
            }
            Err(e) => self.reject(e),
        }
    }

    pub fn probe(&self) {
        if self.status.get_untracked().busy() {
            return;
        }
        let ep = self.probe_endpoint.get_untracked().trim().to_owned();
        if !(ep.starts_with("http://") || ep.starts_with("https://")) {
            self.reject("the endpoint must start with http:// or https://".into());
            return;
        }
        self.probe.set(None);
        self.start(Job::probe(ep, 5), Status::Probing);
    }

    pub fn stop(&self) {
        self.job.with(|j| {
            if let Some(j) = j {
                j.cancel();
            }
        });
        self.log
            .update(|l| l.push("stopping… test data will be cleaned up".into()));
    }

    /// Drain the running job into signals.
    fn pull(&self) {
        let Some(job) = self.job.with(|j| j.clone()) else {
            return;
        };
        let up = job.take();
        batch(|| {
            if let Some(s) = up.snapshot {
                if s.phase == crate::trace::Phase::Measure || s.phase == crate::trace::Phase::Done {
                    let (t, n) = (s.elapsed_s, s.summary.ops);
                    let prev = self.last.replace(Some((t, n)));
                    if let Some((t0, n0)) = prev
                        && t > t0 + 0.05
                        && n >= n0
                    {
                        let p99 = s
                            .timeline
                            .last()
                            .map(|p| p.p99_ms)
                            .unwrap_or(s.summary.latency.p99);
                        let ops = (n - n0) as f64 / (t - t0);
                        let mib = s.summary.mib_per_s * ops / s.summary.ops_per_s.max(1e-9);
                        self.history.update(|h| {
                            h.push(Sample { t, ops, mib, p99 });
                            if h.len() > 600 {
                                h.remove(0);
                            }
                        });
                    }
                }
                self.snapshot.set(Some(s));
            }
            if !up.log.is_empty() {
                self.log.update(|l| {
                    l.extend(up.log.into_iter().map(SharedString::from));
                    let n = l.len();
                    if n > 300 {
                        l.drain(..n - 300);
                    }
                });
            }
            if let Some(t) = up.trials {
                self.trials.set(t);
            }
            if let Some(o) = up.outcome {
                match o {
                    Outcome::Report(r) => {
                        self.snapshot.set(Some(r.result.clone()));
                        self.report.set(Some(*r));
                        self.status.set(Status::Done);
                    }
                    Outcome::Tuned {
                        report,
                        summary,
                        profile_toml,
                        rust,
                    } => {
                        self.knobs.set(report.knobs.clone());
                        self.snapshot.set(Some(report.result.clone()));
                        self.report.set(Some(*report));
                        self.tuned.set(Some(Tuned {
                            summary: *summary,
                            profile_toml,
                            rust,
                        }));
                        self.status.set(Status::Done);
                    }
                    Outcome::Probe(p) => {
                        self.probe.set(Some(*p));
                        self.status.set(Status::Done);
                    }
                    Outcome::Failed { error, hint } => {
                        self.failure.set(Some(Failure { error, hint }));
                        self.status.set(Status::Failed);
                    }
                }
                self.job.replace(None);
            }
        });
    }

    /// The snapshot to show: the finished report's, else the live one.
    pub fn current(&self) -> Option<Snapshot> {
        self.snapshot.get()
    }

    /// Diagnosis of what is shown (live runs are diagnosed continuously).
    pub fn diagnosis(&self) -> Option<Diagnosis> {
        if let Some(r) = self.report.get() {
            return Some(r.diagnosis);
        }
        let s = self.snapshot.get()?;
        let p = self.profile.get();
        let (sla, net) = p
            .map(|p| (p.sla.clone(), p.target.is_network()))
            .unwrap_or_default();
        Some(diagnose(&s, &self.knobs.get(), &sla, net))
    }

    /// Apply the tuned knobs to the form.
    pub fn apply_tuned(&self) {
        let k = self.knobs.get_untracked();
        batch(|| {
            self.form.concurrency.set(k.concurrency.to_string());
            self.form.io_concurrency.set(k.io_concurrency.to_string());
            self.form.chunk.set(k.chunk_size.to_string());
            self.form.cache.set(k.cache);
            self.form.keep_alive.set(k.keep_alive);
            self.notice
                .set(Some("Tuned settings applied to the workbench".into()));
            self.page.set(Page::Workbench);
        });
    }
}

pub fn store() -> Store {
    use_context::<Store>().expect("the root provides the store")
}
