//! `oprobe mcp` end to end: the binary is spawned as a stdio MCP server (JSON-RPC 2.0, one
//! message per line) and driven like an agent would: initialize → tools/list → plan_run →
//! start_run (with progress) → get_run → explain_run → analyze_run → compare_runs →
//! resources → prompts; plus the safety rails (read-only refusal, budgets, allowlist,
//! cancellation) against `memory://` and the in-process S3 lab.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: i64,
    /// Notifications received so far.
    notes: Vec<Value>,
    /// outputSchema of every tool (from tools/list).
    schemas: std::collections::BTreeMap<String, Value>,
    _dir: tempfile::TempDir,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mcp {
    fn start(args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .arg("mcp")
            .args(args)
            .env("OPROBE_DATA_DIR", dir.path().join("data"))
            .env("OPROBE_CONFIG", dir.path().join("config.toml"))
            .env("AWS_ACCESS_KEY_ID", "oprobe-lab")
            .env("AWS_SECRET_ACCESS_KEY", "oprobe-lab")
            .env_remove("OPROBE_OUTPUT")
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
                // stdout is the protocol channel: every line must be JSON-RPC.
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("non-JSON on the MCP stdout ({e}): {line}"));
                assert_eq!(v["jsonrpc"], "2.0", "{line}");
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
            notes: Vec::new(),
            schemas: Default::default(),
            _dir: dir,
        };
        let init = m.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "oprobe-test", "version": "0"}
            }),
        );
        assert_eq!(init["serverInfo"]["name"], "oprobe");
        assert_eq!(init["protocolVersion"], "2025-06-18");
        assert!(init["capabilities"]["tools"].is_object());
        assert!(init["capabilities"]["resources"].is_object());
        assert!(init["capabilities"]["prompts"].is_object());
        assert!(init["instructions"].as_str().unwrap().contains("plan_run"));
        m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        let tools = m.request("tools/list", json!({}));
        for t in tools["tools"].as_array().unwrap() {
            m.schemas.insert(
                t["name"].as_str().unwrap().to_owned(),
                t["outputSchema"].clone(),
            );
        }
        m
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a request and wait for its response (notifications are collected).
    fn request_raw(&mut self, method: &str, params: Value) -> Value {
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
                return v;
            }
            if v.get("method").is_some() && v.get("id").is_none() {
                self.notes.push(v);
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let v = self.request_raw(method, params);
        assert!(v.get("error").is_none(), "{method} failed: {v}");
        v["result"].clone()
    }

    /// Call a tool; returns `(is_error, structuredContent or error JSON, text)`.
    fn tool_full(&mut self, name: &str, args: Value, meta: Option<Value>) -> (bool, Value, String) {
        let mut params = json!({"name": name, "arguments": args});
        if let Some(m) = meta {
            params["_meta"] = m;
        }
        let r = self.request("tools/call", params);
        let content = r["content"].as_array().unwrap();
        let text = content[0]["text"].as_str().unwrap().to_owned();
        if r["isError"] == true {
            let err: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
            assert_eq!(err["type"], "error");
            return (true, err["error"].clone(), text);
        }
        let s = r["structuredContent"].clone();
        let schema = &self.schemas[name];
        let validator = jsonschema::validator_for(schema).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&s)
            .take(5)
            .map(|e| format!("{e} at {}", e.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "{name} output does not match its outputSchema: {errors:?}"
        );
        assert!(!text.is_empty());
        (false, s, text)
    }

    fn tool(&mut self, name: &str, args: Value) -> Value {
        let (err, v, text) = self.tool_full(name, args, None);
        assert!(!err, "{name} failed: {text} {v}");
        v
    }

    fn tool_err(&mut self, name: &str, args: Value) -> Value {
        let (err, v, text) = self.tool_full(name, args, None);
        assert!(err, "{name} should fail: {text}");
        v
    }

    /// Poll `get_run` until the run has ended.
    fn wait_run(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let s = self.tool("get_run", json!({"run": id}));
            let st = s["status"].as_str().unwrap();
            if !matches!(st, "queued" | "running") {
                return s;
            }
            assert!(Instant::now() < deadline, "run {id} did not finish");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn read(&mut self, uri: &str) -> (String, String) {
        let r = self.request("resources/read", json!({"uri": uri}));
        let c = &r["contents"][0];
        assert_eq!(c["uri"], uri);
        (
            c["text"].as_str().unwrap().to_owned(),
            c["mimeType"].as_str().unwrap_or("").to_owned(),
        )
    }
}

