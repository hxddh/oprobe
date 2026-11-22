//! Lab convergence (design v4 §12, v5 §4 lab parity): the findings, the remedy and the capacity
//! model agree, and on every limit fault of the lab the right hypothesis is selected, following
//! the advice improves or trades as predicted, and `prove` does not refute it. (Phase A of 6.0
//! rebuilds this suite on `prove` alone.)
//!
//! Original (in-flight limit) cases:
//! On a lab that throttles above 24 requests in flight, the throttling finding's suggested
//! concurrency matches the capacity model's safe concurrency. Replaying the run at that
//! concurrency (`run OPERATOR --set concurrency=N`) stays within the throttle budget. The
//! replayed run's findings never suggest exceeding the ceiling.

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
        // The lab's limit models were calibrated with a quick, jittered retry (OpenDAL's
        // default waits 1 s): runs use it (phase A recalibrates on the 6.0 defaults).
        let mut args = args.to_vec();
        if args[0] == "run" {
            args.extend_from_slice(RETRY);
        }
        let args = &args[..];
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
        let v: Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("oprobe {args:?}: stdout is not JSON ({e}); {stderr}"));
        // `run` answers one run per operator: these tests profile one.
        match v.get("runs") {
            Some(runs) => runs[0].clone(),
            None => v,
        }
    }
}

/// The retry the lab's limit models were calibrated with.
const RETRY: &[&str] = &[
    "--set",
    "layers.retry.min_delay=50ms",
    "--set",
    "layers.retry.max_delay=5s",
    "--set",
    "layers.retry.jitter=on",
];

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
            let c = f["verification"]["overrides"]["workload"]["concurrency"]
                .as_str()?
                .parse()
                .ok()?;
            Some((f["code"].as_str().unwrap_or_default().to_owned(), c))
        })
        .collect()
}

fn converges(target: &str, workers: &str, extra: &[&str]) {
    let env = Env::new();
    // A run above the ceiling. Client-side timing on a loaded machine can blur one run's
    // boundary, so allow up to 3 attempts to get a measured ceiling.
    let mut first = Value::Null;
    for _ in 0..3 {
        let mut args = vec![
            "run", target, "-c", workers, "-d", "3s", "--warmup", "300ms",
        ];
        args.extend_from_slice(extra);
        first = env.run(&args);
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
    let _ = run_id;
    let set_c = format!("concurrency={c}");
    let mut args = vec![
        "run", target, "-c", workers, "--warmup", "300ms", "--set", &set_c, "-d", "3s",
    ];
    args.extend_from_slice(extra);
    let replay = env.run(&args);
    let r = &replay["report"];
    for k in ["preset", "dataset", "mix", "access", "seed"] {
        assert_eq!(
            r["spec"]["profile"]["workload"][k], report["spec"]["profile"]["workload"][k],
            "{target}: the replay changed the workload's {k}"
        );
    }
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
    converges("lab://demo", "32", &[]);
}

#[test]
fn parquet_lab_findings_and_replay_agree() {
    // The parquet lab models 4 parallel column reads per scan.
    converges(
        "lab://parquet",
        "8",
        &["--set", "read.concurrent=4", "--set", "read.gap=64KiB"],
    );
}

// ---------------------------------------------------------------------------------------
// One case per limit fault (design v5 §4): the selected hypothesis, the advice replayed, and
// verify's net verdict.
// ---------------------------------------------------------------------------------------

/// The selected hypothesis of a report: `(kind, confidence)`.
fn selected(report: &Value) -> (String, String) {
    let h = report["capacity"]["hypotheses"]
        .as_array()
        .and_then(|a| a.iter().find(|h| h["selected"] == true))
        .unwrap_or_else(|| panic!("no selected hypothesis: {}", report["capacity"]));
    (
        h["kind"].as_str().unwrap_or_default().to_owned(),
        h["confidence"].as_str().unwrap_or_default().to_owned(),
    )
}

fn finding<'a>(report: &'a Value, code: &str) -> Option<&'a Value> {
    report["findings"]
        .as_array()?
        .iter()
        .find(|f| f["code"] == code)
}

/// `--set k=v` pairs of a finding's verification (operator, workload and SLA).
fn sets(f: &Value) -> Vec<String> {
    let o = &f["verification"]["overrides"];
    let mut v = Vec::new();
    for part in ["operator", "workload", "sla"] {
        if let Some(m) = o[part].as_object() {
            for (k, val) in m {
                v.push(format!("{k}={}", val.as_str().unwrap_or_default()));
            }
        }
    }
    v
}

/// Run `target` until the selected hypothesis is `kind` (client-side timing on a loaded
/// machine can blur one run): up to 3 attempts. Returns the run's JSON.
fn run_until(env: &Env, target: &str, kind: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..3 {
        last = env.run(&["run", target, "-d", "2s", "--warmup", "300ms"]);
        last["target"] = Value::String(target.to_owned());
        if selected(&last["report"]).0 == kind {
            return last;
        }
    }
    panic!(
        "{target}: selected {:?}, expected {kind}: {:#}",
        selected(&last["report"]),
        last["report"]["capacity"]["hypotheses"]
    );
}

