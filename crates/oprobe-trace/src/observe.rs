//! Operations for an embedded operator (design v7 B1): [`OperationObserveLayer`] wraps every
//! service, whatever its scheme, and runs each OpenDAL call as one logical operation in an
//! [`OpScope`], so the HTTP requests (or in-process backend calls) it issues are linked to it,
//! the way oprobe's own engine links the requests of the operations it drives.
//!
//! What one operation is:
//! - `stat`, `create_dir`, `rename`, `restore`: one call;
//! - a read: one `open` (until its stream ends or is dropped) or one ranged `read`;
//! - a write, list, batch delete or copy: the writer's (lister's, …) life, from its creation
//!   until it is closed or dropped.
//!
//! Retries. The trace layer sits below the retry layer (it installs the HTTP transport, which the
//! layers above wrap), so OpenDAL's `RetryLayer` calls it again for every attempt. With a
//! [`RetryNotify`] installed (`RetryLayer::with_notify(handle.retry_notify())`), the attempts of
//! one call are one operation: a call that failed with a temporary error is parked on its task;
//! the retry notification (which the retry layer sends right after the failure, before another
//! future of the task runs) marks it retrying and records the backoff; the next attempt of the
//! same call (verb and key) on the same task (or, for a future driven by `block_on`, the same
//! thread) resumes it. Calls running concurrently in one task (`join_all`, `FuturesUnordered`)
//! each keep their own parked attempt. A parked call that is not retried completes as failed
//! (at the next call on its task, or when the handle takes a snapshot); one marked retrying whose
//! retry never comes completes as failed once it is stale. Without the notifier every attempt is
//! an operation of its own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use opendal_core::raw::oio::{self};
use opendal_core::raw::*;
use opendal_core::*;

use crate::{Api, OpCompletion, OpScope, Recorder};

/// Parked operations at most (calls that failed and may be retried); beyond it the oldest
/// complete as failed.
const MAX_PARKED: usize = 4096;

/// A call marked retrying whose retry never came (its future was dropped during the backoff)
/// completes as failed after this long.
const STALE_RETRY: std::time::Duration = std::time::Duration::from_secs(600);

/// The key an embedded operation records (design v7 B1, docs/privacy.md): the first path
/// segment (at most 32 bytes, so prefixes still slice the traffic) and a 48-bit hash of the
/// whole key (so hot keys and repeated reads are still told apart), never the key itself:
/// `logs/2026/11/a.parquet` → `logs/#3f2a…`.
pub fn private_key(path: &str) -> String {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return String::new();
    }
    // FNV-1a: stable across processes and Rust versions.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = String::with_capacity(48);
    if let Some(i) = path.find('/') {
        let mut end = i.min(32);
        while !path.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str(&path[..end]);
        out.push('/');
    }
    out.push('#');
    let h = h >> 16;
    for i in (0..12).rev() {
        out.push(char::from(b"0123456789abcdef"[((h >> (i * 4)) & 0xf) as usize]));
    }
    out
}

/// The error class of an operation that failed with `e` (the classes of oprobe's engine:
/// `rate_limited`, `not_found`, `condition_not_match`, `server_error`, `network`, …).
/// `last_status` is the HTTP status of its last request.
pub fn error_class(e: &Error, last_status: Option<u16>) -> String {
    use ErrorKind as K;
    let class = match e.kind() {
        K::RateLimited => "rate_limited",
        K::PermissionDenied => "permission_denied",
        K::NotFound => "not_found",
        K::ConfigInvalid => "config_invalid",
        K::Unsupported => "unsupported",
        K::IsADirectory => "is_a_directory",
        K::NotADirectory => "not_a_directory",
        K::AlreadyExists => "already_exists",
        K::IsSameFile => "is_same_file",
        K::ConditionNotMatch => "condition_not_match",
        K::RangeNotSatisfied => "range_not_satisfied",
        K::Conflict => "conflict",
        K::Unexpected => match (last_status, e.message()) {
            (Some(429 | 503), _) => "rate_limited",
            (Some(401 | 403), _) => "permission_denied",
            (_, "operation timeout reached" | "io operation timeout reached") => "timeout",
            (_, "cancelled by tail cut" | "io cancelled by tail cut") => "tail_cut",
            (Some(s), _) if s >= 500 => "server_error",
            _ if e.is_temporary() => "network",
            _ => "unexpected",
        },
        other => return other.into_static().to_ascii_lowercase().replace(' ', "_"),
    };
    class.to_owned()
}

/// What an operation in progress carries.
struct OpState {
    scope: Arc<OpScope>,
    verb: &'static str,
    key: String,
    started: Instant,
    read: u64,
    written: u64,
}

/// Coalesces chunked or multi-range `oio::Read::read` calls issued on one `OpReader` (OpenDAL's
/// `Reader::fetch` and concurrent chunked reads) into a single logical read operation.
const READ_COALESCE_IDLE: std::time::Duration = std::time::Duration::from_millis(5);

struct ReadGroup {
    st: OpState,
    active: usize,
    calls: usize,
    last_end: Option<Instant>,
    error: Option<Error>,
    /// A read of the group was dropped before it returned (a timeout, the application).
    cancelled: bool,
}

impl ReadGroup {
    /// Complete the group's operation, as cancelled when one of its reads was dropped.
    fn finish(self, obs: &Observer) {
        let end = self.last_end.unwrap_or_else(Instant::now);
        if self.cancelled {
            complete(&obs.recorder, self.st, end, Some("cancelled".into()));
        } else {
            obs.finish_at(self.st, end, self.error.as_ref(), self.calls == 1);
        }
    }
}

