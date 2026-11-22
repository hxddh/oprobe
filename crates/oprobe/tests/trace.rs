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
/// a retry layer above it, as `docs/embedding.md` says; `reads` passes over 30 objects.
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
                .with_max_times(6),
        );
    for i in 0..30 {
        op.write(&format!("k{i}"), vec![7u8; 4096]).await.unwrap();
    }
    for _ in 0..reads {
        for i in 0..30 {
            assert_eq!(op.read(&format!("k{i}")).await.unwrap().len(), 4096);
        }
    }
    for i in 0..10 {
        op.stat(&format!("k{i}")).await.unwrap();
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
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
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
    // Every request the application made is counted, and each stands in for an operation.
    let requests = r["connections"]["requests_count"].as_u64().unwrap();
    assert_eq!(requests, served, "the lab served every traced request");
    assert_eq!(r["summary"]["ops_count"].as_u64().unwrap(), requests);
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
        v["ops_count"], 0,
        "an embedder's trace has no operation rows"
    );
    assert!(r["summary"]["latency"]["p50_ms"].as_f64().unwrap() > 0.0);
    assert!(
        r["warnings"]
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
            oprobe::model::trace_file::TRACE_FINDINGS.contains(&code),
            "{code} needs operations or a varied load"
        );
        assert!(
            f["evidence"]["op_ids"]
                .as_array()
                .unwrap_or(&vec![])
                .is_empty()
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
