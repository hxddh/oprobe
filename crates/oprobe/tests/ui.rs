#![cfg(feature = "app")]
//! The app on the real nsh engine (headless), driving the real service layer against the
//! local lab: the window's structure in both languages, a run started from New Run streaming
//! into the sidebar and opening in Profile, a layer toggled and proved in Operator, Compare
//! and Delete (to the trash, with Undo) from a run's context menu, the menu shortcuts, the
//! language set in Settings and remembered, the sidebar as a keyboard listbox, New Run planning
//! a real target first, quitting during a run, and the platform branches run as macOS and as
//! Windows (menus, shortcuts, Settings, alerts, button order). Each test has its own data
//! directory; tests run one at a time (the launch options are process-wide). Waits are bounded
//! only to fail instead of hanging; nothing asserts on wall-clock time.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nsh_core::element::DialogButtonStyle;
use nsh_core::platform::{Idiom, Locale, LocaleSettings, Os};
use nsh_engine::{EngineConfig, Environment, Points, Root, Size};
use nsh_testing::{DialogAnswer, Headless};
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

    /// Start the app when it remembers the sidebar hidden.
    fn start_without_sidebar(&self) -> Headless {
        let mut app = Headless::start(self.config(Os::Linux, Locale::EN_US)).unwrap();
        app.wait_for("id:sidebar-show").unwrap();
        app
    }

    fn start_on(&self, os: Os, locale: Locale) -> Headless {
        let mut app = Headless::start(self.config(os, locale)).unwrap();
        app.wait_for("id:sidebar").unwrap();
        app
    }

    fn config(&self, os: Os, locale: Locale) -> EngineConfig {
        let viewport = Size::new(Points(1360.0), Points(900.0));
        EngineConfig {
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
        }
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

/// Whether a view `sel` (a selector: `id:`, `role:`, `text:`) is on screen.
fn has(app: &Headless, sel: &str) -> bool {
    app.screen().matches(sel).unwrap_or(false)
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
    // The empty state's one action says what it does (U7).
    wait_long(&mut app, "text:剖析实验室…");
}

/// Settings says where your own storage goes: an op.toml file and credentials from the
/// environment, with the docs one press away (U7).
#[test]
fn settings_points_to_your_storage() {
    let cx = Ctx::new();
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "text:Profile the lab…");
    app.press_key("ctrl+,").unwrap();
    wait_long(&mut app, "id:settings-dialog");
    wait_long(&mut app, "text:Your storage");
    wait_long(&mut app, "text:/op\\.toml file.*AWS_\\*/");
    assert!(has(&app, "id:settings-storage-docs"));
    app.press("id:dialog-done").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
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
    assert!(!has(&app, "id:new-run-dialog"));
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
    // Prove it with a short budget: A/B trials stream in, then the verdict and the diff.
    app.clear("id:prove-budget").unwrap();
    app.type_text("id:prove-budget", "3m").unwrap();
    app.press("id:prove-start").unwrap();
    wait_long(&mut app, "text:/trials finished/");
    wait_long(&mut app, "id:prove-result");
    assert!(
        ["improved", "worse", "inconclusive"]
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
    // Delete asks first with the system's alert, Cancel its default; Cancel keeps the runs.
    app.answer_dialogs_myself();
    app.right_click(&format!("id:run-{a}")).unwrap();
    app.choose_menu_item("Delete…").unwrap();
    let d = app.pending_dialog().expect("Delete asks first").clone();
    assert_eq!(d.title.as_deref(), Some("Delete 2 runs?"));
    assert_cancel_is_default(&d);
    app.answer_dialog(DialogAnswer::Press("Cancel".into()))
        .unwrap();
    assert_eq!(App::new(cx.paths()).list().unwrap().runs.len(), 2);
    // One run alone, then deleted: it goes to the trash, the other stays and shows.
    app.press(&format!("id:run-{a}")).unwrap();
    app.right_click(&format!("id:run-{a}")).unwrap();
    app.choose_menu_item("Delete…").unwrap();
    let d = app.pending_dialog().expect("Delete asks first").clone();
    assert_eq!(
        d.title.as_deref(),
        Some(format!("Delete run {a}?").as_str())
    );
    app.answer_dialog(DialogAnswer::Press("Delete".into()))
        .unwrap();
    app.wait_for_gone(&format!("id:run-{a}"), LONG).unwrap();
    let left = App::new(cx.paths()).list().unwrap().runs;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, b);
    wait_long(&mut app, "id:profile");
    assert!(
        cx.dir.path().join("data/trash").is_dir(),
        "in oprobe's trash"
    );
    // Undo brings it back.
    wait_long(&mut app, "id:notice-undo");
    app.press("id:notice-undo").unwrap();
    wait_long(&mut app, &format!("id:run-{a}"));
    assert_eq!(App::new(cx.paths()).list().unwrap().runs.len(), 2);
}

