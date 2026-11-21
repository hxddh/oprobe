//! Lab convergence (design v4 §12): the findings, the replay path and the capacity model agree.
//! On a lab that throttles above 24 requests in flight, the throttling finding's suggested
//! concurrency matches the capacity model's safe concurrency. Replaying the run at that
//! concurrency (`run --from RUN --set concurrency=N`) stays within the throttle budget. The
//! replayed run's findings never suggest exceeding the ceiling. Tune agreeing with the model,
//! and `verify` confirming the finding, are covered by `tests/tune_verify.rs`.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn run(&self, args: &[&str]) -> Value {
        let o: Output = Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", self.path("data"))
            .env("OPROBE_CONFIG", self.path("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["--color", "never"])
            .args(args)
            .args(["-o", "json"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(
            o.status.code().is_some_and(|c| c == 0 || c == 3),
            "oprobe {args:?} failed: {stderr}"
        );
        serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("oprobe {args:?}: stdout is not JSON ({e}); {stderr}"))
    }
}

/// The capacity model's `safe_concurrency(0.15)` of a report: `floor(lo × 0.85 / per-op peak)`.
fn safe_concurrency(report: &Value) -> Option<u64> {
    let cap = &report["capacity"];
    let lo = cap["inflight_ceiling"]["lo_inflight_count"].as_f64()?;
    let per_op = cap["per_op"]["peak_inflight_per_op_count"]
        .as_f64()
        .unwrap_or(1.0)
        .max(1.0);
    Some(((lo * 0.85 / per_op).floor() as u64).max(1))
}

/// The concurrency each finding's verification suggests.
fn suggestions(report: &Value) -> Vec<(String, u64)> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| {
            let c = f["verification"]["overrides"]["knobs"]["concurrency"]
                .as_str()?
                .parse()
                .ok()?;
            Some((f["code"].as_str().unwrap_or_default().to_owned(), c))
        })
        .collect()
}

fn converges(target: &str, workers: &str) {
    let env = Env::new();
    // A run above the ceiling. Client-side timing on a loaded machine can blur one run's
    // boundary, so allow up to 3 attempts to get a measured ceiling.
    let mut first = Value::Null;
    for _ in 0..3 {
        first = env.run(&[
            "run", target, "-c", workers, "-d", "3s", "--warmup", "300ms",
        ]);
        if safe_concurrency(&first["report"]).is_some() {
            break;
        }
    }
    let report = &first["report"];
    let safe = safe_concurrency(report)
        .unwrap_or_else(|| panic!("{target}: no ceiling measured: {}", report["capacity"]));
    let s = suggestions(report);
    let (_, suggested) = s
        .iter()
        .find(|(code, _)| code == "throttling")
        .unwrap_or_else(|| panic!("{target}: no throttling finding with a concurrency: {s:?}"));
    assert!(
        suggested.abs_diff(safe) <= 1,
        "{target}: the throttling finding suggests {suggested}, the capacity model {safe}"
    );
    // Replay at the suggestion through the spec (nothing but concurrency changes).
    let run_id = first["run_id"].as_str().unwrap();
    let c = suggested.to_string();
    let replay = env.run(&[
        "run",
        "--from",
        run_id,
        "--set",
        &format!("concurrency={c}"),
        "-d",
        "3s",
    ]);
    let r = &replay["report"];
    assert_eq!(
        r["spec"]["profile"]["workload"], report["spec"]["profile"]["workload"],
        "{target}: the replay changed the workload beyond its duration"
    );
    let throttled = r["summary"]["throttled_request_ratio"].as_f64().unwrap();
    assert!(
        throttled <= 0.001,
        "{target}: at concurrency {c} still {:.2}% of requests throttled",
        throttled * 100.0
    );
    // No follow-up finding suggests going past the ceiling the first run measured.
    let lo = report["capacity"]["inflight_ceiling"]["lo_inflight_count"]
        .as_f64()
        .unwrap();
    let per_op = report["capacity"]["per_op"]["peak_inflight_per_op_count"]
        .as_f64()
        .unwrap_or(1.0)
        .max(1.0);
    let max_workers = (lo / per_op).floor() as u64;
    for (code, n) in suggestions(r) {
        assert!(
            n <= max_workers,
            "{target}: after replaying at {c}, `{code}` suggests {n} workers (ceiling allows {max_workers})"
        );
    }
}

#[test]
fn demo_lab_findings_and_replay_agree() {
    converges("lab://demo", "32");
}

#[test]
fn parquet_lab_findings_and_replay_agree() {
    converges("lab://parquet", "8");
}
