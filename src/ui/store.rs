//! App state: one `Copy` store of signals, made in the root and read through context. Its
//! methods are the only writers; engine work runs in `bridge` jobs and arrives by polling.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nsh_core::prelude::*;
use nsh_core::storage::Storage;

use crate::analysis::outliers::{Attribution, Selection, attribute};
use crate::model::Finding;
use crate::net::probe::ProbeReport;
use crate::store::{ConfigFile, Paths, RunStore, RunSummary};
use crate::trace::Snapshot;
use crate::tune::Trial;
use crate::ui::bridge::{self, Job, Outcome, RunRequest, TunedExtras, off_thread};
use crate::ui::kit::Tone;
use crate::ui::launcher::RunForm;
use crate::ui::model::compare::{Comparison, compare};
use crate::ui::model::requests::{Col, Filter, sort};
use crate::ui::model::run::{RunView, Shared};
use crate::ui::model::series::Series;
use crate::ui::model::{Prefs, Tab};

/// What the main area shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// First run: no runs in the store.
    #[default]
    Welcome,
    /// The run inspector (a stored run, or the live one).
    Run,
    Compare,
    Tune,
    Probe,
}

/// The run the inspector shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Sel {
    #[default]
    None,
    Live,
    Run(String),
}

/// What the job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Run,
    Tune,
    Probe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Running,
    /// Stop was pressed (or the app is quitting): cleaning up test data.
    Stopping,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub error: String,
    pub hint: Option<(String, Vec<String>)>,
}

/// The job running now (or the last one, until another starts).
#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    pub kind: JobKind,
    pub title: String,
    pub phase: Phase,
    pub total_s: f64,
    pub snapshot: Option<Snapshot>,
    pub series: Series,
    pub log: Vec<String>,
    pub trials: Vec<Trial>,
    pub failure: Option<Failure>,
    /// The run it produced.
    pub run_id: Option<String>,
}

/// What the inspector panel (right) shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Inspect {
    #[default]
    None,
    /// A request (index into the run's rows).
    Request(u32),
    /// An operation (index into the run's rows).
    Op(u32),
    /// Outlier attribution of a brushed selection.
    Attribution { what: String, attr: Attribution },
}

/// A heatmap/timeline selection: a time window and a latency floor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brush {
    pub from_s: f64,
    pub to_s: f64,
    pub min_ms: f64,
}

/// Overlays over the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    NewRun,
    Palette,
    Help,
    Settings,
    ConfirmQuit,
    /// The inspector panel over the content (narrow windows, the terminal).
    Inspector,
}

/// Request table rows: indices into the current run's rows (shared, compared by identity).
#[derive(Clone, Debug, Default)]
pub struct Rows(pub Arc<Vec<u32>>);

impl PartialEq for Rows {
    fn eq(&self, o: &Self) -> bool {
        Arc::ptr_eq(&self.0, &o.0)
    }
}

/// The New Run sheet's fields (kept here: the sheet's content is made each time it opens).
#[derive(Clone, Copy)]
pub struct Form {
    pub target: Signal<String>,
    pub preset: Signal<Option<SharedString>>,
    pub duration: Signal<String>,
    pub concurrency: Signal<String>,
    pub io_concurrency: Signal<String>,
    pub chunk: Signal<String>,
    pub endpoint: Signal<String>,
    pub region: Signal<String>,
    pub label: Signal<String>,
    pub anonymous: Signal<bool>,
    pub advanced: Signal<bool>,
    pub error: Signal<Option<String>>,
}

