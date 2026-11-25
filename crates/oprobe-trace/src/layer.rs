//! OpenDAL layers that wire oprobe's tracing into an `Operator`:
//! - [`TraceContextLayer`]: installs the tracing HTTP transport and a scope-propagating executor;
//! - [`InProcessObserveLayer`]: records backend calls for `memory` / `fs` (no HTTP involved).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use opendal_core::raw::oio::{self, ReadStream as _};
use opendal_core::raw::*;
use opendal_core::*;

use crate::{
    Api, OpScope, Protocol, Recorder, RequestRecord, RequestTiming, ScopePropagatingExecutor,
};

/// Request extension recording when the request entered the outermost HTTP transport wrapper
/// (before `ConcurrentLimitLayer`'s HTTP semaphore).
#[derive(Clone, Copy, Debug)]
pub struct EnqueuedAt(pub Instant);

#[derive(Clone)]
struct EnqueuedAtTransport {
    inner: HttpTransporter,
}

impl HttpTransport for EnqueuedAtTransport {
    async fn fetch(&self, mut req: http::Request<Buffer>) -> Result<http::Response<HttpBody>> {
        if req.extensions().get::<EnqueuedAt>().is_none() {
            req.extensions_mut().insert(EnqueuedAt(Instant::now()));
        }
        self.inner.fetch(req).await
    }
}

/// Outermost context layer stamping the enqueue timestamp on every HTTP request before outer
/// transport semaphores (`ConcurrentLimitLayer::with_http_concurrent_limit`) acquire their permit.
#[derive(Clone, Default)]
pub struct QueueStampLayer;

impl std::fmt::Debug for QueueStampLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueueStampLayer")
    }
}

impl Layer for QueueStampLayer {
    fn apply_context(&self, _srv: Servicer, inner: OperationContext) -> OperationContext {
        let wrapped = HttpTransporter::new(EnqueuedAtTransport {
            inner: inner.http_transport().clone(),
        });
        inner.with_http_transport(wrapped)
    }
}

/// Installs `transport` as the HTTP transport and propagates [`OpScope`] into OpenDAL's
/// background tasks. Must be the *first* layer applied so other layers wrap it.
#[derive(Clone)]
pub struct TraceContextLayer {
    transport: HttpTransporter,
}

impl std::fmt::Debug for TraceContextLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TraceContextLayer")
    }
}

impl TraceContextLayer {
    pub fn new(transport: impl HttpTransport) -> Self {
        Self {
            transport: HttpTransporter::new(transport),
        }
    }
}

impl Layer for TraceContextLayer {
    fn apply_context(&self, _srv: Servicer, inner: OperationContext) -> OperationContext {
        let exec = ScopePropagatingExecutor::wrap(inner.executor().clone());
        inner
            .with_http_transport(self.transport.clone())
            .with_executor(exec)
    }
}

fn push(
    recorder: &Arc<Recorder>,
    api: Api,
    started: Instant,
    bytes_up: u64,
    bytes_down: u64,
    err: Option<&Error>,
) {
    let op = match api {
        Api::GetObject => "read",
        Api::HeadObject => "stat",
        Api::ListObjects => "list",
        Api::DeleteObject => "delete",
        Api::CopyObject => "copy",
        _ => "write",
    };
    push_op(recorder, op, api, started, bytes_up, bytes_down, err);
}

