//! Target safety (9.2): a run never leaves a multipart upload behind, and its budgets hold on
//! the wire. Every count asserted here is the lab's own (server side), not the client's.

use std::sync::Arc;
use std::time::Duration;

use oprobe::config::{DatasetSpec, OpKind, OpSpec, Profile};
use oprobe::engine::measure::{Measured, Rails, Request, guard_for, measure};
use oprobe::engine::{Cancel, RunOptions, Session};
use oprobe::errors::exit;
use oprobe::guard::Budgets;
use oprobe::lab::{LabConfig, LabHandle, LabServer, PartFault, PartFaultKind};
use oprobe::operator::Timeout;
use oprobe::util::{ByteSize, HumanDuration};

const MIB: u64 = 1024 * 1024;
/// The part size of the multipart writes: S3's minimum, which OpenDAL enforces.
const PART: u64 = 5 * MIB;

fn quiet() -> oprobe::engine::ProgressFn {
    Arc::new(|_| {})
}

async fn lab(part_fault: Option<PartFaultKind>) -> LabHandle {
    let faults = LabConfig {
        throttle_status_code: 503,
        // The first part of every upload is stored; the second one breaks.
        part_fault: part_fault.map(|kind| PartFault { from_part: 2, kind }),
        ..Default::default()
    };
    LabServer::spawn("127.0.0.1:0", faults)
        .await
        .expect("lab starts")
}

/// One worker writing `parts`-part multipart objects, with no retries: what a fault does to
/// an upload is what the run sees.
fn multipart_writes(lab: &LabHandle, parts: u64) -> Profile {
    let mut p = lab.profile("bkt");
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::MultipartWrite {
            size: ByteSize(parts * PART),
        },
    }];
    p.workload.concurrency = 1;
    p.workload.warmup = HumanDuration(Duration::ZERO);
    p.workload.duration = HumanDuration::secs(120);
    p.workload.verify = false;
    p.operator.write.chunk = Some(ByteSize(PART));
    p.operator.layers.retry = None;
    p
}

async fn measured(p: Profile, budgets: Budgets, cancel: Cancel) -> Measured {
    let rails = Rails::new(&p, false, None, budgets.clone()).unwrap();
    let mut req = Request::new(p.clone(), rails, cancel, quiet());
    req.guard = guard_for(&budgets, &p);
    // A fix that does not hold would hang here (9.1 drained stalled operations for ever).
    tokio::time::timeout(Duration::from_secs(120), measure(req))
        .await
        .expect("the run ends")
        .expect("the run is measured")
}

/// The run left nothing on the target: no upload in progress, no object.
#[track_caller]
fn assert_nothing_left(lab: &LabHandle) {
    let c = lab.multipart_counts();
    assert_eq!(lab.open_uploads(), 0, "orphan multipart uploads: {c:?}");
    assert_eq!(lab.stored_object_count(), 0, "{:?}", lab.stored_keys());
}

