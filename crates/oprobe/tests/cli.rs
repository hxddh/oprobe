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
        let b = &json(&o)["by"];
        assert_eq!(b["by"], by);
        assert!(!b["groups"].as_array().unwrap().is_empty(), "{b}");
    }
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
    assert_eq!(last["report"]["run"]["id"], run_id.as_str());
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
        "--budget",
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
            .any(|n| n.as_str().unwrap().contains("allowed only 1 A/B pair")),
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
