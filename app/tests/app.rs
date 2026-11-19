//! UI flows on the real NSH engine (headless), driving the real oprobe engine underneath.

use std::time::Duration;

use nsh_engine::{EngineConfig, Points, Root, Size};
use nsh_testing::Headless;

fn start() -> Headless {
    let mut app = Headless::start(EngineConfig {
        viewport: Size::new(Points(1280.0), Points(900.0)),
        ..EngineConfig::new(Root::of::<oprobe_app::App>())
    })
    .unwrap();
    app.wait_for("Workbench").unwrap();
    app
}

fn set(app: &mut Headless, field: &str, value: &str) {
    let sel = format!("id:{field}");
    app.clear(&sel).unwrap();
    app.type_text(&sel, value).unwrap();
}

#[test]
fn runs_a_memory_workload_end_to_end() {
    let mut app = start();
    set(&mut app, "Duration", "1s");
    app.press("role:button[Run]").unwrap();
    app.wait_for_within("role:button[Stop]", Duration::from_secs(5))
        .unwrap();
    app.wait_for_within("text:● Finished", Duration::from_secs(30))
        .unwrap();
    // Verdict, KPIs and the time breakdown came from the real report.
    assert!(app.shows("HEALTHY") || app.shows("REVIEW") || app.shows("ATTENTION"));
    assert!(app.shows("Where the time goes"));
    assert!(app.shows("read.range") || app.shows("read.full"));
    // Details tabs switch.
    app.press("Requests").unwrap();
    app.wait_for("GetObject").unwrap();
    app.press("Slowest op").unwrap();
    app.wait_for_within("text:/^#1 /", Duration::from_secs(2))
        .unwrap();
    app.press("Cost").unwrap();
    app.wait_for("text:/per 1M ops/").unwrap();
    assert!(app.shows("Save report…"));
}

#[test]
fn invalid_target_shows_an_actionable_hint() {
    let mut app = start();
    set(&mut app, "Storage URL", "ftp://nowhere");
    app.press("role:button[Run]").unwrap();
    app.wait_for("Something went wrong").unwrap();
    app.wait_for("text:/^Hint:/").unwrap();
    app.wait_for("text:● Failed").unwrap();
}

#[test]
fn tuner_runs_trials_and_offers_the_winner() {
    let mut app = start();
    app.press("Tune").unwrap();
    app.wait_for("text:/concurrency ladder/").unwrap();
    set(&mut app, "Duration", "1s");
    set(&mut app, "Trial length (s)", "1");
    set(&mut app, "Max trials", "4");
    app.press("role:button[Start tuning]").unwrap();
    app.wait_for_within("Recommended configuration", Duration::from_secs(90))
        .unwrap();
    app.wait_for("text:/#0 baseline/").unwrap();
    app.press("role:button[Apply to workbench]").unwrap();
    app.wait_for("text:Tuned settings applied to the workbench")
        .unwrap();
}

#[test]
fn probe_rejects_non_http_endpoints() {
    let mut app = start();
    app.press("Probe").unwrap();
    app.wait_for("text:/Five fresh connections/").unwrap();
    set(&mut app, "URL", "ftp://x");
    app.press("role:button[Run probe]").unwrap();
    app.wait_for("text:/must start with http/").unwrap();
}
