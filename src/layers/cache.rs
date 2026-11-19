//! In-memory byte-bounded LRU read cache layer, caching exact byte ranges and whole objects.
//! Lookups are timed and attributed to the current operation scope; writes/deletes invalidate.

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeBounds;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use opendal_core::raw::oio::{self, ReadStream as _};
use opendal_core::raw::*;
use opendal_core::*;

use crate::trace::OpScope;

fn note_lookup(us: u64, hit_bytes: Option<u64>) {
    if let Some(s) = OpScope::current() {
        s.with(|d| {
            d.cache_lookups += 1;
            d.cache_lookup_us += us;
            if let Some(b) = hit_bytes {
                d.cache_hits += 1;
                d.cache_hit_bytes += b;
            }
        });
    }
}

/// Byte-bounded LRU map. Entries weigh their length (min 64 bytes); inserting evicts the
/// least recently used entries until the total fits.
struct ByteLru {
    inner: Mutex<LruInner>,
}

struct LruInner {
    cap: u64,
    used: u64,
    tick: u64,
    map: HashMap<CacheEntryKey, (Buffer, u64, u64)>,
    order: BTreeMap<u64, CacheEntryKey>,
}

impl ByteLru {
    fn new(cap: u64) -> Self {
        Self {
            inner: Mutex::new(LruInner {
                cap,
                used: 0,
                tick: 0,
                map: HashMap::new(),
                order: BTreeMap::new(),
            }),
        }
    }

    fn get(&self, k: &CacheEntryKey) -> Option<Buffer> {
        let mut g = self.inner.lock().ok()?;
        let g = &mut *g;
        g.tick += 1;
        let tick = g.tick;
        let (v, t, _) = g.map.get_mut(k)?;
        let old = std::mem::replace(t, tick);
        let v = v.clone();
        if let Some(key) = g.order.remove(&old) {
            g.order.insert(tick, key);
        }
        Some(v)
    }

    fn insert(&self, k: CacheEntryKey, v: Buffer) {
        let w = (v.len() as u64).max(64);
        let Ok(mut g) = self.inner.lock() else { return };
        let g = &mut *g;
        if w > g.cap {
            return;
        }
        if let Some((_, t, ow)) = g.map.remove(&k) {
            g.order.remove(&t);
            g.used -= ow;
        }
        while g.used + w > g.cap {
            let Some((_, old)) = g.order.pop_first() else {
                break;
            };
            if let Some((_, _, ow)) = g.map.remove(&old) {
                g.used -= ow;
            }
        }
        g.tick += 1;
        let tick = g.tick;
        g.order.insert(tick, k.clone());
        g.map.insert(k, (v, tick, w));
        g.used += w;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheEntryKey {
    path: String,
    epoch: u64,
    offset: u64,
    size: Option<u64>,
}

struct SharedCacheState {
    cache: ByteLru,
    path_epochs: Mutex<HashMap<String, u64>>,
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

/// White-box OpenDAL `Layer` backed by a byte-bounded LRU cache.
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
    pub fn new(capacity_bytes: u64) -> Self {
        let cache = ByteLru::new(capacity_bytes.max(1024 * 1024));
        Self {
            state: Arc::new(SharedCacheState {
                cache,
                path_epochs: Mutex::new(HashMap::new()),
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
        if let Some(buf) = self.state.cache.get(&exact_key) {
            note_lookup(
                t_lookup.elapsed().as_micros() as u64,
                Some(buf.len() as u64),
            );
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
            if let Some(full_buf) = self.state.cache.get(&full_key) {
                let bounds = range.to_range_as_usize();
                let start = match bounds.start_bound() {
                    std::ops::Bound::Included(&s) => s,
                    std::ops::Bound::Excluded(&s) => s + 1,
                    std::ops::Bound::Unbounded => 0,
                };
                if start <= full_buf.len() {
                    let sliced = full_buf.slice(range.to_range_as_usize());
                    note_lookup(
                        t_lookup.elapsed().as_micros() as u64,
                        Some(sliced.len() as u64),
                    );
                    return Some(sliced);
                }
            }
        }

        note_lookup(t_lookup.elapsed().as_micros() as u64, None);
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
            let key = CacheEntryKey {
                path: self.path.clone(),
                epoch,
                offset: range.offset(),
                size: range.size(),
            };
            self.state.cache.insert(key, fetched.clone());
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
            let key = CacheEntryKey {
                path: self.path.clone(),
                epoch,
                offset: range.offset(),
                size: range.size(),
            };
            self.state.cache.insert(key, fetched.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(p: &str) -> CacheEntryKey {
        CacheEntryKey {
            path: p.into(),
            epoch: 0,
            offset: 0,
            size: None,
        }
    }

    #[test]
    fn lru_evicts_least_recently_used_by_bytes() {
        let c = ByteLru::new(300);
        c.insert(key("a"), Buffer::from(vec![0u8; 100]));
        c.insert(key("b"), Buffer::from(vec![0u8; 100]));
        c.insert(key("c"), Buffer::from(vec![0u8; 100]));
        assert!(c.get(&key("a")).is_some()); // a is now most recent
        c.insert(key("d"), Buffer::from(vec![0u8; 100]));
        assert!(c.get(&key("b")).is_none());
        assert!(c.get(&key("a")).is_some() && c.get(&key("c")).is_some());
        assert!(c.get(&key("d")).is_some());
        c.insert(key("huge"), Buffer::from(vec![0u8; 1000]));
        assert!(c.get(&key("huge")).is_none());
        c.insert(key("a"), Buffer::from(vec![0u8; 200]));
        assert_eq!(c.inner.lock().unwrap().used, 300);
    }
}
