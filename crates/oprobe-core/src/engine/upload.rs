//! Writers that never leave a multipart upload behind.
//!
//! A multipart upload that is neither completed nor aborted keeps its parts: they are billed,
//! and no listing of objects shows them. OpenDAL's `Writer` aborts only when asked to, and
//! cannot be asked once one of its calls was dropped half-way: `RetryLayer` moves the writer
//! into the call and refuses every later one ("retry layer is in bad state"), and a dropped
//! `CreateMultipartUpload` loses the upload id. A `select!`, a timeout around an operation, a
//! `try_join` whose sibling failed or an aborted task all drop futures.
//!
//! So every writer the engine opens is owned by an [`Upload`], and **no call of the writer is
//! ever dropped**: each one runs on a task of its own (OpenDAL's executor, so its requests
//! stay attributed to the operation), which
//!
//! - aborts the upload itself when the call fails (an error, a timeout, a tail cut, a request
//!   the budget or a cancellation stopped at the [`super::gate`]), before the caller hears of
//!   the failure;
//! - aborts the upload when the caller is gone by the time the call returns (its future was
//!   dropped): nobody is left to complete it.
//!
//! An `Upload` dropped between two calls aborts on a task as well. The session awaits all of
//! these tasks ([`Aborts::settle`]) before it cleans up. Once the run is cancelled that wait is
//! bounded ([`SETTLE_GRACE`]): the gate has cut every request by then, so a task that still
//! runs is stuck, and the uploads of such tasks are reported ([`Aborts::failure`]) instead of
//! being waited for with the cleanup never starting.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opendal::{Buffer, Execute, Metadata, Operator, Writer};
use tokio::sync::{Notify, oneshot};

use super::Cancel;

/// How long one abort may take (the service may be unreachable, which is often why the upload
/// failed in the first place).
const ABORT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a cancelled run waits for its upload tasks: what the slowest of them may take
/// once the gate has stopped its requests (an upload being created has its grace period, then
/// the abort its timeout), and a margin.
pub const SETTLE_GRACE: Duration =
    Duration::from_secs(super::gate::CREATE_GRACE.as_secs() + ABORT_TIMEOUT.as_secs() + 5);

/// How many uploads [`Aborts::failure`] names.
const NAMED_FAILURES: usize = 3;

/// The upload tasks of one session: those still running, and the uploads that could not be
/// aborted.
#[derive(Clone, Default)]
pub struct Aborts(Arc<State>);

#[derive(Default)]
struct State {
    /// The tasks still running: the path each one's upload writes.
    running: Mutex<BTreeMap<u64, String>>,
    next_task: AtomicU64,
    idle: Notify,
    failed: Mutex<Vec<String>>,
}

/// One running task; the last to end wakes [`Aborts::settle`].
struct Running(Aborts, u64);

impl Drop for Running {
    fn drop(&mut self) {
        let state = &self.0.0;
        let mut running = state.running.lock().unwrap_or_else(|e| e.into_inner());
        running.remove(&self.1);
        if running.is_empty() {
            state.idle.notify_waiters();
        }
    }
}

