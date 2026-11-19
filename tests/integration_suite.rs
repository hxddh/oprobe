use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use oprobe::autotune::{AutoTuneEngine, TuneKnob, TuneSearchSpace};
use oprobe::cost::compare_multi_cloud_finops;
use oprobe::net_probe::probe_endpoint_handshakes_with_tls;
use oprobe::report::diff::{diff_reports, RegressionThresholds};
use oprobe::report::BenchmarkReport;
use oprobe::telemetry::{diagnose_runtime_error, TelemetryFrame};
use oprobe::testbed::{ChaosFaultConfig, S3ChaosServer, TESTBED_TLS_CERT_PEM};
use oprobe::tls::TlsClientOptions;
use oprobe::tui::command::{apply_state_mutation, parse_minibuffer_command, MinibufferCommand};
use oprobe::workload::{
    build_admin_operator, coalesce_ranges_with_gap, execute_workload_round,
    execute_workload_with_options, generate_incompressible_payload, sweep_all_oprobe_prefixes,
    ProfileConfig, TargetConfig, WorkloadKind, WorkloadRunOptions, WorkloadSpec,
};

#[test]
fn payload_stamp_prevents_dedupe_and_verifies_crc32c() {
    let (buf_a, crc_a) = generate_incompressible_payload(1, 65_536);
    let (buf_b, crc_b) = generate_incompressible_payload(2, 65_536);
    assert_ne!(buf_a[..8], buf_b[..8]);
    assert_ne!(buf_a[65_528..], buf_b[65_528..]);
    assert_ne!(crc_a, crc_b);
    assert_eq!(crc32c::crc32c(&buf_a), crc_a);
}

#[test]
fn range_coalescing_tracks_exact_gap_waste_bytes() {
    let ranges = vec![0..1024, 2048..3072, 100_000..101_024];
    let (unmerged, waste_zero) = coalesce_ranges_with_gap(&ranges, 0);
    assert_eq!(unmerged.len(), 3);
    assert_eq!(waste_zero, 0);

    let (merged, waste_2k) = coalesce_ranges_with_gap(&ranges, 2048);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0], 0..3072);
    assert_eq!(waste_2k, 1024);
}

#[tokio::test]
async fn all_four_workloads_execute_and_auto_clean_on_memory_and_fs() {
    let knob = TuneKnob {
        concurrent: 8,
        chunk_bytes: 256 * 1024,
        range_gap_bytes: 256 * 1024,
        enable_foyer: true,
        enable_tail_cut: true,
        keep_alive: true,
    };

    for kind in [
        WorkloadKind::ParquetPointLookup,
        WorkloadKind::CheckpointMultipart,
        WorkloadKind::MetadataPrefixStorm,
        WorkloadKind::MixedReadWrite,
    ] {
        let mut prof = ProfileConfig::memory_demo(kind);
        prof.workload.rounds = 6;
        let frame = execute_workload_round(&prof, &knob, Some(6))
            .await
            .expect("memory workload should succeed");
        assert!(!frame.verbs.is_empty());
        assert_eq!(frame.transport.crc32c_corrupted, 0);
        assert!(frame.cleanup.verified_clean);
    }

    // Verify local filesystem (`fs`) backend end-to-end including verified auto-cleanup
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut opts = BTreeMap::new();
    opts.insert("root".to_owned(), tmp.path().to_string_lossy().into_owned());
    let fs_profile = ProfileConfig {
        target: TargetConfig {
            name: "TempDir FS".to_owned(),
            scheme: "fs".to_owned(),
            options: opts,
        },
        workload: WorkloadSpec {
            kind: WorkloadKind::ParquetPointLookup,
            rounds: 4,
            duration_secs: None,
            warmup_secs: 0.0,
            object_size_mib: 2,
            verify_crc32c: true,
            enable_foyer_cache: false,
            foyer_memory_mib: 16,
            internet_egress_ratio: 0.0,
            auto_cleanup: true,
        },
        ..ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup)
    };
    let fs_frame = execute_workload_round(&fs_profile, &knob, Some(4))
        .await
        .expect("fs workload should succeed");
    assert!(fs_frame.transport.crc32c_verified_ok >= 1);
    assert!(fs_frame.cleanup.verified_clean);
    assert_eq!(fs_frame.cleanup.remaining_objects, 0);
}

