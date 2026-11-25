//! Lab convergence (design v6 §8 "Tests"): `prove` on each lab fault. For every scenario the
//! run names the right finding with the right remedy path (and no sizing number), `prove`
//! sizes the remedy to a value within tolerance of the lab's true limit, and the verdict on the
//! objective is `improved`. A fault-free lab invents no limit.
//!
//! Objectives follow what the remedy changes on OpenDAL's defaults (retry after 1 s, no
//! jitter): capping requests in flight removes the throttled operations' 1 s backoffs from
//! the tail but cannot raise goodput when spare workers already keep the service at its limit,
//! so the throttle-family cases prove on p99; stragglers and serial reads prove on goodput.
//!
//! Timing on a loaded machine can blur one short run, so a run that does not show its fault is
//! repeated (at most 3 times); no test bounds wall-clock time.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

/// One lab test at a time, also under `cargo test`, which runs a binary's tests as threads of
/// one process (nextest runs each in its own process; `.config/nextest.toml` groups them).
/// Every test measures — a prove's noise floor, a top finding, a fitted limit — and two at
/// once load the same cores and measure each other.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Env {
    dir: tempfile::TempDir,
    _one: std::sync::MutexGuard<'static, ()>,
}

impl Env {
    fn new() -> Self {
        Self {
            // A test that failed while holding it poisons nothing the next one needs.
            _one: ONE_AT_A_TIME
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn json(&self, args: &[&str]) -> Value {
        let o: Output = Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", self.path("data"))
            .env("OPROBE_CONFIG", self.path("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["--color", "never", "-q"])
            .args(args)
            .args(["-o", "json"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(
            o.status.code().is_some_and(|c| c == 0 || c == 3),
            "oprobe {args:?} failed: {stderr}\n{}",
            String::from_utf8_lossy(&o.stdout)
        );
        serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("oprobe {args:?}: stdout is not JSON ({e}); {stderr}"))
    }

    /// `oprobe run ARGS` until the report has finding `code` (at most 3 runs); the run's
    /// report.
    fn run_until(&self, args: &[&str], code: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..3 {
            let mut a = vec!["run"];
            a.extend_from_slice(args);
            let v = self.json(&a);
            last = v["runs"][0]["report"].clone();
            if finding(&last, code).is_some() {
                return last;
            }
        }
        panic!(
            "{args:?}: no `{code}` finding in 3 runs: {:#}",
            last["findings"]
        );
    }

    /// The finding's `next` command exactly as printed (the output flags aside): it must be an
    /// `oprobe prove`.
    fn run_next(&self, f: &Value) -> Value {
        let next = f["remedy"]["next"].as_str().unwrap();
        let args: Vec<&str> = next.split_whitespace().collect();
        assert_eq!(&args[..2], ["oprobe", "prove"], "{next}");
        self.json(&args[1..])
    }

    /// `oprobe prove RUN CODE ARGS` (short trials).
    fn prove(&self, run_id: &str, code: &str, extra: &[&str]) -> Value {
        let mut a = vec!["prove", run_id, code, "--trial", "1s"];
        a.extend_from_slice(extra);
        self.json(&a)
    }
}

fn finding<'a>(report: &'a Value, code: &str) -> Option<&'a Value> {
    report["findings"]
        .as_array()?
        .iter()
        .find(|f| f["code"] == code)
}

/// The finding names its remedy path and carries no sizing number (T1, T2); its `next` proves
/// the remedy on the objective it is proved on (W8: p99 for the throttle family).
fn check_finding(report: &Value, code: &str, path: Option<&str>) -> Value {
    let f = finding(report, code).unwrap().clone();
    assert_eq!(f["remedy"]["path"].as_str(), path, "{code}: {f:#}");
    let text = f["remedy"]["text"].as_str().unwrap().replace("HTTP/2", "");
    assert!(
        !text.chars().any(|c| c.is_ascii_digit()),
        "no number in the remedy: {text}"
    );
    let id = report["run"]["id"].as_str().unwrap();
    let objective = match code {
        "throttling" | "rate_limit" => " --objective p99",
        _ => "",
    };
    assert_eq!(
        f["remedy"]["next"].as_str().unwrap(),
        format!("oprobe prove {id} {code}{objective}")
    );
    f
}

fn sized(p: &Value) -> String {
    p["sizing"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("a sized value: {p:#}"))
        .to_owned()
}

fn improved(p: &Value) {
    assert_eq!(p["verdict"], "improved", "{p:#}");
    let d = &p["objective_delta"];
    let (lo, hi) = (d["low"].as_f64().unwrap(), d["high"].as_f64().unwrap());
    assert!(lo > -1.0 && lo <= hi, "the interval stays in range: {d}");
    // Trials are children of the prove.
    assert!(p["trials"].as_array().unwrap().len() >= 4);
}

/// An in-flight limit of 8: the cap is sized at the limit and removes the 1 s backoffs from
/// the tail.
#[test]
fn an_inflight_limit_is_capped_at_the_limit() {
    let env = Env::new();
    let r = env.run_until(&["lab://throttle", "-c", "32", "-d", "2s"], "throttling");
    let f = check_finding(&r, "throttling", Some("layers.concurrent_limit.http"));
    assert_eq!(f["remedy"]["with"][0], "layers.retry.jitter");
    let p = env.prove(
        r["run"]["id"].as_str().unwrap(),
        "throttling",
        &["--objective", "p99"],
    );
    let n: u64 = sized(&p).parse().unwrap();
    assert!((6..=9).contains(&n), "sized {n}, the lab throttles above 8");
    improved(&p);
    let rust = p["diff"]["rust"].as_str().unwrap();
    assert!(
        rust.contains(&format!("with_http_concurrent_limit({n})")),
        "{rust}"
    );
    assert!(
        p["diff"]["toml"]
            .as_str()
            .unwrap()
            .contains("concurrent_limit")
    );
}

/// A token bucket of 300 requests/s: a rate limit, sized from rate × latency so the capped
/// client stays under the rate. The client retries quickly with jitter: with OpenDAL's default
/// (1 s, no jitter) the workers wake in step and a 2 s run of a token bucket cannot be told
/// from an in-flight limit — the remedy path is the same either way.
#[test]
fn a_rate_limit_is_capped_under_the_rate() {
    let env = Env::new();
    // A loaded machine can starve one short run below the rate: up to 3 runs.
    let fitted = |r: &Value| {
        finding(r, "rate_limit")
            .and_then(|f| f["evidence"]["values"]["fitted_requests_per_s"].as_f64())
            .unwrap_or(0.0)
    };
    let mut r = Value::Null;
    for _ in 0..3 {
        r = env.run_until(
            &[
                "lab://ratelimit",
                "-d",
                "2s",
                "--set",
                "layers.retry.min_delay=50ms",
                "--set",
                "layers.retry.jitter=on",
            ],
            "rate_limit",
        );
        if (240.0..=360.0).contains(&fitted(&r)) {
            break;
        }
    }
    check_finding(&r, "rate_limit", Some("layers.concurrent_limit.http"));
    let rps = fitted(&r);
    assert!(
        (240.0..=360.0).contains(&rps),
        "fitted {rps}/s, the lab allows 300"
    );
    assert!(
        r["capacity"]["inflight_ceiling"].is_null(),
        "not an in-flight limit (A1)"
    );
    let p = env.prove(
        r["run"]["id"].as_str().unwrap(),
        "rate_limit",
        &["--objective", "p99"],
    );
    let n: u64 = sized(&p).parse().unwrap();
    assert!((1..=3).contains(&n), "sized {n}");
    improved(&p);
    // The capped client runs at (and not above) the lab's rate.
    for t in p["trials"].as_array().unwrap() {
        if t["kind"] == "candidate" {
            let gp = t["goodput_ops_per_s"].as_f64().unwrap();
            assert!(gp <= 300.0 * 1.15, "{gp} ops/s over a 300/s limit");
        }
    }
}

/// The default `lab://ratelimit` (OpenDAL's default retry: 1 s backoff, no jitter) is a rate
/// limit, not an in-flight limit: the workers wake in step and throttle only above a couple of
/// requests in flight, but the bursts the bucket serves after each lull are more than any cap
/// of that size carries (design 6.3 A1). A run that saw no throttling is repeated; no run may
/// name an in-flight limit.
#[test]
fn the_default_rate_limit_lab_is_a_rate_limit() {
    let env = Env::new();
    let mut found = None;
    for _ in 0..3 {
        let v = env.json(&["run", "lab://ratelimit"]);
        let r = v["runs"][0]["report"].clone();
        assert!(
            finding(&r, "throttling").is_none(),
            "the rate limit read as an in-flight limit: {:#}",
            r["capacity"]["hypotheses"]
        );
        if finding(&r, "rate_limit").is_some() {
            found = Some(r);
            break;
        }
    }
    let r = found.expect("a rate_limit finding in 3 runs");
    let f = check_finding(&r, "rate_limit", Some("layers.concurrent_limit.http"));
    assert!(
        r["capacity"]["inflight_ceiling"].is_null(),
        "not an in-flight limit (A1)"
    );
    // The timeline shows the request rate and the throttled responses where they happened.
    let tl = r["timeline"].as_array().unwrap();
    let requests: u64 = tl
        .iter()
        .map(|p| p["requests_count"].as_u64().unwrap())
        .sum();
    let throttled: u64 = tl
        .iter()
        .map(|p| p["throttled_count"].as_u64().unwrap())
        .sum();
    assert_eq!(
        requests,
        r["connections"]["requests_count"].as_u64().unwrap()
    );
    assert_eq!(
        throttled,
        r["resilience"]["throttled_responses_count"]
            .as_u64()
            .unwrap()
    );
    // W8: the finding's `next`, exactly as printed (default trials, its objective), decides.
    let p = env.run_next(&f);
    assert_eq!(p["objective"], "p99", "{p:#}");
    improved(&p);
}

/// The default `lab://throttle` (32 workers over an in-flight limit of 8): the finding's
/// `next`, run exactly as printed, proves the cap `improved` on p99 (W8: 9.0's `next` named no
/// objective, and at 16 workers the throttled operations' backoffs sat above the p99).
#[test]
fn the_default_throttle_lab_proves_its_next_command() {
    let env = Env::new();
    let r = env.run_until(&["lab://throttle"], "throttling");
    let f = check_finding(&r, "throttling", Some("layers.concurrent_limit.http"));
    let p = env.run_next(&f);
    assert_eq!(p["finding"], "throttling");
    assert_eq!(p["objective"], "p99", "{p:#}");
    let n: u64 = sized(&p).parse().unwrap();
    assert!((6..=9).contains(&n), "sized {n}, the lab throttles above 8");
    improved(&p);
    // The A/B order of the pairs is drawn from the run's seed, and the result says so (W11).
    let notes = p["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("A/B order")),
        "{notes:?}"
    );
}

/// Zipf reads over per-key buckets of 40 requests/s: a key limit. Its remedy is the key
/// layout, not a layer: `next` shows the hot keys, and `prove` refuses it (W9: 9.0 said "not a
/// layer" and suggested `prove … key_limit`).
#[test]
fn a_hot_key_is_a_key_limit() {
    let env = Env::new();
    let r = env.run_until(&["lab://hotkey", "-d", "2s"], "key_limit");
    let f = finding(&r, "key_limit").unwrap();
    assert!(f["remedy"]["path"].is_null(), "{f:#}");
    let text = f["remedy"]["text"].as_str().unwrap();
    assert!(text.contains("not a layer"), "{text}");
    let id = r["run"]["id"].as_str().unwrap();
    assert_eq!(
        f["remedy"]["next"].as_str().unwrap(),
        format!("oprobe show {id} --detail")
    );
    let rps = f["evidence"]["values"]["fitted_requests_per_s"]
        .as_f64()
        .unwrap();
    assert!(
        (20.0..=60.0).contains(&rps),
        "fitted {rps}/s per key, the lab allows 40"
    );
    let o = Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .env("OPROBE_DATA_DIR", env.path("data"))
        .env("OPROBE_CONFIG", env.path("config.toml"))
        .args(["prove", id, "key_limit", "-o", "json"])
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let e: Value = serde_json::from_slice(&o.stdout).unwrap();
    let msg = e["error"]["message"].as_str().unwrap();
    assert!(msg.contains("no operator remedy to prove"), "{e:#}");
    let hint = e["error"]["hint"].as_str().unwrap();
    assert!(
        hint.contains(&format!("oprobe show {id} --detail")),
        "{hint}"
    );
}

/// 3 % of requests wait 400 ms: stragglers, cut by TailCutLayer (with a short re-issue
/// backoff) for more goodput.
#[test]
fn stragglers_are_cut() {
    let env = Env::new();
    let r = env.run_until(&["lab://stragglers", "-d", "2s"], "tail_stragglers");
    check_finding(&r, "tail_stragglers", Some("layers.tail_cut"));
    let p = env.prove(r["run"]["id"].as_str().unwrap(), "tail_stragglers", &[]);
    assert!(
        p["change"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "layers.tail_cut=on"),
        "{p:#}"
    );
    improved(&p);
}

/// Every response stream shares 16 MiB/s: a bandwidth cap, named as such, with the fitted cap
/// in its evidence. OpenDAL's ThrottleLayer shapes writes only, so on this read workload the
/// cap is not a layer (K16: 7.0 named `layers.throttle.bandwidth`, which cannot help reads)
/// and there is nothing to prove.
#[test]
fn a_shared_bandwidth_cap_on_reads_is_not_a_layer() {
    let env = Env::new();
    let r = env.run_until(&["lab://bandwidth", "-d", "2s"], "bandwidth_cap");
    let f = finding(&r, "bandwidth_cap").unwrap();
    assert!(f["remedy"]["path"].is_null(), "{f:#}");
    let text = f["remedy"]["text"].as_str().unwrap();
    assert!(text.contains("not a layer"), "{text}");
    let id = r["run"]["id"].as_str().unwrap();
    assert_eq!(
        f["remedy"]["next"].as_str().unwrap(),
        format!("oprobe show {id} --detail")
    );
    let bps = f["evidence"]["values"]["fitted_bytes_per_s"]
        .as_f64()
        .unwrap();
    let mib = bps / (1024.0 * 1024.0);
    assert!(
        (12.0..=20.0).contains(&mib),
        "fitted {mib} MiB/s, the lab caps at 16"
    );
}

/// Transient failures that retries recover (`lab://flaky`: 4 % 500s and 2 % connection resets;
/// `lab://noisy`: 5 % random 5xx): an `errors` finding whose remedy is the first retry backoff,
/// with jitter. With every default, `prove` sizes it short and proves `improved` (K1: 7.0
/// named jitter alone at OpenDAL's 1 s backoff, and the prove came out worse).
#[test]
fn retried_transient_failures_prove_a_shorter_backoff() {
    let env = Env::new();
    for scenario in ["lab://flaky", "lab://noisy"] {
        let r = env.run_until(&[scenario, "-d", "2s"], "errors");
        let f = check_finding(&r, "errors", Some("layers.retry.min_delay"));
        assert_eq!(f["remedy"]["with"][0], "layers.retry.jitter", "{f:#}");
        // The errors finding by name: on a slow machine the random 503s can also rank a
        // throttling finding first.
        let p = env.json(&["prove", r["run"]["id"].as_str().unwrap(), "errors"]);
        assert_eq!(p["finding"], "errors", "{scenario}: {p:#}");
        assert_eq!(p["objective"], "goodput");
        let v = sized(&p);
        let ms: u64 = v.trim_end_matches("ms").parse().unwrap();
        assert!((20..=500).contains(&ms), "{scenario}: sized {v}");
        assert!(
            p["change"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "layers.retry.jitter=on"),
            "{p:#}"
        );
        improved(&p);
    }
}

/// 4 MiB/s per response stream: large reads streamed through one request each are split
/// into concurrent chunked requests.
#[test]
fn serial_reads_on_a_lossy_link_are_split() {
    let env = Env::new();
    let r = env.run_until(
        &[
            "lab://lossy",
            "--preset",
            "small-files",
            "--objects",
            "8",
            "--object-size",
            "4MiB",
            "-c",
            "4",
            "-d",
            "2s",
        ],
        "small_ranges",
    );
    check_finding(&r, "small_ranges", Some("read.concurrent"));
    let p = env.prove(r["run"]["id"].as_str().unwrap(), "small_ranges", &[]);
    let n: u64 = sized(&p).parse().unwrap();
    assert!(n >= 2, "sized {n}");
    assert!(
        p["change"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str().unwrap().starts_with("read.chunk=")),
        "{p:#}"
    );
    improved(&p);
}

/// Parquet-like scans (a HEAD, a footer, four column ranges): on OpenDAL's defaults the
/// ranges are fetched one after another, so `read.concurrent` is the remedy.
#[test]
fn parquet_ranges_are_fetched_concurrently() {
    let env = Env::new();
    let r = env.run_until(
        &["lab://parquet", "-d", "2s", "--warmup", "300ms"],
        "small_ranges",
    );
    check_finding(&r, "small_ranges", Some("read.concurrent"));
    let p = env.prove(r["run"]["id"].as_str().unwrap(), "small_ranges", &[]);
    let n: u64 = sized(&p).parse().unwrap();
    assert!(n >= 2, "sized {n}");
    improved(&p);
}

/// The same scans with four ranges in flight per scan cross the lab's limit of 24 requests
/// in flight: the cap is sized at that limit.
#[test]
fn parquet_scans_over_the_inflight_limit_are_capped() {
    let env = Env::new();
    let r = env.run_until(
        &[
            "lab://parquet",
            "-d",
            "2s",
            "--warmup",
            "300ms",
            "-c",
            "12",
            "--set",
            "read.concurrent=4",
        ],
        "throttling",
    );
    check_finding(&r, "throttling", Some("layers.concurrent_limit.http"));
    let p = env.prove(
        r["run"]["id"].as_str().unwrap(),
        "throttling",
        &["--objective", "p99"],
    );
    let n: u64 = sized(&p).parse().unwrap();
    assert!(
        (16..=26).contains(&n),
        "sized {n}, the lab throttles above 24"
    );
    improved(&p);
}

/// No fault: no limit is invented, and the verdict says one run cannot size a change (N2).
#[test]
fn a_clean_lab_gets_no_invented_limit() {
    let env = Env::new();
    for scenario in ["lab://clean", "lab://queueing"] {
        let v = env.json(&["run", scenario, "-d", "2s"]);
        let r = &v["runs"][0]["report"];
        for f in r["findings"].as_array().unwrap() {
            let code = f["code"].as_str().unwrap();
            assert!(
                !["throttling", "rate_limit", "key_limit", "bandwidth_cap"].contains(&code),
                "{scenario}: invented `{code}`: {f:#}"
            );
        }
        let h = r["verdict"]["headline"].as_str().unwrap();
        assert!(
            !h.contains("concurrency") && !h.contains("run at"),
            "{scenario}: {h}"
        );
        // Nothing was throttled, and no confidence is claimed for a limit model.
        assert_eq!(
            r["resilience"]["throttled_responses_count"], 0,
            "{scenario}"
        );
        for h in r["capacity"]["hypotheses"].as_array().unwrap() {
            if scenario == "lab://clean"
                && h["selected"] == true
                && h["kind"] != "none"
                && h["kind"] != "client_bound"
            {
                assert_ne!(h["confidence"], "high", "{scenario}: {h}");
            }
        }
    }
}

/// The first impression (6.3): `oprobe demo`, then `oprobe prove last` with every default — the
/// top finding's remedy, the default objective (goodput) and no budget flag — proves
/// `improved`, within `--max-throttle` (7.0: the lab's in-flight limit leaves room for the
/// stragglers' re-issues).
#[test]
fn the_demo_proves_its_top_finding() {
    let env = Env::new();
    let mut r = Value::Null;
    for _ in 0..3 {
        r = env.json(&["demo"])["runs"][0]["report"].clone();
        if r["findings"][0]["code"] == "tail_stragglers" {
            break;
        }
    }
    let top = &r["findings"][0];
    assert_eq!(top["code"], "tail_stragglers", "{:#}", r["findings"]);
    if let Some(t) = finding(&r, "throttling") {
        assert_ne!(
            t["severity"], "critical",
            "throttling that costs no goodput: {t:#}"
        );
    }
    let started = std::time::Instant::now();
    let p = env.json(&["prove", "last"]);
    assert_eq!(p["finding"], "tail_stragglers", "{p:#}");
    assert_eq!(p["objective"], "goodput");
    // Proven, and within --max-throttle (design 7 A2): the lab leaves room for the re-issues.
    improved(&p);
    let ratio = |kind: &str| {
        let ts: Vec<&Value> = p["trials"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == kind)
            .collect();
        let n = |k: &str| ts.iter().map(|t| t[k].as_u64().unwrap()).sum::<u64>();
        n("throttled_count") as f64 / n("requests_count").max(1) as f64
    };
    assert!(
        ratio("candidate") <= p["max_throttle_ratio"].as_f64().unwrap(),
        "{p:#}"
    );
    eprintln!(
        "demo → prove last: {} in {:.0} s ({} pairs)",
        p["objective_delta"]["estimate"],
        started.elapsed().as_secs_f64(),
        p["pairs_count"]
    );
}
