//! Pure-Rust W-TinyLFU local hybrid/memory cache OpenDAL layer (`moka::future::Cache`),
//! supporting both full-object and sparse byte-range caching with exact microsecond lookup telemetry.

use std::collections::HashMap;
use std::ops::RangeBounds;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use await_tree::InstrumentAwait;
use moka::future::Cache;
use opendal_core::raw::oio::{self, ReadStream as _};
use opendal_core::raw::*;
use opendal_core::*;

use crate::telemetry::TelemetryRegistry;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheEntryKey {
    path: String,
    epoch: u64,
    offset: u64,
    size: Option<u64>,
}

struct SharedCacheState {
    cache: Cache<CacheEntryKey, Buffer>,
    path_epochs: Mutex<HashMap<String, u64>>,
    telemetry: Arc<TelemetryRegistry>,
}

impl SharedCacheState {
    fn current_epoch(&self, path: &str) -> u64 {
        self.path_epochs
            .lock()
            .ok()
            .and_then(|m| m.get(path).copied())
            .unwrap_or(0)
    }

    fn bump_epoch(&self, path: &str) {
        if let Ok(mut map) = self.path_epochs.lock() {
            let entry = map.entry(path.to_owned()).or_insert(0);
            *entry = entry.wrapping_add(1);
        }
    }
}

/// White-box OpenDAL `Layer` backed by `moka` W-TinyLFU concurrent byte-bounded cache.
#[derive(Clone)]
pub struct WhiteboxCacheLayer {
    state: Arc<SharedCacheState>,
}

impl std::fmt::Debug for WhiteboxCacheLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhiteboxCacheLayer").finish()
    }
}

impl WhiteboxCacheLayer {
    pub fn new(capacity_bytes: u64, telemetry: Arc<TelemetryRegistry>) -> Self {
        let cache = Cache::builder()
            .max_capacity(capacity_bytes.max(1024 * 1024))
            .weigher(|_k: &CacheEntryKey, v: &Buffer| v.len().clamp(64, u32::MAX as usize) as u32)
            .build();
        Self {
            state: Arc::new(SharedCacheState {
                cache,
                path_epochs: Mutex::new(HashMap::new()),
                telemetry,
            }),
        }
    }
}

impl Layer for WhiteboxCacheLayer {
    fn apply_service(&self, inner: Servicer) -> Servicer {
        Arc::new(WhiteboxCacheService {
            inner,
            state: self.state.clone(),
        })
    }
}

#[derive(Clone)]
struct WhiteboxCacheService {
    inner: Servicer,
    state: Arc<SharedCacheState>,
}

impl std::fmt::Debug for WhiteboxCacheService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhiteboxCacheService").finish()
    }
}

