//! Agent contract: a JSON-only workflow through the CLI (no text parsing) whose every output
//! validates against the published JSON Schemas, and the checked-in `schemas/` (plus the
//! finding-code section of docs/agents.md) staying in sync with the code.
//!
//! Regenerate the checked-in files with `OPROBE_BLESS=1 cargo test --test agent`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn bless() -> bool {
    std::env::var_os("OPROBE_BLESS").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Compare (or, when blessing, write) a generated file.
fn check_file(path: &Path, want: &str, stale: &mut Vec<String>) {
    let have = std::fs::read_to_string(path).unwrap_or_default();
    if have.replace("\r\n", "\n") == want {
        return;
    }
    if bless() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, want).unwrap();
    } else {
        stale.push(path.display().to_string());
    }
}

#[test]
fn checked_in_schemas_match_the_code() {
    let dir = root().join("schemas");
    let mut stale = Vec::new();
    let names = oprobe::mcp::schemas::names();
    for name in &names {
        let v = oprobe::mcp::schemas::schema(name).unwrap();
        let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
        check_file(&dir.join(format!("{name}.json")), &text, &mut stale);
    }
    // No schema files for names that no longer exist.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd {
            let p = e.unwrap().path();
            let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
            if p.extension().is_some_and(|x| x == "json") && !names.contains(&stem.as_str()) {
                if bless() {
                    std::fs::remove_file(&p).unwrap();
                } else {
                    stale.push(format!("{} (no such schema)", p.display()));
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "schemas/ is out of date: {stale:?}\nregenerate with `OPROBE_BLESS=1 cargo test --test agent`"
    );
}

/// Types whose field names are input names (the profile: config-file keys such as
/// `concurrency`, `objects`) or compact trace-row names of the run file (`OpRow`, `ReqRow`:
/// documented in docs/metrics.md), not unit-suffixed.
const INPUT_MODELS: &[&str] = &["Profile", "OpRow", "ReqRow", "TcpRow", "TlsRow"];
/// Output schemas whose field names are config-file names (`oprobe schema defaults`).
const CONFIG_NAMED: &[&str] = &["defaults"];

/// Design §6 and v4 §8: every numeric property of every registered output schema carries its
/// unit in its name, and every enum value is `snake_case` (error codes are dotted
/// `family.snake_case`). Input schemas (the config file, MCP inputs) are named after flags and
/// are checked for enum casing only.
#[test]
fn every_schema_has_unit_suffixes_and_snake_case_enums() {
    fn resolve<'a>(s: &'a Value, defs: &'a serde_json::Map<String, Value>) -> &'a Value {
        match s.get("$ref").and_then(Value::as_str) {
            Some(r) => match defs.get(r.rsplit('/').next().unwrap()) {
                Some(d) => resolve(d, defs),
                None => s,
            },
            None => s,
        }
    }
    fn numeric(s: &Value, defs: &serde_json::Map<String, Value>) -> bool {
        let s = resolve(s, defs);
        let types: Vec<&str> = match &s["type"] {
            Value::String(t) => vec![t.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        types.iter().any(|t| *t == "number" || *t == "integer")
            || ["anyOf", "oneOf", "allOf"].iter().any(|k| {
                s[*k]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| numeric(x, defs)))
            })
            || (types.contains(&"array") && numeric(&s["items"], defs))
    }
    fn snake(v: &str) -> bool {
        let body = v.split_once('.').map(|(f, _)| {
            // `family.snake_case` (error codes).
            f.chars().all(|c| c.is_ascii_lowercase()) && !f.is_empty()
        });
        let rest = v.split_once('.').map(|(_, r)| r).unwrap_or(v);
        body.unwrap_or(true)
            && !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }
    struct Walk<'a> {
        defs: &'a serde_json::Map<String, Value>,
        units: bool,
        seen: Vec<String>,
        bad_units: Vec<String>,
        bad_enums: Vec<String>,
    }
    impl Walk<'_> {
        fn walk(&mut self, s: &Value, path: &str) {
            if let Some(r) = s.get("$ref").and_then(Value::as_str) {
                let name = r.rsplit('/').next().unwrap().to_owned();
                // `report.spec.profile` is the profile (TOML) model: its keys are the config
                // file's input names (`concurrency`, `objects`, …), documented in
                // docs/workloads.md.
                if self.seen.contains(&name) || INPUT_MODELS.contains(&name.as_str()) {
                    return;
                }
                let Some(d) = self.defs.get(&name).cloned() else {
                    return;
                };
                self.seen.push(name);
                self.walk(&d, path);
                self.seen.pop();
                return;
            }
            for v in s["enum"].as_array().into_iter().flatten() {
                if let Some(v) = v.as_str()
                    && !snake(v)
                {
                    self.bad_enums.push(format!("{path}: `{v}`"));
                }
            }
            if let Some(v) = s["const"].as_str()
                && s["type"] == "string"
                && !snake(v)
            {
                self.bad_enums.push(format!("{path}: `{v}`"));
            }
            for k in ["anyOf", "oneOf", "allOf"] {
                for x in s[k].as_array().into_iter().flatten() {
                    self.walk(x, path);
                }
            }
            if s["items"].is_object() {
                self.walk(&s["items"], &format!("{path}[]"));
            }
            if s["additionalProperties"].is_object() {
                self.walk(&s["additionalProperties"], &format!("{path}.*"));
            }
            for (name, p) in s["properties"].as_object().into_iter().flatten() {
                if self.units && numeric(p, self.defs) && !oprobe::model::has_unit(name) {
                    self.bad_units.push(format!("{path}.{name}"));
                }
                self.walk(p, &format!("{path}.{name}"));
            }
        }
    }
    let mut bad_units = Vec::new();
    let mut bad_enums = Vec::new();
    for e in oprobe::mcp::schemas::entries() {
        // Config-named schemas use the config file's vocabulary (flag names, preset slugs
        // such as `small-files`).
        if CONFIG_NAMED.contains(&e.name) {
            continue;
        }
        let schema = oprobe::mcp::schemas::generate(&e);
        let defs = schema["$defs"].as_object().cloned().unwrap_or_default();
        let mut w = Walk {
            defs: &defs,
            units: e.direction == oprobe::mcp::schemas::Direction::Output,
            seen: Vec::new(),
            bad_units: Vec::new(),
            bad_enums: Vec::new(),
        };
        w.walk(&schema, e.name);
        bad_units.extend(w.bad_units);
        bad_enums.extend(w.bad_enums);
    }
    for v in [&mut bad_units, &mut bad_enums] {
        v.sort();
        v.dedup();
    }
    assert!(
        bad_units.is_empty(),
        "numeric fields without a unit suffix ({:?}): {bad_units:#?}",
        oprobe::model::UNIT_SUFFIXES
    );
    assert!(
        bad_enums.is_empty(),
        "enum values that are not snake_case: {bad_enums:#?}"
    );
}

