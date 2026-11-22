//! The capacity model on the lab (design v4 §3): the in-flight ceiling estimated from a run's
//! request rows brackets the lab's limit, and the safe concurrency follows from the per-op
//! request shape (small files: one request per op; parquet: a HEAD, a footer, then 4 parallel
//! reads).

use std::sync::Arc;
use std::time::Duration;

use oprobe::analysis::capacity::{self, DEFAULT_HEADROOM_RATIO, Window};
use oprobe::config::{Preset, Profile, TargetSpec};
use oprobe::engine::Cancel;
use oprobe::engine::measure::{Rails, Request, measure};
use oprobe::guard::Budgets;
use oprobe::util::HumanDuration;

async fn lab_run(scenario: &str, preset: Preset, workers: usize) -> oprobe::model::Run {
    let mut p = Profile::from_target(TargetSpec::from_url(&format!("lab://{scenario}")).unwrap());
    p.workload.preset = preset;
    p.workload.duration = HumanDuration(Duration::from_millis(2500));
    p.workload.warmup = HumanDuration(Duration::from_millis(300));
    p.knobs.concurrency = workers;
    let rails = Rails::new(&p, false, None, Budgets::default()).unwrap();
    let req = Request::new(p, rails, Cancel::new(), Arc::new(|_| {}));
    measure(req).await.unwrap().run
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_lab_ceiling_brackets_its_limit_and_small_files_fit_twenty_workers() {
    let run = lab_run("demo", Preset::SmallFiles, 32).await;
    let r = &run.report;
    assert!(
        r.resilience.throttled_responses > 0,
        "32 workers > 24 throttle"
    );
    let c = &r.capacity;
    let ce = c
        .inflight_ceiling
        .unwrap_or_else(|| panic!("no ceiling: {}", c.confidence_reason));
    assert!(
        ce.lo <= 24.0 && 24.0 < ce.hi,
        "{ce:?}: {} {:?}",
        c.confidence_reason,
        c.observed.levels
    );
    assert!(
        (c.per_op.peak_inflight_per_op - 1.0).abs() < 1e-9,
        "{:?}",
        c.per_op
    );
    let safe = c.safe_concurrency(DEFAULT_HEADROOM_RATIO).unwrap();
    assert!((19..=21).contains(&safe), "safe concurrency {safe}: {ce:?}");
    // The report's estimate is the estimate of its rows.
    let again = capacity::estimate(&run.reqs, &run.ops, Window::measured(r.run.window_s));
    assert_eq!(again.inflight_ceiling, c.inflight_ceiling);
    // The spec replays the same workload and target.
    assert_eq!(r.spec.target_ref, "lab://demo");
    assert_eq!(r.spec.target_id().unwrap().to_string(), "lab://demo/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parquet_lab_ops_peak_at_four_requests_in_flight() {
    let run = lab_run("parquet", Preset::Parquet, 8).await;
    let c = &run.report.capacity;
    let s = c.per_op;
    assert_eq!(s.peak_inflight_per_op, 4.0, "{s:?}");
    assert!(s.requests_per_op >= 6.0, "{s:?}");
    assert!(s.sequential_fraction > 0.2, "{s:?}");
    let ce = c
        .inflight_ceiling
        .unwrap_or_else(|| panic!("8 × 4 > 24 throttles: {}", c.confidence_reason));
    assert!(
        ce.lo <= 24.0 && 24.0 < ce.hi,
        "{ce:?}: {:?}",
        c.observed.levels
    );
    // 24 × 0.85 / 4 → 5 workers (not 24 / 6.3 = 3, nor 24).
    let safe = c.safe_concurrency(DEFAULT_HEADROOM_RATIO).unwrap();
    assert!((4..=5).contains(&safe), "{safe}: {ce:?} {s:?}");
}
