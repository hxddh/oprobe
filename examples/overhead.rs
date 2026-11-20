//! Observer-effect benchmark: the same workload through
//!   A. plain OpenDAL with its default reqwest client and the same timeout/retry layers (no oprobe),
//!   B. oprobe's default tracing (OpenDAL's reqwest client, observed),
//!   C. oprobe `--microscope` (oprobe's own instrumented pool),
//! against a local TLS lab server. Prints throughput, latency and client CPU per GiB.
//!
//! cargo run --release --example overhead [-- <seconds per round> <rounds>]
//!
//! Maintainers: OVERHEAD_DEBUG=1 MODES=plain,client,fbare,fnorec,mbare,traced,micro selects
//! bisection variants (configured client only, bare transports without layers).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use opendal::Operator;
use opendal_core::HttpTransporter;
use opendal_core::OperationContext;
use opendal_core::raw::{Layer, Servicer};
use opendal_http_transport_reqwest::ReqwestTransport;
use oprobe::config::{Knobs, TransportMode};
use oprobe::engine::operator::{base_operator, measured_operator};
use oprobe::engine::procstat::usage;
use oprobe::lab::{LabConfig, LabServer, lab_ca_pem};
use oprobe::trace::{OpCompletion, OpScope, Phase, Recorder};

/// Exactly what the `opendal` facade installs by default, with the lab CA trusted.
#[derive(Clone, Debug)]
struct PlainOpendal(HttpTransporter);

impl Layer for PlainOpendal {
    fn apply_context(&self, _: Servicer, ctx: OperationContext) -> OperationContext {
        ctx.with_http_transport(self.0.clone())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Plain,
    Traced,
    Microscope,
    /// Bisect: the fidelity transport's reqwest client, no oprobe wrapper or layers.
    ConfiguredClient,
    /// Bisect: the fidelity transport alone (no other layers, no scopes).
    FidelityBare,
    /// Bisect: the microscope transport alone.
    MicroscopeBare,
    /// Bisect: the fidelity transport without a recorder.
    FidelityNoRec,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Plain => "OpenDAL, no oprobe",
            Mode::Traced => "oprobe default",
            Mode::Microscope => "oprobe --microscope",
            Mode::ConfiguredClient => "[bisect] client only",
            Mode::FidelityBare => "[bisect] fidelity bare",
            Mode::MicroscopeBare => "[bisect] microscope bare",
            Mode::FidelityNoRec => "[bisect] fidelity no-rec",
        }
    }
}

struct Result {
    ops: f64,
    mib_s: f64,
    p50_ms: f64,
    p99_ms: f64,
    cpu_s_per_gib: f64,
}

