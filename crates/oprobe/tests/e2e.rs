//! End-to-end tests against in-process backends and the local S3 lab over real sockets.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use oprobe::analysis::diagnosis::Bottleneck;
use oprobe::config::*;
use oprobe::engine::{Cancel, RunOptions, Session};
use oprobe::lab::{LabConfig, LabServer};
#[allow(unused_imports)]
use oprobe::operator::OperatorSpec;
use oprobe::report::Report;
use oprobe::trace::{Api, Snapshot};
use oprobe::util::ByteSize;

/// The oprobe binary with its config and run store in a per-process temporary directory
/// (tests never touch the user's files).
fn oprobe_cmd() -> Command {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| tempfile::tempdir().unwrap());
    let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
    c.env("OPROBE_DATA_DIR", home.path().join("data"))
        .env("OPROBE_CONFIG", home.path().join("config.toml"))
        .env_remove("OPROBE_OUTPUT");
    c
}

/// Build the Report v3 of a measured window (no trace rows).
fn build(prof: &Profile, s: Snapshot, c: oprobe::engine::CleanupSummary) -> Report {
    oprobe::model::build_report(oprobe::model::ReportInput {
        run_id: &oprobe::model::RunId::new(),
        target_name: None,
        profile: prof,
        knobs: &oprobe::config::Knobs::of(prof),
        snapshot: s,
        trace: Default::default(),
        cleanup: c,
        warnings: vec![],
        started_at: std::time::SystemTime::now(),
        ended_at: std::time::SystemTime::now(),
        spec: None,
    })
}

fn quiet() -> oprobe::engine::ProgressFn {
    Arc::new(|_| {})
}

fn opts(ms: u64) -> RunOptions {
    RunOptions {
        duration: Duration::from_millis(ms),
        warmup: Duration::from_millis(200),
        max_ops: None,
        rate: None,
        trace_slow_ops: true,
    }
}

async fn lab(cfg: LabConfig) -> oprobe::lab::LabHandle {
    LabServer::spawn(
        "127.0.0.1:0",
        LabConfig {
            throttle_status_code: 503,
            ..cfg
        },
    )
    .await
    .expect("lab starts")
}

fn small(p: &mut Profile, preset: Preset) {
    p.workload.preset = preset;
    let (mut ds, _) = preset.defaults();
    ds.objects = ds.objects.min(8);
    ds.object_size = ByteSize::kib(512);
    p.workload.dataset = Some(ds);
    p.workload.concurrency = 4;
    // The warmup `opts` runs with: a report judges its window against the profile's warmup.
    p.workload.warmup = oprobe::util::HumanDuration(Duration::from_millis(200));
}

async fn run_once(p: Profile, ms: u64) -> (Snapshot, oprobe::engine::CleanupSummary, Profile) {
    let s = Session::open(p, Cancel::new(), quiet())
        .await
        .expect("session opens");
    let knobs = oprobe::config::Knobs::of(&s.profile);
    let snap = s.run(&knobs, &opts(ms), None).await.expect("run succeeds");
    let c = s.close().await;
    (snap, c, s.profile.clone())
}

fn api(s: &Snapshot, a: Api) -> &oprobe::trace::ApiStats {
    s.apis.iter().find(|x| x.api == a).unwrap_or_else(|| {
        panic!(
            "no {a:?} in {:?}",
            s.apis.iter().map(|x| x.api).collect::<Vec<_>>()
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn every_preset_runs_and_cleans_up_on_memory_and_fs() {
    let dir = tempfile::tempdir().unwrap();
    for url in [
        "memory://".to_owned(),
        format!("fs://{}", dir.path().display()),
    ] {
        for preset in Preset::ALL {
            let mut p = Profile::from_url(&url);
            small(&mut p, preset);
            if preset == Preset::LargeObjects {
                p.workload.mix = vec![
                    OpSpec {
                        weight: 3,
                        op: OpKind::MultipartWrite {
                            size: ByteSize::mib(1),
                        },
                    },
                    OpSpec {
                        weight: 1,
                        op: OpKind::Read { size: None },
                    },
                ];
            }
            let (snap, cleanup, _) = run_once(p, 300).await;
            assert!(snap.summary.ops > 0, "{url} {preset:?}");
            assert_eq!(
                snap.summary.failed_ops, 0,
                "{url} {preset:?}: {:?}",
                snap.resilience.error_classes
            );
            assert_eq!(snap.integrity.corrupted, 0);
            assert!(cleanup.verified, "{url} {preset:?}: {cleanup:?}");
        }
    }
    // fs root contains no leftovers
    let left: Vec<_> = walk(dir.path());
    assert!(left.is_empty(), "leftover files: {left:?}");
}

fn walk(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.path());
            }
        }
    }
    out
}