#[derive(Clone, Copy)]
pub struct Store {
    pub mode: Signal<Mode>,
    pub runs: Signal<Vec<RunSummary>>,
    pub targets: Signal<Vec<(String, String)>>,
    pub baselines: Signal<Vec<(String, Vec<String>)>>,
    pub sel: Signal<Sel>,
    pub current: Signal<Option<Shared>>,
    pub loading: Signal<bool>,
    /// Runs marked for comparison in the sidebar (at most two).
    pub marked: Signal<Vec<String>>,
    pub comparison: Signal<Option<(Shared, Shared, Comparison)>>,
    pub live: Signal<Option<Live>>,
    pub tab: Signal<Tab>,
    pub inspect: Signal<Inspect>,
    pub req_query: Signal<String>,
    pub req_sort: Signal<Option<(Col, bool)>>,
    pub req_rows: Memo<Rows>,
    pub req_focus: Signal<Option<u32>>,
    /// The timeline's visible window, seconds.
    pub tl_window: Signal<Option<(f64, f64)>>,
    pub brush: Signal<Option<Brush>>,
    pub log_latency: Signal<bool>,
    pub prefs: Signal<Prefs>,
    pub overlay: Signal<Option<Overlay>>,
    pub form: Form,
    pub toasts: Signal<Vec<(u64, SharedString, Tone)>>,
    pub focused: Signal<bool>,
    pub tuned: Signal<Option<TunedExtras>>,
    pub probe: Signal<Option<ProbeReport>>,
    pub probe_endpoint: Signal<String>,
    /// The Requests filter field (the terminal's `/` focuses it).
    pub filter_ref: ElementRef,
    pub tune_objective: Signal<Option<SharedString>>,
    pub tune_trial: Signal<String>,
    pub tune_max: Signal<String>,
    paths: Stored<Paths>,
    job: Stored<Option<Arc<Job>>>,
    quit_pending: Stored<bool>,
    next_toast: Stored<u64>,
    /// When the app started (cold-start timing).
    pub started: Stored<std::time::Instant>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

pub fn store() -> Store {
    use_context::<Store>().expect("the root provides the store")
}

fn load_prefs() -> Prefs {
    Prefs::from_json(Storage::get_item(Prefs::KEY).as_deref())
}

impl Store {
    pub fn new() -> Self {
        let started = std::time::Instant::now();
        let launch = crate::ui::launch();
        let paths = launch.paths.clone().unwrap_or_else(|| Paths::resolve(None));
        let prefs = load_prefs();
        let req_query = signal(String::new());
        let req_sort = signal(None::<(Col, bool)>);
        let current = signal(None::<Shared>);
        let req_rows = computed(move || {
            let q = req_query.get();
            let s = req_sort.get();
            current.with(|c| {
                let Some(v) = c else { return Rows::default() };
                let f = Filter::parse(&q);
                let mut idx = if f.is_empty() {
                    (0..v.run.reqs.len() as u32).collect()
                } else {
                    f.apply(&v.run.reqs)
                };
                if let Some((col, desc)) = s {
                    sort(&mut idx, &v.run.reqs, col, desc);
                }
                Rows(Arc::new(idx))
            })
        });
        let form = Form {
            target: signal(
                launch
                    .target
                    .clone()
                    .unwrap_or_else(|| prefs.recent_targets.first().cloned().unwrap_or_default()),
            ),
            preset: signal(Some(
                launch
                    .preset
                    .clone()
                    .unwrap_or_else(|| prefs.preset.clone())
                    .into(),
            )),
            duration: signal(
                launch
                    .duration
                    .clone()
                    .unwrap_or_else(|| prefs.duration.clone()),
            ),
            concurrency: signal(
                launch
                    .concurrency
                    .clone()
                    .unwrap_or_else(|| prefs.concurrency.clone()),
            ),
            io_concurrency: signal(String::new()),
            chunk: signal(String::new()),
            endpoint: signal(launch.endpoint.clone().unwrap_or_default()),
            region: signal(launch.region.clone().unwrap_or_default()),
            label: signal(String::new()),
            anonymous: signal(launch.anonymous),
            advanced: signal(false),
            error: signal(None),
        };
        let s = Self {
            mode: signal(Mode::Welcome),
            runs: signal(Vec::new()),
            targets: signal(Vec::new()),
            baselines: signal(Vec::new()),
            sel: signal(Sel::None),
            current,
            loading: signal(false),
            marked: signal(Vec::new()),
            comparison: signal(None),
            live: signal(None),
            tab: signal(prefs.tab),
            inspect: signal(Inspect::None),
            req_query,
            req_sort,
            req_rows,
            req_focus: signal(None),
            tl_window: signal(None),
            brush: signal(None),
            log_latency: signal(prefs.log_latency),
            prefs: signal(prefs),
            overlay: signal(None),
            form,
            toasts: signal(Vec::new()),
            focused: signal(true),
            tuned: signal(None),
            probe: signal(None),
            probe_endpoint: signal(String::new()),
            filter_ref: element_ref(),
            tune_objective: signal(Some("throughput".into())),
            tune_trial: signal(crate::cli::args::tune_defaults::TRIAL_DURATION.to_owned()),
            tune_max: signal("8".to_owned()),
            paths: stored(paths),
            job: stored(None),
            quit_pending: stored(false),
            next_toast: stored(1),
            started: stored(started),
        };
        s.refresh();
        // Open the newest run (or the welcome view when there is none).
        if let Some(first) = s.runs.with_untracked(|r| r.first().map(|r| r.id.clone())) {
            s.select_run(&first);
        }
        // A target given on the command line: straight to the New Run sheet.
        if launch.target.is_some() {
            s.overlay.set(Some(Overlay::NewRun));
        }
        set_interval(Duration::from_millis(120), move || s.pull());
        s
    }