#[tokio::test]
async fn real_s3_https_tls13_cold_vs_warm_socket_reuse_and_handshake_waterfall() {
    let server = S3ChaosServer::spawn(
        "127.0.0.1:0",
        ChaosFaultConfig {
            enable_tls: true,
            enable_http2: false,
            base_ttfb_delay: Duration::from_millis(12),
            ..Default::default()
        },
    )
    .await
    .expect("spawn TLS 1.3 S3 server");

    let mut prof = server.make_s3_profile("lakehouse", WorkloadKind::MetadataPrefixStorm);
    prof.workload.rounds = 6;
    prof.workload.enable_foyer_cache = false;

    // 1. Keep-Alive OFF: every HTTP transaction must dial a fresh TLS 1.3 socket (0% reuse)
    let cold_knob = TuneKnob {
        concurrent: 2,
        chunk_bytes: 256 * 1024,
        range_gap_bytes: 64 * 1024,
        enable_foyer: false,
        enable_tail_cut: false,
        keep_alive: false,
    };
    let cold_frame = execute_workload_round(&prof, &cold_knob, Some(6))
        .await
        .expect("cold S3 run");
    assert!(cold_frame.transport.cold_connections_dialed >= 6);
    assert_eq!(cold_frame.transport.reused_connections, 0);
    assert!(cold_frame.transport.socket_reuse_pct < 1.0);
    assert!(cold_frame.waterfall.tls_handshake_us > 0);
    assert!(cold_frame.waterfall.remote_ttfb_us >= 8_000);
    assert_eq!(cold_frame.diagnosis.primary_stage, "SocketPoolThrashing");
    assert_eq!(server.stored_object_count(), 0);

    // 2. Keep-Alive ON: connection pool reuses warm TLS sockets (>70% reuse)
    let warm_knob = TuneKnob {
        keep_alive: true,
        ..cold_knob
    };
    let warm_frame = execute_workload_round(&prof, &warm_knob, Some(10))
        .await
        .expect("warm S3 run");
    assert!(warm_frame.transport.reused_connections > warm_frame.transport.cold_connections_dialed);
    assert!(warm_frame.transport.socket_reuse_pct >= 65.0);
    assert!(warm_frame.waterfall.dns_tcp_tls_us < cold_frame.waterfall.dns_tcp_tls_us);
    assert!(!warm_frame.transport.recent_request_ids.is_empty());
    assert_eq!(server.stored_object_count(), 0);
}

#[tokio::test]
async fn real_s3_https_h2_multipart_upload_merge_timing_and_crc32c() {
    let server = S3ChaosServer::spawn(
        "127.0.0.1:0",
        ChaosFaultConfig {
            enable_tls: true,
            enable_http2: true,
            base_ttfb_delay: Duration::from_millis(2),
            multipart_merge_delay: Duration::from_millis(35),
            ..Default::default()
        },
    )
    .await
    .expect("spawn H2 TLS 1.3 S3 server");

    let mut prof = server.make_s3_profile("ai-ckpt", WorkloadKind::CheckpointMultipart);
    prof.workload.object_size_mib = 1;
    prof.workload.rounds = 3;
    prof.workload.enable_foyer_cache = false;

    let knob = TuneKnob {
        concurrent: 4,
        chunk_bytes: 256 * 1024, // 1 MiB / 256 KiB = 4 parts per shard -> triggers real S3 Multipart Upload!
        range_gap_bytes: 256 * 1024,
        enable_foyer: false,
        enable_tail_cut: false,
        keep_alive: true,
    };

    let frame = execute_workload_round(&prof, &knob, Some(3))
        .await
        .expect("multipart S3 run");

    assert_eq!(frame.transport.negotiated_alpn, "h2");
    assert!(frame.transport.tls_version.contains("TLSv1_3"));
    assert_eq!(frame.transport.multipart_initiations, 3);
    assert!(frame.transport.multipart_parts_uploaded >= 6);
    assert_eq!(frame.transport.multipart_completions, 3);
    // Each CompleteMultipartUpload sleeps 35ms on server -> mean merge stage must reflect >= 28ms
    assert!(
        frame.waterfall.multipart_merge_us >= 28_000,
        "expected >=28ms multipart merge, got {}us",
        frame.waterfall.multipart_merge_us
    );
    assert!(frame.transport.crc32c_verified_ok >= 2);
    assert_eq!(frame.transport.crc32c_corrupted, 0);
    assert_eq!(server.stored_object_count(), 0);
}