impl Aborts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep what is wrong with the upload of `path`: it could not be aborted.
    pub(super) fn note_failure(&self, path: &str, failure: impl std::fmt::Display) {
        self.0
            .failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("`{path}`: {failure}"));
    }

    /// Abort `writer`'s upload of `path` and keep what went wrong. A writer with nothing to
    /// abort (no multipart upload was started, or the service has none) is fine.
    async fn abort(&self, writer: &mut Writer, path: &str) {
        match tokio::time::timeout(ABORT_TIMEOUT, writer.abort()).await {
            Ok(Ok(())) => {}
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    opendal::ErrorKind::Unsupported | opendal::ErrorKind::NotFound
                ) => {}
            Ok(Err(e)) => self.note_failure(path, e),
            Err(_) => self.note_failure(
                path,
                format_args!(
                    "the abort got no answer within {}s",
                    ABORT_TIMEOUT.as_secs()
                ),
            ),
        }
    }

    /// Run `work`, the task of the upload of `path`, where nothing can drop or cancel it.
    fn run(&self, on: &dyn Execute, path: &str, work: impl Future<Output = ()> + Send + 'static) {
        let task = self.0.next_task.fetch_add(1, Ordering::Relaxed);
        self.0
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task, path.to_owned());
        let running = Running(self.clone(), task);
        on.execute(Box::pin(async move {
            work.await;
            drop(running);
        }));
    }

    /// Wait for every upload task still running (those started while waiting included).
    ///
    /// Once `cancel` has fired the wait ends after [`SETTLE_GRACE`] at the latest: the uploads
    /// of the tasks still running then are reported by [`Aborts::failure`] and no longer waited
    /// for (by this call or a later one). 9.2 waited without a bound: a task stuck after Ctrl-C
    /// kept the run from ever reaching its cleanup.
    pub async fn settle(&self, cancel: &Cancel) {
        let idle = async {
            loop {
                let idle = self.0.idle.notified();
                tokio::pin!(idle);
                // Registered before the tasks are counted: the last one ending in between
                // wakes it.
                idle.as_mut().enable();
                if self.running().is_empty() {
                    break;
                }
                idle.await;
            }
        };
        let given_up = async {
            cancel.cancelled().await;
            tokio::time::sleep(SETTLE_GRACE).await;
        };
        tokio::select! {
            biased;
            () = idle => return,
            () = given_up => {}
        }
        let stuck = std::mem::take(&mut *self.running());
        for path in stuck.values() {
            self.note_failure(
                path,
                format_args!(
                    "its upload was still in flight {}s after the run was stopped",
                    SETTLE_GRACE.as_secs()
                ),
            );
        }
    }

    fn running(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, String>> {
        self.0.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What is wrong with the uploads that could not be aborted so far (`None`: every abort
    /// went through), naming them: their parts stay on the target, billed, until the upload is
    /// aborted, and deleting objects does not remove them.
    pub fn failure(&self) -> Option<String> {
        let failed = self.0.failed.lock().unwrap_or_else(|e| e.into_inner());
        if failed.is_empty() {
            return None;
        }
        let mut named = failed[..failed.len().min(NAMED_FAILURES)].join("; ");
        if failed.len() > NAMED_FAILURES {
            named.push_str(&format!("; and {} more", failed.len() - NAMED_FAILURES));
        }
        Some(format!(
            "{} multipart uploads could not be aborted and may still hold parts ({named})",
            failed.len()
        ))
    }
}

/// A [`Writer`] whose multipart upload is aborted unless [`Upload::close`] succeeds.
pub struct Upload {
    /// `None` while a call runs, and once the upload has ended.
    writer: Option<Writer>,
    path: String,
    aborts: Aborts,
    /// Where the writer's calls run: the operator's executor.
    executor: Arc<dyn Execute>,
}

/// One call of the writer.
enum Call {
    Write(Buffer),
    Close,
}

/// What a call hands back: the writer while the upload goes on, and its result.
type Answer = (Option<Writer>, opendal::Result<Option<Metadata>>);

fn ended() -> opendal::Error {
    opendal::Error::new(opendal::ErrorKind::Unexpected, "the upload has ended")
}

impl Upload {
    /// Own `writer`, a writer of `op` for `path`.
    pub fn new(op: &Operator, writer: Writer, path: &str, aborts: &Aborts) -> Self {
        Self {
            writer: Some(writer),
            path: path.to_owned(),
            aborts: aborts.clone(),
            executor: op.context().executor().clone().into_inner(),
        }
    }

    /// Run `call` to its end on a task, whatever becomes of this future. The task hands the
    /// writer back while the upload goes on, and aborts it when the call failed or when nobody
    /// is left to take it.
    async fn call(&mut self, call: Call) -> opendal::Result<Option<Metadata>> {
        let mut writer = self.writer.take().ok_or_else(ended)?;
        let (answer, answered) = oneshot::channel::<Answer>();
        let (aborts, path) = (self.aborts.clone(), self.path.clone());
        self.aborts
            .run(self.executor.as_ref(), &self.path, async move {
                let (res, goes_on) = match call {
                    Call::Write(data) => (writer.write(data).await.map(|()| None), true),
                    Call::Close => (writer.close().await.map(Some), false),
                };
                if res.is_err() {
                    aborts.abort(&mut writer, &path).await;
                }
                let back = (goes_on && res.is_ok()).then_some(writer);
                if let Err((Some(mut orphan), _)) = answer.send((back, res)) {
                    aborts.abort(&mut orphan, &path).await;
                }
            });
        let (writer, res) = answered.await.map_err(|_| ended())?;
        self.writer = writer;
        res
    }

    /// Write `data`; a failure aborts the upload.
    pub async fn write(&mut self, data: impl Into<Buffer>) -> opendal::Result<()> {
        self.call(Call::Write(data.into())).await.map(|_| ())
    }

    /// Write `data` in pieces of `piece` bytes (what a streaming application hands a writer).
    pub async fn write_in_pieces(
        &mut self,
        data: bytes::Bytes,
        piece: usize,
    ) -> opendal::Result<()> {
        let piece = piece.max(1);
        let mut off = 0;
        while off < data.len() {
            let end = (off + piece).min(data.len());
            self.write(data.slice(off..end)).await?;
            off = end;
        }
        Ok(())
    }

    /// Complete the upload; a failure aborts it.
    pub async fn close(mut self) -> opendal::Result<Metadata> {
        self.call(Call::Close).await?.ok_or_else(ended)
    }
}

impl Drop for Upload {
    /// Dropped between two calls, unfinished: the writer is whole, and aborted on a task.
    fn drop(&mut self) {
        let Some(mut writer) = self.writer.take() else {
            return;
        };
        // Dropped where no task can start (off the runtime's threads, by whoever kept the
        // upload past the runtime): the upload stays, and is reported.
        if tokio::runtime::Handle::try_current().is_err() {
            self.aborts
                .note_failure(&self.path, "it was dropped after the runtime had stopped");
            return;
        }
        let (aborts, path) = (self.aborts.clone(), self.path.clone());
        self.aborts
            .run(self.executor.as_ref(), &self.path, async move {
                aborts.abort(&mut writer, &path).await;
            });
    }
}

#[cfg(test)]
mod tests {
    use opendal::layers::{RetryLayer, TimeoutLayer};

    use super::*;
    use crate::engine::operator::{base_operator, trace_layer};
    use crate::lab::{LabConfig, LabHandle, LabServer, PartFault, PartFaultKind};

    const PART: usize = 5 << 20;

    /// A lab and an operator on it with the layers that drop and move writers: a short
    /// timeout, and one quick retry.
    async fn lab(part_fault: Option<PartFault>) -> (LabHandle, Operator) {
        let faults = LabConfig {
            part_fault,
            ..Default::default()
        };
        let lab = LabServer::spawn("127.0.0.1:0", faults).await.unwrap();
        let target = lab.profile("b").operator.target().unwrap();
        let op = base_operator(&target)
            .unwrap()
            .layer(trace_layer(&target, &Default::default(), None, None).unwrap())
            // Seconds: a healthy 5 MiB part must never time out, however slow the machine (a
            // stalled part waits this long, once per attempt).
            .layer(TimeoutLayer::new().with_io_timeout(Duration::from_secs(3)))
            .layer(
                RetryLayer::new()
                    .with_max_times(1)
                    .with_min_delay(Duration::from_millis(1)),
            );
        (lab, op)
    }

    /// An upload dropped unfinished is aborted by the time `settle` returns; one that closed is
    /// not touched.
    #[tokio::test]
    async fn a_dropped_upload_is_aborted_and_a_closed_one_is_not() {
        let (lab, op) = lab(None).await;
        let aborts = Aborts::new();
        let mut uploads = Vec::new();
        for path in ["dropped", "closed"] {
            let w = op.writer_with(path).chunk(PART).await.unwrap();
            let mut upload = Upload::new(&op, w, path, &aborts);
            let data = bytes::Bytes::from(vec![7u8; 2 * PART + 1]);
            upload.write_in_pieces(data, PART).await.unwrap();
            uploads.push(upload);
        }
        assert_eq!(lab.open_uploads(), 2);
        let closed = uploads.pop().unwrap();
        drop(uploads);
        closed.close().await.unwrap();
        aborts.settle(&Cancel::new()).await;
        assert_eq!(aborts.failure(), None);
        assert_eq!(lab.open_uploads(), 0);
        let c = lab.multipart_counts();
        assert_eq!((c.created, c.completed, c.aborted), (2, 1, 1), "{c:?}");
        assert_eq!(lab.stored_keys(), ["b/bench/closed"]);
    }

    /// A call dropped half-way (its part never answers) still ends in an abort. With
    /// `RetryLayer`, a writer dropped inside a call refuses every later call, `abort` included:
    /// an `Upload` that aborted in its `Drop` alone left this upload on the target.
    #[tokio::test]
    async fn a_call_dropped_half_way_still_aborts() {
        let stall = PartFault {
            from_part: 2,
            kind: PartFaultKind::Stall,
        };
        let (lab, op) = lab(Some(stall)).await;
        let aborts = Aborts::new();
        let w = op.writer_with("k").chunk(PART).await.unwrap();
        let mut upload = Upload::new(&op, w, "k", &aborts);
        // Enough pieces for OpenDAL to send the second part (it holds two back).
        let data = bytes::Bytes::from(vec![7u8; 5 * PART]);
        let write = tokio::spawn(async move { upload.write_in_pieces(data, PART).await });
        // The second part is on the wire and never answers.
        while lab.multipart_counts().parts < 2 {
            assert!(!write.is_finished(), "the write ends at its stalled part");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(lab.open_uploads(), 1);
        // Drop the call, and the upload with it: the call goes on until its part times out,
        // then aborts, since nobody is left to finish the upload.
        write.abort();
        assert!(write.await.unwrap_err().is_cancelled());
        aborts.settle(&Cancel::new()).await;
        assert_eq!(aborts.failure(), None);
        assert_eq!(lab.open_uploads(), 0);
        let c = lab.multipart_counts();
        assert_eq!((c.created, c.completed, c.aborted), (1, 0, 1), "{c:?}");
    }

    /// K1-5: a task that never ends does not keep a cancelled run from its cleanup: `settle`
    /// gives up after its grace period and names the upload (9.2 waited for ever). A run that
    /// was not cancelled still waits: its tasks are doing work that is meant to finish.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_run_stops_waiting_for_a_stuck_task() {
        let aborts = Aborts::new();
        let executor = opendal::Executor::new();
        let stuck = |path: &str| {
            let executor = executor.clone().into_inner();
            aborts.run(executor.as_ref(), path, std::future::pending());
        };
        for i in 0..5 {
            stuck(&format!("stuck-{i}"));
        }
        let cancel = Cancel::new();
        let settled = {
            let (aborts, cancel) = (aborts.clone(), cancel.clone());
            tokio::spawn(async move { aborts.settle(&cancel).await })
        };
        tokio::time::sleep(SETTLE_GRACE * 100).await;
        assert!(!settled.is_finished(), "not cancelled: it waits");
        assert_eq!(aborts.failure(), None);
        cancel.cancel();
        tokio::time::sleep(SETTLE_GRACE - Duration::from_millis(1)).await;
        assert!(!settled.is_finished(), "within the grace period");
        tokio::time::sleep(Duration::from_millis(2)).await;
        settled.await.unwrap();
        let failure = aborts.failure().expect("the stuck uploads are reported");
        assert!(failure.starts_with("5 multipart uploads"), "{failure}");
        for named in ["`stuck-0`", "`stuck-1`", "`stuck-2`", "and 2 more"] {
            assert!(failure.contains(named), "{named}: {failure}");
        }
        assert!(failure.contains("still in flight 45s after"), "{failure}");
        // What was given up is not waited for again (`Session::close` settles once more).
        aborts.settle(&cancel).await;
        assert!(aborts.failure().unwrap().starts_with("5 "));
    }

    /// An upload dropped where no task can run (the runtime is gone) cannot be aborted: it is
    /// reported, and dropping it does not panic.
    #[test]
    fn an_upload_dropped_without_a_runtime_is_reported() {
        let aborts = Aborts::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let upload = rt.block_on(async {
            let op = Operator::via_iter("memory", [] as [(String, String); 0]).unwrap();
            let w = op.writer("kept").await.unwrap();
            Upload::new(&op, w, "kept", &aborts)
        });
        drop(rt);
        drop(upload);
        let failure = aborts.failure().expect("reported");
        assert!(
            failure.contains("`kept`: it was dropped after the runtime had stopped"),
            "{failure}"
        );
    }
}