    pub fn run_store(&self) -> RunStore {
        self.paths.with(|p| p.run_store())
    }

    fn config(&self) -> ConfigFile {
        self.paths
            .with(|p| ConfigFile::load(&p.config_file))
            .unwrap_or_default()
    }

    /// Reload the run history, targets and baselines from disk.
    pub fn refresh(&self) {
        let runs = self.run_store().list().unwrap_or_default();
        let cfg = self.config();
        let targets = cfg
            .targets
            .iter()
            .map(|(n, t)| (n.clone(), t.url.clone()))
            .collect();
        let baselines = self
            .paths
            .with(|p| crate::store::Baselines::load(&p.baselines_file()))
            .map(|b| b.baselines.into_iter().map(|(n, b)| (n, b.runs)).collect())
            .unwrap_or_default();
        batch(|| {
            self.runs.set(runs);
            self.targets.set(targets);
            self.baselines.set(baselines);
        });
    }

    // -----------------------------------------------------------------------------------
    // Preferences
    // -----------------------------------------------------------------------------------

    pub fn update_prefs(&self, f: impl FnOnce(&mut Prefs)) {
        let mut p = self.prefs.get_untracked();
        f(&mut p);
        let _ = Storage::set_item(Prefs::KEY, p.to_json());
        self.prefs.set(p);
    }

    pub fn set_tab(&self, t: Tab) {
        if self.mode.get_untracked() != Mode::Run && self.mode.get_untracked() != Mode::Welcome {
            self.mode.set(Mode::Run);
        }
        self.tab.set(t);
        if self.prefs.with_untracked(|p| p.tab != t) {
            self.update_prefs(|p| p.tab = t);
        }
    }

    pub fn toggle_sidebar(&self) {
        self.update_prefs(|p| p.sidebar_visible = !p.sidebar_visible);
    }

    pub fn toggle_inspector(&self) {
        self.update_prefs(|p| p.inspector_visible = !p.inspector_visible);
    }

    pub fn set_log_latency(&self, on: bool) {
        self.log_latency.set(on);
        self.update_prefs(|p| p.log_latency = on);
    }

    // -----------------------------------------------------------------------------------
    // Toasts
    // -----------------------------------------------------------------------------------

    pub fn toast(&self, msg: impl Into<SharedString>, tone: Tone) {
        let id = self.next_toast.with(|n| {
            *n += 1;
            *n
        });
        let msg = msg.into();
        self.toasts.update(|t| {
            t.push((id, msg, tone));
            if t.len() > 3 {
                t.remove(0);
            }
        });
        let toasts = self.toasts;
        set_timeout(Duration::from_secs(5), move || {
            toasts.update(|t| t.retain(|(i, ..)| *i != id));
        });
    }

    // -----------------------------------------------------------------------------------
    // Selection
    // -----------------------------------------------------------------------------------

