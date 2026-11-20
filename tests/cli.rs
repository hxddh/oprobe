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
    // A command without an HTML renderer: a usage error naming the formats it has.
    let o = env.run(&["runs", "list", "-o", "html"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no HTML output"), "{}", stderr(&o));
    assert!(stderr(&o).contains("-o md"), "{}", stderr(&o));
    assert!(!stderr(&o).contains("not implemented"), "{}", stderr(&o));
}

#[test]
fn runs_open_without_a_window_points_to_the_terminal_workbench() {
    let env = Env::new();
    assert!(run_small(&env, &["-q"]).status.success());
    // Not a terminal here: no fallback to start, the hint names it.
    let o = env
        .cmd()
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .args(["runs", "open", "last", "-o", "json"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "{}", stdout(&o));
    assert!(
        json(&o)["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("oprobe top --open")
    );
}

#[test]
fn an_empty_existing_prefix_is_not_found() {
    let env = Env::new();
    let dir = env.dir.path().join("bucket");
    std::fs::create_dir_all(dir.join("empty")).unwrap();
    let url = format!("fs://{}", dir.display());
    let o = env.run(&[
        "run",
        &url,
        "--existing",
        "empty/",
        "-d",
        "200ms",
        "--no-save",
        "-o",
        "json",
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert_eq!(error_code(&o), "storage.not_found");
    assert!(json(&o)["error"]["hint"].is_string());
}

#[test]
fn a_closed_stdout_ends_quietly() {
    use std::io::Read;
    let env = Env::new();
    // Larger than a pipe buffer: the writer hits the closed pipe.
    let mut child = env
        .cmd()
        .args(["schema", "report"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0u8; 16];
    child.stdout.take().unwrap().read_exact(&mut first).unwrap();
    let o = child.wait_with_output().unwrap();
    let code = o.status.code();
    assert!(matches!(code, Some(0) | Some(141)), "{code:?}");
    assert!(!stderr(&o).contains("panicked"), "{}", stderr(&o));
    assert!(!stderr(&o).contains("error"), "{}", stderr(&o));
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
    // `target test` is the doctor of a configured target.
    let o = env.run(&["target", "test", "x", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(error_code(&o), "usage.unknown_target");
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
fn argument_errors_follow_the_output_mode() {
    let env = Env::new();
    // json: the error document on stdout, exit 2.
    let o = env.run(&["run", "x", "--bogus", "-o", "json"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(error_code(&o), "usage.invalid_argument");
    assert!(
        json(&o)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--bogus")
    );
    // ndjson (also through OPROBE_OUTPUT): version line, then the error event.
    let o = env
        .cmd()
        .env("OPROBE_OUTPUT", "ndjson")
        .args(["run", "--bogus"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    let lines: Vec<serde_json::Value> = stdout(&o)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "version");
    assert_eq!(lines[1]["error"]["code"], "usage.invalid_argument");
    // text: clap's message on stderr; help and version still exit 0.
    let o = env.run(&["run", "x", "--bogus"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stdout(&o).is_empty() && stderr(&o).contains("--bogus"));
    for args in [&["--help", "-o", "json"][..], &["--version"][..]] {
        let o = env.run(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}");
        assert!(!stdout(&o).is_empty());
    }
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
fn budgets_cover_the_whole_command() {
    let env = Env::new();
    // Each run alone fits the 1 s budget; three of them do not.
    let mut args: Vec<String> = ["run", "memory://", "-d", "600ms", "--repeat", "3"]
        .iter()
        .chain(SMALL)
        .map(|s| s.to_string())
        .collect();
    args.extend(["--max-duration", "1s", "-o", "json"].map(String::from));
    let o = env.cmd().args(&args).output().unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", stderr(&o));
    assert_eq!(error_code(&o), "safety.budget_exceeded");
    assert!(env.runs().len() < 3, "the budget stopped the group");
}

#[test]
fn every_measuring_command_honours_dry_run() {
    let env = Env::new();
    // watch: the plan of its windows, nothing measured.
    let o = env.run(&[
        "watch",
        "memory://",
        "--count",
        "3",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v = json(&o);
    assert!(v["plan"]["estimated_requests_count"].is_u64(), "{v:#}");
    // matrix: a typed document, one plan per cell.
    let o = env.run(&[
        "matrix",
        "memory://",
        "--vary",
        "concurrency=2,4",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v = json(&o);
    assert_eq!(v["cells"].as_array().unwrap().len(), 2, "{v:#}");
    assert!(v["cells"][1]["plan"]["concurrency_count"] == 4, "{v:#}");
    assert!(v["estimated_requests_count"].is_u64());
    // compat: the plan under the standard run prefix.
    let o = env.run(&["compat", "memory://", "--dry-run", "-o", "json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        json(&o)["run_prefix"]
            .as_str()
            .unwrap()
            .starts_with("oprobe-run-")
    );
    // tune --budget: the trials are capped to fit, or refused when two do not fit.
    let o = env.run(&[
        "tune",
        "memory://",
        "--budget",
        "1h",
        "--max-trials",
        "5",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = env.run(&[
        "tune",
        "memory://",
        "--budget",
        "5s",
        "--dry-run",
        "-o",
        "json",
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(error_code(&o), "usage.invalid_argument");
    assert!(env.runs().is_empty(), "dry runs save nothing");
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

#[test]
fn html_report_of_run_and_runs_show() {
    let env = Env::new();
    let o = run_small(&env, &["-o", "html"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let html = stdout(&o);
    assert!(html.starts_with("<!doctype html>"), "{html}");
    assert!(html.contains("id=\"oprobe-rows\""));
    assert!(!html.contains("src=\"http"));
    let o = env.run(&["runs", "show", "last", "-o", "html"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("id=\"requests\""));
    let o = env.run(&["runs", "show", "-o", "md"]);
    assert!(stdout(&o).contains("## oprobe report"), "{}", stdout(&o));
    let o = env.run(&["compare", "last", "last", "-o", "html"]);
    assert!(stdout(&o).contains("Regression gate"), "{}", stderr(&o));
}

#[test]
fn runs_show_fits_80_columns_and_detail_adds_sections() {
    let env = Env::new();
    assert!(run_small(&env, &[]).status.success());
    let text = stdout(&env.run(&["runs", "show"]));
    assert!(text.contains("KEY NUMBERS"), "{text}");
    assert!(text.lines().all(|l| l.chars().count() <= 80), "{text}");
    let detail = stdout(&env.run(&["runs", "show", "last", "--detail"]));
    assert!(detail.contains("CONFIGURATION"), "{detail}");
    let list = stdout(&env.run(&["runs", "list"]));
    assert!(
        list.contains("verdict") && list.contains("just now"),
        "{list}"
    );
    let o = env.run(&[
        "runs",
        "gc",
        "--keep",
        "0",
        "--older-than",
        "30d",
        "-o",
        "json",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(json(&o)["removed"].as_array().unwrap().is_empty());
    let o = env.run(&["runs", "gc", "--older-than", "soon"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn demo_narrates_and_saves_a_run() {
    let env = Env::new();
    let o = env.run(&["demo", "--duration", "1s"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.contains("What oprobe found"), "{text}");
    assert!(text.contains("oprobe target add"), "{text}");
    assert_eq!(env.runs().len(), 1);
    let o = env.run(&["demo", "--duration", "1s", "-o", "json"]);
    let v = json(&o);
    assert_eq!(v["report"]["schema"], "oprobe.report/3");
    assert!(v["steps"].as_array().unwrap().len() >= 4);
}

#[test]
fn target_test_runs_the_doctor() {
    let env = Env::new();
    let o = env.run(&["target", "add", "local", "memory://"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("ready to benchmark"), "{}", stdout(&o));
    let o = env.run(&["target", "test", "local", "-o", "json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(json(&o)["ready"], true);
    let o = env.run(&["target", "test", "nope", "-o", "json"]);
    assert_eq!(error_code(&o), "usage.unknown_target");
    // `config edit` refuses without a terminal.
    let o = env.run(&["config", "edit", "-o", "json"]);
    assert_eq!(error_code(&o), "usage.invalid_argument");
}

/// A one-shot HTTP server answering every request with `body`.
fn serve(body: String) -> String {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().take(4) {
            let Ok(mut s) = s else { continue };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    format!("http://{addr}")
}

#[test]
fn self_update_check_reports_a_newer_release() {
    let env = Env::new();
    let api = serve(
        r#"{"tag_name":"v99.0.0","html_url":"https://example.invalid/r","assets":[]}"#.into(),
    );
    let o = env
        .cmd()
        .env("OPROBE_UPDATE_API", &api)
        .args(["self-update", "--check", "-o", "json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let v = json(&o);
    assert_eq!(v["latest_version"], "99.0.0");
    assert_eq!(v["update_available"], true);
    assert_eq!(v["installed"], false);
}
