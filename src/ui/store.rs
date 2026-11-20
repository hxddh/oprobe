//! App state: one `Copy` store of signals, made in the root and read through context.

use std::sync::Arc;
use std::time::Duration;

use crate::analysis::diagnosis::{Diagnosis, diagnose};
use crate::cli::args::{CostArgs, KnobArgs, TargetArgs, WorkloadArgs, tune_defaults};
use crate::cli::{TuneSettings, tune_options};
use crate::config::{Knobs, Preset, Profile, WorkloadSpec};
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
    /// Stop was pressed (or the app is quitting): the job is cleaning up its test data.
    Stopping,
    Done,
    Failed,
}

impl Status {
    pub fn busy(self) -> bool {
        matches!(
            self,
            Status::Running | Status::Tuning | Status::Probing | Status::Stopping
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "Ready",
            Status::Running => "Running",
            Status::Tuning => "Tuning",
            Status::Probing => "Probing",
            Status::Stopping => "Stopping",
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
    /// The winning configuration (what "Apply to workbench" applies).
    pub knobs: Knobs,
    /// The confirmation run of the winner (kept apart from the workbench's own results).
    pub report: Report,
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
    pub proxy: Signal<String>,
    /// `--ca-cert` from the command line (no field: a file path is set where oprobe starts).
    pub ca_cert: Signal<Option<std::path::PathBuf>>,
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
    /// Tuned settings applied from the Tune page that the form has no fields for
    /// (range gap, HTTP version, in-flight cap, cache size, tail cut).
    pub applied: Signal<Option<Knobs>>,
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
    /// What the current job is (Running / Tuning / Probing), also while it is stopping.
    kind: Stored<Option<Status>>,
    /// The user asked to quit while a job ran: quit once it has cleaned up.
    quit_pending: Stored<bool>,
    last: Stored<Option<(f64, u64)>>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        // The same defaults as the CLI (`oprobe run` / `oprobe tune` without flags), so the
        // numbers match.
        let wl = WorkloadSpec::default();
        let kn = Knobs::default();
        let s = Self {
            page: signal(Page::Workbench),
            form: Form {
                target: signal("memory://".to_owned()),
                endpoint: signal(String::new()),
                region: signal(String::new()),
                proxy: signal(String::new()),
                ca_cert: signal(None),
                existing: signal(String::new()),
                preset: signal(Some(SharedString::from(Preset::default().slug()))),
                duration: signal(wl.duration.to_string()),
                concurrency: signal(kn.concurrency.to_string()),
                io_concurrency: signal(kn.io_concurrency.to_string()),
                chunk: signal(kn.chunk_size.to_string()),
                anonymous: signal(false),
                insecure: signal(false),
                cache: signal(kn.cache),
                keep_alive: signal(kn.keep_alive),
                applied: signal(None),
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
            trial_secs: signal(tune_defaults::TRIAL_DURATION.to_owned()),
            max_trials: signal(tune_defaults::MAX_TRIALS.to_string()),
            trials: signal(Vec::new()),
            tuned: signal(None),
            probe_endpoint: signal("https://s3.us-east-1.amazonaws.com".to_owned()),
            probe: signal(None),
            notice: signal(None),
            job: stored(None),
            kind: stored(None),
            quit_pending: stored(false),
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
            if let Some(v) = l.region {
                s.form.region.set(v);
            }
            if let Some(v) = l.proxy {
                s.form.proxy.set(v);
            }
            s.form.ca_cert.set(l.ca_cert);
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
            ca_cert: f.ca_cert.get_untracked(),
            insecure: f.insecure.get_untracked(),
            proxy: opt(f.proxy),
            ..Default::default()
        };
        let workload = WorkloadArgs {
            preset: f.preset.get_untracked().map(|p| p.to_string()),
            duration: opt(f.duration),
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
            cache: f
                .cache
                .get_untracked()
                .then(|| Knobs::default().cache_size.to_string()),
            no_keep_alive: !f.keep_alive.get_untracked(),
            ..Default::default()
        };
        let mut p = crate::cli::build_profile(&target, &workload, &knobs, &CostArgs::default())
            .map_err(|e| format!("{e:#}"))?;
        if let Some(k) = f.applied.get_untracked() {
            p.knobs.range_gap = k.range_gap;
            p.knobs.max_inflight_requests = k.max_inflight_requests;
            p.knobs.http_version = k.http_version;
            p.knobs.cache_size = k.cache_size;
            p.knobs.tail_cut = k.tail_cut;
        }
        Ok(p)
    }

    fn start(&self, job: Arc<Job>, status: Status) {
        batch(|| {
            self.status.set(status);
            self.failure.set(None);
            self.notice.set(None);
        });
        self.last.replace(None);
        self.kind.replace(Some(status));
        self.job.replace(Some(job));
    }

    /// The kind of job that runs now (`None` when idle).
    pub fn job_kind(&self) -> Option<Status> {
        match self.status.get() {
            Status::Stopping => self.kind.with(|k| *k),
            st if st.busy() => Some(st),
            _ => None,
        }
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
        let trial = self.trial_secs.get_untracked();
        let max_trials = match self.max_trials.get_untracked().trim().parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                return self.reject(format!(
                    "Max trials must be a whole number (at least {})",
                    tune_defaults::MIN_TRIALS
                ));
            }
        };
        let p = match self.build_profile() {
            Ok(p) => p,
            Err(e) => return self.reject(e),
        };
        let settings = TuneSettings {
            trial_duration: trial.trim(),
            max_trials,
            ..TuneSettings::default()
        };
        let opts = match tune_options(objective, &settings, p.workload.rate) {
            Ok(o) => o,
            Err(e) => return self.reject(format!("{e:#}")),
        };
        batch(|| {
            self.trials.set(Vec::new());
            self.tuned.set(None);
            self.log.set(vec![
                format!("tuning {} for {}", p.target.display(), objective.label()).into(),
            ]);
        });
        self.start(Job::tune(p, opts), Status::Tuning);
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
        let st = self.status.get_untracked();
        if !st.busy() || st == Status::Stopping {
            return;
        }
        self.job.with(|j| {
            if let Some(j) = j {
                j.cancel();
            }
        });
        let probing = self.kind.with(|k| *k) == Some(Status::Probing);
        batch(|| {
            self.status.set(Status::Stopping);
            self.log.update(|l| {
                l.push(if probing {
                    "stopping…".into()
                } else {
                    "stopping… test data will be cleaned up".into()
                })
            });
        });
    }