fn small(target: &str) -> Value {
    json!({
        "target": target,
        "preset": "small-files",
        "duration": "500ms",
        "warmup": "0s",
        "objects": 8,
        "object_size": "16KiB",
        "concurrency": 4
    })
}

#[test]
fn agent_workflow_over_stdio() {
    let mut m = Mcp::start(&["--max-requests", "1000000"]);

    // Deterministic tools/list order with schemas and annotations.
    let tools = m.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "discover_targets",
            "doctor",
            "sample_objects",
            "probe_endpoint",
            "plan_run",
            "start_run",
            "get_run",
            "cancel_run",
            "list_runs",
            "explain_run",
            "analyze_run",
            "compare_runs",
            "start_tune",
            "verify_finding",
            "recommend_config",
            "estimate_cost",
            "cleanup",
            "list_targets"
        ]
    );
    for t in tools["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert_eq!(t["outputSchema"]["type"], "object", "{t}");
        assert!(t["annotations"]["readOnlyHint"].is_boolean(), "{t}");
    }
    let again = m.request("tools/list", json!({}));
    assert_eq!(again, tools, "tools/list must be deterministic");

    // Targets and server settings.
    let targets = m.tool("list_targets", json!({}));
    assert_eq!(targets["allow_write"], false);
    assert_eq!(targets["server_budgets"]["max_requests_count"], 1000000);

    // Plan.
    let plan = m.tool("plan_run", small("memory://"));
    assert_eq!(plan["allowed"], true, "{plan}");
    assert_eq!(plan["writes"], true);
    assert_eq!(plan["budgets"]["max_requests_count"], 1000000);
    assert!(plan["plan"]["estimated_requests_count"].as_u64().unwrap() > 0);
    let est = m.tool("estimate_cost", small("memory://"));
    assert_eq!(est["applicable"], false);

    // Start with a progress token; the call returns at once.
    let mut args = small("memory://");
    args["label"] = json!("first");
    let (err, started, _) = m.tool_full("start_run", args, Some(json!({"progressToken": "p-1"})));
    assert!(!err, "{started}");
    let id1 = started["run_id"].as_str().unwrap().to_owned();
    assert!(matches!(
        started["status"].as_str().unwrap(),
        "queued" | "running"
    ));
    assert!(started["plan"].is_object());
    let done = m.wait_run(&id1);
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(done["report"]["schema"], "oprobe.report/4");
    assert_eq!(done["report"]["run"]["label"], "first");
    assert_eq!(done["summary"]["id"], id1.as_str());
    // Give the last notifications a moment, then check progress arrived in order.
    let _ = m.tool("list_runs", json!({}));
    let progress: Vec<&Value> = m
        .notes
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
        .collect();
    assert!(
        !progress.is_empty(),
        "no progress notifications: {:?}",
        m.notes
    );
    let mut last = -1.0;
    for p in &progress {
        assert_eq!(p["params"]["progressToken"], "p-1");
        let v = p["params"]["progress"].as_f64().unwrap();
        assert!(v > last, "progress must increase: {progress:?}");
        last = v;
    }

    // Explain, attribute, list.
    let ex = m.tool("explain_run", json!({"run": id1}));
    assert_eq!(ex["run_id"], id1.as_str());
    assert!(!ex["narrative"].as_array().unwrap().is_empty());
    assert!(!ex["next"].as_array().unwrap().is_empty());
    assert!(ex["next"][0]["mcp"]["tool"].is_string(), "{ex}");
    let an = m.tool("analyze_run", json!({"run": "last", "slowest_pct": 5.0}));
    assert!(an["attribution"]["selected_count"].as_u64().unwrap() > 0);
    assert!(an["selection"]["min_total_us"].is_u64());
    let an2 = m.tool(
        "analyze_run",
        json!({"run": id1, "selection": {"api": "GetObject", "from_us": 0, "to_us": 200000}}),
    );
    assert_eq!(an2["selection"]["api"], "GetObject");

    // A second run that blocks until done, then compare.
    let mut args = small("memory://");
    args["wait"] = json!(true);
    let r2 = m.tool("start_run", args);
    assert_eq!(r2["status"], "succeeded");
    let id2 = r2["run_id"].as_str().unwrap().to_owned();
    let cmp = m.tool(
        "compare_runs",
        json!({
            "baseline": id1,
            "candidate": id2,
            "thresholds": {"max_p99_increase_pct": 100000.0, "max_p50_increase_pct": 100000.0,
                           "max_throughput_drop_pct": 100.0, "max_cost_increase_pct": 100000.0}
        }),
    );
    assert!(cmp["passed"].is_boolean());
    // The CLI's comparison (significance included), not the raw report diff.
    assert_eq!(cmp["passed"], cmp["gate"]["passed"], "{cmp:#}");
    assert!(cmp["stats"].is_array(), "{cmp:#}");
    assert_eq!(cmp["candidate"]["representative"], id2.as_str());
    let list = m.tool("list_runs", json!({"limit": 5}));
    assert_eq!(list["total_count"], 2);
    assert_eq!(list["started"].as_array().unwrap().len(), 2);
    let stored = m.tool("get_run", json!({"run": "first", "include_report": false}));
    assert_eq!(stored["run_id"], id1.as_str());
    assert!(stored.get("report").is_none());

    // Resources.
    let res = m.request("resources/list", json!({}));
    let uris: Vec<&str> = res["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"oprobe://docs/findings"));
    assert!(uris.contains(&format!("oprobe://runs/{id2}/report").as_str()));
    let templates = m.request("resources/templates/list", json!({}));
    assert_eq!(templates["resourceTemplates"].as_array().unwrap().len(), 6);
    let (report, mime) = m.read(&format!("oprobe://runs/{id1}/report"));
    assert_eq!(mime, "application/json");
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["run"]["id"], id1.as_str());
    let (events, _) = m.read(&format!("oprobe://runs/{id1}/events"));
    let lines: Vec<Value> = events
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "version");
    assert!(lines.iter().any(|l| l["type"] == "progress"));
    assert_eq!(lines.last().unwrap()["type"], "result");
    let (slow, _) = m.read(&format!("oprobe://runs/{id1}/slowest"));
    let slow: Value = serde_json::from_str(&slow).unwrap();
    assert!(!slow["ops"].as_array().unwrap().is_empty());
    let (doc, mime) = m.read(&format!("oprobe://runs/{id1}/html"));
    assert_eq!(mime, "text/html");
    assert!(doc.starts_with("<!doctype html>"));
    let (schema, _) = m.read("oprobe://schema/report");
    let schema: Value = serde_json::from_str(&schema).unwrap();
    assert_eq!(
        schema["$id"],
        "https://github.com/hxddh/oprobe/schemas/4/report.json"
    );
    let (findings, _) = m.read("oprobe://docs/findings");
    assert!(findings.contains("throttling"));
    let (presets, _) = m.read("oprobe://presets");
    assert_eq!(
        serde_json::from_str::<Value>(&presets).unwrap()[0]["name"],
        "parquet"
    );
    let missing = m.request_raw("resources/read", json!({"uri": "oprobe://nope"}));
    assert!(missing["error"].is_object());

    // Prompts.
    let prompts = m.request("prompts/list", json!({}));
    let pn: Vec<&str> = prompts["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(pn, ["diagnose-slow-reads", "tune-for-p99", "ci-gate-setup"]);
    let p = m.request(
        "prompts/get",
        json!({"name": "tune-for-p99", "arguments": {"target": "memory://", "p99_ms": "50"}}),
    );
    let text = p["messages"][0]["content"]["text"].as_str().unwrap();
    assert!(text.contains("start_tune") && text.contains("50 ms"));
    let bad = m.request_raw(
        "prompts/get",
        json!({"name": "tune-for-p99", "arguments": {}}),
    );
    assert!(bad["error"].is_object());

    // Classified tool errors.
    let e = m.tool_err("get_run", json!({"run": "no-such-run"}));
    assert_eq!(e["code"], "usage.unknown_run");
    let e = m.tool_err(
        "plan_run",
        json!({"target": "memory://", "duration": "soon"}),
    );
    assert_eq!(e["code"], "usage.invalid_argument");
    let e = m.tool_err("get_run", json!({"nope": 1}));
    assert_eq!(e["code"], "usage.invalid_argument");
}