/// One read of a [`ReadGroup`] in flight: dropped before it returned, it leaves the group,
/// which then completes as cancelled. (9.3.3 left the group's count of reads above zero, and
/// the operation never completed: it stayed live and in flight, and held its intervals open.)
struct ActiveRead<'a> {
    slot: &'a ReadSlot,
    returned: bool,
}

impl Drop for ActiveRead<'_> {
    fn drop(&mut self) {
        if self.returned {
            return;
        }
        let mut g = self.slot.group.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = g.as_mut() {
            x.active = x.active.saturating_sub(1);
            x.last_end = Some(Instant::now());
            x.cancelled = true;
        }
    }
}

/// Shared slot registered with [`Linker`] while an [`OpReader`] holds a coalesced [`ReadGroup`],
/// so [`Linker::flush`] (`Handle::snapshot`) completes finished bursts even before the caller
/// drops its `Reader`.
struct ReadSlot {
    group: Mutex<Option<ReadGroup>>,
    obs: Observer,
}

impl ReadSlot {
    fn flush_idle(&self) {
        let done = {
            let mut g = self.group.lock().unwrap_or_else(|e| e.into_inner());
            if g.as_ref().is_some_and(|x| x.active == 0) {
                g.take()
            } else {
                None
            }
        };
        if let Some(g) = done {
            g.finish(&self.obs);
        }
    }
}

fn same_op_family(a: &str, b: &str) -> bool {
    a == b || (a.starts_with("read.") && b.starts_with("read."))
}

/// Where a call runs: its Tokio task, or (a future driven by `block_on`, which never moves) its
/// thread. A retry layer retries a call on the same one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key {
    Task(tokio::task::Id),
    Thread(std::thread::ThreadId),
}

fn key() -> Key {
    match tokio::task::try_id() {
        Some(id) => Key::Task(id),
        None => Key::Thread(std::thread::current().id()),
    }
}

/// A call parked after a temporary failure, waiting to learn whether it is retried.
struct Parked {
    op: OpState,
    ended: Instant,
    error: String,
    retrying: bool,
}

/// Links the attempts of a retried call (see the module documentation).
#[derive(Default)]
pub struct Linker {
    /// A [`RetryNotify`] exists: temporary failures are parked.
    enabled: AtomicBool,
    /// Per task, its parked calls in the order they failed.
    parked: Mutex<HashMap<Key, Vec<Parked>>>,
    /// Multi-call operations (writers, listers, …) whose last call failed temporarily: the
    /// scope a retry notification on that task belongs to.
    scopes: Mutex<HashMap<Key, Arc<OpScope>>>,
    /// Active [`OpReader`] slots holding coalesced chunked/range reads.
    readers: Mutex<Vec< std::sync::Weak<ReadSlot> >>,
}

impl std::fmt::Debug for Linker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Linker")
    }
}

