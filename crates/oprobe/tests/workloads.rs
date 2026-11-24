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

/// The operation mixes of the 5.x presets (still available through `--set mix=…`).
fn legacy(name: &str) -> (DatasetSpec, Vec<OpSpec>) {
    let ds = |objects, size| DatasetSpec {
        objects,
        object_size: size,
        ..DatasetSpec::default()
    };
    let one = |op| vec![OpSpec { weight: 100, op }];
    match name {
        "suffix" => (
            ds(8, ByteSize::mib(16)),
            one(OpKind::ParquetScan {
                columns: 4,
                column_size: ByteSize::kib(512),
                footer_size: ByteSize::kib(64),
                suffix: true,
            }),
        ),
        "delete" => (
            ds(1, ByteSize::kib(4)),
            vec![
                OpSpec {
                    weight: 95,
                    op: OpKind::Write {
                        size: ByteSize::kib(4),
                    },
                },
                OpSpec {
                    weight: 5,
                    op: OpKind::Delete { batch: 20 },
                },
            ],
        ),
        "copy" => (ds(16, ByteSize::mib(1)), one(OpKind::Copy)),
        "list-paged" => (
            DatasetSpec {
                objects: 512,
                object_size: ByteSize::kib(1),
                partitions: 4,
                ..DatasetSpec::default()
            },
            one(OpKind::ListPaged { limit: 32 }),
        ),
        "presign" => (ds(32, ByteSize::kib(256)), one(OpKind::PresignedRead)),
        "conditional" => (
            ds(1, ByteSize::kib(16)),
            one(OpKind::ConditionalWrite {
                size: ByteSize::kib(16),
            }),
        ),
        "metadata-heavy" => (
            DatasetSpec {
                objects: 256,
                object_size: ByteSize::kib(4),
                partitions: 8,
                ..DatasetSpec::default()
            },
            vec![
                OpSpec {
                    weight: 60,
                    op: OpKind::Stat,
                },
                OpSpec {
                    weight: 40,
                    op: OpKind::MetadataWrite {
                        size: ByteSize::kib(4),
                        entries: 8,
                    },
                },
            ],
        ),
        other => panic!("no legacy workload {other}"),
    }
}

async fn run(url: &str, preset: Preset, tweak: impl FnOnce(&mut Profile)) -> Out {
    run_mix(url, preset.defaults(), tweak).await
}

async fn run_legacy(url: &str, name: &str, tweak: impl FnOnce(&mut Profile)) -> Out {
    run_mix(url, legacy(name), tweak).await
}

