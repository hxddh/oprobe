//! A production trace (design v6 §2: one layer, one file): an application embeds the
//! `oprobe-trace` layer on its own operator, saves the profile, and `oprobe show FILE` opens it
//! like a run, honest about what it is; `oprobe prove FILE` refuses it.

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use opendal::layers::RetryLayer;
use oprobe::lab::{LabConfig, LabServer};
use serde_json::Value;

fn oprobe(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .env("OPROBE_DATA_DIR", dir.join("data"))
        .env("OPROBE_CONFIG", dir.join("config.toml"))
        .env_remove("OPROBE_OUTPUT")
        .args(["--color", "never"])
        .args(args)
        .output()
        .unwrap()
}

fn json(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
        panic!(
            "not one JSON document ({e}): {}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    })
}

fn validate(name: &str, v: &Value) {
    let schema = oprobe::cli::schema::schema(name).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{name}: {errors:#?}");
}

/// The application: an S3 operator on the lab with `faults`, the trace layer applied first and
/// a retry layer above it reporting to the trace, as `docs/embedding.md` says; `reads` passes
/// over 30 objects.
async fn traced_workload(
    path: &Path,
    faults: LabConfig,
    cfg: oprobe::trace::Config,
    reads: usize,
) -> u64 {
    let lab = LabServer::spawn("127.0.0.1:0", faults)
        .await
        .expect("lab starts");
    let opts = [
        ("bucket", "app-bucket"),
        ("root", "/prod"),
        ("region", "us-east-1"),
        ("endpoint", lab.endpoint_url.as_str()),
        ("access_key_id", "oprobe-lab"),
        ("secret_access_key", "oprobe-lab-secret"),
        ("disable_config_load", "true"),
    ];
    let (layer, handle) = oprobe::trace::layer(cfg.proxy(None));
    assert!(handle.transport_error().is_none());
    opendal::init_default_registry();
    let op = opendal::Operator::via_iter("s3", opts.map(|(k, v)| (k.to_owned(), v.to_owned())))
        .unwrap()
        .layer(layer)
        .layer(
            RetryLayer::default()
                .with_min_delay(Duration::from_millis(5))
                .with_max_delay(Duration::from_millis(20))
                .with_max_times(6)
                .with_notify(handle.retry_notify()),
        );
    for i in 0..30 {
        op.write(&format!("logs/k{i}"), vec![7u8; 4096])
            .await
            .unwrap();
    }
    for _ in 0..reads {
        for i in 0..30 {
            assert_eq!(op.read(&format!("logs/k{i}")).await.unwrap().len(), 4096);
        }
    }
    for i in 0..10 {
        op.stat(&format!("logs/k{i}")).await.unwrap();
    }
    handle.save(path).unwrap();
    lab.requests_served()
}

#[tokio::test(flavor = "multi_thread")]
async fn show_opens_a_production_trace_and_prove_refuses_it() {
    let dir = tempfile::tempdir().unwrap();
    // Not a `.json`: the file is recognised by its content.
    let file = dir.path().join("prod.oprobe");
    let throttle = LabConfig {
        throttle_every_n_requests: Some(5),
        throttle_status_code: 503,
        ..Default::default()
    };
    let served = traced_workload(&file, throttle, oprobe::trace::Config::default(), 2).await;
    let text = std::fs::read_to_string(&file).unwrap();
    // Keys are hashed (docs/privacy.md): the first path segment, never the key.
    assert!(!text.contains("logs/k1"), "a key is in the file");
    let doc: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["format"], "oprobe.trace/6");
    assert_eq!(doc["service"]["scheme"], "s3");
    assert_eq!(doc["service"]["name"], "app-bucket");
    let reqs_in_file = doc["rows"]["reqs"].as_array().unwrap().len() as u64;
    let f = file.to_str().unwrap();

    // JSON: a schema-valid report that says it is a trace.
    let o = oprobe(dir.path(), &["show", f, "-o", "json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v = json(&o);
    let r = &v["report"];
    validate("report", r);
    assert_eq!(r["run"]["source"], "trace");
    assert_eq!(r["run"]["label"], f);
    assert!(r["target"]["endpoint"].is_null(), "no endpoint is guessed");
    assert_eq!(r["workload"]["preset"], "");
    assert!(r["workload"]["mix"].as_array().unwrap().is_empty());
    assert_eq!(r["target"]["scheme"], "s3");
    assert_eq!(r["spec"]["operator_ref"], "s3://app-bucket/prod");
    // Every request the application made is counted, under the operation that issued it: 30
    // writes, 60 reads and 10 stats, each with its retries (design v7 B1).
    let requests = r["connections"]["requests_count"].as_u64().unwrap();
    assert_eq!(requests, served, "the lab served every traced request");
    assert_eq!(r["summary"]["ops_count"].as_u64().unwrap(), 100);
    assert!(requests > 100, "retried requests: {requests}");
    assert_eq!(r["summary"]["failed_ops_count"].as_u64().unwrap(), 0);
    let verbs: Vec<&str> = r["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["verb"].as_str().unwrap())
        .collect();
    for v in ["read.full", "write", "stat"] {
        assert!(verbs.contains(&v), "{v}: {verbs:?}");
    }
    // The retry layer reported its retries, attributed to their operations.
    let retries = r["resilience"]["retries_count"].as_u64().unwrap();
    assert_eq!(retries, requests - 100, "every extra request is a retry");
    assert!(r["resilience"]["retry_backoff_ms"].as_f64().unwrap() > 0.0);
    // Hot keys and prefixes from the operations, with hashed keys.
    let hot = r["dimensions"]["hot_keys"].as_array().unwrap();
    assert!(!hot.is_empty());
    assert!(
        hot.iter()
            .all(|k| k["key"].as_str().unwrap().starts_with("logs/#")),
        "{hot:#?}"
    );
    assert_eq!(r["dimensions"]["prefixes"][0]["key"], "logs/");
    let api_total: u64 = r["apis"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["requests_count"].as_u64().unwrap())
        .sum();
    assert_eq!(api_total, requests);
    assert_eq!(v["requests_count"].as_u64().unwrap(), reqs_in_file);
    assert_eq!(reqs_in_file, requests, "nothing was sampled away");
    assert_eq!(
        v["ops_count"], 100,
        "an embedder's trace has operation rows"
    );
    assert!(r["summary"]["latency"]["p50_ms"].as_f64().unwrap() > 0.0);
    // Requests are no longer counted as operations.
    assert!(
        !r["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().starts_with("production trace:")),
        "{:#}",
        r["warnings"]
    );
    // The lab throttled every 5th request: the trace says so, and the next step drives a run.
    let throttled = r["resilience"]["throttled_responses_count"]
        .as_u64()
        .unwrap();
    assert!(throttled >= requests / 6, "{throttled} of {requests}");
    let findings = r["findings"].as_array().unwrap();
    let thr = findings
        .iter()
        .find(|f| ["throttling", "rate_limit", "key_limit"].contains(&f["code"].as_str().unwrap()))
        .unwrap_or_else(|| panic!("a throttling finding: {findings:#?}"));
    assert_eq!(thr["remedy"]["next"], "oprobe run OPERATOR");
    for f in findings {
        let code = f["code"].as_str().unwrap();
        assert!(
            oprobe::model::trace_file::TRACE_FINDINGS.contains(&code)
                || oprobe::model::trace_file::OP_TRACE_FINDINGS.contains(&code),
            "{code} needs a varied load"
        );
    }

    // Text: the workload reads "production trace".
    let o = oprobe(dir.path(), &["show", f]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("production trace"), "{text}");
    assert!(!text.contains("small-files"), "{text}");

    // Prove needs a run oprobe drove.
    let o = oprobe(dir.path(), &["prove", f, "-o", "json"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let e = json(&o);
    validate("error", &e);
    assert_eq!(e["error"]["code"], "usage.invalid_argument");
    assert!(
        e["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("prove needs a run oprobe drove: `oprobe run OPERATOR`"),
        "{e:#}"
    );
    let o = oprobe(dir.path(), &["prove", f]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("oprobe run OPERATOR"));

    // Nothing was stored: a trace is opened, not imported.
    assert!(!dir.path().join("data").join("runs").exists());
}

/// Stragglers and connections dialled for every request show in a trace's findings too:
/// they read the request rows, not operations.
#[tokio::test(flavor = "multi_thread")]
async fn a_trace_shows_stragglers_and_cold_connections() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("stragglers.trace");
    let faults = LabConfig {
        straggler_every_n: 25,
        straggler_delay: Duration::from_millis(250),
        ..Default::default()
    };
    let cfg = oprobe::trace::Config::default().keep_alive(false);
    traced_workload(&file, faults, cfg, 5).await;
    let o = oprobe(dir.path(), &["show", file.to_str().unwrap(), "-o", "json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let r = &json(&o)["report"];
    validate("report", r);
    assert_eq!(r["run"]["source"], "trace");
    let codes: Vec<&str> = r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"tail_stragglers"), "{codes:?}");
    assert!(codes.contains(&"cold_connections"), "{codes:?}");
    assert!(!codes.contains(&"throttling"), "{codes:?}");
}

/// A hostile or corrupt trace file never panics `show FILE`: values out of range (a huge,
/// negative or absurd elapsed time, counters at `u64::MAX`, latencies that overflow) are refused
/// as `usage.invalid_argument` with a hint, or read with saturating arithmetic.
#[tokio::test(flavor = "multi_thread")]
async fn hostile_trace_values_never_panic_show() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.oprobe");
    traced_workload(
        &good,
        LabConfig::default(),
        oprobe::trace::Config::default(),
        1,
    )
    .await;
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&good).unwrap()).unwrap();
    let max = Value::from(u64::MAX);
    type Mutation = (&'static str, bool, Box<dyn Fn(&mut Value)>);
    let set_reqs = |field: &'static str, v: Value| -> Box<dyn Fn(&mut Value)> {
        Box::new(move |d: &mut Value| {
            for r in d["rows"]["reqs"].as_array_mut().unwrap() {
                r[field] = v.clone();
            }
        })
    };
    let set_apis = |field: &'static str, v: Value| -> Box<dyn Fn(&mut Value)> {
        Box::new(move |d: &mut Value| {
            for a in d["snapshot"]["apis"].as_array_mut().unwrap() {
                a[field] = v.clone();
            }
        })
    };
    // (what, must be refused, mutation)
    let cases: Vec<Mutation> = vec![
        (
            "huge elapsed",
            true,
            Box::new(|d| d["snapshot"]["elapsed_s"] = Value::from(1e300)),
        ),
        (
            "negative elapsed",
            true,
            Box::new(|d| d["snapshot"]["elapsed_s"] = Value::from(-5.0)),
        ),
        (
            "elapsed past u64 seconds",
            true,
            Box::new(|d| d["snapshot"]["elapsed_s"] = Value::from(1.9e19)),
        ),
        (
            "absurd latency",
            true,
            Box::new(|d| d["snapshot"]["summary"]["latency"]["p99_ms"] = Value::from(1e300)),
        ),
        (
            "negative latency",
            true,
            Box::new(|d| d["snapshot"]["apis"][0]["stages"]["total"]["p50_ms"] = Value::from(-3.0)),
        ),
        (
            "api counts at u64::MAX",
            false,
            set_apis("count", max.clone()),
        ),
        ("api bytes at u64::MAX", false, {
            let up = set_apis("bytes_up", max.clone());
            let down = set_apis("bytes_down", max.clone());
            Box::new(move |d| {
                up(d);
                down(d);
            })
        }),
        ("request bytes at u64::MAX", false, {
            let up = set_reqs("bytes_up", max.clone());
            let down = set_reqs("bytes_down", max.clone());
            Box::new(move |d| {
                up(d);
                down(d);
            })
        }),
        ("request times at u64::MAX", false, {
            let t = set_reqs("t_us", max.clone());
            let total = set_reqs("total_us", max.clone());
            Box::new(move |d| {
                t(d);
                total(d);
            })
        }),
        (
            "garbage created_at",
            false,
            Box::new(|d| d["created_at"] = Value::from("9999-99-99T99:99:99Z")),
        ),
    ];
    for (what, refused, mutate) in cases {
        let mut d = doc.clone();
        mutate(&mut d);
        let file = dir.path().join("hostile.oprobe");
        std::fs::write(&file, serde_json::to_vec(&d).unwrap()).unwrap();
        let o = oprobe(dir.path(), &["show", file.to_str().unwrap(), "-o", "json"]);
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(
            !stderr.contains("panicked"),
            "{what}: show panicked: {stderr}"
        );
        match o.status.code() {
            Some(0) if !refused => {
                json(&o);
            }
            Some(2) => {
                let e = json(&o);
                assert_eq!(e["error"]["code"], "usage.invalid_argument", "{what}: {e}");
                assert!(e["error"]["hint"].is_string(), "{what}: {e}");
            }
            other => panic!("{what}: exit {other:?}\n{stderr}"),
        }
        if refused {
            assert_eq!(o.status.code(), Some(2), "{what}: {stderr}");
        }
    }
}