impl Linker {
    fn parked(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Vec<Parked>>> {
        self.parked.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scopes(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Arc<OpScope>>> {
        self.scopes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Complete every parked call that is not being retried, or whose retry never came
    /// (before a snapshot), plus any idle coalesced [`OpReader`] bursts.
    pub fn flush(&self, rec: &Recorder) {
        let slots: Vec<Arc<ReadSlot>> = {
            let mut r = self.readers.lock().unwrap_or_else(|e| e.into_inner());
            let mut live = Vec::with_capacity(r.len());
            r.retain(|w| match w.upgrade() {
                Some(s) => {
                    live.push(s);
                    true
                }
                None => false,
            });
            live
        };
        for s in slots {
            s.flush_idle();
        }
        if !self.enabled() {
            return;
        }
        let done: Vec<Parked> = {
            let mut p = self.parked();
            let mut done = Vec::new();
            for calls in p.values_mut() {
                done.extend(take_where(calls, |x| {
                    !x.retrying || x.ended.elapsed() > STALE_RETRY
                }));
            }
            p.retain(|_, calls| !calls.is_empty());
            done
        };
        for x in done {
            complete(rec, x.op, x.ended, Some(x.error));
        }
    }

    /// Take the parked calls of task `id` that were not retried: the retry notification
    /// follows a failure at once, so a call still not marked retrying never will be.
    fn take_not_retried(&self, id: Key) -> Vec<Parked> {
        let mut p = self.parked();
        let Some(calls) = p.get_mut(&id) else {
            return Vec::new();
        };
        let done = take_where(calls, |x| !x.retrying);
        if calls.is_empty() {
            p.remove(&id);
        }
        done
    }
}

/// Remove and return the elements of `v` matching `f`, keeping the order of the others.
fn take_where(v: &mut Vec<Parked>, f: impl Fn(&Parked) -> bool) -> Vec<Parked> {
    let (out, keep) = std::mem::take(v).into_iter().partition(|x| f(x));
    *v = keep;
    out
}

/// The retry notifier of [`crate::Handle::retry_notify`]: pass it to OpenDAL's
/// `RetryLayer::with_notify`, so retries land in the profile's Retries section (count, backoff,
/// throttling) and the attempts of one call are one operation.
#[derive(Clone)]
pub struct RetryNotify {
    recorder: Arc<Recorder>,
    linker: Arc<Linker>,
}

impl std::fmt::Debug for RetryNotify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetryNotify")
    }
}

impl RetryNotify {
    pub(crate) fn new(recorder: Arc<Recorder>, linker: Arc<Linker>) -> Self {
        linker.enabled.store(true, Ordering::Relaxed);
        Self { recorder, linker }
    }

    /// The retry layer is about to wait `backoff` before retrying after `err`.
    pub fn retrying(&self, err: &Error, backoff: std::time::Duration) {
        let throttled = err.kind() == ErrorKind::RateLimited;
        let tail_cut = err.message().contains("tail cut");
        // The trace layer above the retry layer: the operation is the current scope.
        if let Some(s) = OpScope::current() {
            s.record_retry_with_cut(backoff, throttled, tail_cut);
            return;
        }
        let id = key();
        // The call that just failed on this task: the last one parked and not yet retrying.
        if let Some(p) = self
            .linker
            .parked()
            .get_mut(&id)
            .and_then(|calls| calls.iter_mut().rev().find(|p| !p.retrying))
        {
            p.retrying = true;
            p.op.scope
                .record_retry_with_cut(backoff, throttled, tail_cut);
            return;
        }
        if let Some(s) = self.linker.scopes().remove(&id) {
            s.record_retry_with_cut(backoff, throttled, tail_cut);
            return;
        }
        self.recorder.record_unscoped_retry(backoff);
    }
}

impl opendal_layer_retry::RetryInterceptor for RetryNotify {
    fn intercept(&self, event: opendal_layer_retry::RetryEvent<'_>) {
        self.retrying(event.err, event.retry_after);
    }
}

/// Hand a completed operation to the recorder.
fn complete(rec: &Recorder, st: OpState, end: Instant, error: Option<String>) {
    let data = st.scope.take();
    let latency_us = end.saturating_duration_since(st.started).as_micros() as u64;
    let verb = match st.verb {
        "write"
            if data.requests.iter().any(|r| {
                matches!(
                    r.api,
                    Api::UploadPart | Api::CreateMultipart | Api::CompleteMultipart
                )
            }) =>
        {
            "write.multipart"
        }
        v => v,
    };
    rec.complete_op(
        true,
        OpCompletion {
            verb,
            op_id: st.scope.id,
            started: st.started,
            latency_us,
            logical_read: st.read,
            logical_write: st.written,
            error,
            key: st.key,
            data,
            schedule_us: None,
        },
    );
}

#[derive(Clone)]
struct Observer {
    recorder: Arc<Recorder>,
    linker: Arc<Linker>,
}

impl Observer {
    /// Start (or, for the retry of a parked call on this task, resume) an operation. `None`
    /// when an operation is already in progress on this task (an outer trace layer, or
    /// oprobe's engine): its scope collects the requests.
    fn start(&self, verb: &'static str, path: &str) -> Option<OpState> {
        if OpScope::current().is_some() {
            return None;
        }
        let key_of_path = private_key(path);
        if self.linker.enabled() {
            let id = key();
            for p in self.linker.take_not_retried(id) {
                complete(&self.recorder, p.op, p.ended, Some(p.error));
            }
            // The retry of a parked call: the retry layer calls again with the same verb family
            // (a mid-stream `read.full` retry resumes with `read.range`) and path.
            let mut parked = self.linker.parked();
            if let Some(calls) = parked.get_mut(&id)
                && let Some(i) = calls
                    .iter()
                    .position(|p| same_op_family(p.op.verb, verb) && p.op.key == key_of_path)
            {
                let p = calls.remove(i);
                if calls.is_empty() {
                    parked.remove(&id);
                }
                return Some(p.op);
            }
        }
        let started = Instant::now();
        let scope = OpScope::new_at(true, started, Some(self.recorder.clone()));
        self.recorder.op_started_at(scope.id, started);
        Some(OpState {
            scope,
            verb,
            key: key_of_path,
            started,
            read: 0,
            written: 0,
        })
    }

    /// End a single-call operation: a temporary failure is parked while a retry may follow.
    fn finish(&self, st: OpState, err: Option<&Error>) {
        self.finish_at(st, Instant::now(), err, true);
    }

    fn finish_at(&self, st: OpState, end: Instant, err: Option<&Error>, may_park: bool) {
        let Some(e) = err else {
            complete(&self.recorder, st, end, None);
            return;
        };
        let class = class_of(&st.scope, e);
        if may_park && e.is_temporary() && self.linker.enabled() {
            let id = key();
            // An earlier failure of a multi-call operation on this task was not retried.
            self.linker.scopes().remove(&id);
            let mut done = self.linker.take_not_retried(id);
            {
                let mut p = self.linker.parked();
                if p.values().map(Vec::len).sum::<usize>() >= MAX_PARKED {
                    // Full: the oldest parked call completes as failed.
                    let oldest = p
                        .iter()
                        .flat_map(|(k, calls)| {
                            calls.iter().enumerate().map(move |(i, x)| (*k, i, x.ended))
                        })
                        .min_by_key(|(_, _, ended)| *ended)
                        .map(|(k, i, _)| (k, i));
                    if let Some((k, i)) = oldest
                        && let Some(calls) = p.get_mut(&k)
                    {
                        done.push(calls.remove(i));
                        if calls.is_empty() {
                            p.remove(&k);
                        }
                    }
                }
                p.entry(id).or_default().push(Parked {
                    op: st,
                    ended: end,
                    error: class,
                    retrying: false,
                });
            }
            for x in done {
                complete(&self.recorder, x.op, x.ended, Some(x.error));
            }
            return;
        }
        complete(&self.recorder, st, end, Some(class));
    }

    /// Creating a writer (lister, copier) failed: the operation ends there.
    fn failed_start(&self, st: Option<OpState>, e: Error) -> Error {
        if let Some(st) = st {
            let class = class_of(&st.scope, &e);
            complete(&self.recorder, st, Instant::now(), Some(class));
        }
        e
    }

    /// A call of a multi-call operation failed: a retry notification on this task belongs to
    /// its scope.
    fn failed_call(&self, st: &OpState, e: &Error) {
        if e.is_temporary() && self.linker.enabled() {
            // Parked calls of this task not retried by now never will be: the notification
            // that follows is this call's.
            for p in self.linker.take_not_retried(key()) {
                complete(&self.recorder, p.op, p.ended, Some(p.error));
            }
            let mut s = self.linker.scopes();
            if s.len() < MAX_PARKED {
                s.insert(key(), st.scope.clone());
            }
        }
    }
}

fn class_of(scope: &OpScope, e: &Error) -> String {
    let last = scope.with(|d| d.requests.last().map(|r| r.status));
    error_class(e, last)
}

/// Run `fut` in the operation's scope (as is when there is none).
async fn in_scope<F: std::future::Future>(st: &Option<OpState>, fut: F) -> F::Output {
    match st {
        Some(s) => s.scope.run(fut).await,
        None => fut.await,
    }
}

/// Synchronous counterpart of [`in_scope`] (constructor calls and `Drop` cleanup).
fn in_sync_scope<R>(st: &Option<OpState>, f: impl FnOnce() -> R) -> R {
    match st {
        Some(s) => s.scope.sync_run(f),
        None => f(),
    }
}

/// An operation that completes when dropped, as cancelled, unless it was finished: a future
/// dropped by a timeout or the application is still counted.
struct OpGuard {
    obs: Observer,
    st: Option<OpState>,
}

impl OpGuard {
    fn new(obs: &Observer, verb: &'static str, path: &str) -> Self {
        Self {
            st: obs.start(verb, path),
            obs: obs.clone(),
        }
    }

    fn finish(mut self, err: Option<&Error>) {
        if let Some(st) = self.st.take() {
            self.obs.finish(st, err);
        }
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        if let Some(st) = self.st.take() {
            complete(
                &self.obs.recorder,
                st,
                Instant::now(),
                Some("cancelled".into()),
            );
        }
    }
}

/// Wraps every service in operation scopes (see the module documentation).
#[derive(Clone)]
pub struct OperationObserveLayer {
    obs: Observer,
}

impl std::fmt::Debug for OperationObserveLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OperationObserveLayer")
    }
}