async fn http1_requests_are_attributed_stage_by_stage_with() {
    let lab = lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(15),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    let (s, c, _) = run_once(p, 800).await;
    assert!(c.verified);
    assert_eq!(s.connections.http1_requests, s.connections.requests);
    let get = api(&s, Api::GetObject);
    // every request waited for the injected server latency, and TTFB captures it (not send/transfer)
    assert!(
        get.stages.ttfb.p50 >= 14.0,
        "ttfb p50 {}",
        get.stages.ttfb.p50
    );
    assert!(get.stages.send.p50 < 5.0);
    assert!(get.stages.total.p50 >= get.stages.ttfb.p50);
    // connections were dialed during warmup and reused in the window
    assert!(
        s.connections.reuse_pct > 95.0,
        "reuse {}",
        s.connections.reuse_pct
    );
    let put = api(&s, Api::PutObject);
    assert!(put.bytes_up > 0);
    // op breakdown sums to the latency mean
    for v in &s.verbs {
        let b = v.breakdown;
        let sum = b.queue_ms + b.cache_ms + b.backend_ms + b.client_ms;
        assert!(
            (sum - v.latency.mean).abs() < v.latency.mean * 0.05 + 0.05,
            "{}: {sum} vs {}",
            v.verb,
            v.latency.mean
        );
    }
    assert_eq!(
        s.bytes.request_amplification,
        s.connections.requests as f64 / s.summary.ops as f64
    );
}

async fn https_h2_multipart_upload_is_traced_per_api_with() {
    let lab = lab(LabConfig {
        enable_tls: true,
        enable_http2: true,
        multipart_merge_delay: Duration::from_millis(30),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    p.workload.dataset = Some(DatasetSpec {
        objects: 1,
        object_size: ByteSize::kib(64),
        ..Default::default()
    });
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::MultipartWrite {
            size: ByteSize::mib(12),
        },
    }];
    p.operator.write.chunk = Some(ByteSize::mib(5));
    p.workload.concurrency = 2;
    // OpenDAL's client speaks HTTP/1.1 unless the app enables reqwest's http2 feature.
    p.operator.http.version = oprobe::net::HttpVersion::H2;
    let (s, c, _) = run_once(p, 1200).await;
    assert!(c.verified);
    assert_eq!(s.connections.alpn, "h2");
    assert!(
        s.connections.tls_version.contains("1.3"),
        "{}",
        s.connections.tls_version
    );
    assert!(s.connections.http2_requests > 0);
    let complete = api(&s, Api::CompleteMultipart);
    assert!(
        complete.stages.ttfb.p50 >= 29.0,
        "complete ttfb {}",
        complete.stages.ttfb.p50
    );
    let parts = api(&s, Api::UploadPart);
    let ops = s.verb("write.multipart").unwrap().count;
    assert!(
        parts.count >= 3 * ops - 3,
        "{} parts for {ops} ops",
        parts.count
    );
    assert!(api(&s, Api::CreateMultipart).count >= ops);
    assert!(s.bytes.request_amplification >= 4.0);
    assert_eq!(s.integrity.corrupted, 0);
}

async fn keep_alive_off_dials_for_every_request_with() {
    let lab = lab(LabConfig::default()).await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.operator.http.keep_alive = false;
    let (s, _, _) = run_once(p, 500).await;
    assert!(s.connections.requests > 10);
    assert!(
        s.connections.reuse_pct < 1.0,
        "reuse {}",
        s.connections.reuse_pct
    );
    assert_eq!(s.connections.dialed, s.connections.requests);
    assert!(s.apis.iter().all(|a| a.stages.connect.count == a.count));
}

