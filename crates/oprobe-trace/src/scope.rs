//! Per-logical-operation trace scope, propagated through OpenDAL's async stack via a
//! task-local and through OpenDAL's background executor via [`ScopePropagatingExecutor`].

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use opendal_core::raw::BoxedStaticFuture;
use opendal_core::{Execute, Executor};

use crate::Recorder;
use crate::record::RequestRecord;

tokio::task_local! {
    static CURRENT: Arc<OpScope>;
}

static NEXT_OP_ID: AtomicU64 = AtomicU64::new(1);

/// Maximum backend requests retained per logical operation (counts are always exact).
pub const MAX_REQUESTS_PER_OP: usize = 512;

/// Maximum retry backoff intervals retained per logical operation.
pub const MAX_RETRY_WAITS_PER_OP: usize = 256;

#[derive(Default, Debug)]
pub struct OpScopeData {
    pub requests: Vec<RequestRecord>,
    pub dropped_requests: u64,
    pub cache_lookups: u64,
    pub cache_hits: u64,
    pub cache_hit_bytes: u64,
    pub cache_lookup_us: u64,
    pub retries: u64,
    pub retry_backoff_us: u64,
    /// Backoff sleeps between retry attempts, as `[start, end)` offsets (µs) from the
    /// operation's start (at most [`MAX_RETRY_WAITS_PER_OP`]; `retry_backoff_us` is exact).
    pub retry_waits: Vec<(u64, u64)>,
    pub throttled: bool,
    /// Set by [`OpScope::take`]: the operation is complete and its data handed over.
    pub closed: bool,
}

/// Collects everything that happened underneath one logical operation.
#[derive(Debug)]
pub struct OpScope {
    pub id: u64,
    pub started: Instant,
    /// Whether this op started inside the measurement window.
    pub measured: bool,
    data: Mutex<OpScopeData>,
    /// Await-tree registry for capturing call stacks of sub-tasks spawned by OpenDAL.
    tree: Mutex<Option<await_tree::Registry>>,
    subtasks: AtomicU64,
    /// Where requests that finish after the operation completed go (OpenDAL background tasks
    /// still running, response bodies dropped late), so they are counted, not lost.
    late: Option<Arc<Recorder>>,
}

/// Await-tree key of a sub-task spawned on behalf of operation `.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubTaskKey(pub u64, pub u64);

impl OpScope {
    pub fn new(measured: bool) -> Arc<Self> {
        Self::new_at(measured, Instant::now(), None)
    }

    /// A scope whose operation started at `started` (in open-loop runs, the intended start, so
    /// request offsets and the operation's latency share one origin). Requests recorded after
    /// the operation completed go to `late`.
    pub fn new_at(measured: bool, started: Instant, late: Option<Arc<Recorder>>) -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_OP_ID.fetch_add(1, Ordering::Relaxed),
            started,
            measured,
            data: Mutex::new(OpScopeData::default()),
            tree: Mutex::new(None),
            subtasks: AtomicU64::new(0),
            late,
        })
    }

    /// Run `fut` with this scope installed as the current task-local scope.
    pub async fn run<F: Future>(self: &Arc<Self>, fut: F) -> F::Output {
        CURRENT.scope(self.clone(), fut).await
    }

    pub fn set_tree_registry(&self, reg: await_tree::Registry) {
        *self.tree.lock().unwrap_or_else(|e| e.into_inner()) = Some(reg);
    }

    pub fn current() -> Option<Arc<OpScope>> {
        CURRENT.try_with(|s| s.clone()).ok()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut OpScopeData) -> R) -> R {
        let mut g = self.data.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }

    pub fn push_request(&self, rec: RequestRecord) {
        let late = self.with(|d| {
            if d.closed {
                return Some(rec);
            }
            if rec.is_throttle() {
                d.throttled = true;
            }
            if d.requests.len() < MAX_REQUESTS_PER_OP {
                d.requests.push(rec);
            } else {
                d.dropped_requests += 1;
            }
            None
        });
        if let (Some(rec), Some(r), true) = (late, &self.late, self.measured) {
            r.record_unscoped_request(&rec);
        }
    }

    /// The retry layer is about to sleep `backoff` before the next attempt: account the
    /// attempt and the wait interval (attributed to *retry* in the time breakdown).
    pub fn record_retry(&self, backoff: std::time::Duration, throttled: bool) {
        let start = self.started.elapsed().as_micros() as u64;
        let us = backoff.as_micros() as u64;
        self.with(|d| {
            if d.closed {
                return;
            }
            d.retries += 1;
            d.retry_backoff_us += us;
            if d.retry_waits.len() < MAX_RETRY_WAITS_PER_OP {
                d.retry_waits.push((start, start + us));
            }
            if throttled {
                d.throttled = true;
            }
        });
    }

    /// Hand over everything recorded and close the scope (later requests go to `late`).
    pub fn take(&self) -> OpScopeData {
        self.with(|d| {
            let out = std::mem::take(d);
            d.closed = true;
            out
        })
    }
}

/// Executor wrapper capturing the spawning task's [`OpScope`] so that OpenDAL's concurrent
/// reader/writer sub-tasks (`.concurrent(n)`) are attributed to the right logical operation.
pub struct ScopePropagatingExecutor {
    inner: Arc<dyn Execute>,
}

impl ScopePropagatingExecutor {
    pub fn wrap(inner: Executor) -> Executor {
        Executor::with(Self {
            inner: inner.into_inner(),
        })
    }
}

impl Execute for ScopePropagatingExecutor {
    fn execute(&self, f: BoxedStaticFuture<()>) {
        match OpScope::current() {
            Some(scope) => {
                let reg = scope.tree.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match reg {
                    Some(reg) => {
                        let n = scope.subtasks.fetch_add(1, Ordering::Relaxed);
                        let root =
                            reg.register(SubTaskKey(scope.id, n), "opendal::background_task");
                        self.inner.execute(Box::pin(async move {
                            root.instrument(CURRENT.scope(scope, f)).await
                        }))
                    }
                    None => self
                        .inner
                        .execute(Box::pin(async move { CURRENT.scope(scope, f).await })),
                }
            }
            None => self.inner.execute(f),
        }
    }

    fn timeout(&self) -> Option<BoxedStaticFuture<()>> {
        self.inner.timeout()
    }
}