/// The alert has a Cancel button (the platform makes it the default) and a destructive one.
fn assert_cancel_is_default(d: &nsh_core::element::Dialog) {
    assert!(
        d.buttons
            .iter()
            .any(|b| b.style == DialogButtonStyle::Cancel && b.text == "Cancel"),
        "{d:?}"
    );
    assert!(
        d.buttons
            .iter()
            .any(|b| b.style == DialogButtonStyle::Destructive),
        "{d:?}"
    );
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
    // Delete (the Delete key off macOS) asks first; not while a field edits.
    app.answer_dialogs_myself();
    app.press_key("Delete").unwrap();
    let d = app.pending_dialog().expect("Delete asks first").clone();
    assert_cancel_is_default(&d);
    app.answer_dialog(DialogAnswer::Press("Cancel".into()))
        .unwrap();
    app.press_key("ctrl+n").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    app.press_key("Delete").unwrap();
    assert!(app.pending_dialog().is_none(), "Delete edits the field");
    app.press_key("Escape").unwrap();
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
    // Settings is a window of its own (not modal: no Done), closed by its close button.
    app.press_key("cmd+,").unwrap();
    wait_long(&mut app, "id:settings-dialog");
    assert!(app.window_state("settings").unwrap().is_some());
    assert!(!has(&app, "id:dialog"), "not a dialog over the main window");
    app.press_window_close("settings").unwrap();
    app.wait_for_gone("id:settings-dialog", LONG).unwrap();
    app.press_key("cmd+n").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // ⌃⌘S hides the sidebar.
    app.press_key("ctrl+cmd+s").unwrap();
    app.wait_for_gone("id:sidebar", LONG).unwrap();
}

/// The order of two texts on screen (the tree's order: leading to trailing in a row).
fn before(app: &Headless, a: &str, b: &str) -> bool {
    let t = app.screen().texts();
    let (i, j) = (
        t.iter()
            .position(|x| *x == a)
            .unwrap_or_else(|| panic!("{a}: {t:?}")),
        t.iter()
            .position(|x| *x == b)
            .unwrap_or_else(|| panic!("{b}: {t:?}")),
    );
    i < j
}

#[test]
fn the_macos_window_follows_the_mac() {
    let cx = Ctx::new();
    let a = cx.seed("lab://demo");
    let b = cx.seed("lab://demo");
    let mut app = cx.start_on(Os::MacOs, Locale::EN_US);
    wait_long(&mut app, &format!("id:run-{b}"));
    // No primary menu: the menus are the system's menu bar.
    assert!(!has(&app, "id:primary-menu"));
    // Dialog buttons end with the default one.
    app.press_key("cmd+n").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    assert!(before(&app, "Cancel", "Run"), "Cancel, then Run");
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // ⌘⌫ deletes the selection, asking first; Delete alone does nothing here.
    app.answer_dialogs_myself();
    app.press(&format!("id:run-{a}")).unwrap();
    app.press_key("cmd+Backspace").unwrap();
    let d = app.pending_dialog().expect("⌘⌫ asks first").clone();
    assert_cancel_is_default(&d);
    app.answer_dialog(DialogAnswer::Press("Cancel".into()))
        .unwrap();
    // ⌘-click adds a run to the selection: Compare.
    assert!(
        app.screen()
            .accessibility_state(&format!("id:run-{a}"))
            .unwrap()
            .selected
    );
    app.hold_keys("cmd", true).unwrap();
    app.press(&format!("id:run-{b}")).unwrap();
    app.hold_keys("cmd", false).unwrap();
    wait_long(&mut app, "text:2 runs compared");
    assert_eq!(App::new(cx.paths()).list().unwrap().runs.len(), 2);
}

