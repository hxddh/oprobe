//! App state: one `Copy` store of signals, made in the root and read through context. Its
//! methods are the only writers; engine work runs in `bridge` jobs and arrives by polling.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nsh_core::prelude::*;
use nsh_core::storage::Storage;

use crate::analysis::outliers::{Attribution, Selection, attribute};
use crate::model::Finding;
use crate::model::spec::{Overrides, RunSpec};
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
use crate::ui::model::target::TargetDraft;
use crate::ui::model::verify::Verdict;
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

/// Where a running job is (design v4 §11: progress per phase).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    /// Before the engine reports anything (resolving the target, the preflight).
    #[default]
    Starting,
    /// Uploading the dataset.
    Preparing,
    Warmup,
    Measuring,
    /// Verifying what was written.
    Verifying,
    /// Removing test data.
    Cleanup,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Starting => "starting",
            Stage::Preparing => "preparing",
            Stage::Warmup => "warmup",
            Stage::Measuring => "measuring",
            Stage::Verifying => "verifying",
            Stage::Cleanup => "cleanup",
        }
    }

    /// The stages a run goes through, in order (the live view's steps).
    pub const STEPS: [Stage; 4] = [
        Stage::Preparing,
        Stage::Warmup,
        Stage::Measuring,
        Stage::Cleanup,
    ];

    fn rank(self) -> u8 {
        match self {
            Stage::Starting => 0,
            Stage::Preparing => 1,
            Stage::Warmup => 2,
            Stage::Measuring => 3,
            Stage::Verifying => 4,
            Stage::Cleanup => 5,
        }
    }

    /// Whether this stage is behind `now` (a step already done).
    pub fn before(self, now: Stage) -> bool {
        self.rank() < now.rank()
    }

    fn of(p: crate::trace::Phase) -> Stage {
        use crate::trace::Phase as P;
        match p {
            P::Idle => Stage::Starting,
            P::Preparing => Stage::Preparing,
            P::Warmup => Stage::Warmup,
            P::Measure => Stage::Measuring,
            P::Verify => Stage::Verifying,
            P::Cleanup | P::Done => Stage::Cleanup,
        }
    }
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
    /// Where the job is (from the engine's snapshots and its cleanup stage).
    pub stage: Stage,
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
    /// The Add Target sheet.
    AddTarget,
    /// Pick a stored run to open (the palette's list of runs).
    PickRun,
    /// Pick a configured target for the next run.
    PickTarget,
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
    pub stored_gib: Signal<String>,
    pub anonymous: Signal<bool>,
    pub advanced: Signal<bool>,
    pub error: Signal<Option<String>>,
    /// The run the sheet repeats (Edit and Run Again): its spec, which the fields edit.
    pub spec: Signal<Option<RunSpec>>,
    /// The id of that run.
    pub spec_run: Signal<Option<String>>,
}

/// The Add Target sheet's fields.
#[derive(Clone, Copy)]
pub struct AddForm {
    pub name: Signal<String>,
    pub url: Signal<String>,
    pub region: Signal<String>,
    pub endpoint: Signal<String>,
    /// A [`crate::ui::model::target::CredChoice`] value (`""`: the default chain).
    pub credentials: Signal<Option<SharedString>>,
    pub error: Signal<Option<String>>,
    /// The connection test's result (tone and message), or `None` before one.
    pub test: Signal<Option<(Tone, String)>>,
    pub testing: Signal<bool>,
}

impl AddForm {
    pub fn draft(&self) -> TargetDraft {
        TargetDraft {
            name: self.name.get_untracked(),
            url: self.url.get_untracked(),
            region: self.region.get_untracked(),
            endpoint: self.endpoint.get_untracked(),
            credentials: self
                .credentials
                .get_untracked()
                .map(|c| c.to_string())
                .unwrap_or_default(),
        }
    }

