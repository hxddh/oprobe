//! Low-level OpenDAL `Service` white-box observer (`WhiteboxObserveLayer`) placed directly
//! above the raw storage backend (below cache & concurrency layers) to measure:
//! - Exact `oio::Write::close()` Multipart merge/commit latency;
//! - Real backend RPC & read-byte flow for non-HTTP backends (`memory`, `fs`).

use std::sync::Arc;
use std::time::Instant;

use await_tree::InstrumentAwait;
use opendal_core::raw::*;
use opendal_core::*;

use crate::telemetry::TelemetryRegistry;

/// Innermost observation layer wrapping the storage service.
#[derive(Clone)]
pub struct WhiteboxObserveLayer {
    telemetry: Arc<TelemetryRegistry>,
    is_non_http_backend: bool,
}

impl std::fmt::Debug for WhiteboxObserveLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhiteboxObserveLayer")
            .field("is_non_http_backend", &self.is_non_http_backend)
            .finish()
    }
}

impl WhiteboxObserveLayer {
    pub fn new(telemetry: Arc<TelemetryRegistry>, scheme: &str) -> Self {
        let is_non_http_backend = matches!(scheme, "memory" | "fs");
        Self {
            telemetry,
            is_non_http_backend,
        }
    }
}

impl Layer for WhiteboxObserveLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        Arc::new(ObserveService {
            inner,
            telemetry: self.telemetry.clone(),
            is_non_http_backend: self.is_non_http_backend,
        })
    }
}

#[derive(Clone)]
struct ObserveService {
    inner: Servicer,
    telemetry: Arc<TelemetryRegistry>,
    is_non_http_backend: bool,
}

impl std::fmt::Debug for ObserveService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObserveService").finish()
    }
}

impl Service for ObserveService {
    type Reader = ObserveReader<oio::Reader>;
    type Writer = ObserveWriter<oio::Writer>;
    type Lister = ObserveLister<oio::Lister>;
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
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(true);
        }
        self.inner.create_dir(ctx, path, args).await
    }

    fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
        let r = self.inner.read(ctx, path, args)?;
        Ok(ObserveReader {
            inner: r,
            telemetry: self.telemetry.clone(),
            is_non_http_backend: self.is_non_http_backend,
        })
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        let w = self.inner.write(ctx, path, args)?;
        Ok(ObserveWriter {
            inner: w,
            telemetry: self.telemetry.clone(),
            is_non_http_backend: self.is_non_http_backend,
            write_calls: 0,
        })
    }

    fn copy(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpCopy,
    ) -> Result<Self::Copier> {
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(true);
        }
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
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(true);
        }
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
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(false);
        }
        self.inner.stat(ctx, path, args).await
    }

    fn delete(&self, ctx: &OperationContext) -> Result<Self::Deleter> {
        self.inner.delete(ctx)
    }

    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(true);
        }
        let l = self.inner.list(ctx, path, args)?;
        Ok(ObserveLister { inner: l })
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

struct ObserveReader<R> {
    inner: R,
    telemetry: Arc<TelemetryRegistry>,
    is_non_http_backend: bool,
}

impl<R: oio::Read> oio::Read for ObserveReader<R> {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(false);
        }
        let (rp, stream) = self.inner.open(range).await?;
        Ok((
            rp,
            Box::new(ObserveReadStream {
                inner: stream,
                telemetry: self.telemetry.clone(),
                is_non_http_backend: self.is_non_http_backend,
            }) as Box<dyn oio::ReadStreamDyn>,
        ))
    }

    async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(false);
        }
        let (rp, buf) = self.inner.read(range).await?;
        if self.is_non_http_backend {
            self.telemetry.record_storage_read_bytes(buf.len() as u64);
        }
        Ok((rp, buf))
    }
}

struct ObserveReadStream {
    inner: Box<dyn oio::ReadStreamDyn>,
    telemetry: Arc<TelemetryRegistry>,
    is_non_http_backend: bool,
}

impl oio::ReadStream for ObserveReadStream {
    async fn read(&mut self) -> Result<Buffer> {
        let buf = self.inner.read_dyn().await?;
        if self.is_non_http_backend && !buf.is_empty() {
            self.telemetry.record_storage_read_bytes(buf.len() as u64);
        }
        Ok(buf)
    }
}

struct ObserveWriter<W> {
    inner: W,
    telemetry: Arc<TelemetryRegistry>,
    is_non_http_backend: bool,
    write_calls: u64,
}

impl<W: oio::Write> oio::Write for ObserveWriter<W> {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        self.write_calls += 1;
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(true);
        }
        self.inner.write(bs).await
    }

    async fn copy_from(&mut self, path: &str, args: OpRead, range: BytesRange) -> Result<()> {
        self.write_calls += 1;
        if self.is_non_http_backend {
            self.telemetry.record_backend_rpc(true);
        }
        self.inner.copy_from(path, args, range).await
    }

    async fn close(&mut self) -> Result<Metadata> {
        if self.is_non_http_backend && self.write_calls > 1 {
            self.telemetry.record_backend_rpc(true);
        }
        let t_close = Instant::now();
        let meta = self
            .inner
            .close()
            .instrument_await("storage::multipart_complete_close")
            .await?;
        self.telemetry
            .record_multipart_merge(t_close.elapsed().as_micros() as u64);
        Ok(meta)
    }

    async fn abort(&mut self) -> Result<()> {
        self.inner.abort().await
    }
}

struct ObserveLister<L> {
    inner: L,
}

impl<L: oio::List> oio::List for ObserveLister<L> {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        self.inner.next().await
    }
}
