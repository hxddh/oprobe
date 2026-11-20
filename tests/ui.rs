#![cfg(feature = "tui")]
//! UI flows on the real NSH engine (headless), driving the real oprobe engine underneath.
//! Each test gets its own data directory; tests run one at a time (the launch options and the
//! engine's job registry are process-wide).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nsh_core::platform::{Idiom, Os};
use nsh_core::prelude::{point, px};
use nsh_engine::{EngineConfig, Environment, Points, Root, Size};
use nsh_testing::{DialogAnswer, Headless};
use oprobe::store::{ConfigFile, Paths, RunStore};
use oprobe::ui::bridge::{Job, Outcome};
use oprobe::ui::launcher::RunForm;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Ctx {
    _lock: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
}

impl Ctx {
    fn new() -> Self {
        let lock = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        oprobe::ui::set_launch(oprobe::ui::Launch {
            duration: Some("2s".into()),
            paths: Some(Paths {
                config_file: dir.path().join("config.toml"),
                data_dir: dir.path().join("data"),
            }),
            ..Default::default()
        });
        Self { _lock: lock, dir }
    }

    fn store(&self) -> RunStore {
        RunStore::new(self.dir.path().join("data").join("runs"))
    }

    /// Measure `target` for `secs` with the engine directly and save the run.
    fn seed(&self, target: &str, secs: u32, conc: &str) -> String {
        let form = RunForm {
            target: target.into(),
            duration: format!("{secs}s"),
            preset: "small-files".into(),
            concurrency: conc.into(),
            ..Default::default()
        };
        let req = form
            .prepare(&ConfigFile::default(), Some(self.store()), 200_000)
            .unwrap();
        let job = Job::run(req);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(o) = job.take().outcome {
                match o {
                    Outcome::Run { run, .. } => return run.report.run.id.clone(),
                    other => panic!("seed run failed: {other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "seed run timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn start(&self) -> Headless {
        self.start_on(Idiom::Desktop, Size::new(Points(1360.0), Points(900.0)))
    }

    fn start_on(&self, idiom: Idiom, viewport: Size<Points>) -> Headless {
        let config = EngineConfig {
            viewport,
            data_dir: Some(self.dir.path().join("app")),
            environment: Environment {
                window: viewport,
                os: Os::Linux,
                idiom,
                ..Environment::default()
            },
            ..EngineConfig::new(Root::of::<oprobe::ui::App>())
        };
        let mut app = Headless::start(config).unwrap();
        app.wait_for("id:run-stop").unwrap();
        app
    }
}

/// Open an inspector tab once the run has loaded.
fn tab(app: &mut Headless, name: &str) {
    wait_long(app, &format!("text:{name}"));
    app.press(&format!("text:{name}")).unwrap();
}

fn wait_long(app: &mut Headless, sel: &str) {
    app.wait_for_within(sel, Duration::from_secs(90))
        .unwrap_or_else(|e| panic!("{sel}: {e}"));
}

fn load(store: &RunStore, id: &str) -> oprobe::model::Run {
    oprobe::store::read_run_file(&store.resolve(id).unwrap()).unwrap()
}

#[test]
fn onboarding_demo_run_shows_findings() {
    let cx = Ctx::new();
    let mut app = cx.start();
    // No runs: the welcome view with its three paths.
    app.wait_for("text:Try the demo").unwrap();
    assert!(app.shows("Connect my bucket"));
    assert!(app.shows("Open a run file"));
    app.press("role:button[Try the Demo]").unwrap();
    // The live run: the one Run/Stop control reads Stop while it runs.
    app.wait_for_within("role:button[Stop]", Duration::from_secs(10))
        .unwrap();
    // It finishes, is saved, and the overview shows the verdict and findings of the lab's
    // throttling and stragglers.
    wait_long(&mut app, "text:TOP FINDINGS");
    app.wait_for("role:button[Run]").unwrap();
    assert!(app.shows("WHERE OPERATION TIME GOES"));
    assert_eq!(cx.store().list().unwrap().len(), 1);
    let run = &cx.store().list().unwrap()[0];
    assert_eq!(run.target_display, "lab://demo");
}

#[test]
fn requests_table_filters_and_sorts() {
    let cx = Ctx::new();
    let id = cx.seed("memory://", 1, "4");
    let run = load(&cx.store(), &id);
    let total = run.reqs.len();
    assert!(total > 50, "{total} requests");
    let mut app = cx.start();
    tab(&mut app, "Requests");
    app.wait_for("id:requests-table").unwrap();
    let all = oprobe::ui::model::fmt::compact(total as f64);
    app.wait_for(&format!("text:{all} of {all}")).unwrap();
    // A filter narrows the rows; the count follows.
    let api = run.reqs[0].api.to_string();
    let n = run.reqs.iter().filter(|r| r.api == api).count();
    app.type_text("id:Filter requests", &format!("api:{api}"))
        .unwrap();
    app.wait_for(&format!(
        "text:{} of {all}",
        oprobe::ui::model::fmt::compact(n as f64)
    ))
    .unwrap();
    app.wait_for("text:/^not understood/").ok();
    app.clear("id:Filter requests").unwrap();
    // Sorting by Total, slowest first: the slowest request is in view.
    app.press("text:Total").unwrap();
    app.wait_for("text:sorted by Total ↑").unwrap();
    app.press("text:Total").unwrap();
    app.wait_for("text:sorted by Total ↓").unwrap();
    let max = run.reqs.iter().map(|r| r.total_us).max().unwrap();
    app.wait_for(&format!(
        "text:{}",
        oprobe::ui::model::fmt::dur_us(max as f64)
    ))
    .unwrap();
}

#[test]
fn timeline_zooms_and_fits() {
    let cx = Ctx::new();
    cx.seed("memory://", 1, "4");
    let mut app = cx.start();
    tab(&mut app, "Timeline");
    app.wait_for("id:timeline-range").unwrap();
    let range = |app: &Headless| {
        app.screen()
            .texts()
            .into_iter()
            .find(|t| t.contains(" – "))
            .unwrap_or_default()
            .to_owned()
    };
    let before = range(&app);
    app.press("role:button[Zoom in]").unwrap();
    app.press("role:button[Zoom in]").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while range(&app) == before {
        assert!(
            Instant::now() < deadline,
            "the visible range did not change"
        );
        app.wait(Duration::from_millis(50)).unwrap();
    }
    app.press("role:button[Fit the whole run]").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while range(&app) != before {
        assert!(Instant::now() < deadline, "Fit did not restore the range");
        app.wait(Duration::from_millis(50)).unwrap();
    }
}

#[test]
fn brushing_the_heatmap_explains_the_selection() {
    let cx = Ctx::new();
    cx.seed("memory://", 1, "4");
    let mut app = cx.start();
    app.wait_for("id:heatmap").unwrap();
    app.drag("id:heatmap", point(px(-250.0), px(-40.0)))
        .unwrap();
    app.wait_for("text:What is different about these requests")
        .unwrap();
    app.wait_for("text:/requests between/").unwrap();
}

#[test]
fn compares_two_marked_runs() {
    let cx = Ctx::new();
    let a = cx.seed("memory://", 1, "2");
    let b = cx.seed("memory://", 1, "8");
    let mut app = cx.start();
    app.press(&format!("role:button[Mark {a} for comparison]"))
        .unwrap();
    app.press(&format!("role:button[Mark {b} for comparison]"))
        .unwrap();
    app.press("role:button[Compare] >> nth=0").unwrap();
    wait_long(&mut app, &format!("text:{a} → {b}"));
    app.wait_for("text:/^CHANGES/").unwrap();
    app.wait_for("text:CONFIGURATION DIFFERENCES").unwrap();
    // The concurrency differs between the two.
    app.wait_for("text:/concurrency/").unwrap();
}

#[test]
fn settings_and_layout_persist_across_launches() {
    let cx = Ctx::new();
    cx.seed("memory://", 1, "4");
    {
        let mut app = cx.start();
        tab(&mut app, "Timeline");
        app.wait_for("id:timeline-range").unwrap();
        app.press("role:button[Sidebar]").unwrap();
        app.wait_for_gone("text:/^RUNS/", Duration::from_secs(3))
            .unwrap();
    }
    let mut app = cx.start();
    // The last tab and the hidden sidebar came back.
    app.wait_for("id:timeline-range").unwrap();
    assert!(!(app.screen().count("text:/^RUNS/").unwrap() > 0));
    app.press("role:button[Sidebar]").unwrap();
    app.wait_for("text:/^RUNS/").unwrap();
}

#[test]
fn terminal_keys_drive_the_app() {
    let cx = Ctx::new();
    cx.seed("memory://", 1, "4");
    // 80×24 cells of 8×16 points: the sidebar collapses, the footer keeps its key hints.
    let mut app = cx.start_on(Idiom::Terminal, Size::new(Points(640.0), Points(384.0)));
    app.wait_for("text:/^TOP FINDINGS|WHERE OPERATION TIME GOES/")
        .ok();
    assert!(app.shows("quit"));
    assert!(!(app.screen().count("text:/^RUNS/").unwrap() > 0));
    app.press_key("2").unwrap();
    app.wait_for("id:requests-table").unwrap();
    app.press_key("3").unwrap();
    app.wait_for("id:timeline-range").unwrap();
    app.press_key("shift+/").unwrap();
    app.wait_for("text:Keyboard").unwrap();
    app.press_key("escape").unwrap();
    app.wait_for_gone("text:Keyboard", Duration::from_secs(3))
        .unwrap();
    app.press_key("n").unwrap();
    app.wait_for("text:New Run").unwrap();
}

#[test]
fn quitting_while_running_stops_and_cleans_up() {
    let cx = Ctx::new();
    oprobe::ui::set_launch(oprobe::ui::Launch {
        duration: Some("60s".into()),
        paths: Some(Paths {
            config_file: cx.dir.path().join("config.toml"),
            data_dir: cx.dir.path().join("data"),
        }),
        ..Default::default()
    });
    let mut app = cx.start_on(Idiom::Terminal, Size::new(Points(640.0), Points(384.0)));
    app.wait_for("role:button[Try the Demo]").unwrap();
    app.press("role:button[Try the Demo]").unwrap();
    app.wait_for_within("role:button[Stop]", Duration::from_secs(10))
        .unwrap();
    assert!(oprobe::ui::jobs_running());
    // q asks first; stopping cleans up the lab's test data and ends the job.
    app.answer_dialogs_myself();
    app.press_key("q").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.pending_dialog().is_none() {
        assert!(Instant::now() < deadline, "no confirmation");
        app.wait(Duration::from_millis(50)).unwrap();
    }
    assert_eq!(
        app.pending_dialog().unwrap().title.as_deref(),
        Some("Stop the run and quit?")
    );
    app.answer_dialog(DialogAnswer::Press("Stop & Quit".into()))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while oprobe::ui::jobs_running() {
        assert!(Instant::now() < deadline, "the job did not stop");
        app.wait(Duration::from_millis(100)).unwrap();
    }
}

#[test]
fn a_run_file_opens_from_disk() {
    let cx = Ctx::new();
    let id = cx.seed("memory://", 1, "2");
    let src = cx.store().resolve(&id).unwrap();
    // Another machine's file: a fresh data directory, opened through the launch target.
    let other = tempfile::tempdir().unwrap();
    let copy: PathBuf = other.path().join("shared.oprobe");
    std::fs::copy(&src, &copy).unwrap();
    std::fs::remove_dir_all(cx.dir.path().join("data")).unwrap();
    oprobe::ui::set_launch(oprobe::ui::Launch {
        target: Some(copy.display().to_string()),
        paths: Some(Paths {
            config_file: cx.dir.path().join("config.toml"),
            data_dir: cx.dir.path().join("data"),
        }),
        ..Default::default()
    });
    let mut app = cx.start();
    wait_long(&mut app, &format!("text:/{id}/"));
    assert!(Path::new(&cx.store().run_path(&id)).exists());
}

#[test]
fn a_large_run_opens_fast_and_scrolls() {
    // Performance: 100k request rows open and the table pages through them.
    let cx = Ctx::new();
    let id = cx.seed("memory://", 1, "2");
    let mut run = load(&cx.store(), &id);
    let base = run.reqs.clone();
    while run.reqs.len() < 100_000 {
        let k = run.reqs.len() as i64;
        run.reqs.extend(base.iter().map(|r| {
            let mut r = r.clone();
            r.t_us += k;
            r
        }));
    }
    run.reqs.truncate(100_000);
    cx.store().save(&run).unwrap();
    let t0 = Instant::now();
    let mut app = cx.start();
    tab(&mut app, "Requests");
    wait_long(&mut app, "text:100k of 100k");
    let open = t0.elapsed();
    app.type_text("id:Filter requests", ">0ms").unwrap();
    app.wait_for("text:100k of 100k").unwrap();
    eprintln!("ui-perf: 100k-row run opened and listed in {open:?}");
    assert!(open < Duration::from_secs(20), "{open:?}");
}