#[tokio::test]
async fn real_s3_range_206_read_amplification_and_local_cache_savings() {
    let server = S3ChaosServer::spawn("127.0.0.1:0", ChaosFaultConfig::clean_https_h2())
        .await
        .expect("spawn S3 server");

    let mut prof = server.make_s3_profile("parquet-lake", WorkloadKind::ParquetPointLookup);
    prof.workload.object_size_mib = 2;

    // 1. Excessive range_gap_bytes (2 MiB) without cache -> pulls entire gaps via HTTP 206, high read amplification
    let wide_gap_knob = TuneKnob {
        concurrent: 4,
        chunk_bytes: 2 * 1024 * 1024,
        range_gap_bytes: 2 * 1024 * 1024,
        enable_foyer: false,
        enable_tail_cut: false,
        keep_alive: true,
    };
    let wide_frame = execute_workload_round(&prof, &wide_gap_knob, Some(6))
        .await
        .expect("wide gap run");
    assert!(wide_frame.transport.http_206_range_count >= 6);
    assert!(wide_frame.transport.coalesced_gap_waste_bytes > 500_000);
    assert!(
        wide_frame.transport.read_amplification > 2.5,
        "expected high read_amplification, got {}",
        wide_frame.transport.read_amplification
    );
    assert_eq!(wide_frame.diagnosis.primary_stage, "HighReadAmplification");

    // 2. Tight range_gap_bytes (32 KiB) WITH WhiteboxCacheLayer enabled -> 1.0x read amp & local cache hits!
    let cached_knob = TuneKnob {
        concurrent: 4,
        chunk_bytes: 256 * 1024,
        range_gap_bytes: 32 * 1024,
        enable_foyer: true,
        enable_tail_cut: false,
        keep_alive: true,
    };
    let cached_frame = execute_workload_round(&prof, &cached_knob, Some(16))
        .await
        .expect("cached tight gap run");
    assert!(cached_frame.transport.read_amplification < 1.15);
    assert!(cached_frame.transport.cache_hits >= 10);
    assert!(cached_frame.transport.cache_hit_ratio_pct > 40.0);
    assert!(cached_frame.waterfall.local_cache_us > 0 || cached_frame.transport.cache_lookups > 0);
    assert!(
        cached_frame.transport.net_wire_read_ratio < 0.85,
        "cache hits should drop net wire/logical read ratio below 0.85, got {}",
        cached_frame.transport.net_wire_read_ratio
    );

    let finops = compare_multi_cloud_finops(&cached_frame.transport, 0.02, 0.25);
    assert_eq!(finops.len(), 5);
    assert!(finops[0].saved_by_cache_usd > 0.0);
    assert_eq!(server.stored_object_count(), 0);
}

#[tokio::test]
async fn real_s3_straggler_captures_slow_await_tree_call_stack_and_queue_wait() {
    let server = S3ChaosServer::spawn(
        "127.0.0.1:0",
        ChaosFaultConfig {
            enable_tls: false,
            enable_http2: false,
            base_ttfb_delay: Duration::from_millis(8),
            straggler_every_n: 3,
            straggler_delay: Duration::from_millis(95),
            ..Default::default()
        },
    )
    .await
    .expect("spawn straggler S3 server");

    let mut prof = server.make_s3_profile("straggler", WorkloadKind::ParquetPointLookup);
    prof.workload.enable_foyer_cache = false;

    // Set concurrent=1 while reader spawns multiple range fetches so both semaphore queueing & slow AwaitTree fire
    let knob = TuneKnob {
        concurrent: 1,
        chunk_bytes: 64 * 1024,
        range_gap_bytes: 0,
        enable_foyer: false,
        enable_tail_cut: false,
        keep_alive: true,
    };

    let frame = execute_workload_round(&prof, &knob, Some(4))
        .await
        .expect("straggler run");

    assert!(
        !frame.slowest_traces.is_empty(),
        "slowest_traces should capture in-flight AwaitTree call stacks"
    );
    let top_trace = &frame.slowest_traces[0];
    assert!(top_trace.observed_ms >= 50.0);
    assert!(top_trace.tree_dump.contains("parquet_lookup#"));
    assert!(top_trace.tree_dump.contains("opendal::"));
    assert!(frame.await_tree_dump.contains("Top Captured Slow-Request Call Stacks"));
    assert_eq!(server.stored_object_count(), 0);
}