#[test]
fn budgets_cancellation_and_allowlist() {
    // A server budget stops every run (exit-4 semantics): the partial run is still saved.
    let mut m = Mcp::start(&["--max-requests", "60", "--allow-target", "memory://"]);
    let mut args = small("memory://");
    args["duration"] = json!("20s");
    args["wait"] = json!(true);
    let r = m.tool("start_run", args);
    assert_eq!(r["status"], "failed", "{r}");
    assert_eq!(r["error"]["code"], "safety.budget_exceeded");
    assert_eq!(r["budgets"]["max_requests_count"], 60);
    assert!(r["path"].is_string(), "the partial run is saved: {r}");
    assert_eq!(r["report"]["run"]["interrupted"], true);
    assert_eq!(r["report"]["verdict"]["exit_code"], 4);
    // A call cannot raise the server budget, only lower it.
    let plan = m.tool(
        "plan_run",
        json!({"target": "memory://", "max_requests": 1000000, "max_duration": "3s"}),
    );
    assert_eq!(plan["budgets"]["max_requests_count"], 60);
    assert_eq!(plan["budgets"]["max_duration_s"], 3.0);
    assert!(!plan["budget_warnings"].as_array().unwrap().is_empty());

    // Not on the allowlist.
    let e = m.tool_err("start_run", small("fs:///tmp/oprobe-mcp-test"));
    assert_eq!(e["code"], "safety.refused");
    let plan = m.tool("plan_run", small("fs:///tmp/oprobe-mcp-test"));
    assert_eq!(plan["allowed"], false);
    assert_eq!(plan["refusals"][0]["code"], "safety.refused");

    // Cancellation: graceful, the partial run is saved.
    let mut m = Mcp::start(&[]);
    let mut args = small("memory://");
    args["duration"] = json!("60s");
    let started = m.tool("start_run", args);
    let id = started["run_id"].as_str().unwrap().to_owned();
    std::thread::sleep(Duration::from_millis(700));
    let c = m.tool("cancel_run", json!({"run_id": id}));
    assert_eq!(c["run_id"], id.as_str());
    let done = m.wait_run(&id);
    assert_eq!(done["status"], "cancelled", "{done}");
    let e = m.tool_err("cancel_run", json!({"run_id": "20000101-000000-0000"}));
    assert_eq!(e["code"], "usage.unknown_run");
}