impl Service for WhiteboxCacheService {
    type Reader = CachedReader;
    type Writer = InvalidatingWriter<oio::Writer>;
    type Lister = oio::Lister;
    type Deleter = InvalidatingDeleter<oio::Deleter>;
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
        Ok(CachedReader {
            inner_srv: self.inner.clone(),
            ctx: ctx.clone(),
            path: path.to_owned(),
            args,
            state: self.state.clone(),
        })
    }

    fn write(&self, ctx: &OperationContext, path: &str, args: OpWrite) -> Result<Self::Writer> {
        let w = self.inner.write(ctx, path, args)?;
        Ok(InvalidatingWriter {
            inner: w,
            path: path.to_owned(),
            state: self.state.clone(),
        })
    }

    fn copy(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpCopy,
    ) -> Result<Self::Copier> {
        self.state.bump_epoch(to);
        self.inner.copy(ctx, from, to, args)
    }

    fn compose(&self, ctx: &OperationContext, to: &str, args: OpCompose) -> Result<Self::Composer> {
        self.state.bump_epoch(to);
        self.inner.compose(ctx, to, args)
    }

    async fn rename(
        &self,
        ctx: &OperationContext,
        from: &str,
        to: &str,
        args: OpRename,
    ) -> Result<RpRename> {
        self.state.bump_epoch(from);
        self.state.bump_epoch(to);
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
        let d = self.inner.delete(ctx)?;
        Ok(InvalidatingDeleter {
            inner: d,
            state: self.state.clone(),
        })
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

struct CachedReader {
    inner_srv: Servicer,
    ctx: OperationContext,
    path: String,
    args: OpRead,
    state: Arc<SharedCacheState>,
}

impl CachedReader {
    async fn lookup_cached_slice(&self, range: BytesRange) -> Option<Buffer> {
        let t_lookup = Instant::now();
        let epoch = self.state.current_epoch(&self.path);

        // 1. Check exact range key
        let exact_key = CacheEntryKey {
            path: self.path.clone(),
            epoch,
            offset: range.offset(),
            size: range.size(),
        };
        if let Some(buf) = self
            .state
            .cache
            .get(&exact_key)
            .instrument_await("layer::local_cache_lookup")
            .await
        {
            let us = t_lookup.elapsed().as_micros() as u64;
            self.state
                .telemetry
                .record_cache_lookup(us, true, buf.len() as u64);
            return Some(buf);
        }

        // 2. If reading a sub-range, check whether the full object `(0, None)` is already cached
        if range.offset() > 0 || range.size().is_some() {
            let full_key = CacheEntryKey {
                path: self.path.clone(),
                epoch,
                offset: 0,
                size: None,
            };
            if let Some(full_buf) = self.state.cache.get(&full_key).await {
                let bounds = range.to_range_as_usize();
                let start = match bounds.start_bound() {
                    std::ops::Bound::Included(&s) => s,
                    std::ops::Bound::Excluded(&s) => s + 1,
                    std::ops::Bound::Unbounded => 0,
                };
                if start <= full_buf.len() {
                    let sliced = full_buf.slice(range.to_range_as_usize());
                    let us = t_lookup.elapsed().as_micros() as u64;
                    self.state
                        .telemetry
                        .record_cache_lookup(us, true, sliced.len() as u64);
                    return Some(sliced);
                }
            }
        }

        let us = t_lookup.elapsed().as_micros() as u64;
        self.state.telemetry.record_cache_lookup(us, false, 0);
        None
    }
}

impl oio::Read for CachedReader {
    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        if let Some(hit_buf) = self.lookup_cached_slice(range).await {
            let rp = RpRead::new(MetadataBuilder::file(hit_buf.len() as u64).build());
            return Ok((rp, Box::new(hit_buf) as Box<dyn oio::ReadStreamDyn>));
        }

        let epoch = self.state.current_epoch(&self.path);
        let reader = self
            .inner_srv
            .read(&self.ctx, &self.path, self.args.clone())?;
        let (rp, mut stream) = reader.open(range).await?;
        let fetched = stream.read_all().await?;

        if fetched.len() <= 32 * 1024 * 1024 {
            let t_ins = Instant::now();
            let key = CacheEntryKey {
                path: self.path.clone(),
                epoch,
                offset: range.offset(),
                size: range.size(),
            };
            self.state.cache.insert(key, fetched.clone()).await;
            if self.state.telemetry.is_measuring() {
                self.state
                    .telemetry
                    .cache_lookup_us
                    .fetch_add(t_ins.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }

        Ok((rp, Box::new(fetched) as Box<dyn oio::ReadStreamDyn>))
    }

    async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
        if let Some(hit_buf) = self.lookup_cached_slice(range).await {
            let rp = RpRead::new(MetadataBuilder::file(hit_buf.len() as u64).build());
            return Ok((rp, hit_buf));
        }

        let epoch = self.state.current_epoch(&self.path);
        let reader = self
            .inner_srv
            .read(&self.ctx, &self.path, self.args.clone())?;
        let (rp, fetched) = reader.read(range).await?;

        if fetched.len() <= 32 * 1024 * 1024 {
            let t_ins = Instant::now();
            let key = CacheEntryKey {
                path: self.path.clone(),
                epoch,
                offset: range.offset(),
                size: range.size(),
            };
            self.state.cache.insert(key, fetched.clone()).await;
            if self.state.telemetry.is_measuring() {
                self.state
                    .telemetry
                    .cache_lookup_us
                    .fetch_add(t_ins.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }

        Ok((rp, fetched))
    }
}

struct InvalidatingWriter<W> {
    inner: W,
    path: String,
    state: Arc<SharedCacheState>,
}

impl<W: oio::Write> oio::Write for InvalidatingWriter<W> {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        self.inner.write(bs).await
    }

    async fn copy_from(&mut self, path: &str, args: OpRead, range: BytesRange) -> Result<()> {
        self.inner.copy_from(path, args, range).await
    }

    async fn close(&mut self) -> Result<Metadata> {
        let meta = self.inner.close().await?;
        self.state.bump_epoch(&self.path);
        Ok(meta)
    }

    async fn abort(&mut self) -> Result<()> {
        self.inner.abort().await
    }
}

struct InvalidatingDeleter<D> {
    inner: D,
    state: Arc<SharedCacheState>,
}

impl<D: oio::Delete> oio::Delete for InvalidatingDeleter<D> {
    async fn delete(&mut self, path: &str, args: OpDelete) -> Result<()> {
        self.state.bump_epoch(path);
        self.inner.delete(path, args).await
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }
}
