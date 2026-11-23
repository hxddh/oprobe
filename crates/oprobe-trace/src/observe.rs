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
//! the retry notification marks it retrying and records the backoff; the next attempt on the same
//! task (or, for a future driven by `block_on`, the same thread) resumes it. A parked call that is not retried completes as failed (at the next call on
//! its task, or when the handle takes a snapshot). Without the notifier every attempt is an
//! operation of its own.

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
    parked: Mutex<HashMap<Key, Parked>>,
    /// Multi-call operations (writers, listers, …) whose last call failed temporarily: the
    /// scope a retry notification on that task belongs to.
    scopes: Mutex<HashMap<Key, Arc<OpScope>>>,
}

impl std::fmt::Debug for Linker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Linker")
    }
}

impl Linker {
    fn parked(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Parked>> {
        self.parked.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scopes(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Arc<OpScope>>> {
        self.scopes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Complete every parked call that is not being retried, or whose retry never came
    /// (before a snapshot).
    pub fn flush(&self, rec: &Recorder) {
        if !self.enabled() {
            return;
        }
        let done: Vec<Parked> = {
            let mut p = self.parked();
            let ids: Vec<_> = p
                .iter()
                .filter(|(_, x)| !x.retrying || x.ended.elapsed() > STALE_RETRY)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter().filter_map(|id| p.remove(&id)).collect()
        };
        for x in done {
            complete(rec, x.op, x.ended, Some(x.error));
        }
    }
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
        // The trace layer above the retry layer: the operation is the current scope.
        if let Some(s) = OpScope::current() {
            s.record_retry(backoff, throttled);
            return;
        }
        let id = key();
        if let Some(p) = self.linker.parked().get_mut(&id) {
            p.retrying = true;
            p.op.scope.record_retry(backoff, throttled);
            return;
        }
        if let Some(s) = self.linker.scopes().remove(&id) {
            s.record_retry(backoff, throttled);
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
    let verb = match st.verb {
        "write" if data.requests.iter().any(|r| r.api == Api::UploadPart) => "write.multipart",
        v => v,
    };
    rec.complete_op(
        true,
        OpCompletion {
            verb,
            op_id: st.scope.id,
            started: st.started,
            latency_us: end.saturating_duration_since(st.started).as_micros() as u64,
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
            let parked = self.linker.parked().remove(&key());
            if let Some(p) = parked {
                // The retry of the same call (the retry layer calls again with the same verb
                // and path); anything else means the parked call was not retried.
                if p.retrying && p.op.verb == verb && p.op.key == key_of_path {
                    return Some(p.op);
                }
                complete(&self.recorder, p.op, p.ended, Some(p.error));
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
        let end = Instant::now();
        let Some(e) = err else {
            complete(&self.recorder, st, end, None);
            return;
        };
        let class = class_of(&st.scope, e);
        if e.is_temporary() && self.linker.enabled() {
            let id = key();
            let evicted = {
                let mut p = self.linker.parked();
                let evicted = if p.len() >= MAX_PARKED {
                    let oldest = p
                        .iter()
                        .filter(|(_, x)| !x.retrying)
                        .min_by_key(|(_, x)| x.ended)
                        .map(|(id, _)| *id);
                    oldest.and_then(|id| p.remove(&id))
                } else {
                    None
                };
                let prev = p.insert(
                    id,
                    Parked {
                        op: st,
                        ended: end,
                        error: class,
                        retrying: false,
                    },
                );
                (evicted, prev)
            };
            for x in [evicted.0, evicted.1].into_iter().flatten() {
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
        })
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        let st = self.obs.start("write", path);
        let inner = match self.inner.write(ctx, path, args) {
            Ok(i) => i,
            Err(e) => return Err(self.obs.failed_start(st, e)),
        };
        Ok(OpWriter {
            inner,
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
        let st = self.obs.start("copy", to);
        let inner = match self.inner.copy(ctx, from, to, args) {
            Ok(i) => i,
            Err(e) => return Err(self.obs.failed_start(st, e)),
        };
        Ok(OpCopier {
            inner,
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
            inner: self.inner.delete(ctx)?,
            obs: self.obs.clone(),
            st: None,
            started: false,
            error: None,
        })
    }

    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        let st = self.obs.start("list", path);
        let inner = match self.inner.list(ctx, path, args) {
            Ok(i) => i,
            Err(e) => return Err(self.obs.failed_start(st, e)),
        };
        Ok(OpLister {
            inner,
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
        let mut g = OpGuard::new(&self.obs, read_verb(&range), &self.path);
        match in_scope(&g.st, self.inner.open(range)).await {
            Ok((rp, stream)) => {
                let st = g.st.take();
                Ok((
                    rp,
                    Box::new(OpStream {
                        inner: stream,
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
        let mut g = OpGuard::new(&self.obs, read_verb(&range), &self.path);
        let r = in_scope(&g.st, self.inner.read(range)).await;
        if let (Ok((_, b)), Some(st)) = (&r, g.st.as_mut()) {
            st.read += b.len() as u64;
        }
        g.finish(r.as_ref().err());
        r
    }
}

/// The body of an opened read: the operation ends with the stream (its end, an error, or the
/// stream dropped part-way).
pub struct OpStream {
    inner: Box<dyn oio::ReadStreamDyn>,
    obs: Observer,
    st: Option<OpState>,
}

impl oio::ReadStream for OpStream {
    async fn read(&mut self) -> Result<Buffer> {
        // The body of a request already linked to the operation: no scope needed.
        let r = self.inner.read_dyn().await;
        match &r {
            Ok(b) if b.is_empty() => {
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
        // Dropped before its end: the application stopped reading; what it read counts.
        if let Some(st) = self.st.take() {
            complete(&self.obs.recorder, st, Instant::now(), None);
        }
    }
}

pub struct OpWriter {
    inner: oio::Writer,
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
        let r = in_scope(&self.st, self.inner.write(bs)).await;
        if let (Ok(()), Some(st)) = (&r, self.st.as_mut()) {
            st.written += n;
        }
        self.note(&r);
        r
    }

    async fn copy_from(&mut self, path: &str, args: OpRead, range: BytesRange) -> Result<()> {
        let n = range.size().unwrap_or(0);
        let r = in_scope(&self.st, self.inner.copy_from(path, args, range)).await;
        if let (Ok(()), Some(st)) = (&r, self.st.as_mut()) {
            st.written += n;
        }
        self.note(&r);
        r
    }

    async fn close(&mut self) -> Result<Metadata> {
        let r = in_scope(&self.st, self.inner.close()).await;
        self.note(&r);
        if r.is_ok() {
            self.closed = true;
            end_multi(&self.obs, &mut self.st, None);
        }
        r
    }

    async fn abort(&mut self) -> Result<()> {
        let r = in_scope(&self.st, self.inner.abort()).await;
        self.closed = true;
        let error = self.error.take().or_else(|| Some("aborted".into()));
        end_multi(&self.obs, &mut self.st, error);
        r
    }
}

impl Drop for OpWriter {
    fn drop(&mut self) {
        let error = self.error.take().or_else(|| Some("cancelled".into()));
        end_multi(&self.obs, &mut self.st, error.filter(|_| !self.closed));
    }
}

pub struct OpLister {
    inner: oio::Lister,
    obs: Observer,
    st: Option<OpState>,
    error: Option<String>,
}

impl oio::List for OpLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        let r = in_scope(&self.st, self.inner.next()).await;
        match (&r, &self.st) {
            (Ok(Some(_)), _) => self.error = None,
            (Ok(None), _) => {
                self.error = None;
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
        let error = self.error.take();
        end_multi(&self.obs, &mut self.st, error);
    }
}

/// A batch delete: one operation from the first path queued until the deleter is closed.
pub struct OpDeleter {
    inner: oio::Deleter,
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
            self.st = self.obs.start("delete", path);
        }
        let r = in_scope(&self.st, self.inner.delete(path, args)).await;
        self.note(&r);
        r
    }

    async fn close(&mut self) -> Result<()> {
        let r = in_scope(&self.st, self.inner.close()).await;
        self.note(&r);
        if r.is_ok() {
            end_multi(&self.obs, &mut self.st, None);
        }
        r
    }
}

impl Drop for OpDeleter {
    fn drop(&mut self) {
        let error = self.error.take();
        end_multi(&self.obs, &mut self.st, error);
    }
}

pub struct OpCopier {
    inner: oio::Copier,
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
        let r = in_scope(&self.st, self.inner.next()).await;
        if let (Ok(Some(n)), Some(st)) = (&r, self.st.as_mut()) {
            st.written += *n as u64;
        }
        self.note(&r);
        r
    }

    async fn close(&mut self) -> Result<Metadata> {
        let r = in_scope(&self.st, self.inner.close()).await;
        self.note(&r);
        if r.is_ok() {
            end_multi(&self.obs, &mut self.st, None);
        }
        r
    }

    async fn abort(&mut self) -> Result<()> {
        let r = in_scope(&self.st, self.inner.abort()).await;
        let error = self.error.take().or_else(|| Some("aborted".into()));
        end_multi(&self.obs, &mut self.st, error);
        r
    }
}

impl Drop for OpCopier {
    fn drop(&mut self) {
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
        // completes it as failed and starts afresh.
        let st = obs.start("stat", "a/b").unwrap();
        obs.finish(st, Some(&slow_down()));
        notify.retrying(&slow_down(), std::time::Duration::from_millis(1));
        let st = obs.start("stat", "a/other").unwrap();
        obs.finish(st, None);
        let s = rec.snapshot();
        assert_eq!(s.summary.ops, 4);
        assert_eq!(s.summary.failed_ops, 3, "the exhausted retry, the not-found and the abandoned call");
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
