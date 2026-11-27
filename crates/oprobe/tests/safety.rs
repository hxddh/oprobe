//! Target safety (9.2, 9.3): a run never leaves a multipart upload or an object behind,
//! however it is stopped, and its budgets hold on the wire. Every count asserted here is the
//! lab's own (server side), not the client's.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use oprobe::config::{DatasetSpec, OpKind, OpSpec, Profile};
use oprobe::engine::measure::{Measured, Rails, Request, Stage, guard_for, measure};
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
    lab_with(LabConfig {
        // The first part of every upload is stored; the second one breaks.
        part_fault: part_fault.map(|kind| PartFault { from_part: 2, kind }),
        ..Default::default()
    })
    .await
}

async fn lab_with(faults: LabConfig) -> LabHandle {
    let faults = LabConfig {
        throttle_status_code: 503,
        ..faults
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
    let mut req = Request::new(p, rails, cancel, quiet());
    req.guard = guard_for(&budgets);
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
    // Seconds, not a fraction of one: the 5 MiB part before the stalled one must never time
    // out, however slow the machine. The stall is for ever, so the timeout always fires.
    p.operator.layers.timeout = Some(Timeout {
        op: HumanDuration::secs(60),
        io: HumanDuration::secs(3),
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
    // Stopped, not failed: 9.2 said "cancelled while uploading the dataset", an `internal`
    // error (exit 1) for the app's Stop.
    let body = oprobe::errors::classify(&e, "s3");
    assert_eq!(
        body.code,
        oprobe::errors::ErrorCode::RunInterrupted,
        "{e:#}"
    );
    assert_eq!(body.message, "run interrupted");
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
    let lab = Arc::new(lab(None).await);
    let p = multipart_writes(&lab, 8);
    let budgets = Budgets {
        max_requests_count: Some(8),
        ..Default::default()
    };
    let rails = Rails::new(&p, false, None, budgets.clone()).unwrap();
    let mut req = Request::new(p, rails, Cancel::new(), quiet());
    req.guard = guard_for(&budgets);
    // Every request the lab has served when the window has ended, before cleanup lists and
    // deletes the run prefix.
    let served = Arc::new(AtomicU64::new(0));
    let (at_the_end, target) = (served.clone(), lab.clone());
    req.on_stage = Some(Arc::new(move |stage| {
        if stage == Stage::Measured {
            at_the_end.store(target.requests_served(), Ordering::SeqCst);
        }
    }));
    let m = tokio::time::timeout(Duration::from_secs(120), measure(req))
        .await
        .expect("the run ends")
        .expect("the run is measured");
    let b = m.breach.as_ref().expect("the budget stopped the run");
    assert_eq!(
        (b.budget.as_str(), b.limit, b.observed),
        ("requests", 8.0, 8.0)
    );
    assert_eq!(m.run.report.verdict.exit_code, exit::BUDGET);
    assert!(m.run.report.is_partial());
    let c = lab.multipart_counts();
    // The reachability probe and the preflight count too (one request, then a list and a
    // write): the first upload gets the other 5 of the 8 requests, and never completes.
    assert_eq!((c.created, c.parts), (1, 4), "{c:?}");
    assert_eq!((c.completed, c.aborted), (0, 1), "{c:?}");
    // The lab's total, not only its multipart requests (9.2 asserted `c.requests() <= 8`,
    // which a ninth request of any other kind would have passed): the 8 of the budget, and
    // the two that are cleanup, which no budget counts: the preflight's probe object deleted,
    // the upload aborted.
    assert_eq!(served.load(Ordering::SeqCst), 8 + 2, "{c:?}");
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
    let mut req = Request::new(p, rails, Cancel::new(), quiet());
    req.guard = guard_for(&budgets);
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

/// K1-4: the preflight's probe object never stays. 9.2 deleted it through the run's gate: with
/// `--max-requests 3` the write spent the budget, the delete was refused, and since a run
/// that does not open has no cleanup, `oprobe-run-<id>/.oprobe-preflight` was left on the
/// target. Whatever request the budget ends at, nothing is left.
#[tokio::test(flavor = "multi_thread")]
async fn the_preflight_probe_object_is_deleted_whatever_the_budget_stops() {
    for max in 1..=8 {
        let lab = lab(None).await;
        let mut p = lab.profile("bkt");
        p.workload.mix = vec![OpSpec {
            weight: 1,
            op: OpKind::Read { size: None },
        }];
        p.workload.dataset = Some(DatasetSpec {
            objects: 4,
            object_size: ByteSize::kib(4),
            ..Default::default()
        });
        let budgets = Budgets {
            max_requests_count: Some(max),
            ..Default::default()
        };
        let rails = Rails::new(&p, false, None, budgets.clone()).unwrap();
        let mut req = Request::new(p, rails, Cancel::new(), quiet());
        req.guard = guard_for(&budgets);
        let stopped = tokio::time::timeout(Duration::from_secs(120), measure(req))
            .await
            .expect("the run ends");
        // The budget stopped it: while it opened (an error), or in its window (a breach).
        match stopped {
            Ok(m) => assert!(m.breach.is_some(), "--max-requests {max}"),
            Err(e) => assert_eq!(
                oprobe::errors::classify(&e, "s3").code,
                oprobe::errors::ErrorCode::SafetyBudgetExceeded,
                "--max-requests {max}: {e:#}"
            ),
        }
        assert_eq!(
            lab.stored_keys(),
            Vec::<String>::new(),
            "--max-requests {max}"
        );
        assert_eq!(lab.open_uploads(), 0, "--max-requests {max}");
    }
}

/// K1-5: a `CreateMultipartUpload` that never answers does not hang a cancelled run. 9.2
/// never cut that request (its answer carries the id the upload is aborted with) and waited
/// for it without a bound, so after Ctrl-C the run never reached its cleanup. It now has a
/// grace period; then the run goes on, cleans up, and names the upload it could not abort.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_that_never_answers_does_not_hang_a_cancelled_run() {
    let lab = lab_with(LabConfig {
        create_never_answers: true,
        ..Default::default()
    })
    .await;
    let mut p = multipart_writes(&lab, 4);
    // Nothing but the cancellation ends the stalled request.
    p.operator.layers.timeout = None;
    let cancel = Cancel::new();
    let run = tokio::spawn(measured(p, Budgets::default(), cancel.clone()));
    until(&lab, "the upload being created", |l| {
        l.multipart_counts().created >= 1
    })
    .await;
    cancel.cancel();
    // `measured` gives the run two minutes; 9.2 never came back.
    let m = run.await.unwrap();
    assert!(m.cancelled);
    assert!(m.run.report.is_partial());
    let cleanup = &m.run.report.cleanup;
    assert!(cleanup.verified, "the test data is deleted: {cleanup:?}");
    let error = cleanup.error.as_deref().expect("the upload is reported");
    assert!(
        error.starts_with("1 multipart uploads could not be aborted"),
        "{error}"
    );
    assert!(
        error.contains("/writes/") && error.contains("no upload id to abort"),
        "the upload is named: {error}"
    );
    // The report is true: the lab did create the upload, and nobody knows its id.
    let c = lab.multipart_counts();
    assert_eq!((c.created, c.aborted, c.parts), (1, 0, 0), "{c:?}");
    assert_eq!(lab.open_uploads(), 1);
    assert_eq!(lab.stored_object_count(), 0, "{:?}", lab.stored_keys());
}

/// The real binary against a lab of this process: what the stop tests below drive.
#[cfg(unix)]
mod binary {
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::{Child, Command, ExitStatus, Stdio};

    use super::*;

    pub fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// `oprobe run` of `mix` against `lab`, with its data under `home`: a two-minute window,
    /// no retries and no timeouts (only a stop ends a stalled request).
    pub fn run(home: &std::path::Path, lab: &LabHandle, mix: &[OpSpec]) -> Command {
        let mix = serde_json::to_string(mix).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_oprobe"));
        c.env("OPROBE_DATA_DIR", home.join("data"))
            .env("OPROBE_CONFIG", home.join("config.toml"))
            .env_remove("OPROBE_OUTPUT")
            .args(["run", "s3://bkt/x", "-d", "120s", "--warmup", "0s"])
            .args(["--objects", "8", "--object-size", "64KiB"])
            .args(["--set", &format!("options.endpoint={}", lab.endpoint_url)])
            .args(["--set", "http.proxy=none"])
            .args(["--set", "options.region=us-east-1"])
            .args(["--set", "options.access_key_id=x"])
            .args(["--set", "options.secret_access_key=x"])
            .args(["--set", &format!("mix={mix}"), "--set", "write.chunk=5MiB"])
            .args(["--set", "layers.retry=off", "--set", "layers.timeout=off"])
            .stdin(Stdio::null());
        c
    }

    pub fn multipart_write() -> OpSpec {
        OpSpec {
            weight: 1,
            op: OpKind::MultipartWrite {
                size: ByteSize(4 * PART),
            },
        }
    }

    /// Reads of an uploaded dataset next to the multipart writes: a run with objects on the
    /// target while one of its uploads is stalled.
    pub fn reads_and_multipart_writes() -> Vec<OpSpec> {
        let read = OpSpec {
            weight: 1,
            op: OpKind::Read { size: None },
        };
        vec![read, multipart_write()]
    }

    /// The exit status of `child`; a child still running after two minutes is killed (a stop
    /// that hangs fails its test, not the whole suite).
    pub fn wait(child: &mut Child, what: &str) -> ExitStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{what}: oprobe was still running two minutes after it was stopped");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn signal(child: &Child, signal: libc::c_int) {
        // SAFETY: a plain signal to our own child process.
        let sent = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
        assert_eq!(sent, 0, "kill: {}", std::io::Error::last_os_error());
    }

    /// The runs saved under `home`, and the run files still being written.
    pub fn run_files(home: &std::path::Path) -> (usize, usize) {
        let names: Vec<String> = std::fs::read_dir(home.join("data").join("runs"))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let count = |suffix: &str| names.iter().filter(|n| n.ends_with(suffix)).count();
        (count(".oprobe"), count(".oprobe.tmp"))
    }

    /// The run is in the middle of its window: its dataset is on the target and one of its
    /// multipart uploads has a part on the wire that never answers.
    pub async fn mid_run(lab: &LabHandle) {
        until(lab, "a dataset and a stalled part", |l| {
            l.stored_object_count() >= 8 && l.multipart_counts().parts >= 2
        })
        .await;
    }

    /// A pseudo-terminal: the master side, and the slave side to give a child as its
    /// terminal. Both are closed on exec: only what is passed as stdio reaches the child.
    pub fn pty() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        // SAFETY: plain libc calls on a descriptor this function has just opened and owns;
        // `ptsname`'s static buffer is copied at once, and nothing else here calls it.
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "{}", std::io::Error::last_os_error());
            assert_eq!(libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC), 0);
            assert_eq!(libc::grantpt(master), 0);
            assert_eq!(libc::unlockpt(master), 0);
            let name = libc::ptsname(master);
            assert!(!name.is_null());
            let path = std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned();
            let slave = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(path)
                .unwrap();
            (std::fs::File::from_raw_fd(master), slave)
        }
    }
}