impl OperationObserveLayer {
    pub fn new(recorder: Arc<Recorder>, linker: Arc<Linker>) -> Self {
        Self {
            obs: Observer { recorder, linker },
        }
    }
}

impl Layer for OperationObserveLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        Arc::new(OpService {
            inner,
            obs: self.obs.clone(),
        })
    }
}

struct OpService {
    inner: Servicer,
    obs: Observer,
}

impl std::fmt::Debug for OpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OpService")
    }
}

impl OpService {
    async fn one<T>(
        &self,
        verb: &'static str,
        path: &str,
        fut: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        if OpScope::current().is_some() {
            return fut.await;
        }
        let g = OpGuard::new(&self.obs, verb, path);
        let r = in_scope(&g.st, fut).await;
        g.finish(r.as_ref().err());
        r
    }
}

impl Service for OpService {
    type Reader = OpReader;
    type Writer = OpWriter;
    type Lister = OpLister;
    type Deleter = OpDeleter;
    type Copier = OpCopier;
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
        self.one("create_dir", path, self.inner.create_dir(ctx, path, args))
            .await
    }

    fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
        Ok(OpReader {
            inner: self.inner.read(ctx, path, args)?,
            obs: self.obs.clone(),
            path: path.to_owned(),
            slot: std::sync::OnceLock::new(),
        })
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        let st = if OpScope::current().is_some() {
            None
        } else {
            self.obs.start("write", path)
        };
        let inner = match in_sync_scope(&st, || self.inner.write(ctx, path, args)) {
            Ok(i) => i,
            Err(e) => return Err(self.obs.failed_start(st, e)),
        };
        Ok(OpWriter {
            inner: Some(inner),
            obs: self.obs.clone(),
            st,
            error: None,
            closed: false,
        })
    }

    fn copy(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpCopy,
    ) -> Result<Self::Copier> {
        let st = if OpScope::current().is_some() {
            None
        } else {
            self.obs.start("copy", to)
        };
        let inner = match in_sync_scope(&st, || self.inner.copy(ctx, from, to, args)) {
            Ok(i) => i,
            Err(e) => return Err(self.obs.failed_start(st, e)),
        };
        Ok(OpCopier {
            inner: Some(inner),
            obs: self.obs.clone(),
            st,
            error: None,
        })
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
        self.one("rename", to, self.inner.rename(ctx, from, to, args))
            .await
    }

    async fn restore(
        &self,
        ctx: &OperationContext,
        path: &str,
        args: OpRestore,
    ) -> Result<RpRestore> {
        self.one("restore", path, self.inner.restore(ctx, path, args))
            .await
    }

    async fn stat(&self, ctx: &OperationContext, path: &str, args: OpStat) -> Result<RpStat> {
        self.one("stat", path, self.inner.stat(ctx, path, args))
            .await
    }

    fn delete(&self, ctx: &OperationContext) -> Result<Self::Deleter> {
        Ok(OpDeleter {
            inner: Some(self.inner.delete(ctx)?),
            obs: self.obs.clone(),
            st: None,
            started: false,
            error: None,
        })
    }

    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        let st = if OpScope::current().is_some() {
            None
        } else {
            self.obs.start("list", path)
        };
        let inner = match in_sync_scope(&st, || self.inner.list(ctx, path, args)) {
            Ok(i) => i,
            Err(e) => return Err(self.obs.failed_start(st, e)),
        };
        Ok(OpLister {
            inner: Some(inner),
            obs: self.obs.clone(),
            st,
            error: None,
        })
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