#[tokio::test]
async fn time_window_streaming_and_guaranteed_cleanup_plus_manual_sweep() {
    let server = S3ChaosServer::spawn(
        "127.0.0.1:0",
        ChaosFaultConfig {
            enable_tls: false,
            enable_http2: false,
            base_ttfb_delay: Duration::from_millis(4),
            ..Default::default()
        },
    )
    .await
    .expect("spawn S3 server");

    let prof = server.make_s3_profile("streaming", WorkloadKind::MixedReadWrite);
    let knob = TuneKnob {
        concurrent: 4,
        chunk_bytes: 256 * 1024,
        range_gap_bytes: 128 * 1024,
        enable_foyer: true,
        enable_tail_cut: true,
        keep_alive: true,
    };

    let (live_tx, mut live_rx) = mpsc::unbounded_channel::<TelemetryFrame>();
    let frame = execute_workload_with_options(
        &prof,
        &knob,
        WorkloadRunOptions {
            rounds_override: None,
            duration_override: Some(Duration::from_millis(380)),
            warmup_override: Some(Duration::from_millis(80)),
            cancel_flag: None,
            live_frame_tx: Some(live_tx),
        },
    )
    .await
    .expect("streaming window run");

    let mut streamed_frames = 0_usize;
    while live_rx.try_recv().is_ok() {
        streamed_frames += 1;
    }
    assert!(streamed_frames >= 2, "expected live 100ms frames during streaming window");
    assert!(frame.transport.logical_ops >= 4);
    assert_eq!(server.stored_object_count(), 0);

    // Test cooperative cancellation mid-stream still cleans up 100% of remote objects
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_trigger = cancel_flag.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        cancel_trigger.store(true, Ordering::Relaxed);
    });
    let cancelled_frame = execute_workload_with_options(
        &prof,
        &knob,
        WorkloadRunOptions {
            rounds_override: None,
            duration_override: Some(Duration::from_secs(10)),
            warmup_override: None,
            cancel_flag: Some(cancel_flag),
            live_frame_tx: None,
        },
    )
    .await
    .expect("cooperatively cancelled run should exit cleanly");
    assert!(cancelled_frame.cleanup.verified_clean);
    assert_eq!(server.stored_object_count(), 0);

    // Test `--keep-remote-data` followed by `sweep_all_oprobe_prefixes` (`oprobe cleanup`)
    let mut keep_prof = prof.clone();
    keep_prof.workload.auto_cleanup = false;
    let _ = execute_workload_round(&keep_prof, &knob, Some(3))
        .await
        .expect("keep data run");
    assert!(server.stored_object_count() > 0);

    let admin_op = build_admin_operator(&keep_prof).expect("admin operator");
    let sweep = sweep_all_oprobe_prefixes(&admin_op)
        .await
        .expect("cleanup sweep");
    assert!(sweep.verified_clean);
    assert_eq!(server.stored_object_count(), 0);
}

#[tokio::test]
async fn autotune_detects_and_avoids_503_slowdown_and_roundtrips_toml_profile() {
    let server = S3ChaosServer::spawn(
        "127.0.0.1:0",
        ChaosFaultConfig {
            enable_tls: true,
            enable_http2: true,
            base_ttfb_delay: Duration::from_millis(6),
            max_inflight_before_503: Some(5),
            throttle_status_code: 503,
            ..Default::default()
        },
    )
    .await
    .expect("spawn rate-limited S3 server");

    let mut prof = server.make_s3_profile("throttled-bucket", WorkloadKind::MetadataPrefixStorm);
    prof.workload.rounds = 8;

    let space = TuneSearchSpace {
        concurrencies: vec![2, 4, 16, 32],
        chunks_bytes: vec![256 * 1024, 1024 * 1024],
        gaps_bytes: vec![64 * 1024, 256 * 1024],
        trial_rounds: 8,
    };
    let engine = AutoTuneEngine::new(prof.sla.clone(), space);
    let outcome = engine
        .run_adaptive_sweep(&prof)
        .await
        .expect("autotune must survive 503 SlowDown and converge");

    assert!(outcome.circuit_breaker_triggered);
    assert!(server.emitted_throttles() > 0);
    assert!(!outcome.winner.throttled_503);
    assert!(
        outcome.winner.knob.concurrent < 16,
        "winner concurrency ({}) must stay below 503 cliff (16)",
        outcome.winner.knob.concurrent
    );
    // Verify conc=32 was skipped by the 503 circuit breaker
    assert!(
        !outcome.trials.iter().any(|t| t.knob.concurrent == 32),
        "circuit breaker should skip conc=32 after conc=16 throttled"
    );

    // Verify generated TOML profile round-trips cleanly with exact `[knobs]`
    let roundtrip: ProfileConfig =
        toml::from_str(&outcome.generated_toml_profile).expect("parse generated TOML profile");
    assert_eq!(roundtrip.effective_knob(), outcome.winner.knob);
    assert!(outcome.generated_rust_code.contains("Operator::via_iter"));
    assert!(outcome.generated_python_code.contains("AsyncOperator"));
    assert_eq!(server.stored_object_count(), 0);
}