/// Start `oprobe lab` on a free port; returns the child (callers wrap it in [`Kill`]) and its
/// endpoint.
#[allow(clippy::zombie_processes)]
fn lab() -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args(["lab", "--bind", "127.0.0.1:0", "-o", "ndjson"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let l = lines.next().expect("lab prints its endpoint").unwrap();
        let v: Value = serde_json::from_str(&l).unwrap();
        if v["type"] == "result" {
            let ep = v["data"]["endpoint"].as_str().unwrap().to_owned();
            // Keep draining stdout so the lab never blocks on a full pipe.
            std::thread::spawn(move || for _ in lines {});
            return (child, ep);
        }
    }
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn lab_target(endpoint: &str) -> Value {
    json!({
        "target": "s3://mcp-bucket/bench",
        "endpoint": endpoint,
        "region": "us-east-1",
        "proxy": "none",
        "preset": "small-files",
        "duration": "1s",
        "warmup": "0s",
        "objects": 8,
        "object_size": "16KiB",
        "concurrency": 4
    })
}

#[test]
fn read_only_server_refuses_writes_to_a_real_endpoint() {
    let (child, ep) = lab();
    let _lab = Kill(child);
    let mut m = Mcp::start(&[]);

    // Preflight works read-only (no write probe).
    let d = m.tool("doctor", json!({"target": "s3://mcp-bucket/bench", "endpoint": ep, "region": "us-east-1", "proxy": "none"}));
    assert_eq!(d["ready"], true, "{d}");
    assert!(
        d["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["status"] == "skipped")
    );

    // A run that would upload a dataset is refused…
    let plan = m.tool("plan_run", lab_target(&ep));
    assert_eq!(plan["allowed"], false);
    assert_eq!(plan["refusals"][0]["code"], "safety.read_only");
    let e = m.tool_err("start_run", lab_target(&ep));
    assert_eq!(e["code"], "safety.read_only");
    assert!(e["hint"].as_str().unwrap().contains("--allow-write"));
    let e = m.tool_err("start_tune", lab_target(&ep));
    assert_eq!(e["code"], "safety.read_only");
    // …a read-only benchmark of existing objects is not.
    let mut ro = lab_target(&ep);
    ro["existing"] = json!("data/");
    assert_eq!(m.tool("plan_run", ro)["allowed"], true);
    // Cleanup stays a dry run even when confirmed.
    let c = m.tool(
        "cleanup",
        json!({"target": "s3://mcp-bucket/bench", "endpoint": ep, "region": "us-east-1", "proxy": "none", "confirm": true}),
    );
    assert_eq!(c["dry_run"], true);

    // With --allow-write and a budget, the same run executes against the lab.
    let mut m = Mcp::start(&["--allow-write", "--max-duration", "30s"]);
    let mut args = lab_target(&ep);
    args["wait"] = json!(true);
    let r = m.tool("start_run", args);
    assert_eq!(r["status"], "succeeded", "{r}");
    assert!(r["report"]["summary"]["ops_count"].as_u64().unwrap() > 0);
    assert_eq!(r["report"]["target"]["scheme"], "s3");
    let id = r["run_id"].as_str().unwrap().to_owned();
    let ex = m.tool("explain_run", json!({"run": id}));
    assert!(
        ex["slowest"]["attribution"]["selected_count"]
            .as_u64()
            .unwrap()
            > 0
    );
}

/// `recommend_config { from }` of a read-only (`existing`) run keeps the run's `existing`
/// prefix: the child `fix` never generates a dataset on a read-only server.
#[test]
fn read_only_server_recommends_from_an_existing_run_without_writing() {
    let bucket = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(bucket.path().join("data")).unwrap();
    for i in 0..8 {
        std::fs::write(
            bucket.path().join("data").join(format!("obj-{i}")),
            vec![7u8; 16 * 1024],
        )
        .unwrap();
    }
    let url = format!("fs://{}", bucket.path().display());
    let listing = || -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(bucket.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let mut m = Mcp::start(&[]);
    let r = m.tool(
        "start_run",
        json!({
            "target": url,
            "existing": "data/",
            "preset": "small-files",
            "duration": "1s",
            "warmup": "0s",
            "concurrency": 2,
            "wait": true
        }),
    );
    assert_eq!(r["status"], "succeeded", "{r}");
    assert_eq!(r["report"]["spec"]["existing"], "data/", "{r}");
    let id = r["run_id"].as_str().unwrap().to_owned();
    assert_eq!(listing(), ["data"]);

    let (err, v, text) = m.tool_full(
        "recommend_config",
        json!({"from": id, "set": ["concurrency=3"], "budget": "30s"}),
        None,
    );
    // Nothing was written next to the existing objects, whatever the outcome…
    assert_eq!(listing(), ["data"], "recommend_config wrote: {text} {v}");
    // …and a recommendation measured the same read-only workload.
    if !err {
        let diag = v["diagnosis_run_id"].as_str().unwrap();
        let d = m.tool("get_run", json!({"run": diag}));
        assert_eq!(d["report"]["spec"]["existing"], "data/", "{d}");
        assert_eq!(
            d["report"]["spec"]["profile"]["knobs"]["concurrency"], 3,
            "`set` reaches the child: {d}"
        );
    } else {
        assert!(
            v["code"].as_str().unwrap().starts_with("safety."),
            "{text} {v}"
        );
    }
}

/// POST one JSON-RPC message to a Streamable HTTP endpoint; returns the session id and the
/// JSON-RPC response (from a JSON body or the SSE stream), if any.
async fn http_post(
    client: &reqwest::Client,
    url: &str,
    session: Option<&str>,
    msg: Value,
) -> (Option<String>, Option<Value>) {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18");
    if let Some(s) = session {
        req = req.header("Mcp-Session-Id", s);
    }
    let mut resp = req.body(msg.to_string()).send().await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_owned());
    if msg.get("id").is_none() {
        return (sid, None);
    }
    let mut buf = String::new();
    while let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_secs(60), resp.chunk())
        .await
        .unwrap()
    {
        buf += &String::from_utf8_lossy(&chunk);
        let found = buf
            .lines()
            .map(|l| l.strip_prefix("data:").unwrap_or(l).trim())
            .filter(|l| l.starts_with('{'))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("id") == msg.get("id"));
        if let Some(v) = found {
            return (sid, Some(v));
        }
    }
    let v = serde_json::from_str(buf.trim()).unwrap_or_else(|_| panic!("no response: {buf}"));
    (sid, Some(v))
}