/// Poll the lab until `ready` (a state the test then acts on; never a race against it).
async fn until(lab: &LabHandle, what: &str, ready: impl Fn(&LabHandle) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !ready(lab) {
        assert!(
            std::time::Instant::now() < deadline,
            "the lab never saw {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A multipart write that fails mid-way (its second part is refused) aborts its upload:
/// 9.1 dropped the writer, and the first part stayed on the target, billed and unlisted.
#[tokio::test(flavor = "multi_thread")]
async fn a_multipart_write_that_fails_midway_aborts_its_upload() {
    let lab = lab(Some(PartFaultKind::Deny)).await;
    let mut p = multipart_writes(&lab, 4);
    p.workload.max_ops = Some(3);
    let m = measured(p, Budgets::default(), Cancel::new()).await;
    let s = &m.run.report.summary;
    assert_eq!((s.ops, s.failed_ops), (3, 3), "every write fails: {s:?}");
    let c = lab.multipart_counts();
    assert_eq!(c.created, 3, "{c:?}");
    assert_eq!(c.aborted, c.created, "every upload was aborted: {c:?}");
    assert_eq!(c.completed, 0, "{c:?}");
    assert_nothing_left(&lab);
    assert!(
        m.run.report.cleanup.verified && m.run.report.cleanup.error.is_none(),
        "{:?}",
        m.run.report.cleanup
    );
}

/// A multipart write cut by the operator's timeout (a part never answers) aborts its upload.
#[tokio::test(flavor = "multi_thread")]
async fn a_multipart_write_cut_by_a_timeout_aborts_its_upload() {
    let lab = lab(Some(PartFaultKind::Stall)).await;
    let mut p = multipart_writes(&lab, 4);
    p.workload.max_ops = Some(1);
    p.operator.layers.timeout = Some(Timeout {
        op: HumanDuration::secs(30),
        io: HumanDuration(Duration::from_millis(300)),
    });
    let m = measured(p, Budgets::default(), Cancel::new()).await;
    let s = &m.run.report.summary;
    assert_eq!((s.ops, s.failed_ops), (1, 1), "{s:?}");
    let c = lab.multipart_counts();
    assert_eq!((c.created, c.aborted), (1, 1), "{c:?}");
    assert_nothing_left(&lab);
}

/// A run cancelled in the middle of an upload (a part is on the wire and never answers) cuts
/// the request, aborts the upload and ends: 9.1 waited for the operation to finish (here: for
/// ever) and aborted nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_cancelled_mid_upload_aborts_it() {
    let lab = lab(Some(PartFaultKind::Stall)).await;
    let mut p = multipart_writes(&lab, 4);
    // Nothing but the cancellation ends the stalled part.
    p.operator.layers.timeout = None;
    let cancel = Cancel::new();
    let run = tokio::spawn(measured(p, Budgets::default(), cancel.clone()));
    until(&lab, "the stalled part", |l| {
        l.multipart_counts().parts >= 2
    })
    .await;
    assert_eq!(lab.open_uploads(), 1);
    cancel.cancel();
    let m = run.await.unwrap();
    assert!(m.cancelled);
    assert!(m.run.report.is_partial());
    // The operation the cancellation stopped did not fail: it is not in the window at all.
    let s = &m.run.report.summary;
    assert_eq!((s.ops, s.failed_ops), (0, 0), "{s:?}");
    let c = lab.multipart_counts();
    assert_eq!((c.created, c.aborted, c.completed), (1, 1, 0), "{c:?}");
    assert_nothing_left(&lab);
}

/// A dataset upload cancelled in the middle of its multipart uploads aborts them and removes
/// what it uploaded: 9.1 dropped the writers where they were.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_dataset_upload_aborts_its_uploads() {
    let lab = lab(Some(PartFaultKind::Stall)).await;
    let mut p = lab.profile("bkt");
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::Read { size: None },
    }];
    // Two parts of 8 MiB each per object: the dataset uploads are multipart.
    p.workload.dataset = Some(DatasetSpec {
        objects: 3,
        object_size: ByteSize(16 * MIB),
        ..Default::default()
    });
    let cancel = Cancel::new();
    let open = tokio::spawn(Session::open(p, cancel.clone(), quiet()));
    until(&lab, "three uploads with a stalled part", |l| {
        l.open_uploads() == 3 && l.multipart_counts().parts >= 6
    })
    .await;
    cancel.cancel();
    let e = tokio::time::timeout(Duration::from_secs(120), open)
        .await
        .expect("the upload ends")
        .unwrap()
        .err()
        .expect("the session does not open");
    assert!(format!("{e:#}").contains("cancelled"), "{e:#}");
    let c = lab.multipart_counts();
    assert_eq!((c.created, c.aborted), (3, 3), "{c:?}");
    assert_nothing_left(&lab);
}

/// `--max-requests` holds on the wire: against 10-request operations, a budget of 8 sends 8
/// requests and not one more (9.1 counted an operation when it had finished: it sent all 10
/// of the first one, and let whatever was in flight finish). The upload the budget stopped is
/// aborted, the partial run is reported, and the exit code is 4.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_budget_holds_inside_a_multipart_operation() {
    let lab = lab(None).await;
    let p = multipart_writes(&lab, 8);
    let budgets = Budgets {
        max_requests_count: Some(8),
        ..Default::default()
    };
    let m = measured(p, budgets, Cancel::new()).await;
    let b = m.breach.as_ref().expect("the budget stopped the run");
    assert_eq!(
        (b.budget.as_str(), b.limit, b.observed),
        ("requests", 8.0, 8.0)
    );
    assert_eq!(m.run.report.verdict.exit_code, exit::BUDGET);
    assert!(m.run.report.is_partial());
    let c = lab.multipart_counts();
    // The reachability probe and the preflight count too: the first upload gets what they
    // left of the 8 requests, and never completes.
    assert!(c.parts >= 1, "the write started: {c:?}");
    assert!(c.requests() <= 8, "{c:?}");
    assert_eq!((c.completed, c.created, c.aborted), (0, 1, 1), "{c:?}");
    assert_nothing_left(&lab);
    assert!(m.run.report.cleanup.verified, "{:?}", m.run.report.cleanup);
}

/// `--max-bytes` holds on the wire: of a 40 MiB multipart write, a 12 MiB budget lets two
/// 5 MiB parts leave and refuses the third (9.1 sent all 40 MiB, then stopped).
#[tokio::test(flavor = "multi_thread")]
async fn a_byte_budget_holds_inside_a_multipart_operation() {
    let lab = lab(None).await;
    let p = multipart_writes(&lab, 8);
    let budgets = Budgets {
        max_bytes: Some(12 * MIB),
        ..Default::default()
    };
    let m = measured(p, budgets, Cancel::new()).await;
    let b = m.breach.as_ref().expect("the budget stopped the run");
    assert_eq!(b.budget, "bytes");
    assert!(b.observed <= b.limit, "nothing past the budget: {b:?}");
    assert_eq!(m.run.report.verdict.exit_code, exit::BUDGET);
    let c = lab.multipart_counts();
    assert_eq!((c.parts, c.part_bytes), (2, 2 * PART), "{c:?}");
    assert_eq!((c.completed, c.aborted), (0, 1), "{c:?}");
    assert_nothing_left(&lab);
}