/// A multi-call operation's end: `error` is the last call's failure, if it was not followed
/// by a success.
fn end_multi(obs: &Observer, st: &mut Option<OpState>, error: Option<String>) {
    if let Some(st) = st.take() {
        complete(&obs.recorder, st, Instant::now(), error);
    }
}

pub struct OpReader {
    inner: oio::Reader,
    obs: Observer,
    path: String,
    slot: std::sync::OnceLock<Arc<ReadSlot>>,
}

fn read_verb(range: &BytesRange) -> &'static str {
    if range.is_full() {
        "read.full"
    } else {
        "read.range"
    }
}

impl oio::Read for OpReader {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        if OpScope::current().is_some() {
            return self.inner.open(range).await;
        }
        let mut g = OpGuard::new(&self.obs, read_verb(&range), &self.path);
        match in_scope(&g.st, self.inner.open(range)).await {
            Ok((rp, stream)) => {
                let st = g.st.take();
                Ok((
                    rp,
                    Box::new(OpStream {
                        inner: Some(stream),
                        obs: self.obs.clone(),
                        st,
                    }) as Box<dyn oio::ReadStreamDyn>,
                ))
            }
            Err(e) => {
                g.finish(Some(&e));
                Err(e)
            }
        }
    }

    async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
        if OpScope::current().is_some() {
            return self.inner.read(range).await;
        }
        let verb = read_verb(&range);
        let slot = self.slot.get_or_init(|| {
            let s = Arc::new(ReadSlot {
                group: Mutex::new(None),
                obs: self.obs.clone(),
            });
            let mut r = self
                .obs
                .linker
                .readers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if r.len() >= MAX_PARKED {
                r.retain(|w| w.strong_count() > 0);
            }
            r.push(Arc::downgrade(&s));
            s
        });
        let (flushed, scope) = {
            let mut g = slot.group.lock().unwrap_or_else(|e| e.into_inner());
            let mut flushed = None;
            let reuse = g.as_ref().is_some_and(|x| {
                x.active > 0
                    || x.last_end
                        .is_some_and(|t| t.elapsed() <= READ_COALESCE_IDLE)
            });
            if !reuse {
                flushed = g.take();
            }
            if let Some(x) = g.as_mut() {
                x.active += 1;
                x.calls += 1;
                if verb == "read.range" || x.calls > 1 {
                    x.st.verb = "read.range";
                }
                (flushed, Some(x.st.scope.clone()))
            } else if let Some(st) = self.obs.start(verb, &self.path) {
                let sc = st.scope.clone();
                *g = Some(ReadGroup {
                    st,
                    active: 1,
                    calls: 1,
                    last_end: None,
                    error: None,
                    cancelled: false,
                });
                (flushed, Some(sc))
            } else {
                (flushed, None)
            }
        };
        if let Some(prev) = flushed {
            prev.finish(&self.obs);
        }
        let mut active = scope.as_ref().map(|_| ActiveRead {
            slot,
            returned: false,
        });
        let r = match &scope {
            Some(s) => s.run(self.inner.read(range)).await,
            None => self.inner.read(range).await,
        };
        let end = Instant::now();
        if let Some(a) = active.as_mut() {
            a.returned = true;
        }
        {
            let mut g = slot.group.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(x) = g.as_mut() {
                x.active = x.active.saturating_sub(1);
                x.last_end = Some(end);
                match &r {
                    Ok((_, b)) => {
                        x.st.read += b.len() as u64;
                        x.error = None;
                    }
                    Err(e) => {
                        if x.calls > 1 {
                            self.obs.failed_call(&x.st, e);
                        }
                        x.error = Some(Error::new(e.kind(), e.message().to_owned()));
                        if e.is_temporary()
                            && let Some(err) = x.error.take()
                        {
                            x.error = Some(err.set_temporary());
                        }
                    }
                }
            }
        }
        r
    }
}

impl Drop for OpReader {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.get() {
            slot.flush_idle();
        }
    }
}

/// The body of an opened read: the operation ends with the stream (its end, an error, or the
/// stream dropped part-way).
pub struct OpStream {
    inner: Option<Box<dyn oio::ReadStreamDyn>>,
    obs: Observer,
    st: Option<OpState>,
}

impl oio::ReadStream for OpStream {
    async fn read(&mut self) -> Result<Buffer> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Buffer::new());
        };
        // Keep the operation scope active while reading so mid-stream retries link here.
        let r = in_scope(&self.st, inner.read_dyn()).await;
        match &r {
            Ok(b) if b.is_empty() => {
                drop(self.inner.take());
                if let Some(st) = self.st.take() {
                    self.obs.finish(st, None);
                }
            }
            Ok(b) => {
                if let Some(st) = self.st.as_mut() {
                    st.read += b.len() as u64;
                }
            }
            Err(e) => {
                drop(self.inner.take());
                if let Some(st) = self.st.take() {
                    self.obs.finish(st, Some(e));
                }
            }
        }
        r
    }
}