#[tokio::test(flavor = "multi_thread")]
async fn throttling_is_detected_and_diagnosed() {
    let lab = lab(LabConfig {
        throttle_every_n_requests: Some(4),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    let (s, c, prof) = run_once(p, 600).await;
    assert!(c.verified, "cleanup must bypass throttling: {c:?}");
    assert!(s.resilience.throttled_responses > 0);
    assert!(s.resilience.status_503 > 0);
    assert!(s.resilience.retries > 0);
    assert!(s.resilience.max_retry_after_ms >= 1000);
    let r = build(&prof, s, c);
    assert_eq!(r.verdict.bottleneck, Bottleneck::Throttling);
    assert_eq!(r.stages.bottleneck, Bottleneck::Throttling);
    assert!(r.findings.iter().any(|f| f.code == "throttling"));
    // Backoff between attempts is retry time, never "client" time (every retried op sleeps
    // ≥ 50 ms; the lab answers in well under a millisecond).
    let sh = &r.stages.shares;
    assert!(
        sh.retry_pct > 20.0 && sh.retry_pct > sh.client_pct,
        "retry backoff must be its own share: {sh:?}"
    );
    let retry_ms: f64 = r
        .verbs
        .iter()
        .map(|v| v.breakdown.retry_ms * v.count as f64)
        .sum();
    assert!(retry_ms > 0.0 && retry_ms <= r.resilience.retry_backoff_ms + 1.0);
    // The verdict line is the top finding's cause, and its remedy is a layer path.
    assert_eq!(r.verdict.headline, r.findings[0].cause);
    let f = r.findings.iter().find(|f| f.code == "throttling").unwrap();
    assert!(f.remedy.path.is_some(), "{f:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stragglers_show_up_in_slow_op_timelines_with_call_stacks() {
    let lab = lab(LabConfig {
        straggler_every_n: 10,
        straggler_delay: Duration::from_millis(150),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    let (s, _, _) = run_once(p, 1000).await;
    let slow = s.slow_ops.first().expect("slow ops captured");
    assert!(slow.latency_ms >= 150.0);
    let worst = slow
        .requests
        .iter()
        .map(|r| r.timing.ttfb_us)
        .max()
        .unwrap();
    assert!(
        worst >= 149_000,
        "straggler must be attributed to TTFB, got {worst}us"
    );
    assert!(
        s.slow_ops.iter().any(|o| o.await_tree.is_some()),
        "await-tree stacks captured"
    );
    assert!(slow.requests.iter().all(|r| r.request_id.is_some()));
}

#[tokio::test(flavor = "multi_thread")]
async fn open_loop_rate_is_respected() {
    let mut p = Profile::from_url("memory://");
    small(&mut p, Preset::SmallFiles);
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1500);
    o.rate = Some(200.0);
    let snap = s
        .run(&oprobe::config::Knobs::of(&s.profile), &o, None)
        .await
        .unwrap();
    s.close().await;
    let ol = snap.open_loop.expect("open-loop stats");
    assert!(
        (ol.achieved_rate - 200.0).abs() < 30.0,
        "achieved {}",
        ol.achieved_rate
    );
    assert!(
        (snap.summary.ops_per_s - 200.0).abs() < 40.0,
        "ops/s {}",
        snap.summary.ops_per_s
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn inflight_cap_shows_up_as_queue_time() {
    let lab = lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(10),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.workload.concurrency = 16;
    p.operator.set("layers.concurrent_limit.http", "2").unwrap();
    let (s, _, _) = run_once(p, 800).await;
    assert!(s.connections.peak_inflight_requests >= 2);
    let q = s.request_stage_means().queue;
    assert!(q > 20.0, "mean queue {q}ms");
    let shares = oprobe::analysis::diagnosis::time_shares(&s);
    assert!(shares.queue > 0.5, "queue share {}", shares.queue);
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_hits_reduce_backend_bytes() {
    let lab = lab(LabConfig::default()).await;
    let mut p = lab.profile("bkt");
    p.workload.dataset = Some(DatasetSpec {
        objects: 4,
        object_size: ByteSize::kib(256),
        ..Default::default()
    });
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::Read { size: None },
    }];
    p.workload.access = Access::Hotset {
        fraction: 0.25,
        probability: 0.9,
    };
    p.operator.set("layers.cache.memory", "64MiB").unwrap();
    let (s, _, _) = run_once(p, 600).await;
    assert!(
        s.bytes.read_amplification < 0.5,
        "read amp {}",
        s.bytes.read_amplification
    );
    assert!(s.bytes.request_amplification < 0.5);
}

/// Minimal HTTP CONNECT proxy for tests.
async fn connect_proxy() -> (String, Arc<std::sync::atomic::AtomicU64>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let tunnels = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let t2 = tunnels.clone();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = l.accept().await.unwrap();
            let t3 = t2.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if c.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let target = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .replace("localhost", "127.0.0.1");
                let Ok(mut up) = tokio::net::TcpStream::connect(&target).await else {
                    return;
                };
                t3.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
            });
        }
    });
    (format!("http://{addr}"), tunnels)
}