fn push_op(
    recorder: &Arc<Recorder>,
    operation: &'static str,
    api: Api,
    started: Instant,
    bytes_up: u64,
    bytes_down: u64,
    err: Option<&Error>,
) {
    let scope = OpScope::current();
    let total = started.elapsed().as_micros() as u64;
    let rec = RequestRecord {
        operation: Some(operation),
        api,
        method: api.label().to_owned(),
        status: match err.map(Error::kind) {
            None => 200,
            Some(ErrorKind::NotFound) => 404,
            Some(ErrorKind::PermissionDenied) => 403,
            Some(ErrorKind::RateLimited) => 429,
            Some(ErrorKind::ConditionNotMatch) => 412,
            Some(ErrorKind::AlreadyExists | ErrorKind::Conflict) => 409,
            Some(ErrorKind::RangeNotSatisfied) => 416,
            Some(_) => 500,
        },
        protocol: Protocol::InProcess,
        reused: true,
        conn_id: 0,
        start_offset_us: scope
            .as_ref()
            .map(|s| started.saturating_duration_since(s.started).as_micros() as u64)
            .unwrap_or(0),
        timing: RequestTiming {
            ttfb_us: total,
            total_us: total,
            ..Default::default()
        },
        bytes_up,
        bytes_down,
        request_id: None,
        retry_after_ms: None,
        error: err.map(|e| e.to_string()),
        remote: None,
        service_op: None,
        tls: None,
        tcp: None,
        // The backend's own error kind and temporary bit (design v7 B6).
        detail: err.map(|e| {
            Box::new(crate::rows::ReqDetail {
                error_code: Some(e.kind().into_static().to_owned()),
                temporary: Some(e.is_temporary()),
                ..Default::default()
            })
        }),
    };
    match scope {
        Some(s) => s.push_request(rec),
        None => recorder.record_unscoped_request(&rec),
    }
}

/// Bottom-most layer for in-process backends: one record per backend call.
#[derive(Clone)]
pub struct InProcessObserveLayer {
    recorder: Arc<Recorder>,
}

impl std::fmt::Debug for InProcessObserveLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InProcessObserveLayer")
    }
}

impl InProcessObserveLayer {
    pub fn new(recorder: Arc<Recorder>) -> Self {
        Self { recorder }
    }
}

impl Layer for InProcessObserveLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        Arc::new(ObserveService {
            inner,
            recorder: self.recorder.clone(),
        })
    }
}

#[derive(Clone)]
struct ObserveService {
    inner: Servicer,
    recorder: Arc<Recorder>,
}

impl std::fmt::Debug for ObserveService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObserveService")
    }
}

