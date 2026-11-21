//! Tune, verify and compare end to end on the in-process lab (design v4 §5–§7):
//! - tune on `lab://parquet` recommends the capacity model's safe concurrency (±1) and never
//!   reports a negative headline;
//! - `verify` confirms the throttling finding on `lab://demo`;
//! - two identical runs compare as not significant;
//! - going from 0 to 1 throttled response does not fail the gate.

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
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", self.path("data"))
            .env("OPROBE_CONFIG", self.path("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["--color", "never"])
            .args(args)
            .output()
            .unwrap()
    }
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&o.stdout),
            err(o)
        )
    })
}

/// `floor(ceiling.lo × (1 − 0.15) / peak_inflight_per_op)` of a report's capacity.
fn safe_concurrency(cap: &Value) -> Option<u64> {
    let lo = cap["inflight_ceiling"]["lo_inflight_count"].as_f64()?;
    let per_op = cap["per_op"]["peak_inflight_per_op_count"]
        .as_f64()
        .unwrap_or(1.0)
        .max(1.0);
    Some(((lo * 0.85 / per_op).floor() as u64).max(1))
}

#[test]
fn tune_on_parquet_recommends_the_safe_concurrency() {
    let env = Env::new();
    // The capacity model of a plain run that throttles (8 parquet workers × 4 in flight > 24).
    // On a loaded machine one run's client-side timing can blur the boundary: up to 3 runs.
    let mut model = None;
    let mut last = Value::Null;
    for _ in 0..3 {
        let o = env.run(&[
            "run",
            "lab://parquet",
            "--preset",
            "parquet",
            "-c",
            "8",
            "-d",
            "3s",
            "--warmup",
            "300ms",
            "-o",
            "json",
        ]);
        assert!(
            o.status.code().is_some_and(|c| c == 0 || c == 3),
            "{}",
            err(&o)
        );
        last = json(&o)["report"]["capacity"].clone();
        model = safe_concurrency(&last);
        if model.is_some() {
            break;
        }
    }
    let model = model.unwrap_or_else(|| panic!("no ceiling in 3 runs: {last}"));
    let o = env.run(&[
        "tune",
        "lab://parquet",
        "--preset",
        "parquet",
        "--trial-duration",
        "1s",
        "--trial-warmup",
        "200ms",
        "--cooldown",
        "0s",
        "--max-trials",
        "12",
        "-o",
        "json",
    ]);
    assert!(
        o.status.code().is_some_and(|c| c == 0 || c == 3),
        "{}",
        err(&o)
    );
    let v = json(&o);
    let t = &v["tune"];
    let rec = &v["recommendation"];
    let c = rec["knobs"]["concurrency_count"]
        .as_u64()
        .expect("recommended concurrency");
    let safe = t["safe_concurrency_count"].as_u64().expect("tune's margin");
    assert!(
        c.abs_diff(safe) <= 1,
        "recommended {c}, tune's safe concurrency {safe}: {}",
        serde_json::to_string_pretty(t).unwrap()
    );
    assert!(
        c.abs_diff(model) <= 1,
        "recommended {c}, the capacity model of a plain run says {model}"
    );
    // Never a negative headline.
    if let Some(i) = t["improvement_ratio"].as_f64() {
        assert!(i >= 0.0, "{t}");
    }
    let headline = t["headline"].as_str().unwrap();
    assert!(
        !headline.contains("+-") && !headline.contains(" -"),
        "{headline}"
    );
    assert_eq!(t["objective"], "goodput");
    assert!(t["max_throttle_ratio"].as_f64().unwrap() > 0.0);
    assert!(!t["pareto_trial_ids"].as_array().unwrap().is_empty());
    let toml = rec["profile_toml"].as_str().unwrap();
    assert!(toml.contains(&format!("concurrency = {c}")), "{toml}");
    assert!(!toml.contains("proxy"), "{toml}");
    assert!(!rec["rust_snippet"].as_str().unwrap().is_empty());
}

