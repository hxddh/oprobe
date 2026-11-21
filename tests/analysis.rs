//! Analysis & export commands end to end: findings on a lab with stragglers, `analyze`,
//! `compare` (repeat-aware, `--baseline`), `baseline`, `run --repeat`, `runs export`,
//! `matrix` and `watch`. Every test uses its own config and data directory.

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

const MEM: &[&str] = &[
    "memory://",
    "--preset",
    "small-files",
    "--objects",
    "4",
    "--object-size",
    "16KiB",
    "-c",
    "2",
    "--warmup",
    "0s",
    "-d",
    "300ms",
];

fn mem_run(env: &Env, extra: &[&str]) -> Output {
    env.cmd().arg("run").args(MEM).args(extra).output().unwrap()
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
    [
        "s3://bkt/x",
        "--endpoint",
        endpoint,
        "--proxy",
        "none",
        "--option",
        "access_key_id=x",
        "--option",
        "secret_access_key=x",
        "--region",
        "us-east-1",
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
    let v = json(&o);
    let findings = v["report"]["findings"].as_array().unwrap();
    let tail = findings
        .iter()
        .find(|f| f["code"] == "tail_stragglers")
        .unwrap_or_else(|| panic!("no straggler finding: {findings:#?}"));
    assert!(
        tail["cause"].as_str().unwrap().contains("tail-cut at"),
        "{tail}"
    );
    let vals = &tail["evidence"]["values"];
    let predicted = vals["predicted_p99_ms"].as_f64().unwrap();
    let current = vals["current_p99_ms"].as_f64().unwrap();
    // The injected stragglers wait 300 ms on top of a few ms: whatever the runner's speed,
    // some requests are above the threshold, the threshold is below the straggler delay and
    // a tail-cut takes most of the p99 away.
    assert!(vals["straggler_pct"].as_f64().unwrap() > 0.0, "{tail}");
    assert!(
        vals["straggler_ttfb_ms"].as_f64().unwrap() >= 250.0,
        "{tail}"
    );
    assert!(
        vals["tail_cut_threshold_ms"].as_f64().unwrap() < 300.0,
        "{tail}"
    );
    assert!(current >= 250.0, "{tail}");
    assert!(predicted < current / 3.0, "{tail}");
    assert!(!tail["evidence"]["op_ids"].as_array().unwrap().is_empty());
    let ver = &tail["verification"];
    let run_id = v["run_id"].as_str().unwrap();
    assert_eq!(
        ver["command"].as_str().unwrap(),
        format!("oprobe verify {run_id} tail_stragglers")
    );
    assert_eq!(ver["overrides"]["knobs"]["tail_cut"], "on", "{ver}");
    let cmd = ver["replay"].as_str().unwrap();
    assert!(
        cmd.contains("--tail-cut")
            && cmd.contains(&format!("--endpoint {}", lab.endpoint_url))
            && cmd.contains("--concurrency 8"),
        "{cmd}"
    );
    // Both forms are valid: they plan the same run with tail-cut on.
    for cmd in [cmd, ver["rerun"].as_str().unwrap()] {
        let words: Vec<&str> = cmd.split_whitespace().collect();
        assert_eq!(words[0], "oprobe");
        let plan = env
            .cmd()
            .args(&words[1..])
            .args(["--dry-run", "-o", "json"])
            .output()
            .unwrap();
        assert!(plan.status.success(), "{cmd}: {}", err(&plan));
    }
    // At most one tail finding.
    assert_eq!(
        findings
            .iter()
            .filter(|f| f["code"].as_str().unwrap().starts_with("tail"))
            .count(),
        1
    );

    // analyze: groups, stage table, attribution, filters.
    let a = env.run(&["analyze", "last", "--by", "status", "-o", "json"]);
    assert!(a.status.success(), "{}", err(&a));
    let av = json(&a);
    assert_eq!(av["by"], "status");
    assert!(av["groups"][0]["key"].as_str().unwrap().starts_with('2'));
    assert!(
        av["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["api"] == "GetObject")
    );
    assert!(av["attribution"]["selected_count"].as_u64().unwrap() > 0);
    let w = env.run(&[
        "analyze",
        "last",
        "--where",
        "api=GetObject",
        "--window",
        "1..2",
        "--by",
        "second",
        "-o",
        "json",
    ]);
    assert!(w.status.success(), "{}", err(&w));
    let wv = json(&w);
    assert_eq!(wv["groups"].as_array().unwrap().len(), 1);
    assert_eq!(wv["groups"][0]["key"], "1");
    let text = env.run(&["analyze", "last", "--slowest", "5"]);
    let t = String::from_utf8_lossy(&text.stdout);
    assert!(
        t.contains("STAGES") && t.contains("OUTLIERS slowest 5%"),
        "{t}"
    );
    let bad = env.run(&["analyze", "last", "--by", "bogus", "-o", "json"]);
    assert_eq!(bad.status.code(), Some(2));
    assert_eq!(json(&bad)["error"]["code"], "usage.invalid_argument");
}

#[test]
fn exports_cover_every_format() {
    let env = Env::new();
    assert!(mem_run(&env, &["-q"]).status.success());
    for (fmt, file) in [
        ("json", "r.json"),
        ("ndjson", "r.ndjson"),
        ("csv", "r.csv"),
        ("har", "r.har"),
        ("otlp", "r.otlp.json"),
        ("md", "r.md"),
    ] {
        let out = env.path(file);
        let o = env.run(&[
            "runs",
            "export",
            "last",
            "--format",
            fmt,
            "--out",
            out.to_str().unwrap(),
            "-o",
            "json",
        ]);
        assert!(o.status.success(), "{fmt}: {}", err(&o));
        assert_eq!(json(&o)["format"], fmt);
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(!body.is_empty(), "{fmt}");
        match fmt {
            "har" => {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["log"]["version"], "1.2");
                assert!(!v["log"]["entries"].as_array().unwrap().is_empty());
            }
            "otlp" => {
                // One valid OTLP/JSON document (traces by default), not an array of signals.
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert!(v["resourceSpans"][0]["scopeSpans"][0]["spans"].is_array());
            }
            "csv" => assert!(body.starts_with("t_us,op_id,")),
            "ndjson" => assert!(body.lines().next().unwrap().contains("\"kind\":\"report\"")),
            _ => {}
        }
    }
    // OTLP: metrics alone, or both signals as two documents in a directory.
    let o = env.run(&[
        "runs", "export", "last", "-f", "otlp", "--signal", "metrics",
    ]);
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["resourceMetrics"].is_array());
    let o = env.run(&[
        "runs", "export", "last", "-f", "otlp", "--signal", "all", "-o", "json",
    ]);
    assert_eq!(o.status.code(), Some(2), "all needs --out DIR");
    let dir = env.path("otlp");
    let o = env.run(&[
        "runs",
        "export",
        "last",
        "-f",
        "otlp",
        "--signal",
        "all",
        "--out",
        dir.to_str().unwrap(),
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", err(&o));
    let files = json(&o)["files"].as_array().unwrap().clone();
    assert_eq!(files.len(), 2);
    for (f, key) in files.iter().zip(["resourceSpans", "resourceMetrics"]) {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(f.as_str().unwrap()).unwrap()).unwrap();
        assert!(v[key].is_array(), "{f}");
    }
    // stdout export
    let o = env.run(&["runs", "export", "last", "-f", "csv", "--table", "ops"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("op_id,verb,"));
    // --endpoint only with otlp; an unreachable collector is a classified network error.
    let o = env.run(&[
        "runs",
        "export",
        "last",
        "-f",
        "har",
        "--endpoint",
        "http://127.0.0.1:9",
        "-o",
        "json",
    ]);
    assert_eq!(o.status.code(), Some(2));
    let o = env.run(&[
        "runs",
        "export",
        "last",
        "-f",
        "otlp",
        "--endpoint",
        "http://127.0.0.1:9",
        "-o",
        "json",
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
}

#[test]
fn repeat_baseline_and_compare() {
    let env = Env::new();
    let o = mem_run(
        &env,
        &["--repeat", "3", "--label", "grp", "-q", "-o", "json"],
    );
    assert!(o.status.success(), "{}", err(&o));
    let v = json(&o);
    assert_eq!(v["group"], "grp");
    assert_eq!(v["runs"].as_array().unwrap().len(), 3);
    assert_eq!(v["stats"]["runs_count"], 3);

    // baseline set / list / show
    let o = env.run(&["baseline", "set", "main", "all:grp", "-o", "json"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(json(&o)["runs"].as_array().unwrap().len(), 3);
    let o = env.run(&["baseline", "list", "-o", "json"]);
    assert_eq!(json(&o)["baselines"][0]["name"], "main");
    let o = env.run(&["baseline", "show", "main"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("median of 3 runs"));

    // run --baseline: compares against the baseline's runs and joins it when it passes
    // (in-memory throughput is noisy, so either outcome is accepted here).
    let o = mem_run(&env, &["--baseline", "main", "-q", "-o", "json"]);
    let code = o.status.code().unwrap();
    assert!(code == 0 || code == 3, "{}", err(&o));
    let v = json(&o);
    assert_eq!(
        v["comparison"]["baseline"]["run_ids"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(v["comparison"]["stats"][0]["method"], "runs_t");
    assert_eq!(v["added_to_baseline"], code == 0);
    assert_eq!(v["exit_code"], code);

    // compare --baseline NAME with markdown + junit; repeat groups on either side.
    let md = env.path("gate.md");
    let junit = env.path("gate.xml");
    let o = env.run(&[
        "compare",
        "--baseline",
        "main",
        "last",
        "--markdown",
        md.to_str().unwrap(),
        "--junit",
        junit.to_str().unwrap(),
        "--max-p99-increase",
        "1000",
        "--max-p50-increase",
        "1000",
        "--max-throughput-drop",
        "90",
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        std::fs::read_to_string(&md)
            .unwrap()
            .starts_with("<!-- oprobe-compare -->")
    );
    assert!(
        std::fs::read_to_string(&junit)
            .unwrap()
            .contains("<testsuite")
    );
    let o = env.run(&["compare", "all:grp", "last", "-o", "json"]);
    assert!(
        o.status.code().is_some_and(|c| c == 0 || c == 3),
        "{}",
        err(&o)
    );
    assert_eq!(json(&o)["baseline"]["run_ids"].as_array().unwrap().len(), 3);

    // A workload change is not comparable: no verdict (exit 0 with a warning, 2 with --strict).
    let o = env
        .cmd()
        .args([
            "run",
            "memory://",
            "--preset",
            "metadata",
            "-d",
            "300ms",
            "--warmup",
            "0s",
            "-q",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let o = env.run(&["compare", "last~1", "last", "-o", "json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let v = json(&o);
    assert_eq!(v["passed"], false);
    assert_eq!(v["comparable"], false);
    assert_eq!(v["verdict"], "not_comparable");
    assert!(
        v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("not comparable")),
        "{v}"
    );
    let o = env.run(&["compare", "last~1", "last", "--strict", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));

    let o = env.run(&["baseline", "rm", "main", "-o", "json"]);
    assert!(o.status.success());
    let o = env.run(&["compare", "--baseline", "main", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn a_slower_server_is_a_significant_regression() {
    let env = Env::new();
    let (_rt1, fast) = background_lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(2),
        ..Default::default()
    });
    let (_rt2, slow) = background_lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(25),
        ..Default::default()
    });
    for ep in [&fast.endpoint_url, &slow.endpoint_url] {
        let o = env
            .cmd()
            .arg("run")
            .args(lab_args(ep))
            .args(["-c", "4", "-d", "1500ms", "-q"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", err(&o));
    }
    let o = env.run(&[
        "compare",
        "last~1",
        "last",
        "--allow-target-change",
        "-o",
        "json",
    ]);
    assert_eq!(
        o.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let v = json(&o);
    let p50 = v["stats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["metric"] == "p50_ms")
        .unwrap();
    assert_eq!(p50["significant"], true);
    assert!(p50["change"]["estimate"].as_f64().unwrap() > 1.0, "{p50}");
}

#[test]
fn matrix_ranks_cells_and_watch_evaluates_slos() {
    let env = Env::new();
    let o = env
        .cmd()
        .arg("matrix")
        .args(MEM)
        .args([
            "--vary",
            "concurrency=1,2",
            "--rank-by",
            "throughput",
            "-q",
            "-o",
            "json",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v = json(&o);
    let cells = v["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0]["rank"], 1);
    assert!(cells[0]["ops_per_s"].as_f64() >= cells[1]["ops_per_s"].as_f64());
    for c in cells {
        let id = c["run_id"].as_str().unwrap();
        assert!(
            env.run(&["runs", "show", id, "-o", "json"])
                .status
                .success()
        );
    }
    let o = env.run(&["matrix", "memory://", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2), "a single cell is not a matrix");

    let o = env
        .cmd()
        .args([
            "watch",
            "memory://",
            "--preset",
            "small-files",
            "--objects",
            "4",
        ])
        .args([
            "--object-size",
            "4KiB",
            "-d",
            "300ms",
            "--rate",
            "50",
            "--interval",
            "300ms",
        ])
        .args([
            "--count",
            "2",
            "--slo",
            "p99<5s",
            "--slo",
            "errors<1%",
            "-o",
            "json",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v = json(&o);
    assert_eq!(v["windows_count"], 2);
    assert_eq!(v["slos"].as_array().unwrap().len(), 2);
    assert!(
        v["slos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["ok"] == true)
    );
    let o = env.run(&["watch", "memory://", "--slo", "cpu<1", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));
}
