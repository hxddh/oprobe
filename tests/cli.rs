//! The 3.0 CLI contract: output modes, classified errors and exit codes, safety rails, the
//! run store and configured targets. Every invocation uses its own config and data directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
        c.env("OPROBE_DATA_DIR", self.data())
            .env("OPROBE_CONFIG", self.dir.path().join("config.toml"))
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

const SMALL: &[&str] = &[
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
];

fn small_run(extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = ["run", "memory://", "-d", "300ms"]
        .iter()
        .chain(SMALL)
        .map(|s| s.to_string())
        .collect();
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

fn run_small(env: &Env, extra: &[&str]) -> Output {
    env.cmd().args(small_run(extra)).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `-o json`: exactly one JSON document on stdout, logs on stderr.
fn json(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", stdout(o)))
}

fn error_code(o: &Output) -> String {
    let v = json(o);
    assert_eq!(v["type"], "error", "{v}");
    v["error"]["code"].as_str().unwrap().to_owned()
}

#[test]
fn run_saves_the_run_and_prints_one_json_document() {
    let env = Env::new();
    let o = run_small(&env, &["-o", "json", "--label", "first"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v = json(&o);
    let id = v["run_id"].as_str().unwrap().to_owned();
    assert_eq!(v["report"]["schema"], "oprobe.report/3");
    assert_eq!(v["report"]["run"]["id"], id.as_str());
    assert_eq!(v["report"]["run"]["label"], "first");
    for key in [
        "run",
        "target",
        "workload",
        "knobs",
        "sla",
        "verdict",
        "summary",
        "stages",
        "apis",
        "verbs",
        "connections",
        "network",
        "timeline",
        "heatmap",
        "findings",
        "cost",
        "slowest",
        "cleanup",
    ] {
        assert!(v["report"].get(key).is_some(), "report.{key} missing");
    }
    assert!(v["report"]["knobs"]["requested"]["chunk_bytes"].is_u64());
    assert!(v["report"]["summary"]["latency"]["p99_ms"].is_number());
    assert!(
        !v["report"]["heatmap"]["counts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let path = PathBuf::from(v["path"].as_str().unwrap());
    assert!(path.is_file());
    assert_eq!(env.runs(), vec![path.clone()]);

    // RUNREFs: id, prefix, last, label and path all resolve to it.
    let prefix = &id[..17];
    for r in [id.as_str(), prefix, "last", "first", path.to_str().unwrap()] {
        let o = env.run(&["runs", "show", r, "-o", "json"]);
        assert!(o.status.success(), "{r}: {}", stderr(&o));
        assert_eq!(json(&o)["report"]["run"]["id"], id.as_str(), "{r}");
    }
    let list = json(&env.run(&["runs", "list", "-o", "json"]));
    assert_eq!(list["runs"][0]["id"], id.as_str());
    assert_eq!(list["total_count"], 1);
    // A run compares cleanly with itself (RUNREFs and paths alike).
    let o = env.run(&["compare", "last", path.to_str().unwrap(), "-o", "json"]);
    assert!(o.status.success(), "{}", stdout(&o));
    assert_eq!(json(&o)["passed"], true);
    // rm + unknown run.
    assert!(env.run(&["runs", "rm", "last"]).status.success());
    assert!(env.runs().is_empty());
    let o = env.run(&["runs", "show", "last", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(error_code(&o), "usage.unknown_run");
}

#[test]
fn text_output_is_brief() {
    let env = Env::new();
    let o = run_small(&env, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.lines().count() <= 20, "{text}");
    assert!(text.lines().all(|l| l.chars().count() <= 80), "{text}");
    assert!(text.contains("full report: oprobe runs show"), "{text}");
}

#[test]
fn markdown_and_html_outputs() {
    let env = Env::new();
    let o = run_small(&env, &["-o", "md", "--no-save"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("## oprobe report"), "{}", stdout(&o));
    assert!(env.runs().is_empty(), "--no-save");
    let o = env.run(&["runs", "list", "-o", "html"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("not implemented yet"), "{}", stderr(&o));
}

#[test]
fn ndjson_streams_events_then_the_result() {
    let env = Env::new();
    let o = run_small(&env, &["-o", "ndjson"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let lines: Vec<serde_json::Value> = stdout(&o)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert_eq!(lines[0]["type"], "version");
    assert_eq!(lines[0]["schema"], "oprobe.events/1");
    assert!(
        lines
            .iter()
            .all(|l| l["ts"].is_string() && l["type"].is_string())
    );
    assert!(lines.iter().any(|l| l["type"] == "phase"));
    assert!(lines.iter().any(|l| l["type"] == "progress"));
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["report"]["schema"], "oprobe.report/3");
    assert!(Path::new(last["path"].as_str().unwrap()).is_file());
}

#[test]
fn errors_are_classified_with_exit_codes() {
    let env = Env::new();
    // usage
    let o = env.run(&["run", "no-such-target", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(error_code(&o), "usage.unknown_target");
    // later streams: internal, exit 1
    for cmd in [
        &["demo"][..],
        &["schema", "report"][..],
        &["mcp"][..],
        &["explain", "throttling"][..],
        &["target", "test", "x"][..],
    ] {
        // Global flags go before the arguments of a pending command (it accepts any).
        let mut args = vec!["-o", "json"];
        args.extend(cmd);
        let o = env.run(&args);
        assert_eq!(o.status.code(), Some(1), "{cmd:?}");
        assert_eq!(error_code(&o), "internal", "{cmd:?}");
        assert!(
            json(&o)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not implemented yet")
        );
    }
    // ndjson: version line, then the error event.
    let o = env.run(&["runs", "show", "nope", "-o", "ndjson"]);
    assert_eq!(o.status.code(), Some(2));
    let lines: Vec<serde_json::Value> = stdout(&o)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "version");
    assert_eq!(lines[1]["type"], "error");
    assert_eq!(lines[1]["error"]["code"], "usage.unknown_run");
    // text: error + hint on stderr, nothing on stdout.
    let o = env.run(&["runs", "show", "nope"]);
    assert!(stdout(&o).is_empty());
    assert!(stderr(&o).contains("error:") && stderr(&o).contains("hint:"));
}

#[test]
fn read_only_refuses_runs_that_write() {
    let env = Env::new();
    let o = env
        .cmd()
        .args(small_run(&["--read-only", "-o", "json"]))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert_eq!(error_code(&o), "safety.read_only");
    assert!(env.runs().is_empty());
}

#[test]
fn budgets_stop_the_run_with_exit_4() {
    let env = Env::new();
    let mut args: Vec<String> = ["run", "memory://", "-d", "30s"]
        .iter()
        .chain(SMALL)
        .map(|s| s.to_string())
        .collect();
    args.extend(["--max-requests", "40", "-o", "json"].map(String::from));
    let t = std::time::Instant::now();
    let o = env.cmd().args(&args).output().unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", stderr(&o));
    assert_eq!(error_code(&o), "safety.budget_exceeded");
    assert!(t.elapsed().as_secs() < 20, "the budget stops the 30 s run");
    // The partial run is saved and marked.
    let runs = env.runs();
    assert_eq!(runs.len(), 1);
    let show = json(&env.run(&["runs", "show", "last", "-o", "json"]));
    assert_eq!(show["report"]["run"]["interrupted"], true);
    assert_eq!(show["report"]["verdict"]["exit_code"], 4);

    let mut args: Vec<String> = ["run", "memory://", "-d", "30s"]
        .iter()
        .chain(SMALL)
        .map(|s| s.to_string())
        .collect();
    args.extend(["--max-duration", "1s", "-o", "json"].map(String::from));
    let t = std::time::Instant::now();
    let o = env.cmd().args(&args).output().unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", stderr(&o));
    assert!(error_code(&o).contains("budget"));
    assert!(t.elapsed().as_secs() < 20);
}

#[test]
fn dry_run_prints_the_plan_and_touches_nothing() {
    let env = Env::new();
    let root = env.dir.path().join("bucket");
    std::fs::create_dir_all(&root).unwrap();
    let url = format!("fs://{}/", root.display());
    let o = env.run(&[
        "run",
        &url,
        "--dry-run",
        "--max-requests",
        "1000",
        "--idempotency-key",
        "ci-42",
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let p = &json(&o)["plan"];
    for k in [
        "estimated_requests_count",
        "estimated_bytes",
        "objects_written_count",
        "objects_deleted_count",
        "eta_s",
    ] {
        assert!(p[k].is_number(), "{k}: {p}");
    }
    assert!(p["objects_written_count"].as_u64().unwrap() > 0);
    assert!(
        p["run_prefix"]
            .as_str()
            .unwrap()
            .starts_with("oprobe-run-key-ci-42-")
    );
    assert_eq!(p["budgets"]["max_requests_count"], 1000);
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "nothing written"
    );
    assert!(env.runs().is_empty());
}

#[test]
fn preflight_fails_before_any_write() {
    let env = Env::new();
    let dir = env.dir.path().join("pf");
    std::fs::create_dir_all(&dir).unwrap();
    // The target root is a file: listing or writing under it fails before the dataset upload.
    std::fs::write(dir.join("root"), b"x").unwrap();
    let url = format!("fs://{}/", dir.join("root").display());
    let o = env
        .cmd()
        .args(["run", &url, "-d", "1s", "-o", "json"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let v = json(&o);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("preflight"),
        "{v}"
    );
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(left.len(), 1, "only the file itself remains");
    assert_eq!(std::fs::read(dir.join("root")).unwrap(), b"x");
    assert!(env.runs().is_empty());
}

#[test]
fn configured_targets_and_config_commands() {
    let env = Env::new();
    let o = env.run(&["target", "add", "local", "memory://", "-o", "json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(json(&o)["name"], "local");
    let list = json(&env.run(&["target", "list", "-o", "json"]));
    assert_eq!(list["targets"][0]["url"], "memory://");
    let mut args = vec!["run", "local", "-d", "200ms", "-o", "json"];
    args.extend(SMALL);
    let o = env.run(&args);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(json(&o)["report"]["target"]["name"], "local");
    let paths = json(&env.run(&["config", "path", "-o", "json"]));
    assert!(paths["config_exists"].as_bool().unwrap());
    let show = json(&env.run(&["config", "show", "-o", "json"]));
    let output = show["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "output")
        .unwrap()
        .clone();
    assert_eq!(output["source"], "flag");
    assert!(env.run(&["target", "rm", "local"]).status.success());
    let o = env.run(&["target", "show", "local", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(error_code(&o), "usage.unknown_target");
}

#[test]
fn every_contract_command_is_in_help() {
    let env = Env::new();
    let help = stdout(&env.run(&["--help"]));
    for c in [
        "run",
        "demo",
        "tune",
        "compare",
        "runs",
        "analyze",
        "explain",
        "target",
        "doctor",
        "probe",
        "watch",
        "matrix",
        "compat",
        "baseline",
        "lab",
        "mcp",
        "schema",
        "capabilities",
        "config",
        "cleanup",
        "self-update",
        "top",
        "gui",
        "completions",
        "man",
    ] {
        assert!(
            help.lines()
                .any(|l| l.trim_start().starts_with(&format!("{c} ")) || l.trim() == c),
            "`{c}` missing from --help:\n{help}"
        );
    }
}