#[tokio::test]
async fn streamable_http_transport() {
    use tokio::io::AsyncBufReadExt;
    let dir = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args(["mcp", "--http", "127.0.0.1:0", "-o", "ndjson"])
        .env("OPROBE_DATA_DIR", dir.path().join("data"))
        .env("OPROBE_CONFIG", dir.path().join("config.toml"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let url = loop {
        let l = lines
            .next_line()
            .await
            .unwrap()
            .expect("server prints its URL");
        let v: Value = serde_json::from_str(&l).unwrap();
        if v["type"] == "result" {
            break v["data"]["url"].as_str().unwrap().to_owned();
        }
    };
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (sid, init) = http_post(
        &client,
        &url,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "oprobe-test", "version": "0"}}}),
    )
    .await;
    assert_eq!(init.unwrap()["result"]["serverInfo"]["name"], "oprobe");
    let sid = sid.expect("a session id");
    http_post(
        &client,
        &url,
        Some(&sid),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    let (_, tools) = http_post(
        &client,
        &url,
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    assert_eq!(
        tools.unwrap()["result"]["tools"].as_array().unwrap().len(),
        18
    );
    let (_, r) = http_post(
        &client,
        &url,
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "start_run",
            "arguments": {"target": "memory://", "preset": "small-files", "duration": "300ms",
                          "warmup": "0s", "objects": 4, "object_size": "16KiB", "wait": true}}}),
    )
    .await;
    let r = r.unwrap();
    assert_eq!(
        r["result"]["structuredContent"]["status"], "succeeded",
        "{r}"
    );
    let other = client
        .get(url.replace("/mcp", "/elsewhere"))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 404);
}