async fn run_mix(
    url: &str,
    (ds, mix): (DatasetSpec, Vec<OpSpec>),
    tweak: impl FnOnce(&mut Profile),
) -> Out {
    let mut p = Profile::from_url(url);
    p.workload.mix = mix;
    let mut ds = ds;
    ds.objects = ds.objects.min(32);
    ds.object_size = ByteSize(ds.object_size.0.min(256 * 1024));
    p.workload.dataset = Some(ds);
    p.workload.concurrency = 4;
    tweak(&mut p);
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let knobs = oprobe::config::Knobs::of(&s.profile);
    let opts = RunOptions {
        duration: Duration::from_millis(400),
        warmup: Duration::from_millis(50),
        max_ops: None,
        rate: None,
        trace_slow_ops: false,
    };
    let snap = s.run(&knobs, &opts, None).await.unwrap();
    let t = s.take_trace().unwrap();
    let report = oprobe::model::build_run(oprobe::model::ReportInput {
        run_id: &oprobe::model::RunId::new(),
        target_name: None,
        profile: &s.profile,
        knobs: &knobs,
        snapshot: snap,
        trace: t.clone(),
        cleanup: Default::default(),
        warnings: vec![],
        started_at: std::time::SystemTime::now(),
        ended_at: std::time::SystemTime::now(),
        spec: None,
    })
    .report;
    let findings = report.findings.into_iter().map(|f| f.code).collect();
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
    let plain = run("lab://clean/suffix", Preset::RangeReads, |_| {}).await;
    let suffix = run_legacy("lab://clean/suffix", "suffix", |_| {}).await;
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
    // Every finding is a 6.0 code.
    for c in plain.findings.iter().chain(&suffix.findings) {
        assert!(
            oprobe::analysis::findings::CODES.contains(&c.as_str()),
            "{c}"
        );
    }
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
    let o = run_legacy("lab://clean/delete", "delete", |p| {
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
    let o = run_legacy("lab://clean/copy", "copy", |_| {}).await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    assert!(o.apis().contains_key("CopyObject"), "{:?}", o.apis());
    assert!(o.cleaned);

    let o = run_legacy("lab://clean/paged", "list-paged", |p| {
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

    let o = run_legacy("lab://clean/presign", "presign", |_| {}).await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    let apis = o.apis();
    assert!(apis.contains_key("PresignedGetObject"), "{apis:?}");
    assert!(!apis.contains_key("GetObject"), "{apis:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn conditional_and_metadata_writes() {
    let o = run_legacy("lab://clean/cond", "conditional", |_| {}).await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    // Creates, then If-Match overwrites of the same key: few distinct keys.
    let keys: std::collections::BTreeSet<&str> = o.ops.iter().map(|x| x.key.as_str()).collect();
    assert!(
        o.ops.len() >= 4 * keys.len(),
        "{} keys for {} ops",
        keys.len(),
        o.ops.len()
    );

    let o = run_legacy("lab://clean/meta", "metadata-heavy", |_| {}).await;
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
    let o = run_legacy("memory://", "presign", |_| {}).await;
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
    // An unknown scheme lists the schemes of this build, and only those.
    let e = full(TargetSpec::from_url("ftp://x").unwrap_err());
    assert!(
        e.contains("unsupported storage scheme") && e.contains("s3"),
        "{e}"
    );
    let compiled = oprobe::engine::operator::missing_service_feature("azdls").is_none();
    assert_eq!(e.contains("azdls"), compiled, "{e}");
    // A pasted cloud address is the bucket's URL.
    let t = TargetSpec::from_url("https://bkt.s3.eu-west-1.amazonaws.com/data")
        .unwrap()
        .normalized()
        .unwrap();
    assert_eq!(t.scheme, "s3");
    assert_eq!(t.options["bucket"], "bkt");
    assert_eq!(t.options["root"], "/data");
    assert_eq!(t.options["region"], "eu-west-1");
    let t = TargetSpec::from_url("https://acct.blob.core.windows.net/cont/p")
        .unwrap()
        .normalized()
        .unwrap();
    assert_eq!(t.scheme, "azblob");
    assert_eq!(t.options["container"], "cont");
    assert_eq!(t.options["endpoint"], "https://acct.blob.core.windows.net");
    let t = TargetSpec::from_url("https://storage.googleapis.com/gb/x")
        .unwrap()
        .normalized()
        .unwrap();
    assert_eq!(
        (t.scheme.as_str(), t.options["bucket"].as_str()),
        ("gcs", "gb")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn checksums_are_sent_and_verified_by_the_lab() {
    let o = run("lab://clean/crc", Preset::SmallFiles, |p| {
        p.operator
            .options
            .insert("checksum_algorithm".into(), "crc32c".into());
    })
    .await;
    assert!(o.failed().is_empty(), "{:?}", o.failed());
    assert!(o.apis().contains_key("PutObject"));
}

#[tokio::test(flavor = "multi_thread")]
async fn lab_scenarios_inject_their_faults() {
    async fn snap(url: &str, concurrency: usize) -> oprobe::trace::Snapshot {
        let mut p = Profile::from_url(url);
        p.workload.preset = Preset::SmallFiles;
        p.workload.dataset = Some(DatasetSpec {
            objects: 8,
            object_size: ByteSize::kib(16),
            ..Default::default()
        });
        p.workload.concurrency = concurrency;
        let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
        let knobs = oprobe::config::Knobs::of(&s.profile);
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
    // demo: throttling above 40 concurrent requests (48 workers here), ~2.5% stragglers of 250 ms.
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

/// `lab://flaky` injects what its description says (K4: 7.0 answered 2 % 500s, not 4 %: every
/// second error request was a reset instead). Plain HTTP requests, one connection each.
#[tokio::test(flavor = "multi_thread")]
async fn the_flaky_lab_injects_its_described_rates() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let sc = oprobe::lab::Scenario::Flaky;
    assert!(sc.describe().contains("4% 500") && sc.describe().contains("2% connection resets"));
    let lab = oprobe::lab::LabServer::spawn("127.0.0.1:0", sc.config())
        .await
        .unwrap();
    let addr = lab.endpoint_url.trim_start_matches("http://").to_owned();
    let (mut errors, mut resets) = (0, 0);
    let n = 400;
    for i in 0..n {
        let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let req = format!("GET /b/k{i} HTTP/1.1\r\nhost: lab\r\nconnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        let head = String::from_utf8_lossy(&buf);
        if head.starts_with("HTTP/1.1 500") {
            errors += 1;
        } else if !head.starts_with("HTTP/") {
            resets += 1;
        }
    }
    assert!((14..=18).contains(&errors), "{errors} 500s in {n} requests");
    assert!(
        (6..=10).contains(&resets),
        "{resets} resets in {n} requests"
    );
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
            "list-stat",
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
    let r = &v["runs"][0]["report"];
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
