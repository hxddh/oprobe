#![cfg(feature = "app")]
//! The app on the real nsh engine (headless), driving the real service layer against the
//! local lab: every view in both languages, a run streaming into Runs and opening in Profile,
//! a layer toggled and proved in Operator, the language choice remembered. Each test has its
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
        let viewport = Size::new(Points(1360.0), Points(900.0));
        let config = EngineConfig {
            viewport,
            data_dir: Some(self.dir.path().join("app")),
            environment: Environment {
                window: viewport,
                os: Os::Linux,
                idiom: Idiom::Desktop,
                locale,
                locale_settings: LocaleSettings::of(locale),
                ..Environment::default()
            },
            ..EngineConfig::new(Root::of::<oprobe::ui::App>())
        };
        let mut app = Headless::start(config).unwrap();
        app.wait_for("id:nav.runs").unwrap();
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

/// Choose a language in the bar.
fn choose(app: &mut Headless, lang: &str) {
    app.press(&format!("text:{lang}")).unwrap();
}

#[test]
fn every_view_renders_in_both_languages() {
    let cx = Ctx::new();
    let id = cx.seed("lab://demo");
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    let views = [
        ("nav.runs", "text:NEW RUN", "text:新建运行"),
        (
            "nav.profile",
            "text:WHERE THE TIME WENT",
            "text:时间花在哪里",
        ),
        ("nav.operator", "text:LAYERS", "text:层"),
        ("nav.compare", "text:RUNS TO COMPARE", "text:要对比的运行"),
    ];
    for (nav, en, _) in views {
        app.press(&format!("id:{nav}")).unwrap();
        wait_long(&mut app, en);
    }
    // Profile shows the run's numbers, untranslated.
    app.press("id:nav.profile").unwrap();
    wait_long(&mut app, &format!("text:{id}"));
    assert!(app.shows("Goodput"));
    choose(&mut app, "中文");
    for (nav, _, zh) in views {
        app.press(&format!("id:{nav}")).unwrap();
        wait_long(&mut app, zh);
    }
    app.press("id:nav.profile").unwrap();
    wait_long(&mut app, "text:有效吞吐");
    assert!(app.shows(&id), "ids stay as they are");
    // Compare with nothing picked explains itself in both languages.
    app.press("id:nav.compare").unwrap();
    wait_long(&mut app, "text:请选择两次或更多运行");
    choose(&mut app, "English");
    wait_long(&mut app, "text:Pick two runs or more");
}

#[test]
fn the_app_follows_the_system_language() {
    let cx = Ctx::new();
    let mut app = cx.start_in(Locale::parse("zh-CN").unwrap());
    wait_long(&mut app, "text:新建运行");
    wait_long(&mut app, "text:还没有运行");
}

#[test]
fn a_lab_run_streams_into_runs_and_opens_in_profile() {
    let cx = Ctx::new();
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "text:No runs yet");
    app.clear("id:run-operator").unwrap();
    app.type_text("id:run-operator", "lab://throttle").unwrap();
    app.type_text("id:run-duration", "2s").unwrap();
    app.press("id:run-start").unwrap();
    // It streams in: the live panel with its sparkline, the button reads Stop.
    wait_long(&mut app, "id:live-run");
    assert!(app.shows("LIVE"));
    app.wait_for("role:button[Stop]").unwrap();
    // It finishes, is saved and opens in Profile.
    wait_long(&mut app, "id:profile");
    wait_long(&mut app, "text:WHERE THE TIME WENT");
    let runs = App::new(cx.paths()).list().unwrap().runs;
    assert_eq!(runs.len(), 1);
    assert!(app.shows(&runs[0].id));
    app.press("id:nav.runs").unwrap();
    wait_long(&mut app, "text:HISTORY · 1 RUNS");
    wait_long(&mut app, &format!("role:button[Open run {}]", runs[0].id));
}

#[test]
fn operator_toggles_a_layer_and_proves_it_against_the_lab() {
    let cx = Ctx::new();
    let id = cx.seed("lab://stragglers");
    cx.launch(Some(id.clone()));
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, "id:profile");
    app.press("id:nav.operator").unwrap();
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
        choose(&mut app, "中文");
        wait_long(&mut app, "text:新建运行");
    }
    {
        let mut app = cx.start();
        wait_long(&mut app, "text:新建运行");
        assert!(app.shows("对比"));
        // Back to the system's language (English here), remembered too.
        choose(&mut app, "跟随系统");
        wait_long(&mut app, "text:NEW RUN");
    }
    let mut app = cx.start();
    wait_long(&mut app, "text:NEW RUN");
}

#[test]
fn compare_shows_two_runs_side_by_side() {
    let cx = Ctx::new();
    let a = cx.seed("lab://demo");
    let b = cx.seed("lab://throttle");
    let mut app = cx.start();
    choose(&mut app, "English");
    wait_long(&mut app, &format!("role:button[Compare {a}]"));
    app.press(&format!("role:button[Compare {a}]")).unwrap();
    app.press(&format!("role:button[Compare {b}]")).unwrap();
    app.press("role:button[Compare 2 picked]").unwrap();
    wait_long(&mut app, "id:compare-table");
    wait_long(&mut app, "text:2 RUNS SIDE BY SIDE");
    assert!(app.shows("Operator vs first"));
    assert!(app.shows(&a) && app.shows(&b));
}
