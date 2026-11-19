use std::collections::BTreeMap;

use oprobe::autotune::{AutoTuneEngine, TuneKnob, TuneSearchSpace};
use oprobe::cost::compare_multi_cloud_finops;
use oprobe::report::diff::{diff_reports, RegressionThresholds};
use oprobe::report::BenchmarkReport;
use oprobe::tui::command::{apply_state_mutation, parse_minibuffer_command, MinibufferCommand};
use oprobe::workload::{
    coalesce_ranges_with_gap, execute_workload_round, generate_incompressible_payload,
    ProfileConfig, TargetConfig, WorkloadKind, WorkloadSpec,
};

#[test]
fn payload_stamp_prevents_dedupe_and_verifies_crc32c() {
    let (buf_a, crc_a) = generate_incompressible_payload(1, 65_536);
    let (buf_b, crc_b) = generate_incompressible_payload(2, 65_536);
    assert_ne!(buf_a[..8], buf_b[..8]);
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
async fn all_four_workloads_execute_cleanly_on_memory_and_fs() {
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
    }

    // Verify local filesystem (`fs`) backend end-to-end
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
            object_size_mib: 2,
            verify_crc32c: true,
            enable_foyer_cache: false,
            foyer_memory_mib: 16,
            internet_egress_ratio: 0.0,
        },
        ..ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup)
    };
    let fs_frame = execute_workload_round(&fs_profile, &knob, Some(4))
        .await
        .expect("fs workload should succeed");
    assert!(fs_frame.transport.crc32c_verified_ok >= 1);
}

#[tokio::test]
async fn autotune_generates_code_and_report_diff_catches_regression() {
    let mut prof = ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup);
    prof.workload.rounds = 4;

    let space = TuneSearchSpace {
        concurrencies: vec![4, 8],
        chunks_bytes: vec![256 * 1024, 512 * 1024],
        gaps_bytes: vec![64 * 1024, 512 * 1024],
    };
    let engine = AutoTuneEngine::new(prof.sla.clone(), space);
    let outcome = engine
        .run_adaptive_sweep(&prof)
        .await
        .expect("autotune sweep should converge");
    assert!(outcome.trials.len() >= 4);
    assert!(outcome.generated_rust_code.contains("Operator::via_iter"));
    assert!(outcome.generated_python_code.contains("AsyncOperator"));

    let baseline = BenchmarkReport::from_autotune(prof, outcome);
    let finops = compare_multi_cloud_finops(&baseline.frame.transport, 0.05, 0.20);
    assert_eq!(finops.len(), 3);

    let tmp = tempfile::tempdir().expect("tempdir");
    let json_path = tmp.path().join("baseline.json");
    baseline.save_json(&json_path).expect("save json");
    let reloaded = BenchmarkReport::load_json(&json_path).expect("reload json");
    assert_eq!(reloaded.schema_version, "oprobe.report.v1");

    // Simulate a severe P99 and Read Amplification regression in candidate
    let mut degraded = reloaded.clone();
    degraded.frame.transport.read_amplification += 1.2;
    if let Some(first_verb) = degraded.frame.verbs.first_mut() {
        first_verb.p99_ms = first_verb.p99_ms * 2.0 + 10.0;
    }
    let summary = diff_reports(&reloaded, &degraded, &RegressionThresholds::default());
    assert!(!summary.passed_gate);
    assert!(!summary.reasons.is_empty());
}

#[test]
fn minibuffer_parser_handles_mutations_and_commands() {
    let mut prof = ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup);
    let mut knob = TuneKnob::default();

    let cmd = parse_minibuffer_command(":set bucket=analytics-prod").unwrap();
    let msg = apply_state_mutation(&cmd, &mut prof, &mut knob).unwrap();
    assert!(msg.contains("bucket=analytics-prod"));
    assert_eq!(
        prof.target.options.get("bucket").map(String::as_str),
        Some("analytics-prod")
    );

    let c2 = parse_minibuffer_command("/conc 48").unwrap();
    apply_state_mutation(&c2, &mut prof, &mut knob);
    assert_eq!(knob.concurrent, 48);

    assert_eq!(
        parse_minibuffer_command(":export out/snap").unwrap(),
        MinibufferCommand::ExportReport("out/snap".to_owned())
    );
}
