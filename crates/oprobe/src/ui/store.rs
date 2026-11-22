//! The app's state (signals) and its only writers. Every action calls the service layer:
//! `App::list`/`show`/`rm` through [`crate::ui::data`], `App::run`/`prove` through
//! [`crate::ui::jobs`]. Settings (the language) persist in the app's storage.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use nsh_core::prelude::*;
use nsh_core::storage::Storage;

use crate::app::{App, ProveObjective, ProveRequest, RunRef, RunRequest, WorkloadInput};
use crate::ui::data::{self, ProfileModel, RunRow};
use crate::ui::i18n::{self, Lang};
use crate::ui::jobs::{self, Job, Outcome, State};
use crate::ui::kit::Tone;

/// The four views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Runs,
    Profile,
    Operator,
    Compare,
}

impl Page {
    pub const ALL: [Page; 4] = [Page::Runs, Page::Profile, Page::Operator, Page::Compare];

    pub fn key(self) -> &'static str {
        match self {
            Page::Runs => "nav.runs",
            Page::Profile => "nav.profile",
            Page::Operator => "nav.operator",
            Page::Compare => "nav.compare",
        }
    }
}

/// The four workload presets (design v6 §8).
pub const PRESETS: [&str; 4] = ["small-files", "large-objects", "range-reads", "list-stat"];

/// The lab scenarios the Runs view offers for a first, offline run.
pub const LAB: [&str; 6] = [
    "demo",
    "throttle",
    "stragglers",
    "bandwidth",
    "flaky",
    "ratelimit",
];

const SETTINGS_KEY: &str = "oprobe.settings";

/// What the app starts with: where runs and the config file are (tests point them at a
/// temporary directory), and a run to open.
#[derive(Clone, Debug, Default)]
pub struct Launch {
    pub paths: Option<crate::store::Paths>,
    pub open: Option<String>,
}

static LAUNCH: std::sync::Mutex<Option<Launch>> = std::sync::Mutex::new(None);

/// Set the launch options before the app starts (again before each start, in tests).
pub fn set_launch(l: Launch) {
    *LAUNCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(l);
}