/// SIGTERM and SIGHUP stop `oprobe run` as Ctrl-C does: what is in flight is cut, the
/// multipart upload is aborted, the test data is deleted, and the exit code is 130. In 9.1
/// only SIGINT was handled: the others killed the process where it stood.
#[cfg(unix)]
#[test]
fn sigterm_and_sighup_stop_a_run_cleanly() {
    let rt = binary::runtime();
    let home = tempfile::tempdir().unwrap();
    for (name, signal) in [("TERM", libc::SIGTERM), ("HUP", libc::SIGHUP)] {
        let lab = rt.block_on(lab(Some(PartFaultKind::Stall)));
        let mut child = binary::run(home.path(), &lab, &[binary::multipart_write()])
            .args(["-c", "1", "-q", "--color", "never"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The run is in the middle of an upload: a part is on the wire and never answers.
        rt.block_on(until(&lab, "the stalled part", |l| {
            l.multipart_counts().parts >= 2
        }));
        binary::signal(&child, signal);
        let status = binary::wait(&mut child, name);
        let err = std::io::read_to_string(child.stderr.take().unwrap()).unwrap();
        assert_eq!(status.code(), Some(130), "SIG{name}: {err}");
        // "Interrupted", whatever stopped it: 9.2 said "before Ctrl-C" of a SIGTERM.
        assert!(err.contains("interrupted: partial results"), "{err}");
        assert!(!err.contains("before Ctrl-C"), "SIG{name}: {err}");
        let c = lab.multipart_counts();
        assert_eq!((c.created, c.aborted), (1, 1), "SIG{name}: {c:?}");
        assert_nothing_left(&lab);
    }
}

/// K1-1, the terminal that closes: SIGHUP arrives and stderr is gone. 9.2 handled the signal,
/// then panicked at its next line for stderr ("cleaning up…", through `eprintln!`): exit 101,
/// no cleanup, no saved run, the dataset left on the target. No line for stderr can fail now:
/// the run is cut, its upload aborted, its test data deleted, the partial run saved.
#[cfg(unix)]
#[test]
fn a_closed_stderr_never_costs_the_cleanup() {
    let rt = binary::runtime();
    let home = tempfile::tempdir().unwrap();
    let lab = rt.block_on(lab(Some(PartFaultKind::Stall)));
    let mut child = binary::run(home.path(), &lab, &binary::reads_and_multipart_writes())
        .args(["-c", "2", "--color", "never"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    rt.block_on(binary::mid_run(&lab));
    assert_eq!(
        binary::run_files(home.path()),
        (0, 1),
        "the run is measuring"
    );
    // The reader of stderr goes away (the SSH session, `2>&1 | head`), then the hangup.
    drop(child.stderr.take());
    binary::signal(&child, libc::SIGHUP);
    let status = binary::wait(&mut child, "SIGHUP with stderr closed");
    assert_eq!(status.code(), Some(130), "a panic exits 101");
    let c = lab.multipart_counts();
    assert_eq!(c.aborted, c.created, "{c:?}");
    assert_nothing_left(&lab);
    assert_eq!(
        binary::run_files(home.path()),
        (1, 0),
        "the partial run is saved"
    );
}

/// K1-1, without a signal: the terminal a run draws its live line on goes away and no SIGHUP
/// reaches the process (it is not the session's controlling terminal, or the signal is
/// blocked). The line that cannot be drawn stops the run as Ctrl-C would. 9.2 ignored the
/// failed write and went on measuring for nobody, here for ever: its upload is stalled.
#[cfg(unix)]
#[test]
fn a_terminal_that_goes_away_stops_the_run_like_a_signal() {
    use std::io::Read;
    let rt = binary::runtime();
    let home = tempfile::tempdir().unwrap();
    let lab = rt.block_on(lab(Some(PartFaultKind::Stall)));
    let (mut master, slave) = binary::pty();
    // stdin, stdout and stderr on the terminal: the run is interactive and draws its line.
    let mut child = binary::run(home.path(), &lab, &binary::reads_and_multipart_writes())
        .args(["-c", "2"])
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    // The terminal shows what the run draws until it is told to go away.
    let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let terminal = {
        let gone = gone.clone();
        std::thread::spawn(move || {
            let mut drawn = Vec::new();
            let mut buf = [0u8; 4096];
            while !gone.load(Ordering::SeqCst) {
                match master.read(&mut buf) {
                    Ok(n) if n > 0 => drawn.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
            // `master` is dropped here: the terminal is gone.
            String::from_utf8_lossy(&drawn).into_owned()
        })
    };
    rt.block_on(binary::mid_run(&lab));
    gone.store(true, Ordering::SeqCst);
    let drawn = terminal.join().unwrap();
    assert!(
        drawn.contains("ops/s"),
        "the live line was drawn: {drawn:?}"
    );
    let status = binary::wait(&mut child, "the terminal went away");
    assert_eq!(status.code(), Some(130));
    let c = lab.multipart_counts();
    assert_eq!(c.aborted, c.created, "{c:?}");
    assert_nothing_left(&lab);
    assert_eq!(
        binary::run_files(home.path()),
        (1, 0),
        "the partial run is saved"
    );
}

/// K1-1: no line for stderr is printed with a macro that panics when stderr is closed. The
/// writers are `oprobe_core::output::{stderr_line, stderr_text}`.
#[test]
fn nothing_prints_to_stderr_with_a_macro_that_can_panic() {
    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut files = Vec::new();
    for krate in ["oprobe", "oprobe-core", "oprobe-trace"] {
        sources(&crates.join(krate).join("src"), &mut files);
    }
    assert!(files.len() > 50, "{} source files found", files.len());
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            if ["eprintln!(", "eprint!(", "dbg!("]
                .iter()
                .any(|m| code.contains(m))
            {
                found.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "use `stderr_line`:\n{}", found.join("\n"));
}

/// The exit status of `child` if it ends within `within`; `None` (and it is killed) if not.
#[cfg(unix)]
fn exits_within(
    child: &mut std::process::Child,
    within: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// A pipe whose buffer is full: (its read end, kept open and never read; its write end, in
/// blocking mode).
#[cfg(unix)]
fn full_pipe() -> (std::fs::File, std::fs::File) {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    // SAFETY: plain libc calls on descriptors this function has just opened and owns.
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        for fd in fds {
            assert_eq!(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC), 0);
        }
        let flags = libc::fcntl(fds[1], libc::F_GETFL);
        assert_eq!(
            libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK),
            0
        );
        let mut w = std::fs::File::from_raw_fd(fds[1]);
        let chunk = [b'x'; 4096];
        while w.write(&chunk).is_ok() {}
        assert_eq!(libc::fcntl(fds[1], libc::F_SETFL, flags), 0);
        (std::fs::File::from_raw_fd(fds[0]), w)
    }
}

/// A stderr that is open but read by nobody (a terminal paused with Ctrl-S) fills up, and
/// the write that fills it blocks. 9.3.2 wrote the signal handler's "stopping…" behind it: the
/// handler blocked, the second Ctrl-C was never seen, and nothing but SIGKILL ended the
/// process. Lines for stderr are queued now, and a stop always goes through.
#[cfg(unix)]
#[test]
fn a_full_stderr_never_blocks_a_stop() {
    let rt = binary::runtime();
    let home = tempfile::tempdir().unwrap();
    let lab = rt.block_on(lab(Some(PartFaultKind::Stall)));
    // A pipe already full, that nobody reads: the first line oprobe writes to stderr
    // blocks, for as long as the process lives.
    let (reader, writer) = full_pipe();
    let mut child = binary::run(home.path(), &lab, &binary::reads_and_multipart_writes())
        .args(["-c", "2"])
        .stdout(std::process::Stdio::null())
        .stderr(writer)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    binary::signal(&child, libc::SIGINT);
    std::thread::sleep(Duration::from_millis(500));
    binary::signal(&child, libc::SIGINT);
    let status = exits_within(&mut child, Duration::from_secs(20));
    drop(reader);
    let status = status.expect("oprobe was still running 20 s after two Ctrl-C");
    assert_eq!(status.code(), Some(130), "{status:?}");
}

/// SIGTERM stops the terminal app as Ctrl+C does, with exit code 130 (and the terminal
/// restored). 9.3.2 handled stop signals in the commands only: the app was killed where it
/// stood, its jobs' test data and the terminal's raw mode left behind.
#[cfg(unix)]
#[test]
fn sigterm_stops_the_terminal_app() {
    use std::io::Read;
    let home = tempfile::tempdir().unwrap();
    let (mut master, slave) = binary::pty();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .env("OPROBE_DATA_DIR", home.path().join("data"))
        .env("OPROBE_CONFIG", home.path().join("config.toml"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    // The terminal shows what the app draws.
    let drawn = std::thread::spawn(move || {
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        all.len()
    });
    std::thread::sleep(Duration::from_secs(3));
    assert!(child.try_wait().unwrap().is_none(), "the app is open");
    binary::signal(&child, libc::SIGTERM);
    let status = exits_within(&mut child, Duration::from_secs(20))
        .expect("the app was still open 20 s after SIGTERM");
    assert_eq!(status.code(), Some(130), "{status:?}");
    drop(drawn);
}

/// S4: a service without an HTTP transport (`fs://`, `memory://`) has no gate in its
/// transport. In 9.3.3 its dataset upload was charged nowhere: with `--max-requests 10` and
/// cleanup off, a run wrote all 50 objects, measured and exited 0 with 57 files left. Every
/// call is charged now: the budget stops the upload before it writes past it.
#[test]
fn a_budget_covers_the_dataset_upload_of_a_local_service() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .env("OPROBE_DATA_DIR", home.path().join("data"))
        .env("OPROBE_CONFIG", home.path().join("config.toml"))
        .args(["run", &format!("fs://{}", target.path().display())])
        .args([
            "--max-requests",
            "10",
            "--objects",
            "50",
            "--object-size",
            "4KiB",
        ])
        .args(["-d", "1s", "-q", "--set", "cleanup=false"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(exit::BUDGET.into()),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fn files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| {
                        let p = e.path();
                        if p.is_dir() { files(&p) } else { 1 }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
    let written = files(target.path());
    assert!(
        written <= 10,
        "{written} files written under a budget of 10 requests"
    );
}

/// A dataset larger than the byte budget is refused before anything is written. 9.3.3 began
/// the upload, and generated each object whole in memory first: `--object-size 64GiB` aborted
/// the process before the budget could stop it.
#[test]
fn a_dataset_larger_than_the_byte_budget_is_refused_up_front() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oprobe"))
        .env("OPROBE_DATA_DIR", home.path().join("data"))
        .env("OPROBE_CONFIG", home.path().join("config.toml"))
        .args(["run", &format!("fs://{}", target.path().display())])
        .args([
            "--objects",
            "1",
            "--object-size",
            "64GiB",
            "--max-bytes",
            "1MiB",
        ])
        .args(["-d", "1s", "-q", "--set", "cleanup=false"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(exit::BUDGET.into()), "{stderr}");
    assert!(stderr.contains("does not fit the byte budget"), "{stderr}");
    let left = walk(target.path());
    assert!(
        left.iter()
            .all(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) == 0),
        "{left:?}"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .flat_map(|e| {
                    let p = e.path();
                    if p.is_dir() { walk(&p) } else { vec![p] }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// An in-process service never goes past a request budget, whatever the budget's parity: its
/// calls are admitted before they run. (The first fix of the upload charged the measured
/// window as its operations completed: an operation of two calls jumped 399 to 401.)
#[test]
fn a_local_service_never_passes_a_request_budget() {
    for max in 300..310u32 {
        let home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_oprobe"))
            .env("OPROBE_DATA_DIR", home.path().join("data"))
            .env("OPROBE_CONFIG", home.path().join("config.toml"))
            .args([
                "run",
                "memory://",
                "-c",
                "64",
                "-d",
                "5s",
                "--set",
                "warmup=0s",
                "-o",
                "json",
            ])
            .args(["--max-requests", &max.to_string()])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(exit::BUDGET.into()), "{max}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let msg = v["error"]["message"].as_str().unwrap().to_owned();
        assert!(
            msg.contains(&format!(": {max} requests (limit {max})")),
            "{msg}"
        );
    }
}