#[test]
fn docs_list_every_finding_code() {
    let path = root().join("docs/agents.md");
    let text = std::fs::read_to_string(&path).unwrap();
    let begin = text
        .find("<!-- BEGIN finding-codes")
        .expect("docs/agents.md has a BEGIN finding-codes marker");
    let begin_end = begin + text[begin..].find("-->").unwrap() + 3;
    let end = text.find("<!-- END finding-codes -->").expect("END marker");
    let mut block = String::from("\n\n");
    for d in oprobe::analysis::catalog::all() {
        block += &format!(
            "<a id=\"finding-{}\"></a>\n### `{}` — {}\n\n{} *({}, usually {})*\n\n",
            d.code,
            d.code,
            d.title,
            d.meaning,
            d.category,
            d.typical_severity.label()
        );
        for (h, items) in [
            ("Causes", d.causes),
            ("Verify", d.verify),
            ("Knobs", d.knobs),
        ] {
            if !items.is_empty() {
                block += &format!("- **{h}:** {}\n", items.join("; "));
            }
        }
        block.push('\n');
    }
    let want = format!("{}{}{}", &text[..begin_end], block, &text[end..]);
    let mut stale = Vec::new();
    check_file(&path, &want, &mut stale);
    assert!(
        stale.is_empty(),
        "docs/agents.md finding codes are out of date; regenerate with `OPROBE_BLESS=1 cargo test --test agent`"
    );
}

