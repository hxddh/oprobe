#![cfg(feature = "tui")]
//! UI flows on the real NSH engine (headless), driving the real oprobe engine underneath.

use std::time::Duration;

use nsh_engine::{EngineConfig, Points, Root, Size};
use nsh_testing::Headless;

fn start() -> Headless {
    let mut app = Headless::start(EngineConfig {
        viewport: Size::new(Points(1280.0), Points(900.0)),
        ..EngineConfig::new(Root::of::<oprobe::ui::App>())
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
    // The default (CLI) preset is the Parquet scan.
    app.wait_for("text:parquet.scan").unwrap();
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
    set(&mut app, "Trial length", "1");
    set(&mut app, "Max trials", "4");
    app.press("role:button[Start tuning]").unwrap();
    app.wait_for_within("Recommended configuration", Duration::from_secs(90))
        .unwrap();
    app.wait_for("text:/#0 baseline/").unwrap();
    // The card shows the winner's knobs; Apply must put exactly those into the form.
    let winner = app
        .screen()
        .texts()
        .into_iter()
        .find_map(|t| t.strip_prefix("concurrency=").map(str::to_owned))
        .expect("the winner's settings are shown");
    let conc = winner.split_whitespace().next().unwrap().to_owned();
    let io = winner
        .split_whitespace()
        .find_map(|kv| kv.strip_prefix("io="))
        .unwrap()
        .to_owned();
    app.press("role:button[Apply to workbench]").unwrap();
    app.wait_for("text:/^Tuned settings applied to the workbench/")
        .unwrap();
    assert_eq!(
        app.screen().input_text("id:Concurrency").unwrap(),
        Some(conc.as_str())
    );
    assert_eq!(
        app.screen().input_text("id:I/O per op").unwrap(),
        Some(io.as_str())
    );
    // Tuning left the workbench's own (empty) results alone: no confirmation report there.
    app.wait_for("text:/Choose a target and press Run/")
        .unwrap();
}

#[test]
fn defaults_match_the_cli() {
    let mut app = start();
    // `oprobe run` without flags: preset parquet, 10 s, the engine's default knobs.
    app.wait_for("text:Parquet scan").unwrap();
    let d = oprobe::config::Knobs::default();
    assert_eq!(app.screen().input_text("id:Duration").unwrap(), Some("10s"));
    assert_eq!(
        app.screen().input_text("id:Concurrency").unwrap(),
        Some(d.concurrency.to_string().as_str())
    );
    // Inputs carry an accessibility label; placeholders read as examples.
    app.wait_for("label:Storage URL").unwrap();
    app.press("Tune").unwrap();
    app.wait_for("text:/concurrency ladder/").unwrap();
    use oprobe::cli::args::tune_defaults;
    assert_eq!(
        app.screen().input_text("id:Trial length").unwrap(),
        Some(tune_defaults::TRIAL_DURATION)
    );
    assert_eq!(
        app.screen().input_text("id:Max trials").unwrap(),
        Some(tune_defaults::MAX_TRIALS.to_string().as_str())
    );
}

#[test]
fn invalid_tuner_settings_are_rejected() {
    let mut app = start();
    app.press("Tune").unwrap();
    app.wait_for("text:/concurrency ladder/").unwrap();
    set(&mut app, "Trial length", "soon");
    app.press("role:button[Start tuning]").unwrap();
    app.wait_for("text:/invalid trial duration/").unwrap();
    set(&mut app, "Trial length", "1s");
    set(&mut app, "Max trials", "lots");
    app.press("role:button[Start tuning]").unwrap();
    app.wait_for("text:/Max trials must be a whole number/")
        .unwrap();
    set(&mut app, "Max trials", "1");
    app.press("role:button[Start tuning]").unwrap();
    app.wait_for("text:/max trials must be at least 2/")
        .unwrap();
    app.wait_for("text:● Failed").unwrap();
}

#[test]
fn stop_cancels_the_run_and_shows_stopping() {
    let mut app = start();
    set(&mut app, "Duration", "60s");
    app.press("role:button[Run]").unwrap();
    app.wait_for_within("role:button[Stop]", Duration::from_secs(5))
        .unwrap();
    app.press("role:button[Stop]").unwrap();
    // A stop while preparing the dataset ends "Ready"; one while measuring, "Finished" with
    // the partial report. Either way it is not shown as an error.
    app.wait_for_within("text:/^● (Finished|Ready)$/", Duration::from_secs(30))
        .unwrap();
    app.wait_for("text:/stopping… test data will be cleaned up/")
        .unwrap();
    app.wait_for("role:button[Run]").unwrap();
    assert!(!app.shows("Something went wrong"));
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
