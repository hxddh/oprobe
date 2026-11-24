//! The public API on a `memory://` operator: layer it, use it, read a snapshot, save a profile.

use opendal_core::Operator;
use opendal_core::services::Memory;

fn temp_file(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oprobe-trace-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("profile.oprobe")
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn a_memory_operator_reports_its_requests() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    assert!(handle.transport_error().is_none());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    for i in 0..10 {
        op.write(&format!("k{i}"), vec![7u8; 1024]).await.unwrap();
    }
    for i in 0..10 {
        assert_eq!(op.read(&format!("k{i}")).await.unwrap().len(), 1024);
    }
    op.stat("k0").await.unwrap();

    let snap = handle.snapshot();
    let count = |api: &str| {
        snap.apis
            .iter()
            .filter(|a| a.api.label() == api)
            .map(|a| a.count)
            .sum::<u64>()
    };
    assert_eq!(count("PutObject"), 10, "{:?}", snap.apis);
    assert_eq!(count("GetObject"), 10, "{:?}", snap.apis);
    assert_eq!(count("HeadObject"), 1, "{:?}", snap.apis);
    let down: u64 = snap.apis.iter().map(|a| a.bytes_down).sum();
    assert_eq!(down, 10 * 1024);

    // Every call is an operation with its request (design v7 B1).
    assert_eq!(snap.summary.ops, 21);
    let verbs: Vec<&str> = snap.verbs.iter().map(|v| v.verb.as_str()).collect();
    assert_eq!(verbs, ["read.full", "stat", "write"]);
    assert!(snap.verbs.iter().all(|v| v.requests_per_op == 1.0));
    assert_eq!(snap.bytes.logical_read, 10 * 1024);
    assert_eq!(snap.bytes.logical_write, 10 * 1024);

    // The profile holds the aggregates, every operation and every request, linked.
    let path = temp_file("requests");
    handle.save(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["format"], "oprobe.trace/8");
    assert_eq!(doc["oprobe_trace_version"], env!("CARGO_PKG_VERSION"));
    // The service the layer was applied to (no options, no credentials).
    assert_eq!(doc["service"]["scheme"], "memory");
    let reqs = doc["rows"]["reqs"].as_array().unwrap();
    assert_eq!(reqs.len(), 21);
    let ops = doc["rows"]["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 21);
    let ids: std::collections::BTreeSet<u64> =
        ops.iter().map(|o| o["op_id"].as_u64().unwrap()).collect();
    assert!(
        reqs.iter()
            .all(|r| ids.contains(&r["op_id"].as_u64().unwrap()))
    );
    // Keys are hashed, never recorded (docs/privacy.md).
    assert!(
        ops.iter()
            .all(|o| o["key"].as_str().unwrap().starts_with('#')),
        "{ops:#?}"
    );
    assert_eq!(doc["rows"]["sampling"]["seen_requests"], 21);
    let apis = doc["snapshot"]["apis"].as_array().unwrap();
    let total: u64 = apis
        .iter()
        .map(|a| a["requests_count"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 21);

    // Saving moved the rows out of memory: the next profile starts afresh.
    let path = temp_file("again");
    handle.save(&path).unwrap();
    let doc = read_json(&path);
    assert!(doc["rows"]["reqs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn memory_stays_bounded() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default().max_rows(64));
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("k", vec![1u8; 16]).await.unwrap();
    for _ in 0..2_000 {
        op.stat("k").await.unwrap();
    }
    let snap = handle.snapshot();
    let heads: u64 = snap
        .apis
        .iter()
        .filter(|a| a.api.label() == "HeadObject")
        .map(|a| a.count)
        .sum();
    // Aggregates are exact; rows are capped.
    assert_eq!(heads, 2_000);
    let path = temp_file("bounded");
    handle.save(&path).unwrap();
    let doc = read_json(&path);
    let kept = doc["rows"]["reqs"].as_array().unwrap().len();
    assert!(kept <= 64, "{kept}");
    assert_eq!(doc["rows"]["sampling"]["seen_requests"], 2_001);
}

#[tokio::test]
async fn a_snapshot_saves_without_rows() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("a", "x").await.unwrap();
    let path = temp_file("snapshot");
    handle.snapshot().save(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["format"], "oprobe.trace/8");
    assert!(doc.get("rows").is_none());
    assert_eq!(doc["snapshot"]["apis"][0]["api"], "PutObject");
}

/// Hourly files are hourly profiles: `save_and_reset` starts a fresh window.
#[tokio::test]
async fn save_and_reset_starts_a_fresh_window() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("a", "x").await.unwrap();
    for _ in 0..3 {
        op.stat("a").await.unwrap();
    }
    let path = temp_file("hour1");
    handle.save_and_reset(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["snapshot"]["summary"]["ops_count"], 4);
    // The next window holds only what happened since.
    op.stat("a").await.unwrap();
    let snap = handle.snapshot();
    assert_eq!(snap.summary.ops, 1);
    let requests: u64 = snap.apis.iter().map(|a| a.count).sum();
    assert_eq!(requests, 1);
    let path = temp_file("hour2");
    handle.save_and_reset(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["snapshot"]["summary"]["ops_count"], 1);
    assert_eq!(doc["rows"]["ops"].as_array().unwrap().len(), 1);
}

struct Count(std::sync::atomic::AtomicU64);

impl oprobe_trace::Exporter for Count {
    fn on_request(&self, _r: &oprobe_trace::RequestRecord) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::test]
async fn the_exporter_hook_sees_every_request() {
    let count = std::sync::Arc::new(Count(Default::default()));
    let (layer, _handle) =
        oprobe_trace::layer(oprobe_trace::Config::default().exporter(count.clone()));
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("a", "x").await.unwrap();
    op.read("a").await.unwrap();
    assert_eq!(count.0.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
fn an_invalid_proxy_is_reported_not_fatal() {
    let (_layer, handle) =
        oprobe_trace::layer(oprobe_trace::Config::default().proxy(Some("socks5://x:1")));
    let err = handle
        .transport_error()
        .expect("the transport must refuse the proxy");
    assert!(err.contains("http://"), "{err}");
}

#[tokio::test]
async fn in_process_copy_and_rename_record_requests() {
    let dir = std::env::temp_dir().join(format!("oprobe-trace-fs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(opendal_service_fs::Fs::default().root(dir.to_str().unwrap()))
        .unwrap()
        .layer(layer);
    op.write("src", vec![9u8; 512]).await.unwrap();
    op.copy("src", "dst").await.unwrap();
    op.rename("dst", "moved").await.unwrap();
    let snap = handle.snapshot();
    let copies: u64 = snap
        .apis
        .iter()
        .filter(|a| a.api.label() == "CopyObject")
        .map(|a| a.count)
        .sum();
    assert_eq!(copies, 2, "{:?}", snap.apis);
    let copy_verb = snap.verbs.iter().find(|v| v.verb == "copy").unwrap();
    assert_eq!(copy_verb.requests_per_op, 1.0);
    let rename_verb = snap.verbs.iter().find(|v| v.verb == "rename").unwrap();
    assert_eq!(rename_verb.requests_per_op, 1.0);
    let _ = std::fs::remove_dir_all(&dir);
}