async fn proxy_connect_is_used_and_timed_with() {
    let lab = lab(LabConfig {
        enable_tls: true,
        enable_http2: true,
        ..Default::default()
    })
    .await;
    let (proxy, tunnels) = connect_proxy().await;
    let mut p = lab.profile("bkt");
    p.operator.http.proxy = Some(proxy.clone());
    small(&mut p, Preset::SmallFiles);
    p.operator.http.keep_alive = false;
    let (s, c, _) = run_once(p, 400).await;
    assert!(c.verified);
    assert!(tunnels.load(std::sync::atomic::Ordering::Relaxed) > 5);
    assert_eq!(s.connections.proxy.as_deref(), Some(proxy.as_str()));
    assert!(s.connections.proxy_connect.count > 0);
    assert!(s.connections.tls.count > 0);
}
#[tokio::test(flavor = "multi_thread")]
async fn report_roundtrips_through_json() {
    let mut p = Profile::from_url("memory://");
    small(&mut p, Preset::SmallFiles);
    let (s, c, prof) = run_once(p, 400).await;
    let r = build(&prof, s, c);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    oprobe::report::write_file(&path, &oprobe::report::to_json(&r)).unwrap();
    let back = oprobe::report::load_json(&path).unwrap();
    assert_eq!(back, r);
    // Another version's schema is refused, never misread.
    let mut v = serde_json::to_value(&r).unwrap();
    v["schema"] = "oprobe.report/6".into();
    std::fs::write(&path, v.to_string()).unwrap();
    assert!(oprobe::report::load_json(&path).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn kept_data_is_swept_by_cleanup() {
    let lab = lab(LabConfig::default()).await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.workload.cleanup = false;
    let (_, c, prof) = run_once(p, 300).await;
    assert!(!c.enabled);
    assert!(lab.stored_object_count() > 0);
    let t = prof.operator.target().unwrap();
    let base = oprobe::engine::operator::base_operator(&t).unwrap();
    let admin = oprobe::engine::operator::admin_operator(&base, &t).unwrap();
    let dry = oprobe::engine::cleanup::sweep(&admin, Duration::ZERO, false, true)
        .await
        .unwrap();
    assert_eq!(dry.len(), 1);
    assert!(lab.stored_object_count() > 0, "dry run deletes nothing");
    let done = oprobe::engine::cleanup::sweep(&admin, Duration::ZERO, false, false)
        .await
        .unwrap();
    assert!(done[0].deleted);
    assert_eq!(lab.stored_object_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn http1_requests_are_attributed_stage_by_stage() {
    http1_requests_are_attributed_stage_by_stage_with().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn https_h2_multipart_upload_is_traced_per_api() {
    https_h2_multipart_upload_is_traced_per_api_with().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_alive_off_dials_for_every_request() {
    keep_alive_off_dials_for_every_request_with().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_connect_is_used_and_timed() {
    proxy_connect_is_used_and_timed_with().await;
}

/// Gateways answer 503 and hang up while the client is still uploading; the client sees a reset
/// or abort (always on Windows) and must retry rather than fail the operation or the seeding.
async fn early_close_throttling_is_retried_with() {
    let lab = lab(LabConfig {
        throttle_every_n_requests: Some(5),
        throttle_early_close: true,
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.operator.set("layers.retry.max_times", "6").unwrap();
    p.operator.set("layers.retry.min_delay", "50ms").unwrap();
    p.operator.set("layers.retry.jitter", "on").unwrap();
    let (s, c, _) = run_once(p, 600).await;
    assert!(c.verified, "{c:?}");
    assert!(s.summary.ops > 0);
    assert_eq!(s.summary.failed_ops, 0, "{:?}", s.verbs);
    assert!(s.resilience.retries > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn early_close_throttling_is_retried() {
    early_close_throttling_is_retried_with().await;
}

/// Requests abandoned by the operation timeout must release their in-flight slot: a leak would
/// make the in-flight count (and its peak) grow with every timeout.
async fn timeouts_do_not_leak_inflight_requests_with() {
    let lab = lab(LabConfig {
        straggler_every_n: 2,
        straggler_delay: Duration::from_millis(800),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.workload.concurrency = 2;
    p.operator.set("layers.retry", "off").unwrap();
    p.operator.set("layers.timeout.op", "60ms").unwrap();
    p.operator.set("layers.timeout.io", "60ms").unwrap();
    let (s, _, _) = run_once(p, 1500).await;
    assert!(
        s.summary.failed_ops > 3,
        "expected timeouts, got {:?}",
        s.verbs
    );
    assert!(
        s.connections.peak_inflight_requests <= 4,
        "in-flight requests leaked: peak {}",
        s.connections.peak_inflight_requests
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn timeouts_do_not_leak_inflight_requests() {
    timeouts_do_not_leak_inflight_requests_with().await;
}

/// An open-loop arrival interval longer than the window must not run past it.
#[tokio::test(flavor = "multi_thread")]
async fn open_loop_never_runs_past_the_window() {
    let mut p = Profile::from_url("memory://");
    small(&mut p, Preset::SmallFiles);
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1000);
    o.warmup = Duration::ZERO;
    o.rate = Some(0.2); // one arrival every 5 s
    let t = std::time::Instant::now();
    let snap = s
        .run(&oprobe::config::Knobs::of(&s.profile), &o, None)
        .await
        .unwrap();
    s.close().await;
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "took {:?}",
        t.elapsed()
    );
    assert!(snap.summary.ops <= 1, "issued {} ops", snap.summary.ops);
}

/// The window ends at the deadline: draining slow in-flight operations must not stretch it
/// (which would understate ops/s and MiB/s), yet their latencies are still recorded.
#[tokio::test(flavor = "multi_thread")]
async fn drain_does_not_stretch_the_measurement_window() {
    let lab = lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(300),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.workload.concurrency = 4;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1000);
    o.warmup = Duration::ZERO;
    let snap = s
        .run(&oprobe::config::Knobs::of(&s.profile), &o, None)
        .await
        .unwrap();
    let rows = s.take_trace().expect("the window's rows").ops;
    s.close().await;
    // Asserted on the rows, not on the wall clock (which a slow machine stretches): each
    // row has its start and its latency, in µs from the window start.
    let window_us = (snap.elapsed_s * 1e6) as i64;
    let end_us = |r: &oprobe::trace::OpRow| r.t_us + r.latency_us as i64;
    // The workers are in an operation when the window ends (they never idle, and an operation
    // takes 300 ms at least): those operations drain after it...
    let drained: Vec<_> = rows.iter().filter(|r| end_us(r) > window_us).collect();
    assert!(
        (1..=4).contains(&drained.len()),
        "window {window_us} µs: {rows:?}"
    );
    // ... the window does not wait for them (it would end with the last of them) ...
    let last_end = rows.iter().map(end_us).max().unwrap();
    assert!(
        window_us < last_end,
        "the window ({window_us} µs) includes the drain (until {last_end} µs)"
    );
    // ... they started inside it, and they are recorded with their whole latency.
    assert!(rows.iter().all(|r| r.t_us <= window_us), "{rows:?}");
    assert!(
        drained.iter().all(|r| r.latency_us >= 300_000),
        "{drained:?}"
    );
    assert_eq!(rows.len() as u64, snap.summary.ops);
    assert!(snap.summary.ops > 0);
    let rate = snap.summary.ops as f64 / snap.elapsed_s;
    assert!(
        (snap.summary.ops_per_s - rate).abs() < 1e-6,
        "{} vs {rate}",
        snap.summary.ops_per_s
    );
    assert!(!snap.interrupted);
}

/// Closed-loop `max_ops` is exact under concurrency.
#[tokio::test(flavor = "multi_thread")]
async fn closed_loop_max_ops_is_exact() {
    let mut p = Profile::from_url("memory://");
    small(&mut p, Preset::SmallFiles);
    p.workload.concurrency = 16;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(10_000);
    o.warmup = Duration::ZERO;
    o.max_ops = Some(101);
    let snap = s
        .run(&oprobe::config::Knobs::of(&s.profile), &o, None)
        .await
        .unwrap();
    s.close().await;
    assert_eq!(snap.summary.ops, 101);
}

/// An overloaded open-loop run queues arrivals: latency grows from the intended start
/// (coordinated omission avoided), arrivals still queued at the end are `missed`, and
/// concurrent writes never share a key (no false integrity failures).
#[tokio::test(flavor = "multi_thread")]
async fn open_loop_saturation_shows_in_latency() {
    let lab = lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(40),
        straggler_every_n: 7,
        straggler_delay: Duration::from_millis(120),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::Write {
            size: ByteSize::kib(16),
        },
    }];
    p.workload.concurrency = 2;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1500);
    o.warmup = Duration::ZERO;
    o.rate = Some(200.0); // far above the ~40 ops/s two slots can serve
    let snap = s
        .run(&oprobe::config::Knobs::of(&s.profile), &o, None)
        .await
        .unwrap();
    let (_, bad) = s.verify_writes(None).await;
    s.close().await;
    let ol = snap.open_loop.clone().expect("open-loop stats");
    assert!(ol.missed > 0, "{ol:?}");
    assert!(ol.achieved_rate < 100.0, "{ol:?}");
    // Queueing delay is part of latency: far above the ~40 ms service time.
    assert!(
        snap.summary.latency.p50 > 200.0,
        "p50 {} must include queueing",
        snap.summary.latency.p50
    );
    assert_eq!(snap.integrity.corrupted, 0);
    assert_eq!(bad, 0, "concurrent writes raced on a slot");
}

/// A read-only dataset whose remaining operations all have weight 0 is a clear error, not a
/// worker panic.
#[tokio::test(flavor = "multi_thread")]
async fn existing_prefix_with_only_weightless_reads_is_rejected() {
    let mut p = Profile::from_url("memory://");
    p.workload.dataset = Some(DatasetSpec {
        existing_prefix: Some("data/".into()),
        ..Default::default()
    });
    p.workload.mix = vec![
        OpSpec {
            weight: 1,
            op: OpKind::Write {
                size: ByteSize::kib(4),
            },
        },
        OpSpec {
            weight: 0,
            op: OpKind::Read { size: None },
        },
    ];
    let err = Session::open(p, Cancel::new(), quiet())
        .await
        .err()
        .expect("must fail");
    assert!(format!("{err:#}").contains("weight"), "{err:#}");
}

/// Cancelling during the dataset upload stops it and removes what was uploaded.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_prepare_leaves_no_test_data() {
    let lab = lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(60),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    p.workload.dataset = Some(DatasetSpec {
        objects: 200,
        object_size: ByteSize::kib(64),
        ..Default::default()
    });
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::Read { size: None },
    }];
    let cancel = Cancel::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c2.cancel();
    });
    let t = std::time::Instant::now();
    let res = Session::open(p, cancel, quiet()).await;
    assert!(res.is_err(), "open must report the cancellation");
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    assert_eq!(lab.stored_object_count(), 0, "partial dataset left behind");
}

/// An endpoint with nothing listening fails fast instead of retrying uploads for minutes.
#[tokio::test(flavor = "multi_thread")]
async fn connection_refused_fails_fast() {
    let lab = lab(LabConfig::default()).await;
    let mut p = lab.profile("bkt");
    let port = REFUSING_PORT;
    p.operator
        .options
        .insert("endpoint".into(), format!("http://127.0.0.1:{port}"));
    small(&mut p, Preset::SmallFiles);
    // Fast: the reachability probe fails once, where the dataset upload would retry for
    // minutes (the error below is the probe's; the timeout only keeps a regression from
    // hanging the suite).
    let opened = Session::open(p, Cancel::new(), quiet());
    let err = tokio::time::timeout(Duration::from_secs(120), opened)
        .await
        .expect("it does not retry for minutes")
        .err()
        .expect("must fail");
    let msg = format!("{err:#}");
    // One plain cause; OpenDAL's report is the detail (U2).
    assert_eq!(
        msg,
        format!("connection refused by 127.0.0.1:{port} (s3://bkt/bench)")
    );
    let body = oprobe::errors::classify(&err, "s3");
    let detail = body.detail.expect("the storage library's report");
    // "Connection refused" on Unix, "actively refused" on Windows.
    assert!(detail.to_lowercase().contains("refused"), "{detail}");
    assert!(
        !detail.contains("error sending request: error sending request"),
        "no text twice: {detail}"
    );
    let hint = oprobe::analysis::hints::hint_for(&msg, "s3").expect("hint");
    assert!(hint.title.contains("refused"), "{}", hint.title);
}

/// A local port that refuses connections on every platform: 1 (tcpmux), privileged and
/// served by nothing on a test machine. A socket bound without `listen()` refuses on Linux
/// but lets the connection time out on macOS, and a port taken from a dropped listener can
/// be handed to another test.
const REFUSING_PORT: u16 = 1;

/// An `http.proxy` that cannot be reached is named: 9.2 said the endpoint had refused the
/// connection, and hinted at `options.endpoint`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_proxy_is_reported_as_the_proxy() {
    let lab = lab(LabConfig::default()).await;
    let mut p = lab.profile("bkt");
    let port = REFUSING_PORT;
    p.operator.http.proxy = Some(format!("http://127.0.0.1:{port}"));
    small(&mut p, Preset::SmallFiles);
    let opened = Session::open(p, Cancel::new(), quiet());
    let err = tokio::time::timeout(Duration::from_secs(120), opened)
        .await
        .expect("it does not retry for minutes")
        .err()
        .expect("must fail");
    let body = oprobe::errors::classify(&err, "s3");
    assert_eq!(body.code, oprobe::errors::ErrorCode::NetUnreachable);
    assert_eq!(
        body.message,
        format!(
            "connection refused by the proxy 127.0.0.1:{port} (s3://bkt/bench, proxy set by \
             `http.proxy`)"
        )
    );
    let hint = body.hint.expect("hint");
    assert!(hint.contains("http.proxy"), "{hint}");
    assert!(!hint.contains("options.endpoint"), "{hint}");
    assert_eq!(lab.requests_served(), 0, "nothing reached the endpoint");
}

/// A run stopped with Cancel is flagged `interrupted` (and partial) in the snapshot and report.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_run_is_marked_interrupted() {
    let mut p = Profile::from_url("memory://");
    small(&mut p, Preset::SmallFiles);
    let cancel = Cancel::new();
    let s = Session::open(p, cancel.clone(), quiet()).await.unwrap();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        c2.cancel();
    });
    let mut o = opts(30_000);
    o.warmup = Duration::ZERO;
    let snap = s
        .run(&oprobe::config::Knobs::of(&s.profile), &o, None)
        .await
        .unwrap();
    let c = s.close().await;
    assert!(snap.interrupted);
    assert!(snap.elapsed_s < 5.0, "{}", snap.elapsed_s);
    let prof = s.profile.clone();
    let r = build(&prof, snap, c);
    assert!(r.is_partial());
    assert_eq!(r.verdict.exit_code, 130);
    let json: serde_json::Value = serde_json::to_value(&r).unwrap();
    assert!(json["run"].get("interrupted").is_none());
    assert_eq!(json["run"]["partial"]["reason"], "interrupted");
    assert_eq!(
        r.run.partial.as_ref().map(|p| p.reason),
        Some(oprobe::model::PartialReason::Interrupted)
    );
}

