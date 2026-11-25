//! The capacity model on the lab (design v4 §3): the in-flight ceiling estimated from a run's
//! request rows brackets the lab's limit, and the safe concurrency follows from the per-op
//! request shape (small files: one request per op; parquet: a HEAD, a footer, then 4 parallel
//! reads).

use std::sync::Arc;
use std::time::Duration;

use oprobe::analysis::capacity::{self, Window};
use oprobe::config::{Preset, Profile};
use oprobe::engine::Cancel;
use oprobe::engine::measure::{Rails, Request, measure};
use oprobe::guard::Budgets;
use oprobe::util::HumanDuration;

/// One lab run at a time: the ceiling is read from the client's in-flight counts, and a second
/// lab run loading the same CPUs (52 demo workers) delays responses in the client, so its
/// counts read several levels above the server's (a clean 29 on a lab limited at 24).
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lab_run(scenario: &str, preset: Preset, workers: usize) -> oprobe::model::Run {
    let _one = ONE_AT_A_TIME.lock().await;
    let mut p = Profile::from_url(&format!("lab://{scenario}"));
    p.workload.preset = preset;
    if preset == Preset::RangeReads {
        // The parquet lab models 4 parallel column reads per scan, ranges 64 KiB apart kept
        // apart.
        p.operator.read.concurrent = Some(4);
        p.operator.read.gap = Some(oprobe::util::ByteSize::kib(64));
    }
    p.workload.duration = HumanDuration(Duration::from_millis(2500));
    p.workload.warmup = HumanDuration(Duration::from_millis(300));
    p.workload.concurrency = workers;
    let rails = Rails::new(&p, false, None, Budgets::default()).unwrap();
    let req = Request::new(p, rails, Cancel::new(), Arc::new(|_| {}));
    measure(req).await.unwrap().run
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_lab_ceiling_brackets_its_limit_and_small_files_fit_twenty_workers() {
    // More workers than the demo lab's in-flight limit (40), so it throttles.
    let limit = oprobe::lab::DEMO_THROTTLE_ABOVE as f64;
    let run = lab_run("demo", Preset::SmallFiles, 52).await;
    let r = &run.report;
    assert!(
        r.resilience.throttled_responses > 0,
        "52 workers > {limit} throttle"
    );
    let c = &r.capacity;
    let ce = c
        .inflight_ceiling
        .unwrap_or_else(|| panic!("no ceiling: {}", c.confidence_reason));
    assert!(
        ce.lo <= limit && limit < ce.hi,
        "{ce:?}: {} {:?}",
        c.confidence_reason,
        c.observed.levels
    );
    assert!(
        (c.per_op.peak_inflight_per_op - 1.0).abs() < 1e-9,
        "{:?}",
        c.per_op
    );
    // The report's estimate is the estimate of its rows.
    let again = capacity::estimate(&run.reqs, &run.ops, Window::measured(r.run.window_s));
    assert_eq!(again.inflight_ceiling, c.inflight_ceiling);
    // The spec replays the same workload and target.
    assert_eq!(r.spec.operator_ref, "lab://demo");
    assert_eq!(r.spec.target_id().unwrap().to_string(), "lab://demo/");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parquet_lab_ops_peak_at_four_requests_in_flight() {
    // The shape of an operation, at a load the lab mostly accepts (8 × 4 around its 24).
    let run = lab_run("parquet", Preset::RangeReads, 8).await;
    let s = run.report.capacity.per_op;
    assert_eq!(s.peak_inflight_per_op, 4.0, "{s:?}");
    assert!(s.requests_per_op >= 6.0, "{s:?}");
    assert!(s.sequential_fraction > 0.2, "{s:?}");
    // The ceiling, at a load well above it (12 × 4), so the lab throttles even on a slow
    // runner where the scans overlap less (8 workers stayed under 24 on macOS CI).
    let run = lab_run("parquet", Preset::RangeReads, 12).await;
    let c = &run.report.capacity;
    assert_eq!(c.per_op.peak_inflight_per_op, 4.0, "{:?}", c.per_op);
    let ce = c
        .inflight_ceiling
        .unwrap_or_else(|| panic!("12 × 4 > 24 throttles: {}", c.confidence_reason));
    assert!(
        ce.lo <= 24.0 && 24.0 < ce.hi,
        "{ce:?}: {:?}",
        c.observed.levels
    );
}