impl Drop for OpStream {
    fn drop(&mut self) {
        // Drop the inner body stream inside `OpScope` BEFORE closing it so `TimedResponseBody::drop`
        // records its `aborted` `RequestRecord` onto this operation instead of orphaning it.
        if let Some(inner) = self.inner.take() {
            in_sync_scope(&self.st, || drop(inner));
        }
        if let Some(st) = self.st.take() {
            complete(&self.obs.recorder, st, Instant::now(), None);
        }
    }
}

pub struct OpWriter {
    inner: Option<oio::Writer>,
    obs: Observer,
    st: Option<OpState>,
    error: Option<String>,
    closed: bool,
}

impl OpWriter {
    fn note<T>(&mut self, r: &Result<T>) {
        match (r, &self.st) {
            (Ok(_), _) => self.error = None,
            (Err(e), Some(st)) => {
                self.error = Some(class_of(&st.scope, e));
                self.obs.failed_call(st, e);
            }
            (Err(_), None) => {}
        }
    }
}

impl oio::Write for OpWriter {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        let n = bs.len() as u64;
        let inner = self.inner.as_mut().expect("writer open");
        let r = in_scope(&self.st, inner.write(bs)).await;
        if let (Ok(()), Some(st)) = (&r, self.st.as_mut()) {
            st.written += n;
        }
        self.note(&r);
        r
    }

    async fn copy_from(&mut self, path: &str, args: OpRead, range: BytesRange) -> Result<()> {
        let n = range.size().unwrap_or(0);
        let inner = self.inner.as_mut().expect("writer open");
        let r = in_scope(&self.st, inner.copy_from(path, args, range)).await;
        if let (Ok(()), Some(st)) = (&r, self.st.as_mut()) {
            st.written += n;
        }
        self.note(&r);
        r
    }

    async fn close(&mut self) -> Result<Metadata> {
        let inner = self.inner.as_mut().expect("writer open");
        let r = in_scope(&self.st, inner.close()).await;
        self.note(&r);
        if r.is_ok() {
            self.closed = true;
            drop(self.inner.take());
            end_multi(&self.obs, &mut self.st, None);
        }
        r
    }

    async fn abort(&mut self) -> Result<()> {
        let inner = self.inner.as_mut().expect("writer open");
        let r = in_scope(&self.st, inner.abort()).await;
        self.closed = true;
        drop(self.inner.take());
        let error = self.error.take().or_else(|| Some("aborted".into()));
        end_multi(&self.obs, &mut self.st, error);
        r
    }
}

impl Drop for OpWriter {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            in_sync_scope(&self.st, || drop(inner));
        }
        let error = self.error.take().or_else(|| Some("cancelled".into()));
        end_multi(&self.obs, &mut self.st, error.filter(|_| !self.closed));
    }
}

pub struct OpLister {
    inner: Option<oio::Lister>,
    obs: Observer,
    st: Option<OpState>,
    error: Option<String>,
}

impl oio::List for OpLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        let r = in_scope(&self.st, inner.next()).await;
        match (&r, &self.st) {
            (Ok(Some(_)), _) => self.error = None,
            (Ok(None), _) => {
                self.error = None;
                drop(self.inner.take());
                end_multi(&self.obs, &mut self.st, None);
            }
            (Err(e), Some(st)) => {
                self.error = Some(class_of(&st.scope, e));
                self.obs.failed_call(st, e);
            }
            (Err(_), None) => {}
        }
        r
    }
}

impl Drop for OpLister {
    fn drop(&mut self) {
        // Dropped part-way: the application stopped listing (not a failure).
        if let Some(inner) = self.inner.take() {
            in_sync_scope(&self.st, || drop(inner));
        }
        let error = self.error.take();
        end_multi(&self.obs, &mut self.st, error);
    }
}

/// A batch delete: one operation from the first path queued until the deleter is closed.
pub struct OpDeleter {
    inner: Option<oio::Deleter>,
    obs: Observer,
    st: Option<OpState>,
    started: bool,
    error: Option<String>,
}

impl OpDeleter {
    fn note<T>(&mut self, r: &Result<T>) {
        match (r, &self.st) {
            (Ok(_), _) => self.error = None,
            (Err(e), Some(st)) => {
                self.error = Some(class_of(&st.scope, e));
                self.obs.failed_call(st, e);
            }
            (Err(_), None) => {}
        }
    }
}

impl oio::Delete for OpDeleter {
    async fn delete(&mut self, path: &str, args: OpDelete) -> Result<()> {
        if !self.started {
            self.started = true;
            if OpScope::current().is_none() {
                self.st = self.obs.start("delete", path);
            }
        }
        let inner = self.inner.as_mut().expect("deleter open");
        let r = in_scope(&self.st, inner.delete(path, args)).await;
        self.note(&r);
        r
    }

    async fn close(&mut self) -> Result<()> {
        let inner = self.inner.as_mut().expect("deleter open");
        let r = in_scope(&self.st, inner.close()).await;
        self.note(&r);
        if r.is_ok() {
            drop(self.inner.take());
            end_multi(&self.obs, &mut self.st, None);
        }
        r
    }
}

impl Drop for OpDeleter {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            in_sync_scope(&self.st, || drop(inner));
        }
        let error = self.error.take();
        end_multi(&self.obs, &mut self.st, error);
    }
}

