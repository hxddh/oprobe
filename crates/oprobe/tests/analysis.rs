//! Findings end to end: a lab with stragglers, profiled through the CLI, its finding, evidence
//! and remedy, and `show --by stage`. Every test uses its own config and data directory.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use oprobe::lab::{LabConfig, LabServer};

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
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
        c.env("OPROBE_DATA_DIR", self.path("data"))
            .env("OPROBE_CONFIG", self.path("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["--color", "never"]);
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&o.stdout),
            err(o)
        )
    })
}

fn background_lab(cfg: LabConfig) -> (tokio::runtime::Runtime, oprobe::lab::LabHandle) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(async {
        LabServer::spawn(
            "127.0.0.1:0",
            LabConfig {
                throttle_status_code: 503,
                ..cfg
            },
        )
        .await
        .expect("lab starts")
    });
    (rt, h)
}

fn lab_args(endpoint: &str) -> Vec<String> {
    let endpoint = format!("options.endpoint={endpoint}");
    [
        "s3://bkt/x",
        "--set",
        endpoint.as_str(),
        "--set",
        "http.proxy=none",
        "--set",
        "options.access_key_id=x",
        "--set",
        "options.secret_access_key=x",
        "--set",
        "options.region=us-east-1",
        "--preset",
        "small-files",
        "--objects",
        "8",
        "--object-size",
        "16KiB",
        "--warmup",
        "0s",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn stragglers_are_found_explained_and_attributed() {
    let env = Env::new();
    let (_rt, lab) = background_lab(LabConfig {
        straggler_every_n: 25,
        straggler_delay: Duration::from_millis(300),
        base_ttfb_delay: Duration::from_millis(3),
        ..Default::default()
    });
    let o = env
        .cmd()
        .arg("run")
        .args(lab_args(&lab.endpoint_url))
        .args(["-c", "8", "-d", "3s", "-o", "json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v = &json(&o)["runs"][0];
    let findings = v["report"]["findings"].as_array().unwrap();
    let tail = findings
        .iter()
        .find(|f| f["code"] == "tail_stragglers")
        .unwrap_or_else(|| panic!("no straggler finding: {findings:#?}"));
    let vals = &tail["evidence"]["values"];
    // The injected stragglers wait 300 ms on top of a few ms, and a replay of the requests
    // through TailCutLayer's rule takes most of the p99 away — even on OpenDAL's default
    // retry (the remedy re-issues with a short backoff).
    assert!(vals["straggler_pct"].as_f64().unwrap() > 0.0, "{tail}");
    assert!(
        vals["straggler_ttfb_ms"].as_f64().unwrap() >= 250.0,
        "{tail}"
    );
    let (p99, replayed) = (
        vals["p99_ms"].as_f64().unwrap(),
        vals["replayed_p99_ms"].as_f64().unwrap(),
    );
    assert!(p99 >= 250.0 && replayed < p99 / 2.0, "{tail}");
    assert!(!tail["evidence"]["op_ids"].as_array().unwrap().is_empty());
    // The remedy is a path, the next command proves it.
    let run_id = v["run_id"].as_str().unwrap();
    assert_eq!(tail["remedy"]["path"], "layers.tail_cut", "{tail}");
    assert_eq!(
        tail["remedy"]["next"].as_str().unwrap(),
        format!("oprobe prove {run_id} tail_stragglers")
    );
    // At most one tail finding.
    assert_eq!(
        findings
            .iter()
            .filter(|f| f["code"].as_str().unwrap().starts_with("tail"))
            .count(),
        1
    );
    // Where the time went: the stragglers' time is TTFB.
    let by = env.run(&["show", run_id, "--by", "stage", "-o", "json"]);
    assert!(by.status.success(), "{}", err(&by));
    let bv = json(&by);
    assert!(
        bv["by"]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["api"] == "GetObject"),
        "{bv}"
    );
    assert_eq!(bv["by"]["groups"][0]["key"], "ttfb", "{bv}");
}

/// Slow operations keep the async call stack captured while they were in flight, in the stored
/// run: rows are streamed to the run file as operations complete, so the stack must be on the
/// row then (a stack patched in from the slow-op samples afterwards never reached the file).
#[test]
fn slow_operations_keep_their_await_tree() {
    let env = Env::new();
    let o = env.run(&["run", "lab://stragglers", "-d", "3s", "-o", "json"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let o = env.run(&["show", "last", "--detail"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        text.contains("async call stack captured while in flight:"),
        "{text}"
    );
}

/// S3 also throttles with `503 ServiceUnavailable` ("reduce your request rate"): it counts as
/// throttling, not as server errors (9.0 counted only `SlowDown` and 429), and the finding is
/// `throttling`, never `errors` (macOS CI, 9.2: `["client_bound", "errors"]`).
///
/// Decided by construction, whatever the machine: a request holds its slot for 50 ms, so
/// the 8 workers overlap however slowly they start, and the limit of 2 rejects 6 of them.
/// With OpenDAL's default retry those 6 come back 1 s later, in step, and are rejected again
/// by the 2 that ran on: about a dozen 503s in the 2 s window, in two bursts — too few for
/// any limit model, which is the shape that read as random errors.
#[test]
fn a_503_service_unavailable_is_counted_as_throttling() {
    let env = Env::new();
    let (_rt, lab) = background_lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(50),
        max_inflight_before_503: Some(2),
        throttle_service_unavailable: true,
        ..Default::default()
    });
    let o = env
        .cmd()
        .arg("run")
        .args(lab_args(&lab.endpoint_url))
        .args(["-c", "8", "-d", "2s", "-o", "json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let r = &json(&o)["runs"][0]["report"];
    let res = &r["resilience"];
    assert!(lab.emitted_throttles() > 0, "8 workers > 2 throttle");
    assert!(
        res["throttled_responses_count"].as_u64().unwrap() > 0,
        "{res}"
    );
    assert!(res["status_503_count"].as_u64().unwrap() > 0, "{res}");
    assert_eq!(res["server_errors_count"], 0, "{res}");
    let codes: Vec<&str> = r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"throttling"), "{codes:?}");
    assert!(!codes.contains(&"errors"), "{codes:?}");
}

/// K2-2 and K4-1: an open loop that asks `lab://queueing` (four requests served at once,
/// 10 ms each: 400 a second) for 1 000 operations a second. 9.2 printed "[Attention] The 16
/// workers set the pace, not the service … raise the workers (`-c`)", at 16 workers and at 64
/// alike, and its checks passed.
///
/// Decided by construction: the lab cannot serve more than 400 of the 1 000 arrivals a
/// second, so more than half of them never complete on any machine, far over the 1 % that
/// leaves the p99 without an upper bound.
#[test]
fn an_overloaded_open_loop_is_stalled_and_not_blamed_on_the_workers() {
    let env = Env::new();
    for workers in ["16", "64"] {
        let o = env.run(&[
            "run",
            "lab://queueing",
            "--rate",
            "1000",
            "-c",
            workers,
            "-d",
            "2s",
            "-o",
            "json",
        ]);
        assert!(o.status.success(), "{}", err(&o));
        let r = &json(&o)["runs"][0]["report"];
        // Stalled, with no limit set: the checks fail, the verdict is critical, and the
        // headline leads with the arithmetic and the run's own p99.
        let breaches = r["verdict"]["sla"]["breaches"].as_array().unwrap();
        assert_eq!(breaches.len(), 1, "{breaches:?}");
        let b = breaches[0].as_str().unwrap();
        assert!(b.contains("scheduled arrivals never completed"), "{b}");
        assert!(b.ends_with("has no upper bound"), "{b}");
        assert_eq!(r["verdict"]["level"], "critical");
        let h = r["verdict"]["headline"].as_str().unwrap();
        assert!(h.starts_with(b), "{h}");
        let missed = r["open_loop"]["missed_count"].as_u64().unwrap();
        let intended = r["open_loop"]["intended_count"].as_u64().unwrap();
        assert!(missed * 2 > intended, "{missed} of {intended}");
        assert!(b.starts_with(&format!("{missed} of {intended} ")), "{b}");
        // No finding tells the user to add workers: the same lab carried the same rate
        // with four times as many.
        for f in r["findings"].as_array().unwrap() {
            let text = format!("{} {}", f["cause"], f["remedy"]["text"]);
            assert!(!text.contains("`-c`"), "{workers} workers: {f:#}");
            assert!(
                !text.contains("set the pace, not the service: the arrivals"),
                "{f:#}"
            );
            let next = f["remedy"]["next"].as_str().unwrap_or_default();
            assert!(!next.starts_with("oprobe prove"), "{next}");
        }
    }
}