    /// The user asked to quit (window close, Quit item, Cmd-Q): quit now when idle; otherwise
    /// stop the job, show that it is cleaning up, and quit when it has ([`Self::pull`]).
    pub fn request_quit(&self) {
        if self.job.with(|j| j.is_none()) {
            nsh_core::element::desktop::quit();
            return;
        }
        self.quit_pending.replace(true);
        self.stop();
        self.notice.set(Some(
            "Stopping and cleaning up test data before quitting…".into(),
        ));
    }

    /// The app is ending: stop this store's job and wait (bounded) for its cleanup.
    pub fn finish(&self) {
        if crate::ui::bridge::abandoned() {
            return;
        }
        if let Some(j) = self.job.with(|j| j.clone())
            && !j.is_done()
        {
            eprintln!("oprobe: stopping the running job and cleaning up its test data…");
            if !j.stop_and_wait(crate::ui::bridge::SHUTDOWN_TIMEOUT) {
                crate::ui::log_fatal(
                    "quit before the running job finished cleaning up; leftover test data \
                     (oprobe-run-*) can be removed with `oprobe cleanup`",
                );
            }
        }
    }

    /// Drain the running job into signals.
    fn pull(&self) {
        if self.quit_pending.with(|q| *q) && self.job.with(|j| j.is_none()) {
            self.quit_pending.replace(false);
            nsh_core::element::desktop::quit();
        }
        // The terminal host stops jobs itself when the user quits (Ctrl+C): show it.
        if crate::ui::bridge::quitting()
            && self.status.get_untracked().busy()
            && self.status.get_untracked() != Status::Stopping
        {
            batch(|| {
                self.status.set(Status::Stopping);
                self.notice.set(Some(
                    "Stopping and cleaning up test data before quitting… (Ctrl+C again to abort)"
                        .into(),
                ));
            });
        }
        let Some(job) = self.job.with(|j| j.clone()) else {
            return;
        };
        let up = job.take();
        // Tuning trials don't feed the workbench's charts.
        let tuning = self.kind.with(|k| *k) == Some(Status::Tuning);
        batch(|| {
            if let Some(s) = up.snapshot.filter(|_| !tuning) {
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
                    Outcome::Report { report, snapshot } => {
                        self.snapshot.set(Some(*snapshot));
                        self.report.set(Some(*report));
                        self.status.set(Status::Done);
                    }
                    Outcome::Tuned {
                        report,
                        knobs,
                        summary,
                        profile_toml,
                        rust,
                    } => {
                        // The workbench keeps its own run; the winner lives in `tuned`.
                        self.tuned.set(Some(Tuned {
                            knobs: *knobs,
                            report: *report,
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
                    // Stopped before there was anything to report (e.g. while preparing the
                    // dataset): that is what the user asked for, not an error.
                    Outcome::Failed { error, .. }
                        if self.status.get_untracked() == Status::Stopping
                            && error.contains("cancel") =>
                    {
                        self.log
                            .update(|l| l.push("stopped; test data cleaned up".into()));
                        self.status.set(Status::Idle);
                    }
                    Outcome::Failed { error, hint } => {
                        self.failure.set(Some(Failure { error, hint }));
                        self.status.set(Status::Failed);
                    }
                }
                self.job.replace(None);
                self.kind.replace(None);
            }
        });
    }

    /// The snapshot to show: the finished report's, else the live one.
    pub fn current(&self) -> Option<Snapshot> {
        self.snapshot.get()
    }

    /// Diagnosis of what is shown (live runs are diagnosed continuously).
    pub fn diagnosis(&self) -> Option<Diagnosis> {
        let s = self.snapshot.get()?;
        let p = self.profile.get();
        let (sla, net) = p
            .map(|p| (p.sla.clone(), p.target.is_network()))
            .unwrap_or_default();
        Some(diagnose(&s, &self.knobs.get(), &sla, net))
    }

    /// Apply the tuner's winning knobs to the workbench form.
    pub fn apply_tuned(&self) {
        let Some(t) = self.tuned.get_untracked() else {
            return;
        };
        let k = t.knobs;
        batch(|| {
            self.form.concurrency.set(k.concurrency.to_string());
            self.form.io_concurrency.set(k.io_concurrency.to_string());
            self.form.chunk.set(k.chunk_size.to_string());
            self.form.cache.set(k.cache);
            self.form.keep_alive.set(k.keep_alive);
            self.form.applied.set(Some(k.clone()));
            self.notice
                .set(Some("Tuned settings applied to the workbench".into()));
            self.page.set(Page::Workbench);
        });
    }
}

pub fn store() -> Store {
    use_context::<Store>().expect("the root provides the store")
}
