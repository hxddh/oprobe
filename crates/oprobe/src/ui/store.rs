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

/// The two views of one selected run (the toolbar's segmented control). Two or more selected
/// runs show Compare instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Profile,
    Operator,
}

impl Page {
    pub const ALL: [Page; 2] = [Page::Profile, Page::Operator];

    pub fn key(self) -> &'static str {
        match self {
            Page::Profile => "nav.profile",
            Page::Operator => "nav.operator",
        }
    }
}

/// How a press on a sidebar row changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Select {
    /// A plain press: this run alone.
    Only,
    /// With the platform's modifier (⌘ / Ctrl): add it or take it out.
    Toggle,
    /// With Shift: every run from the last one picked to this one.
    Extend,
}

/// The dialog on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialog {
    NewRun,
    Settings,
    /// Asks before deleting these runs.
    Delete(Vec<String>),
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
    /// The runs selected in the sidebar, in the order picked: one shows in Profile or
    /// Operator, two or more in Compare.
    pub selection: Signal<Vec<String>>,
    /// The sidebar shows.
    pub sidebar: Signal<bool>,
    pub dialog: Signal<Option<Dialog>>,
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
    pub compared: Signal<Vec<Rc<ProfileModel>>>,
    pub notice: Signal<Option<(String, Tone)>>,
    app: Stored<App>,
    data_dir: Stored<String>,
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
            page: signal(Page::Profile),
            selection: signal(Vec::new()),
            sidebar: signal(true),
            dialog: signal(None),
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
            compared: signal(Vec::new()),
            notice: signal(None),
            data_dir: stored(paths.data_dir.display().to_string()),
            app: stored(App::new(paths)),
            root: Owner::current(),
            run_job: stored(None),
            prove_job: stored(None),
        };
        s.refresh();
        // A run to open, else the newest run is selected.
        match l.open {
            Some(id) => s.open_run(&id),
            None => {
                if let Some(id) = s.runs.with_untracked(|r| r.first().map(|r| r.id.clone())) {
                    s.open_run(&id);
                }
            }
        }
        set_interval(Duration::from_millis(100), move || s.poll());
        s
    }

    /// Where runs are stored.
    pub fn data_dir(&self) -> String {
        self.data_dir.with(|d| d.clone())
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
        self.dialog.set(None);
        self.run_state.set(Some(job.peek()));
        self.run_job.replace(Some(job));
    }

    pub fn stop_run(&self) {
        if let Some(j) = self.run_job.with(|j| j.clone()) {
            j.stop();
        }
    }

    /// Select stored run `id` alone and show it.
    pub fn open_run(&self, id: &str) {
        self.selection.set(vec![id.to_owned()]);
        self.load_run(id);
    }

    /// Open a run file (`.oprobe`, a trace) that may not be in the history.
    pub fn open_file(&self, path: &str) {
        self.selection.set(Vec::new());
        self.load_run(path);
    }

    /// Change the selection as a press on run `id` with `how` does, then show it: one run in
    /// Profile or Operator, two or more in Compare.
    pub fn select(&self, id: &str, how: Select) {
        let before = self.selection.get_untracked();
        let next = match how {
            Select::Only => vec![id.to_owned()],
            Select::Toggle => {
                let mut v = before.clone();
                if let Some(i) = v.iter().position(|x| x == id) {
                    v.remove(i);
                } else {
                    v.push(id.to_owned());
                }
                v
            }
            Select::Extend => {
                let ids: Vec<String> = self
                    .runs
                    .with_untracked(|r| r.iter().map(|r| r.id.clone()).collect());
                let anchor = before.last().and_then(|a| ids.iter().position(|x| x == a));
                match (anchor, ids.iter().position(|x| x == id)) {
                    (Some(a), Some(b)) => ids[a.min(b)..=a.max(b)].to_vec(),
                    _ => vec![id.to_owned()],
                }
            }
        };
        if next == before {
            return;
        }
        self.show_selection(next);
    }

    /// Select every run of `ids` and show them.
    pub fn show_selection(&self, ids: Vec<String>) {
        self.selection.set(ids.clone());
        match ids.len() {
            0 => self.current.set(None),
            1 => self.load_run(&ids[0]),
            _ => self.compare(ids),
        }
    }

    /// Load stored run `id` (or a run file) as the current run.
    fn load_run(&self, id: &str) {
        let (app, id, s) = (self.app(), id.to_owned(), *self);
        self.loading.set(true);
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

    /// Put `text` on the clipboard.
    pub fn copy(&self, text: String) {
        set_clipboard(text);
    }

    /// Run the selected run's operator and workload again.
    pub fn run_again(&self) {
        let Some(cur) = self.current.get_untracked() else {
            return;
        };
        let f = self.form;
        f.operator.set(cur.operator_ref.clone());
        if let Some(p) = PRESETS.iter().find(|p| **p == cur.preset) {
            f.preset.set(p);
        }
        self.start_run();
    }

    /// Ask for a run file (`.oprobe`, a trace) with the system's open dialog and open it.
    pub fn pick_run_file(&self) {
        let s = *self;
        self.root.run(|| {
            spawn(async move {
                let mut req = nsh_core::picker::FilePickRequest::default();
                req.limit = 1;
                let files = pick_files(req).await;
                let Some(f) = files.into_iter().next() else {
                    return;
                };
                // The picker hands over the bytes: open them from a file of their own.
                let name = std::path::Path::new(&f.name)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "run.oprobe".into());
                let path = std::env::temp_dir().join(format!("oprobe-open-{name}"));
                match std::fs::write(&path, f.data.bytes()) {
                    Ok(()) => s.open_file(&path.to_string_lossy()),
                    Err(e) => s.say(format!("{e}"), Tone::Crit),
                }
            })
        });
    }

    /// Open the first run file dropped on the window (`file:` URLs).
    pub fn open_dropped(&self, uris: &[String]) {
        let Some(path) = uris.iter().find_map(|u| {
            let p = u.strip_prefix("file://")?;
            Some(percent_decode(p))
        }) else {
            return;
        };
        self.open_file(&path);
    }

    /// Ask before deleting runs `ids` (the Delete dialog).
    pub fn ask_delete(&self, ids: Vec<String>) {
        if !ids.is_empty() {
            self.dialog.set(Some(Dialog::Delete(ids)));
        }
    }

    /// Delete stored runs `ids` (`App::rm`).
    pub fn delete_runs(&self, ids: &[String]) {
        let refs: Vec<RunRef> = ids.iter().map(|i| RunRef::from(i.as_str())).collect();
        if let Err(e) = self.app().rm(&refs) {
            self.say(format!("{e:#}"), Tone::Crit);
        }
        let left: Vec<String> = self
            .selection
            .get_untracked()
            .into_iter()
            .filter(|x| !ids.contains(x))
            .collect();
        self.refresh();
        if left.is_empty() {
            if let Some(id) = self
                .runs
                .with_untracked(|r| r.first().map(|r| r.id.clone()))
            {
                self.open_run(&id);
            } else {
                self.selection.set(Vec::new());
                self.current.set(None);
            }
        } else {
            self.show_selection(left);
        }
    }

    /// Compare runs `ids` side by side (each loaded with `App::show`).
    pub fn compare(&self, ids: Vec<String>) {
        let (app, s) = (self.app(), *self);
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
        let with = data::prove_with(&self.edits.get_untracked());
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
                    Outcome::Ran(ids) if ids.len() > 1 => self.show_selection(ids),
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

/// `%XX` escapes of a `file:` URL's path decoded.
fn percent_decode(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let s = String::from_utf8_lossy(&out).into_owned();
    // `file:///C:/x` on Windows.
    if cfg!(windows) {
        s.trim_start_matches('/').to_owned()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dropped_file_urls_decode_to_paths() {
        if !cfg!(windows) {
            assert_eq!(
                super::percent_decode("/tmp/a%20b.oprobe"),
                "/tmp/a b.oprobe"
            );
            assert_eq!(super::percent_decode("/x/%E4%B8%AD.oprobe"), "/x/中.oprobe");
            assert_eq!(super::percent_decode("/bad%2"), "/bad%2");
        }
    }
}