async fn round(
    op: &Operator,
    rec: Option<&Arc<Recorder>>,
    objs: &[String],
    conc: usize,
    secs: f64,
) -> Result {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let u0 = usage().expect("process usage");
    let t0 = Instant::now();
    let lat = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    futures::stream::iter(0..conc)
        .for_each_concurrent(conc, |w| {
            let (op, lat, bytes) = (op.clone(), lat.clone(), bytes.clone());
            let rec = rec.cloned();
            async move {
                let mut i = w * 7919;
                while Instant::now() < deadline {
                    let path = &objs[i % objs.len()];
                    i += 1;
                    let s = Instant::now();
                    let n = match &rec {
                        None => op.read(path).await.expect("read").len(),
                        Some(rec) => {
                            // What oprobe's runner does around every operation.
                            let scope = OpScope::new(true);
                            rec.op_started(scope.id);
                            let n = scope.run(op.read(path)).await.expect("read").len();
                            rec.complete_op(
                                true,
                                OpCompletion {
                                    verb: "read.full",
                                    op_id: scope.id,
                                    started: s,
                                    latency_us: s.elapsed().as_micros() as u64,
                                    logical_read: n as u64,
                                    logical_write: 0,
                                    error: None,
                                    key: path.to_string(),
                                    data: scope.take(),
                                },
                            );
                            n
                        }
                    };
                    lat.lock().unwrap().push(s.elapsed().as_secs_f64() * 1000.0);
                    bytes.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                }
            }
        })
        .await;
    let wall = t0.elapsed().as_secs_f64();
    let cpu = usage().unwrap().cpu.saturating_sub(u0.cpu).as_secs_f64();
    let mut l = lat.lock().unwrap().clone();
    l.sort_by(|a, b| a.total_cmp(b));
    let q = |p: f64| l[((l.len() - 1) as f64 * p) as usize];
    let gib = bytes.load(std::sync::atomic::Ordering::Relaxed) as f64 / (1u64 << 30) as f64;
    Result {
        ops: l.len() as f64 / wall,
        mib_s: gib * 1024.0 / wall,
        p50_ms: q(0.5),
        p99_ms: q(0.99),
        cpu_s_per_gib: cpu / gib.max(1e-9),
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(4.0);
    let rounds: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let lab = LabServer::spawn("127.0.0.1:0", LabConfig::default()).await?;
    let profile = lab.profile("bench");
    let base = base_operator(&profile.target)?;
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(lab_ca_pem().as_bytes())?)
        .build()?;
    let plain_transport = HttpTransporter::new(ReqwestTransport::new(client));
    // The baseline carries the same client configuration oprobe applies for the default knobs
    // (timeouts, retries): those are what the user deploys, not observer cost.
    let plain = base
        .clone()
        .layer(PlainOpendal(plain_transport.clone()))
        .layer(
            opendal::layers::TimeoutLayer::new()
                .with_timeout(Duration::from_secs(60))
                .with_io_timeout(Duration::from_secs(30)),
        )
        .layer(
            opendal::layers::RetryLayer::default()
                .with_jitter()
                .with_min_delay(Duration::from_millis(50))
                .with_max_delay(Duration::from_secs(5))
                .with_max_times(3),
        );

    println!(
        "lab {} (TLS 1.3, HTTP/1.1), {secs}s x {rounds} rounds per cell, median shown\n",
        lab.endpoint_url
    );
    for (name, size, count, conc) in [
        ("small GET 16 KiB", 16 << 10, 256, 32),
        ("large GET 4 MiB", 4 << 20, 32, 16),
    ] {
        let objs: Vec<String> = (0..count).map(|i| format!("overhead/{size}/{i}")).collect();
        for o in &objs {
            plain.write(o, vec![7u8; size]).await?;
        }
        println!("{name}, {conc} concurrent");
        println!(
            "  {:22} {:>10} {:>10} {:>9} {:>9} {:>14}",
            "client", "ops/s", "MiB/s", "p50 ms", "p99 ms", "CPU s / GiB"
        );
        let mut base_row = None;
        let mut modes = vec![Mode::Plain, Mode::Traced, Mode::Microscope];
        if std::env::var("OVERHEAD_DEBUG").is_ok() {
            modes = std::env::var("MODES")
                .map(|m| {
                    m.split(',')
                        .map(|x| match x {
                            "plain" => Mode::Plain,
                            "traced" => Mode::Traced,
                            "micro" => Mode::Microscope,
                            "client" => Mode::ConfiguredClient,
                            "fbare" => Mode::FidelityBare,
                            "fnorec" => Mode::FidelityNoRec,
                            _ => Mode::MicroscopeBare,
                        })
                        .collect()
                })
                .unwrap_or(modes);
        }
        for mode in modes {
            let rec = Recorder::new();
            rec.set_phase(Phase::Measure);
            let op = match mode {
                Mode::Plain => plain.clone(),
                Mode::ConfiguredClient => {
                    let t = oprobe::net::FidelityTransport::new(
                        oprobe::engine::operator::transport_config(
                            &profile.target,
                            &Knobs::default(),
                        )?,
                        false,
                        None,
                    )?;
                    base.clone()
                        .layer(PlainOpendal(HttpTransporter::new(ReqwestTransport::new(
                            t.reqwest_client(),
                        ))))
                }
                Mode::FidelityBare => {
                    let t = oprobe::net::FidelityTransport::new(
                        oprobe::engine::operator::transport_config(
                            &profile.target,
                            &Knobs::default(),
                        )?,
                        false,
                        Some(rec.clone()),
                    )?;
                    base.clone().layer(PlainOpendal(HttpTransporter::new(t)))
                }
                Mode::FidelityNoRec => {
                    let t = oprobe::net::FidelityTransport::new(
                        oprobe::engine::operator::transport_config(
                            &profile.target,
                            &Knobs::default(),
                        )?,
                        false,
                        None,
                    )?;
                    base.clone().layer(PlainOpendal(HttpTransporter::new(t)))
                }
                Mode::MicroscopeBare => {
                    let t = oprobe::engine::operator::transport(
                        &profile.target,
                        &Knobs::default(),
                        Some(rec.clone()),
                    )?;
                    base.clone().layer(PlainOpendal(HttpTransporter::new(t)))
                }
                Mode::Traced | Mode::Microscope => {
                    let knobs = Knobs {
                        transport: if mode == Mode::Traced {
                            TransportMode::Opendal
                        } else {
                            TransportMode::Microscope
                        },
                        ..Knobs::default()
                    };
                    let reg = std::env::var("NO_TREE")
                        .is_err()
                        .then(|| await_tree::Registry::new(await_tree::Config::default()));
                    measured_operator(&base, &profile.target, &knobs, &rec, reg)?
                }
            };
            let r = matches!(mode, Mode::Traced | Mode::Microscope).then_some(&rec);
            round(&op, r, &objs, conc, 1.0).await; // warm connections
            let mut rs = Vec::new();
            for _ in 0..rounds {
                rs.push(round(&op, r, &objs, conc, secs).await);
            }
            if std::env::var("OVERHEAD_DEBUG").is_ok()
                && matches!(mode, Mode::Traced | Mode::Microscope)
            {
                let c = rec.snapshot().connections;
                eprintln!(
                    "    [{}] requests {} dialed {} reuse {:.1}%",
                    mode.label(),
                    c.requests,
                    c.dialed,
                    c.reuse_pct
                );
            }
            let ops = median(rs.iter().map(|x| x.ops).collect());
            let mib = median(rs.iter().map(|x| x.mib_s).collect());
            let p50 = median(rs.iter().map(|x| x.p50_ms).collect());
            let p99 = median(rs.iter().map(|x| x.p99_ms).collect());
            let cpu = median(rs.iter().map(|x| x.cpu_s_per_gib).collect());
            let delta = match base_row {
                None => {
                    base_row = Some(ops);
                    String::new()
                }
                Some(b) => format!("  ({:+.1}% ops/s)", (ops / b - 1.0) * 100.0),
            };
            println!(
                "  {:22} {ops:>10.0} {mib:>10.1} {p50:>9.2} {p99:>9.2} {cpu:>14.2}{delta}",
                mode.label()
            );
        }
        println!();
    }
    Ok(())
}