fn launch() -> Launch {
    LAUNCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The new-run form.
#[derive(Clone, Copy)]
pub struct Form {
    /// One or more operators (URLs, op.toml files, configured names), space separated.
    pub operator: Signal<String>,
    pub preset: Signal<&'static str>,
    pub duration: Signal<String>,
    pub concurrency: Signal<String>,
}

#[derive(Clone, Copy)]
pub struct Store {
    pub page: Signal<Page>,
    pub lang: Signal<Lang>,
    /// The language chosen in the app (`None`: the system's).
    pub lang_choice: Signal<Option<Lang>>,
    pub runs: Signal<Vec<RunRow>>,
    pub current: Signal<Option<Rc<ProfileModel>>>,
    pub loading: Signal<bool>,
    pub form: Form,
    pub run_state: Signal<Option<State>>,
    pub prove_state: Signal<Option<State>>,
    /// The Operator view's changes to the current run's operator (`PATH → VALUE`).
    pub edits: Signal<BTreeMap<String, String>>,
    pub objective: Signal<ProveObjective>,
    pub prove_budget: Signal<String>,
    pub prove_pairs: Signal<usize>,
    /// Runs picked for Compare (in order).
    pub picked: Signal<Vec<String>>,
    pub compared: Signal<Vec<Rc<ProfileModel>>>,
    pub notice: Signal<Option<(String, Tone)>>,
    app: Stored<App>,
    /// The app's root: work started from a view outlives the view (a page switch).
    root: Owner,
    run_job: Stored<Option<Job>>,
    prove_job: Stored<Option<Job>>,
}

/// The store of the running app.
pub fn store() -> Store {
    use_context::<Store>().expect("the app provides its store")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Settings {
    #[serde(default)]
    lang: Option<String>,
}

impl Store {
    pub fn new() -> Self {
        let l = launch();
        let paths = l
            .paths
            .clone()
            .unwrap_or_else(|| crate::store::Paths::resolve(None));
        let settings: Settings = Storage::get_item(SETTINGS_KEY)
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let choice = settings.lang.as_deref().and_then(Lang::parse);
        let s = Self {
            page: signal(Page::Runs),
            lang: signal(choice.unwrap_or_else(i18n::system)),
            lang_choice: signal(choice),
            runs: signal(Vec::new()),
            current: signal(None),
            loading: signal(false),
            form: Form {
                operator: signal("lab://demo".to_owned()),
                preset: signal(PRESETS[0]),
                duration: signal(String::new()),
                concurrency: signal(String::new()),
            },
            run_state: signal(None),
            prove_state: signal(None),
            edits: signal(BTreeMap::new()),
            objective: signal(ProveObjective::Goodput),
            prove_budget: signal("2m".to_owned()),
            prove_pairs: signal(4),
            picked: signal(Vec::new()),
            compared: signal(Vec::new()),
            notice: signal(None),
            app: stored(App::new(paths)),
            root: Owner::current(),
            run_job: stored(None),
            prove_job: stored(None),
        };
        s.refresh();
        // A run to open, else the newest run is ready in Profile and Operator.
        match l.open {
            Some(id) => s.open_run(&id),
            None => {
                if let Some(id) = s.runs.with_untracked(|r| r.first().map(|r| r.id.clone())) {
                    s.load_run(&id, false);
                }
            }
        }
        set_interval(Duration::from_millis(100), move || s.poll());
        s
    }

    fn app(&self) -> App {
        self.app.with(|a| a.clone())
    }

    /// Reload the run history.
    pub fn refresh(&self) {
        match data::list(&self.app()) {
            Ok(runs) => self.runs.set(runs),
            Err(e) => self.say(format!("{e:#}"), Tone::Crit),
        }
    }

    pub fn say(&self, msg: impl Into<String>, tone: Tone) {
        self.notice.set(Some((msg.into(), tone)));
    }

    // ---------------------------------------------------------------------------------
    // Settings
    // ---------------------------------------------------------------------------------

    /// Choose the app's language (`None`: follow the system); remembered.
    pub fn set_lang(&self, choice: Option<Lang>) {
        let settings = Settings {
            lang: choice.map(|l| l.code().to_owned()),
        };
        if let Ok(j) = serde_json::to_string(&settings) {
            let _ = Storage::set_item(SETTINGS_KEY, j);
        }
        batch(|| {
            self.lang_choice.set(choice);
            self.lang.set(choice.unwrap_or_else(i18n::system));
        });
    }

    // ---------------------------------------------------------------------------------
    // Runs
    // ---------------------------------------------------------------------------------

    pub fn running(&self) -> bool {
        self.run_state
            .with(|s| s.as_ref().is_some_and(|s| s.outcome.is_none()))
    }

    pub fn proving(&self) -> bool {
        self.prove_state
            .with(|s| s.as_ref().is_some_and(|s| s.outcome.is_none()))
    }

    /// Start a run of the form's operators (`App::run`).
    pub fn start_run(&self) {
        if self.running() {
            return;
        }
        let f = self.form;
        let operators: Vec<String> = f
            .operator
            .get_untracked()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if operators.is_empty() {
            self.say(i18n::tr("runs.need_operator"), Tone::Warn);
            return;
        }
        let nonempty =
            |s: Signal<String>| Some(s.get_untracked().trim().to_owned()).filter(|v| !v.is_empty());
        let concurrency = match nonempty(f.concurrency).map(|c| c.parse::<usize>()) {
            Some(Ok(n)) if n > 0 => Some(n),
            Some(_) => {
                self.say(i18n::tr("runs.bad_concurrency"), Tone::Warn);
                return;
            }
            None => None,
        };
        let req = RunRequest {
            operators,
            workload: WorkloadInput {
                preset: Some(f.preset.get_untracked().to_owned()),
                duration: nonempty(f.duration),
                concurrency,
                ..Default::default()
            },
            ..Default::default()
        };
        let job = Job::run(self.app(), req);
        jobs::track(&job);
        self.notice.set(None);
        self.run_state.set(Some(job.peek()));
        self.run_job.replace(Some(job));
    }

    pub fn stop_run(&self) {
        if let Some(j) = self.run_job.with(|j| j.clone()) {
            j.stop();
        }
    }

    /// Show stored run `id` in the Profile view (`App::show`, off the UI thread).
    pub fn open_run(&self, id: &str) {
        self.load_run(id, true);
    }

    /// Load stored run `id` as the current run; `show` switches to Profile.
    fn load_run(&self, id: &str, show: bool) {
        let (app, id, s) = (self.app(), id.to_owned(), *self);
        self.loading.set(true);
        if show {
            self.page.set(Page::Profile);
        }
        self.root.run(|| {
            spawn(async move {
                let res = jobs::off_thread(move || {
                    data::profile(&app, &id).map_err(|e| format!("{e:#}"))
                })
                .await;
                s.loading.set(false);
                match res {
                    Some(Ok(m)) => batch(|| {
                        s.current.set(Some(Rc::new(m)));
                        s.edits.set(BTreeMap::new());
                        s.prove_state.set(None);
                    }),
                    Some(Err(e)) => s.say(e, Tone::Crit),
                    None => s.say(i18n::tr("app.failed"), Tone::Crit),
                }
            })
        });
    }

    /// Delete stored run `id` (`App::rm`).
    pub fn delete_run(&self, id: &str) {
        if let Err(e) = self.app().rm(&[RunRef::from(id)]) {
            self.say(format!("{e:#}"), Tone::Crit);
        }
        self.picked.update(|p| p.retain(|x| x != id));
        if self
            .current
            .with_untracked(|c| c.as_ref().is_some_and(|c| c.id == id))
        {
            self.current.set(None);
        }
        self.refresh();
    }

    /// Pick or unpick run `id` for Compare.
    pub fn toggle_pick(&self, id: &str) {
        self.picked.update(|p| {
            if let Some(i) = p.iter().position(|x| x == id) {
                p.remove(i);
            } else {
                p.push(id.to_owned());
            }
        });
    }

    /// Compare runs `ids` side by side (each loaded with `App::show`).
    pub fn compare(&self, ids: Vec<String>) {
        let (app, s) = (self.app(), *self);
        self.page.set(Page::Compare);
        self.picked.set(ids.clone());
        self.loading.set(true);
        self.root.run(|| {
            spawn(async move {
                let res = jobs::off_thread(move || {
                    ids.iter()
                        .map(|id| data::profile(&app, id))
                        .collect::<anyhow::Result<Vec<_>>>()
                        .map_err(|e| format!("{e:#}"))
                })
                .await;
                s.loading.set(false);
                match res {
                    Some(Ok(ms)) => s.compared.set(ms.into_iter().map(Rc::new).collect()),
                    Some(Err(e)) => s.say(e, Tone::Crit),
                    None => s.say(i18n::tr("app.failed"), Tone::Crit),
                }
            })
        });
    }

    // ---------------------------------------------------------------------------------
    // Operator and Prove
    // ---------------------------------------------------------------------------------

    /// Change one operator value (`""`: back to the run's value).
    pub fn edit(&self, path: &str, value: &str) {
        let base = self
            .current
            .with_untracked(|c| c.as_ref().map(|c| data::value_of(&c.operator, path)))
            .unwrap_or_default();
        self.edits.update(|e| {
            let v = value.trim();
            if v.is_empty() || v == base {
                e.remove(path);
            } else {
                e.insert(path.to_owned(), v.to_owned());
            }
            // Switching a layer off drops the changes to its parameters.
            if v == "off" && data::layer_of(path).is_none() {
                let prefix = format!("{path}.");
                e.retain(|k, _| !k.starts_with(&prefix));
            }
        });
    }

    /// Apply a finding's remedy: its paths with the values to try (a layer that needs a value
    /// gets one sized from the run).
    pub fn apply_remedy(&self, remedy: &[(String, String)]) {
        for (path, value) in remedy {
            let v = if !value.is_empty() {
                value.clone()
            } else {
                suggested(path)
            };
            self.edit(path, &v);
        }
        self.page.set(Page::Operator);
    }

    /// Prove the current changes (or, without any, the top finding's remedy) with `App::prove`.
    pub fn start_prove(&self) {
        if self.proving() {
            return;
        }
        let Some(cur) = self.current.get_untracked() else {
            return;
        };
        let with: Vec<String> = self
            .edits
            .get_untracked()
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let budget =
            Some(self.prove_budget.get_untracked().trim().to_owned()).filter(|b| !b.is_empty());
        let req = ProveRequest {
            run: cur.id.clone(),
            with,
            objective: self.objective.get_untracked(),
            budget,
            pairs: self.prove_pairs.get_untracked(),
            ..Default::default()
        };
        let job = Job::prove(self.app(), req, cur.operator.clone());
        jobs::track(&job);
        self.notice.set(None);
        self.prove_state.set(Some(job.peek()));
        self.prove_job.replace(Some(job));
    }

    pub fn stop_prove(&self) {
        if let Some(j) = self.prove_job.with(|j| j.clone()) {
            j.stop();
        }
    }

    // ---------------------------------------------------------------------------------

    /// Copy the jobs' states into the signals; act on the ones that ended.
    fn poll(&self) {
        if let Some(job) = self.run_job.with(|j| j.clone()) {
            let st = job.peek();
            let ended = st.outcome.clone();
            if self.run_state.with_untracked(|s| s.as_ref() != Some(&st)) {
                self.run_state.set(Some(st));
            }
            if let Some(o) = ended {
                self.run_job.replace(None);
                self.refresh();
                match o {
                    Outcome::Ran(ids) if ids.len() == 1 => self.open_run(&ids[0]),
                    Outcome::Ran(ids) if ids.len() > 1 => self.compare(ids),
                    Outcome::Failed { message, hint } => self.say(
                        match hint {
                            Some(h) => format!("{message} — {h}"),
                            None => message,
                        },
                        Tone::Crit,
                    ),
                    _ => {}
                }
            }
        }
        if let Some(job) = self.prove_job.with(|j| j.clone()) {
            let st = job.peek();
            let ended = st.outcome.clone();
            if self.prove_state.with_untracked(|s| s.as_ref() != Some(&st)) {
                self.prove_state.set(Some(st));
            }
            if let Some(o) = ended {
                self.prove_job.replace(None);
                self.refresh();
                if let Outcome::Failed { message, hint } = o {
                    self.say(
                        match hint {
                            Some(h) => format!("{message} — {h}"),
                            None => message,
                        },
                        Tone::Crit,
                    );
                }
            }
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// The value tried for a remedy path that names no value: a layer goes in with its defaults,
/// `concurrent_limit` gets a starting limit, switches go on.
fn suggested(path: &str) -> String {
    match path {
        "layers.concurrent_limit.http" => "16".into(),
        "layers.timeout.io" => "5s".into(),
        "layers.throttle.bandwidth" => "64MiB".into(),
        "read.gap" => "1MiB".into(),
        "read.concurrent" | "write.concurrent" => "4".into(),
        "write.chunk" => "16MiB".into(),
        "http.version" => "h2".into(),
        _ => "on".into(),
    }
}

/// Stop the running jobs when the app quits and wait (bounded) for their cleanup.
pub fn finish_jobs() {
    if jobs::busy() {
        eprintln!("oprobe: stopping the running job and cleaning up its test data…");
        if !jobs::shutdown(Duration::from_secs(30)) {
            eprintln!("oprobe: the running job did not finish cleaning up in time");
        }
    }
}
