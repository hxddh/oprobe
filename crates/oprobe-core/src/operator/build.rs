//! Building the measured operator from an [`OperatorSpec`]: the service (through oprobe's
//! instrumented `HttpFetch`) and the spec's layer stack, applied innermost first in the one
//! order oprobe stacks layers in ([`super::diff::LAYER_ORDER`]).

use std::time::Duration;

use anyhow::{Context, Result};
use opendal::Operator;
use opendal::layers::{
    ConcurrentLimitLayer, FoyerKey, FoyerLayer, FoyerValue, RetryLayer, TailCutLayer,
    ThrottleLayer, TimeoutLayer,
};

use super::diff::{LAYER_ORDER, UNLIMITED_OPERATIONS};
use super::*;
use crate::config::TargetSpec;
use crate::trace::OpScope;

impl OperatorSpec {
    /// The service target of this spec (URL and options; not yet normalized).
    pub fn target_spec(&self) -> TargetSpec {
        TargetSpec {
            url: Some(self.url.clone()),
            scheme: String::new(),
            options: self.options.clone(),
            insecure: self.http.insecure,
            ca_cert: self.http.ca_cert.clone(),
            proxy: self.http.proxy.clone(),
        }
    }

    /// The service target, normalized (the URL's options merged, required options checked).
    pub fn target(&self) -> Result<TargetSpec> {
        self.target_spec().normalized()
    }
}

/// A layer parameter's value, as passed to the OpenDAL builder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg {
    Count(usize),
    F32(f32),
    F64(f64),
    /// A builder switch without an argument (`with_jitter()`).
    Switch(bool),
    Time(Duration),
    Bytes(u64),
}

/// OpenDAL's own value for a parameter when its builder method is not called.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpendalDefault {
    /// A constructor argument: always spelled out.
    Ctor,
    /// Not applied unless the method is called.
    Unset,
    /// This value.
    Value(Arg),
}

/// One applied layer parameter: the builder method that sets it, the value oprobe applies and
/// OpenDAL's default for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Param {
    pub method: &'static str,
    pub value: Arg,
    pub opendal_default: OpendalDefault,
}

impl Param {
    fn ctor(method: &'static str, value: Arg) -> Self {
        Self {
            method,
            value,
            opendal_default: OpendalDefault::Ctor,
        }
    }

    fn with(method: &'static str, value: Arg, default: Arg) -> Self {
        Self {
            method,
            value,
            opendal_default: OpendalDefault::Value(default),
        }
    }

    /// Whether generated code must call the method for the layer to behave as measured.
    pub fn spelled(&self) -> bool {
        match self.opendal_default {
            OpendalDefault::Ctor | OpendalDefault::Unset => true,
            OpendalDefault::Value(d) => d != self.value,
        }
    }
}

/// Every parameter oprobe applies to layer `name` of `s` (`None`: not in the stack). Both the
/// measured operator ([`apply_layers`]) and the generated code ([`super::diff`]) are made from
/// this list, so what is measured is what is emitted.
///
/// The OpenDAL defaults are those of OpenDAL 0.59.5.
pub fn layer_params(s: &OperatorSpec, name: &str) -> Option<Vec<Param>> {
    use Arg::*;
    let l = &s.layers;
    let secs = |n| Time(Duration::from_secs(n));
    Some(match name {
        "throttle" => {
            let t = l.throttle.as_ref()?;
            vec![
                Param::ctor("bandwidth", Bytes(t.bandwidth.0.min(u64::from(u32::MAX)))),
                Param::ctor("burst", Bytes(t.burst.0.min(u64::from(u32::MAX)))),
            ]
        }
        "concurrent_limit" => {
            let c = l.concurrent_limit.as_ref()?;
            let mut v = vec![Param::ctor(
                "permits",
                Count(c.operations.unwrap_or(UNLIMITED_OPERATIONS)),
            )];
            if let Some(h) = c.http {
                v.push(Param {
                    method: "with_http_concurrent_limit",
                    value: Count(h),
                    opendal_default: OpendalDefault::Unset,
                });
            }
            v
        }
        "timeout" => {
            let t = l.timeout.as_ref()?;
            vec![
                Param::with("with_timeout", Time(t.op.get()), secs(60)),
                Param::with("with_io_timeout", Time(t.io.get()), secs(10)),
            ]
        }
        "tail_cut" => {
            let p = l.tail_cut.as_ref()?.params(s.call_timeout());
            vec![
                Param::with("percentile", Count(usize::from(p.percentile)), Count(95)),
                Param::with("safety_factor", F64(p.safety_factor), F64(1.3)),
                Param::with("min_samples", Count(p.min_samples), Count(200)),
                Param::with(
                    "min_deadline",
                    Time(p.min_deadline),
                    Time(Duration::from_millis(500)),
                ),
                Param::with("max_deadline", Time(p.max_deadline), secs(30)),
                Param::with("window", Time(p.window), secs(60)),
            ]
        }
        "retry" => {
            let r = l.retry.as_ref()?;
            vec![
                Param::with("with_jitter", Switch(r.jitter), Switch(false)),
                Param::with("with_max_times", Count(r.max_times), Count(3)),
                Param::with("with_min_delay", Time(r.min_delay.get()), secs(1)),
                Param::with("with_max_delay", Time(r.max_delay.get()), secs(60)),
                Param::with("with_factor", F32(r.factor), F32(2.0)),
            ]
        }
        "cache" => {
            let c = l.cache.as_ref()?;
            vec![Param::ctor("memory", Bytes(c.memory.0))]
        }
        _ => return None,
    })
}

