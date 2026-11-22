//! The public API on a `memory://` operator: layer it, use it, read a snapshot.

use opendal_core::Operator;
use opendal_core::services::Memory;

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

    // Requests outside any operation scope are kept as rows too (op id 0), in every shard.
    let rows = handle.take_rows();
    assert_eq!(rows.reqs.len(), 21);
    assert!(rows.reqs.iter().all(|r| r.op_id == 0));
    assert_eq!(rows.sampling.seen_requests, 21);
}

#[tokio::test]
async fn memory_stays_bounded() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config {
        max_rows: 64,
        ..Default::default()
    });
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
    let rows = handle.take_rows();
    assert!(rows.reqs.len() <= 64, "{}", rows.reqs.len());
    assert_eq!(rows.sampling.seen_requests, 2_001);
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
        oprobe_trace::layer(oprobe_trace::Config::default().with_exporter(count.clone()));
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("a", "x").await.unwrap();
    op.read("a").await.unwrap();
    assert_eq!(count.0.load(std::sync::atomic::Ordering::Relaxed), 2);
}
