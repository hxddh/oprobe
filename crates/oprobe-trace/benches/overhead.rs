//! The cost of `oprobe-trace` per operation: the same operations on a plain `memory://`
//! operator and on one with the trace layer, interleaved, in one process.
//!
//! ```sh
//! cargo bench -p oprobe-trace --bench overhead              # ~10 s
//! cargo bench -p oprobe-trace --bench overhead -- --quick   # ~1 s (CI)
//! ```
//!
//! On `memory://` the backend call itself costs a few hundred nanoseconds, so the difference is
//! what the layer adds: wrapping the call, timing it, classifying it, and the recorder's
//! bookkeeping (aggregates, histograms, timeline, the bounded row sample). HTTP services go
//! through the same recorder from the instrumented `HttpFetch`; its own timing hooks are a few
//! `Instant::now()` calls per request, small next to a network round trip.
//!
//! The budget is 2 µs per operation (design v6 §4). The bench exits with status 1 when the
//! median overhead exceeds it.

use std::time::{Duration, Instant};

use opendal_core::Operator;
use opendal_core::services::Memory;

const BUDGET_US: f64 = 2.0;

struct Opts {
    rounds: usize,
    ops_per_round: usize,
}

fn opts() -> Opts {
    // `cargo bench` passes `--bench`; filters and other flags are ignored.
    if std::env::args().any(|a| a == "--quick") {
        Opts {
            rounds: 9,
            ops_per_round: 5_000,
        }
    } else {
        Opts {
            rounds: 31,
            ops_per_round: 50_000,
        }
    }
}

/// One round of `n` operations: alternately a `stat` and a 4 KiB `read`.
async fn round(op: &Operator, n: usize) -> Duration {
    let start = Instant::now();
    for i in 0..n {
        if i % 2 == 0 {
            std::hint::black_box(op.stat("obj").await.unwrap());
        } else {
            std::hint::black_box(op.read("obj").await.unwrap());
        }
    }
    start.elapsed()
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    // Test runners (cargo-nextest with `--all-targets`) list a harness-less bench with the
    // libtest `--list` protocol: report one benchmark (none when asked for ignored ones), so it
    // is listed and never run as a test.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            println!("overhead: benchmark");
        }
        return;
    }
    let o = opts();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let plain = Operator::new(Memory::default()).unwrap();
        let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
        let traced = Operator::new(Memory::default()).unwrap().layer(layer);
        for op in [&plain, &traced] {
            op.write("obj", vec![7u8; 4096]).await.unwrap();
            round(op, o.ops_per_round / 5).await; // warm up
        }

        let n = o.ops_per_round;
        let (mut plain_us, mut traced_us, mut over_us) = (Vec::new(), Vec::new(), Vec::new());
        for r in 0..o.rounds {
            // Alternate the order so drift (frequency scaling, allocator state) cancels out.
            let (p, t) = if r % 2 == 0 {
                let p = round(&plain, n).await;
                (p, round(&traced, n).await)
            } else {
                let t = round(&traced, n).await;
                (round(&plain, n).await, t)
            };
            let p = p.as_secs_f64() * 1e6 / n as f64;
            let t = t.as_secs_f64() * 1e6 / n as f64;
            plain_us.push(p);
            traced_us.push(t);
            over_us.push(t - p);
        }
        let recorded: u64 = handle.snapshot().apis.iter().map(|a| a.count).sum();
        let (p, t, d) = (
            median(&mut plain_us),
            median(&mut traced_us),
            median(&mut over_us),
        );
        let lo = over_us[over_us.len() / 10];
        let hi = over_us[over_us.len() - 1 - over_us.len() / 10];
        println!(
            "oprobe-trace overhead on memory:// ({} rounds × {n} ops)",
            o.rounds
        );
        println!("  plain OpenDAL      {p:8.3} µs/op");
        println!("  with oprobe-trace  {t:8.3} µs/op");
        println!(
            "  overhead           {d:8.3} µs/op  (p10 {lo:.3}, p90 {hi:.3}; budget {BUDGET_US} µs)"
        );
        println!("  requests recorded  {recorded}");
        assert!(
            recorded >= (o.rounds * n) as u64,
            "the layer did not record every operation"
        );
        if d > BUDGET_US {
            eprintln!("overhead {d:.3} µs/op is over the {BUDGET_US} µs budget");
            std::process::exit(1);
        }
    });
}