fn count(p: &[Param], method: &str) -> Option<usize> {
    p.iter()
        .find(|x| x.method == method)
        .and_then(|x| match x.value {
            Arg::Count(n) => Some(n),
            _ => None,
        })
}

fn bytes(p: &[Param], method: &str) -> u64 {
    p.iter()
        .find(|x| x.method == method)
        .and_then(|x| match x.value {
            Arg::Bytes(n) => Some(n),
            _ => None,
        })
        .unwrap_or_default()
}

/// `TailCutLayer` from its applied parameters.
pub fn tail_cut_layer(params: &[Param]) -> TailCutLayer {
    let mut b = TailCutLayer::builder();
    for p in params {
        b = match (p.method, p.value) {
            ("percentile", Arg::Count(n)) => b.percentile(n as u8),
            ("safety_factor", Arg::F64(f)) => b.safety_factor(f),
            ("min_samples", Arg::Count(n)) => b.min_samples(n),
            ("min_deadline", Arg::Time(d)) => b.min_deadline(d),
            ("max_deadline", Arg::Time(d)) => b.max_deadline(d),
            ("window", Arg::Time(d)) => b.window(d),
            other => unreachable!("tail_cut parameter {other:?}"),
        };
    }
    b.build()
}

/// `RetryLayer` from its applied parameters.
fn retry_layer(params: &[Param]) -> RetryLayer {
    let mut l = RetryLayer::default();
    for p in params {
        l = match (p.method, p.value) {
            ("with_jitter", Arg::Switch(true)) => l.with_jitter(),
            ("with_jitter", Arg::Switch(false)) => l,
            ("with_max_times", Arg::Count(n)) => l.with_max_times(n),
            ("with_min_delay", Arg::Time(d)) => l.with_min_delay(d),
            ("with_max_delay", Arg::Time(d)) => l.with_max_delay(d),
            ("with_factor", Arg::F32(f)) => l.with_factor(f),
            other => unreachable!("retry parameter {other:?}"),
        };
    }
    l
}

/// `TimeoutLayer` from its applied parameters.
fn timeout_layer(params: &[Param]) -> TimeoutLayer {
    let mut l = TimeoutLayer::new();
    for p in params {
        l = match (p.method, p.value) {
            ("with_timeout", Arg::Time(d)) => l.with_timeout(d),
            ("with_io_timeout", Arg::Time(d)) => l.with_io_timeout(d),
            other => unreachable!("timeout parameter {other:?}"),
        };
    }
    l
}

/// Wrap `op` (the service, already traced) in the spec's layers. Retries are reported to the
/// operation's [`OpScope`] (backoff, throttling) when `observe` is set.
pub async fn apply_layers(op: Operator, spec: &OperatorSpec, observe: bool) -> Result<Operator> {
    apply_layers_with_tail_cut(op, spec, observe, None).await
}

/// [`apply_layers`], reusing an already-warmed [`TailCutLayer`] when given.
pub async fn apply_layers_with_tail_cut(
    op: Operator,
    spec: &OperatorSpec,
    observe: bool,
    shared_tail_cut: Option<TailCutLayer>,
) -> Result<Operator> {
    apply_layers_with_recorder(op, spec, observe, shared_tail_cut, None).await
}