/// Whether this build has the CLI subcommand `name` (stream T adds `verify` and `fix`).
fn has_subcommand(name: &str) -> bool {
    Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args([name, "--help"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Every `next` step has a CLI command without placeholders; those with an MCP call name a
/// tool this server has.
fn check_next(m: &Mcp, v: &Value, what: &str) {
    let next = v["next"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: no next: {v}"));
    assert!(!next.is_empty(), "{what}: empty next");
    for n in next {
        let cli = n["cli"].as_str().unwrap();
        assert!(cli.starts_with("oprobe "), "{what}: {n}");
        assert!(!cli.contains('<'), "{what}: placeholder in {cli}");
        if let Some(tool) = n["mcp"]["tool"].as_str() {
            assert!(m.schemas.contains_key(tool), "{what}: unknown tool {tool}");
            assert!(
                !n["mcp"]["arguments"].to_string().contains('<'),
                "{what}: {n}"
            );
        }
    }
    assert!(
        next.iter().any(|n| n["mcp"]["tool"].is_string()),
        "{what}: no MCP call in next: {next:?}"
    );
}

/// The agent workflow "find why my bucket is slow and fix the config" over MCP on a read-only
/// server (design v4 §9): discover → doctor → sample → plan → run → explain → verify /
/// recommend → replay with `from` + `set`, against the in-process `lab://demo`.
#[test]
fn read_only_slow_bucket_workflow_against_lab_demo() {
    let mut m = Mcp::start(&["--max-requests", "200000"]);

    // Extra arguments are refused (AG-6).
    let e = m.tool_err("plan_run", json!({"target": "memory://", "bogus": 1}));
    assert_eq!(e["code"], "usage.invalid_argument", "{e}");
    assert!(e["message"].as_str().unwrap().contains("bogus"), "{e}");
    let e = m.tool_err("plan_run", json!({"target": "memory://", "concurency": 4}));
    assert!(e["hint"].as_str().unwrap().contains("concurrency"), "{e}");

    // 1. Discover: credential sources by name, configured targets.
    let d = m.tool("discover_targets", json!({}));
    assert!(d["sources"].is_array(), "{d}");
    check_next(&m, &d, "discover_targets");

    // 2. Doctor: read-only by default, with a sample.
    let doc = m.tool("doctor", json!({"target": "lab://demo"}));
    assert_eq!(doc["ready"], true, "{doc}");
    assert_eq!(doc["write_probe"], false);
    assert_eq!(doc["permissions"]["write"], "skipped");
    assert!(doc["sample"].is_object(), "{doc}");
    check_next(&m, &doc, "doctor");
    let e = m.tool_err(
        "doctor",
        json!({"target": "s3://some-bucket/x", "endpoint": "http://127.0.0.1:9", "write_probe": true}),
    );
    assert_eq!(e["code"], "safety.read_only", "{e}");

    // 3. Sample: read-only listing.
    let s = m.tool("sample_objects", json!({"target": "lab://demo"}));
    assert!(s["objects_count"].is_u64(), "{s}");
    check_next(&m, &s, "sample_objects");

    // 4. Plan: lab:// writes only into the server's own process, so it is allowed read-only.
    let run_args = json!({
        "target": "lab://demo",
        "preset": "small-files",
        "objects": 16,
        "object_size": "16KiB",
        "duration": "2s",
        "warmup": "0s",
        "concurrency": 4
    });
    let plan = m.tool("plan_run", run_args.clone());
    assert_eq!(plan["allowed"], true, "{plan}");
    assert_eq!(plan["writes"], true);

    // 5. Run; `done` only together with the final event (AG-6).
    let (err, started, _) = m.tool_full(
        "start_run",
        run_args.clone(),
        Some(json!({"progressToken": "w"})),
    );
    assert!(!err, "{started}");
    let id = started["run_id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(120);
    let done = loop {
        let st = m.tool("get_run", json!({"run": id, "include_report": false}));
        let status = st["status"].as_str().unwrap().to_owned();
        if let Some(p) = st["progress"].as_object() {
            if p.get("phase").and_then(Value::as_str) == Some("done") {
                assert!(
                    !matches!(status.as_str(), "queued" | "running"),
                    "phase done while {status}: {st}"
                );
            }
            if let (Some(e), Some(t)) = (
                p.get("elapsed_s").and_then(Value::as_f64),
                p.get("total_s").and_then(Value::as_f64),
            ) {
                assert!(e <= t + 1e-9, "progress overshoots: {e} > {t}");
            }
        }
        if !matches!(status.as_str(), "queued" | "running") {
            break st;
        }
        assert!(Instant::now() < deadline, "run {id} did not finish");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(done["status"], "succeeded", "{done}");
    check_next(&m, &done, "get_run");
    let (events, _) = m.read(&format!("oprobe://runs/{id}/events"));
    let last: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "result", "{events}");
    let phases: Vec<String> = events
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "phase")
        .map(|e| e["phase"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        phases.last().map(String::as_str),
        Some("done"),
        "{phases:?}"
    );
    assert_eq!(
        phases.iter().filter(|p| *p == "done").count(),
        1,
        "{phases:?}"
    );
    let total = started["plan"]["eta_s"].as_f64().unwrap();
    for p in m
        .notes
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
    {
        let v = p["params"]["progress"].as_f64().unwrap();
        assert!(v <= total + 1e-9, "progress {v} past its total {total}");
    }

    // 6. Explain: next steps are MCP calls.
    let ex = m.tool("explain_run", json!({"run": id}));
    check_next(&m, &ex, "explain_run");

    // 7. Replay with a change: `from` + `set` (the CLI's `run --from RUN --set k=v`).
    let r2 = m.tool(
        "start_run",
        json!({"from": id, "set": ["concurrency=2"], "wait": true}),
    );
    assert_eq!(r2["status"], "succeeded", "{r2}");
    assert_eq!(
        r2["report"]["spec"]["profile"]["knobs"]["concurrency"], 2,
        "{r2}"
    );
    assert_eq!(r2["report"]["spec"]["target_ref"], "lab://demo");
    let e = m.tool_err("start_run", json!({"from": id, "set": ["concurency=2"]}));
    assert_eq!(e["code"], "usage.invalid_argument");

    // 8. Recommend (and verify) a configuration: `oprobe fix` / `oprobe verify` as children.
    if has_subcommand("verify") {
        let (err, v, text) = m.tool_full("verify_finding", json!({"run": id, "repeat": 1}), None);
        assert!(
            !err || v["code"] != "internal",
            "verify_finding: {text} {v}"
        );
    } else {
        eprintln!("skipping verify_finding: this build has no `oprobe verify` (stream T)");
    }
    if has_subcommand("fix") {
        let (err, v, text) = m.tool_full(
            "recommend_config",
            json!({"target": "lab://demo", "preset": "small-files", "objects": 16, "object_size": "16KiB", "budget": "1m"}),
            None,
        );
        assert!(!err, "recommend_config: {text} {v}");
    } else {
        eprintln!("skipping recommend_config: this build has no `oprobe fix` (stream T)");
    }
}
