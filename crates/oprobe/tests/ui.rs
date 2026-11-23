#![cfg(feature = "app")]
//! The app on the real nsh engine (headless), driving the real service layer against the
//! local lab: the window's structure in both languages, a run started from New Run streaming
//! into the sidebar and opening in Profile, a layer toggled and proved in Operator, Compare
//! and Delete from a run's context menu, the menu shortcuts, the language set in Settings and
//! remembered. Each test has its
//! own data directory; tests run one at a time (the launch options are process-wide). Waits
//! are bounded only to fail instead of hanging; nothing asserts on wall-clock time.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nsh_core::platform::{Idiom, Locale, LocaleSettings, Os};
use nsh_engine::{EngineConfig, Environment, Points, Root, Size};
use nsh_testing::Headless;
use oprobe::app::{App, Cancel, EventSink, RunOutcome, RunRequest, WorkloadInput};
use oprobe::store::Paths;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Long enough for a debug build to finish a lab run or a short Prove on a busy machine.
const LONG: Duration = Duration::from_secs(300);

struct Ctx {
    _lock: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
}

impl Ctx {
    fn new() -> Self {
        let lock = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cx = Self { _lock: lock, dir };
        cx.launch(None);
        cx
    }

    fn paths(&self) -> Paths {
        Paths {
            config_file: self.dir.path().join("config.toml"),
            data_dir: self.dir.path().join("data"),
        }
    }

    fn launch(&self, open: Option<String>) {
        oprobe::ui::set_launch(oprobe::ui::Launch {
            paths: Some(self.paths()),
            open,
        });
    }

