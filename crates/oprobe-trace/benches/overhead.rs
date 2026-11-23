//! The cost of `oprobe-trace` per operation: the same operations on a plain `memory://`
//! operator and on one with the trace layer, interleaved, in one process.
//!
//! ```sh
//! cargo bench -p oprobe-trace --bench overhead              # ~10 s
//! cargo bench -p oprobe-trace --bench overhead -- --quick   # ~1 s (CI)
//! ```
//!
//! On `memory://` the backend call itself costs a few hundred nanoseconds, so the difference is
//! what the layer adds: wrapping the call in an operation, timing it, classifying it, and the
//! recorder's bookkeeping (aggregates, histograms, timeline, the bounded row sample).
//!
//! The HTTP line runs `stat` and 4 KiB reads against a local HTTP server (OpenDAL's `http`
//! service): OpenDAL's default reqwest client against the layer's instrumented one (DNS, dial,
//! TLS and body timing, kernel TCP statistics), so it also counts the transport's hooks. A
//! loopback round trip is tens of microseconds, so this line is noisier than `memory://`.
//!
//! The budget is 2 µs per operation (design v6 §4) for the layer's bookkeeping, measured on
//! `memory://`. The bench exits with status 1 when that median overhead exceeds it.

use std::time::{Duration, Instant};

use opendal_core::Operator;
use opendal_core::services::Memory;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BUDGET_US: f64 = 2.0;

struct Opts {
    rounds: usize,
    ops_per_round: usize,
    http_ops_per_round: usize,
}

fn opts() -> Opts {
    // `cargo bench` passes `--bench`; filters and other flags are ignored.
    if std::env::args().any(|a| a == "--quick") {
        Opts {
            rounds: 9,
            ops_per_round: 5_000,
            http_ops_per_round: 500,
        }
    } else {
        Opts {
            rounds: 31,
            ops_per_round: 50_000,
            http_ops_per_round: 4_000,
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

/// A minimal HTTP/1.1 server on loopback: every object is 4 KiB (HEAD answers its length, GET
/// its bytes), keep-alive. Returns its endpoint.
async fn http_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let _ = sock.set_nodelay(true);
            tokio::spawn(async move {
                let body = vec![7u8; 4096];
                let mut buf = Vec::with_capacity(4096);
                let mut chunk = [0u8; 4096];
                loop {
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                        continue;
                    };
                    let head = buf.starts_with(b"HEAD");
                    buf.drain(..end + 4);
                    let mut resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nlast-modified: Sun, 22 Nov 2026 10:00:00 GMT\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    if !head {
                        resp.extend_from_slice(&body);
                    }
                    if sock.write_all(&resp).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{addr}")
}

/// Plain and traced rounds of `n` operations on `plain` and `traced`, interleaved (alternating
/// the order so drift cancels out): per-op medians of plain, traced and their difference, and
/// the p10/p90 of the difference.
async fn compare(plain: &Operator, traced: &Operator, rounds: usize, n: usize) -> [f64; 5] {
    let (mut plain_us, mut traced_us, mut over_us) = (Vec::new(), Vec::new(), Vec::new());
    for r in 0..rounds {
        let (p, t) = if r % 2 == 0 {
            let p = round(plain, n).await;
            (p, round(traced, n).await)
        } else {
            let t = round(traced, n).await;
            (round(plain, n).await, t)
        };
        let p = p.as_secs_f64() * 1e6 / n as f64;
        let t = t.as_secs_f64() * 1e6 / n as f64;
        plain_us.push(p);
        traced_us.push(t);
        over_us.push(t - p);
    }
    let (p, t, d) = (
        median(&mut plain_us),
        median(&mut traced_us),
        median(&mut over_us),
    );
    let lo = over_us[over_us.len() / 10];
    let hi = over_us[over_us.len() - 1 - over_us.len() / 10];
    [p, t, d, lo, hi]
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
    // A test runner runs listed targets without `--bench`; the budget is meant for an
    // optimised `cargo bench` build (the CI overhead job), so do nothing here.
    if !args.iter().any(|a| a == "--bench") {
        println!("overhead: measured by `cargo bench -p oprobe-trace --bench overhead`");
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
        let [p, t, d, lo, hi] = compare(&plain, &traced, o.rounds, n).await;
        let snap = handle.snapshot();
        let recorded: u64 = snap.apis.iter().map(|a| a.count).sum();
        println!(
            "oprobe-trace overhead on memory:// ({} rounds × {n} ops)",
            o.rounds
        );
        println!("  plain OpenDAL      {p:8.3} µs/op");
        println!("  with oprobe-trace  {t:8.3} µs/op");
        println!(
            "  overhead           {d:8.3} µs/op  (p10 {lo:.3}, p90 {hi:.3}; budget {BUDGET_US} µs)"
        );
        println!("  operations recorded {}", snap.summary.ops);
        assert!(
            recorded >= (o.rounds * n) as u64 && snap.summary.ops >= (o.rounds * n) as u64,
            "the layer did not record every operation"
        );

        // The same against a local HTTP server: OpenDAL's client against the traced one.
        opendal_http_transport_reqwest::install_default();
        let endpoint = http_server().await;
        let http =
            || Operator::new(opendal_service_http::Http::default().endpoint(&endpoint)).unwrap();
        let (layer, handle) =
            oprobe_trace::layer(oprobe_trace::Config::default().https(false).proxy(None));
        let (plain_http, traced_http) = (http(), http().layer(layer));
        for op in [&plain_http, &traced_http] {
            round(op, o.http_ops_per_round / 5).await; // warm up: dial, pool
        }
        let n = o.http_ops_per_round;
        let [hp, ht, hd, hlo, hhi] = compare(&plain_http, &traced_http, o.rounds, n).await;
        let ops = handle.snapshot().summary.ops;
        println!(
            "oprobe-trace overhead over HTTP on loopback ({} rounds × {n} ops)",
            o.rounds
        );
        println!("  plain OpenDAL      {hp:8.3} µs/op");
        println!("  with oprobe-trace  {ht:8.3} µs/op");
        println!("  overhead           {hd:8.3} µs/op  (p10 {hlo:.3}, p90 {hhi:.3})");
        println!("  operations recorded {ops}");
        assert!(
            ops >= (o.rounds * n) as u64,
            "the layer did not record every HTTP operation"
        );
        if d > BUDGET_US {
            eprintln!("overhead {d:.3} µs/op is over the {BUDGET_US} µs budget");
            std::process::exit(1);
        }
    });
}
