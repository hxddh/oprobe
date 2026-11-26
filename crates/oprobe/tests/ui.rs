#![cfg(feature = "app")]
//! The app on the real nsh engine (headless), driving the real service layer against the
//! local lab: the window's structure in both languages, a run started from New Run streaming
//! into the sidebar and opening in Profile, a layer toggled and proved in Operator, Compare
//! and Delete (to the trash, with Undo) from a run's context menu, the menu shortcuts, the
//! language set in Settings and remembered, the sidebar as a keyboard listbox, New Run planning
//! a real target first, quitting during a run (the wait for its cleanup, and what is said when
//! that wait gives up), and the platform branches run as macOS and as
//! Windows (menus, shortcuts, Settings, alerts, button order). Each test has its own data
//! directory; tests run one at a time (the launch options are process-wide). Waits are bounded
//! only to fail instead of hanging; nothing asserts on wall-clock time.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nsh_core::element::DialogButtonStyle;
use nsh_core::platform::{Idiom, Locale, LocaleSettings, Os};
use nsh_engine::{EngineConfig, Environment, Points, Root, Size};
use nsh_testing::{DialogAnswer, DragOptions, Headless};
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
            stop_grace: None,
        });
    }

    /// Start the app so that quitting gives up on the jobs it stops at once.
    fn start_giving_up_at_once(&self) -> Headless {
        oprobe::ui::set_launch(oprobe::ui::Launch {
            paths: Some(self.paths()),
            open: None,
            stop_grace: Some(Duration::ZERO),
        });
        self.start()
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

    /// The app, as soon as its window shows. The run it opens at start still loads off the
    /// app's thread: nothing waits for it here, and a menu shortcut may be pressed at once
    /// (the menu bar is no longer rebuilt when the run has loaded; 9.2's tests waited for
    /// that rebuild, which lost a shortcut pressed across it).
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
fn proven_change_saves_back_to_op_toml() {
    let cx = Ctx::new();
    let op_file = cx.dir.path().join("op.toml");
    std::fs::write(
        &op_file,
        "url = \"memory://\"\n[options]\nroot = \"/keep\"\n",
    )
    .unwrap();
    let id = cx.seed(op_file.to_str().unwrap());
    cx.launch(Some(id));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:profile");
    app.press_key("ctrl+2").unwrap();
    wait_long(&mut app, "id:operator");
    app.press("role:switch[layers.retry.jitter]").unwrap();
    wait_long(&mut app, "text:layers.retry.jitter = on");
    app.press("id:prove-start").unwrap();
    wait_long(&mut app, "id:prove-result");
    wait_long(&mut app, "id:save-operator");
    app.press("id:save-operator").unwrap();
    wait_long(&mut app, "text:Saved to op.toml.");
    let saved = std::fs::read_to_string(&op_file).unwrap();
    assert!(saved.contains("jitter = true"), "{saved}");
    assert!(saved.contains("root = \"/keep\""), "{saved}");
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
    // The tail of the latency, requests, retries, failures and throttling side by side; p99 and
    // requests over time; each API's requests and TTFB p99.
    for row in [
        "p999",
        "requests_per_s",
        "retries",
        "failed",
        "throttled_count",
    ] {
        wait_long(&mut app, &format!("id:compare-row-{row}"));
    }
    wait_long(&mut app, "id:compare-p99");
    wait_long(&mut app, "id:compare-requests");
    wait_long(&mut app, "id:compare-apis");
    wait_long(&mut app, "id:compare-apis >> text:GetObject");
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

/// T6. The menu bar is rebuilt only when something it shows changes. 9.2 rebuilt it whenever
/// the selection changed, and nsh gives the items of a rebuilt bar new ids: a shortcut typed
/// right after an arrow key in the sidebar named an item that was gone and did nothing.
#[test]
fn a_shortcut_typed_ahead_of_a_selection_change_is_not_lost() {
    let cx = Ctx::new();
    let a = cx.seed("lab://demo");
    let b = cx.seed("lab://demo");
    let mut app = cx.start();
    let selected = |app: &Headless, id: &str| {
        app.screen()
            .accessibility_state(&format!("id:run-{id}"))
            .unwrap()
            .selected
    };
    // Newest first: b, a. A press puts the focus on the list.
    app.press(&format!("id:run-{b}")).unwrap();
    // ↓ and Ctrl+2 typed ahead: the app has not handled the arrow (the selection moving to
    // `a`, which then loads) when the shortcut comes.
    app.press_keys(&["down", "ctrl+2"]).unwrap();
    wait_long(&mut app, "id:operator");
    assert!(selected(&app, &a) && !selected(&app, &b));
    // And back, across the load of the other run.
    app.press_keys(&["up", "ctrl+1"]).unwrap();
    wait_long(&mut app, "id:profile");
    assert!(selected(&app, &b) && !selected(&app, &a));
    // A change of what the bar shows still rebuilds it (nsh 0.7 has no stable item ids): one
    // run selected becoming several enables Compare, so only a shortcut typed after the app
    // has handled that arrow is sure to arrive.
    app.press_key("shift+down").unwrap();
    wait_long(&mut app, "text:2 runs compared");
    app.press_keys(&["ctrl+shift+s"]).unwrap();
    app.wait_for_gone("id:sidebar", LONG).unwrap();
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

/// Start a run of `lab://demo` long enough to quit during it, and ask to quit: the alert.
fn quit_during_a_run(app: &mut Headless) {
    choose(app, "English");
    app.press("id:new-run").unwrap();
    wait_long(app, "id:new-run-dialog");
    app.clear("id:run-operator").unwrap();
    app.type_text("id:run-operator", "lab://demo").unwrap();
    app.type_text("id:run-duration", "60s").unwrap();
    app.press("id:run-start").unwrap();
    wait_long(app, "id:stop");
    app.answer_dialogs_myself();
    app.press_key("ctrl+q").unwrap();
    assert!(app.pending_dialog().is_some(), "Quit asks first");
}

/// K1-10. "Stop and Quit" stops the run and quits once it has removed its test data: the
/// Quitting dialog shows the wait, and the app quits by itself. (9.2 quit at once and then
/// blocked the app's thread, the window frozen, for up to 30 s.)
#[test]
fn stop_and_quit_waits_for_the_cleanup_and_then_quits() {
    let cx = Ctx::new();
    let mut app = cx.start();
    quit_during_a_run(&mut app);
    app.answer_dialog(DialogAnswer::Press("Stop and Quit".into()))
        .unwrap();
    // Not gone yet, or gone already: never quit with the job still running.
    let waiting = !app.quit();
    if waiting {
        assert!(has(&app, "id:quit-dialog"), "the wait is shown");
        assert!(!has(&app, "id:quit-anyway"), "it has not given up");
        // The wait is not cancelled: its job is stopped already.
        app.press_key("Escape").unwrap();
        assert!(app.quit() || has(&app, "id:quit-dialog"));
    }
    let deadline = std::time::Instant::now() + LONG;
    while !app.quit() {
        assert!(std::time::Instant::now() < deadline, "the app did not quit");
        app.wait(Duration::from_millis(50)).unwrap();
    }
    // Its job is over (a run stopped before it measured anything saves nothing; one stopped
    // later is saved as a partial run).
    assert!(!oprobe::ui::jobs::busy());
    let runs = App::new(cx.paths()).list().unwrap().runs;
    assert!(runs.iter().all(|r| r.partial.is_some()), "{runs:?}");
}

/// K1-10. When the wait gives up (here at once: `Launch::stop_grace`), the dialog says what
/// was left and where, and quits only when asked. (9.2 printed one line to a stderr a
/// desktop app does not have, and quit.)
#[test]
fn quitting_says_what_was_left_when_it_gives_up() {
    let cx = Ctx::new();
    let mut app = cx.start_giving_up_at_once();
    quit_during_a_run(&mut app);
    app.answer_dialog(DialogAnswer::Press("Stop and Quit".into()))
        .unwrap();
    wait_long(&mut app, "id:quit-anyway");
    assert!(app.shows("The cleanup did not finish"));
    wait_long(
        &mut app,
        "text:/^The run of lab:\\/\\/demo (had not stopped|was still removing its test data)\\.$/",
    );
    wait_long(
        &mut app,
        "text:/oprobe-run-…\\/ prefix may be left on the target/",
    );
    app.press("id:quit-anyway").unwrap();
    assert!(app.quit());
    // Given up on: quitting no longer waits for it.
    assert!(!oprobe::ui::jobs::busy());
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
    app.drag("id:chart-goodput", right(160.0), DragOptions::default())
        .unwrap();
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
    app.drag("id:heatmap", right(-120.0), DragOptions::default())
        .unwrap();
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
    // Throughput over time: its own chart, or goodput's right axis when every operation moves
    // the same bytes (`TimelineView::layout`) — then a verb's goodput is not every
    // operation's, and it has its chart again.
    wait_long(&mut app, "id:chart-throughput");
    app.press("id:verb-filter >> text:All verbs").unwrap();
    app.wait_for_gone("text:Goodput · stat", LONG).unwrap();
    // The throttled responses (and the retries) on their own axis.
    wait_long(&mut app, "id:chart-problems");
    wait_long(&mut app, "label:/^Problems per second over /");
    wait_long(&mut app, "id:timeline >> text:throttled responses");
    // The latency distribution is a tail plot with p99.9 marked.
    wait_long(
        &mut app,
        "id:distribution >> text:/^Share of operations slower than a latency/",
    );
    // A click on a heatmap cell (a press that barely moves) lists the operations behind it.
    app.drag("id:heatmap", right(1.0), DragOptions::default())
        .unwrap();
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
    // In flight with the connections dialed per interval (the client's CPU has its own chart,
    // drawn when the client was saturated).
    wait_long(&mut app, "id:timeline >> text:new connections");
    // The same statistics as `oprobe show` in the API table.
    wait_long(&mut app, "id:requests >> text:Total p50 / p99");
    // Each verb with its rates.
    wait_long(&mut app, "id:verbs >> text:/MiB\\/s · .* req\\/op$/");
    // The consumer's stall is its own segment of the waterfall.
    wait_long(&mut app, "id:slowest >> text:stall");
    // Connections: one model with the CLI's, the kernel's view where the kernel gives it; the
    // dials counted once, with their window.
    wait_long(&mut app, "id:connections");
    wait_long(&mut app, "id:conn-dialed >> text:/in the window$/");
    assert!(!app.shows("HTTP/2 0"), "no protocol that carried nothing");
    #[cfg(target_os = "linux")]
    {
        wait_long(&mut app, "id:conn-tcp");
        wait_long(&mut app, "id:conn-min-rtt");
    }
    // A finding's evidence is labelled, its values in the tiles' format.
    wait_long(
        &mut app,
        "id:finding-tail_stragglers >> text:Typical time to first byte",
    );
    wait_long(&mut app, "id:finding-tail_stragglers >> text:APIs affected");
    wait_long(&mut app, "id:finding-tail_stragglers >> text:/^\\d.* ms$/");
    // Details: the bytes on the wire, from the kernel's counters.
    app.press("id:details").unwrap();
    wait_long(&mut app, "id:details-body");
    #[cfg(target_os = "linux")]
    wait_long(&mut app, "id:bytes-wire");
    wait_long(&mut app, "id:details-body >> text:/ – /");
    // A slow operation's async call stack opens under its waterfall on demand (every slow
    // operation of a live run has one; the slowest is first).
    let toggle = "id:slowest >> id:await-tree-toggle >> nth=0";
    wait_long(&mut app, toggle);
    assert!(!has(&app, "id:await-tree-body"));
    app.press(toggle).unwrap();
    wait_long(&mut app, "id:await-tree-body");
}

#[test]
fn concurrent_limit_toggle_seeds_default_and_stored_proves_reload_and_compare_trials() {
    let cx = Ctx::new();
    let id_a = cx.seed("lab://clean");
    let id_b = cx.seed("lab://throttle");
    // Run a short prove on id_a via the service layer so `<prove_id>.prove.json` and trial runs exist.
    {
        let svc = App::new(cx.paths());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let req = oprobe::app::ProveRequest {
            run: id_a.clone(),
            with: vec!["layers.retry.jitter=on".into()],
            trial_duration: Some("300ms".into()),
            budget: Some("1s".into()),
            ..Default::default()
        };
        let _ = rt
            .block_on(svc.prove(req, EventSink::silent(), Cancel::new()))
            .unwrap();
    }
    cx.launch(Some(id_a.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:profile");
    app.press_key("ctrl+2").unwrap();
    wait_long(&mut app, "id:operator");
    // Stored prove is loaded automatically for id_a!
    wait_long(&mut app, "id:prove-result");
    wait_long(&mut app, "id:compare-prove-trials");
    // Toggling `layers.concurrent_limit` seeds `http = 32` instead of snapping back off.
    app.press("role:switch[layers.concurrent_limit]").unwrap();
    wait_long(&mut app, "text:layers.concurrent_limit.http = 32");
    // Switching to id_b hides id_a's prove; switching back restores id_a's stored prove.
    app.press(&format!("id:run-{id_b}")).unwrap();
    app.wait_for_gone("id:prove-result", LONG).unwrap();
    app.press(&format!("id:run-{id_a}")).unwrap();
    wait_long(&mut app, "id:prove-result");
    // Comparing the stored prove's A/B trials opens Compare on those trial runs.
    app.press("id:compare-prove-trials").unwrap();
    wait_long(&mut app, "id:compare-table");
    wait_long(&mut app, "text:2 runs compared");
}

#[test]
fn standalone_prove_json_opens_without_origin_run_and_fields_clear_without_snapback() {
    let cx = Ctx::new();
    let id = cx.seed("lab://clean");
    let prove_file = {
        let svc = App::new(cx.paths());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let req = oprobe::app::ProveRequest {
            run: id.clone(),
            with: vec!["layers.retry.jitter=on".into()],
            trial_duration: Some("300ms".into()),
            budget: Some("1s".into()),
            ..Default::default()
        };
        let out = match rt
            .block_on(svc.prove(req, EventSink::silent(), Cancel::new()))
            .unwrap()
        {
            oprobe::app::ProveOutcome::Proved(p) => *p,
            oprobe::app::ProveOutcome::Plan(_) => unreachable!(),
        };
        // A sized prove: its sweep is plotted with the knee marked.
        let mut out = out;
        let mut trial = out.trials[0].clone();
        trial.kind = oprobe::app::prove::TrialKind::Sizing;
        trial.pair = None;
        for (v, feasible) in [("1", true), ("3", true), ("5", false)] {
            let mut t = trial.clone();
            t.value = Some(v.into());
            t.feasible = feasible;
            out.trials.insert(0, t);
        }
        out.sizing = Some(oprobe::app::prove::Sizing {
            path: "layers.retry.max_times".into(),
            value: "3".into(),
            start: "1".into(),
            feasible: true,
            unmeasured: None,
        });
        let external = cx.dir.path().join("shared.prove.json");
        std::fs::write(&external, serde_json::to_vec_pretty(&out).unwrap()).unwrap();
        // Delete both the origin run and the stored prove so `shared.prove.json` is truly standalone.
        svc.rm(&[
            oprobe::app::RunRef::from(out.prove_id.as_str()),
            oprobe::app::RunRef::from(id.as_str()),
        ])
        .unwrap();
        external
    };
    cx.launch(Some(prove_file.to_string_lossy().into_owned()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:operator");
    wait_long(&mut app, "id:prove-result");
    wait_long(&mut app, "text:/Pair 1/");
    wait_long(&mut app, "id:prove-sizing");
    wait_long(&mut app, "text:Sizing sweep · layers.retry.max_times");
    wait_long(&mut app, "id:prove-notes");
    // Typing into a parameter field and clearing it never snaps back to the base value.
    app.type_text("id:layers.retry.max_times", "7").unwrap();
    wait_long(&mut app, "text:layers.retry.max_times = 7");
    app.clear("id:layers.retry.max_times").unwrap();
    app.wait_for_gone("text:layers.retry.max_times = 7", LONG)
        .unwrap();
}

/// nsh 0.7's window model: the app root is an invisible host, so an app that declared no
/// window of its own would show nothing on a desktop. The app opens its one window, `main`,
/// with its content in it; on macOS Settings is a window of its own beside it.
#[test]
fn the_app_opens_its_main_window_with_its_content() {
    let cx = Ctx::new();
    let mut app = cx.start();
    assert_eq!(app.window_ids().unwrap(), vec!["main".to_owned()]);
    let state = app.window_state("main").unwrap().expect("main is open");
    assert!(state.visible, "{state:?}");
    let sidebar = app.screen().layout("id:sidebar").unwrap().expect("drawn");
    assert!(sidebar.bounds.size.width.0 > 0.0 && sidebar.bounds.size.height.0 > 0.0);
    drop(app);
    let mut app = cx.start_on(Os::MacOs, Locale::EN_US);
    app.press_key("cmd+,").unwrap();
    wait_long(&mut app, "id:settings-dialog");
    assert_eq!(
        app.window_ids().unwrap(),
        vec!["main".to_owned(), "settings".to_owned()]
    );
    // Closing the main window quits (one window: macOS too).
    app.press_window_close("settings").unwrap();
    app.wait_for_gone("id:settings-dialog", LONG).unwrap();
    app.press_window_close("main").unwrap();
    assert!(app.quit(), "closing the last window quits");
}

impl Ctx {
    /// The app on a terminal grid 200 columns wide and 40 rows high.
    fn start_in_terminal(&self) -> Headless {
        let viewport = Size::new(Points(200.0 * 8.0), Points(40.0 * 16.0));
        let config = EngineConfig {
            viewport,
            data_dir: Some(self.dir.path().join("app")),
            environment: Environment {
                window: viewport,
                os: Os::Linux,
                idiom: Idiom::Terminal,
                locale: Locale::EN_US,
                locale_settings: LocaleSettings::of(Locale::EN_US),
                ..Environment::default()
            },
            ..EngineConfig::new(Root::of::<oprobe::ui::App>())
        };
        let mut app = Headless::start(config).unwrap();
        app.wait_for("id:sidebar").unwrap();
        app
    }
}

/// Where a view is drawn, top edge (root points).
fn top(screen: &nsh_testing::Screen, sel: &str) -> f32 {
    screen
        .layout(sel)
        .unwrap()
        .unwrap_or_else(|| panic!("{sel} is not drawn"))
        .bounds
        .origin
        .y
        .0
}

/// On a terminal the content pages from the keyboard (PageDown, Space, Home/End, j/k) wherever
/// the focus is, and every bar is at least a row high.
#[test]
fn the_terminal_pages_the_content_and_draws_its_bars() {
    let cx = Ctx::new();
    let id = seed_long(&cx, "lab://throttle");
    cx.launch(Some(id.clone()));
    let mut app = cx.start_in_terminal();
    wait_long(&mut app, "id:tiles");
    // Bars are a row high: a thinner one rounds to nothing on the grid.
    let bar = app
        .screen()
        .layout("id:time >> id:stage-bar >> nth=0")
        .unwrap()
        .expect("the time breakdown's bar");
    assert!(bar.bounds.size.height.0 >= 16.0, "{:?}", bar.bounds);
    let y0 = top(app.screen(), "id:tiles");
    app.press_key("pagedown").unwrap();
    app.wait_until("PageDown pages the content", LONG, |sc| {
        top(sc, "id:tiles") < y0 - 160.0
    })
    .unwrap();
    let paged = top(app.screen(), "id:tiles");
    app.press_key("k").unwrap();
    app.wait_until("k scrolls back up", LONG, |sc| top(sc, "id:tiles") > paged)
        .unwrap();
    app.press_key("home").unwrap();
    app.wait_until("Home goes to the top", LONG, |sc| {
        (top(sc, "id:tiles") - y0).abs() < 1.0
    })
    .unwrap();
    app.press_key("space").unwrap();
    app.wait_until("Space pages too", LONG, |sc| {
        top(sc, "id:tiles") < y0 - 160.0
    })
    .unwrap();
    // The sections below the first screen are reachable: End shows the last one.
    app.press_key("end").unwrap();
    app.wait_until("End reaches the connections", LONG, |sc| {
        top(sc, "id:connections") < 40.0 * 16.0
    })
    .unwrap();
}

/// The sections 9.1 added: the capacity, the slowest 1 %'s split, the traffic line, every
/// error code of an API with its errors and throttled columns, and what a range does not
/// recompute says so.
#[test]
fn the_profile_shows_capacity_the_tail_split_traffic_and_whole_run_labels() {
    let cx = Ctx::new();
    let id = seed_long(&cx, "lab://throttle");
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:profile");
    wait_long(&mut app, "id:time-tail");
    wait_long(
        &mut app,
        "id:time-tail >> text:The slowest 1 % of operations",
    );
    wait_long(&mut app, "id:traffic-line");
    wait_long(&mut app, "id:traffic >> text:/requests per operation$/");
    wait_long(&mut app, "id:requests >> text:Throttled");
    wait_long(&mut app, "id:requests >> text:/ · 503 ×/");
    wait_long(&mut app, "id:retries");
    wait_long(&mut app, "id:capacity");
    wait_long(&mut app, "text:Reused connections");
    // The CPU is drawn only when the client was saturated.
    assert!(!has(&app, "id:chart-cpu"));
    // A range: the time breakdown is the range's; retries, connections and traffic say they
    // are the whole run's.
    assert!(!has(&app, "id:whole-run"));
    app.drag("id:chart-goodput", right(160.0), DragOptions::default())
        .unwrap();
    wait_long(&mut app, "id:range-bar");
    wait_long(&mut app, "id:retries >> id:whole-run");
    wait_long(&mut app, "id:connections >> id:whole-run");
    wait_long(&mut app, "id:time-tail >> text:/whole run$/");
    // The verb filter applies to the heatmap too.
    app.press("id:range-clear").unwrap();
    app.press("id:verb-filter >> text:stat").unwrap();
    wait_long(&mut app, "text:/^Request latency over time · stat/");
}

/// An open-loop run says what it was asked for and what it achieved, and rules its target.
#[test]
fn an_open_loop_run_shows_its_schedule() {
    let cx = Ctx::new();
    let svc = App::new(cx.paths());
    let req = RunRequest {
        operators: vec!["lab://demo".into()],
        workload: WorkloadInput {
            rate: Some(200.0),
            duration: Some("2s".into()),
            ..small()
        },
        ..Default::default()
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let id = match rt
        .block_on(svc.run(req, EventSink::silent(), Cancel::new()))
        .unwrap()
    {
        RunOutcome::Run(set) => set.runs[0].run_id.clone(),
        RunOutcome::Plan(_) => unreachable!(),
    };
    cx.launch(Some(id));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:chart-goodput");
    // Missed arrivals are a problem of their own over time.
    wait_long(&mut app, "id:timeline >> text:missed arrivals");
    app.press("id:setup").unwrap();
    wait_long(&mut app, "id:setup-schedule");
    wait_long(
        &mut app,
        "id:setup-body >> text:/^target 200\\/s · achieved /",
    );
}

/// Profile `req` with the service layer directly; the run's id.
fn seed_with(cx: &Ctx, req: RunRequest) -> String {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt
        .block_on(App::new(cx.paths()).run(req, EventSink::silent(), Cancel::new()))
        .unwrap()
    {
        RunOutcome::Run(set) => set.runs[0].run_id.clone(),
        RunOutcome::Plan(_) => unreachable!(),
    }
}

/// Wait until the store holds `n` runs (the app's run was saved), then until the app lists it.
fn wait_for_runs(cx: &Ctx, app: &mut Headless, n: usize) {
    let svc = App::new(cx.paths());
    app.wait_until(&format!("{n} runs stored"), LONG, |_| {
        svc.list().is_ok_and(|l| l.runs.len() == n)
    })
    .unwrap();
    let newest = svc.list().unwrap().runs[0].id.clone();
    wait_long(app, &format!("id:run-{newest}"));
}

/// The stored runs' specs, oldest first.
fn specs(cx: &Ctx) -> Vec<oprobe::model::RunSpec> {
    svc_specs(cx)
}

/// The specs of the stored runs, oldest first.
fn svc_specs(cx: &Ctx) -> Vec<oprobe::model::RunSpec> {
    let svc = App::new(cx.paths());
    let mut runs = svc.list().unwrap().runs;
    runs.sort_by(|a, b| a.id.cmp(&b.id));
    runs.iter()
        .map(|r| svc.store().load(&r.id).unwrap().report.spec)
        .collect()
}

/// An open-loop workload with its own dataset, seed and key popularity, on an operator with
/// its own HTTP client, layers and read options: nothing of it has a field in New Run.
fn particular(operator: &str) -> RunRequest {
    RunRequest {
        operators: vec![operator.to_owned()],
        workload: WorkloadInput {
            preset: Some("range-reads".into()),
            rate: Some(150.0),
            concurrency: Some(3),
            objects: Some(6),
            object_size: Some("96KiB".into()),
            seed: Some(7),
            access: Some("zipf:1.1".into()),
            duration: Some("600ms".into()),
            warmup: Some("0s".into()),
            ..Default::default()
        },
        set: [
            "http.version=h1",
            "http.keep_alive=false",
            "layers.retry.max_times=5",
            "layers.concurrent_limit.http=12",
            "read.gap=64KiB",
        ]
        .map(str::to_owned)
        .to_vec(),
        ..Default::default()
    }
}

/// Run Again repeats the run as it ran. 9.1 rebuilt it from the New Run form's five fields
/// and a diff of the operator values the Operator page lists: the open-loop rate, the
/// dataset, the seed and the key popularity were lost (the run came back closed-loop on the
/// preset's defaults).
#[test]
fn run_again_repeats_the_whole_run_of_a_lab() {
    let cx = Ctx::new();
    let id = seed_with(&cx, particular("lab://clean"));
    cx.launch(Some(id));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:profile");
    // A lab run starts at once.
    app.press("id:run-again").unwrap();
    wait_for_runs(&cx, &mut app, 2);
    let specs = specs(&cx);
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[1].profile.workload.rate, Some(150.0));
    assert_eq!(specs[1], specs[0], "{}", specs[1].command());
}

/// A real target opens New Run on its plan first: the form shows the run's operator, preset,
/// duration and concurrency, says what else it keeps, and a field changed there changes only
/// that. Another operator is another run: nothing is carried over to it.
#[test]
fn run_again_on_a_real_target_keeps_what_the_form_has_no_field_for() {
    let cx = Ctx::new();
    let id = seed_with(&cx, particular("memory://"));
    cx.launch(Some(id));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:profile");
    app.press("id:run-again").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    wait_long(&mut app, "id:run-again-command");
    wait_long(
        &mut app,
        "text:/^oprobe run memory:\\/\\/ --preset range-reads -c 3 -d 600ms --rate 150 /",
    );
    wait_long(&mut app, "id:run-plan-ready");
    // A longer window, everything else as it ran.
    app.clear("id:run-duration").unwrap();
    app.type_text("id:run-duration", "700ms").unwrap();
    wait_long(&mut app, "text:Review Plan");
    assert!(has(&app, "id:run-again-command"));
    app.press("id:run-start").unwrap();
    wait_long(&mut app, "id:run-plan-ready");
    app.press("id:run-start").unwrap();
    wait_for_runs(&cx, &mut app, 2);
    let specs = specs(&cx);
    assert_eq!(specs.len(), 2);
    let mut expected = specs[0].clone();
    expected.profile.workload.duration = specs[1].profile.workload.duration;
    assert_eq!(specs[1].profile.workload.duration.to_string(), "700ms");
    assert_eq!(specs[1], expected, "{}", specs[1].command());
    // New Run afterwards is a new run, and so is another operator in a run being repeated.
    app.press("id:new-run").unwrap();
    wait_long(&mut app, "id:new-run-dialog");
    assert!(!has(&app, "id:run-again-command"));
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    wait_long(&mut app, "id:profile");
    app.press("id:run-again").unwrap();
    wait_long(&mut app, "id:run-again-command");
    app.clear("id:run-operator").unwrap();
    app.type_text("id:run-operator", "lab://clean").unwrap();
    app.wait_for_gone("id:run-again-command", LONG).unwrap();
    app.press_key("Escape").unwrap();
    app.wait_for_gone("id:dialog", LONG).unwrap();
    // K1-3. Another preset is another workload on the same operator: the operator's values
    // stay (9.2 dropped them with the rest, so a run on a local endpoint went to the real
    // service), the old workload's rate, dataset and seed go, and the row shows the command
    // the form sends now.
    app.press("id:run-again").unwrap();
    wait_long(&mut app, "id:run-again-command");
    app.press("id:run-preset").unwrap();
    wait_long(&mut app, "text:list-stat");
    app.press("text:list-stat").unwrap();
    wait_long(
        &mut app,
        "text:/^oprobe run memory:\\/\\/ --preset list-stat -c 3 -d 700ms --set /",
    );
    let command = app
        .screen()
        .texts()
        .into_iter()
        .find(|t| t.starts_with("oprobe run memory://"))
        .unwrap()
        .to_owned();
    for kept in [
        "http.version=h1",
        "http.keep_alive=false",
        "layers.retry.max_times=5",
        "layers.concurrent_limit.http=12",
        "read.gap=64KiB",
    ] {
        assert!(command.contains(&format!("--set {kept}")), "{command}");
    }
    for gone in [
        "--rate",
        "--objects",
        "--object-size",
        "--seed",
        "access=",
        "warmup=",
    ] {
        assert!(!command.contains(gone), "{command}");
    }
    // The form changed: its plan is reviewed again, then it runs.
    wait_long(&mut app, "text:Review Plan");
    app.press("id:run-start").unwrap();
    wait_long(&mut app, "id:run-plan-ready");
    app.press("id:run-start").unwrap();
    wait_for_runs(&cx, &mut app, 3);
    let third = &svc_specs(&cx)[2];
    assert_eq!(third.profile.operator, specs[0].profile.operator);
    assert_eq!(third.profile.workload.preset.slug(), "list-stat");
    assert_eq!(third.profile.workload.rate, None);
}
