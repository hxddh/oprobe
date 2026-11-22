//! A coarse guard on the layer's cost, in every test run (debug build, shared CI machines): the
//! real measurement with its 2 µs budget is `cargo bench -p oprobe-trace --bench overhead`.

use std::time::Instant;

use opendal_core::Operator;
use opendal_core::services::Memory;

/// Per operation, unoptimized, on a busy machine: an order of magnitude above the budget, so the
/// test only catches a pathological regression (a lock per request, an unbounded buffer).
const GENEROUS_BOUND_US: f64 = 50.0;

async fn per_op_us(op: &Operator, n: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..n {
        op.stat("obj").await.unwrap();
    }
    start.elapsed().as_secs_f64() * 1e6 / n as f64
}

#[tokio::test(flavor = "current_thread")]
async fn the_layer_costs_little_per_operation() {
    let plain = Operator::new(Memory::default()).unwrap();
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let traced = Operator::new(Memory::default()).unwrap().layer(layer);
    for op in [&plain, &traced] {
        op.write("obj", vec![1u8; 64]).await.unwrap();
        per_op_us(op, 500).await;
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let p = per_op_us(&plain, 2_000).await;
        let t = per_op_us(&traced, 2_000).await;
        best = best.min(t - p);
    }
    let heads: u64 = handle
        .snapshot()
        .apis
        .iter()
        .filter(|a| a.api.label() == "HeadObject")
        .map(|a| a.count)
        .sum();
    assert_eq!(heads, 500 + 5 * 2_000);
    assert!(
        best < GENEROUS_BOUND_US,
        "oprobe-trace adds {best:.2} µs per operation (bound {GENEROUS_BOUND_US} µs)"
    );
}
