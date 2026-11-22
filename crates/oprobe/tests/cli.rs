//! The 6.0 CLI contract (design v6 §8): four commands and the bare app, JSON output validated
//! against the checked-in schemas, classified errors and exit codes, safety rails, the run
//! store and the operator argument. Every invocation uses its own config and data directory.
//!
//! Regenerate `schemas/` with `OPROBE_BLESS=1 cargo test --test cli`.

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

    fn data(&self) -> PathBuf {
        self.path("data")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
        c.env("OPROBE_DATA_DIR", self.data())
            .env("OPROBE_CONFIG", self.path("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["--color", "never"]);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn runs(&self) -> Vec<PathBuf> {
        match std::fs::read_dir(self.data().join("runs")) {
            Ok(rd) => rd
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "oprobe"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// A short, small workload.
const SMALL: &[&str] = &[
    "--preset",
    "small-files",
    "--objects",
    "4",
    "--object-size",
    "16KiB",
    "-c",
    "2",
    "-d",
    "300ms",
    "--warmup",
    "0s",
];

fn run_args<'a>(operators: &[&'a str], extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["run"];
    v.extend_from_slice(operators);
    v.extend_from_slice(SMALL);
    v.extend_from_slice(extra);
    v
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("not one JSON document ({e}): {}\n{}", stdout(o), stderr(o)))
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn error_code(o: &Output) -> String {
    json(o)["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn bless() -> bool {
    std::env::var_os("OPROBE_BLESS").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Validate `v` against the registered schema `name` (`sub`: a JSON pointer into `v`).
fn validate(name: &str, v: &Value) {
    let schema = oprobe::cli::schema::schema(name).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{name}: {errors:#?}\n{v:#}");
}

#[test]
fn checked_in_schemas_match_the_code() {
    let dir = root().join("schemas");
    let mut stale = Vec::new();
    let names = oprobe::cli::schema::names();
    assert_eq!(
        names,
        ["report", "prove", "run-list", "operator", "error", "events"]
    );
    for name in &names {
        let v = oprobe::cli::schema::schema(name).unwrap();
        assert_eq!(
            v["$id"],
            format!("https://github.com/hxddh/oprobe/schemas/6/{name}.json")
        );
        let want = serde_json::to_string_pretty(&v).unwrap() + "\n";
        let path = dir.join(format!("{name}.json"));
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        if have.replace("\r\n", "\n") != want {
            if bless() {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, want).unwrap();
            } else {
                stale.push(path.display().to_string());
            }
        }
    }
    // Nothing else in schemas/.
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
        if !names.contains(&stem.as_str()) {
            if bless() {
                std::fs::remove_file(&p).unwrap();
            } else {
                stale.push(p.display().to_string());
            }
        }
    }
    assert!(
        stale.is_empty(),
        "schemas/ is out of date: {stale:?}\nregenerate with `OPROBE_BLESS=1 cargo test --test cli`"
    );
}

#[test]
fn run_saves_the_run_and_prints_one_json_document() {
    let env = Env::new();
    let o = env.run(&run_args(&["memory://"], &["-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    let run = &v["runs"][0];
    let report = &run["report"];
    validate("report", report);
    assert_eq!(report["schema"], "oprobe.report/6");
    assert_eq!(report["spec"]["operator_ref"], "memory://");
    assert_eq!(report["spec"]["profile"]["operator"]["url"], "memory://");
    assert_eq!(report["workload"]["preset"], "small-files");
    assert!(v["next"].is_array() && v["warnings"].is_array());
    assert_eq!(env.runs().len(), 1);
    // The run file is oprobe.run/6 and opens with `show`.
    let id = run["run_id"].as_str().unwrap();
    let o = env.run(&["show", id, "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let s = json(&o);
    validate("report", &s["report"]);
    assert_eq!(s["report"]["run"]["id"], id);
    assert!(s["requests_count"].as_u64().unwrap() > 0);
    // Text output is the brief.
    let o = env.run(&run_args(&["memory://"], &[]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let t = stdout(&o);
    assert!(t.contains("ops/s") && t.contains("oprobe show"), "{t}");
}

#[test]
fn several_operators_run_one_after_another_on_the_same_workload() {
    let env = Env::new();
    let o = env.run(&run_args(&["memory://", "lab://clean"], &["-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["report"]["spec"]["operator_ref"], "memory://");
    assert_eq!(runs[1]["report"]["spec"]["operator_ref"], "lab://clean");
    assert_eq!(
        runs[0]["report"]["spec"]["seed"],
        runs[1]["report"]["spec"]["seed"]
    );
    assert_eq!(env.runs().len(), 2);
    let t = stdout(&env.run(&run_args(&["memory://", "memory://"], &[])));
    assert!(t.contains("operator") && t.contains("throttled"), "{t}");
}

#[test]
fn operators_come_from_urls_files_and_the_config() {
    let env = Env::new();
    let op = env.path("op.toml");
    std::fs::write(
        &op,
        "url = \"memory://\"\n[layers]\nconcurrent_limit = { http = 4 }\ntail_cut = true\n[read]\nconcurrent = 2\n",
    )
    .unwrap();
    let o = env.run(&run_args(
        &[op.to_str().unwrap()],
        &["--set", "layers.retry.max_times=5", "-o", "json"],
    ));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let spec = &json(&o)["runs"][0]["report"]["spec"]["profile"]["operator"];
    validate("operator", spec);
    assert_eq!(spec["layers"]["concurrent_limit"]["http"], 4);
    assert_eq!(spec["layers"]["retry"]["max_times"], 5);
    assert_eq!(spec["read"]["concurrent"], 2);
    std::fs::write(
        env.path("config.toml"),
        "[operators.mem]\nurl = \"memory://\"\n[operators.mem.layers]\ncache = { memory = \"8MiB\" }\n",
    )
    .unwrap();
    let o = env.run(&run_args(&["mem"], &["-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let r = &json(&o)["runs"][0]["report"];
    assert_eq!(r["spec"]["operator_ref"], "mem");
    assert_eq!(
        r["spec"]["profile"]["operator"]["layers"]["cache"]["memory"],
        "8MiB"
    );
    // An unknown name, a mistyped path, a missing file.
    let o = env.run(&run_args(&["men"], &["-o", "json"]));
    assert_eq!(code(&o), 2);
    assert_eq!(error_code(&o), "usage.unknown_target");
    assert!(json(&o)["error"]["hint"].as_str().unwrap().contains("mem"));
    let o = env.run(&run_args(
        &["memory://"],
        &["--set", "layers.retry.max_tims=5", "-o", "json"],
    ));
    assert_eq!(code(&o), 2);
    assert!(
        json(&o)["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("layers.retry.max_times")
    );
    let o = env.run(&run_args(&["nope.toml"], &["-o", "json"]));
    assert_eq!(code(&o), 2, "{}", stdout(&o));
}

#[test]
fn show_lists_groups_and_removes_runs() {
    let env = Env::new();
    for _ in 0..2 {
        let o = env.run(&run_args(&["memory://"], &["-q"]));
        assert_eq!(code(&o), 0, "{}", stderr(&o));
    }
    let o = env.run(&["show", "--list", "-o", "json"]);
    assert_eq!(code(&o), 0);
    let v = json(&o);
    validate("run-list", &v);
    assert_eq!(v["total_count"], 2);
    let first = v["runs"][0]["id"].as_str().unwrap().to_owned();
    for by in ["stage", "api", "conn"] {
        let o = env.run(&["show", "last", "--by", by, "-o", "json"]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        let b = &json(&o)["by"];
        assert_eq!(b["by"], by);
        assert!(!b["groups"].as_array().unwrap().is_empty(), "{b}");
    }
    let t = stdout(&env.run(&["show", "last", "--by", "stage"]));
    assert!(t.contains("ttfb") && t.contains("p99"), "{t}");
    let detail = stdout(&env.run(&["show", "last", "--detail"]));
    assert!(detail.len() > stdout(&env.run(&["show", "last"])).len());
    let o = env.run(&["show", "--rm", &first, "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(json(&o)["removed"][0], first.as_str());
    assert_eq!(env.runs().len(), 1);
    let o = env.run(&["show", "no-such-run", "-o", "json"]);
    assert_eq!(code(&o), 2);
    assert_eq!(error_code(&o), "usage.unknown_run");
}

#[test]
fn errors_are_classified_with_exit_codes() {
    let env = Env::new();
    let o = env.run(&["run", "ftp://x", "-o", "json"]);
    assert_eq!(code(&o), 2);
    validate("error", &json(&o));
    // clap errors follow the output mode.
    let o = env.run(&["run", "-o", "json"]);
    assert_eq!(code(&o), 2);
    assert_eq!(error_code(&o), "usage.invalid_argument");
    let o = env.run(&["run"]);
    assert_eq!(code(&o), 2);
    assert!(o.stdout.is_empty() && stderr(&o).contains("OPERATOR"));
    let o = env.run(&["run", "memory://", "--preset", "nope", "-o", "json"]);
    assert_eq!(code(&o), 2);
    assert!(
        json(&o)["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("small-files")
    );
    // Text errors: message, hint and code on stderr.
    let o = env.run(&["show", "nothing"]);
    assert_eq!(code(&o), 2);
    let e = stderr(&o);
    assert!(
        e.contains("error:") && e.contains("(usage.unknown_run)"),
        "{e}"
    );
}

#[test]
fn safety_rails_refuse_or_stop_runs() {
    let env = Env::new();
    // --read-only: a generated dataset would be written (exit 5).
    let o = env.run(&run_args(&["memory://"], &["--read-only", "-o", "json"]));
    assert_eq!(code(&o), 5, "{}", stdout(&o));
    assert_eq!(error_code(&o), "safety.read_only");
    // A request budget stops the run (exit 4) and keeps the partial run.
    let o = env.run(&[
        "run",
        "memory://",
        "-d",
        "5s",
        "--warmup",
        "0s",
        "--max-requests",
        "50",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 4, "{}", stdout(&o));
    assert_eq!(error_code(&o), "safety.budget_exceeded");
    // --dry-run: the plan of every operator; nothing is written.
    let env = Env::new();
    let o = env.run(&run_args(
        &["memory://", "lab://clean"],
        &["--dry-run", "-o", "json"],
    ));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    assert_eq!(v["plans"].as_array().unwrap().len(), 2);
    assert!(v["plans"][0]["estimated_requests_count"].as_u64().unwrap() > 0);
    assert!(env.runs().is_empty());
}

#[test]
fn demo_is_run_lab_demo() {
    let env = Env::new();
    let o = env.run(&["demo", "-d", "1s", "--warmup", "0s", "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let r = &json(&o)["runs"][0]["report"];
    assert_eq!(r["spec"]["operator_ref"], "lab://demo");
    assert!(
        r["resilience"]["throttled_responses_count"]
            .as_u64()
            .unwrap()
            > 0,
        "{r}"
    );
}

#[test]
fn prove_judges_a_change_and_renders_the_diff() {
    let env = Env::new();
    let o = env.run(&[
        "run",
        "lab://demo",
        "-d",
        "1s",
        "--warmup",
        "0s",
        "--preset",
        "small-files",
        "-q",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = env.run(&[
        "prove",
        "last",
        "--with",
        "layers.tail_cut=on",
        "-o",
        "json",
        "-q",
    ]);
    assert!(matches!(code(&o), 0 | 3), "{}\n{}", stdout(&o), stderr(&o));
    let v = json(&o);
    validate("prove", &v);
    assert!(["improved", "worse", "inconclusive"].contains(&v["verdict"].as_str().unwrap()));
    assert_eq!(v["change"]["operator"]["layers.tail_cut"], "on");
    assert!(
        v["diff"]["rust"]
            .as_str()
            .unwrap()
            .contains("+ TailCutLayer::new()"),
        "{v}"
    );
    assert!(
        v["diff"]["toml"].as_str().unwrap().contains("tail_cut"),
        "{v}"
    );
    assert!(v["pairs_count"].as_u64().unwrap() >= 2);
    // A dry run plans the replays and sends nothing.
    let o = env.run(&[
        "prove",
        "last",
        "--with",
        "read.gap=1MiB",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 2, "prove has no --dry-run flag: {}", stdout(&o));
}

#[test]
fn bare_oprobe_is_the_app() {
    let env = Env::new();
    let o = env.run(&[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("app"), "{}", stdout(&o));
    let o = env.run(&["-o", "json"]);
    assert_eq!(code(&o), 2);
    validate("error", &json(&o));
}

#[test]
fn help_names_the_four_commands() {
    let env = Env::new();
    let h = stdout(&env.run(&["--help"]));
    for c in ["run", "prove", "show", "lab", "demo"] {
        assert!(h.contains(&format!("  {c}")), "{c}: {h}");
    }
    for gone in ["mcp", "tune", "compare", "doctor", "target", "watch"] {
        assert!(!h.contains(&format!("\n  {gone} ")), "{gone}: {h}");
    }
}

#[test]
fn a_closed_stdout_ends_quietly() {
    let env = Env::new();
    let _ = env.run(&run_args(&["memory://"], &["-q"]));
    let mut child = env
        .cmd()
        .args(["show", "last", "--detail"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
}