// ---------------------------------------------------------------------------------------
// JSON workflow
// ---------------------------------------------------------------------------------------

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", self.dir.path().join("data"))
            .env("OPROBE_CONFIG", self.dir.path().join("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["--color", "never"])
            .args(args)
            .output()
            .unwrap()
    }

    /// Run with `-o json`, check the exit code, parse stdout and validate it against `schema`.
    fn json(&self, args: &[&str], schema: &str, exit: i32) -> Value {
        let mut a = args.to_vec();
        a.extend(["-o", "json"]);
        let o = self.run(&a);
        assert_eq!(
            o.status.code(),
            Some(exit),
            "{args:?}: stderr {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let v: Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?}: stdout is not one JSON document ({e}): {}",
                String::from_utf8_lossy(&o.stdout)
            )
        });
        validate(schema, &v);
        v
    }
}

fn validate(schema: &str, v: &Value) {
    let s = oprobe::mcp::schemas::schema(schema).unwrap_or_else(|| panic!("no schema {schema}"));
    let validator = jsonschema::validator_for(&s)
        .unwrap_or_else(|e| panic!("schema {schema} does not compile: {e}"));
    let errors: Vec<String> = validator
        .iter_errors(v)
        .take(10)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "output does not match schema `{schema}`:\n{}\n{}",
        errors.join("\n"),
        serde_json::to_string(v)
            .unwrap()
            .chars()
            .take(2000)
            .collect::<String>()
    );
}

const SMALL: &[&str] = &[
    "run",
    "memory://",
    "-d",
    "400ms",
    "--warmup",
    "0s",
    "--preset",
    "small-files",
    "--objects",
    "8",
    "--object-size",
    "16KiB",
    "-c",
    "4",
];

