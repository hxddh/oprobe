//! End-to-end tests against in-process backends and the local S3 lab over real sockets.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use oprobe::analysis::diagnosis::Bottleneck;
use oprobe::config::*;
use oprobe::engine::{Cancel, RunOptions, Session};
use oprobe::lab::{LabConfig, LabServer};
use oprobe::report::Report;
use oprobe::report::diff::{Thresholds, diff};
use oprobe::trace::{Api, Snapshot};
use oprobe::util::ByteSize;

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
    p.knobs.concurrency = 4;
}

async fn run_once(p: Profile, ms: u64) -> (Snapshot, oprobe::engine::CleanupSummary, Profile) {
    let s = Session::open(p, Cancel::new(), quiet())
        .await
        .expect("session opens");
    let knobs = s.profile.knobs.clone();
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
            let mut p = Profile::from_target(TargetSpec::from_url(&url).unwrap());
            small(&mut p, preset);
            if preset == Preset::Checkpoint {
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

async fn http1_requests_are_attributed_stage_by_stage_with(mode: TransportMode) {
    let lab = lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(15),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
    small(&mut p, Preset::Mixed);
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

async fn https_h2_multipart_upload_is_traced_per_api_with(mode: TransportMode) {
    let lab = lab(LabConfig {
        enable_tls: true,
        enable_http2: true,
        multipart_merge_delay: Duration::from_millis(30),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
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
    p.knobs.chunk_size = ByteSize::mib(5);
    p.knobs.concurrency = 2;
    // OpenDAL's client speaks HTTP/1.1 unless the app enables reqwest's http2 feature.
    p.knobs.http_version = oprobe::net::HttpVersion::H2;
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

async fn keep_alive_off_dials_for_every_request_with(mode: TransportMode) {
    let lab = lab(LabConfig::default()).await;
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
    small(&mut p, Preset::SmallFiles);
    p.knobs.keep_alive = false;
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
    let r = Report::build(&prof, &prof.knobs, s, c, vec![]);
    assert_eq!(r.diagnosis.bottleneck, Bottleneck::Throttling);
    assert!(r.diagnosis.findings.iter().any(|f| f.code == "throttling"));
    assert!(r.cost.per_op.failed_requests > 0.0);
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
    small(&mut p, Preset::Mixed);
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
    let mut p = Profile::from_target(TargetSpec::from_url("memory://").unwrap());
    small(&mut p, Preset::SmallFiles);
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1500);
    o.rate = Some(200.0);
    let snap = s.run(&s.profile.knobs.clone(), &o, None).await.unwrap();
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
    p.knobs.concurrency = 16;
    p.knobs.max_inflight_requests = 2;
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
    p.knobs.cache = true;
    let (s, _, _) = run_once(p, 600).await;
    assert!(s.cache.hits > 0);
    assert!(s.bytes.cache_hit_bytes > 0);
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

async fn proxy_connect_is_used_and_timed_with(mode: TransportMode) {
    let lab = lab(LabConfig {
        enable_tls: true,
        enable_http2: true,
        ..Default::default()
    })
    .await;
    let (proxy, tunnels) = connect_proxy().await;
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
    p.target.proxy = Some(proxy.clone());
    small(&mut p, Preset::SmallFiles);
    p.knobs.keep_alive = false;
    let (s, c, _) = run_once(p, 400).await;
    assert!(c.verified);
    assert!(tunnels.load(std::sync::atomic::Ordering::Relaxed) > 5);
    assert_eq!(s.connections.proxy.as_deref(), Some(proxy.as_str()));
    assert!(s.connections.proxy_connect.count > 0);
    assert!(s.connections.tls.count > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn tuner_finds_throttling_ceiling() {
    use oprobe::tune::{Objective, TuneOptions, tune};
    let lab = lab(LabConfig {
        base_ttfb_delay: Duration::from_millis(5),
        max_inflight_before_503: Some(6),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    small(&mut p, Preset::SmallFiles);
    p.knobs.concurrency = 32;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let o = TuneOptions {
        objective: Objective::Throughput,
        trial: RunOptions {
            duration: Duration::from_millis(500),
            warmup: Duration::from_millis(100),
            max_ops: None,
            rate: None,
            trace_slow_ops: false,
        },
        max_concurrency: 64,
        max_trials: 12,
        cooldown: Duration::ZERO,
    };
    let out = tune(&s, &o, Arc::new(|_| {})).await.unwrap();
    s.close().await;
    let ceiling = out.summary.throttle_ceiling.expect("ceiling detected");
    assert!((4..=16).contains(&ceiling), "ceiling {ceiling}");
    assert!(out.winner.concurrency < ceiling);
    // The headroom cap (80 % of the ceiling) holds for the exported winner too.
    assert!(
        out.winner.concurrency <= (ceiling as f64 * 0.8).floor().max(1.0) as usize,
        "winner {} exceeds 80% of ceiling {ceiling}",
        out.winner.concurrency
    );
    assert!(
        out.winner_snapshot.resilience.throttled_responses == 0,
        "winner must be clean"
    );
    assert!(!out.summary.trials[out.summary.baseline_trial].feasible);
}

#[tokio::test(flavor = "multi_thread")]
async fn report_roundtrip_and_diff_gate() {
    let mut p = Profile::from_target(TargetSpec::from_url("memory://").unwrap());
    small(&mut p, Preset::SmallFiles);
    let (s, c, prof) = run_once(p, 400).await;
    let r = Report::build(&prof, &prof.knobs, s, c, vec![]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    r.save_json(&path).unwrap();
    let back = Report::load_json(&path).unwrap();
    assert_eq!(back, r);
    let same = diff(&r, &back, &Thresholds::default());
    assert!(same.passed, "{}", same.render_terminal());

    let mut worse = r.clone();
    worse.result.summary.ops_per_s *= 0.5;
    for v in &mut worse.result.verbs {
        v.latency.p99 = v.latency.p99 * 3.0 + 10.0;
    }
    worse.result.integrity.corrupted = 1;
    let d = diff(&r, &worse, &Thresholds::default());
    assert!(!d.passed);
    let fails: Vec<_> = d
        .checks
        .iter()
        .filter(|c| c.status == oprobe::report::diff::Status::Fail)
        .map(|c| c.name.clone())
        .collect();
    assert!(fails.iter().any(|n| n.contains("throughput")));
    assert!(fails.iter().any(|n| n.contains("p99")));
    assert!(fails.iter().any(|n| n.contains("integrity")));
    assert!(d.render_junit().contains("<failure"));
    assert!(d.render_markdown().contains("FAIL"));
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
    let t = prof.target.normalized().unwrap();
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

#[test]
fn cli_binary_end_to_end() {
    let bin = env!("CARGO_BIN_EXE_oprobe");
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.json");
    let out = Command::new(bin)
        .args([
            "run",
            "memory://",
            "--preset",
            "small-files",
            "-d",
            "500ms",
            "--warmup",
            "100ms",
            "--objects",
            "8",
            "--object-size",
            "64KiB",
            "-q",
            "--color",
            "never",
            "--json",
        ])
        .arg(&base)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("WHERE THE TIME GOES"), "{text}");
    assert!(text.contains("CLEANUP"));

    let junit = dir.path().join("gate.xml");
    let d = Command::new(bin)
        .args(["diff", "--color", "never"])
        .arg(&base)
        .arg(&base)
        .arg("--junit")
        .arg(&junit)
        .output()
        .unwrap();
    assert!(d.status.success(), "{}", String::from_utf8_lossy(&d.stdout));
    assert!(
        std::fs::read_to_string(&junit)
            .unwrap()
            .contains("testsuite")
    );

    let init = Command::new(bin)
        .args(["init", "--preset", "checkpoint"])
        .output()
        .unwrap();
    let prof: Profile = toml::from_str(&String::from_utf8_lossy(&init.stdout)).unwrap();
    assert_eq!(prof.workload.preset, Preset::Checkpoint);

    let bad = Command::new(bin).args(["run", "ftp://x"]).output().unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("hint:"));

    let comp = Command::new(bin)
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&comp.stdout).contains("oprobe"));
    let fail_on = Command::new(bin)
        .args([
            "run",
            "memory://",
            "-d",
            "200ms",
            "--warmup",
            "0s",
            "-q",
            "--objects",
            "4",
            "--object-size",
            "64KiB",
            "--fail-on",
            "warning",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let rep: serde_json::Value = serde_json::from_slice(&fail_on.stdout).unwrap();
    assert_eq!(rep["schema"], "oprobe.report/v2");
}

#[test]
fn shipped_profiles_are_valid() {
    // FIXME: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("R2_ACCOUNT_ID", "x") };
    // FIXME: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("MINIO_ROOT_USER", "x") };
    // FIXME: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("MINIO_ROOT_PASSWORD", "x") };
    let mut n = 0;
    for e in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/profiles")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "toml") {
            let prof = Profile::load(&p).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
            prof.target
                .normalized()
                .unwrap()
                .resolved_options()
                .unwrap();
            n += 1;
        }
    }
    assert!(n >= 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn http1_requests_are_attributed_stage_by_stage() {
    http1_requests_are_attributed_stage_by_stage_with(TransportMode::Opendal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http1_requests_are_attributed_stage_by_stage_microscope() {
    http1_requests_are_attributed_stage_by_stage_with(TransportMode::Microscope).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn https_h2_multipart_upload_is_traced_per_api() {
    https_h2_multipart_upload_is_traced_per_api_with(TransportMode::Opendal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn https_h2_multipart_upload_is_traced_per_api_microscope() {
    https_h2_multipart_upload_is_traced_per_api_with(TransportMode::Microscope).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_alive_off_dials_for_every_request() {
    keep_alive_off_dials_for_every_request_with(TransportMode::Opendal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_alive_off_dials_for_every_request_microscope() {
    keep_alive_off_dials_for_every_request_with(TransportMode::Microscope).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_connect_is_used_and_timed() {
    proxy_connect_is_used_and_timed_with(TransportMode::Opendal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_connect_is_used_and_timed_microscope() {
    proxy_connect_is_used_and_timed_with(TransportMode::Microscope).await;
}

/// Gateways answer 503 and hang up while the client is still uploading; the client sees a reset
/// or abort (always on Windows) and must retry rather than fail the operation or the seeding.
async fn early_close_throttling_is_retried_with(mode: TransportMode) {
    let lab = lab(LabConfig {
        throttle_every_n_requests: Some(5),
        throttle_early_close: true,
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
    small(&mut p, Preset::SmallFiles);
    p.knobs.retries = 6;
    let (s, c, _) = run_once(p, 600).await;
    assert!(c.verified, "{c:?}");
    assert!(s.summary.ops > 0);
    assert_eq!(s.summary.failed_ops, 0, "{:?}", s.verbs);
    assert!(s.resilience.retries > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn early_close_throttling_is_retried() {
    early_close_throttling_is_retried_with(TransportMode::Opendal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn early_close_throttling_is_retried_microscope() {
    early_close_throttling_is_retried_with(TransportMode::Microscope).await;
}

/// Requests abandoned by the operation timeout must release their in-flight slot: a leak would
/// make the in-flight count (and its peak) grow with every timeout.
async fn timeouts_do_not_leak_inflight_requests_with(mode: TransportMode) {
    let lab = lab(LabConfig {
        straggler_every_n: 2,
        straggler_delay: Duration::from_millis(800),
        ..Default::default()
    })
    .await;
    let mut p = lab.profile("bkt");
    p.knobs.transport = mode;
    small(&mut p, Preset::SmallFiles);
    p.knobs.concurrency = 2;
    p.knobs.io_concurrency = 1;
    p.knobs.retries = 0;
    p.knobs.timeout = oprobe::util::HumanDuration(Duration::from_millis(60));
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
    timeouts_do_not_leak_inflight_requests_with(TransportMode::Opendal).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timeouts_do_not_leak_inflight_requests_microscope() {
    timeouts_do_not_leak_inflight_requests_with(TransportMode::Microscope).await;
}

/// An open-loop arrival interval longer than the window must not run past it.
#[tokio::test(flavor = "multi_thread")]
async fn open_loop_never_runs_past_the_window() {
    let mut p = Profile::from_target(TargetSpec::from_url("memory://").unwrap());
    small(&mut p, Preset::SmallFiles);
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1000);
    o.warmup = Duration::ZERO;
    o.rate = Some(0.2); // one arrival every 5 s
    let t = std::time::Instant::now();
    let snap = s.run(&s.profile.knobs.clone(), &o, None).await.unwrap();
    s.close().await;
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "took {:?}",
        t.elapsed()
    );
    assert!(snap.summary.ops <= 1, "issued {} ops", snap.summary.ops);
}

/// A bare `oprobe` is what `oprobe.app` runs when opened from Finder (with `-psn_…` on some
/// macOS versions): it must reach the workbench entry, never fail with a usage error (exit 2)
/// before any window code runs. Here stdout is a pipe and the test build has no window, so
/// the entry falls back to printing help and succeeding.
#[test]
fn bare_invocation_is_not_a_usage_error() {
    let bin = env!("CARGO_BIN_EXE_oprobe");
    for args in [&[][..], &["-psn_0_1234567"][..]] {
        let out = Command::new(bin)
            .args(args)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            // macOS / Windows builds with `gui`: no window from a "remote" shell.
            .env("SSH_CONNECTION", "test")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("Usage: oprobe"));
    }
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
    p.knobs.concurrency = 4;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1000);
    o.warmup = Duration::ZERO;
    let t = std::time::Instant::now();
    let snap = s.run(&s.profile.knobs.clone(), &o, None).await.unwrap();
    let wall = t.elapsed();
    s.close().await;
    assert!(
        snap.elapsed_s <= 1.05,
        "window {}s includes the drain",
        snap.elapsed_s
    );
    assert!(
        wall.as_secs_f64() > snap.elapsed_s + 0.1,
        "ops drained after the window"
    );
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
    let mut p = Profile::from_target(TargetSpec::from_url("memory://").unwrap());
    small(&mut p, Preset::SmallFiles);
    p.knobs.concurrency = 16;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(10_000);
    o.warmup = Duration::ZERO;
    o.max_ops = Some(101);
    let snap = s.run(&s.profile.knobs.clone(), &o, None).await.unwrap();
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
    p.knobs.concurrency = 2;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let mut o = opts(1500);
    o.warmup = Duration::ZERO;
    o.rate = Some(200.0); // far above the ~40 ops/s two slots can serve
    let snap = s.run(&s.profile.knobs.clone(), &o, None).await.unwrap();
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
    let mut p = Profile::from_target(TargetSpec::from_url("memory://").unwrap());
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
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    p.target
        .options
        .insert("endpoint".into(), format!("http://127.0.0.1:{port}"));
    small(&mut p, Preset::SmallFiles);
    let t = std::time::Instant::now();
    let err = Session::open(p, Cancel::new(), quiet())
        .await
        .err()
        .expect("must fail");
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    let msg = format!("{err:#}");
    assert!(msg.contains("cannot reach"), "{msg}");
    let hint = oprobe::analysis::hints::hint_for(&msg, "s3").expect("hint");
    assert!(hint.title.contains("refused"), "{}", hint.title);
}

/// A run stopped with Cancel is flagged `interrupted` in the snapshot and report, and
/// `oprobe diff` warns about it.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_run_is_marked_interrupted() {
    let mut p = Profile::from_target(TargetSpec::from_url("memory://").unwrap());
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
    let snap = s.run(&s.profile.knobs.clone(), &o, None).await.unwrap();
    let c = s.close().await;
    assert!(snap.interrupted);
    assert!(snap.elapsed_s < 5.0, "{}", snap.elapsed_s);
    let prof = s.profile.clone();
    let r = Report::build(&prof, &prof.knobs, snap, c, vec![]);
    assert!(r.interrupted);
    let json = oprobe::report::to_json(&r);
    assert!(json.contains("\"interrupted\": true"));
    assert!(oprobe::report::terminal::render(&r).contains("INTERRUPTED"));
    assert!(oprobe::report::markdown::render(&r).contains("Interrupted"));
    let mut done = r.clone();
    done.interrupted = false;
    done.result.interrupted = false;
    let d = diff(&done, &r, &Thresholds::default());
    assert!(
        d.warnings.iter().any(|w| w.contains("interrupted")),
        "{:?}",
        d.warnings
    );
    // A complete report does not carry the field at all.
    assert!(!oprobe::report::to_json(&done).contains("interrupted"));
}

// ---------------------------------------------------------------------------------------
// Interactive entry points and argument validation (UI/CLI review fixes).
// ---------------------------------------------------------------------------------------

/// `oprobe top` without a terminal explains itself and exits 2 (not a cryptic os error).
#[test]
fn top_without_a_terminal_is_a_clear_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args(["top", "memory://"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    if cfg!(feature = "tui") {
        assert_eq!(out.status.code(), Some(2), "{err}");
        assert!(err.contains("needs an interactive terminal"), "{err}");
        assert!(err.contains("oprobe run"), "{err}");
    } else {
        assert!(!out.status.success());
    }
}

/// `oprobe gui` without a display (or a window build) fails clearly and points to `top`.
#[test]
fn gui_without_a_display_fails_clearly() {
    let out = Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args(["gui", "memory://"])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("SSH_CONNECTION", "test")
        .env("XDG_STATE_HOME", tempfile::tempdir().unwrap().path())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("oprobe top"), "{err}");
}

/// Zero workers or too few tuner trials are rejected, never silently clamped.
#[test]
fn zero_concurrency_and_too_few_trials_are_rejected() {
    let bin = env!("CARGO_BIN_EXE_oprobe");
    for args in [
        &["run", "memory://", "-c", "0"][..],
        &["run", "memory://", "--io-concurrency", "0"][..],
        &["tune", "memory://", "--max-trials", "1"][..],
    ] {
        let out = Command::new(bin).args(args).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("must be at least"), "{args:?}: {err}");
    }
}

/// Ctrl-C (SIGINT) during `oprobe run` still cleans up and prints partial results, but exits
/// 130 so scripts never mistake them for a complete run.
#[cfg(unix)]
#[test]
fn interrupted_run_exits_130() {
    let child = Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args(["run", "memory://", "-d", "60s", "--warmup", "0s", "--quiet"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    // SAFETY: plain signal to our own child process.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{err}");
    assert!(err.contains("partial results"), "{err}");
}