    pub fn fill(&self, d: &TargetDraft) {
        batch(|| {
            self.name.set(d.name.clone());
            self.url.set(d.url.clone());
            self.region.set(d.region.clone());
            self.endpoint.set(d.endpoint.clone());
            self.credentials.set(Some(d.credentials.clone().into()));
            self.error.set(None);
            self.test.set(None);
        });
    }
}

/// A Verify in flight: when its run lands, it is compared with the run it verifies.
#[derive(Clone, Debug, PartialEq)]
struct PendingVerify {
    base_id: String,
    code: String,
    expect: Vec<crate::analysis::verification::Expectation>,
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
    /// The spec the tuner started from (what its winner is applied to).
    pub tune_base: Signal<Option<(String, RunSpec)>>,
    pub add: AddForm,
    /// The sidebar's run search and how many runs it lists.
    pub run_query: Signal<String>,
    pub run_limit: Signal<usize>,
    /// The verdict of the last Verify (shown over its comparison).
    pub verdict: Signal<Option<Verdict>>,
    /// What Compare shows now, side A and side B (set at once, before the comparison has
    /// loaded; Compare opened without a pair has only B).
    pub compare_pick: Signal<(Option<String>, Option<String>)>,
    /// A comparison is loading.
    pub comparing: Signal<bool>,
    pending_verify: Stored<Option<PendingVerify>>,
    /// Bumped by the run-directory watcher when the directory changes.
    runs_changed: Stored<Arc<std::sync::atomic::AtomicU64>>,
    runs_seen: Stored<u64>,
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
            stored_gib: signal(String::new()),
            anonymous: signal(launch.anonymous),
            advanced: signal(false),
            error: signal(None),
            spec: signal(None),
            spec_run: signal(None),
        };
        let add = AddForm {
            name: signal(String::new()),
            url: signal(String::new()),
            region: signal(String::new()),
            endpoint: signal(String::new()),
            credentials: signal(Some(SharedString::default())),
            error: signal(None),
            test: signal(None),
            testing: signal(false),
        };
        let tune = crate::config::defaults().tune;
        let runs_changed = watch_dir(paths.run_store().dir().to_path_buf());
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
            tune_trial: signal(tune.trial_duration.to_string()),
            tune_max: signal(tune.max_trials.to_string()),
            tune_base: signal(None),
            add,
            run_query: signal(String::new()),
            run_limit: signal(crate::ui::model::runs::PAGE),
            verdict: signal(None),
            compare_pick: signal((None, None)),
            comparing: signal(false),
            pending_verify: stored(None),
            runs_seen: stored(runs_changed.load(std::sync::atomic::Ordering::Relaxed)),
            runs_changed: stored(runs_changed),
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
                stage: Stage::Starting,
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
            if form.spec.is_none() && !form.preset.trim().is_empty() {
                p.preset = form.preset.trim().to_owned();
            }
        });
        self.start_request(req);
        Ok(())
    }

    pub fn start_request(&self, req: RunRequest) {
        let title = req
            .target_name
            .clone()
            .or_else(|| req.profile.target.url.clone())
            .unwrap_or_else(|| req.profile.target.display());
        let total = req.profile.workload.duration.get().as_secs_f64();
        self.begin(Job::run(req), JobKind::Run, title, total);
    }

    /// The New Run sheet's values.
    pub fn form_values(&self) -> RunForm {
        let f = self.form;
        RunForm {
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
            stored_gib: f.stored_gib.get_untracked(),
            anonymous: f.anonymous.get_untracked(),
            spec: f.spec.get_untracked(),
        }
    }

    /// Show `form` in the New Run sheet's fields.
    pub fn fill_form(&self, form: &RunForm, run_id: Option<String>) {
        let f = self.form;
        batch(|| {
            f.target.set(form.target.clone());
            f.preset.set(Some(form.preset.clone().into()));
            f.duration.set(form.duration.clone());
            f.concurrency.set(form.concurrency.clone());
            f.io_concurrency.set(form.io_concurrency.clone());
            f.chunk.set(form.chunk.clone());
            f.endpoint.set(form.endpoint.clone());
            f.region.set(form.region.clone());
            f.label.set(form.label.clone());
            f.stored_gib.set(form.stored_gib.clone());
            f.anonymous.set(form.anonymous);
            f.spec.set(form.spec.clone());
            f.spec_run.set(run_id);
            f.error.set(None);
        });
    }

    /// New Run…: a fresh form (the last target and the defaults), not tied to a run.
    pub fn open_new_run(&self) {
        let f = self.form;
        if f.spec.get_untracked().is_some() {
            let p = self.prefs.get_untracked();
            batch(|| {
                f.spec.set(None);
                f.spec_run.set(None);
                f.preset.set(Some(p.preset.clone().into()));
                f.duration.set(p.duration.clone());
                f.concurrency.set(p.concurrency.clone());
                f.io_concurrency.set(String::new());
                f.chunk.set(String::new());
                f.label.set(String::new());
                f.stored_gib.set(String::new());
            });
        }
        self.open(Overlay::NewRun);
    }

    /// The shown run's spec and id.
    pub fn current_spec(&self) -> Option<(String, RunSpec)> {
        self.current.with_untracked(|c| {
            c.as_ref()
                .map(|c| (c.id().to_owned(), c.run.report.spec.clone()))
        })
    }

    /// Edit and Run Again…: the sheet on the shown run's spec (its fields edit the spec).
    pub fn edit_rerun(&self) {
        match self.current_spec() {
            Some((id, spec)) => {
                self.fill_form(&RunForm::from_spec(&spec), Some(id));
                self.open(Overlay::NewRun);
            }
            None => self.open_new_run(),
        }
    }

    /// The New Run sheet's Start.
    pub fn submit_form(&self) {
        let f = self.form;
        let form = self.form_values();
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

    /// "Try the demo": `lab://demo` with the CLI's demo defaults ([`crate::config::defaults`]).
    /// TODO(v4 stream D): the lab://demo workload defaults land with `oprobe demo` = lab://demo.
    pub fn demo(&self) {
        let d = crate::config::defaults();
        let duration = crate::ui::launch()
            .duration
            .unwrap_or_else(|| d.demo_duration.to_string());
        let form = RunForm {
            target: d.demo_target.clone(),
            duration,
            label: "demo".into(),
            ..Default::default()
        };
        if let Err(e) = self.start_form(form) {
            self.toast(e, Tone::Crit);
        }
    }

    /// Run the shown run again: its spec, exactly (`oprobe run --from RUN`).
    pub fn rerun(&self) {
        match self.current_spec() {
            Some((_, spec)) => {
                if let Err(e) = self.start_form(RunForm::from_spec(&spec)) {
                    self.toast(e, Tone::Crit);
                }
            }
            None => self.open_new_run(),
        }
    }

    /// Run the spec of run `base_id` with `overrides`; when it lands it is compared with
    /// `base_id` and the hypothesis `code` judged against `expect`.
    fn start_verify(
        &self,
        base_id: String,
        spec: &RunSpec,
        overrides: &Overrides,
        code: &str,
        expect: Vec<crate::analysis::verification::Expectation>,
    ) {
        let mut next = match spec.try_with(overrides) {
            Ok(s) => s,
            Err(e) => {
                self.toast(format!("cannot verify: {}", e.message), Tone::Crit);
                return;
            }
        };
        next.label = Some(format!("verify {code}"));
        next.profile.name = next.label.clone();
        let form = RunForm::from_spec(&next);
        match self.start_form(form) {
            Ok(()) => {
                self.pending_verify.replace(Some(PendingVerify {
                    base_id,
                    code: code.to_owned(),
                    expect,
                }));
                self.verdict.set(None);
                self.toast(
                    format!("Verifying {code}: {}", overrides.summary()),
                    Tone::Info,
                );
            }
            Err(e) => self.toast(e, Tone::Crit),
        }
    }

    /// Verify a finding of the shown run: replay its spec with the finding's overrides, then
    /// compare the two runs and judge the hypothesis (design v4 §5).
    pub fn verify(&self, f: &Finding) {
        let Some(v) = f.verification.clone() else {
            return;
        };
        let Some((id, spec)) = self.current_spec() else {
            return;
        };
        self.start_verify(id, &spec, &v.overrides, &f.code, v.expect);
    }

    /// The most severe finding of the shown run that has a verification.
    pub fn top_verifiable(&self) -> Option<Finding> {
        self.current.with(|c| {
            c.as_ref().and_then(|c| {
                c.run
                    .report
                    .findings
                    .iter()
                    .filter(|f| f.verification.is_some())
                    .max_by_key(|f| f.severity)
                    .cloned()
            })
        })
    }

    pub fn verify_top(&self) {
        match self.top_verifiable() {
            Some(f) => self.verify(&f),
            None => self.toast("The shown run has no finding to verify", Tone::Warn),
        }
    }

    /// The tuner's winner as overrides of the spec it started from.
    fn tuned_overrides(&self) -> Option<(String, RunSpec, Overrides)> {
        let (id, spec) = self.tune_base.get_untracked()?;
        let k = self
            .tuned
            .with_untracked(|t| t.as_ref().map(|t| t.knobs.clone()))?;
        let base = spec.resolved_profile().knobs;
        let mut o = Overrides::default();
        let mut set = |key: &str, a: String, b: String| {
            if a != b {
                let _ = o.set(key, &b);
            }
        };
        set(
            "concurrency",
            base.concurrency.to_string(),
            k.concurrency.to_string(),
        );
        set(
            "io_concurrency",
            base.io_concurrency.to_string(),
            k.io_concurrency.to_string(),
        );
        set(
            "chunk_size",
            base.chunk_size.to_string(),
            k.chunk_size.to_string(),
        );
        set(
            "max_inflight_requests",
            base.max_inflight_requests.to_string(),
            k.max_inflight_requests.to_string(),
        );
        set(
            "http_version",
            format!("{:?}", base.http_version).to_lowercase(),
            format!("{:?}", k.http_version).to_lowercase(),
        );
        Some((id, spec, o))
    }

    /// The tune result's "Run with these settings": the spec tuned, with the winner applied.
    pub fn run_tuned(&self) {
        let Some((_, spec, o)) = self.tuned_overrides() else {
            self.toast("No tuning result to run", Tone::Warn);
            return;
        };
        let mut next = spec.with(&o);
        next.label = Some("tuned settings".into());
        next.profile.name = next.label.clone();
        if let Err(e) = self.start_form(RunForm::from_spec(&next)) {
            self.toast(e, Tone::Crit);
        }
    }

    /// The tune result's "Verify": the winner against the run tuned, compared and judged.
    pub fn verify_tuned(&self) {
        let Some((id, spec, o)) = self.tuned_overrides() else {
            self.toast("No tuning result to verify", Tone::Warn);
            return;
        };
        let expect = vec![crate::analysis::verification::Expectation {
            metric: "summary.ops_per_s".into(),
            direction: crate::analysis::verification::Direction::Increase,
            unit: "ops_per_s".into(),
            ..Default::default()
        }];
        self.start_verify(id, &spec, &o, "tune", expect);
    }

    /// Fix (recommend): diagnose → tune → verify on the shown run's target. In the App this
    /// is the tuner on the shown run (its result offers Run, Verify and Save profile).
    /// TODO(v4 stream T): run `oprobe fix` / `recommend_config` once it lands.
    pub fn fix(&self) {
        self.mode.set(Mode::Tune);
        self.start_tune();
    }

    /// Run the tuner on the shown run's target (its spec), with the CLI's search defaults.
    pub fn start_tune(&self) {
        if self.busy() {
            self.toast("a job is in progress: stop it first", Tone::Warn);
            return;
        }
        let base = self.current_spec();
        let form = match &base {
            Some((_, spec)) => RunForm::from_spec(spec),
            None => RunForm {
                target: self.form.target.get_untracked(),
                ..Default::default()
            },
        };
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
        // The CLI's own defaults for everything the page does not show (trial warmup,
        // cooldown, max concurrency): `config::defaults().tune` through `TuneSettings`.
        let settings = crate::cli::TuneSettings {
            trial_duration: &trial,
            max_trials: max,
            ..crate::cli::TuneSettings::default()
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
        let title = req
            .target_name
            .clone()
            .or_else(|| req.profile.target.url.clone())
            .unwrap_or_else(|| req.profile.target.display());
        let job = Job::tune(
            req.profile,
            req.target_name,
            opts,
            req.budgets,
            Some(self.run_store()),
        );
        batch(|| {
            self.tuned.set(None);
            self.tune_base.set(base);
        });
        self.begin(job, JobKind::Tune, title, 0.0);
        self.mode.set(Mode::Tune);
    }

    // -----------------------------------------------------------------------------------
    // Targets
    // -----------------------------------------------------------------------------------

    /// Add Target…: the sheet, empty or started from a credentials source found here.
    pub fn open_add_target(&self, from: Option<TargetDraft>) {
        self.add.fill(&from.unwrap_or_default());
        self.open(Overlay::AddTarget);
    }

    /// Test the sheet's target, read-only: list at most one entry under its root.
    pub fn test_target(&self) {
        let draft = self.add.draft();
        let add = self.add;
        let (name, entry) = match draft.entry() {
            Ok(x) => x,
            Err(e) => {
                add.error.set(Some(e));
                return;
            }
        };
        let mut cfg = self.config();
        cfg.targets.insert(name.clone(), entry);
        batch(|| {
            add.error.set(None);
            add.testing.set(true);
            add.test
                .set(Some((Tone::Info, "Testing (read-only)…".into())));
        });
        spawn(async move {
            let res = off_thread(move || test_connection(&name, &cfg)).await;
            batch(|| {
                add.testing.set(false);
                add.test.set(Some(match res {
                    Some(Ok(msg)) => (Tone::Ok, msg),
                    Some(Err(e)) => (Tone::Crit, e),
                    None => (Tone::Crit, "the test did not finish".into()),
                }));
            });
        });
    }

    /// Save the sheet's target to the config file (`[targets.NAME]`).
    pub fn save_target(&self) {
        let draft = self.add.draft();
        let path = self.paths.with(|p| p.config_file.clone());
        let mut cfg = self.config();
        match draft.add_to(&mut cfg, false) {
            Ok(name) => match cfg.save(&path) {
                Ok(()) => {
                    self.refresh();
                    batch(|| {
                        self.overlay.set(None);
                        self.form.target.set(name.clone());
                    });
                    self.toast(format!("Saved target {name}"), Tone::Ok);
                }
                Err(e) => self.add.error.set(Some(format!("{e:#}"))),
            },
            Err(e) => self.add.error.set(Some(e)),
        }
    }

    /// Switch target: the next run uses `name` (the New Run sheet opens on it).
    pub fn switch_target(&self, name: &str) {
        self.form.target.set(name.to_owned());
        self.open_new_run();
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
        // Runs saved elsewhere (the CLI, another window): the watcher saw the directory change.
        let stamp = self
            .runs_changed
            .with(|g| g.load(std::sync::atomic::Ordering::Relaxed));
        let focus_in = REFRESH_REQUESTED.swap(false, std::sync::atomic::Ordering::Relaxed);
        if self.runs_seen.with(|s| *s != stamp) || focus_in {
            self.runs_seen.replace(stamp);
            self.refresh();
        }
        let Some(job) = self.job.with(|j| j.clone()) else {
            return;
        };
        let up = job.take();
        let stage_now = self.live.with_untracked(|l| l.as_ref().map(|l| l.stage));
        let cleaning = up.cleaning && stage_now != Some(Stage::Cleanup);
        if up.snapshot.is_none()
            && up.log.is_empty()
            && up.trials.is_none()
            && up.outcome.is_none()
            && !cleaning
        {
            return;
        }
        let mut progress = None;
        self.live.update(|l| {
            let Some(l) = l else { return };
            if let Some(snap) = up.snapshot {
                // Append what is new to the series (finished seconds never change).
                l.series.update_timeline(&snap.timeline, snap.elapsed_s);
                l.stage = Stage::of(snap.phase);
                if l.total_s > 0.0 && l.stage == Stage::Measuring {
                    progress = Some((snap.elapsed_s / l.total_s).clamp(0.0, 1.0));
                }
                l.snapshot = Some(snap);
            }
            if up.cleaning {
                l.stage = Stage::Cleanup;
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
            Outcome::Run {
                run,
                path,
                tuned,
                partial,
            } => {
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
                match &partial {
                    // A budget stop is not a failure: the partial run is kept and shown.
                    Some(why) => self.toast(
                        format!("Run {id} stopped by a budget (partial run kept): {why}"),
                        Tone::Warn,
                    ),
                    None => self.toast(format!("Run {id} finished · {headline}"), Tone::Ok),
                }
                // A Verify's run: compare it with the run it verifies and judge.
                if let Some(v) = self.pending_verify.with(|p| p.take()) {
                    self.compare_verify(v, id.clone());
                }
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
                self.pending_verify.replace(None);
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
        if on {
            // Coming back: runs may have been saved meanwhile.
            self.refresh();
            if !kit_in_terminal() {
                set_badge(0);
            }
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
        self.verdict.set(None);
        self.compare_then(a, b, None);
    }

    /// A Verify's run landed: compare it with the original and judge the hypothesis.
    fn compare_verify(&self, v: PendingVerify, cand: String) {
        let base = v.base_id.clone();
        self.compare_then(&base, &cand, Some(v));
    }

    fn compare_then(&self, a: &str, b: &str, verify: Option<PendingVerify>) {
        let store = self.run_store();
        let (a, b) = (a.to_owned(), b.to_owned());
        let s = *self;
        batch(|| {
            self.mode.set(Mode::Compare);
            self.comparison.set(None);
            self.compare_pick.set((Some(a.clone()), Some(b.clone())));
            self.comparing.set(true);
        });
        spawn(async move {
            let res = off_thread(move || {
                let load = |id: &str| -> anyhow::Result<(crate::model::Run, std::path::PathBuf)> {
                    let path = store.resolve(id)?;
                    Ok((crate::store::read_run_file(&path)?, path))
                };
                let ((ra, pa), (rb, pb)) = (load(&a)?, load(&b)?);
                let c = compare(&ra, &rb);
                let (va, vb) = (RunView::new(ra, Some(pa)), RunView::new(rb, Some(pb)));
                anyhow::Ok((va, vb, c))
            })
            .await;
            match res {
                Some(Ok((va, vb, c))) => {
                    let verdict = verify.map(|v| {
                        crate::ui::model::verify::judge(
                            &v.code,
                            &v.expect,
                            &va.run.report,
                            &vb.run.report,
                            &c.diff,
                        )
                    });
                    batch(|| {
                        s.verdict.set(verdict);
                        s.comparing.set(false);
                        s.comparison
                            .set(Some((Shared(Arc::new(va)), Shared(Arc::new(vb)), c)));
                    });
                }
                Some(Err(e)) => {
                    s.comparing.set(false);
                    s.toast(format!("cannot compare: {e:#}"), Tone::Crit)
                }
                None => s.comparing.set(false),
            }
        });
    }

    /// Compare the marked runs, or the shown run with the newest earlier run of the same
    /// target and workload (design v4 §2: never an unrelated run).
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
        match crate::ui::model::runs::default_pair(&runs, &cur) {
            Some((a, b)) => self.compare(&a, &b),
            None => {
                // No earlier run measured the same thing: open Compare on the shown run so
                // the other side can be picked (a non-comparable pair says why).
                batch(|| {
                    self.verdict.set(None);
                    self.comparison.set(None);
                    self.compare_pick.set((None, Some(cur.clone())));
                    self.mode.set(Mode::Compare);
                });
                self.toast(
                    "No earlier run of the same target and workload: pick the other run, \
                     or mark two in the sidebar",
                    Tone::Warn,
                );
            }
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

/// Read-only connection test of configured target `name` (the Add Target sheet's "Test"):
/// resolve it as a run would (credentials included), then list at most one entry under its
/// root. Nothing is written. Runs on an engine thread (blocking).
pub fn test_connection(name: &str, cfg: &ConfigFile) -> Result<String, String> {
    use crate::cli::args::{CostArgs, KnobArgs, TargetArgs, WorkloadArgs};
    use futures::TryStreamExt;
    let l = crate::ui::launch();
    let ta = TargetArgs {
        target: Some(name.to_owned()),
        ca_cert: l.ca_cert,
        insecure: l.insecure,
        proxy: l.proxy,
        ..Default::default()
    };
    let err = |e: anyhow::Error| format!("{e:#}");
    let (ta, _) = crate::cli::resolve_target(&ta, cfg).map_err(err)?;
    let p = crate::cli::build_profile(
        &ta,
        &WorkloadArgs::default(),
        &KnobArgs::default(),
        &CostArgs::default(),
    )
    .map_err(err)?;
    let mut target = p.target.clone();
    let shown = target.display();
    let fut = async move {
        crate::engine::operator::prepare_target(&mut target, &p.knobs).await?;
        let op = crate::engine::operator::base_operator(&target)?;
        let mut lister = op.lister_with("").limit(2).await?;
        let mut n = 0usize;
        while lister.try_next().await?.is_some() {
            n += 1;
            if n >= 2 {
                break;
            }
        }
        anyhow::Ok(n)
    };
    let res = tokio::runtime::Handle::current()
        .block_on(async { tokio::time::timeout(Duration::from_secs(20), fut).await });
    match res {
        Err(_) => Err(format!("no answer from {shown} within 20 s")),
        Ok(Ok(0)) => Ok(format!(
            "Connected to {shown} (read-only): nothing stored yet"
        )),
        Ok(Ok(_)) => Ok(format!(
            "Connected to {shown} (read-only): objects are listed"
        )),
        Ok(Err(e)) => {
            let kind = e
                .downcast_ref::<opendal::Error>()
                .map(|o| o.kind())
                .unwrap_or(opendal::ErrorKind::Unexpected);
            let first = format!("{e:#}");
            Err(match kind {
                opendal::ErrorKind::PermissionDenied => format!(
                    "the credentials were rejected (permission denied): check the credentials \
                     source — {first}"
                ),
                opendal::ErrorKind::NotFound => {
                    format!("the bucket or container was not found: check the URL — {first}")
                }
                _ => first,
            })
        }
    }
}

/// Set when the terminal regains the focus (`term.rs`): the next poll reloads the runs.
static REFRESH_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ask the store to reload the run history at its next poll (the terminal's focus-in).
pub fn request_refresh() {
    REFRESH_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn kit_in_terminal() -> bool {
    crate::ui::kit::in_terminal()
}

/// What changes in a directory when a run file is written, renamed or removed: its own
/// modification time and its entry count (cheap: one `stat` and one listing).
fn dir_stamp(dir: &std::path::Path) -> (Option<std::time::SystemTime>, usize) {
    let mtime = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    let n = std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
    (mtime, n)
}

/// Watch the runs directory on a background thread (about once a second) and bump the
/// counter it returns whenever it changes: runs saved by the CLI (or another window) appear
/// without a restart. The thread ends when the store drops the counter.
fn watch_dir(dir: PathBuf) -> Arc<std::sync::atomic::AtomicU64> {
    use std::sync::atomic::Ordering;
    let stamp = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let weak = Arc::downgrade(&stamp);
    let _ = std::thread::Builder::new()
        .name("oprobe-runs-watch".into())
        .spawn(move || {
            let mut last = dir_stamp(&dir);
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                let Some(g) = weak.upgrade() else { return };
                let now = dir_stamp(&dir);
                if now != last {
                    last = now;
                    g.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    stamp
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