// ---------------------------------------------------------------------------------------
// Interactive entry points and argument validation (UI/CLI review fixes).
// ---------------------------------------------------------------------------------------

/// Ctrl-C (SIGINT) during `oprobe run` still cleans up and prints partial results, but exits
/// 130 so scripts never mistake them for a complete run.
#[cfg(unix)]
#[test]
fn interrupted_run_exits_130() {
    let home = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .env("OPROBE_DATA_DIR", home.path().join("data"))
        .env("OPROBE_CONFIG", home.path().join("config.toml"))
        .env_remove("OPROBE_OUTPUT")
        .args(["run", "memory://", "-d", "60s", "--warmup", "0s", "--quiet"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Interrupt once the run is measuring: its run file is being written. (A fixed sleep
    // raced the child's start: on a loaded runner the signal arrived before the run had
    // installed its handler, and the child died of it instead of exiting 130.)
    let measuring = || {
        std::fs::read_dir(home.path().join("data").join("runs")).is_ok_and(|d| {
            d.filter_map(|e| e.ok()).any(|e| {
                e.file_name().to_string_lossy().ends_with(".oprobe.tmp")
                    && e.metadata().is_ok_and(|m| m.len() > 0)
            })
        })
    };
    let t0 = std::time::Instant::now();
    while !measuring() && t0.elapsed() < std::time::Duration::from_secs(120) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(measuring(), "the run never started measuring");
    // SAFETY: plain signal to our own child process.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{err}");
    assert!(err.contains("partial results"), "{err}");
}

/// D7: Ctrl-C during `oprobe prove` leaves no trial runs behind. The prove result is never
/// saved, so its trials would be orphans no `show --rm` could reach.
#[cfg(unix)]
#[test]
fn an_interrupted_prove_leaves_no_trial_runs() {
    let home = tempfile::tempdir().unwrap();
    let cmd = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
        c.env("OPROBE_DATA_DIR", home.path().join("data"))
            .env("OPROBE_CONFIG", home.path().join("config.toml"))
            .env_remove("OPROBE_OUTPUT");
        c
    };
    let o = cmd()
        .args(["run", "memory://", "-d", "500ms", "--warmup", "0s", "-q"])
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let runs_dir = home.path().join("data").join("runs");
    let run_files = || {
        std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "oprobe")
            })
            .count()
    };
    assert_eq!(run_files(), 1);
    let child = cmd()
        .args([
            "prove",
            "last",
            "--with",
            "layers.retry.jitter=on",
            "--trial",
            "500ms",
            "--pairs",
            "12",
            "-q",
            "-o",
            "json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Interrupt once a trial has been stored.
    let t0 = std::time::Instant::now();
    while run_files() < 3 && t0.elapsed() < std::time::Duration::from_secs(60) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(run_files() >= 3, "no trial was stored");
    // SAFETY: plain signal to our own child process.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{err}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "run.interrupted", "{v}");
    assert_eq!(v["error"]["retryable"], true, "{v}");
    assert_eq!(run_files(), 1, "trial runs were left behind");
    let all = cmd()
        .args(["show", "--list", "--all", "-o", "json"])
        .output()
        .unwrap();
    let all: serde_json::Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(all["total_count"], 1, "{all}");
}

// ---------------------------------------------------------------------------------------
// Trust fixes (v3): results must never lead to a wrong conclusion.
// ---------------------------------------------------------------------------------------

/// A lab served from a background runtime, for tests that drive the CLI binary.
fn background_lab(cfg: LabConfig) -> (tokio::runtime::Runtime, oprobe::lab::LabHandle) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(lab(cfg));
    (rt, h)
}