impl Service for ObserveService {
    type Reader = ObserveReader;
    type Writer = ObserveWriter;
    type Lister = ObserveLister;
    type Deleter = ObserveDeleter;
    type Copier = ObserveCopier;
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
        let t = Instant::now();
        let r = self.inner.create_dir(ctx, path, args).await;
        push_op(
            &self.recorder,
            "create_dir",
            Api::PutObject,
            t,
            0,
            0,
            r.as_ref().err(),
        );
        r
    }

    fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
        Ok(ObserveReader {
            inner: self.inner.read(ctx, path, args)?,
            recorder: self.recorder.clone(),
        })
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        Ok(ObserveWriter {
            inner: self.inner.write(ctx, path, args)?,
            recorder: self.recorder.clone(),
            started: None,
            bytes: 0,
        })
    }

    fn copy(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpCopy,
    ) -> Result<Self::Copier> {
        Ok(ObserveCopier {
            inner: self.inner.copy(ctx, from, to, args)?,
            recorder: self.recorder.clone(),
            started: None,
            bytes: 0,
            done: false,
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
        let t = Instant::now();
        let r = self.inner.rename(ctx, from, to, args).await;
        push_op(
            &self.recorder,
            "rename",
            Api::CopyObject,
            t,
            0,
            0,
            r.as_ref().err(),
        );
        r
    }

    async fn restore(
        &self,
        ctx: &OperationContext,
        path: &str,
        args: OpRestore,
    ) -> Result<RpRestore> {
        self.inner.restore(ctx, path, args).await
    }

    async fn stat(&self, ctx: &OperationContext, path: &str, args: OpStat) -> Result<RpStat> {
        let t = Instant::now();
        let r = self.inner.stat(ctx, path, args).await;
        push(&self.recorder, Api::HeadObject, t, 0, 0, r.as_ref().err());
        r
    }

    fn delete(&self, ctx: &OperationContext) -> Result<Self::Deleter> {
        Ok(ObserveDeleter {
            inner: self.inner.delete(ctx)?,
            recorder: self.recorder.clone(),
        })
    }

    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        Ok(ObserveLister {
            inner: self.inner.list(ctx, path, args)?,
            recorder: self.recorder.clone(),
            started: None,
            done: false,
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

pub struct ObserveReader {
    inner: oio::Reader,
    recorder: Arc<Recorder>,
}

impl oio::Read for ObserveReader {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        let t = Instant::now();
        let (rp, mut stream) = match self.inner.open(range).await {
            Ok(v) => v,
            Err(e) => {
                push(&self.recorder, Api::GetObject, t, 0, 0, Some(&e));
                return Err(e);
            }
        };
        // In-process streams are cheap: drain eagerly so the record covers the whole transfer.
        let res = stream.read_all().await;
        let n = res.as_ref().map(|b| b.len() as u64).unwrap_or(0);
        push(&self.recorder, Api::GetObject, t, 0, n, res.as_ref().err());
        Ok((rp, Box::new(res?) as Box<dyn oio::ReadStreamDyn>))
    }

    async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
        let t = Instant::now();
        let r = self.inner.read(range).await;
        let n = r.as_ref().map(|(_, b)| b.len() as u64).unwrap_or(0);
        push(&self.recorder, Api::GetObject, t, 0, n, r.as_ref().err());
        r
    }
}

pub struct ObserveWriter {
    inner: oio::Writer,
    recorder: Arc<Recorder>,
    started: Option<Instant>,
    bytes: u64,
}

impl oio::Write for ObserveWriter {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        self.started.get_or_insert_with(Instant::now);
        self.bytes += bs.len() as u64;
        self.inner.write(bs).await
    }

    async fn copy_from(&mut self, path: &str, args: OpRead, range: BytesRange) -> Result<()> {
        self.inner.copy_from(path, args, range).await
    }

    async fn close(&mut self) -> Result<Metadata> {
        let t = *self.started.get_or_insert_with(Instant::now);
        let r = self.inner.close().await;
        push(
            &self.recorder,
            Api::PutObject,
            t,
            self.bytes,
            0,
            r.as_ref().err(),
        );
        r
    }

    async fn abort(&mut self) -> Result<()> {
        self.inner.abort().await
    }
}

pub struct ObserveLister {
    inner: oio::Lister,
    recorder: Arc<Recorder>,
    started: Option<Instant>,
    done: bool,
}

impl oio::List for ObserveLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        let t = *self.started.get_or_insert_with(Instant::now);
        let r = self.inner.next().await;
        if !self.done && !matches!(r, Ok(Some(_))) {
            self.done = true;
            push(&self.recorder, Api::ListObjects, t, 0, 0, r.as_ref().err());
        }
        r
    }
}

pub struct ObserveDeleter {
    inner: oio::Deleter,
    recorder: Arc<Recorder>,
}

impl oio::Delete for ObserveDeleter {
    async fn delete(&mut self, path: &str, args: OpDelete) -> Result<()> {
        let t = Instant::now();
        let r = self.inner.delete(path, args).await;
        push(&self.recorder, Api::DeleteObject, t, 0, 0, r.as_ref().err());
        r
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }
}

pub struct ObserveCopier {
    inner: oio::Copier,
    recorder: Arc<Recorder>,
    started: Option<Instant>,
    bytes: u64,
    done: bool,
}

impl oio::Copy for ObserveCopier {
    async fn next(&mut self) -> Result<Option<usize>> {
        let t = *self.started.get_or_insert_with(Instant::now);
        let r = self.inner.next().await;
        match &r {
            Ok(Some(n)) => self.bytes += *n as u64,
            Ok(None) => {}
            Err(e) if !self.done => {
                self.done = true;
                push(&self.recorder, Api::CopyObject, t, self.bytes, 0, Some(e));
            }
            Err(_) => {}
        }
        r
    }