#[test]
fn the_windows_window_follows_windows() {
    let cx = Ctx::new();
    let id = cx.seed("lab://demo");
    let mut app = cx.start_on(Os::Windows, Locale::EN_US);
    wait_long(&mut app, &format!("id:run-{id}"));
    // Settings is a dialog (Ctrl+,), closed with its close button.
    app.press_key("ctrl+,").unwrap();
    wait_long(&mut app, "id:settings-dialog");
    assert!(has(&app, "text:Settings"));
    app.press("id:dialog-done").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // Dialog buttons start with the default one.
    app.press_key("ctrl+n").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    assert!(before(&app, "Run", "Cancel"), "Run, then Cancel");
    // Menu actions wait while a dialog is open: Ctrl+, does not replace New Run.
    app.press_key("ctrl+,").unwrap();
    assert!(has(&app, "id:new-run-dialog"));
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // The primary menu is flat: About opens the About dialog, Exit is there.
    app.press("id:primary-menu").unwrap();
    let labels: Vec<String> = app
        .menu()
        .expect("the primary menu")
        .iter()
        .map(|e| format!("{e:?}"))
        .collect();
    assert!(labels.iter().any(|l| l.contains("Exit")), "{labels:?}");
    // Help has the glossary of the terms a profile prints (U5).
    assert!(
        labels.iter().any(|l| l.contains("Glossary of Terms")),
        "{labels:?}"
    );
    app.choose_menu_item("About oprobe").unwrap();
    wait_long(&mut app, "id:about-dialog");
    assert!(has(
        &app,
        &format!("text:Version {}", env!("CARGO_PKG_VERSION"))
    ));
    app.press("id:dialog-done").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // A run's context menu shows its file in Explorer, in Windows' words.
    app.right_click(&format!("id:run-{id}")).unwrap();
    let labels: Vec<String> = app
        .menu()
        .expect("the context menu")
        .iter()
        .map(|e| format!("{e:?}"))
        .collect();
    assert!(
        labels.iter().any(|l| l.contains("Show in Explorer")),
        "{labels:?}"
    );
    app.choose_menu_item("Copy Run ID").unwrap();
    // Delete asks with Cancel the default.
    app.answer_dialogs_myself();
    app.press(&format!("id:run-{id}")).unwrap();
    app.press_key("Delete").unwrap();
    let d = app.pending_dialog().expect("Delete asks first").clone();
    assert_cancel_is_default(&d);
    app.answer_dialog(DialogAnswer::Press("Cancel".into()))
        .unwrap();
}

#[test]
fn the_sidebar_is_a_keyboard_listbox() {
    let cx = Ctx::new();
    let a = cx.seed("lab://demo");
    let b = cx.seed("lab://demo");
    let c = cx.seed("lab://demo");
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, &format!("id:run-{c}"));
    // One list of options, one stop of Tab.
    assert!(has(&app, "role:listbox"));
    assert_eq!(app.screen().count("role:option").unwrap(), 3);
    let selected = |app: &Headless, id: &str| {
        app.screen()
            .accessibility_state(&format!("id:run-{id}"))
            .unwrap()
            .selected
    };
    // Newest first: c, b, a. A press puts the focus on the list; ↓ moves the selection.
    app.press(&format!("id:run-{c}")).unwrap();
    app.press_key("down").unwrap();
    assert!(selected(&app, &b) && !selected(&app, &c));
    // Shift+↓ extends it: two runs show in Compare.
    app.press_key("shift+down").unwrap();
    wait_long(&mut app, "text:2 runs compared");
    assert!(selected(&app, &a) && selected(&app, &b));
    // Home goes to the first run alone; End to the last.
    app.press_key("home").unwrap();
    assert!(selected(&app, &c) && !selected(&app, &a));
    app.press_key("end").unwrap();
    assert!(selected(&app, &a) && !selected(&app, &c));
    // Type-select: the run whose short id starts with what is typed.
    let short = b.rsplit('-').next().unwrap().to_owned();
    for ch in short.chars().take(4) {
        app.press_key(&ch.to_string()).unwrap();
    }
    assert!(selected(&app, &b), "typed {short}");
    // Delete asks first.
    app.answer_dialogs_myself();
    app.press_key("Delete").unwrap();
    let d = app.pending_dialog().expect("Delete asks first").clone();
    assert_eq!(
        d.title.as_deref(),
        Some(format!("Delete run {b}?").as_str())
    );
    app.answer_dialog(DialogAnswer::Press("Cancel".into()))
        .unwrap();
    // The width and the sidebar's visibility are remembered.
    app.press_key("ctrl+shift+s").unwrap();
    app.wait_for_gone("id:sidebar", LONG).unwrap();
    drop(app);
    let mut app = cx.start_without_sidebar();
    wait_long(&mut app, "id:sidebar-show");
}