/// `oprobe run` against a lab with S3 credentials `x`/`x` and no proxy.
fn run_cli_against(endpoint: &str, extra: &[&str]) -> std::process::Output {
    oprobe_cmd()
        .args([
            "run",
            "s3://bkt/x",
            "--set",
            &format!("options.endpoint={endpoint}"),
            "--set",
            "http.proxy=none",
        ])
        .args([
            "--set",
            "options.access_key_id=x",
            "--set",
            "options.secret_access_key=x",
        ])
        .args([
            "--set",
            "options.region=us-east-1",
            "--preset",
            "small-files",
        ])
        .args(["--objects", "4", "--object-size", "16KiB", "-c", "2"])
        .args([
            "-d", "400ms", "--warmup", "0s", "-q", "--color", "never", "-o", "json",
        ])
        .args(extra)
        .output()
        .unwrap()
}

/// Failed operations breach the run's default checks (≤ 1% failed, no corruption): the report
/// says so (a breach is reported, not an exit code). The 5.x keys that relaxed them
/// (`--set sla=none`, `max_error_rate`) are refused since 6.3: oprobe 6 has no SLA.
#[test]
fn failed_operations_breach_the_default_sla() {
    let (_rt, lab) = background_lab(LabConfig {
        throttle_every_n_requests: Some(3),
        ..Default::default()
    });
    let no_retry = ["--set", "layers.retry=off"];
    let out = run_cli_against(&lab.endpoint_url, &no_retry);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let sla = &v["runs"][0]["report"]["verdict"]["sla"];
    assert_eq!(sla["passed"], false, "{sla}");
    assert!(!sla["breaches"].as_array().unwrap().is_empty());
    for relaxed in [
        &["--set", "sla=none"][..],
        &["--set", "max_error_rate=100%"][..],
    ] {
        let args: Vec<&str> = no_retry.iter().chain(relaxed).copied().collect();
        let out = run_cli_against(&lab.endpoint_url, &args);
        assert_eq!(out.status.code(), Some(2), "{relaxed:?}: a usage error");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            v["error"]["message"].as_str().unwrap().contains("no SLA"),
            "{relaxed:?}: {v}"
        );
    }
    let bad = run_cli_against(&lab.endpoint_url, &["--set", "max_error_rate=150%"]);
    assert_eq!(bad.status.code(), Some(2), "a usage error");
}