#[tokio::test]
async fn net_probe_cold_vs_warm_and_diff_regression_gate_and_diagnostics() {
    let server = S3ChaosServer::spawn(
        "127.0.0.1:0",
        ChaosFaultConfig {
            enable_tls: true,
            enable_http2: true,
            base_ttfb_delay: Duration::from_millis(6),
            ..Default::default()
        },
    )
    .await
    .expect("spawn TLS S3 server");

    let tls_opts = TlsClientOptions {
        alpn_protocols: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        extra_ca_pem: Some(TESTBED_TLS_CERT_PEM.to_owned()),
        insecure_skip_verify: false,
    };
    let hs = probe_endpoint_handshakes_with_tls(
        &server.endpoint_url,
        3,
        Duration::from_secs(3),
        &tls_opts,
    )
    .await
    .expect("net-probe TLS 1.3 H2");
    assert_eq!(hs.sample_count, 3);
    assert_eq!(hs.negotiated_alpn, "h2");
    assert!(hs.tls_p50_ms > 0.0);
    assert!(hs.head_ttfb_p50_ms >= 5.0);
    assert!(hs.warm_ttfb_p50_ms >= 5.0);
    assert!(hs.cold_total_ms > hs.warm_ttfb_p50_ms);

    // Test BenchmarkReport + `oprobe diff` gate
    let prof = server.make_s3_profile("diff-gate", WorkloadKind::ParquetPointLookup);
    let knob = TuneKnob::default();
    let base_frame = execute_workload_round(&prof, &knob, Some(6))
        .await
        .expect("baseline run");
    let baseline = BenchmarkReport::from_single_run(prof, knob, base_frame);

    let pass_diff = diff_reports(&baseline, &baseline, &RegressionThresholds::default());
    assert!(pass_diff.passed_gate);

    let mut degraded = baseline.clone();
    degraded.frame.transport.read_amplification += 0.8;
    degraded.frame.transport.http_503_slowdown_count = 2;
    if let Some(v) = degraded.frame.verbs.first_mut() {
        v.p99_ms = v.p99_ms * 1.8 + 5.0;
    }
    let fail_diff = diff_reports(&baseline, &degraded, &RegressionThresholds::default());
    assert!(!fail_diff.passed_gate);
    assert!(fail_diff.reasons.len() >= 3);

    // Test human-friendly error diagnostics & minibuffer parser
    let hint_403 = diagnose_runtime_error("HTTP 403 SignatureDoesNotMatch", "s3");
    assert!(hint_403.category.contains("401/403"));
    let hint_tls = diagnose_runtime_error("TLS UnknownIssuer certificate error", "s3");
    assert!(hint_tls.category.contains("TLS"));

    let mut m_prof = ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup);
    let mut m_knob = TuneKnob::default();
    let cmd = parse_minibuffer_command(":url s3://analytics-bucket/warehouse").unwrap();
    apply_state_mutation(&cmd, &mut m_prof, &mut m_knob);
    assert_eq!(m_prof.target.scheme, "s3");
    assert_eq!(
        m_prof.target.options.get("bucket").map(String::as_str),
        Some("analytics-bucket")
    );
    assert_eq!(
        parse_minibuffer_command("/stream 12s").unwrap(),
        MinibufferCommand::StreamRun(Duration::from_secs(12))
    );
}
