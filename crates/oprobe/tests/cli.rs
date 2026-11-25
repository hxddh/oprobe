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
        [
            "report",
            "run",
            "plan",
            "prove",
            "prove-plan",
            "show",
            "run-list",
            "removed",
            "lab",
            "operator",
            "error",
            "events"
        ]
    );
    for name in &names {
        let v = oprobe::cli::schema::schema(name).unwrap();
        assert_eq!(
            v["$id"],
            format!("https://github.com/hxddh/oprobe/schemas/9/{name}.json")
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
    validate("run", &v);
    let run = &v["runs"][0];
    let report = &run["report"];
    validate("report", report);
    assert_eq!(report["schema"], "oprobe.report/7");
    assert_eq!(report["spec"]["operator_ref"], "memory://");
    assert_eq!(report["spec"]["profile"]["operator"]["url"], "memory://");
    assert_eq!(report["workload"]["preset"], "small-files");
    assert!(v["next"].is_array() && v["warnings"].is_array());
    assert_eq!(env.runs().len(), 1);
    // The run file is oprobe.run/7 and opens with `show`.
    let id = run["run_id"].as_str().unwrap();
    let o = env.run(&["show", id, "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let s = json(&o);
    validate("show", &s);
    validate("report", &s["report"]);
    assert_eq!(s["report"]["run"]["id"], id);
    assert!(s["requests_count"].as_u64().unwrap() > 0);
    // Every finding names a 6.0 code and its next command is a real 6.0 command.
    for f in report["findings"].as_array().unwrap() {
        let code = f["code"].as_str().unwrap();
        assert!(oprobe::analysis::findings::CODES.contains(&code), "{code}");
        if let Some(n) = f["remedy"]["next"].as_str() {
            assert!(
                n.starts_with("oprobe prove ") || n.starts_with("oprobe show "),
                "{n}"
            );
        }
    }
    for n in v["next"].as_array().unwrap() {
        let cli = n["cli"].as_str().unwrap();
        let cmd = cli.split_whitespace().nth(1).unwrap();
        assert!(["run", "prove", "show", "lab"].contains(&cmd), "{cli}");
    }
    // Text output is the brief (not JSON), naming the run.
    let o = env.run(&run_args(&["memory://"], &[]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let t = stdout(&o);
    assert!(serde_json::from_str::<Value>(&t).is_err());
    assert!(t.lines().count() <= 30, "the brief is one screen: {t}");
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
    // One comparison: a row per operator, and how each differs from the first.
    let cmp = &v["comparison"];
    let rows = cmp["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["operator"], "lab://clean");
    for k in [
        "goodput_ops_per_s",
        "p50_ms",
        "p99_ms",
        "throttled_request_ratio",
        "failed_ops_count",
    ] {
        assert!(rows[0][k].is_number(), "{k}: {cmp}");
    }
    let d = cmp["differences"][0].as_str().unwrap();
    assert!(d.contains("service"), "{d}");
    // The same service with a layer changed differs in layers.
    let op = env.path("cap.toml");
    std::fs::write(
        &op,
        "url = \"memory://\"\n[layers]\nconcurrent_limit = { http = 2 }\n",
    )
    .unwrap();
    let o = env.run(&run_args(
        &["memory://", op.to_str().unwrap()],
        &["-o", "json"],
    ));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let d = json(&o)["comparison"]["differences"][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(d.contains("layers") && !d.contains("service"), "{d}");
    // One run alone has no comparison.
    let o = env.run(&run_args(&["memory://"], &["-o", "json"]));
    assert!(json(&o)["comparison"].is_null());
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
        let shown = json(&o);
        validate("show", &shown);
        let b = &shown["by"];
        assert_eq!(b["by"], by);
        assert!(!b["groups"].as_array().unwrap().is_empty(), "{b}");
    }
    let detail = stdout(&env.run(&["show", "last", "--detail"]));
    assert!(detail.len() > stdout(&env.run(&["show", "last"])).len());
    // The text names the run's file, to hand a teammate.
    let file = detail
        .lines()
        .find_map(|l| l.strip_prefix("file: "))
        .expect("a file: line");
    assert!(
        file.ends_with(".oprobe") && std::path::Path::new(file).is_file(),
        "{file}"
    );
    let o = env.run(&["show", "--rm", &first, "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let rem = json(&o);
    validate("removed", &rem);
    assert_eq!(rem["removed"][0], first.as_str());
    assert_eq!(env.runs().len(), 1);
    // `-n` without `--list` is refused by clap.
    let o = env.run(&["show", "-n", "5", "-o", "json"]);
    assert_eq!(code(&o), 2);
    assert_eq!(error_code(&o), "usage.invalid_argument");
    let o = env.run(&["show", "no-such-run", "-o", "json"]);
    assert_eq!(code(&o), 2);
    assert_eq!(error_code(&o), "usage.unknown_run");
}

#[test]
fn show_recomputes_a_time_range_from_the_rows() {
    let env = Env::new();
    let o = env.run(&run_args(&["memory://"], &["-q"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let whole = json(&env.run(&["show", "last", "--since", "0s", "-o", "json"]));
    let all = whole["range"]["ops_count"].as_u64().unwrap();
    assert!(all > 0, "{}", whole["range"]);
    let o = env.run(&[
        "show", "last", "--since", "100ms", "--until", "200ms", "-o", "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    let r = &v["range"];
    assert_eq!(r["since_s"], 0.1);
    assert_eq!(r["until_s"], 0.2);
    let part = r["ops_count"].as_u64().unwrap();
    assert!(part > 0 && part < all, "{part} of {all}");
    assert!(r["p99_ms"].as_f64().unwrap() > 0.0);
    assert!(!r["apis"].as_array().unwrap().is_empty());
    // The whole report is still there; without the flags there is no range.
    validate("show", &v);
    validate("report", &v["report"]);
    assert!(
        json(&env.run(&["show", "last", "-o", "json"]))
            .get("range")
            .is_none()
    );
    let text = stdout(&env.run(&[
        "show", "last", "--since", "100ms", "--until", "200ms", "--color", "never",
    ]));
    assert!(text.contains("0.1–0.2 s of the"), "{text}");
    assert!(text.contains("Requests by API"), "{text}");
    // `--by` groups only the range's requests.
    let by = json(&env.run(&[
        "show", "last", "--since", "100ms", "--until", "200ms", "--by", "api", "-o", "json",
    ]));
    let n: u64 = by["by"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["requests_count"].as_u64().unwrap())
        .sum();
    assert_eq!(n, r["requests_count"].as_u64().unwrap());
    let by_text = stdout(&env.run(&[
        "show", "last", "--since", "100ms", "--until", "200ms", "--by", "api", "--color", "never",
    ]));
    assert!(by_text.contains("0.1–0.2 s of the"), "{by_text}");
    // A backwards range is a usage error.
    let o = env.run(&[
        "show", "last", "--since", "2s", "--until", "1s", "-o", "json",
    ]);
    assert_eq!(code(&o), 2);
    assert_eq!(error_code(&o), "usage.invalid_argument");
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
    // It stops at the limit, not one past it.
    let msg = json(&o)["error"]["message"].as_str().unwrap().to_owned();
    assert!(
        msg.contains("(limit 50)") && !msg.contains("51 requests"),
        "{msg}"
    );
    // The partial run is Review (not Critical), leads with why it cannot be judged, and
    // prove refuses it.
    let o = env.run(&["show", "last", "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let r = &json(&o)["report"];
    assert_eq!(r["verdict"]["level"], "review", "{}", r["verdict"]);
    assert_eq!(r["findings"][0]["code"], "invalid_run", "{}", r["findings"]);
    let o = env.run(&["prove", "last", "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    assert_eq!(error_code(&o), "usage.invalid_argument");
    let e = json(&o);
    validate("error", &e);
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot be judged"),
        "{e}"
    );
    // --dry-run: the plan of every operator; nothing is written.
    let env = Env::new();
    let o = env.run(&run_args(
        &["memory://", "lab://clean"],
        &["--dry-run", "-o", "json"],
    ));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    validate("plan", &v);
    assert_eq!(v["plans"].as_array().unwrap().len(), 2);
    assert!(v["plans"][0]["estimated_requests_count"].as_u64().unwrap() > 0);
    assert!(env.runs().is_empty());
    // The plan states its budgets and warns when it would spend one (6.3.0 planned 6 541
    // requests under --max-requests 500 without a word).
    let o = env.run(&run_args(
        &["memory://"],
        &["--dry-run", "--max-requests", "5", "-o", "json"],
    ));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    validate("plan", &v);
    assert_eq!(v["plans"][0]["budgets"]["max_requests_count"], 5);
    let warnings = v["warnings"].to_string();
    assert!(warnings.contains("--max-requests"), "{v}");
    let t = stdout(&env.run(&run_args(
        &["memory://"],
        &["--dry-run", "--max-requests", "5"],
    )));
    assert!(t.contains("budgets ≤ 5 requests"), "{t}");
    assert!(
        t.contains("warning:") && t.contains("request budget"),
        "{t}"
    );
    assert!(env.runs().is_empty());
}

#[test]
fn demo_is_run_lab_demo() {
    let env = Env::new();
    let o = env.run(&["demo", "-d", "1s", "--warmup", "0s", "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let r = &json(&o)["runs"][0]["report"];
    assert_eq!(r["spec"]["operator_ref"], "lab://demo");
    // The demo lab's stragglers wait 250 ms for their first byte.
    let slowest_ttfb = r["apis"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["stages"]["ttfb"]["max_ms"].as_f64())
        .fold(0.0, f64::max);
    assert!(slowest_ttfb >= 250.0, "{r}");
}

#[test]
fn prove_judges_a_change_and_hides_its_trials() {
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
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let run_id = json(&o)["runs"][0]["run_id"].as_str().unwrap().to_owned();
    let o = env.run(&[
        "prove",
        "last",
        "--with",
        "read.gap=1MiB",
        "--trial",
        "300ms",
        "--pairs",
        "3",
        "-o",
        "json",
        "-q",
    ]);
    assert!(matches!(code(&o), 0 | 3), "{}\n{}", stdout(&o), stderr(&o));
    let v = json(&o);
    validate("prove", &v);
    assert!(["improved", "worse", "inconclusive"].contains(&v["verdict"].as_str().unwrap()));
    assert_eq!(v["run_id"], run_id.as_str());
    assert_eq!(v["change"][0], "read.gap=1MiB");
    assert!(v["diff"]["toml"].as_str().unwrap().contains("gap"), "{v}");
    assert_eq!(v["pairs_count"], 3);
    let d = &v["objective_delta"];
    assert!(d["low"].as_f64().unwrap() > -1.0, "never below -100 %: {d}");
    assert!(d["low"].as_f64().unwrap() <= d["high"].as_f64().unwrap());
    let trials = v["trials"].as_array().unwrap();
    assert_eq!(trials.len(), 6);
    // The trials are children of the prove: hidden from the list, and `last` is still the
    // run the user made (U4).
    let o = env.run(&["show", "--list", "-o", "json"]);
    let list = json(&o);
    validate("run-list", &list);
    assert_eq!(list["total_count"], 1);
    assert_eq!(list["hidden_trials_count"], 6);
    assert_eq!(list["proves"][0]["prove_id"], v["prove_id"]);
    let all = json(&env.run(&["show", "--list", "--all", "-o", "json"]));
    assert_eq!(all["total_count"], 7);
    let trial = json(&env.run(&["show", trials[0]["run_id"].as_str().unwrap(), "-o", "json"]));
    assert_eq!(trial["report"]["run"]["parent_id"], v["prove_id"]);
    let last = json(&env.run(&["show", "last", "-o", "json"]));
    assert_eq!(last["report"]["run"]["id"], run_id.as_str(), "{last:#}");
    // Removing the prove removes its trials.
    let o = env.run(&[
        "show",
        "--rm",
        v["prove_id"].as_str().unwrap(),
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(env.runs().len(), 1);
    // A finding the run does not have; a dry run plans and sends nothing.
    let o = env.run(&["prove", "last", "hung_requests", "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    assert_eq!(error_code(&o), "usage.invalid_argument");
    let o = env.run(&[
        "prove",
        "last",
        "--with",
        "read.gap=1MiB",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        json(&o)["plan"]["estimated_requests_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(env.runs().len(), 1);
}

/// A budget breach in the middle of a prove (`--max-requests`) stops the trials, keeps exit 4
/// and prints and saves the partial result, so the trials stay attached to the prove.
#[test]
fn a_budget_breach_keeps_the_partial_prove() {
    let env = Env::new();
    let o = env.run(&run_args(&["lab://clean"], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let prove = |max_requests: &str| {
        env.run(&[
            "prove",
            "last",
            "--with",
            "read.gap=1MiB",
            "--trial",
            "300ms",
            "--pairs",
            "3",
            "--max-requests",
            max_requests,
            "-o",
            "json",
        ])
    };
    // How many requests a trial makes, then a budget that breaks in the third trial.
    let o = prove("100000000");
    assert!(matches!(code(&o), 0 | 3), "{}", stderr(&o));
    let full = json(&o);
    let per_trial = full["trials"][0]["requests_count"].as_u64().unwrap();
    let o = env.run(&["show", "--rm", full["prove_id"].as_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    for max in [per_trial * 5 / 2, 10] {
        let o = prove(&max.to_string());
        assert_eq!(code(&o), 4, "{}\n{}", stdout(&o), stderr(&o));
        let v = json(&o);
        validate("prove", &v);
        assert_eq!(v["budget_exceeded"], true, "{v}");
        assert!(v["pairs_count"].as_u64().unwrap() <= 1, "{v}");
        assert_eq!(v["verdict"], "inconclusive", "{v}");
        assert!(
            v["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("a budget stopped the prove")),
            "{v}"
        );
        let id = v["prove_id"].as_str().unwrap();
        assert!(
            env.data()
                .join("runs")
                .join(format!("{id}.prove.json"))
                .is_file(),
            "the partial prove is saved"
        );
        assert!(env.runs().len() > 1, "the trial runs are saved");
        // No trial is orphaned: removing the prove removes them all.
        let o = env.run(&["show", "--rm", id]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        assert_eq!(env.runs().len(), 1, "max {max}");
    }
}

/// A budget that allows fewer than three A/B pairs cannot decide: the verdict is
/// `inconclusive` (exit 0), with a note, however large the measured difference.
#[test]
fn prove_needs_three_pairs_to_decide() {
    let env = Env::new();
    let o = env.run(&run_args(&["lab://clean"], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // Whatever one pair measures, it cannot decide.
    let o = env.run(&[
        "prove",
        "last",
        "--with",
        "layers.retry.min_delay=10ms",
        "--trial",
        "300ms",
        "--max-duration",
        "1s",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}\n{}", stdout(&o), stderr(&o));
    let v = json(&o);
    validate("prove", &v);
    assert_eq!(v["pairs_count"], 1, "{v}");
    assert_eq!(v["verdict"], "inconclusive", "{v}");
    assert!(
        v["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("allowed only 1 pair")),
        "{v}"
    );
    // `--pairs` below three is refused.
    let o = env.run(&["prove", "last", "--with", "read.gap=1MiB", "--pairs", "2"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
}

/// `--with` (and `--set`) pairs apply as one change, in any order: a layer switched on with its
/// field, as the app sends its edits.
#[test]
fn with_and_set_pairs_apply_in_any_order() {
    let env = Env::new();
    let o = env.run(&run_args(&["memory://"], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    for with in [
        [
            "layers.concurrent_limit=on",
            "layers.concurrent_limit.http=24",
        ],
        [
            "layers.concurrent_limit.http=24",
            "layers.concurrent_limit=on",
        ],
    ] {
        let o = env.run(&[
            "prove",
            "last",
            "--with",
            with[0],
            "--with",
            with[1],
            "--dry-run",
            "-o",
            "json",
        ]);
        assert_eq!(code(&o), 0, "{with:?}: {}\n{}", stdout(&o), stderr(&o));
        let o = env.run(&run_args(
            &["memory://"],
            &[
                "--set",
                with[0],
                "--set",
                with[1],
                "--dry-run",
                "-o",
                "json",
            ],
        ));
        assert_eq!(code(&o), 0, "{with:?}: {}\n{}", stdout(&o), stderr(&o));
    }
    // A limit layer without a limit is still refused.
    let o = env.run(&[
        "prove",
        "last",
        "--with",
        "layers.concurrent_limit=on",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    assert!(stdout(&o).contains("needs `http`"), "{}", stdout(&o));
}

/// Every file under `dir`, recursively, as text (`.oprobe` run files decompressed too).
fn files_text(dir: &std::path::Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let raw = std::fs::read(&p).unwrap();
            let mut text = String::from_utf8_lossy(&raw).into_owned();
            // Run files are zstd segments: read every frame that decodes.
            if let Ok(mut dec) = zstd::stream::read::Decoder::new(&raw[..]) {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut dec, &mut buf);
                text.push_str(&String::from_utf8_lossy(&buf));
            }
            out.push((p, text));
        }
    }
    out
}

/// A secret given with `--with` or in `op.toml` never shows in what prove prints (the plan,
/// the diff, the next command) or stores (`<id>.prove.json`, the trial runs).
#[test]
fn prove_never_prints_or_stores_secrets() {
    let env = Env::new();
    let op = env.path("op.toml");
    std::fs::write(
        &op,
        "url = \"lab://clean\"\n[options]\nsecret_access_key = \"SEKRIT2\"\n",
    )
    .unwrap();
    let op = op.to_str().unwrap();
    let mut outputs = Vec::new();
    let o = env.run(&run_args(&[op], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    outputs.push(o);
    let with = [
        "--with",
        "options.secret_access_key=SEKRIT1",
        "--with",
        "read.gap=1MiB",
    ];
    for extra in [
        &["--dry-run"][..],
        &["--dry-run", "-o", "json"],
        &["--trial", "300ms", "--pairs", "3"],
        &["--trial", "300ms", "--pairs", "3", "-o", "json"],
    ] {
        let mut args = vec!["prove", "last"];
        args.extend_from_slice(&with);
        args.extend_from_slice(extra);
        let o = env.run(&args);
        assert!(matches!(code(&o), 0 | 3), "{args:?}: {}", stderr(&o));
        outputs.push(o);
    }
    // The change is still shown, redacted, and the next command reads it from the
    // environment.
    let v = json(outputs.last().unwrap());
    assert!(
        v["change"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "options.secret_access_key=<redacted>"),
        "{v}"
    );
    let next = v["next"].to_string();
    assert!(next.contains("${SECRET_ACCESS_KEY}"), "{next}");
    for o in &outputs {
        for secret in ["SEKRIT1", "SEKRIT2"] {
            assert!(
                !stdout(o).contains(secret),
                "{secret} on stdout: {}",
                stdout(o)
            );
            assert!(
                !stderr(o).contains(secret),
                "{secret} on stderr: {}",
                stderr(o)
            );
        }
    }
    let files = files_text(&env.data());
    assert!(
        files
            .iter()
            .any(|(p, _)| p.to_string_lossy().ends_with(".prove.json"))
    );
    for (p, text) in files {
        for secret in ["SEKRIT1", "SEKRIT2"] {
            assert!(!text.contains(secret), "{secret} in {}", p.display());
        }
    }
}

/// One prove budget (6.3): `--max-duration` bounds the whole proof, `--budget` is a hidden
/// alias, and the dry run estimates the whole proof.
#[test]
fn prove_has_one_budget_and_estimates_the_whole_proof() {
    let env = Env::new();
    let h = stdout(&env.run(&["prove", "--help"]));
    assert!(h.contains("--max-duration"), "{h}");
    assert!(!h.contains("--budget"), "{h}");
    let o = env.run(&run_args(&["lab://clean"], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    for flag in ["--max-duration", "--budget"] {
        let o = env.run(&[
            "prove",
            "last",
            "--with",
            "read.gap=1MiB",
            flag,
            "90s",
            "--dry-run",
            "-o",
            "json",
        ]);
        assert_eq!(code(&o), 0, "{flag}: {}", stderr(&o));
        validate("prove-plan", &json(&o));
        let p = &json(&o)["proof"];
        assert_eq!(p["budget_s"], 90.0, "{flag}: {p}");
        assert_eq!(p["pairs_min_count"], 3, "{p}");
        assert_eq!(p["trials_max_count"], 12, "{p}");
        let (lo, hi) = (
            p["eta_min_s"].as_f64().unwrap(),
            p["eta_max_s"].as_f64().unwrap(),
        );
        assert!(lo > 0.0 && lo <= hi && hi <= 90.0, "{p}");
        // Six trials at least: the estimate is the proof's, not one trial's.
        assert!(lo >= 6.0 * p["trial_eta_s"].as_f64().unwrap() - 1e-6, "{p}");
        assert!(
            p["command"]
                .as_str()
                .unwrap()
                .ends_with("--with read.gap=1MiB --max-duration 90s"),
            "{p}"
        );
    }
    let o = env.run(&["prove", "last", "--with", "read.gap=1MiB", "--dry-run"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let t = stdout(&o);
    assert!(t.contains("6–12 trials") && t.contains("ETA"), "{t}");
    assert!(!t.contains("--warmup"), "{t}");
    assert_eq!(env.runs().len(), 1, "a dry run runs nothing");
}

/// 5.x vestiges are gone from the surface (6.3): `--set sla…` is refused, the run list has no
/// label column and names verdicts OK / Review / Attention / Critical, and `lab` lists the
/// app's six scenarios.
#[test]
fn five_x_vestiges_are_gone() {
    let env = Env::new();
    for bad in ["sla=none", "p99_ms=50"] {
        let o = env.run(&["run", "memory://", "--set", bad, "-o", "json"]);
        assert_eq!(code(&o), 2, "{bad}: {}", stdout(&o));
        let v = json(&o);
        assert!(
            v["error"]["message"].as_str().unwrap().contains("no SLA"),
            "{v}"
        );
    }
    let o = env.run(&run_args(&["memory://"], &["-q"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let list = stdout(&env.run(&["show", "--list", "--color", "never"]));
    assert!(!list.contains("label"), "{list}");
    assert!(
        ["OK", "Review", "Attention", "Critical"]
            .iter()
            .any(|l| list.contains(l)),
        "{list}"
    );
    let show = stdout(&env.run(&["show", "last", "--color", "never"]));
    assert!(!show.contains("sla"), "{show}");
    let o = env.run(&["lab", "bogus"]);
    assert_eq!(code(&o), 2);
    assert!(
        stderr(&o).contains("demo, throttle, stragglers, bandwidth, flaky, ratelimit"),
        "{}",
        stderr(&o)
    );
    let h = stdout(&env.run(&["lab", "--help"]));
    assert!(!h.contains("hotkey") && h.contains("ratelimit"), "{h}");
}

/// `lab --max-store-size` is hidden from the help (design 9.1 §3), and still taken.
#[test]
fn lab_max_store_size_is_hidden_but_accepted() {
    let env = Env::new();
    let h = stdout(&env.run(&["lab", "--help"]));
    assert!(!h.contains("--max-store-size"), "{h}");
    assert!(h.contains("--allow-remote"), "{h}");
    // A zero size is refused by its parser, not as an unknown flag.
    let o = env.run(&["lab", "--max-store-size", "0"]);
    assert_eq!(code(&o), 2);
    let e = stderr(&o);
    assert!(!e.contains("unexpected argument"), "{e}");
    assert!(e.contains("max-store-size"), "{e}");
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

/// A double click on a run file runs `oprobe PATH` (6.3.0: `unrecognized subcommand`). When the
/// only argument is an existing `.oprobe` or `.json` file, it is the bare app opening it; off a
/// desktop and a terminal, the help says how to read it here.
#[test]
fn a_run_file_alone_opens_the_app() {
    let env = Env::new();
    let o = env.run(&run_args(&["memory://"], &["-q"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let oprobe = env.runs().pop().unwrap();
    let report = env.path("report.JSON");
    std::fs::write(&report, stdout(&env.run(&["show", "last", "-o", "json"]))).unwrap();
    // Exactly as a file manager starts it: the path alone.
    let alone = |path: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", env.data())
            .env("OPROBE_CONFIG", env.path("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .arg(path)
            .output()
            .unwrap()
    };
    for file in [&oprobe, &report] {
        let o = alone(file);
        assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
        let t = stdout(&o);
        assert!(t.contains("the app"), "{t}");
        // The path is quoted for the shell where it needs it (Windows paths always do).
        let name = file.file_name().unwrap().to_string_lossy();
        assert!(
            t.lines()
                .any(|l| l.contains("oprobe show ") && l.contains(name.as_ref())),
            "{t}"
        );
    }
    // Anything else is still a command line: a missing file, another extension, more arguments.
    let toml = env.path("op.toml");
    std::fs::write(&toml, "url = \"memory://\"\n").unwrap();
    for path in [env.path("missing.oprobe"), toml] {
        let o = alone(&path);
        assert_eq!(code(&o), 2, "{}", stdout(&o));
    }
    let o = env.run(&[oprobe.to_str().unwrap()]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
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

/// `run A B` runs one workload for every operator (K2): the first operator's, not each lab's
/// own defaults (7.0: 28 workers on `lab://demo`, 16 on `lab://ratelimit`). The budgets cover
/// what all the operators spend together.
#[test]
fn several_operators_share_one_workload_and_one_budget() {
    let env = Env::new();
    let o = env.run(&[
        "run",
        "lab://demo",
        "lab://ratelimit",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    validate("plan", &v);
    let p = v["plans"].as_array().unwrap();
    assert_eq!(
        p[0]["concurrency_count"], p[1]["concurrency_count"],
        "{v:#}"
    );
    assert_eq!(p[0]["preset"], p[1]["preset"]);
    assert_eq!(p[0]["duration_s"], p[1]["duration_s"]);
    // One operator's plan fits the budget; the two together do not.
    let one = json(&env.run(&run_args(&["memory://"], &["--dry-run", "-o", "json"])));
    let n = one["plans"][0]["estimated_requests_count"]
        .as_u64()
        .unwrap();
    let budget = (n * 3 / 2).to_string();
    let o = env.run(&run_args(
        &["memory://", "memory://"],
        &["--dry-run", "--max-requests", &budget, "-o", "json"],
    ));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    let w = v["warnings"].to_string();
    assert!(
        w.contains("2 operators together") && w.contains("--max-requests"),
        "{v:#}"
    );
}

/// A spent budget prints what it let finish, like Ctrl-C does (K10): the brief on stdout and
/// the error on stderr; under `-o json`, the runs with the error. Still exit 4, and the
/// operators after it do not run.
#[test]
fn a_budget_stop_prints_the_runs_it_let_finish() {
    let env = Env::new();
    let args = [
        "run",
        "memory://",
        "memory://",
        "-d",
        "5s",
        "--warmup",
        "0s",
        "--max-requests",
        "50",
    ];
    let o = env.run(&args);
    assert_eq!(code(&o), 4, "{}", stderr(&o));
    assert!(stdout(&o).contains("ops/s"), "the brief: {}", stdout(&o));
    assert!(
        stderr(&o).contains("error:") && stderr(&o).contains("request budget"),
        "{}",
        stderr(&o)
    );
    let mut a = args.to_vec();
    a.extend(["-o", "json"]);
    let o = env.run(&a);
    assert_eq!(code(&o), 4, "{}", stderr(&o));
    let v = json(&o);
    validate("run", &v);
    assert_eq!(error_code(&o), "safety.budget_exceeded");
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "the second operator does not run: {v:#}");
    assert!(runs[0]["report"]["run"]["partial"].is_object(), "{v:#}");
}

/// `show PROVE_ID` shows the stored result as `prove` printed it (K11), also once its run is
/// removed; `show --list` limits the proves with `-n` and says so (K7).
#[test]
fn show_opens_a_prove_and_lists_proves_within_the_limit() {
    let env = Env::new();
    let o = env.run(&run_args(&["lab://clean"], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let run_id = json(&o)["runs"][0]["run_id"].as_str().unwrap().to_owned();
    let prove = || {
        let o = env.run(&[
            "prove",
            "last",
            "--with",
            "read.gap=1MiB",
            "--trial",
            "300ms",
            "--max-duration",
            "2500ms",
            "-o",
            "json",
        ]);
        assert!(matches!(code(&o), 0 | 3), "{}", stderr(&o));
        json(&o)
    };
    let first = prove();
    let second = prove();
    let id = first["prove_id"].as_str().unwrap();
    // The list: one of each with -n 1, and it says how many proves there are.
    let o = env.run(&["show", "--list", "-n", "1", "-o", "json"]);
    let list = json(&o);
    validate("run-list", &list);
    assert_eq!(list["proves"].as_array().unwrap().len(), 1, "{list:#}");
    assert_eq!(list["proves_total_count"], 2);
    assert_eq!(list["proves"][0]["prove_id"], second["prove_id"]);
    let t = stdout(&env.run(&["show", "--list", "-n", "1"]));
    assert!(t.contains("1 of 2 proves"), "{t}");
    // The prove itself: the prove schema, the same text as `prove`, exit 0.
    let o = env.run(&["show", id, "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let v = json(&o);
    validate("prove", &v);
    assert_eq!(v["prove_id"], first["prove_id"]);
    assert_eq!(v["change"], first["change"]);
    let t = stdout(&env.run(&["show", id]));
    assert!(t.contains(" on goodput"), "{t}");
    // Its run removed: the prove still shows.
    let o = env.run(&["show", "--rm", &run_id]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = env.run(&["show", id, "-o", "json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    validate("prove", &json(&o));
}

/// `show` refuses what it cannot show, naming the right flag: `--by` on a file without
/// request rows (K9), `--since` past the window (K12: 7.0 blamed an `--until` nobody passed).
#[test]
fn show_refuses_a_range_or_grouping_it_cannot_show() {
    let env = Env::new();
    let o = env.run(&run_args(&["memory://"], &["-q"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = env.run(&["show", "last", "--since", "100s", "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    let e = json(&o);
    let msg = e["error"]["message"].as_str().unwrap();
    assert!(msg.contains("--since") && msg.contains("window"), "{e:#}");
    assert!(!msg.contains("--until"), "{e:#}");
    // A report `.json` has no request rows: --by is refused like --since.
    let report = json(&env.run(&["show", "last", "-o", "json"]))["report"].clone();
    let file = env.path("report.json");
    std::fs::write(&file, serde_json::to_vec(&report).unwrap()).unwrap();
    let f = file.to_str().unwrap();
    for by in ["api", "conn", "stage"] {
        let o = env.run(&["show", f, "--by", by, "-o", "json"]);
        assert_eq!(code(&o), 2, "{by}: {}", stdout(&o));
        assert_eq!(error_code(&o), "usage.invalid_argument");
        let e = json(&o);
        assert!(
            e["error"]["message"]
                .as_str()
                .unwrap()
                .contains("request rows"),
            "{e:#}"
        );
    }
}

/// A zero duration is refused with a classified usage error (K13): a 0 s window measures
/// nothing, a 0 s budget stops at once, a 0 s trial proves nothing.
#[test]
fn zero_durations_are_refused() {
    let env = Env::new();
    for args in [
        vec!["run", "memory://", "-d", "0s"],
        vec!["run", "memory://", "-d", "0"],
        vec!["run", "memory://", "--max-duration", "0s"],
        vec!["run", "memory://", "--set", "duration=0s"],
        vec!["prove", "last", "--trial", "0s"],
        vec!["prove", "last", "--max-duration", "0s"],
        vec!["run", "memory://", "-d", "-1s"],
    ] {
        let mut a = args.clone();
        a.extend(["-o", "json"]);
        let o = env.run(&a);
        assert_eq!(code(&o), 2, "{args:?}: {}", stdout(&o));
        assert_eq!(error_code(&o), "usage.invalid_argument", "{args:?}");
        validate("error", &json(&o));
    }
    assert!(env.runs().is_empty());
}

/// Requests that lost their connection (`lab://flaky` resets) are counted in the remotes and
/// `--by conn` tables under a `no connection` row, not dropped or filed under connection 0.
#[test]
fn reset_requests_show_under_no_connection() {
    let env = Env::new();
    let o = env.run(&[
        "run",
        "lab://flaky",
        "-d",
        "1s",
        "--warmup",
        "0s",
        "-q",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let r = json(&o)["runs"][0]["report"].clone();
    let resets = r["resilience"]["network_errors_count"].as_u64().unwrap();
    assert!(resets > 0, "{}", r["resilience"]);
    let remotes = r["network"]["remotes"].as_array().unwrap();
    let none = remotes
        .iter()
        .find(|x| x["remote"] == "no connection")
        .unwrap_or_else(|| panic!("{remotes:#?}"));
    assert_eq!(none["requests_count"], resets);
    let total: u64 = remotes
        .iter()
        .map(|x| x["requests_count"].as_u64().unwrap())
        .sum();
    assert_eq!(total, r["connections"]["requests_count"].as_u64().unwrap());
    let by = json(&env.run(&["show", "last", "--by", "conn", "-o", "json"]));
    validate("show", &by);
    let groups = by["by"]["groups"].as_array().unwrap();
    assert!(!groups.iter().any(|g| g["key"] == "0"), "{groups:#?}");
    let g = groups
        .iter()
        .find(|g| g["key"] == "no connection")
        .unwrap_or_else(|| panic!("{groups:#?}"));
    assert_eq!(g["requests_count"], resets);
}

#[test]
fn show_compares_multiple_runs_and_prove_saves_when_improved() {
    let env = Env::new();
    let o1 = env.run(&run_args(&["lab://clean"], &["-q", "-o", "json"]));
    assert_eq!(code(&o1), 0, "{}", stderr(&o1));
    let id1 = json(&o1)["runs"][0]["run_id"].as_str().unwrap().to_owned();

    let o2 = env.run(&run_args(
        &["lab://clean"],
        &["--set", "layers.retry.jitter=on", "-q", "-o", "json"],
    ));
    assert_eq!(code(&o2), 0, "{}", stderr(&o2));
    let id2 = json(&o2)["runs"][0]["run_id"].as_str().unwrap().to_owned();

    // `show A B -o json` is a `schemas/run.json` document with a side-by-side comparison.
    let cmp = env.run(&["show", &id1, &id2, "-o", "json"]);
    assert_eq!(code(&cmp), 0, "{}", stderr(&cmp));
    let v = json(&cmp);
    validate("run", &v);
    assert_eq!(v["runs"].as_array().unwrap().len(), 2);
    assert!(v["comparison"].is_object(), "{v:#}");

    // `show A B --detail` renders both runs and the per-API comparison table in text.
    let text = stdout(&env.run(&["show", &id1, &id2, "--detail"]));
    assert!(text.contains(&id1) && text.contains(&id2), "{text}");
    assert!(text.contains("GetObject"), "{text}");

    // `prove --save` on an improved remedy writes `op.toml`, and `show last-prove` opens it.
    // Deterministic: one operation at a time against 20 ms of latency, then sixteen (about
    // 16× the goodput, whatever the machine).
    let o_lim = env.run(&[
        "run",
        "lab://lossy",
        "-c",
        "16",
        "-d",
        "800ms",
        "--warmup",
        "0s",
        "--set",
        "layers.concurrent_limit.operations=1",
        "-q",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o_lim), 0, "{}", stderr(&o_lim));
    let save_path = env.path("proven-op.toml");
    let p = env.run(&[
        "prove",
        "last",
        "--with",
        "layers.concurrent_limit.operations=64",
        "--trial",
        "400ms",
        "--pairs",
        "3",
        "--save",
        save_path.to_str().unwrap(),
        "-q",
        "-o",
        "json",
    ]);
    assert_eq!(code(&p), 0, "{}", stderr(&p));
    let pv = json(&p);
    validate("prove", &pv);
    assert_eq!(pv["verdict"], "improved", "{pv:#}");
    let saved = std::fs::read_to_string(&save_path).unwrap();
    assert!(saved.contains("operations = 64"), "{saved}");
    assert_eq!(pv["saved_to"].as_str(), save_path.to_str());
    // W7: the next profile replays the run's workload on the proven operator.
    let next = pv["run_command"].as_str().unwrap();
    assert!(
        next.starts_with("oprobe run lab://lossy")
            && next.contains("-c 16")
            && next.contains("-d 800ms")
            && next.contains("layers.concurrent_limit.operations=64"),
        "{next}"
    );
    let stored: Value = serde_json::from_slice(
        &std::fs::read(env.data().join(format!(
            "runs/{}.prove.json",
            pv["prove_id"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(stored["saved_to"], pv["saved_to"]);
    // Not improved (sixteen workers down to one operation at a time): nothing is written.
    let o_free = env.run(&[
        "run",
        "lab://lossy",
        "-c",
        "16",
        "-d",
        "800ms",
        "--warmup",
        "0s",
        "-q",
        "-o",
        "json",
    ]);
    assert_eq!(code(&o_free), 0, "{}", stderr(&o_free));
    let not_saved = env.path("not-saved-op.toml");
    let p_worse = env.run(&[
        "prove",
        "last",
        "--with",
        "layers.concurrent_limit.operations=1",
        "--trial",
        "400ms",
        "--pairs",
        "3",
        "--save",
        not_saved.to_str().unwrap(),
        "-q",
        "-o",
        "json",
    ]);
    let wv = json(&p_worse);
    validate("prove", &wv);
    assert_eq!(wv["verdict"], "worse", "{wv:#}");
    assert_eq!(code(&p_worse), 3, "{}", stderr(&p_worse));
    assert!(!not_saved.exists(), "a worse change was saved");
    assert!(wv.get("saved_to").is_none(), "{wv:#}");
    assert!(
        wv["notes"]
            .to_string()
            .contains("skipped: verdict is worse"),
        "{wv:#}"
    );
    // D1/D2: a `--save` target that is not this service's operator is refused before any
    // trial runs, and left as it was.
    let foreign = env.path("Cargo.toml");
    std::fs::write(&foreign, "[package]\nname = \"app\"\n").unwrap();
    let proves_before = std::fs::read_dir(env.data().join("runs")).unwrap().count();
    let bad = env.run(&[
        "prove",
        "last",
        "--with",
        "layers.concurrent_limit.operations=8",
        "--trial",
        "400ms",
        "--save",
        foreign.to_str().unwrap(),
        "-o",
        "json",
    ]);
    assert_eq!(code(&bad), 2, "{}", stdout(&bad));
    assert_eq!(error_code(&bad), "usage.invalid_argument");
    assert_eq!(
        std::fs::read_to_string(&foreign).unwrap(),
        "[package]\nname = \"app\"\n"
    );
    assert_eq!(
        std::fs::read_dir(env.data().join("runs")).unwrap().count(),
        proves_before,
        "no trial ran"
    );
    // The last prove is still the worse one.
    let last_p = env.run(&["show", "last-prove", "-o", "json"]);
    assert_eq!(json(&last_p)["prove_id"], wv["prove_id"]);
    let pv = wv;
    assert_eq!(code(&last_p), 0, "{}", stderr(&last_p));
    assert_eq!(json(&last_p)["prove_id"], pv["prove_id"]);
    assert_eq!(json(&last_p)["schema"], "oprobe.prove/9");
    // Run-only flags on a prove result are refused cleanly instead of silently ignored.
    for extra in [
        &["--by", "stage"][..],
        &["--detail"][..],
        &["--since", "100ms"][..],
    ] {
        let mut args = vec!["show", "last-prove"];
        args.extend_from_slice(extra);
        args.extend(["-o", "json"]);
        let bad = env.run(&args);
        assert_eq!(code(&bad), 2, "{extra:?}: {}", stdout(&bad));
        assert_eq!(error_code(&bad), "usage.invalid_argument");
    }
}

/// D3: the hint for an unreadable prove result never suggests a `show --rm` that would match
/// something else: a file outside the store (whose stem prefix-matches a stored run) is to be
/// deleted as a file; only a stored prove named by its full id is removed with `--rm`.
#[test]
fn an_unreadable_prove_hint_never_removes_an_unrelated_run() {
    let env = Env::new();
    let o = env.run(&run_args(&["memory://"], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let run_id = json(&o)["runs"][0]["run_id"].as_str().unwrap().to_owned();
    // `20261125.prove.json` next to the user's files: its stem prefixes the stored run's id.
    let outside = env.path(&format!("{}.prove.json", &run_id[..8]));
    std::fs::write(
        &outside,
        r#"{"schema":"oprobe.prove/1","prove_id":"x","verdict":"improved"}"#,
    )
    .unwrap();
    let o = env.run(&["show", outside.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    let hint = json(&o)["error"]["hint"].as_str().unwrap().to_owned();
    assert!(!hint.contains("--rm"), "{hint}");
    assert!(hint.contains("delete the file"), "{hint}");
    assert_eq!(env.runs().len(), 1);
    // A stored prove, named by its full id, is removed with `--rm ID`.
    let id = "20200101-000000-0001";
    std::fs::write(
        env.data().join(format!("runs/{id}.prove.json")),
        r#"{"schema":"oprobe.prove/1"}"#,
    )
    .unwrap();
    let o = env.run(&["show", id, "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    let hint = json(&o)["error"]["hint"].as_str().unwrap().to_owned();
    assert!(hint.contains(&format!("oprobe show --rm {id}")), "{hint}");
}

/// W5/W6: a file oprobe cannot read says what it is and which command takes it: random bytes
/// are "not an oprobe run file" (a usage error, not `internal`), a prove result passed to
/// `prove` is a prove result, a run's sidecar points at its run.
#[test]
fn files_that_are_not_runs_say_what_they_are() {
    let env = Env::new();
    let junk = env.path("junk.bin");
    let bytes: Vec<u8> = (0..2048u32).map(|i| (i * 7919 % 251) as u8).collect();
    std::fs::write(&junk, bytes).unwrap();
    let o = env.run(&["show", junk.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    let e = &json(&o)["error"];
    assert_eq!(e["code"], "usage.invalid_argument", "{e}");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("not an oprobe run file"),
        "{e}"
    );
    assert!(e["hint"].is_string(), "{e}");
    // A run's sidecar.
    let o = env.run(&run_args(&["memory://"], &["-q", "-o", "json"]));
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let run_id = json(&o)["runs"][0]["run_id"].as_str().unwrap().to_owned();
    let sidecar = env.data().join(format!("runs/{run_id}.json"));
    assert!(sidecar.is_file());
    let o = env.run(&["prove", sidecar.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    let e = &json(&o)["error"];
    assert!(e["message"].as_str().unwrap().contains("sidecar"), "{e}");
    assert!(
        e["hint"]
            .as_str()
            .unwrap()
            .contains(&format!("{run_id}.oprobe")),
        "{e}"
    );
    // A prove result passed to `prove`.
    let fake = env.path("x.prove.json");
    std::fs::write(
        &fake,
        format!(
            r#"{{"schema":"oprobe.prove/9","prove_id":"20200101-000000-0001","run_id":"{run_id}","verdict":"improved"}}"#
        ),
    )
    .unwrap();
    let o = env.run(&["prove", fake.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&o), 2, "{}", stdout(&o));
    let e = &json(&o)["error"];
    assert!(
        e["message"].as_str().unwrap().contains("prove result"),
        "{e}"
    );
    let hint = e["hint"].as_str().unwrap();
    assert!(hint.contains(&run_id) && !hint.contains("versions"), "{e}");
}
