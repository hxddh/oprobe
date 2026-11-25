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
    static CACHE_PROBE: Arc<AtomicU64>;
}

/// Run `fut` with an isolated per-lookup backend request counter (`0` if answered from cache).
pub async fn with_cache_probe<F: Future>(probe: Arc<AtomicU64>, fut: F) -> F::Output {
    CACHE_PROBE.scope(probe, fut).await
}

/// Synchronous counterpart of [`with_cache_probe`] (for `Drop` paths that tear down streams).
pub fn with_cache_probe_sync<R>(probe: Arc<AtomicU64>, f: impl FnOnce() -> R) -> R {
    CACHE_PROBE.sync_scope(probe, f)
}

/// Note that one backend request ran in the current task (increments any active cache probe).
pub fn note_backend_request() {
    let _ = CACHE_PROBE.try_with(|p| {
        p.fetch_add(1, Ordering::AcqRel);
    });
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

    /// Run `f` synchronously with this scope installed as the current task-local scope.
    pub fn sync_run<R>(self: &Arc<Self>, f: impl FnOnce() -> R) -> R {
        CURRENT.sync_scope(self.clone(), f)
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
        note_backend_request();
        let late = self.with(|d| {
            if d.closed {
                return Some(rec);
            }
            if rec.is_throttle() {
                d.throttled = true;
            }
            if d.requests.len() < MAX_REQUESTS_PER_OP {
                // Most operations issue one request: no room for four records up front.
                if d.requests.capacity() == 0 {
                    d.requests.reserve_exact(1);
                }
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

    /// Number of backend requests recorded on this scope so far.
    pub fn requests_recorded(&self) -> u64 {
        self.with(|d| d.requests.len() as u64 + d.dropped_requests)
    }

    /// Record a local cache lookup (hit or miss) performed for this operation.
    pub fn record_cache_lookup(&self, hit: bool, hit_bytes: u64, lookup_us: u64) {
        self.with(|d| {
            if d.closed {
                return;
            }
            d.cache_lookups += 1;
            d.cache_lookup_us += lookup_us;
            if hit {
                d.cache_hits += 1;
                d.cache_hit_bytes += hit_bytes;
            }
        });
    }

    /// The retry layer is about to sleep `backoff` before the next attempt: account the
    /// attempt and the wait interval (attributed to *retry* in the time breakdown).
    pub fn record_retry(&self, backoff: std::time::Duration, throttled: bool) {
        self.record_retry_with_cut(backoff, throttled, false);
    }

    /// [`Self::record_retry`], marking the preceding cancelled attempt as a `TailCutLayer` cut
    /// when `tail_cut` is set.
    pub fn record_retry_with_cut(
        &self,
        backoff: std::time::Duration,
        throttled: bool,
        tail_cut: bool,
    ) {
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
            if tail_cut
                && let Some(target) = d.requests.iter_mut().rev().find(|r| {
                    (r.status == 0 || r.error.is_some())
                        && !r.detail.as_deref().is_some_and(|det| det.cut)
                })
            {
                let det = target.detail.get_or_insert_with(Default::default);
                det.cut = true;
                det.aborted = true;
                det.temporary = Some(true);
                if det.error_code.is_none() {
                    det.error_code = Some("tail_cut".into());
                }
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
        let probe = CACHE_PROBE.try_with(|p| p.clone()).ok();
        let wrap_probe = |fut: BoxedStaticFuture<()>| -> BoxedStaticFuture<()> {
            match probe {
                Some(p) => Box::pin(async move { CACHE_PROBE.scope(p, fut).await }),
                None => fut,
            }
        };
        match OpScope::current() {
            Some(scope) => {
                let reg = scope.tree.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match reg {
                    Some(reg) => {
                        let n = scope.subtasks.fetch_add(1, Ordering::Relaxed);
                        let root =
                            reg.register(SubTaskKey(scope.id, n), "opendal::background_task");
                        let fut = wrap_probe(f);
                        self.inner.execute(Box::pin(async move {
                            root.instrument(CURRENT.scope(scope, fut)).await
                        }))
                    }
                    None => {
                        let fut = wrap_probe(f);
                        self.inner
                            .execute(Box::pin(async move { CURRENT.scope(scope, fut).await }))
                    }
                }
            }
            None => self.inner.execute(wrap_probe(f)),
        }
    }

    fn timeout(&self) -> Option<BoxedStaticFuture<()>> {
        self.inner.timeout()
    }
}