/// Dataset preparation counts against the budgets (9.1 did not count it at all): a dataset
/// the budget has no room for stops there, with exit 4 and nothing left behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_budget_covers_the_dataset_upload() {
    let lab = lab(None).await;
    let mut p = lab.profile("bkt");
    p.workload.mix = vec![OpSpec {
        weight: 1,
        op: OpKind::Read { size: None },
    }];
    p.workload.dataset = Some(DatasetSpec {
        objects: 40,
        object_size: ByteSize::kib(4),
        ..Default::default()
    });
    let budgets = Budgets {
        max_requests_count: Some(12),
        ..Default::default()
    };
    let rails = Rails::new(&p, false, None, budgets.clone()).unwrap();
    let mut req = Request::new(p.clone(), rails, Cancel::new(), quiet());
    req.guard = guard_for(&budgets, &p);
    let guard = req.guard.clone().unwrap();
    let e = measure(req)
        .await
        .err()
        .expect("the budget stops the upload");
    let c = oprobe::errors::classify(&e, "s3");
    assert_eq!(
        c.code,
        oprobe::errors::ErrorCode::SafetyBudgetExceeded,
        "{e:#}"
    );
    assert_eq!(guard.usage().0, 12);
    assert_nothing_left(&lab);
}

/// Conditional and metadata writes write the size they were asked to: 9.1 sized the payload
/// pool from plain and multipart writes only, and cut these down to about 1 MiB without a word.
#[tokio::test(flavor = "multi_thread")]
async fn large_conditional_and_metadata_writes_write_their_size() {
    let lab = lab(None).await;
    let mut p = lab.profile("bkt");
    let (conditional, metadata) = (3 * MIB, 2 * MIB + 4096);
    p.workload.mix = vec![
        OpSpec {
            weight: 1,
            op: OpKind::ConditionalWrite {
                size: ByteSize(conditional),
            },
        },
        OpSpec {
            weight: 1,
            op: OpKind::MetadataWrite {
                size: ByteSize(metadata),
                entries: 2,
            },
        },
    ];
    p.workload.concurrency = 2;
    let s = Session::open(p, Cancel::new(), quiet()).await.unwrap();
    let knobs = oprobe::config::Knobs::of(&s.profile);
    let opts = RunOptions {
        duration: Duration::from_secs(60),
        warmup: Duration::ZERO,
        max_ops: Some(24),
        rate: None,
        trace_slow_ops: false,
    };
    let snap = s.run(&knobs, &opts, None).await.unwrap();
    assert_eq!(snap.summary.failed_ops, 0, "{:?}", snap.resilience);
    let ops = s.take_trace().unwrap().ops;
    for (verb, size) in [
        ("write.conditional", conditional),
        ("write.metadata", metadata),
    ] {
        let written: Vec<u64> = ops
            .iter()
            .filter(|o| o.verb == verb)
            .map(|o| o.bytes)
            .collect();
        assert!(!written.is_empty(), "no {verb} ran");
        assert!(written.iter().all(|b| *b == size), "{verb}: {written:?}");
    }
    // What the lab received is what was reported.
    assert!(s.close().await.verified);
}

/// SIGTERM and SIGHUP stop `oprobe run` as Ctrl-C does: what is in flight is cut, the
/// multipart upload is aborted, the test data is deleted, and the exit code is 130. In 9.1
/// only SIGINT was handled: the others killed the process where it stood.
#[cfg(unix)]
#[test]
fn sigterm_and_sighup_stop_a_run_cleanly() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let mix = serde_json::to_string(&vec![OpSpec {
        weight: 1,
        op: OpKind::MultipartWrite {
            size: ByteSize(4 * PART),
        },
    }])
    .unwrap();
    for signal in ["TERM", "HUP"] {
        let lab = rt.block_on(lab(Some(PartFaultKind::Stall)));
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", home.path().join("data"))
            .env("OPROBE_CONFIG", home.path().join("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["run", "s3://bkt/x", "-c", "1", "-d", "120s", "-q"])
            .args(["--color", "never", "--warmup", "0s"])
            .args(["--set", &format!("options.endpoint={}", lab.endpoint_url)])
            .args([
                "--set",
                "http.proxy=none",
                "--set",
                "options.region=us-east-1",
            ])
            .args(["--set", "options.access_key_id=x"])
            .args(["--set", "options.secret_access_key=x"])
            .args(["--set", &format!("mix={mix}"), "--set", "write.chunk=5MiB"])
            .args(["--set", "layers.retry=off", "--set", "layers.timeout=off"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The run is in the middle of an upload: a part is on the wire and never answers.
        rt.block_on(until(&lab, "the stalled part", |l| {
            l.multipart_counts().parts >= 2
        }));
        let killed = std::process::Command::new("kill")
            .args([&format!("-{signal}"), &child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(130), "SIG{signal}: {err}");
        let c = lab.multipart_counts();
        assert_eq!((c.created, c.aborted), (1, 1), "SIG{signal}: {c:?}");
        assert_nothing_left(&lab);
    }
}