#[test]
fn json_only_agent_workflow_validates_against_the_schemas() {
    let env = Env::new();

    // 1. Self-description.
    let caps = env.json(&["capabilities"], "capabilities", 0);
    assert_eq!(caps["schema"], "oprobe.capabilities/1");
    let tools: Vec<&str> = caps["mcp"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools[0], "discover_targets");
    assert!(tools.contains(&"start_run") && tools.contains(&"compare_runs"));
    assert!(caps["error_codes"].as_array().unwrap().iter().any(|e| {
        e["code"] == "safety.budget_exceeded" && e["exit_code"] == 4 && e["retryable"] == false
    }));
    let list = env.json(&["schema"], "schema-list", 0);
    assert!(list["schemas"].as_array().unwrap().len() >= 20);
    let report_schema = env.run(&["schema", "report", "-o", "json"]);
    let rs: Value = serde_json::from_slice(&report_schema.stdout).unwrap();
    assert_eq!(
        rs["$id"],
        "https://github.com/hxddh/oprobe/schemas/5/report.json"
    );

    // 2. Preflight and plan.
    let doc = env.json(&["doctor", "memory://"], "doctor", 0);
    assert_eq!(doc["ready"], true);
    let mut plan_args = SMALL.to_vec();
    plan_args.push("--dry-run");
    let plan = env.json(&plan_args, "plan", 0);
    assert!(plan["plan"]["estimated_requests_count"].as_u64().unwrap() > 0);

    // 3. Two runs.
    let mut a = SMALL.to_vec();
    a.extend(["--label", "base"]);
    let r1 = env.json(&a, "run", 0);
    let id1 = r1["run_id"].as_str().unwrap().to_owned();
    let r2 = env.json(SMALL, "run", 0);
    let id2 = r2["run_id"].as_str().unwrap().to_owned();
    assert_ne!(id1, id2);

    // 4. History, explanation, comparison.
    let runs = env.json(&["runs", "list"], "runs-list", 0);
    assert_eq!(runs["total_count"], 2);
    assert_eq!(runs["runs"][0]["id"], id2.as_str());
    env.json(&["runs", "show", "base"], "runs-show", 0);
    let ex = env.json(&["explain", "last"], "explain", 0);
    assert_eq!(ex["kind"], "run");
    assert_eq!(ex["run_id"], id2.as_str());
    assert!(!ex["narrative"].as_array().unwrap().is_empty());
    assert!(
        ex["slowest"]["attribution"]["selected_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        ex["next"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["cli"].as_str().unwrap().contains(&id2) && c["mcp"]["tool"].is_string())
    );
    let code = env.json(&["explain", "throttling"], "explain", 0);
    assert_eq!(code["kind"], "finding_code");
    assert_eq!(code["code"], "throttling");
    let all = env.json(&["explain", "--list"], "explain", 0);
    assert!(all["codes"].as_array().unwrap().len() >= 15);
    // Codes are snake_case as reports emit them; `-` spellings are accepted too.
    for spelling in ["tail_stragglers", "tail-stragglers"] {
        let c = env.json(&["explain", spelling], "explain", 0);
        assert_eq!(c["code"], "tail_stragglers");
    }
    // capabilities.finding_codes is exactly the set the findings engine can emit.
    let mut advertised: Vec<&str> = caps["finding_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["code"].as_str().unwrap())
        .collect();
    advertised.sort_unstable();
    let mut emitted = oprobe::analysis::findings::CODES.to_vec();
    emitted.sort_unstable();
    assert_eq!(advertised, emitted);
    for f in r2["report"]["findings"].as_array().into_iter().flatten() {
        let c = f["code"].as_str().unwrap();
        assert!(emitted.contains(&c), "report emits undocumented code {c}");
    }
    let cmp = env.json(
        &[
            "compare",
            "base",
            "last",
            "--max-p99-increase",
            "10000",
            "--max-p50-increase",
            "10000",
            "--max-throughput-drop",
            "100",
            "--max-cost-increase",
            "10000",
        ],
        "compare",
        0,
    );
    assert!(cmp["passed"].is_boolean());

    // 5. Classified errors are JSON too.
    let err = env.json(&["explain", "no-such-run-or-code"], "error", 2);
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["code"], "usage.unknown_run");
    let err = env.json(&["schema", "nope"], "error", 2);
    assert_eq!(err["error"]["code"], "usage.invalid_argument");
    let mut ro = SMALL.to_vec();
    ro.push("--read-only");
    let err = env.json(&ro, "error", 5);
    assert_eq!(err["error"]["code"], "safety.read_only");
}

#[test]
fn ndjson_events_validate_line_by_line() {
    let env = Env::new();
    let mut a = SMALL.to_vec();
    a.extend(["-o", "ndjson"]);
    let o = env.run(&a);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8(o.stdout).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "version");
    assert_eq!(lines.last().unwrap()["type"], "result");
    for l in &lines {
        validate("events", l);
    }
    // The run file's blocks validate as run-file blocks (frames: kind u8, len u32 LE, JSON).
    let run_id = lines.last().unwrap()["run_id"].as_str().unwrap();
    let path = env.dir.path().join(format!("data/runs/{run_id}.oprobe"));
    let raw = zstd::stream::decode_all(std::fs::File::open(path).unwrap()).unwrap();
    let mut kinds = std::collections::BTreeSet::new();
    let mut at = 0;
    while at < raw.len() {
        let kind = raw[at];
        let len = u32::from_le_bytes(raw[at + 1..at + 5].try_into().unwrap()) as usize;
        let mut v: Value = serde_json::from_slice(&raw[at + 5..at + 5 + len]).unwrap();
        at += 5 + len;
        let name = match kind {
            1 => "header",
            2 => "segment",
            3 => "interval",
            4 => "report",
            other => panic!("unknown block kind {other}"),
        };
        kinds.insert(name);
        v.as_object_mut()
            .unwrap()
            .insert("kind".into(), Value::String(name.into()));
        validate("run-file", &v);
    }
    for k in ["header", "segment", "report"] {
        assert!(kinds.contains(k), "run file has no `{k}` block");
    }
}
