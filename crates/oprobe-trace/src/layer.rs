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
    let scope = OpScope::current();
    let total = started.elapsed().as_micros() as u64;
    let rec = RequestRecord {
        api,
        method: api.label().to_owned(),
        status: match err {
            None => 200,
            Some(e) if e.kind() == ErrorKind::NotFound => 404,
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
        let t = Instant::now();
        let r = self.inner.create_dir(ctx, path, args).await;
        push(&self.recorder, Api::PutObject, t, 0, 0, r.as_ref().err());
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
