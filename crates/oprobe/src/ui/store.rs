//! The app's state (signals) and its only writers. Every action calls the service layer:
//! `App::list`/`show`/`plan`/`trash`/`restore` through [`crate::ui::data`] or directly,
//! `App::run`/`prove` through [`crate::ui::jobs`]. Settings (the language, the sidebar, the
//! page, recent operator files) persist in the app's storage.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use nsh_core::element::desktop::{main_window, quit};
use nsh_core::prelude::*;
use nsh_core::storage::Storage;

use crate::app::{
    App, ProveObjective, ProveRequest, RunRef, RunRequest, TrashedRuns, WorkloadInput,
};
use crate::ui::data::{self, PlanView, ProfileModel, RunRow};
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
    /// Settings: a window of its own on macOS (not modal), a preferences dialog elsewhere.
    Settings,
    /// About oprobe (macOS shows the system's About panel instead).
    About,
    /// Asks before deleting these runs (on a terminal; a window asks with the system's alert).
    Delete(Vec<String>),
}

/// Where the keyboard moves the selection in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Prev,
    Next,
    First,
    Last,
}

/// The plan of the New Run form for a real target (`App::plan`), shown before Run.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanState {
    Planning,
    Ready(Rc<PlanView>),
    Failed(String),
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
/// The main window's placement (`WindowPlacement`), restored at the next start.
pub const PLACEMENT_KEY: &str = "oprobe.window";