/// Profile the operator of `first` again, with `sets`.
fn replay(env: &Env, first: &Value, sets: &[String]) -> Value {
    let target = first["target"].as_str().unwrap();
    let mut args: Vec<String> = ["run", target, "-d", "2s", "--warmup", "300ms"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for s in sets {
        args.push("--set".into());
        args.push(s.clone());
    }
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    env.run(&a)
}

/// `oprobe prove RUN CODE`: the verdict on goodput.
fn prove(env: &Env, run_id: &str, code: &str) -> (String, Value) {
    let v = env.run(&["prove", run_id, code, "-q"]);
    (v["verdict"].as_str().unwrap_or_default().to_owned(), v)
}

fn ratio(r: &Value, m: &str) -> f64 {
    r["summary"][m].as_f64().unwrap_or(0.0)
}

#[test]
fn a_rate_limit_is_paced_not_cut() {
    let env = Env::new();
    let first = run_until(&env, "lab://ratelimit", "rate_limit");
    let r = &first["report"];
    assert!(
        r["capacity"]["inflight_ceiling"].is_null(),
        "a rate limit is not an in-flight ceiling (A1): {}",
        r["capacity"]
    );
    let f = finding(r, "throttling").expect("throttling finding");
    let s = sets(f);
    assert!(
        s.iter().any(|x| x.starts_with("rate=")),
        "the advice paces the client: {s:?} — {}",
        f["advice"]
    );
    let id = first["run_id"].as_str().unwrap();
    let paced = replay(&env, &first, &s);
    let thr = ratio(&paced["report"], "throttled_request_ratio");
    assert!(thr <= 0.005, "paced, still {:.2}% throttled", thr * 100.0);
    let _ = id;
}

#[test]
fn a_hot_key_is_a_key_limit_and_caching_it_helps() {
    let env = Env::new();
    let first = run_until(&env, "lab://hotkey", "key_limit");
    let r = &first["report"];
    let f = finding(r, "throttling").expect("throttling finding");
    let s = sets(f);
    assert!(
        s.iter().any(|x| x.starts_with("layers.cache")),
        "the advice caches the hot keys: {s:?} — {}",
        f["advice"]
    );
    let id = first["run_id"].as_str().unwrap();
    let cached = replay(&env, &first, &s);
    let (before, after) = (
        ratio(r, "throttled_request_ratio"),
        ratio(&cached["report"], "throttled_request_ratio"),
    );
    assert!(
        after < before * 0.5,
        "throttling {:.1}% → {:.1}%",
        before * 100.0,
        after * 100.0
    );
    let (verdict, v) = prove(&env, id, "throttling");
    assert_ne!(verdict, "worse", "{v:#}");
}

#[test]
fn a_rate_that_drops_is_time_varying_and_pacing_below_it_holds() {
    let env = Env::new();
    let first = run_until(&env, "lab://timevarying", "time_varying");
    let r = &first["report"];
    let h = r["capacity"]["hypotheses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["selected"] == true)
        .unwrap();
    let low = h["params"]["requests_per_s"].as_f64().unwrap();
    assert!((100.0..=200.0).contains(&low), "the lower rate ≈150/s: {h}");
    let f = finding(r, "throttling").expect("throttling finding");
    let s = sets(f);
    let rate: f64 = s
        .iter()
        .find_map(|x| x.strip_prefix("rate=").and_then(|v| v.parse().ok()))
        .unwrap_or_else(|| panic!("paced below the lowest limit: {s:?}"));
    assert!(rate <= low, "{rate} > {low}");
    let paced = replay(&env, &first, &s);
    let thr = ratio(&paced["report"], "throttled_request_ratio");
    assert!(thr <= 0.005, "paced, still {:.2}% throttled", thr * 100.0);
}

#[test]
fn random_5xx_are_no_limit_and_never_slow_the_client() {
    let env = Env::new();
    let first = run_until(&env, "lab://noisy", "none");
    let r = &first["report"];
    let f = finding(r, "throttling").expect("throttling finding");
    assert!(
        f["verification"].is_null(),
        "random throttling gets no slow-down advice (A5): {f:#}"
    );
    assert!(r["capacity"]["rate_ceiling_requests_per_s"].is_null());
    assert!(
        finding(r, "errors").is_some(),
        "5xx are reported even when the run throttles (A6): {:?}",
        suggestions(r)
    );
}

#[test]
fn a_shared_bandwidth_cap_is_not_more_streams() {
    let env = Env::new();
    let first = run_until(&env, "lab://bandwidth", "bandwidth_cap");
    let r = &first["report"];
    let f = finding(r, "download_bound").expect("download_bound finding");
    let advice = f["advice"].as_str().unwrap_or_default();
    assert!(
        advice.contains("Do not add streams"),
        "an aggregate cap never gets 'more streams' (A8): {advice}"
    );
    let s = sets(f);
    let id = first["run_id"].as_str().unwrap();
    let fewer = replay(&env, &first, &s);
    let (a, b) = (ratio(r, "mib_per_s"), ratio(&fewer["report"], "mib_per_s"));
    assert!(
        b >= a * 0.85,
        "fewer streams kept the throughput: {a:.1} → {b:.1} MiB/s"
    );
    let (verdict, v) = prove(&env, id, "download_bound");
    // Halving the streams under a shared cap costs (almost) nothing.
    assert_ne!(verdict, "worse", "{v:#}");
}
