//! The abort of a multipart upload, shielded from what times and delays the writer's calls.
//!
//! [`super::upload::Upload`] never drops a call of its writer, and aborts the upload when a
//! call fails. Two things of the measured stack can still keep that abort from happening:
//!
//! - **The per-call timers cut it.** `TimeoutLayer` (`layers.timeout.io`) and `TailCutLayer`
//!   wrap `Writer::abort` like any other call: an abort slower than the I/O timeout, or than
//!   the tail cut's deadline for writes, is dropped half-way. Without a retry layer above, the
//!   upload stays.
//! - **A wait above the transport outlasts the stop.** `ThrottleLayer` waits for its bytes,
//!   and `RetryLayer` backs off, before a request reaches the [`super::gate`], which is what
//!   stops a cancelled run's requests. A 64 MiB write at 1 MiB/s waits a minute: the session's
//!   wait for its uploads ([`super::upload::SETTLE_GRACE`]) gives up first, and the upload is
//!   reported as left behind although nothing has been sent for it.
//!
//! [`AbortShieldLayer`] sits beneath every measured layer, directly above the transport. Its
//! writer runs `abort` on a task of its own, which no timer above can drop: a cut abort is
//! still running, and the next call of `abort` (a retry, the session's own) waits for it. And
//! it keeps each writer where the session finds it ([`super::upload::Aborts`]): once the run
//! is cancelled, the uploads whose writer is between two calls (waiting in a throttle or a
//! backoff above, or between two writes) are aborted at once, and the waiting call then fails
//! without sending anything.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use opendal::raw::oio;
use opendal::raw::{
    Layer, OpCompose, OpCopy, OpCreateDir, OpList, OpPresign, OpRead, OpRename, OpRestore, OpStat,
    OpWrite, RpCreateDir, RpPresign, RpRename, RpRestore, RpStat, Service, ServiceInfo, Servicer,
};
use opendal::{
    BytesRange, Capability, Error, ErrorKind, Execute, Metadata, OperationContext, Result,
};
use tokio::sync::oneshot;

/// The writers of one session's uploads, for [`super::upload::Aborts`].
#[derive(Default)]
pub(super) struct Shields {
    slots: Mutex<Vec<Weak<Slot>>>,
}

impl Shields {
    fn register(&self, slot: &Arc<Slot>) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|s| s.strong_count() > 0);
        slots.push(Arc::downgrade(slot));
    }

    /// Abort every upload whose writer is between two calls now; the paths whose abort went
    /// through (or had nothing to abort). A writer inside a call is left to it: the gate
    /// stops the call, and the upload's task aborts.
    pub(super) async fn abort_idle(&self, timeout: Duration) -> Vec<String> {
        let live: Vec<Arc<Slot>> = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let aborts = live.into_iter().map(|slot| async move {
            let Ok(mut state) = slot.state.try_lock() else {
                return None;
            };
            let aborted = tokio::time::timeout(timeout, slot.abort(&mut state)).await;
            matches!(aborted, Ok(Ok(()))).then(|| slot.path.clone())
        });
        futures::future::join_all(aborts)
            .await
            .into_iter()
            .flatten()
            .collect()
    }
}

/// Puts [`ShieldWriter`]s beneath the layers stacked after it.
#[derive(Clone)]
pub struct AbortShieldLayer {
    shields: Arc<Shields>,
}

impl std::fmt::Debug for AbortShieldLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbortShieldLayer")
    }
}

impl AbortShieldLayer {
    pub(super) fn new(shields: Arc<Shields>) -> Self {
        Self { shields }
    }
}

impl Layer for AbortShieldLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        Arc::new(ShieldService {
            inner,
            shields: self.shields.clone(),
        })
    }
}

struct ShieldService {
    inner: Servicer,
    shields: Arc<Shields>,
}

impl std::fmt::Debug for ShieldService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ShieldService")
    }
}

impl Service for ShieldService {
    type Reader = oio::Reader;
    type Writer = ShieldWriter;
    type Lister = oio::Lister;
    type Deleter = oio::Deleter;
    type Copier = oio::Copier;
    type Composer = oio::Composer;

    fn info(&self) -> ServiceInfo {
        self.inner.info()
    }

    fn capability(&self) -> Capability {
        self.inner.capability()
    }

    async fn create_dir(
        &self,
        ctx: &OperationContext,
        path: &str,
        args: OpCreateDir,
    ) -> Result<RpCreateDir> {
        self.inner.create_dir(ctx, path, args).await
    }

    async fn stat(&self, ctx: &OperationContext, path: &str, args: OpStat) -> Result<RpStat> {
        self.inner.stat(ctx, path, args).await
    }

    fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
        self.inner.read(ctx, path, args)
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        let writer = self.inner.write(ctx, path, args)?;
        let slot = Arc::new(Slot {
            path: path.to_owned(),
            executor: ctx.executor().clone().into_inner(),
            state: tokio::sync::Mutex::new(State::Live(writer)),
        });
        self.shields.register(&slot);
        Ok(ShieldWriter { slot })
    }

    fn delete(&self, ctx: &OperationContext) -> Result<Self::Deleter> {
        self.inner.delete(ctx)
    }

    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        self.inner.list(ctx, path, args)
    }

    fn copy(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpCopy,
    ) -> Result<Self::Copier> {
        self.inner.copy(ctx, from, to, args)
    }

    fn compose(&self, ctx: &OperationContext, to: &str, args: OpCompose) -> Result<Self::Composer> {
        self.inner.compose(ctx, to, args)
    }

    async fn rename(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpRename,
    ) -> Result<RpRename> {
        self.inner.rename(ctx, from, to, args).await
    }

    async fn restore(
        &self,
        ctx: &OperationContext,
        path: &str,
        args: OpRestore,
    ) -> Result<RpRestore> {
        self.inner.restore(ctx, path, args).await
    }

    async fn presign(
        &self,
        ctx: &OperationContext,
        path: &str,
        args: OpPresign,
    ) -> Result<RpPresign> {
        self.inner.presign(ctx, path, args).await
    }
}

/// Where an upload stands.
enum State {
    Live(oio::Writer),
    /// Its abort runs on a task: the writer comes back with the abort's result.
    Aborting(oneshot::Receiver<(oio::Writer, Result<()>)>),
    /// Aborted (or there was nothing to abort).
    Aborted,
    /// Completed.
    Closed,
    /// A call was dropped while it held the writer: nothing is left to abort with.
    Lost,
}

/// One writer, shared by its [`ShieldWriter`] and the session's [`Shields`].
struct Slot {
    path: String,
    executor: Arc<dyn Execute>,
    state: tokio::sync::Mutex<State>,
}

fn stopped() -> Error {
    Error::new(
        ErrorKind::Unexpected,
        "the upload was aborted: the run was stopped",
    )
}

impl Slot {
    /// Abort the upload (`state` is this slot's, locked), on a task no caller can drop. A
    /// call dropped while it waits leaves the abort running, and the next call waits for it.
    async fn abort(&self, state: &mut State) -> Result<()> {
        if let State::Live(_) = state {
            let State::Live(mut writer) = std::mem::replace(state, State::Lost) else {
                unreachable!()
            };
            let (tx, rx) = oneshot::channel();
            self.executor.execute(Box::pin(async move {
                let res = oio::Write::abort(&mut writer).await;
                let _ = tx.send((writer, res));
            }));
            *state = State::Aborting(rx);
        }
        match state {
            State::Aborting(rx) => {
                let answer = rx.await;
                let (writer, res) = answer.map_err(|_| {
                    Error::new(
                        ErrorKind::Unexpected,
                        "the abort's task ended without an answer",
                    )
                })?;
                *state = match res {
                    Ok(()) => State::Aborted,
                    // Failed: it may be tried again.
                    Err(_) => State::Live(writer),
                };
                res
            }
            State::Aborted | State::Closed => Ok(()),
            State::Lost => Err(Error::new(
                ErrorKind::Unexpected,
                "the writer was lost in a dropped call",
            )),
            State::Live(_) => unreachable!("handled above"),
        }
    }
}

/// A writer whose abort no layer above can cut, and which the session can abort while it
/// waits between two calls ([`Shields::abort_idle`]).
pub struct ShieldWriter {
    slot: Arc<Slot>,
}

/// Why a writer that is not live refuses a call.
fn not_live(state: &State) -> Error {
    match state {
        State::Closed => Error::new(ErrorKind::Unexpected, "the writer is closed"),
        State::Lost => Error::new(
            ErrorKind::Unexpected,
            "the writer was lost in a dropped call",
        ),
        _ => stopped(),
    }
}

impl oio::Write for ShieldWriter {
    async fn write(&mut self, bs: opendal::Buffer) -> Result<()> {
        let mut state = self.slot.state.lock().await;
        match &mut *state {
            State::Live(writer) => oio::Write::write(writer, bs).await,
            other => Err(not_live(other)),
        }
    }

    async fn copy_from(&mut self, path: &str, args: OpRead, range: BytesRange) -> Result<()> {
        let mut state = self.slot.state.lock().await;
        match &mut *state {
            State::Live(writer) => oio::Write::copy_from(writer, path, args, range).await,
            other => Err(not_live(other)),
        }
    }

    async fn close(&mut self) -> Result<Metadata> {
        let mut state = self.slot.state.lock().await;
        let State::Live(writer) = &mut *state else {
            return Err(not_live(&state));
        };
        let meta = oio::Write::close(writer).await?;
        *state = State::Closed;
        Ok(meta)
    }

    async fn abort(&mut self) -> Result<()> {
        let mut state = self.slot.state.lock().await;
        self.slot.abort(&mut state).await
    }
}
