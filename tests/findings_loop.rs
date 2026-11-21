//! Following the findings converges (design v4 §3, VER-2): on `lab://demo` and
//! `lab://parquet`, the throttling finding's suggestion runs without throttling, and no
//! finding — on the first run or on any run that follows a suggestion — tells the user to go
//! past the in-flight ceiling. Every follow-up is the run's spec with the finding's
//! verification overrides (`RunSpec::with`), measured for real against the lab.

use std::sync::Arc;
use std::time::Duration;

use oprobe::analysis::capacity::DEFAULT_HEADROOM_RATIO;
use oprobe::config::{Preset, Profile, TargetSpec};
use oprobe::engine::Cancel;
use oprobe::engine::measure::{Rails, Request, measure};
use oprobe::guard::Budgets;
use oprobe::model::{Finding, Run, RunSpec};
use oprobe::store::RunStore;
use oprobe::util::HumanDuration;

/// Throttled-request share a followed suggestion may leave.
const BUDGET: f64 = 0.001;

fn short(p: &mut Profile) {
    // Short windows keep the test fast; a verification that lengthens the run (tail-cut's
    // blind start) is measured short too: only throttling and the advice are checked here.
    p.workload.duration = HumanDuration(Duration::from_millis(2500));
    p.workload.warmup = HumanDuration(Duration::from_millis(300));
}

async fn measure_profile(p: Profile, target: &str, store: &RunStore) -> Run {
    let rails = Rails::new(&p, false, None, Budgets::default()).unwrap();
    let mut req = Request::new(p.clone(), rails, Cancel::new(), Arc::new(|_| {}));
    req.store = Some(store.clone());
    req.spec = Some(RunSpec::new(&p, target));
    measure(req).await.unwrap().run
}

/// Concurrency values the findings' verifications would run.
fn suggested(findings: &[Finding]) -> Vec<(String, u64)> {
    findings
        .iter()
        .filter_map(|f| {
            let v = f.verification.as_ref()?;
            let c = v.overrides.knobs.get("concurrency")?.parse().ok()?;
            Some((f.code.clone(), c))
        })
        .collect()
}

fn assert_within(findings: &[Finding], safe: u64, what: &str) {
    for (code, c) in suggested(findings) {
        assert!(
            c <= safe,
            "{what}: `{code}` suggests concurrency {c} above the safe {safe}: {findings:#?}"
        );
    }
}

async fn converges(scenario: &str, preset: Preset, workers: usize) {
    let dir = tempfile::tempdir().unwrap();
    let store = RunStore::new(dir.path().join("runs"));
    let target = format!("lab://{scenario}");
    let mut p = Profile::from_target(TargetSpec::from_url(&target).unwrap());
    p.workload.preset = preset;
    p.knobs.concurrency = workers;
    short(&mut p);
    let first = measure_profile(p, &target, &store).await;
    let r = &first.report;
    assert!(
        r.summary.throttled_request_ratio > BUDGET,
        "{workers} workers throttle"
    );
    let safe = u64::from(
        r.capacity
            .safe_concurrency(DEFAULT_HEADROOM_RATIO)
            .unwrap_or_else(|| panic!("no ceiling: {}", r.capacity.confidence_reason)),
    );
    assert_within(&r.findings, safe, "first run");
    let thr = r
        .findings
        .iter()
        .find(|f| f.code == "throttling")
        .expect("throttling finding");
    let v = thr.verification.as_ref().expect("throttling verification");
    assert_eq!(v.overrides.knobs["concurrency"], safe.to_string(), "{v:#?}");
    assert!(
        v.expect
            .iter()
            .any(|e| e.metric == "summary.throttled_request_ratio" && e.hi <= BUDGET),
        "{v:#?}"
    );

    // Follow the throttling finding, then every suggestion of the run that follows.
    let mut queue = vec![(r.spec.clone(), v.overrides.clone(), "throttling".to_owned())];
    let mut depth = 0;
    while let Some((spec, overrides, code)) = queue.pop() {
        let mut p = spec.with(&overrides).resolved_profile();
        // A spec keeps nothing of the ephemeral lab server; resolve the target again, as
        // `oprobe run --from` does.
        p.target = TargetSpec::from_url(&target).unwrap();
        short(&mut p);
        let run = measure_profile(p, &target, &store).await;
        let fr = &run.report;
        assert!(
            fr.summary.throttled_request_ratio <= BUDGET,
            "following `{code}` ({}) throttled {:.3}%: {:?}",
            overrides.summary(),
            fr.summary.throttled_request_ratio * 100.0,
            fr.capacity.observed.levels
        );
        assert_within(&fr.findings, safe, &format!("after `{code}`"));
        if depth == 0 {
            for f in &fr.findings {
                if let Some(v) = &f.verification {
                    queue.push((fr.spec.clone(), v.overrides.clone(), f.code.clone()));
                }
            }
        }
        depth += 1;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_findings_converge_under_the_ceiling() {
    converges("demo", Preset::SmallFiles, 32).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parquet_findings_converge_under_the_ceiling() {
    converges("parquet", Preset::Parquet, 16).await;
}