/// [`apply_layers_with_tail_cut`], also observing in-memory cache lookups into `recorder`
/// when `layers.cache` is enabled.
pub async fn apply_layers_with_recorder(
    mut op: Operator,
    spec: &OperatorSpec,
    observe: bool,
    mut shared_tail_cut: Option<TailCutLayer>,
    recorder: Option<std::sync::Arc<crate::trace::Recorder>>,
) -> Result<Operator> {
    for name in LAYER_ORDER {
        let Some(p) = layer_params(spec, name) else {
            continue;
        };
        op = match name {
            "timeout" => op.layer(timeout_layer(&p)),
            "tail_cut" => op.layer(shared_tail_cut.take().unwrap_or_else(|| tail_cut_layer(&p))),
            "concurrent_limit" => {
                let mut layer = ConcurrentLimitLayer::new(count(&p, "permits").unwrap_or(1));
                let http = count(&p, "with_http_concurrent_limit");
                if let Some(h) = http {
                    layer = layer.with_http_concurrent_limit(h);
                }
                let op = op.layer(layer);
                // The stamp wraps the transport the HTTP semaphore just wrapped: a request is
                // stamped before it waits for its HTTP permit, wherever the layer sits in
                // the stack.
                if observe && http.is_some() {
                    op.layer(oprobe_trace::QueueStampLayer)
                } else {
                    op
                }
            }
            "throttle" => op.layer(ThrottleLayer::new(
                bytes(&p, "bandwidth") as u32,
                bytes(&p, "burst") as u32,
            )),
            "retry" => {
                let layer = retry_layer(&p);
                if observe {
                    op.layer(layer.with_notify(|ev: opendal::layers::RetryEvent<'_>| {
                        if let Some(s) = OpScope::current() {
                            let msg = ev.err.message();
                            let cut =
                                matches!(msg, "cancelled by tail cut" | "io cancelled by tail cut")
                                    || msg.contains("tail cut");
                            // 429/503 responses mark the scope throttled when they are
                            // recorded (`OpScope::push_request`); OpenDAL's kind covers the
                            // rest.
                            s.record_retry_with_cut(
                                ev.retry_after,
                                ev.err.kind() == opendal::ErrorKind::RateLimited,
                                cut,
                            );
                        }
                    }))
                } else {
                    op.layer(layer)
                }
            }
            "cache" => {
                let memory = bytes(&p, "memory") as usize;
                let cache = memory_cache(memory).await?;
                let op = op.layer(
                    FoyerLayer::new(cache).with_size_limit(..=cache_max_entry_bytes(memory)),
                );
                match &recorder {
                    Some(rec) if observe => {
                        op.layer(crate::trace::CacheObserveLayer::new(rec.clone()))
                    }
                    _ => op,
                }
            }
            other => unreachable!("layer {other}"),
        };
    }
    Ok(op)
}

/// Shards of the in-memory cache: one, so `layers.cache.memory` is one byte budget.
///
/// `FoyerLayer` caches whole objects and each entry weighs its object's size. Foyer splits its
/// capacity evenly between its shards (8 by default) and picks an entry's shard by hash: an
/// object larger than a shard's share cannot be kept, and a budget that covers the objects
/// read still evicts whenever the hash fills one shard before the others. With one shard, the
/// budget holds any set of objects that fits it.
pub const CACHE_SHARDS: usize = 1;

/// The largest object a cache of `memory_bytes` admits: any object that fits the budget. The
/// one admission rule of `layers.cache`, used by the measured cache, by the generated Rust
/// and by `prove` when it sizes the budget.
pub fn cache_max_entry_bytes(memory_bytes: usize) -> usize {
    memory_bytes / CACHE_SHARDS
}

impl Cache {
    /// The budget `prove` gives a cache for a run that read `read_set_bytes` of distinct
    /// objects, the largest of `largest_object_bytes`: the read set with a quarter of headroom,
    /// between 16 MiB and 1 GiB, and never less than the admission rule
    /// ([`cache_max_entry_bytes`]) needs to keep the largest object, so a sized cache cannot be
    /// one that admits nothing it was sized for.
    pub fn sized_for(read_set_bytes: u64, largest_object_bytes: u64) -> Self {
        let budget = ((read_set_bytes as f64 * 1.25) as u64).clamp(16 << 20, 1 << 30);
        let admits_largest = largest_object_bytes.saturating_mul(CACHE_SHARDS as u64);
        Self {
            memory: ByteSize(budget.max(admits_largest)),
        }
    }
}

