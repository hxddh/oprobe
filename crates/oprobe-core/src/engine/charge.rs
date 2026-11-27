//! The run's budgets and its stop, for services without an HTTP transport (`fs://`,
//! `memory://`).
//!
//! [`super::gate::Gate`] charges every request a run sends, but it sits in the HTTP
//! transport, and an in-process service has none. In 9.3.3 the measured window of such a run
//! was charged through its recorder, while the dataset upload and the preflight were charged
//! nowhere. `--max-requests 10` let 2 005 files be written, and `--max-bytes 1MiB` let 64 MiB
//! be written and then reported as 1 MiB.
//!
//! [`ChargeLayer`] goes where the gate would, on every operator of such a run, the measured one
//! included. Each call of the service is admitted by the gate before it runs: one request,
//! with the bytes it writes; the bytes a read returns are counted as they come, as a response
//! body is. A call of a stopped run, or one the budget has no room for, is refused before it
//! reaches the service. (9.3.3 charged the measured calls as they completed: an operation of
//! two calls could jump the count from 399 to 401, and the calls in flight finished past the
//! limit.) Aborting a write is cleanup, and is never charged or refused; the run's cleaner is
//! a separate operator that this layer is not on.

use opendal::raw::oio;
use opendal::raw::{
    Layer, OpCompose, OpCopy, OpCreateDir, OpList, OpPresign, OpRead, OpRename, OpRestore, OpStat,
    OpWrite, RpCreateDir, RpPresign, RpRead, RpRename, RpRestore, RpStat, Service, ServiceInfo,
    Servicer,
};
use opendal::{BytesRange, Capability, Metadata, OperationContext, Result};
use std::sync::Arc;

use super::gate::Gate;

/// Charges every call of the service beneath it to `gate`.
#[derive(Clone)]
pub struct ChargeLayer {
    gate: Gate,
}

impl std::fmt::Debug for ChargeLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChargeLayer")
    }
}

impl ChargeLayer {
    pub fn new(gate: Gate) -> Self {
        Self { gate }
    }
}

impl Layer for ChargeLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        Arc::new(ChargeService {
            inner,
            gate: self.gate.clone(),
        })
    }
}

struct ChargeService {
    inner: Servicer,
    gate: Gate,
}

impl std::fmt::Debug for ChargeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChargeService")
    }
}

impl Service for ChargeService {
    type Reader = ChargeReader;
    type Writer = ChargeWriter;
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
        self.gate.admit_call(0)?;
        self.inner.create_dir(ctx, path, args).await
    }

    async fn stat(&self, ctx: &OperationContext, path: &str, args: OpStat) -> Result<RpStat> {
        self.gate.admit_call(0)?;
        self.inner.stat(ctx, path, args).await
    }

    fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
        // Each read of the reader is a call, charged when it is made.
        Ok(ChargeReader {
            inner: self.inner.read(ctx, path, args)?,
            gate: self.gate.clone(),
        })
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        Ok(ChargeWriter {
            inner: self.inner.write(ctx, path, args)?,
            gate: self.gate.clone(),
        })
    }

    fn delete(&self, ctx: &OperationContext) -> Result<Self::Deleter> {
        self.gate.admit_call(0)?;
        self.inner.delete(ctx)
    }

    fn list(&self, ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        self.gate.admit_call(0)?;
        self.inner.list(ctx, path, args)
    }

    fn copy(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpCopy,
    ) -> Result<Self::Copier> {
        self.gate.admit_call(0)?;
        self.inner.copy(ctx, from, to, args)
    }

    fn compose(&self, ctx: &OperationContext, to: &str, args: OpCompose) -> Result<Self::Composer> {
        self.gate.admit_call(0)?;
        self.inner.compose(ctx, to, args)
    }

    async fn rename(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpRename,
    ) -> Result<RpRename> {
        self.gate.admit_call(0)?;
        self.inner.rename(ctx, from, to, args).await
    }

    async fn restore(
        &self,
        ctx: &OperationContext,
        path: &str,
        args: OpRestore,
    ) -> Result<RpRestore> {
        self.gate.admit_call(0)?;
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

/// A writer whose writes are charged, one request each with their bytes.
pub struct ChargeWriter {
    inner: oio::Writer,
    gate: Gate,
}

impl oio::Write for ChargeWriter {
    async fn write(&mut self, bs: opendal::Buffer) -> Result<()> {
        self.gate.admit_call(bs.len() as u64)?;
        oio::Write::write(&mut self.inner, bs).await
    }

    async fn copy_from(&mut self, path: &str, args: OpRead, range: BytesRange) -> Result<()> {
        self.gate.admit_call(0)?;
        oio::Write::copy_from(&mut self.inner, path, args, range).await
    }

    async fn close(&mut self) -> Result<Metadata> {
        oio::Write::close(&mut self.inner).await
    }

    /// Cleanup: never charged or refused.
    async fn abort(&mut self) -> Result<()> {
        oio::Write::abort(&mut self.inner).await
    }
}

/// A reader whose reads are charged: one request each, and the bytes they return.
pub struct ChargeReader {
    inner: oio::Reader,
    gate: Gate,
}

impl oio::Read for ChargeReader {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        self.gate.admit_call(0)?;
        let (rp, stream) = oio::ReadDyn::open_dyn(self.inner.as_ref(), range).await?;
        Ok((
            rp,
            Box::new(ChargeStream {
                inner: stream,
                gate: self.gate.clone(),
            }),
        ))
    }

    async fn read(&self, range: BytesRange) -> Result<(RpRead, opendal::Buffer)> {
        self.gate.admit_call(0)?;
        let (rp, buf) = oio::ReadDyn::read_dyn(self.inner.as_ref(), range).await?;
        self.gate.received_call(buf.len() as u64)?;
        Ok((rp, buf))
    }
}

/// The body of a charged read: its bytes are counted as they come.
struct ChargeStream {
    inner: Box<dyn oio::ReadStreamDyn>,
    gate: Gate,
}

impl oio::ReadStream for ChargeStream {
    async fn read(&mut self) -> Result<opendal::Buffer> {
        let buf = self.inner.read_dyn().await?;
        self.gate.received_call(buf.len() as u64)?;
        Ok(buf)
    }
}