pub struct OpCopier {
    inner: Option<oio::Copier>,
    obs: Observer,
    st: Option<OpState>,
    error: Option<String>,
}

impl OpCopier {
    fn note<T>(&mut self, r: &Result<T>) {
        match (r, &self.st) {
            (Ok(_), _) => self.error = None,
            (Err(e), Some(st)) => {
                self.error = Some(class_of(&st.scope, e));
                self.obs.failed_call(st, e);
            }
            (Err(_), None) => {}
        }
    }
}

impl oio::Copy for OpCopier {
    async fn next(&mut self) -> Result<Option<usize>> {
        let inner = self.inner.as_mut().expect("copier open");
        let r = in_scope(&self.st, inner.next()).await;
        if let (Ok(Some(n)), Some(st)) = (&r, self.st.as_mut()) {
            st.written += *n as u64;
        }
        self.note(&r);
        r
    }

    async fn close(&mut self) -> Result<Metadata> {
        let inner = self.inner.as_mut().expect("copier open");
        let r = in_scope(&self.st, inner.close()).await;
        self.note(&r);
        if r.is_ok() {
            drop(self.inner.take());
            end_multi(&self.obs, &mut self.st, None);
        }
        r
    }

    async fn abort(&mut self) -> Result<()> {
        let inner = self.inner.as_mut().expect("copier open");
        let r = in_scope(&self.st, inner.abort()).await;
        drop(self.inner.take());
        let error = self.error.take().or_else(|| Some("aborted".into()));
        end_multi(&self.obs, &mut self.st, error);
        r
    }
}