/// The in-memory foyer cache of `layers.cache.memory`: at most `memory_bytes` of cached data.
/// Each entry weighs its value's length (foyer's default weighs every entry 1, so "1 KiB" held
/// 1024 objects of any size), and an object larger than [`cache_max_entry_bytes`] is not
/// admitted (foyer would keep an entry heavier than its shard). The layer over it gets the
/// same limit (`FoyerLayer::with_size_limit`), so OpenDAL does not fetch or buffer a whole
/// object the cache will not keep.
pub(crate) async fn memory_cache(
    memory_bytes: usize,
) -> Result<foyer::HybridCache<FoyerKey, FoyerValue>> {
    let max_entry = cache_max_entry_bytes(memory_bytes);
    foyer::HybridCacheBuilder::new()
        .memory(memory_bytes.max(1))
        .with_shards(CACHE_SHARDS)
        .with_weighter(|_: &FoyerKey, v: &FoyerValue| v.len())
        .with_filter(move |_: &FoyerKey, v: &FoyerValue| v.len() <= max_entry)
        .storage()
        .build()
        .await
        .context("cannot build the in-memory foyer cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cache = { memory = "1KiB" }` holds 1 KiB of data, not 1024 objects: a 1 KiB cache
    /// cannot keep 64 MiB (9.0 weighed every entry 1, so it "fit" 1024 × 64 KiB).
    #[tokio::test]
    async fn the_cache_budget_is_bytes_not_entries() {
        let cache = memory_cache(1024).await.unwrap();
        for i in 0..1024 {
            cache.insert(
                FoyerKey {
                    path: format!("k{i}"),
                    version: None,
                },
                FoyerValue(opendal::Buffer::from(vec![0u8; 64 * 1024])),
            );
        }
        assert!(
            cache.memory().usage() <= 1024,
            "{} bytes cached",
            cache.memory().usage()
        );
        // A small value fits and is served.
        let key = FoyerKey {
            path: "small".into(),
            version: None,
        };
        cache.insert(
            key.clone(),
            FoyerValue(opendal::Buffer::from(vec![1u8; 100])),
        );
        assert!(cache.memory().contains(&key));
    }

    #[tokio::test]
    async fn every_layer_builds_and_serves() {
        let spec = OperatorSpec::parse(
            "url = \"memory://\"\n[layers]\nconcurrent_limit = { http = 4, operations = 8 }\ntail_cut = true\nthrottle = { bandwidth = \"64MiB\", burst = \"8MiB\" }\ncache = { memory = \"8MiB\" }\n",
        )
        .unwrap();
        let base = Operator::new(opendal::services::Memory::default()).unwrap();
        let op = apply_layers(base, &spec, true).await.unwrap();
        op.write("a", vec![1u8; 1024]).await.unwrap();
        assert_eq!(op.read("a").await.unwrap().len(), 1024);
        // Read twice: the second read can come from the cache.
        assert_eq!(op.read("a").await.unwrap().len(), 1024);
        let t = spec.target().unwrap();
        assert_eq!(t.scheme, "memory");
    }

    /// An in-memory service and the same service under `layers` (a `[layers]` table).
    async fn stack(layers: &str) -> (Operator, Operator) {
        let spec =
            OperatorSpec::parse(&format!("url = \"memory://\"\n[layers]\n{layers}\n")).unwrap();
        let service = Operator::new(opendal::services::Memory::default()).unwrap();
        let op = apply_layers(service.clone(), &spec, false).await.unwrap();
        (service, op)
    }

    /// Any object that fits `layers.cache.memory` is cached and served from the cache. 9.1
    /// split the budget in 8 shards and admitted only objects up to an eighth of it: a cache
    /// "sized" to hold the objects could hold none of them.
    #[tokio::test]
    async fn an_object_within_the_cache_budget_is_admitted_and_served() {
        const BUDGET: usize = 2 << 20;
        let (service, op) = stack("cache = { memory = \"2MiB\" }").await;
        assert_eq!(cache_max_entry_bytes(BUDGET), BUDGET);
        // More than an eighth of the budget, and the whole budget.
        for (key, len) in [("quarter", BUDGET / 4), ("whole", BUDGET)] {
            let data = vec![len as u8; len];
            service.write(key, data.clone()).await.unwrap();
            assert_eq!(op.read(key).await.unwrap().to_vec(), data);
            // Gone from the service: only the cache can serve it now.
            service.delete(key).await.unwrap();
            assert_eq!(op.read(key).await.unwrap().to_vec(), data, "{key}");
            assert_eq!(op.read_with(key).range(5..9).await.unwrap().len(), 4);
        }
        // One byte over the budget: read through, never cached.
        service.write("over", vec![0u8; BUDGET + 1]).await.unwrap();
        assert_eq!(op.read("over").await.unwrap().len(), BUDGET + 1);
        service.delete("over").await.unwrap();
        let gone = op.read("over").await.unwrap_err();
        assert_eq!(gone.kind(), opendal::ErrorKind::NotFound);
    }

    /// The budget `prove` sizes admits the largest object the run read. 9.1 sized from the
    /// dataset's nominal object size and capped the budget at 1 GiB after that, while the
    /// cache admitted an eighth of its budget: an object above 128 MiB, or larger than the
    /// nominal size, got a "sized" cache that never kept it.
    #[test]
    fn the_sized_cache_admits_the_largest_object_read() {
        const MIB: u64 = 1 << 20;
        // (distinct bytes read, largest object, the budget).
        for (read_set, largest, budget) in [
            // Small objects: the floor.
            (100 * 4096, 4096, 16 * MIB),
            // The read set with a quarter of headroom.
            (64 * MIB, MIB, 80 * MIB),
            // Three 300 MiB objects (9.1: 1 GiB in 8 shards of 128 MiB, none admitted).
            (900 * MIB, 300 * MIB, 1024 * MIB),
            // A read set above the cap: the cap, which still admits each object.
            (4096 * MIB, 512 * MIB, 1024 * MIB),
            // One object above the cap: the budget is that object.
            (3000 * MIB, 3000 * MIB, 3000 * MIB),
        ] {
            let sized = Cache::sized_for(read_set, largest).memory.0;
            assert_eq!(sized, budget, "{read_set} bytes read, largest {largest}");
            assert!(
                cache_max_entry_bytes(sized as usize) as u64 >= largest,
                "a cache of {sized} bytes does not admit its largest object ({largest})"
            );
        }
    }

    /// `ThrottleLayer` is outside `ConcurrentLimitLayer` and the timers: a write that waits
    /// for its bytes holds no operation permit and is not a call the io timeout severs. In
    /// 9.1 the throttle was innermost: the wait held the permit and ended as "io timeout".
    #[tokio::test(start_paused = true)]
    async fn a_throttled_write_waits_without_a_permit_and_outside_the_timeout() {
        // One byte per second after a burst of 4 KiB; one operation at a time.
        let (_, op) = stack(
            "retry = false\nconcurrent_limit = { operations = 1 }\nthrottle = { bandwidth = 1, burst = 4096 }",
        )
        .await;
        op.write("first", vec![0u8; 4096]).await.unwrap();
        // The burst is spent: this write waits for the throttle (over an hour of real time).
        let mut second = std::pin::pin!(op.write("second", vec![0u8; 4096]));
        assert!(futures::poll!(second.as_mut()).is_pending());
        // The one operation permit is free: another operation goes through at once.
        tokio::time::timeout(Duration::from_secs(1), op.stat("first"))
            .await
            .expect("the throttled write holds the operation permit while it waits")
            .unwrap();
        // Six io timeouts later the write still waits: no timer runs over the throttle.
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(futures::poll!(second.as_mut()).is_pending());
    }

    /// `ConcurrentLimitLayer` is outside `TimeoutLayer` (and `TailCutLayer`, which sits
    /// between them): a call that queues for an operation permit is not timed while it
    /// queues. In 9.1 the limiter was inside both: the queued call ended as a timeout.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_an_operation_permit_is_not_timed() {
        let (_, op) = stack(
            "retry = false\ntail_cut = true\nconcurrent_limit = { operations = 1 }\ntimeout = { op = \"1s\", io = \"1s\" }",
        )
        .await;
        op.write("a", vec![7u8; 16]).await.unwrap();
        // A writer keeps the one permit from its first write until it is dropped.
        let mut holder = op.writer("held").await.unwrap();
        holder.write(vec![0u8; 16]).await.unwrap();
        // `stat` is under the operation timeout, a read under the io timeout: both queue for
        // five times their timeout and are still waiting.
        let mut stat = std::pin::pin!(op.stat("a"));
        let mut read = std::pin::pin!(op.read("a"));
        let queued = Duration::from_secs(5);
        assert!(tokio::time::timeout(queued, stat.as_mut()).await.is_err());
        assert!(tokio::time::timeout(queued, read.as_mut()).await.is_err());
        // The permit comes back: both complete.
        drop(holder);
        assert_eq!(stat.await.unwrap().content_length(), 16);
        assert_eq!(read.await.unwrap().to_vec(), vec![7u8; 16]);
    }
}