/// The sidebar's width: its default and the range a drag keeps it in.
pub const SIDEBAR_W: f32 = 248.0;
pub const SIDEBAR_MIN: f32 = 200.0;
pub const SIDEBAR_MAX: f32 = 420.0;
/// How many operator files New Run remembers.
const RECENT_MAX: usize = 6;

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
    /// The sidebar shows, and its width (both remembered).
    pub sidebar: Signal<bool>,
    pub sidebar_w: Signal<f32>,
    /// The keyboard's place in the sidebar's list and where a Shift-extended range starts.
    pub cursor: Signal<Option<String>>,
    pub anchor: Signal<Option<String>>,
    /// The row a context menu acts on while it is open (highlighted).
    pub context_row: Signal<Option<String>>,
    pub dialog: Signal<Option<Dialog>>,
    /// The Settings window of macOS is open.
    pub settings_open: Signal<bool>,
    /// A system alert (Delete, Quit) is on screen: the menus' actions wait.
    pub alert: Signal<bool>,
    /// A file is dragged over the window.
    pub drag_over: Signal<bool>,
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
    /// The runs the last Delete moved to the trash, while the notice offers Undo.
    pub trashed: Signal<Option<TrashedRuns>>,
    /// The finding the next Prove sizes and proves (its remedy), when no value was changed.
    pub prove_finding: Signal<Option<String>>,
    /// The New Run form's plan (real targets plan before they run).
    pub plan: Signal<Option<PlanState>>,
    /// Operator files used before (newest first), and the operators the config file names.
    pub recent: Signal<Vec<String>>,
    pub configured: Signal<Vec<String>>,
    app: Stored<App>,
    data_dir: Stored<String>,
    config_file: Stored<String>,
    typed: Stored<(String, std::time::Instant)>,
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
    #[serde(default)]
    sidebar: Option<bool>,
    #[serde(default)]
    sidebar_w: Option<f32>,
    #[serde(default)]
    page: Option<String>,
    #[serde(default)]
    recent: Vec<String>,
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
        let page = match settings.page.as_deref() {
            Some("operator") => Page::Operator,
            _ => Page::Profile,
        };
        let app = App::new(paths.clone());
        let configured = app.configured_operators().unwrap_or_default();
        let s = Self {
            page: signal(page),
            selection: signal(Vec::new()),
            sidebar: signal(settings.sidebar.unwrap_or(true)),
            sidebar_w: signal(
                settings
                    .sidebar_w
                    .unwrap_or(SIDEBAR_W)
                    .clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            ),
            cursor: signal(None),
            anchor: signal(None),
            context_row: signal(None),
            dialog: signal(None),
            settings_open: signal(false),
            alert: signal(false),
            drag_over: signal(false),
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
            trashed: signal(None),
            prove_finding: signal(None),
            plan: signal(None),
            recent: signal(settings.recent),
            configured: signal(configured),
            data_dir: stored(paths.data_dir.display().to_string()),
            config_file: stored(paths.config_file.display().to_string()),
            app: stored(app),
            typed: stored((String::new(), std::time::Instant::now())),
            root: Owner::current(),
            run_job: stored(None),
            prove_job: stored(None),
        };
        s.refresh();
        // A run to open, else the newest run is selected.
        match l.open {
            // A run file the app was opened with (`oprobe RUN.oprobe`).
            Some(p) if std::path::Path::new(&p).is_file() => s.open_file(&p),
            Some(id) => s.open_run(&id),
            None => {
                if let Some(id) = s.runs.with_untracked(|r| r.first().map(|r| r.id.clone())) {
                    s.open_run(&id);
                }
            }
        }
        set_interval(Duration::from_millis(100), move || s.poll());
        // What is remembered follows the signals; the width is saved once a drag ends.
        effect(move || {
            let _ = (
                s.sidebar.get(),
                s.page.get(),
                s.recent.get(),
                s.lang_choice.get(),
            );
            untracked(|| s.persist());
        });
        // A change of the form drops its plan: it no longer says what Run does.
        effect(move || {
            let f = s.form;
            let _ = (
                f.operator.get(),
                f.preset.get(),
                f.duration.get(),
                f.concurrency.get(),
            );
            untracked(|| s.plan.set(None));
        });
        s
    }

    /// Save the settings (the language, the sidebar and its width, the page, recent files).
    pub fn persist(&self) {
        let settings = Settings {
            lang: self
                .lang_choice
                .get_untracked()
                .map(|l| l.code().to_owned()),
            sidebar: Some(self.sidebar.get_untracked()),
            sidebar_w: Some(self.sidebar_w.get_untracked()),
            page: Some(
                match self.page.get_untracked() {
                    Page::Profile => "profile",
                    Page::Operator => "operator",
                }
                .to_owned(),
            ),
            recent: self.recent.get_untracked(),
        };
        if let Ok(j) = serde_json::to_string(&settings) {
            let _ = Storage::set_item(SETTINGS_KEY, j);
        }
    }

    /// Run `f` under the app's root (it outlives the view that started it).
    pub fn task(&self, f: impl std::future::Future<Output = ()> + 'static) {
        self.root.run(|| {
            spawn(f);
        });
    }

    /// The config file's path.
    pub fn config_file(&self) -> String {
        self.config_file.with(|d| d.clone())
    }

    /// A dialog of the main window or a system alert is open: the menus' actions wait (the
    /// Settings window of macOS is not modal, so it does not count).
    pub fn modal(&self) -> bool {
        self.alert.get() || self.dialog.with(|d| d.is_some())
    }

    /// Open Settings: its window on macOS, the preferences dialog elsewhere.
    pub fn open_settings(&self) {
        if crate::ui::menus::settings_window() {
            self.settings_open.set(true);
        } else {
            self.dialog.set(Some(Dialog::Settings));
        }
    }

    /// The operators of the form are all `lab://` scenarios (nothing to plan: the lab is
    /// local and its data goes with it).
    pub fn form_is_lab(&self) -> bool {
        let ops = self.form.operator.get();
        let mut it = ops.split_whitespace().peekable();
        it.peek().is_some() && it.all(|o| o.starts_with("lab://"))
    }

    /// Where runs are stored.
    pub fn data_dir(&self) -> String {
        self.data_dir.with(|d| d.clone())
    }

    fn app(&self) -> App {
        self.app.with(|a| a.clone())
    }

    /// The `.oprobe` file of stored run `id`.
    pub fn run_file(&self, id: &str) -> std::path::PathBuf {
        self.app().store().run_path(id)
    }

    /// Reload the run history.
    pub fn refresh(&self) {
        match data::list(&self.app()) {
            Ok(runs) => self.runs.set(runs),
            Err(e) => self.say(format!("{e:#}"), Tone::Crit),
        }
    }

    /// Show `msg` in the notice bar (and read it to screen readers). A notice offering Undo
    /// gives way: its runs stay in the trash.
    pub fn say(&self, msg: impl Into<String>, tone: Tone) {
        let msg = msg.into();
        self.let_trash_go();
        announce_for_accessibility(msg.clone());
        self.notice.set(Some((msg, tone)));
    }

    /// Dismiss the notice bar.
    pub fn dismiss(&self) {
        self.let_trash_go();
        self.notice.set(None);
    }

    // ---------------------------------------------------------------------------------
    // Settings
    // ---------------------------------------------------------------------------------

    /// Choose the app's language (`None`: follow the system); remembered.
    pub fn set_lang(&self, choice: Option<Lang>) {
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

    /// The run request of the form (the key of a message for the notice bar when it is not
    /// one).
    fn form_request(&self) -> Result<RunRequest, &'static str> {
        let f = self.form;
        let operators: Vec<String> = f
            .operator
            .get_untracked()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if operators.is_empty() {
            return Err("runs.need_operator");
        }
        let nonempty =
            |s: Signal<String>| Some(s.get_untracked().trim().to_owned()).filter(|v| !v.is_empty());
        let concurrency = match nonempty(f.concurrency).map(|c| c.parse::<usize>()) {
            Some(Ok(n)) if n > 0 => Some(n),
            Some(_) => return Err("runs.bad_concurrency"),
            None => None,
        };
        Ok(RunRequest {
            operators,
            workload: WorkloadInput {
                preset: Some(f.preset.get_untracked().to_owned()),
                duration: nonempty(f.duration),
                concurrency,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Plan the form's run (`App::plan`): what it writes, reads and deletes, its budgets.
    pub fn plan_run(&self) {
        let req = match self.form_request() {
            Ok(r) => r,
            Err(key) => {
                self.plan
                    .set(Some(PlanState::Failed(i18n::tr(key).to_owned())));
                return;
            }
        };
        let (app, s) = (self.app(), *self);
        self.plan.set(Some(PlanState::Planning));
        self.root.run(|| {
            spawn(async move {
                let res = jobs::off_thread(move || {
                    app.plan(&req)
                        .map(|p| data::plan_view(&p))
                        .map_err(|e| format!("{e:#}"))
                })
                .await;
                // A change of the form meanwhile dropped what this plan was for.
                if s.plan.get_untracked() != Some(PlanState::Planning) {
                    return;
                }
                s.plan.set(Some(match res {
                    Some(Ok(p)) => PlanState::Ready(Rc::new(p)),
                    Some(Err(e)) => PlanState::Failed(e),
                    None => PlanState::Failed(i18n::tr("app.failed").to_owned()),
                }));
            })
        });
    }

    /// The New Run dialog's default action: a lab run starts; a real target plans first, then
    /// runs once its plan shows.
    pub fn submit_new_run(&self) {
        if untracked(|| self.form_is_lab())
            || matches!(self.plan.get_untracked(), Some(PlanState::Ready(_)))
        {
            self.start_run();
        } else if self.plan.get_untracked() != Some(PlanState::Planning) {
            self.plan_run();
        }
    }

    /// Start a run of the form's operators (`App::run`).
    pub fn start_run(&self) {
        if self.running() {
            return;
        }
        let req = match self.form_request() {
            Ok(r) => r,
            Err(key) => {
                self.say(i18n::tr(key), Tone::Warn);
                return;
            }
        };
        let files: Vec<String> = req
            .operators
            .iter()
            .filter(|o| o.ends_with(".toml"))
            .cloned()
            .collect();
        if !files.is_empty() {
            self.recent.update(|r| {
                for f in files.iter().rev() {
                    r.retain(|x| x != f);
                    r.insert(0, f.clone());
                }
                r.truncate(RECENT_MAX);
            });
        }
        let job = Job::run(self.app(), req);
        jobs::track(&job);
        self.dismiss();
        self.dialog.set(None);
        self.plan.set(None);
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
        batch(|| {
            self.selection.set(vec![id.to_owned()]);
            self.cursor.set(Some(id.to_owned()));
            self.anchor.set(Some(id.to_owned()));
        });
        self.load_run(id);
    }

    /// Open a run file (`.oprobe`, a trace) that may not be in the history.
    pub fn open_file(&self, path: &str) {
        self.selection.set(Vec::new());
        self.load_run(path);
    }

    /// The runs in the sidebar's order: grouped under their operator, the group of the newest
    /// run first.
    pub fn ordered(&self) -> Vec<RunRow> {
        self.runs.with(|r| data::sidebar_order(r))
    }

    /// Change the selection as a press on run `id` with `how` does, then show it: one run in
    /// Profile or Operator, two or more in Compare.
    pub fn select(&self, id: &str, how: Select) {
        let before = self.selection.get_untracked();
        let ids: Vec<String> = untracked(|| self.ordered())
            .into_iter()
            .map(|r| r.id)
            .collect();
        let anchor = self
            .anchor
            .get_untracked()
            .or_else(|| before.last().cloned());
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
                let a = anchor
                    .as_ref()
                    .and_then(|a| ids.iter().position(|x| x == a));
                match (a, ids.iter().position(|x| x == id)) {
                    (Some(a), Some(b)) if a <= b => ids[a..=b].to_vec(),
                    // Upwards: the anchor stays the run shown first.
                    (Some(a), Some(b)) => ids[b..=a].iter().rev().cloned().collect(),
                    _ => vec![id.to_owned()],
                }
            }
        };
        batch(|| {
            self.cursor.set(Some(id.to_owned()));
            if how != Select::Extend {
                self.anchor.set(Some(id.to_owned()));
            }
        });
        if next == before {
            return;
        }
        self.show_selection(next);
    }

    /// Move the keyboard's place in the sidebar (`extend`: Shift held, the range from the
    /// anchor grows or shrinks), selecting and showing what it reaches.
    pub fn move_cursor(&self, to: Move, extend: bool) {
        let ids: Vec<String> = untracked(|| self.ordered())
            .into_iter()
            .map(|r| r.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let at = self
            .cursor
            .get_untracked()
            .and_then(|c| ids.iter().position(|x| *x == c));
        let last = ids.len() - 1;
        let i = match (to, at) {
            (Move::First, _) => 0,
            (Move::Last, _) => last,
            (Move::Prev, Some(i)) => i.saturating_sub(1),
            (Move::Next, Some(i)) => (i + 1).min(last),
            (Move::Prev, None) => last,
            (Move::Next, None) => 0,
        };
        let how = if extend { Select::Extend } else { Select::Only };
        self.select(&ids[i].clone(), how);
    }

    /// Type-select: the next run (after the keyboard's place) whose short id, operator or
    /// workload starts with what was typed in the last second.
    pub fn type_select(&self, c: char) {
        let now = std::time::Instant::now();
        let typed = self.typed.with(|(t, at)| {
            if now.duration_since(*at) > Duration::from_secs(1) {
                c.to_string()
            } else {
                format!("{t}{c}")
            }
        });
        self.typed.replace((typed.clone(), now));
        let rows = untracked(|| self.ordered());
        let start = self
            .cursor
            .get_untracked()
            .and_then(|c| rows.iter().position(|r| r.id == c))
            // A longer prefix may still match where the place is; one letter moves on.
            .map(|i| if typed.chars().count() > 1 { i } else { i + 1 })
            .unwrap_or(0);
        let n = rows.len();
        let hit = (0..n).map(|k| &rows[(start + k) % n.max(1)]).find(|r| {
            let short = r.id.rsplit('-').next().unwrap_or(&r.id);
            [short, r.operator.as_str(), r.preset.as_str()]
                .iter()
                .any(|t| t.to_lowercase().starts_with(&typed))
        });
        if let Some(r) = hit {
            self.select(&r.id.clone(), Select::Only);
        }
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
                        s.prove_finding.set(None);
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

    /// Ask for a run file (`.oprobe`, a report or trace `.json`) with the system's open dialog
    /// and open it where it is.
    pub fn pick_run_file(&self) {
        use nsh_core::file_dialog::{FileFilter, OpenDialog};
        let s = *self;
        let dialog = OpenDialog::files()
            .filter(FileFilter::new(i18n::tr("open.filter"), ["oprobe", "json"]))
            .button_label(i18n::tr("open.button"));
        self.root.run(|| {
            spawn(async move {
                match pick_paths(dialog).await {
                    Ok(Some(paths)) => {
                        if let Some(p) = paths.first() {
                            s.open_file(&p.to_string_lossy());
                        }
                    }
                    Ok(None) => {}
                    Err(e) => s.say(e, Tone::Crit),
                }
            })
        });
    }

    /// Ask for an operator file (`op.toml`) for the New Run form.
    pub fn pick_operator_file(&self) {
        use nsh_core::file_dialog::{FileFilter, OpenDialog};
        let s = *self;
        let dialog =
            OpenDialog::files().filter(FileFilter::new(i18n::tr("runs.operator_file"), ["toml"]));
        self.root.run(|| {
            spawn(async move {
                if let Ok(Some(paths)) = pick_paths(dialog).await
                    && let Some(p) = paths.first()
                {
                    s.form.operator.set(p.to_string_lossy().into_owned());
                }
            })
        });
    }

    /// Open the run files the system opens the app with (Open With, a double click), for as
    /// long as the app runs.
    pub fn follow_open_files(&self) {
        let s = *self;
        self.root.run(|| {
            spawn(async move {
                loop {
                    let files = next_open_files().await;
                    if let Some(p) = files.first() {
                        s.open_file(&p.to_string_lossy());
                        main_window().focus();
                    }
                }
            })
        });
    }

    /// Open the first run file dropped on the window (`file:` URLs).
    pub fn open_dropped(&self, uris: &[String]) {
        self.drag_over.set(false);
        let Some(path) = uris.iter().find_map(|u| {
            let p = u.strip_prefix("file://")?;
            Some(percent_decode(p))
        }) else {
            return;
        };
        self.open_file(&path);
    }

    /// Ask before deleting runs `ids`: the system's alert in a window (Cancel the default,
    /// Delete destructive), a dialog on a terminal.
    pub fn ask_delete(&self, ids: Vec<String>) {
        if ids.is_empty() || self.alert.get_untracked() {
            return;
        }
        if crate::ui::kit::in_terminal() {
            self.dialog.set(Some(Dialog::Delete(ids)));
            return;
        }
        let n = ids.len();
        let title = if n == 1 {
            i18n::trf("delete.one", &[&ids[0]])
        } else {
            i18n::trf("delete.many", &[&n])
        };
        let answer = nsh_base::alert_dialog()
            .title(title)
            .description(i18n::tr("delete.detail"))
            .cancel("cancel", i18n::tr("dialog.cancel"))
            .destructive("delete", i18n::tr("delete.confirm"))
            .close_on_escape(true)
            .show();
        let s = *self;
        self.alert.set(true);
        self.root.run(|| {
            spawn(async move {
                let chosen = answer.await.ok().and_then(|r| r.action);
                s.alert.set(false);
                if chosen.as_deref() == Some("delete") {
                    s.delete_runs(&ids);
                }
            })
        });
    }

    /// Move stored runs `ids` to the trash (`App::trash`); the notice offers Undo.
    pub fn delete_runs(&self, ids: &[String]) {
        let refs: Vec<RunRef> = ids.iter().map(|i| RunRef::from(i.as_str())).collect();
        match self.app().trash(&refs) {
            Ok(t) => {
                let n = t.ids.len();
                let msg = if n == 1 {
                    i18n::trf("delete.moved_one", &[&t.ids[0]])
                } else {
                    i18n::trf("delete.moved_many", &[&n])
                };
                self.say(msg, Tone::Neutral);
                self.trashed.set(Some(t));
            }
            Err(e) => self.say(format!("{e:#}"), Tone::Crit),
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

    /// Undo the last Delete: its runs come back from the trash (`App::restore`).
    pub fn undo_delete(&self) {
        let Some(t) = self.trashed.get_untracked() else {
            return;
        };
        self.trashed.set(None);
        self.notice.set(None);
        match self.app().restore(&t) {
            Ok(()) => {
                self.refresh();
                self.show_selection(t.ids.clone());
            }
            Err(e) => self.say(format!("{e:#}"), Tone::Crit),
        }
    }

    /// The Undo of a Delete is no longer offered: its folder goes to the system's trash where
    /// there is one (else it stays in oprobe's trash, emptied after a week).
    fn let_trash_go(&self) {
        let Some(t) = self.trashed.get_untracked() else {
            return;
        };
        self.trashed.set(None);
        if !crate::ui::kit::in_terminal() {
            let _ = move_to_trash(t.folder);
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

    /// Prove finding `code`'s remedy as `oprobe prove RUN CODE` does: `App::prove` sizes the
    /// remedy from the run and proves it. Changes made by hand are dropped (they would be
    /// proved instead).
    pub fn prove_remedy(&self, code: &str) {
        batch(|| {
            self.edits.set(BTreeMap::new());
            self.prove_finding.set(Some(code.to_owned()));
            self.page.set(Page::Operator);
        });
        self.start_prove();
    }

    /// Prove the current changes as they are (`with`: the user's own values) or, without any,
    /// the chosen finding's remedy (else the top finding's), sized by `App::prove`.
    pub fn start_prove(&self) {
        if self.proving() {
            return;
        }
        let Some(cur) = self.current.get_untracked() else {
            return;
        };
        let with = data::prove_with(&self.edits.get_untracked());
        let finding = if with.is_empty() {
            self.prove_finding.get_untracked()
        } else {
            None
        };
        let budget =
            Some(self.prove_budget.get_untracked().trim().to_owned()).filter(|b| !b.is_empty());
        let req = ProveRequest {
            run: cur.id.clone(),
            finding,
            with,
            objective: self.objective.get_untracked(),
            budget,
            pairs: self.prove_pairs.get_untracked(),
            ..Default::default()
        };
        let job = Job::prove(self.app(), req, cur.operator.clone());
        jobs::track(&job);
        self.dismiss();
        self.prove_state.set(Some(job.peek()));
        self.prove_job.replace(Some(job));
    }

    pub fn stop_prove(&self) {
        if let Some(j) = self.prove_job.with(|j| j.clone()) {
            j.stop();
        }
    }

    // ---------------------------------------------------------------------------------
    // Quitting and closing
    // ---------------------------------------------------------------------------------

    /// The user asked to quit (Quit, Exit, ⌘Q / Ctrl+Q, the main window's close button): while
    /// a run or a prove goes on, ask first ("Stop and Quit" or Cancel); else quit.
    /// A terminal quits with Ctrl+C, which stops a job and waits for its cleanup (`term`).
    pub fn request_quit(&self) {
        if crate::ui::kit::in_terminal() {
            return;
        }
        if !untracked(|| self.running() || self.proving()) {
            quit();
            return;
        }
        if self.alert.get_untracked() {
            return;
        }
        let answer = nsh_base::alert_dialog()
            .title(i18n::tr(if self.proving() {
                "quit.title_prove"
            } else {
                "quit.title_run"
            }))
            .description(i18n::tr("quit.detail"))
            .cancel("cancel", i18n::tr("dialog.cancel"))
            .destructive("quit", i18n::tr("quit.confirm"))
            .close_on_escape(true)
            .show();
        let s = *self;
        self.alert.set(true);
        self.root.run(|| {
            spawn(async move {
                let chosen = answer.await.ok().and_then(|r| r.action);
                s.alert.set(false);
                if chosen.as_deref() == Some("quit") {
                    s.stop_and_quit();
                }
            })
        });
    }

    /// Stop the jobs and quit (the app's quit handler waits for their cleanup).
    pub fn stop_and_quit(&self) {
        self.stop_run();
        self.stop_prove();
        quit();
    }

    /// When a job ends while the window is in the background: a notification and the Dock's
    /// or taskbar's attention.
    fn tell_in_background(&self, title: String, body: String) {
        if crate::ui::kit::in_terminal() {
            return;
        }
        let w = main_window();
        if untracked(|| w.state()).is_none_or(|st| st.focused) {
            return;
        }
        let _ = show_notification(nsh_core::notifications::Notification::new(
            "oprobe-job",
            title,
            body,
        ));
        w.request_attention(false);
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
                self.tell_in_background(
                    i18n::tr(match &o {
                        Outcome::Failed { .. } => "notify.run_failed",
                        Outcome::Stopped => "notify.run_stopped",
                        _ => "notify.run_done",
                    })
                    .to_owned(),
                    self.form.operator.get_untracked(),
                );
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
                let body = match &o {
                    Outcome::Proved(p) => i18n::tr(match p.verdict {
                        crate::app::ProveVerdict::Improved => "prove.improved",
                        crate::app::ProveVerdict::Worse => "prove.worse",
                        crate::app::ProveVerdict::Inconclusive => "prove.inconclusive",
                    })
                    .to_owned(),
                    Outcome::Failed { message, .. } => message.clone(),
                    _ => String::new(),
                };
                self.tell_in_background(i18n::tr("notify.prove_done").to_owned(), body);
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