    /// Profile a short lab workload with the service layer directly; the run's id.
    fn seed(&self, operator: &str) -> String {
        let app = App::new(self.paths());
        let req = RunRequest {
            operators: vec![operator.to_owned()],
            workload: small(),
            ..Default::default()
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        match rt
            .block_on(app.run(req, EventSink::silent(), Cancel::new()))
            .unwrap()
        {
            RunOutcome::Run(set) => set.runs[0].run_id.clone(),
            RunOutcome::Plan(_) => unreachable!(),
        }
    }

    fn start(&self) -> Headless {
        self.start_in(Locale::EN_US)
    }

    fn start_in(&self, locale: Locale) -> Headless {
        self.start_on(Os::Linux, locale)
    }

    fn start_on(&self, os: Os, locale: Locale) -> Headless {
        let viewport = Size::new(Points(1360.0), Points(900.0));
        let config = EngineConfig {
            viewport,
            data_dir: Some(self.dir.path().join("app")),
            environment: Environment {
                window: viewport,
                os,
                idiom: Idiom::Desktop,
                locale,
                locale_settings: LocaleSettings::of(locale),
                ..Environment::default()
            },
            ..EngineConfig::new(Root::of::<oprobe::ui::App>())
        };
        let mut app = Headless::start(config).unwrap();
        app.wait_for("id:sidebar").unwrap();
        app
    }
}

fn small() -> WorkloadInput {
    WorkloadInput {
        preset: Some("small-files".into()),
        objects: Some(4),
        object_size: Some("16KiB".into()),
        concurrency: Some(2),
        duration: Some("400ms".into()),
        warmup: Some("0s".into()),
        ..Default::default()
    }
}

fn wait_long(app: &mut Headless, sel: &str) {
    app.wait_for_within(sel, LONG)
        .unwrap_or_else(|e| panic!("{sel}: {e}\n{:?}", app.screen().texts()));
}

/// Choose a language in Settings (opened with its shortcut, closed with Done).
fn choose(app: &mut Headless, lang: &str) {
    app.press_key("ctrl+,").unwrap();
    app.wait_for("id:settings-dialog").unwrap();
    app.press(&format!("text:{lang}")).unwrap();
    app.press("id:dialog-done").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
}

#[test]
fn the_window_reads_in_both_languages() {
    let cx = Ctx::new();
    let id = cx.seed("lab://demo");
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    // The sidebar lists the run under its operator; the toolbar titles it.
    wait_long(&mut app, &format!("id:run-{id}"));
    wait_long(&mut app, "text:lab://demo");
    wait_long(&mut app, "text:Where the time went");
    assert!(app.shows(&id), "the toolbar's title is the run's id");
    assert!(app.shows("Goodput"));
    // Profile and Operator by their shortcuts (mod is Ctrl on Linux).
    app.press_key("ctrl+2").unwrap();
    wait_long(&mut app, "id:operator");
    app.press_key("ctrl+1").unwrap();
    wait_long(&mut app, "id:profile");
    // New Run is a dialog, cancelled with its button.
    app.press("id:new-run").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    app.press("id:dialog-cancel").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    choose(&mut app, "简体中文");
    wait_long(&mut app, "text:时间花在哪里");
    assert!(app.shows(&id), "ids stay as they are");
    app.press("id:new-run").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    wait_long(&mut app, "text:新建运行");
    app.press("id:dialog-cancel").unwrap();
    choose(&mut app, "English");
    wait_long(&mut app, "text:Where the time went");
}

#[test]
fn the_app_follows_the_system_language() {
    let cx = Ctx::new();
    let mut app = cx.start_in(Locale::parse("zh-CN").unwrap());
    wait_long(&mut app, "text:还没有运行");
    wait_long(&mut app, "text:新建运行…");
}

#[test]
fn a_lab_run_streams_into_the_sidebar_and_opens_in_profile() {
    let cx = Ctx::new();
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "text:No runs yet");
    app.press("id:new-run").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    app.clear("id:run-operator").unwrap();
    app.type_text("id:run-operator", "lab://throttle").unwrap();
    app.type_text("id:run-duration", "2s").unwrap();
    app.press("id:run-start").unwrap();
    // It streams in: the dialog closes, the running row shows in the sidebar, Stop in the
    // toolbar.
    wait_long(&mut app, "id:live-run");
    assert!(!app.shows("id:new-run-dialog"));
    wait_long(&mut app, "id:stop");
    // It finishes, is saved and opens in Profile.
    wait_long(&mut app, "id:profile");
    wait_long(&mut app, "text:Where the time went");
    // Every chart of the profile: the timeline, the latency distribution, and the throttling
    // finding's evidence (the share throttled per in-flight level).
    wait_long(&mut app, "id:timeline");
    wait_long(&mut app, "id:distribution");
    wait_long(&mut app, "text:Latency distribution");
    wait_long(&mut app, "id:evidence-throttling");
    wait_long(&mut app, "text:Share throttled by requests in flight");
    // The backend's requests by API, the retries, and the counts behind the numbers.
    wait_long(&mut app, "id:requests");
    assert!(app.shows("GetObject"), "{:?}", app.screen().texts());
    wait_long(&mut app, "id:retries");
    wait_long(&mut app, "text:/operations · .* requests/");
    // Details and the run's setup open on demand.
    app.press("id:details").unwrap();
    wait_long(&mut app, "id:details-body");
    wait_long(&mut app, "text:/Bytes asked for/");
    app.press("id:setup").unwrap();
    wait_long(&mut app, "id:setup-body");
    wait_long(&mut app, "text:/^oprobe run lab/");
    let runs = App::new(cx.paths()).list().unwrap().runs;
    assert_eq!(runs.len(), 1);
    wait_long(&mut app, &format!("id:run-{}", runs[0].id));
    wait_long(&mut app, "id:run-again");
}

#[test]
fn operator_toggles_a_layer_and_proves_it_against_the_lab() {
    let cx = Ctx::new();
    let id = cx.seed("lab://stragglers");
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:profile");
    app.press_key("ctrl+2").unwrap();
    wait_long(&mut app, "id:operator");
    app.wait_for("text:No changes yet: switch a layer or change a value above.")
        .unwrap();
    // Switch TailCutLayer into the stack: the change and its diff show.
    app.press("role:switch[layers.tail_cut]").unwrap();
    wait_long(&mut app, "text:layers.tail_cut = on");
    wait_long(&mut app, "text:/TailCutLayer/");
    // Prove it with a short budget: A/B replays stream in, then the verdict and the diff.
    app.clear("id:prove-budget").unwrap();
    app.type_text("id:prove-budget", "3m").unwrap();
    app.press("id:prove-start").unwrap();
    wait_long(&mut app, "text:/replays finished/");
    wait_long(&mut app, "id:prove-result");
    assert!(
        ["proved", "refuted", "inconclusive"]
            .iter()
            .any(|v| app.shows(v)),
        "{:?}",
        app.screen().texts()
    );
    assert!(app.shows("The proven diff"));
    app.wait_for("text:/Pair 1/").unwrap();
    // The pairs are drawn as they converge.
    wait_long(&mut app, "id:prove-chart");
    assert!(app.shows("Change of the objective, pair by pair"));
    // Prove saved its trial runs as children of the prove run: stored, hidden from the list.
    let app_ = App::new(cx.paths());
    let all = app_.list_all().unwrap().runs.len();
    assert!(
        all > app_.list().unwrap().runs.len(),
        "trials are stored and hidden"
    );
}

