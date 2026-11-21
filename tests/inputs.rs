//! No user input yields `internal` (design v4 §8): a sweep of the bad inputs of the 3.0 review
//! (docs/design/v4-review.md, CLI-1) through the CLI (`-o json`) and, where the MCP server
//! takes the same input, through its tools. Every error must be classified (`usage.*`,
//! `config.*`, `target.*`, `auth.*` or `run.*`), exit 2 for usage and config errors, and the
//! CLI and MCP must agree on the code.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn cmd(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
    c.env("OPROBE_DATA_DIR", dir.join("data"))
        .env("OPROBE_CONFIG", dir.join("config.toml"))
        .env_remove("OPROBE_OUTPUT")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .stdin(Stdio::null());
    c
}

/// Run the CLI with `-o json`; returns `(exit code, error code)`. The error is the JSON error
/// document on stdout.
fn cli_error(dir: &Path, args: &[&str]) -> (i32, String, Value) {
    let o = cmd(dir).args(args).args(["-o", "json"]).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| {
        panic!(
            "{args:?}: no JSON on stdout ({e}): {out}\nstderr: {}",
            String::from_utf8_lossy(&o.stderr)
        )
    });
    assert_eq!(v["type"], "error", "{args:?} did not fail: {v}");
    let code = v["error"]["code"].as_str().unwrap().to_owned();
    (o.status.code().unwrap_or(-1), code, v["error"].clone())
}

/// The code families user input may produce.
fn classified(code: &str) -> bool {
    ["usage.", "config.", "target.", "auth.", "run."]
        .iter()
        .any(|p| code.starts_with(p))
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: i64,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mcp {
    fn start(dir: &Path) -> Self {
        let mut child = cmd(dir)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let mut m = Self {
            child,
            stdin,
            rx,
            next_id: 0,
        };
        m.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "oprobe-inputs", "version": "0"}
            }),
        );
        m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        m
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to {method}"));
            if v.get("id") == Some(&json!(id)) && v.get("method").is_none() {
                assert!(v.get("error").is_none(), "{method}: {v}");
                return v["result"].clone();
            }
        }
    }

    /// Call a tool that must fail; returns its error code.
    fn tool_error(&mut self, name: &str, args: Value) -> String {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert_eq!(r["isError"], true, "{name} {args} should fail: {r}");
        let err: Value = serde_json::from_str(r["content"][1]["text"].as_str().unwrap()).unwrap();
        err["error"]["code"].as_str().unwrap().to_owned()
    }
}