    async fn close(&mut self) -> Result<Metadata> {
        let t = *self.started.get_or_insert_with(Instant::now);
        let r = self.inner.close().await;
        if !self.done {
            self.done = true;
            push(
                &self.recorder,
                Api::CopyObject,
                t,
                self.bytes,
                0,
                r.as_ref().err(),
            );
        }
        r
    }

    async fn abort(&mut self) -> Result<()> {
        self.inner.abort().await
    }
}

/// Observes a local cache layer (`FoyerLayer`, `MokaLayer`, …) placed beneath it:
/// `op.layer(trace).layer(FoyerLayer::new(cache)).layer(handle.cache_layer())`.
///
/// A read or stat answered without issuing any backend request counts as a cache hit
/// (`cache_hits`, `cache_hit_bytes`, `cache_lookup_us`); one that issues backend requests
/// counts as a cache miss (`cache_lookups` only) and groups the cache layer's internal `stat`
/// + full-object fetch into a single logical read operation so `overread_ratio` stays exact.
#[derive(Clone)]
pub struct CacheObserveLayer {
    recorder: Arc<Recorder>,
    linker: Arc<crate::observe::Linker>,
}

impl std::fmt::Debug for CacheObserveLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheObserveLayer")
    }
}

impl CacheObserveLayer {
    pub fn new(recorder: Arc<Recorder>) -> Self {
        Self {
            recorder,
            linker: Arc::new(crate::observe::Linker::default()),
        }
    }

    pub(crate) fn with_linker(
        recorder: Arc<Recorder>,
        linker: Arc<crate::observe::Linker>,
    ) -> Self {
        Self { recorder, linker }
    }
}

impl Layer for CacheObserveLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        self.recorder.set_cache_enabled(true);
        let probe_srv: Servicer = Arc::new(CacheObserveService { inner });
        // Wrap with OperationObserveLayer so standalone embedders get full ReadGroup coalescing
        // and Linker retry attribution, while calls already inside an outer `OpScope` pass
        // straight through without nesting.
        crate::observe::OperationObserveLayer::new(self.recorder.clone(), self.linker.clone())
            .apply_service(probe_srv)
    }

    fn apply_context(&self, srv: Servicer, inner: OperationContext) -> OperationContext {
        QueueStampLayer.apply_context(srv, inner)
    }
}

#[derive(Clone)]
struct CacheObserveService {
    inner: Servicer,
}

impl std::fmt::Debug for CacheObserveService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheObserveService")
    }
}

impl Service for CacheObserveService {
    type Reader = CacheObserveReader;
    type Writer = oio::Writer;
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

    fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
        Ok(CacheObserveReader {
            inner: self.inner.read(ctx, path, args)?,
        })
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        self.inner.write(ctx, path, args)
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

    async fn stat(&self, ctx: &OperationContext, path: &str, args: OpStat) -> Result<RpStat> {
        let Some(scope) = OpScope::current() else {
            return self.inner.stat(ctx, path, args).await;
        };
        let t0 = Instant::now();
        let probe = Arc::new(AtomicU64::new(0));
        let r = crate::scope::with_cache_probe(probe.clone(), self.inner.stat(ctx, path, args))
            .await;
        if r.is_ok() {
            let hit = probe.load(Ordering::Acquire) == 0;
            let us = if hit {
                t0.elapsed().as_micros() as u64
            } else {
                0
            };
            scope.record_cache_lookup(hit, 0, us);
        }
        r
    }

