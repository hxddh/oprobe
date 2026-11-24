//! OpenDAL layers that wire oprobe's tracing into an `Operator`:
//! - [`TraceContextLayer`]: installs the tracing HTTP transport and a scope-propagating executor;
//! - [`InProcessObserveLayer`]: records backend calls for `memory` / `fs` (no HTTP involved).

use std::sync::Arc;
use std::time::Instant;

use opendal_core::raw::oio::{self, ReadStream as _};
use opendal_core::raw::*;
use opendal_core::*;

use crate::{
    Api, OpScope, Protocol, Recorder, RequestRecord, RequestTiming, ScopePropagatingExecutor,
};

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
/// A read answered without issuing any backend request counts as a cache hit (`cache_hits`,
/// `cache_hit_bytes`, `cache_lookup_us`); a read that issues backend requests counts as a cache
/// miss (`cache_lookups` only) and groups the cache layer's internal `stat` + full-object fetch
/// into a single logical read operation so `overread_ratio` stays exact.
#[derive(Clone)]
pub struct CacheObserveLayer {
    recorder: Arc<Recorder>,
}

impl std::fmt::Debug for CacheObserveLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheObserveLayer")
    }
}

impl CacheObserveLayer {
    pub fn new(recorder: Arc<Recorder>) -> Self {
        Self { recorder }
    }
}

impl Layer for CacheObserveLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        self.recorder.set_cache_enabled(true);
        Arc::new(CacheObserveService {
            inner,
            recorder: self.recorder.clone(),
        })
    }
}

#[derive(Clone)]
struct CacheObserveService {
    inner: Servicer,
    recorder: Arc<Recorder>,
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
            recorder: self.recorder.clone(),
            path: path.to_owned(),
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
        self.inner.stat(ctx, path, args).await
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
    recorder: Arc<Recorder>,
    path: String,
}

impl oio::Read for CacheObserveReader {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        let verb = if range.is_full() {
            "read.full"
        } else {
            "read.range"
        };
        let t0 = Instant::now();
        if let Some(scope) = OpScope::current() {
            let before = scope.requests_recorded();
            let (rp, stream) = self.inner.open(range).await?;
            return Ok((
                rp,
                Box::new(CacheObserveStream {
                    inner: stream,
                    outer_scope: Some(scope),
                    owned_scope: None,
                    recorder: self.recorder.clone(),
                    verb,
                    key: String::new(),
                    t0,
                    before,
                    bytes: 0,
                    finished: false,
                }) as Box<dyn oio::ReadStreamDyn>,
            ));
        }
        let scope = OpScope::new_at(true, t0, Some(self.recorder.clone()));
        self.recorder.op_started_at(scope.id, t0);
        let key = crate::observe::private_key(&self.path);
        match scope.run(self.inner.open(range)).await {
            Ok((rp, stream)) => Ok((
                rp,
                Box::new(CacheObserveStream {
                    inner: stream,
                    outer_scope: None,
                    owned_scope: Some(scope),
                    recorder: self.recorder.clone(),
                    verb,
                    key,
                    t0,
                    before: 0,
                    bytes: 0,
                    finished: false,
                }) as Box<dyn oio::ReadStreamDyn>,
            )),
            Err(e) => {
                let data = scope.take();
                let last = data.requests.last().map(|r| r.status);
                let class = crate::observe::error_class(&e, last);
                self.recorder.complete_op(
                    true,
                    crate::OpCompletion {
                        verb,
                        op_id: scope.id,
                        started: t0,
                        latency_us: t0.elapsed().as_micros() as u64,
                        logical_read: 0,
                        logical_write: 0,
                        error: Some(class),
                        key,
                        data,
                        schedule_us: None,
                    },
                );
                Err(e)
            }
        }
    }

    async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
        let verb = if range.is_full() {
            "read.full"
        } else {
            "read.range"
        };
        let t0 = Instant::now();
        if let Some(scope) = OpScope::current() {
            let before = scope.requests_recorded();
            let r = self.inner.read(range).await;
            if let Ok((_, ref b)) = r {
                let hit = scope.requests_recorded() == before;
                let us = if hit {
                    t0.elapsed().as_micros() as u64
                } else {
                    0
                };
                scope.record_cache_lookup(hit, b.len() as u64, us);
            }
            return r;
        }
        let scope = OpScope::new_at(true, t0, Some(self.recorder.clone()));
        self.recorder.op_started_at(scope.id, t0);
        let r = scope.run(self.inner.read(range)).await;
        let latency_us = t0.elapsed().as_micros() as u64;
        let (n, err) = match &r {
            Ok((_, b)) => {
                let n = b.len() as u64;
                let hit = scope.requests_recorded() == 0;
                scope.record_cache_lookup(hit, n, if hit { latency_us } else { 0 });
                (n, None)
            }
            Err(e) => {
                let last = scope.with(|d| d.requests.last().map(|req| req.status));
                (0, Some(crate::observe::error_class(e, last)))
            }
        };
        let data = scope.take();
        self.recorder.complete_op(
            true,
            crate::OpCompletion {
                verb,
                op_id: scope.id,
                started: t0,
                latency_us,
                logical_read: n,
                logical_write: 0,
                error: err,
                key: crate::observe::private_key(&self.path),
                data,
                schedule_us: None,
            },
        );
        r
    }
}

pub struct CacheObserveStream {
    inner: Box<dyn oio::ReadStreamDyn>,
    outer_scope: Option<Arc<OpScope>>,
    owned_scope: Option<Arc<OpScope>>,
    recorder: Arc<Recorder>,
    verb: &'static str,
    key: String,
    t0: Instant,
    before: u64,
    bytes: u64,
    finished: bool,
}

impl CacheObserveStream {
    fn finish(&mut self, err: Option<String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let latency_us = self.t0.elapsed().as_micros() as u64;
        if let Some(scope) = self.outer_scope.take() {
            if err.is_none() {
                let hit = scope.requests_recorded() == self.before;
                scope.record_cache_lookup(hit, self.bytes, if hit { latency_us } else { 0 });
            }
            return;
        }
        if let Some(scope) = self.owned_scope.take() {
            if err.is_none() {
                let hit = scope.requests_recorded() == 0;
                scope.record_cache_lookup(hit, self.bytes, if hit { latency_us } else { 0 });
            }
            let data = scope.take();
            self.recorder.complete_op(
                true,
                crate::OpCompletion {
                    verb: self.verb,
                    op_id: scope.id,
                    started: self.t0,
                    latency_us,
                    logical_read: self.bytes,
                    logical_write: 0,
                    error: err,
                    key: std::mem::take(&mut self.key),
                    data,
                    schedule_us: None,
                },
            );
        }
    }
}

impl oio::ReadStream for CacheObserveStream {
    async fn read(&mut self) -> Result<Buffer> {
        let r = match &self.owned_scope {
            Some(s) => s.run(self.inner.read_dyn()).await,
            None => self.inner.read_dyn().await,
        };
        match &r {
            Ok(b) if b.is_empty() => self.finish(None),
            Ok(b) => self.bytes += b.len() as u64,
            Err(e) => {
                let last = self
                    .owned_scope
                    .as_ref()
                    .and_then(|s| s.with(|d| d.requests.last().map(|req| req.status)));
                self.finish(Some(crate::observe::error_class(e, last)));
            }
        }
        r
    }
}

impl Drop for CacheObserveStream {
    fn drop(&mut self) {
        self.finish(None);
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