#[test]
fn new_run_plans_a_real_target_first() {
    let cx = Ctx::new();
    let mut app = cx.start();
    choose(&mut app, "English");
    app.press("id:new-run").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    // A lab run starts at once; a real target plans first.
    assert!(has(&app, "text:Run"));
    app.clear("id:run-operator").unwrap();
    app.type_text("id:run-operator", "memory://").unwrap();
    wait_long(&mut app, "text:Review Plan");
    app.press("id:run-start").unwrap();
    wait_long(&mut app, "id:run-plan-ready");
    wait_long(&mut app, "text:/^Writes .* objects under/");
    wait_long(&mut app, "text:Run");
    // A change of the form drops the plan.
    app.type_text("id:run-duration", "3s").unwrap();
    wait_long(&mut app, "text:Review Plan");
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
}

#[test]
fn quitting_during_a_run_asks_first() {
    let cx = Ctx::new();
    let mut app = cx.start();
    choose(&mut app, "English");
    app.press("id:new-run").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    app.clear("id:run-operator").unwrap();
    app.type_text("id:run-operator", "lab://demo").unwrap();
    app.type_text("id:run-duration", "60s").unwrap();
    app.press("id:run-start").unwrap();
    wait_long(&mut app, "id:stop");
    app.answer_dialogs_myself();
    // Ctrl+Q (GNOME's Quit) asks; Cancel keeps it running.
    app.press_key("ctrl+q").unwrap();
    let d = app.pending_dialog().expect("Quit asks first").clone();
    assert_eq!(
        d.title.as_deref(),
        Some("A run is in progress. Stop it and quit?")
    );
    assert!(d.buttons.iter().any(|b| b.text == "Stop and Quit"));
    assert_cancel_is_default(&d);
    app.answer_dialog(DialogAnswer::Press("Cancel".into()))
        .unwrap();
    assert!(has(&app, "id:stop"));
    // The window's close button asks too and leaves the window open.
    app.press_window_close("main").unwrap();
    assert!(app.pending_dialog().is_some(), "closing asks first");
    app.answer_dialog(DialogAnswer::Press("Cancel".into()))
        .unwrap();
    assert!(app.window_state("main").unwrap().is_some_and(|w| w.visible));
    assert!(has(&app, "id:stop"));
    app.press("id:stop").unwrap();
    app.wait_for_gone("id:stop", LONG).unwrap();
}