    fn delete(&self, ctx: &OperationContext) -> Result<Self::Deleter> {
        self.inner.delete(ctx)
    }

    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        self.inner.list(ctx, path, args)
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

pub struct CacheObserveReader {
    inner: oio::Reader,
}

impl oio::Read for CacheObserveReader {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        let Some(scope) = OpScope::current() else {
            return self.inner.open(range).await;
        };
        let t0 = Instant::now();
        let probe = Arc::new(AtomicU64::new(0));
        let (rp, stream) =
            crate::scope::with_cache_probe(probe.clone(), self.inner.open(range)).await?;
        Ok((
            rp,
            Box::new(CacheObserveStream {
                inner: Some(stream),
                scope,
                probe,
                t0,
                bytes: 0,
                finished: false,
            }) as Box<dyn oio::ReadStreamDyn>,
        ))
    }

    async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
        let Some(scope) = OpScope::current() else {
            return self.inner.read(range).await;
        };
        let t0 = Instant::now();
        let probe = Arc::new(AtomicU64::new(0));
        let r = crate::scope::with_cache_probe(probe.clone(), self.inner.read(range)).await;
        if let Ok((_, ref b)) = r {
            let hit = probe.load(Ordering::Acquire) == 0;
            let us = if hit {
                t0.elapsed().as_micros() as u64
            } else {
                0
            };
            scope.record_cache_lookup(hit, b.len() as u64, us);
        }
        r
    }
}

pub struct CacheObserveStream {
    inner: Option<Box<dyn oio::ReadStreamDyn>>,
    scope: Arc<OpScope>,
    probe: Arc<AtomicU64>,
    t0: Instant,
    bytes: u64,
    finished: bool,
}

impl CacheObserveStream {
    fn finish(&mut self, ok: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        if ok {
            let hit = self.probe.load(Ordering::Acquire) == 0;
            let us = if hit {
                self.t0.elapsed().as_micros() as u64
            } else {
                0
            };
            self.scope.record_cache_lookup(hit, self.bytes, us);
        }
    }
}

impl oio::ReadStream for CacheObserveStream {
    async fn read(&mut self) -> Result<Buffer> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Buffer::new());
        };
        let r = crate::scope::with_cache_probe(self.probe.clone(), inner.read_dyn()).await;
        match &r {
            Ok(b) if b.is_empty() => {
                drop(self.inner.take());
                self.finish(true);
            }
            Ok(b) => self.bytes += b.len() as u64,
            Err(_) => {
                drop(self.inner.take());
                self.finish(false);
            }
        }
        r
    }
}

impl Drop for CacheObserveStream {
    fn drop(&mut self) {
        // Drop `self.inner` inside `with_cache_probe_sync` BEFORE inspecting `self.probe` so any
        // aborted `TimedResponseBody` emits its backend request first instead of misclassifying a
        // cancelled miss as a hit.
        if let Some(inner) = self.inner.take() {
            crate::scope::with_cache_probe_sync(self.probe.clone(), || drop(inner));
        }
        self.finish(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::FailureCode;

    /// An in-process backend's failure keeps OpenDAL's kind and temporary bit (design v7 B6).
    #[test]
    fn in_process_failures_keep_their_kind() {
        let rec = Recorder::always_on(100);
        let t = Instant::now();
        let exists = Error::new(ErrorKind::AlreadyExists, "exists");
        push(&rec, Api::PutObject, t, 1, 0, Some(&exists));
        let busy = Error::new(ErrorKind::RateLimited, "busy").set_temporary();
        push(&rec, Api::GetObject, t, 0, 0, Some(&busy));
        let range = Error::new(ErrorKind::RangeNotSatisfied, "range");
        push(&rec, Api::GetObject, t, 0, 0, Some(&range));
        let rows = rec.take_trace();
        let got: Vec<(Option<FailureCode>, Option<bool>)> = rows
            .reqs
            .iter()
            .map(|r| (r.error_code, r.detail.as_ref().and_then(|d| d.temporary)))
            .collect();
        assert_eq!(got.len(), 3);
        for want in [
            (Some(FailureCode::StorageAlreadyExists), Some(false)),
            (Some(FailureCode::StorageThrottled), Some(true)),
            (Some(FailureCode::StorageRangeNotSatisfied), Some(false)),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
    }
}