impl Drop for OpCopier {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            in_sync_scope(&self.st, || drop(inner));
        }
        let error = self.error.take();
        end_multi(&self.obs, &mut self.st, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_never_recorded_in_full() {
        let k = private_key("logs/2026/11/secret-customer.parquet");
        assert!(k.starts_with("logs/#"), "{k}");
        assert!(!k.contains("secret"), "{k}");
        assert_eq!(k.len(), "logs/#".len() + 12);
        // Stable, and distinct keys stay distinct.
        assert_eq!(k, private_key("logs/2026/11/secret-customer.parquet"));
        assert_ne!(k, private_key("logs/2026/11/other.parquet"));
        // A flat key keeps no clear text; a long first segment is truncated.
        assert!(private_key("flat-secret").starts_with('#'));
        let long = private_key(&format!("{}/x", "a".repeat(100)));
        assert_eq!(long.split('/').next().unwrap().len(), 32);
        assert_eq!(private_key(""), "");
    }

    fn linked() -> (Arc<Recorder>, Observer, RetryNotify) {
        let rec = Recorder::always_on(100);
        let linker = Arc::new(Linker::default());
        let notify = RetryNotify::new(rec.clone(), linker.clone());
        let obs = Observer {
            recorder: rec.clone(),
            linker,
        };
        (rec, obs, notify)
    }

    fn slow_down() -> Error {
        Error::new(ErrorKind::RateLimited, "slow down").set_temporary()
    }

    /// T5: a read dropped before it returns (a timeout) completes its operation, as
    /// cancelled. 9.3.3 never decremented the group's count of reads in flight, and the
    /// operation stayed live and in flight for good.
    #[tokio::test]
    async fn a_dropped_read_completes_as_cancelled() {
        struct Hang;
        impl oio::Read for Hang {
            async fn open(&self, _: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
                futures::future::pending().await
            }
            async fn read(&self, _: BytesRange) -> Result<(RpRead, Buffer)> {
                futures::future::pending().await
            }
        }
        let (rec, obs, _) = linked();
        let reader = OpReader {
            inner: Box::new(Hang),
            obs: obs.clone(),
            path: "a/b".into(),
            slot: std::sync::OnceLock::new(),
        };
        let cut = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            oio::Read::read(&reader, BytesRange::new(0, Some(10))),
        )
        .await;
        assert!(cut.is_err(), "the read hangs");
        drop(reader);
        let s = rec.snapshot();
        assert_eq!(s.summary.ops, 1, "{:?}", s.summary);
        assert_eq!(s.summary.failed_ops, 1);
        assert_eq!(s.resilience.error_classes.get("cancelled"), Some(&1));
        assert_eq!(s.summary.inflight_ops, 0);
    }

    #[tokio::test]
    async fn retried_attempts_are_one_operation() {
        let (rec, obs, notify) = linked();
        // In a task and in `block_on` alike.
        let in_task = {
            let obs = obs.clone();
            let notify = notify.clone();
            tokio::spawn(async move {
                let st = obs.start("stat", "a/b").unwrap();
                obs.finish(st, Some(&slow_down()));
                notify.retrying(&slow_down(), std::time::Duration::from_millis(5));
                let st = obs.start("stat", "a/b").unwrap();
                obs.finish(st, None);
            })
        };
        in_task.await.unwrap();
        let st = obs.start("stat", "a/c").unwrap();
        obs.finish(st, Some(&slow_down()));
        notify.retrying(&slow_down(), std::time::Duration::from_millis(5));
        let st = obs.start("stat", "a/c").unwrap();
        obs.finish(st, None);
        obs.linker.flush(&rec);
        let s = rec.snapshot();
        assert_eq!(s.summary.ops, 2, "two calls, four attempts");
        assert_eq!(s.summary.failed_ops, 0);
        assert_eq!(s.resilience.retries, 2);
        assert!((s.resilience.retry_backoff_ms - 10.0).abs() < 1e-9);
        assert_eq!(s.verbs[0].retries, 2);
    }

    #[tokio::test]
    async fn a_call_not_retried_fails_once() {
        let (rec, obs, notify) = linked();
        let st = obs.start("stat", "a/b").unwrap();
        obs.finish(st, Some(&slow_down()));
        notify.retrying(&slow_down(), std::time::Duration::from_millis(1));
        let st = obs.start("stat", "a/b").unwrap();
        // Retries exhausted: no notification follows.
        obs.finish(st, Some(&slow_down()));
        // Parked until the handle takes a snapshot (or the next call on this task).
        assert_eq!(rec.snapshot().summary.ops, 0);
        obs.linker.flush(&rec);
        let s = rec.snapshot();
        assert_eq!(s.summary.ops, 1);
        assert_eq!(s.summary.failed_ops, 1);
        assert_eq!(s.resilience.retries, 1);
        assert_eq!(s.resilience.error_classes.get("rate_limited"), Some(&1));
        // A persistent failure is never parked.
        let st = obs.start("stat", "a/b").unwrap();
        obs.finish(st, Some(&Error::new(ErrorKind::NotFound, "no")));
        assert_eq!(rec.snapshot().summary.ops, 2);
        // A call marked retrying is resumed only by the same call: another call on the task
        // starts afresh (the first may be waiting out its backoff concurrently), and the
        // abandoned call completes as failed once stale.
        let st = obs.start("stat", "a/b").unwrap();
        obs.finish(st, Some(&slow_down()));
        notify.retrying(&slow_down(), std::time::Duration::from_millis(1));
        let st = obs.start("stat", "a/other").unwrap();
        obs.finish(st, None);
        let s = rec.snapshot();
        assert_eq!(s.summary.ops, 3);
        assert_eq!(s.summary.failed_ops, 2, "the exhausted retry and the not-found");
        for calls in obs.linker.parked().values_mut() {
            for p in calls {
                p.ended -= STALE_RETRY + std::time::Duration::from_secs(1);
            }
        }
        obs.linker.flush(&rec);
        let s = rec.snapshot();
        assert_eq!(s.summary.ops, 4);
        assert_eq!(s.summary.failed_ops, 3, "the exhausted retry, the not-found and the abandoned call");
    }

    /// Calls running concurrently in one task (`join_all`) under throttling: each call's
    /// attempts are one operation, whatever order the attempts and retries interleave in.
    #[tokio::test]
    async fn concurrent_calls_in_one_task_keep_their_own_retries() {
        let (rec, obs, notify) = linked();
        let backoff = |ms| std::time::Duration::from_millis(ms);
        tokio::spawn(async move {
            // Three calls in flight; each fails, and the retry layer notifies right away.
            let a = obs.start("stat", "a/1").unwrap();
            let b = obs.start("read.full", "a/2").unwrap();
            let c = obs.start("stat", "a/3").unwrap();
            obs.finish(a, Some(&slow_down()));
            notify.retrying(&slow_down(), backoff(1));
            obs.finish(b, Some(&slow_down()));
            notify.retrying(&slow_down(), backoff(2));
            obs.finish(c, Some(&slow_down()));
            notify.retrying(&slow_down(), backoff(4));
            // Their retries in another order; `c` fails again and is retried once more.
            let c = obs.start("stat", "a/3").unwrap();
            let a = obs.start("stat", "a/1").unwrap();
            obs.finish(c, Some(&slow_down()));
            notify.retrying(&slow_down(), backoff(8));
            obs.finish(a, None);
            let b = obs.start("read.full", "a/2").unwrap();
            let c = obs.start("stat", "a/3").unwrap();
            obs.finish(b, None);
            obs.finish(c, None);
            obs.linker.flush(&rec);
            let s = rec.snapshot();
            assert_eq!(s.summary.ops, 3, "three calls");
            assert_eq!(s.summary.failed_ops, 0, "every call succeeded on a retry");
            assert_eq!(s.resilience.retries, 4);
            assert!((s.resilience.retry_backoff_ms - 15.0).abs() < 1e-9);
            let retries = |verb: &str| s.verbs.iter().find(|v| v.verb == verb).unwrap().retries;
            assert_eq!(retries("stat"), 3);
            assert_eq!(retries("read.full"), 1);
        })
        .await
        .unwrap();
    }

    #[test]
    fn a_retry_outside_any_operation_still_counts() {
        let (rec, _obs, notify) = linked();
        notify.retrying(&slow_down(), std::time::Duration::from_millis(3));
        let s = rec.snapshot();
        assert_eq!(s.resilience.retries, 1);
        assert_eq!(s.summary.ops, 0);
    }

    #[test]
    fn error_classes_keep_opendal_kinds() {
        let e = |k| Error::new(k, "x");
        assert_eq!(error_class(&e(ErrorKind::ConditionNotMatch), Some(412)), "condition_not_match");
        assert_eq!(error_class(&e(ErrorKind::AlreadyExists), Some(409)), "already_exists");
        assert_eq!(error_class(&e(ErrorKind::RangeNotSatisfied), Some(416)), "range_not_satisfied");
        assert_eq!(error_class(&e(ErrorKind::Unexpected), Some(503)), "rate_limited");
        assert_eq!(error_class(&e(ErrorKind::Unexpected), Some(500)), "server_error");
        assert_eq!(
            error_class(&e(ErrorKind::Unexpected).set_temporary(), None),
            "network"
        );
    }
}