/// One bad input: CLI arguments and, where MCP takes it too, a tool call.
struct Case {
    what: &'static str,
    cli: Vec<String>,
    mcp: Option<(&'static str, Value)>,
    /// The code both surfaces must give.
    want: &'static str,
}

fn case(
    what: &'static str,
    cli: &[&str],
    mcp: Option<(&'static str, Value)>,
    want: &'static str,
) -> Case {
    Case {
        what,
        cli: cli.iter().map(|s| (*s).to_owned()).collect(),
        mcp,
        want,
    }
}

fn plan(args: Value) -> Option<(&'static str, Value)> {
    Some(("plan_run", args))
}

#[test]
fn bad_inputs_are_classified_and_cli_and_mcp_agree() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(
        d.join("config.toml"),
        "[targets.bad]\nurl = \"s3://\"\n\n[targets.ok]\nurl = \"memory://\"\n",
    )
    .unwrap();
    let missing_profile = d.join("missing.toml").display().to_string();
    let missing_config = d.join("nope.toml").display().to_string();
    let run = |extra: &[&str]| -> Vec<&'static str> {
        let mut v = vec!["run", "memory://", "--dry-run"];
        for e in extra {
            v.push(Box::leak(e.to_string().into_boxed_str()));
        }
        v
    };
    let cases = vec![
        case(
            "-d 5x",
            &run(&["-d", "5x"]),
            plan(json!({"target": "memory://", "duration": "5x"})),
            "usage.invalid_argument",
        ),
        case(
            "-d -3s",
            &run(&["-d", "-3s"]),
            plan(json!({"target": "memory://", "duration": "-3s"})),
            "usage.invalid_argument",
        ),
        case(
            "--warmup 1x",
            &run(&["--warmup", "1x"]),
            plan(json!({"target": "memory://", "warmup": "1x"})),
            "usage.invalid_argument",
        ),
        case(
            "lab --latency 5x",
            &["lab", "--latency", "5x"],
            None,
            "usage.invalid_argument",
        ),
        case(
            "lab --scenario nope",
            &["lab", "--scenario", "nope"],
            None,
            "usage.invalid_argument",
        ),
        case(
            "--preset nope",
            &run(&["--preset", "nope"]),
            plan(json!({"target": "memory://", "preset": "nope"})),
            "usage.invalid_argument",
        ),
        case(
            "--provider nope",
            &run(&["--provider", "nope"]),
            plan(json!({"target": "memory://", "provider": "nope"})),
            "usage.invalid_argument",
        ),
        case(
            "--access weird",
            &run(&["--access", "weird"]),
            plan(json!({"target": "memory://", "access": "weird"})),
            "usage.invalid_argument",
        ),
        case(
            "foo://bar",
            &["run", "foo://bar", "--dry-run"],
            plan(json!({"target": "foo://bar"})),
            "usage.invalid_argument",
        ),
        case(
            "s3://",
            &["run", "s3://", "--dry-run"],
            plan(json!({"target": "s3://"})),
            "usage.invalid_argument",
        ),
        case(
            "s3:///x",
            &["run", "s3:///x", "--dry-run"],
            plan(json!({"target": "s3:///x"})),
            "usage.invalid_argument",
        ),
        case(
            "config url s3://",
            &["run", "bad", "--dry-run"],
            plan(json!({"target": "bad"})),
            "config.invalid",
        ),
        case(
            "unknown target",
            &["run", "nope", "--dry-run"],
            plan(json!({"target": "nope"})),
            "usage.unknown_target",
        ),
        case(
            "--profile missing.toml",
            &["run", "--profile", &missing_profile, "--dry-run"],
            None,
            "usage.invalid_argument",
        ),
        case(
            "--object-size 0",
            &run(&["--object-size", "0"]),
            plan(json!({"target": "memory://", "object_size": "0"})),
            "usage.invalid_argument",
        ),
        case(
            "--object-size 4x",
            &run(&["--object-size", "4x"]),
            plan(json!({"target": "memory://", "object_size": "4x"})),
            "usage.invalid_argument",
        ),
        case(
            "--endpoint ftp://x",
            &["run", "s3://b/x", "--endpoint", "ftp://x", "--dry-run"],
            plan(json!({"target": "s3://b/x", "endpoint": "ftp://x"})),
            "usage.invalid_argument",
        ),
        case(
            "--http h3",
            &run(&["--http", "h3"]),
            plan(json!({"target": "memory://", "http": "h3"})),
            "usage.invalid_argument",
        ),
        case(
            "--chunk-size 1q",
            &run(&["--chunk-size", "1q"]),
            plan(json!({"target": "memory://", "chunk_size": "1q"})),
            "usage.invalid_argument",
        ),
        case(
            "--option without =",
            &run(&["--option", "novalue"]),
            None,
            "usage.invalid_argument",
        ),
        case(
            "unknown backend option",
            &["run", "s3://b/x", "--option", "regin=x", "--dry-run"],
            plan(json!({"target": "s3://b/x", "options": {"regin": "x"}})),
            "config.unknown_option",
        ),
        case(
            "--max-cost-usd -1",
            &run(&["--max-cost-usd", "-1"]),
            plan(json!({"target": "memory://", "max_cost_usd": -1.0})),
            "usage.invalid_argument",
        ),
        case(
            "--max-bytes lots",
            &run(&["--max-bytes", "lots"]),
            plan(json!({"target": "memory://", "max_bytes": "lots"})),
            "usage.invalid_argument",
        ),
        case(
            "--max-duration 1x",
            &run(&["--max-duration", "1x"]),
            plan(json!({"target": "memory://", "max_duration": "1x"})),
            "usage.invalid_argument",
        ),
        case(
            "--max-error-rate 200%",
            &run(&["--max-error-rate", "200%"]),
            None,
            "usage.invalid_argument",
        ),
        case(
            "--rate -5",
            &run(&["--rate", "-5"]),
            plan(json!({"target": "memory://", "rate": -5.0})),
            "usage.invalid_argument",
        ),
        case(
            "--egress-fraction 2",
            &run(&["--egress-fraction", "2"]),
            None,
            "usage.invalid_argument",
        ),
        case(
            "--set unknown key",
            &run(&["--set", "concurency=8"]),
            None,
            "usage.invalid_argument",
        ),
        case(
            "--set bad value",
            &run(&["--set", "warmup=5x"]),
            None,
            "usage.invalid_argument",
        ),
        case(
            "--from unknown run",
            &["run", "--from", "nope", "--dry-run"],
            None,
            "usage.unknown_run",
        ),
        case(
            "--from last~x",
            &["run", "--from", "last~x", "--dry-run"],
            None,
            "usage.invalid_argument",
        ),
        case(
            "non-existent --config",
            &["--config", &missing_config, "runs", "list"],
            None,
            "config.invalid",
        ),
        case(
            "target add duplicate",
            &["target", "add", "ok", "memory://", "--no-test"],
            None,
            "usage.invalid_argument",
        ),
        case(
            "target add s3://",
            &["target", "add", "x", "s3://", "--no-test"],
            None,
            "usage.invalid_argument",
        ),
        case(
            "baseline show nope",
            &["baseline", "show", "nope"],
            None,
            "usage.unknown_baseline",
        ),
        case(
            "runs show nope",
            &["runs", "show", "nope"],
            Some(("get_run", json!({"run": "nope"}))),
            "usage.unknown_run",
        ),
        case(
            "explain nope",
            &["explain", "nope"],
            Some(("explain_run", json!({"run": "nope"}))),
            "usage.unknown_run",
        ),
        case(
            "top without a terminal",
            &["top"],
            None,
            "usage.invalid_argument",
        ),
        case(
            "gui without the feature",
            &["gui"],
            None,
            "usage.invalid_argument",
        ),
    ];
    let mut mcp = Mcp::start(d);
    let mut failures = Vec::new();
    for c in &cases {
        let args: Vec<&str> = c.cli.iter().map(String::as_str).collect();
        // `gui` with the `gui` feature opens a window; it is a usage error only without it.
        if c.what == "gui without the feature" && cfg!(feature = "gui") {
            continue;
        }
        let (exit, code, err) = cli_error(d, &args);
        if !classified(&code) {
            failures.push(format!("{}: CLI gave `{code}`: {err}", c.what));
            continue;
        }
        if code != c.want {
            failures.push(format!(
                "{}: CLI gave `{code}`, want `{}`: {err}",
                c.what, c.want
            ));
        }
        let want_exit = if code.starts_with("usage.") || code.starts_with("config.") {
            2
        } else {
            1
        };
        if exit != want_exit {
            failures.push(format!("{}: CLI exit {exit}, want {want_exit}", c.what));
        }
        if err["message"].as_str().is_none_or(str::is_empty) {
            failures.push(format!("{}: no message", c.what));
        }
        if let Some((tool, a)) = &c.mcp {
            let m = mcp.tool_error(tool, a.clone());
            if m != code {
                failures.push(format!(
                    "{}: CLI gave `{code}` but MCP {tool} gave `{m}`",
                    c.what
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The hints of typed parsers survive clap: a near miss names what was meant.
#[test]
fn near_misses_get_a_did_you_mean_hint() {
    let dir = tempfile::tempdir().unwrap();
    for (args, want) in [
        (
            vec!["run", "memory://", "--preset", "parqet", "--dry-run"],
            "did you mean `parquet`",
        ),
        (
            vec!["run", "memory://", "--provider", "gcss", "--dry-run"],
            "did you mean `gcs`",
        ),
        (
            vec!["run", "memory://", "--access", "unifrom", "--dry-run"],
            "did you mean `uniform`",
        ),
        (vec!["run", "s4://b/x", "--dry-run"], "did you mean `s3`"),
        (
            vec!["run", "memory://", "-d", "5sec", "--dry-run"],
            "did you mean `5s`",
        ),
        (
            vec!["run", "memory://", "--set", "concurency=8", "--dry-run"],
            "did you mean `concurrency`",
        ),
    ] {
        let (_, _, err) = cli_error(dir.path(), &args);
        let hint = err["hint"].as_str().unwrap_or_default();
        assert!(hint.contains(want), "{args:?}: {err}");
    }
}

/// Aliases resolve instead of failing (`--provider aws` was `internal` in 3.0).
#[test]
fn provider_aliases_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    for (alias, key) in [
        ("aws", "aws-s3"),
        ("gcp", "gcs"),
        ("azure", "azure-blob"),
        ("r2", "cloudflare-r2"),
    ] {
        let o = cmd(dir.path())
            .args([
                "run",
                "memory://",
                "--provider",
                alias,
                "--dry-run",
                "-o",
                "json",
            ])
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{alias}: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert!(
            v["plan"]["estimated_cost_usd"].is_number(),
            "{alias} → {key}: {v}"
        );
    }
}

/// Missing credentials are `auth.no_credentials` (3.0: `internal` from the preflight).
#[test]
fn missing_gcs_credentials_are_auth_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let (exit, code, err) = cli_error(
        dir.path(),
        &[
            "run",
            "gcs://oprobe-no-such-bucket/x",
            "-d",
            "1s",
            "--option",
            "disable_vm_metadata=true",
            "--option",
            "disable_config_load=true",
        ],
    );
    assert_eq!(code, "auth.no_credentials", "{err}");
    assert_eq!(exit, 1);
}