    /// Show a stored run.
    pub fn select_run(&self, id: &str) {
        if self.sel.get_untracked() == Sel::Run(id.to_owned())
            && self
                .current
                .with_untracked(|c| c.as_ref().is_some_and(|c| c.id() == id))
        {
            self.mode.set(Mode::Run);
            return;
        }
        batch(|| {
            self.sel.set(Sel::Run(id.to_owned()));
            self.mode.set(Mode::Run);
            self.inspect.set(Inspect::None);
            self.brush.set(None);
            self.tl_window.set(None);
            self.req_focus.set(None);
            self.loading.set(true);
        });
        let store = self.run_store();
        let id = id.to_owned();
        let s = *self;
        spawn(async move {
            let id2 = id.clone();
            let res = off_thread(move || {
                let path = store.resolve(&id2)?;
                let run = crate::store::read_run_file(&path)?;
                anyhow::Ok(RunView::new(run, Some(path)))
            })
            .await;
            if s.sel.get_untracked() != Sel::Run(id.clone()) {
                return;
            }
            match res {
                Some(Ok(v)) => batch(|| {
                    s.current.set(Some(Shared(Arc::new(v))));
                    s.loading.set(false);
                }),
                Some(Err(e)) => {
                    s.loading.set(false);
                    s.toast(format!("cannot open run {id}: {e:#}"), Tone::Crit);
                }
                None => s.loading.set(false),
            }
        });
    }

    /// Show the job that runs now.
    pub fn select_live(&self) {
        batch(|| {
            self.sel.set(Sel::Live);
            self.mode.set(Mode::Run);
            self.current.set(None);
            self.inspect.set(Inspect::None);
            self.tab.set(Tab::Overview);
        });
    }

    /// Mark or unmark a run for comparison (at most two; the oldest mark gives way).
    pub fn toggle_mark(&self, id: &str) {
        self.marked.update(|m| {
            if let Some(i) = m.iter().position(|x| x == id) {
                m.remove(i);
            } else {
                m.push(id.to_owned());
                if m.len() > 2 {
                    m.remove(0);
                }
            }
        });
    }

    /// Open a `.oprobe` (or report `.json`) file: imported into the history, then shown.
    pub fn open_path(&self, path: PathBuf) {
        let store = self.run_store();
        let s = *self;
        spawn(async move {
            let shown = path.display().to_string();
            let res = off_thread(move || {
                let run = crate::store::read_run_file(&path)?;
                let id = run.id().to_owned();
                if store.resolve(&id).is_err() {
                    store.save(&run)?;
                }
                anyhow::Ok(id)
            })
            .await;
            match res {
                Some(Ok(id)) => {
                    s.refresh();
                    s.select_run(&id);
                    s.toast(format!("Opened {shown}"), Tone::Info);
                }
                Some(Err(e)) => s.toast(format!("cannot open {shown}: {e:#}"), Tone::Crit),
                None => {}
            }
        });
    }

    /// Delete a stored run.
    pub fn delete_run(&self, id: &str) {
        match self.run_store().delete(id) {
            Ok(()) => {
                if self.sel.get_untracked() == Sel::Run(id.to_owned()) {
                    batch(|| {
                        self.sel.set(Sel::None);
                        self.current.set(None);
                    });
                }
                self.marked.update(|m| m.retain(|x| x != id));
                self.refresh();
                if self.runs.with_untracked(|r| r.is_empty())
                    && self.live.with_untracked(|l| l.is_none())
                {
                    self.mode.set(Mode::Welcome);
                }
                self.toast(format!("Deleted run {id}"), Tone::Neutral);
            }
            Err(e) => self.toast(format!("cannot delete {id}: {e:#}"), Tone::Crit),
        }
    }

    // -----------------------------------------------------------------------------------
    // Jobs
    // -----------------------------------------------------------------------------------

    pub fn busy(&self) -> bool {
        self.live.with(|l| {
            l.as_ref()
                .is_some_and(|l| matches!(l.phase, Phase::Running | Phase::Stopping))
        })
    }

