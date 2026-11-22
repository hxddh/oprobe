//! The newer workloads (batched deletes, copies, paged listings, presigned reads, suffix
//! footer reads, conditional and metadata writes) against `lab://` targets, with the API of
//! every request checked.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use oprobe::config::*;
use oprobe::engine::{Cancel, RunOptions, Session};
use oprobe::model::{OpRow, ReqRow};
use oprobe::util::ByteSize;

fn quiet() -> oprobe::engine::ProgressFn {
    Arc::new(|_| {})
}

struct Out {
    ops: Vec<OpRow>,
    reqs: Vec<ReqRow>,
    warnings: Vec<String>,
    cleaned: bool,
    findings: Vec<String>,
}

impl Out {
    fn apis(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for r in &self.reqs {
            *m.entry(r.api.to_string()).or_default() += 1;
        }
        m
    }
    fn failed(&self) -> Vec<&OpRow> {
        self.ops.iter().filter(|o| !o.ok).collect()
    }
}

async fn run(url: &str, preset: Preset, tweak: impl FnOnce(&mut Profile)) -> Out {
    let mut p = Profile::from_target(TargetSpec::from_url(url).unwrap());
    p.workload.preset = preset;
    let (mut ds, _) = preset.defaults();
    ds.objects = ds.objects.min(32);
    ds.object_size = ByteSize(ds.object_size.0.min(256 * 1024));
    p.workload.dataset = Some(ds);
    p.knobs.concurrency = 4;
    tweak(&mut p);
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let knobs = s.profile.knobs.clone();
    let opts = RunOptions {
        duration: Duration::from_millis(400),
        warmup: Duration::from_millis(50),
        max_ops: None,
        rate: None,
        trace_slow_ops: false,
    };
    let snap = s.run(&knobs, &opts, None).await.unwrap();
    let findings = oprobe::analysis::diagnosis::diagnose(&snap, &knobs, &s.profile.sla, true)
        .findings
        .into_iter()
        .map(|f| f.code)
        .collect();
    let t = s.take_trace().unwrap();
    let warnings = s.warnings.clone();
    let c = s.close().await;
    Out {
        ops: t.ops,
        reqs: t.reqs,
        warnings,
        cleaned: c.verified,
        findings,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn suffix_footer_reads_skip_the_head() {
    let plain = run("lab://clean/suffix", Preset::Parquet, |_| {}).await;
    let suffix = run("lab://clean/suffix", Preset::ParquetSuffix, |_| {}).await;
    for o in [&plain, &suffix] {
        assert!(o.failed().is_empty(), "{:?}", o.failed());
        assert!(o.cleaned);
    }
    assert!(
        plain.apis().contains_key("HeadObject"),
        "{:?}",
        plain.apis()
    );
    assert!(
        !suffix.apis().contains_key("HeadObject"),
        "{:?}",
        suffix.apis()
    );
    assert!(suffix.ops.iter().all(|o| o.verb == "parquet.scan.suffix"));
    // The plain scan gets the advice, the suffix scan does not need it.
    assert!(
        plain.findings.iter().any(|c| c == "suffix_read"),
        "{:?}",
        plain.findings
    );
    assert!(!suffix.findings.iter().any(|c| c == "suffix_read"));
    // One request fewer per file.
    let per_op = |o: &Out| o.reqs.len() as f64 / o.ops.len() as f64;
    assert!(
        per_op(&suffix) < per_op(&plain),
        "{} vs {}",
        per_op(&suffix),
        per_op(&plain)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn batched_deletes_remove_what_writes_created() {
    let o = run("lab://clean/delete", Preset::Delete, |p| {
        p.workload.mix = vec![
            OpSpec {
                weight: 4,
                op: OpKind::Write {
                    size: ByteSize::kib(1),
                },
            },
            OpSpec {
                weight: 1,
                op: OpKind::Delete { batch: 4 },
            },
        ];
    })
    .await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    let apis = o.apis();
    assert!(apis.contains_key("DeleteObjects"), "{apis:?}");
    assert!(o.ops.iter().any(|x| x.verb == "delete.batch"));
    assert!(o.cleaned);
}

#[tokio::test(flavor = "multi_thread")]
async fn copies_paged_listings_and_presigned_reads() {
    let o = run("lab://clean/copy", Preset::Copy, |_| {}).await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    assert!(o.apis().contains_key("CopyObject"), "{:?}", o.apis());
    assert!(o.cleaned);

    let o = run("lab://clean/paged", Preset::ListPaged, |p| {
        p.workload.dataset = Some(DatasetSpec {
            objects: 64,
            object_size: ByteSize::kib(1),
            partitions: 2,
            ..Default::default()
        });
        p.workload.mix = vec![OpSpec {
            weight: 1,
            op: OpKind::ListPaged { limit: 8 },
        }];
    })
    .await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    // 32 keys per partition, 8 per page: at least 4 billed pages per listing.
    let lists = o.reqs.iter().filter(|r| r.api == "ListObjectsV2").count();
    assert!(
        lists >= 4 * o.ops.len(),
        "{lists} pages for {} listings",
        o.ops.len()
    );

    let o = run("lab://clean/presign", Preset::Presign, |_| {}).await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    let apis = o.apis();
    assert!(apis.contains_key("PresignedGetObject"), "{apis:?}");
    assert!(!apis.contains_key("GetObject"), "{apis:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn conditional_and_metadata_writes() {
    let o = run("lab://clean/cond", Preset::Conditional, |_| {}).await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    // Creates, then If-Match overwrites of the same key: few distinct keys.
    let keys: std::collections::BTreeSet<&str> = o.ops.iter().map(|x| x.key.as_str()).collect();
    assert!(
        o.ops.len() >= 4 * keys.len(),
        "{} keys for {} ops",
        keys.len(),
        o.ops.len()
    );

    let o = run("lab://clean/meta", Preset::MetadataHeavy, |_| {}).await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    let apis = o.apis();
    assert!(
        apis.contains_key("HeadObject") && apis.contains_key("PutObject"),
        "{apis:?}"
    );
    assert!(o.warnings.is_empty(), "{:?}", o.warnings);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_operations_are_adapted_on_memory() {
    let o = run("memory://", Preset::Presign, |_| {}).await;
    assert!(o.failed().is_empty());
    assert!(
        o.warnings.iter().any(|w| w.contains("presign")),
        "{:?}",
        o.warnings
    );
    assert!(o.ops.iter().all(|x| x.verb == "read.full"));
}

#[test]
fn lab_urls_resolve_to_an_in_process_server() {
    let t = TargetSpec::from_url("lab://demo/some/prefix").unwrap();
    assert_eq!(t.scheme, "s3");
    assert_eq!(t.options["root"], "/some/prefix");
    assert!(t.endpoint().unwrap().starts_with("http://127.0.0.1:"));
    assert_eq!(t.proxy.as_deref(), Some("none"));
    let n = t.normalized().unwrap();
    assert_eq!(n.display(), "lab://demo/some/prefix");
    // The same server for the same scenario; another one for another scenario.
    let again = TargetSpec::from_url("lab://demo").unwrap();
    assert_eq!(again.endpoint(), t.endpoint());
    let clean = TargetSpec::from_url("lab://").unwrap();
    assert_ne!(clean.endpoint(), t.endpoint());
    // A profile keeps only the URL.
    let toml = Profile::from_target(n).to_toml().unwrap();
    assert!(
        !toml.contains("endpoint") && !toml.contains("secret"),
        "{toml}"
    );
    let e = full(TargetSpec::from_url("lab://nope").unwrap_err());
    assert!(
        e.contains("unknown lab scenario") && e.contains("stragglers"),
        "{e}"
    );
}

#[test]
fn every_compiled_service_resolves_from_its_url() {
    if oprobe::engine::operator::missing_service_feature("azdls").is_none() {
        let azdls = TargetSpec::from_url("azdls://acct.dfs.core.windows.net/fs/data")
            .unwrap()
            .normalized()
            .unwrap();
        assert_eq!(azdls.scheme, "azdls");
        assert_eq!(azdls.options["filesystem"], "fs");
        assert_eq!(azdls.options["account_name"], "acct");
    } else {
        // An optional service not compiled in: refused with the feature to build it with.
        let e = TargetSpec::from_url("azdls://acct.dfs.core.windows.net/fs/data")
            .and_then(|t| t.normalized())
            .and_then(|t| oprobe::engine::operator::base_operator(&t).map(|_| ()))
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("azdls"), "{e}");
    }
    let s3 = TargetSpec::from_url("s3://b/p?region=eu-west-1&enable_virtual_host_style=true")
        .unwrap()
        .normalized()
        .unwrap();
    assert_eq!(s3.options["region"], "eu-west-1");
    assert_eq!(s3.options["enable_virtual_host_style"], "true");
    // Missing required options are named.
    let e = TargetSpec::from_url("swift://")
        .and_then(|t| t.normalized())
        .unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("swift"), "{e}");
    let e = TargetSpec::from_url("obs://bkt")
        .and_then(|t| t.normalized())
        .map(|t| oprobe::engine::operator::base_operator(&t).map(|_| ()))
        .unwrap()
        .unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("endpoint"), "{e}");
    let e = full(TargetSpec::from_url("ftp://x").unwrap_err());
    assert!(
        e.contains("unsupported storage scheme") && e.contains("azdls"),
        "{e}"
    );
}

#[test]
fn provider_knobs_become_s3_options() {
    let k = ProviderKnobs {
        addressing: Addressing::Virtual,
        checksum: Some("CRC32C".into()),
        sse: Some("AES256".into()),
    };
    let o = k.options("s3").unwrap();
    assert!(o.contains(&("enable_virtual_host_style", "true".into())));
    assert!(o.contains(&("checksum_algorithm", "crc32c".into())));
    assert!(o.contains(&("server_side_encryption", "AES256".into())));
    assert!(k.options("gcs").is_err());
    assert!(ProviderKnobs::default().options("gcs").unwrap().is_empty());
    let bad = ProviderKnobs {
        checksum: Some("md5".into()),
        ..Default::default()
    };
    assert!(bad.options("s3").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn checksums_are_sent_and_verified_by_the_lab() {
    let o = run("lab://clean/crc", Preset::SmallFiles, |p| {
        p.knobs.provider.checksum = Some("crc32c".into());
    })
    .await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    assert!(o.apis().contains_key("PutObject"));
}

#[tokio::test(flavor = "multi_thread")]
async fn lab_scenarios_inject_their_faults() {
    async fn snap(url: &str, concurrency: usize) -> oprobe::trace::Snapshot {
        let mut p = Profile::from_target(TargetSpec::from_url(url).unwrap());
        p.workload.preset = Preset::SmallFiles;
        p.workload.dataset = Some(DatasetSpec {
            objects: 8,
            object_size: ByteSize::kib(16),
            ..Default::default()
        });
        p.knobs.concurrency = concurrency;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = s.profile.knobs.clone();
        let opts = RunOptions {
            duration: Duration::from_millis(600),
            warmup: Duration::ZERO,
            max_ops: None,
            rate: None,
            trace_slow_ops: false,
        };
        let out = s.run(&knobs, &opts, None).await.unwrap();
        s.close().await;
        out
    }
    // flaky: 500s and resets, absorbed by retries.
    let s = snap("lab://flaky/t", 4).await;
    assert!(s.resilience.server_errors > 0, "{:?}", s.resilience);
    assert!(s.resilience.network_errors > 0, "{:?}", s.resilience);
    assert!(s.resilience.retries > 0);
    // demo: throttling above 24 concurrent requests, ~2.5% stragglers of 250 ms.
    let s = snap("lab://demo/t", 48).await;
    assert!(s.resilience.throttled_responses > 0, "{:?}", s.resilience);
    assert!(s.summary.latency.max >= 250.0, "{:?}", s.summary.latency);
    // clean: nothing.
    let s = snap("lab://clean/t", 8).await;
    assert_eq!(
        s.resilience.throttled_responses + s.resilience.server_errors,
        0
    );
    assert_eq!(s.summary.failed_ops, 0);
}

/// An error's message and hint (typed errors carry the choices in the hint).
fn full(e: anyhow::Error) -> String {
    let b = oprobe::errors::classify(&e, "s3");
    assert_eq!(
        b.code,
        oprobe::errors::ErrorCode::UsageInvalidArgument,
        "{}",
        b.message
    );
    format!("{} {}", b.message, b.hint.unwrap_or_default())
}

/// Peak resident memory of the largest child process this test waited for (Linux: KiB).
#[cfg(target_os = "linux")]
fn children_max_rss_mib() -> f64 {
    // SAFETY: getrusage(RUSAGE_CHILDREN) fills the zero-initialised struct it is given.
    let r = unsafe {
        let mut r: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut r);
        r
    };
    r.ru_maxrss as f64 / 1024.0
}

/// Design v5 §2 (fixes M3): the run file is written while the run is in progress, so memory
/// stays flat however many rows the run produces (4.x held up to 1M rows, ~1 KB each).
#[cfg(target_os = "linux")]
#[test]
fn a_streamed_run_keeps_memory_flat() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .args([
            "run",
            "memory://",
            "--preset",
            "metadata",
            "-d",
            "10s",
            "--warmup",
            "0s",
            "-c",
            "8",
            "-o",
            "json",
        ])
        .env("OPROBE_DATA_DIR", dir.path())
        .env("OPROBE_CONFIG", dir.path().join("config.toml"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rss = children_max_rss_mib();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let r = v.get("report").unwrap_or(&v);
    let seen = r["sampling"]["seen_requests_count"].as_u64().unwrap();
    let kept = r["sampling"]["kept_requests_count"].as_u64().unwrap();
    // The file holds every request (up to trace.max_requests), more than memory keeps.
    let run_file = std::fs::read_dir(dir.path().join("runs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "oprobe"))
        .unwrap()
        .path();
    let run = oprobe::store::read_run_file(&run_file).unwrap();
    assert_eq!(run.reqs.len() as u64, kept);
    if seen > 2 * oprobe::store::runfile::STREAM_MEMORY_ROWS as u64 {
        assert!(
            kept > oprobe::store::runfile::STREAM_MEMORY_ROWS as u64,
            "{kept} of {seen}"
        );
    }
    // 4.x held ~1 KB per row on top of the process (~150 MiB here): allow half of that
    // growth (measured: ~150 MiB flat for 400k requests). Too few requests (a starved
    // machine) cannot tell the two apart.
    eprintln!("peak RSS {rss:.0} MiB, {seen} requests seen, {kept} in the run file");
    if seen >= 200_000 {
        let budget = 160.0 + 0.5 * seen.min(1_000_000) as f64 / 1024.0;
        assert!(
            rss < budget,
            "peak RSS {rss:.0} MiB for {seen} requests (budget {budget:.0} MiB)"
        );
    }
}