/// A run long enough to brush: three seconds of a lab scenario (several verbs).
fn seed_long(cx: &Ctx, operator: &str) -> String {
    let app = App::new(cx.paths());
    let req = RunRequest {
        operators: vec![operator.to_owned()],
        workload: WorkloadInput {
            duration: Some("3s".into()),
            // Past the throttling lab's limit, so it throttles.
            concurrency: Some(32),
            ..small()
        },
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

fn right(by: f32) -> nsh_core::Point<Points> {
    nsh_core::geometry::point(Points(by), Points(0.0))
}

#[test]
fn a_drag_selects_a_range_and_the_profile_recomputes_it() {
    let cx = Ctx::new();
    let id = seed_long(&cx, "lab://throttle");
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:chart-goodput");
    wait_long(&mut app, "id:range-hint");
    // Every chart says what it shows to a screen reader.
    wait_long(&mut app, "label:/^Goodput over /");
    wait_long(&mut app, "label:/^Requests per second over /");
    wait_long(&mut app, "label:/operations per cell$/");
    // A drag across the goodput chart selects a range: the numbers are the range's.
    app.drag("id:chart-goodput", right(160.0)).unwrap();
    wait_long(&mut app, "id:range-bar");
    wait_long(&mut app, "text:/^Selected /");
    wait_long(
        &mut app,
        "text:/operations · .* requests in the .* selected/",
    );
    // Clear drops it.
    app.press("id:range-clear").unwrap();
    app.wait_for_gone("id:range-bar", LONG).unwrap();
    wait_long(&mut app, "text:/operations · .* requests · .* measured/");
    // A drag across the heatmap selects one too, and Escape drops it.
    app.drag("id:heatmap", right(-120.0)).unwrap();
    wait_long(&mut app, "id:range-bar");
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:range-bar", LONG).unwrap();
}

#[test]
fn findings_apis_and_the_verb_filter_drill_down() {
    let cx = Ctx::new();
    let id = seed_long(&cx, "lab://throttle");
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:finding-throttling");
    // A finding's examples open on demand, request by request.
    assert!(!app.shows("id:examples"));
    wait_long(&mut app, "text:/^Show \\d+ examples$/");
    app.press("id:examples-throttling").unwrap();
    wait_long(&mut app, "id:examples");
    wait_long(&mut app, "text:Hide examples");
    // An API lists its slowest requests; one of them opens its operation's waterfall.
    app.press("id:api-row-0").unwrap();
    wait_long(&mut app, "id:api-drill");
    wait_long(&mut app, "text:/^Slowest .* requests$/");
    app.press("id:api-req-0").unwrap();
    wait_long(&mut app, "id:drill-waterfall");
    // The timeline follows the verb filter; what it does not split by verb says so.
    wait_long(&mut app, "id:verb-filter");
    app.press("id:verb-filter >> text:stat").unwrap();
    wait_long(&mut app, "text:Goodput · stat");
    wait_long(&mut app, "text:Requests per second · all verbs");
    app.press("id:verb-filter >> text:All verbs").unwrap();
    app.wait_for_gone("text:Goodput · stat", LONG).unwrap();
    // A click on a heatmap cell (a press that barely moves) lists the operations behind it.
    app.drag("id:heatmap", right(1.0)).unwrap();
    wait_long(&mut app, "id:cell-drill");
    wait_long(&mut app, "text:/^Slowest operations at /");
}

#[test]
fn the_profile_shows_every_captured_signal_where_it_explains_something() {
    let cx = Ctx::new();
    let id = seed_long(&cx, "lab://stragglers");
    // The slowest operation as it reads when its async call stack was captured in flight.
    let shown = App::new(cx.paths())
        .show(oprobe::app::RunRef::from(id.as_str()))
        .unwrap();
    let mut run = *shown.full.unwrap();
    let slowest = run.report.slowest[0];
    run.ops
        .iter_mut()
        .find(|o| o.op_id == slowest)
        .unwrap()
        .await_tree = Some("read.full [400ms]\n  GetObject [400ms]".into());
    oprobe::store::runs::write_run_file(std::path::Path::new(&shown.path), &run).unwrap();
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:chart-inflight");
    // In flight with the connections dialed per interval and, faint, the client's CPU.
    wait_long(&mut app, "id:timeline >> text:new connections");
    wait_long(&mut app, "id:timeline >> text:client CPU (cores)");
    // The same statistics as `oprobe show` in the API table.
    wait_long(&mut app, "id:requests >> text:Total p50 / p99");
    // Each verb with its rates.
    wait_long(&mut app, "id:verbs >> text:/MiB\\/s · .* req\\/op$/");
    // The consumer's stall is its own segment of the waterfall.
    wait_long(&mut app, "id:slowest >> text:stall");
    // Connections: one model with the CLI's, the kernel's view where the kernel gives it.
    wait_long(&mut app, "id:connections");
    #[cfg(target_os = "linux")]
    wait_long(&mut app, "id:conn-tcp");
    // A slow operation's async call stack opens under its waterfall on demand.
    wait_long(&mut app, "id:slowest >> id:await-tree-toggle");
    assert!(!has(&app, "id:await-tree-body"));
    app.press("id:slowest >> id:await-tree-toggle").unwrap();
    wait_long(&mut app, "id:await-tree-body");
}