    fn trace_max(&self) -> usize {
        std::env::var("OPROBE_MAX_REQUESTS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .or_else(|| self.config().defaults.max_requests)
            .unwrap_or(crate::trace::DEFAULT_MAX_REQUESTS)
    }

    fn begin(&self, job: Arc<Job>, kind: JobKind, title: String, total_s: f64) {
        self.job.replace(Some(job));
        batch(|| {
            self.live.set(Some(Live {
                kind,
                title,
                phase: Phase::Running,
                total_s,
                snapshot: None,
                series: Series::default(),
                log: Vec::new(),
                trials: Vec::new(),
                failure: None,
                run_id: None,
            }));
            if kind == JobKind::Run {
                self.select_live();
            }
        });
        main_progress(Some(0.0));
    }

    /// Start a run from a form; returns the error to show in the form.
    pub fn start_form(&self, form: RunForm) -> Result<(), String> {
        if self.busy() {
            return Err("a run is in progress: stop it first".into());
        }
        let req = form.prepare(&self.config(), Some(self.run_store()), self.trace_max())?;
        self.update_prefs(|p| {
            p.push_recent(&form.target);
            if !form.preset.trim().is_empty() {
                p.preset = form.preset.trim().to_owned();
            }
        });
        self.start_request(req);
        Ok(())
    }

    pub fn start_request(&self, req: RunRequest) {
        let title = req
            .display
            .clone()
            .unwrap_or_else(|| req.profile.target.display());
        let total = req.profile.workload.duration.get().as_secs_f64();
        self.begin(Job::run(req), JobKind::Run, title, total);
    }

    /// The New Run sheet's Start.
    pub fn submit_form(&self) {
        let f = self.form;
        let form = RunForm {
            target: f.target.get_untracked(),
            preset: f
                .preset
                .get_untracked()
                .map(|p| p.to_string())
                .unwrap_or_default(),
            duration: f.duration.get_untracked(),
            concurrency: f.concurrency.get_untracked(),
            io_concurrency: f.io_concurrency.get_untracked(),
            chunk: f.chunk.get_untracked(),
            endpoint: f.endpoint.get_untracked(),
            region: f.region.get_untracked(),
            label: f.label.get_untracked(),
            anonymous: f.anonymous.get_untracked(),
            knobs: None,
        };
        if form.target.trim().is_empty() {
            f.error.set(Some(
                "Choose a target: a storage URL, a configured target name, or lab://demo.".into(),
            ));
            return;
        }
        match self.start_form(form) {
            Ok(()) => batch(|| {
                f.error.set(None);
                self.overlay.set(None);
            }),
            Err(e) => f.error.set(Some(e)),
        }
    }

    /// "Try the demo": the lab's demo scenario (throttling and stragglers), no credentials.
    pub fn demo(&self) {
        let duration = crate::ui::launch()
            .duration
            .unwrap_or_else(|| "8s".to_owned());
        let form = RunForm {
            target: "lab://demo".into(),
            preset: "mixed".into(),
            duration,
            label: "demo".into(),
            ..Default::default()
        };
        if let Err(e) = self.start_form(form) {
            self.toast(e, Tone::Crit);
        }
    }

    /// Repeat the shown run (target, workload, knobs).
    pub fn rerun(&self) {
        let form = self
            .current
            .with_untracked(|c| c.as_ref().map(|c| RunForm::from_report(&c.run.report)));
        match form {
            Some(f) => {
                if let Err(e) = self.start_form(f) {
                    self.toast(e, Tone::Crit);
                }
            }
            None => self.open(Overlay::NewRun),
        }
    }

    /// Run a finding's verification command as a new run.
    pub fn verify(&self, f: &Finding) {
        let Some(cmd) = f.verification.as_ref().and_then(|v| v.command.clone()) else {
            return;
        };
        let form = self.current.with_untracked(|c| {
            c.as_ref()
                .map(|c| RunForm::from_command(&cmd, &c.run.report))
        });
        match form {
            Some(Ok(form)) => {
                if let Err(e) = self.start_form(form) {
                    self.toast(e, Tone::Crit);
                }
            }
            Some(Err(e)) => self.toast(e, Tone::Crit),
            None => {}
        }
    }

    /// Run the tuner on the shown run's target.
    pub fn start_tune(&self) {
        if self.busy() {
            self.toast("a job is in progress: stop it first", Tone::Warn);
            return;
        }
        let form = self
            .current
            .with_untracked(|c| c.as_ref().map(|c| RunForm::from_report(&c.run.report)))
            .unwrap_or_else(|| RunForm {
                target: self.form.target.get_untracked(),
                ..Default::default()
            });
        let objective = match self.tune_objective.get_untracked().as_deref() {
            Some("latency") => crate::tune::Objective::Latency,
            Some("cost") => crate::tune::Objective::Cost,
            _ => crate::tune::Objective::Throughput,
        };
        let trial = self.tune_trial.get_untracked();
        let max: usize = match self.tune_max.get_untracked().trim().parse() {
            Ok(n) => n,
            Err(_) => {
                self.toast("Max trials must be a whole number", Tone::Crit);
                return;
            }
        };
        let settings = crate::cli::TuneSettings {
            trial_duration: &trial,
            trial_warmup: "0s",
            max_trials: max,
            cooldown: "1s",
            ..Default::default()
        };
        let opts = match crate::cli::tune_options(objective, &settings, None) {
            Ok(o) => o,
            Err(e) => {
                self.toast(format!("{e:#}"), Tone::Crit);
                return;
            }
        };
        let req = match form.prepare(&self.config(), None, 0) {
            Ok(r) => r,
            Err(e) => {
                self.toast(e, Tone::Crit);
                return;
            }
        };
        if req.lab.is_some() {
            self.toast(
                "the tuner needs a real target (or `oprobe lab` + its URL)",
                Tone::Warn,
            );
            return;
        }
        let title = req.profile.target.display();
        let job = Job::tune(
            req.profile,
            req.target_name,
            opts,
            req.budgets,
            Some(self.run_store()),
        );
        self.tuned.set(None);
        self.begin(job, JobKind::Tune, title, 0.0);
        self.mode.set(Mode::Tune);
    }

    /// Probe an endpoint's handshakes.
    pub fn start_probe(&self) {
        if self.busy() {
            self.toast("a job is in progress: stop it first", Tone::Warn);
            return;
        }
        let ep = self.probe_endpoint.get_untracked().trim().to_owned();
        if ep.is_empty() {
            self.toast(
                "Enter an endpoint, e.g. https://s3.amazonaws.com",
                Tone::Warn,
            );
            return;
        }
        self.probe.set(None);
        self.begin(Job::probe(ep.clone(), 5), JobKind::Probe, ep, 0.0);
        self.mode.set(Mode::Probe);
    }

    /// Stop the job; its test data is still cleaned up.
    pub fn stop(&self) {
        if let Some(j) = self.job.with(|j| j.clone()) {
            j.cancel();
            self.live.update(|l| {
                if let Some(l) = l
                    && l.phase == Phase::Running
                {
                    l.phase = Phase::Stopping;
                }
            });
        }
    }

    /// The user asked to quit: with a job running, stop it, clean up, then quit.
    pub fn request_quit(&self) {
        if self.job.with(|j| j.is_none()) {
            nsh_core::element::desktop::quit();
            return;
        }
        self.quit_pending.replace(true);
        self.stop();
        self.toast(
            "Stopping and cleaning up test data before quitting…",
            Tone::Warn,
        );
    }

    /// The terminal's `q`: confirm first while a job runs.
    pub fn quit_or_confirm(&self) {
        if self.busy() {
            self.overlay.set(Some(Overlay::ConfirmQuit));
        } else {
            self.request_quit();
        }
    }

    /// The app is ending: stop this store's job and wait (bounded) for its cleanup.
    pub fn finish(&self) {
        if bridge::abandoned() {
            return;
        }
        if let Some(j) = self.job.with(|j| j.clone())
            && !j.is_done()
        {
            eprintln!("oprobe: stopping the running job and cleaning up its test data…");
            if !j.stop_and_wait(bridge::SHUTDOWN_TIMEOUT) {
                crate::ui::log_fatal(
                    "quit before the running job finished cleaning up; leftover test data \
                     (oprobe-run-*) can be removed with `oprobe cleanup`",
                );
            }
        }
    }

    pub fn open(&self, o: Overlay) {
        self.overlay.set(Some(o));
    }

    /// Poll the job (the UI only reads its snapshots).
    fn pull(&self) {
        if self.quit_pending.with(|q| *q) && self.job.with(|j| j.is_none()) {
            self.quit_pending.replace(false);
            nsh_core::element::desktop::quit();
        }
        // The terminal host stops jobs itself when the user quits (Ctrl+C): show it.
        if bridge::quitting() {
            self.live.update(|l| {
                if let Some(l) = l
                    && l.phase == Phase::Running
                {
                    l.phase = Phase::Stopping;
                }
            });
        }
        let Some(job) = self.job.with(|j| j.clone()) else {
            return;
        };
        let up = job.take();
        if up.snapshot.is_none() && up.log.is_empty() && up.trials.is_none() && up.outcome.is_none()
        {
            return;
        }
        let mut progress = None;
        self.live.update(|l| {
            let Some(l) = l else { return };
            if let Some(snap) = up.snapshot {
                l.series = Series::from_timeline(&snap.timeline, snap.elapsed_s);
                if l.total_s > 0.0 {
                    progress = Some((snap.elapsed_s / l.total_s).clamp(0.0, 1.0));
                }
                l.snapshot = Some(snap);
            }
            l.log.extend(up.log);
            if l.log.len() > 200 {
                let cut = l.log.len() - 200;
                l.log.drain(..cut);
            }
            if let Some(t) = up.trials {
                l.trials = t;
            }
        });
        if let Some(p) = progress {
            main_progress(Some(p as f32));
        }
        let Some(outcome) = up.outcome else { return };
        self.job.replace(None);
        main_progress(None);
        match outcome {
            Outcome::Run { run, path, tuned } => {
                let id = run.id().to_owned();
                let headline = run.report.verdict.headline.clone();
                let kind = self.live.with_untracked(|l| l.as_ref().map(|l| l.kind));
                self.live.update(|l| {
                    if let Some(l) = l {
                        l.phase = Phase::Done;
                        l.run_id = Some(id.clone());
                    }
                });
                if let Some(t) = tuned {
                    self.tuned.set(Some(*t));
                }
                self.refresh();
                let view = RunView::new(*run, path);
                let showing_live = self.sel.get_untracked() == Sel::Live;
                if showing_live
                    || kind == Some(JobKind::Tune) && self.mode.get_untracked() != Mode::Tune
                {
                    batch(|| {
                        self.sel.set(Sel::Run(id.clone()));
                        self.current.set(Some(Shared(Arc::new(view))));
                        self.loading.set(false);
                    });
                }
                self.finished_in_background(&format!("Run {id} finished"), &headline);
                self.toast(format!("Run {id} finished · {headline}"), Tone::Ok);
            }
            Outcome::Probe(r) => {
                self.live.update(|l| {
                    if let Some(l) = l {
                        l.phase = Phase::Done;
                    }
                });
                self.probe.set(Some(*r));
            }
            Outcome::Failed { error, hint } => {
                let stopped = self
                    .live
                    .with_untracked(|l| l.as_ref().is_some_and(|l| l.phase == Phase::Stopping));
                self.live.update(|l| {
                    if let Some(l) = l {
                        l.phase = Phase::Failed;
                        l.failure = Some(Failure {
                            error: error.clone(),
                            hint,
                        });
                    }
                });
                if !stopped {
                    self.finished_in_background("Run failed", &error);
                }
                let first = error.lines().next().unwrap_or("").to_owned();
                self.toast(
                    if stopped {
                        "Stopped".to_owned()
                    } else {
                        format!("Failed: {first}")
                    },
                    if stopped { Tone::Neutral } else { Tone::Crit },
                );
            }
        }
    }

    /// Notification and badge when the window is not in front.
    fn finished_in_background(&self, title: &str, body: &str) {
        if self.focused.get_untracked() || kit_in_terminal() {
            return;
        }
        set_badge(1);
        if self.prefs.with_untracked(|p| p.notify) {
            let n = nsh_core::notifications::Notification::new("run-finished", title, body);
            spawn(async move {
                let _ = show_notification(n).await;
            });
        }
    }

    /// The window came to the front: the badge has been seen.
    pub fn window_focused(&self, on: bool) {
        self.focused.set(on);
        if on && !kit_in_terminal() {
            set_badge(0);
        }
    }

    // -----------------------------------------------------------------------------------
    // Analysis
    // -----------------------------------------------------------------------------------

    /// Attribute a brushed selection and show it in the inspector.
    pub fn apply_brush(&self, b: Brush) {
        self.brush.set(Some(b));
        let attr = self.current.with_untracked(|c| {
            c.as_ref().map(|c| {
                let sel = Selection {
                    from_us: Some((b.from_s * 1e6) as i64),
                    to_us: Some((b.to_s * 1e6) as i64),
                    min_total_us: (b.min_ms > 0.0).then_some((b.min_ms * 1e3) as u64),
                    api: None,
                };
                attribute(&c.run.reqs, &sel)
            })
        });
        if let Some(attr) = attr {
            let what = format!(
                "{} requests between {} and {}{}",
                attr.selected_count,
                crate::ui::model::fmt::clock_s(b.from_s),
                crate::ui::model::fmt::clock_s(b.to_s),
                if b.min_ms > 0.0 {
                    format!(" slower than {}", crate::ui::model::fmt::dur_ms(b.min_ms))
                } else {
                    String::new()
                }
            );
            batch(|| {
                self.inspect.set(Inspect::Attribution { what, attr });
            });
            if !self.prefs.with_untracked(|p| p.inspector_visible) {
                self.update_prefs(|p| p.inspector_visible = true);
            }
        }
    }

    /// Attribution of the slowest 1 % of requests.
    pub fn explain_slowest(&self) {
        let attr = self.current.with_untracked(|c| {
            c.as_ref()
                .map(|c| attribute(&c.run.reqs, &Selection::slowest(&c.run.reqs, 1.0)))
        });
        if let Some(attr) = attr {
            self.inspect.set(Inspect::Attribution {
                what: format!("the slowest 1 % ({} requests)", attr.selected_count),
                attr,
            });
        }
    }

    /// Compare two stored runs (A = baseline, B = candidate).
    pub fn compare(&self, a: &str, b: &str) {
        let store = self.run_store();
        let (a, b) = (a.to_owned(), b.to_owned());
        let s = *self;
        self.mode.set(Mode::Compare);
        self.comparison.set(None);
        spawn(async move {
            let res = off_thread(move || {
                let load = |id: &str| -> anyhow::Result<RunView> {
                    let path = store.resolve(id)?;
                    Ok(RunView::new(
                        crate::store::read_run_file(&path)?,
                        Some(path),
                    ))
                };
                let (va, vb) = (load(&a)?, load(&b)?);
                let c = compare(&va.run.report, &vb.run.report);
                anyhow::Ok((va, vb, c))
            })
            .await;
            match res {
                Some(Ok((va, vb, c))) => {
                    s.comparison
                        .set(Some((Shared(Arc::new(va)), Shared(Arc::new(vb)), c)))
                }
                Some(Err(e)) => s.toast(format!("cannot compare: {e:#}"), Tone::Crit),
                None => {}
            }
        });
    }

    /// Compare the marked runs, or the shown run with the one before it.
    pub fn compare_default(&self) {
        let marked = self.marked.get_untracked();
        if marked.len() == 2 {
            // Oldest first: ids sort by time.
            let (mut a, mut b) = (marked[0].clone(), marked[1].clone());
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            self.compare(&a, &b);
            return;
        }
        let runs = self.runs.get_untracked();
        let cur = match self.sel.get_untracked() {
            Sel::Run(id) => Some(id),
            _ => runs.first().map(|r| r.id.clone()),
        };
        let Some(cur) = cur else {
            self.toast("Compare needs two runs", Tone::Warn);
            return;
        };
        let pos = runs.iter().position(|r| r.id == cur).unwrap_or(0);
        match runs.get(pos + 1) {
            Some(prev) => self.compare(&prev.id.clone(), &cur),
            None => self.toast(
                "Compare needs two runs: mark two in the sidebar",
                Tone::Warn,
            ),
        }
    }

    /// The live elapsed/total in seconds for the toolbar.
    pub fn live_progress(&self) -> Option<(f64, f64)> {
        self.live.with(|l| {
            l.as_ref().filter(|l| l.phase == Phase::Running).map(|l| {
                (
                    l.snapshot.as_ref().map(|s| s.elapsed_s).unwrap_or(0.0),
                    l.total_s,
                )
            })
        })
    }

    pub fn cold_start_ms(&self) -> f64 {
        self.started.with(|s| s.elapsed().as_secs_f64() * 1000.0)
    }
}

fn kit_in_terminal() -> bool {
    crate::ui::kit::in_terminal()
}

/// Dock / taskbar progress of the main window (`None` clears it).
fn main_progress(value: Option<f32>) {
    if kit_in_terminal() {
        return;
    }
    use nsh_core::desktop::{ProgressState, WindowAttributes, WindowProgress};
    let mut progress = WindowProgress::default();
    progress.value = value.map(percentage);
    progress.state = ProgressState::Normal;
    let mut attrs = WindowAttributes::default();
    attrs.progress = Some(value.map(|_| progress));
    nsh_core::element::desktop::main_window().set(attrs);
}