/// OpenDAL raises S3 write chunks to the 5 MiB minimum part size: the report says which chunk
/// was actually used instead of presenting the requested one as measured.
#[tokio::test(flavor = "multi_thread")]
async fn effective_write_chunk_is_reported() {
    let lab = lab(LabConfig::default()).await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::LargeObjects);
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::MultipartWrite {
            size: ByteSize::mib(6),
        },
    }];
    p.operator.write.chunk = Some(ByteSize::mib(1));
    p.operator.read.chunk = Some(ByteSize::mib(1));
    p.workload.concurrency = 2;
    let (s, c, prof) = run_once(p, 400).await;
    let e = &s.effective_knobs;
    assert_eq!(e.requested_chunk_size, ByteSize::mib(1).0);
    assert_eq!(e.write_chunk_size, ByteSize::mib(5).0);
    assert_eq!(e.service_min_part_size, Some(ByteSize::mib(5).0));
    // 6 MiB in 5 MiB parts: two UploadPart requests per object, not six.
    let parts = api(&s, Api::UploadPart).count as f64;
    let writes = s.verbs.iter().map(|v| v.count).sum::<u64>() as f64;
    assert!(parts / writes <= 2.5, "{parts} parts for {writes} writes");
    let r = build(&prof, s, c);
    assert_eq!(r.effective_write.write_chunk_size, ByteSize::mib(5).0);
    assert_eq!(r.effective_write.requested_chunk_size, ByteSize::mib(1).0);
    assert!(
        r.warnings.iter().any(|w| w.contains("5MiB parts")),
        "{:?}",
        r.warnings
    );
}