#[test]
fn verify_confirms_the_throttling_finding_on_the_demo_lab() {
    let env = Env::new();
    let o = env.run(&[
        "run",
        "lab://demo",
        "--preset",
        "small-files",
        "-c",
        "40",
        "-d",
        "2s",
        "--warmup",
        "300ms",
        "-o",
        "json",
    ]);
    assert!(
        o.status.code().is_some_and(|c| c == 0 || c == 3),
        "{}",
        err(&o)
    );
    let run = json(&o);
    let codes: Vec<&str> = run["report"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"throttling"), "{codes:?}");
    let o = env.run(&[
        "verify",
        "last",
        "throttling",
        "--repeat",
        "2",
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", err(&o));
    let v = json(&o);
    let f = &v["findings"][0];
    assert_eq!(f["code"], "throttling");
    assert_eq!(
        f["outcome"],
        "confirmed",
        "{}",
        serde_json::to_string_pretty(f).unwrap()
    );
    assert_eq!(v["baseline_run_ids"].as_array().unwrap().len(), 2);
    assert_eq!(f["candidate_run_ids"].as_array().unwrap().len(), 2);
    assert!(v["noise_pairs_count"].as_u64().unwrap() >= 1);
    assert_eq!(v["calibrated"], true);
    // The A/A pairs and the outcome were stored.
    assert!(env.path("data").join("noise.toml").exists());
    let history = std::fs::read_to_string(env.path("data").join("capacity.toml")).unwrap();
    assert!(history.contains("throttling"), "{history}");
}

#[test]
fn identical_runs_compare_as_not_significant_and_calibrate_learns_the_noise() {
    let env = Env::new();
    let args = [
        "lab://demo",
        "--preset",
        "small-files",
        "-c",
        "4",
        "-d",
        "3s",
        "--warmup",
        "300ms",
    ];
    let o = env.run(
        &["baseline", "calibrate"]
            .iter()
            .chain(args.iter())
            .chain(["--repeat", "3", "-o", "json"].iter())
            .copied()
            .collect::<Vec<_>>(),
    );
    assert!(o.status.success(), "{}", err(&o));
    let cal = json(&o);
    assert_eq!(cal["pairs_count"], 2);
    assert_eq!(cal["run_ids"].as_array().unwrap().len(), 3);
    assert!(cal["noise_floor_ratio"]["p50_ms"].as_f64().unwrap() > 0.0);
    // The two calibration runs are two identical runs.
    let o = env.run(&["compare", "last~1", "last", "-o", "json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let v = json(&o);
    assert_eq!(v["comparable"], true);
    assert_eq!(v["verdict"], "passed");
    for s in v["stats"].as_array().unwrap() {
        assert_eq!(s["significant"], false, "{s}");
        assert_eq!(s["confidence"], "low", "one run per side: {s}");
    }
}

#[test]
fn zero_to_one_throttled_response_does_not_fail_the_gate() {
    let env = Env::new();
    let a = env.path("a.json");
    let o = env.run(&[
        "run",
        "memory://",
        "--preset",
        "small-files",
        "--objects",
        "4",
        "--object-size",
        "16KiB",
        "-c",
        "2",
        "-d",
        "500ms",
        "--warmup",
        "0s",
        "--json",
        a.to_str().unwrap(),
        "-q",
    ]);
    assert!(o.status.success(), "{}", err(&o));
    let mut r: Value = serde_json::from_str(&std::fs::read_to_string(&a).unwrap()).unwrap();
    r["resilience"]["throttled_responses_count"] = 1.into();
    r["resilience"]["status_503_count"] = 1.into();
    r["run"]["id"] = "20261121-000000-b0b0".into();
    let b = env.path("b.json");
    std::fs::write(&b, serde_json::to_string(&r).unwrap()).unwrap();
    let o = env.run(&[
        "compare",
        a.to_str().unwrap(),
        b.to_str().unwrap(),
        "-o",
        "json",
    ]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let v = json(&o);
    let chk = v["gate"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "throttled_request_ratio")
        .expect("throttling check")
        .clone();
    assert_eq!(chk["verdict"], "pass", "{chk}");
    assert_eq!(chk["unit"], "ratio");
    for k in ["baseline", "candidate", "delta_ratio", "limit_ratio"] {
        assert!(chk.get(k).is_some(), "{k}: {chk}");
    }
}