#[test]
fn the_language_choice_persists() {
    let cx = Ctx::new();
    {
        let mut app = cx.start();
        choose(&mut app, "简体中文");
        wait_long(&mut app, "text:还没有运行");
    }
    {
        let mut app = cx.start();
        wait_long(&mut app, "text:还没有运行");
        // Back to the system's language (English here), remembered too.
        choose(&mut app, "跟随系统");
        wait_long(&mut app, "text:No runs yet");
    }
    let mut app = cx.start();
    wait_long(&mut app, "text:No runs yet");
}

#[test]
fn a_context_menu_compares_and_deletes_runs() {
    let cx = Ctx::new();
    let a = cx.seed("lab://demo");
    let b = cx.seed("lab://throttle");
    let mut app = cx.start();
    choose(&mut app, "English");
    // The newest run is selected; another one joins it from its context menu.
    wait_long(&mut app, &format!("id:run-{b}"));
    app.press(&format!("id:run-{b}")).unwrap();
    app.right_click(&format!("id:run-{a}")).unwrap();
    app.choose_menu_item("Compare With Selected").unwrap();
    wait_long(&mut app, "id:compare-table");
    wait_long(&mut app, "text:2 runs compared");
    assert!(app.shows("Operator vs first"));
    wait_long(&mut app, "id:compare-charts");
    wait_long(&mut app, "id:compare-note");
    assert!(app.shows("Goodput over time") && app.shows("Latency distribution"));
    // Delete asks first; Cancel keeps the runs.
    app.right_click(&format!("id:run-{a}")).unwrap();
    app.choose_menu_item("Delete…").unwrap();
    wait_long(&mut app, "id:delete-dialog");
    wait_long(&mut app, "text:Delete 2 runs?");
    app.press("id:dialog-cancel").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    assert_eq!(App::new(cx.paths()).list().unwrap().runs.len(), 2);
    // One run alone, then deleted: the other stays and shows.
    app.press(&format!("id:run-{a}")).unwrap();
    app.right_click(&format!("id:run-{a}")).unwrap();
    app.choose_menu_item("Delete…").unwrap();
    wait_long(&mut app, &format!("text:Delete run {a}?"));
    app.press("id:delete-confirm").unwrap();
    app.wait_for_gone(&format!("id:run-{a}"), LONG).unwrap();
    let left = App::new(cx.paths()).list().unwrap().runs;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, b);
    wait_long(&mut app, "id:profile");
}

#[test]
fn the_menu_shortcuts_drive_the_window() {
    let cx = Ctx::new();
    let id = cx.seed("lab://demo");
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, &format!("id:run-{id}"));
    // New Run (mod+N) and Escape.
    app.press_key("ctrl+n").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // Hide and show the sidebar (mod+shift+S).
    app.press_key("ctrl+shift+s").unwrap();
    app.wait_for_gone("id:sidebar", LONG).unwrap();
    wait_long(&mut app, "id:sidebar-show");
    app.press("id:sidebar-show").unwrap();
    wait_long(&mut app, "id:sidebar");
    // Delete (mod+Backspace) asks first.
    app.press_key("ctrl+Backspace").unwrap();
    wait_long(&mut app, "id:delete-dialog");
    app.press("id:dialog-cancel").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
}

#[test]
fn the_macos_menu_bar_opens_settings_and_new_run() {
    // On macOS the menus are the system's menu bar, built from the app's items with ⌘
    // shortcuts: a keystroke that does not parse there left the app with the default menu (no
    // Settings, no shortcuts).
    let cx = Ctx::new();
    let mut app = cx.start_on(Os::MacOs, Locale::EN_US);
    wait_long(&mut app, "text:No runs yet");
    app.press_key("cmd+,").unwrap();
    wait_long(&mut app, "id:settings-dialog");
    app.press("id:dialog-done").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    app.press_key("cmd+n").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // ⌃⌘S hides the sidebar.
    app.press_key("ctrl+cmd+s").unwrap();
    app.wait_for_gone("id:sidebar", LONG).unwrap();
}
