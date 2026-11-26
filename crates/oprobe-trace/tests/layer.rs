//! The public API on a `memory://` operator: layer it, use it, read a snapshot, save a profile.

use opendal_core::Operator;
use opendal_core::services::Memory;

fn temp_file(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oprobe-trace-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("profile.oprobe")
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn a_memory_operator_reports_its_requests() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    assert!(handle.transport_error().is_none());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    for i in 0..10 {
        op.write(&format!("k{i}"), vec![7u8; 1024]).await.unwrap();
    }
    for i in 0..10 {
        assert_eq!(op.read(&format!("k{i}")).await.unwrap().len(), 1024);
    }
    op.stat("k0").await.unwrap();

    let snap = handle.snapshot();
    let count = |api: &str| {
        snap.apis
            .iter()
            .filter(|a| a.api.label() == api)
            .map(|a| a.count)
            .sum::<u64>()
    };
    assert_eq!(count("PutObject"), 10, "{:?}", snap.apis);
    assert_eq!(count("GetObject"), 10, "{:?}", snap.apis);
    assert_eq!(count("HeadObject"), 1, "{:?}", snap.apis);
    let down: u64 = snap.apis.iter().map(|a| a.bytes_down).sum();
    assert_eq!(down, 10 * 1024);

    // Every call is an operation with its request (design v7 B1).
    assert_eq!(snap.summary.ops, 21);
    let verbs: Vec<&str> = snap.verbs.iter().map(|v| v.verb.as_str()).collect();
    assert_eq!(verbs, ["read.full", "stat", "write"]);
    assert!(snap.verbs.iter().all(|v| v.requests_per_op == 1.0));
    assert_eq!(snap.bytes.logical_read, 10 * 1024);
    assert_eq!(snap.bytes.logical_write, 10 * 1024);

    // The profile holds the aggregates, every operation and every request, linked.
    let path = temp_file("requests");
    handle.save(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["format"], "oprobe.trace/9");
    assert_eq!(doc["oprobe_trace_version"], env!("CARGO_PKG_VERSION"));
    // The service the layer was applied to (no options, no credentials).
    assert_eq!(doc["service"]["scheme"], "memory");
    let reqs = doc["rows"]["reqs"].as_array().unwrap();
    assert_eq!(reqs.len(), 21);
    let ops = doc["rows"]["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 21);
    let ids: std::collections::BTreeSet<u64> =
        ops.iter().map(|o| o["op_id"].as_u64().unwrap()).collect();
    assert!(
        reqs.iter()
            .all(|r| ids.contains(&r["op_id"].as_u64().unwrap()))
    );
    // Keys are hashed, never recorded (docs/privacy.md).
    assert!(
        ops.iter()
            .all(|o| o["key"].as_str().unwrap().starts_with('#')),
        "{ops:#?}"
    );
    assert_eq!(doc["rows"]["sampling"]["seen_requests"], 21);
    let apis = doc["snapshot"]["apis"].as_array().unwrap();
    let total: u64 = apis
        .iter()
        .map(|a| a["requests_count"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 21);

    // Saving moved the rows out of memory: the next profile starts afresh.
    let path = temp_file("again");
    handle.save(&path).unwrap();
    let doc = read_json(&path);
    assert!(doc["rows"]["reqs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn memory_stays_bounded() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default().max_rows(64));
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("k", vec![1u8; 16]).await.unwrap();
    for _ in 0..2_000 {
        op.stat("k").await.unwrap();
    }
    let snap = handle.snapshot();
    let heads: u64 = snap
        .apis
        .iter()
        .filter(|a| a.api.label() == "HeadObject")
        .map(|a| a.count)
        .sum();
    // Aggregates are exact; rows are capped.
    assert_eq!(heads, 2_000);
    let path = temp_file("bounded");
    handle.save(&path).unwrap();
    let doc = read_json(&path);
    let kept = doc["rows"]["reqs"].as_array().unwrap().len();
    assert!(kept <= 64, "{kept}");
    assert_eq!(doc["rows"]["sampling"]["seen_requests"], 2_001);
}

#[tokio::test]
async fn a_snapshot_saves_without_rows() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("a", "x").await.unwrap();
    let path = temp_file("snapshot");
    handle.snapshot().save(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["format"], "oprobe.trace/9");
    assert!(doc.get("rows").is_none());
    assert_eq!(doc["snapshot"]["apis"][0]["api"], "PutObject");
}

/// Hourly files are hourly profiles: `save_and_reset` starts a fresh window.
#[tokio::test]
async fn save_and_reset_starts_a_fresh_window() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("a", "x").await.unwrap();
    for _ in 0..3 {
        op.stat("a").await.unwrap();
    }
    let path = temp_file("hour1");
    handle.save_and_reset(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["snapshot"]["summary"]["ops_count"], 4);
    // The next window holds only what happened since.
    op.stat("a").await.unwrap();
    let snap = handle.snapshot();
    assert_eq!(snap.summary.ops, 1);
    let requests: u64 = snap.apis.iter().map(|a| a.count).sum();
    assert_eq!(requests, 1);
    let path = temp_file("hour2");
    handle.save_and_reset(&path).unwrap();
    let doc = read_json(&path);
    assert_eq!(doc["snapshot"]["summary"]["ops_count"], 1);
    assert_eq!(doc["rows"]["ops"].as_array().unwrap().len(), 1);
}

struct Count(std::sync::atomic::AtomicU64);

impl oprobe_trace::Exporter for Count {
    fn on_request(&self, _r: &oprobe_trace::RequestRecord) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::test]
async fn the_exporter_hook_sees_every_request() {
    let count = std::sync::Arc::new(Count(Default::default()));
    let (layer, _handle) =
        oprobe_trace::layer(oprobe_trace::Config::default().exporter(count.clone()));
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("a", "x").await.unwrap();
    op.read("a").await.unwrap();
    assert_eq!(count.0.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
fn an_invalid_proxy_is_reported_not_fatal() {
    let (_layer, handle) =
        oprobe_trace::layer(oprobe_trace::Config::default().proxy(Some("socks5://x:1")));
    let err = handle
        .transport_error()
        .expect("the transport must refuse the proxy");
    assert!(err.contains("http://"), "{err}");
}

#[tokio::test]
async fn in_process_copy_and_rename_record_requests() {
    let dir = std::env::temp_dir().join(format!("oprobe-trace-fs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(opendal_service_fs::Fs::default().root(dir.to_str().unwrap()))
        .unwrap()
        .layer(layer);
    op.write("src", vec![9u8; 512]).await.unwrap();
    op.copy("src", "dst").await.unwrap();
    op.rename("dst", "moved").await.unwrap();
    let snap = handle.snapshot();
    let copies: u64 = snap
        .apis
        .iter()
        .filter(|a| a.api.label() == "CopyObject")
        .map(|a| a.count)
        .sum();
    assert_eq!(copies, 2, "{:?}", snap.apis);
    let copy_verb = snap.verbs.iter().find(|v| v.verb == "copy").unwrap();
    assert_eq!(copy_verb.requests_per_op, 1.0);
    let rename_verb = snap.verbs.iter().find(|v| v.verb == "rename").unwrap();
    assert_eq!(rename_verb.requests_per_op, 1.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn early_reader_drop_does_not_fabricate_cache_hits() {
    let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let op = Operator::new(Memory::default()).unwrap().layer(layer);
    op.write("k0", vec![1u8; 4096]).await.unwrap();

    // Opening a reader and dropping its stream before reading any bytes issues zero backend
    // GETs, which is NOT a local cache hit when no cache layer is installed.
    let reader = op.reader("k0").await.unwrap();
    let stream = reader.into_bytes_stream(..).await.unwrap();
    drop(stream);

    let snap = handle.snapshot();
    assert!(!snap.cache.enabled, "{:?}", snap.cache);
    assert_eq!(snap.cache.hits, 0);
    assert_eq!(snap.cache.lookups, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_layer_isolates_concurrent_hits_and_misses_and_counts_stat() {
    use opendal_core::raw::oio::{self, ReadStreamDyn};
    use opendal_core::raw::*;
    use opendal_core::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, Default)]
    struct TinyCacheLayer {
        entries: Arc<Mutex<HashMap<String, Buffer>>>,
        stats: Arc<Mutex<HashMap<String, Metadata>>>,
    }

    impl Layer for TinyCacheLayer {
        fn apply_service(&self, inner: Servicer) -> Servicer {
            Arc::new(TinyCacheService {
                inner,
                entries: self.entries.clone(),
                stats: self.stats.clone(),
            })
        }
    }

    #[derive(Debug)]
    struct TinyCacheService {
        inner: Servicer,
        entries: Arc<Mutex<HashMap<String, Buffer>>>,
        stats: Arc<Mutex<HashMap<String, Metadata>>>,
    }

    struct MissArgs {
        inner: Servicer,
        ctx: OperationContext,
        path: String,
        args: OpRead,
        entries: Arc<Mutex<HashMap<String, Buffer>>>,
    }

    struct TinyCacheReader {
        hit: Option<Buffer>,
        miss: Option<MissArgs>,
    }

    impl oio::Read for TinyCacheReader {
        async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn ReadStreamDyn>)> {
            if let Some(buf) = &self.hit {
                let bytes = buf.to_bytes();
                let start = range.offset() as usize;
                let end = range
                    .size()
                    .map_or(bytes.len(), |s| (start + s as usize).min(bytes.len()));
                let slice = Buffer::from(bytes.slice(start..end));
                return Ok((RpRead::default(), Box::new(slice) as Box<dyn ReadStreamDyn>));
            }
            let m = self.miss.as_ref().unwrap();
            tokio::task::yield_now().await;
            let reader = m.inner.read(&m.ctx, &m.path, m.args.clone())?;
            let (_rp, mut stream) = reader.open(BytesRange::from(..)).await?;
            let mut chunks = Vec::new();
            loop {
                let b = stream.read_dyn().await?;
                if b.is_empty() {
                    break;
                }
                chunks.push(b.to_bytes());
            }
            let full = Buffer::from(chunks);
            m.entries
                .lock()
                .unwrap()
                .insert(m.path.clone(), full.clone());
            let bytes = full.to_bytes();
            let start = range.offset() as usize;
            let end = range
                .size()
                .map_or(bytes.len(), |s| (start + s as usize).min(bytes.len()));
            let slice = Buffer::from(bytes.slice(start..end));
            Ok((RpRead::default(), Box::new(slice) as Box<dyn ReadStreamDyn>))
        }

        async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
            let (rp, mut stream) = self.open(range).await?;
            let mut chunks = Vec::new();
            loop {
                let b = stream.read_dyn().await?;
                if b.is_empty() {
                    break;
                }
                chunks.push(b.to_bytes());
            }
            Ok((rp, Buffer::from(chunks)))
        }
    }

    impl Service for TinyCacheService {
        type Reader = TinyCacheReader;
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
        async fn stat(&self, ctx: &OperationContext, path: &str, args: OpStat) -> Result<RpStat> {
            if let Some(meta) = self.stats.lock().unwrap().get(path).cloned() {
                return Ok(RpStat::new(meta));
            }
            let rp = self.inner.stat(ctx, path, args).await?;
            self.stats
                .lock()
                .unwrap()
                .insert(path.to_owned(), rp.clone().into_metadata());
            Ok(rp)
        }
        fn read(&self, ctx: &OperationContext, path: &str, args: OpRead) -> Result<Self::Reader> {
            if let Some(buf) = self.entries.lock().unwrap().get(path).cloned() {
                return Ok(TinyCacheReader {
                    hit: Some(buf),
                    miss: None,
                });
            }
            Ok(TinyCacheReader {
                hit: None,
                miss: Some(MissArgs {
                    inner: self.inner.clone(),
                    ctx: ctx.clone(),
                    path: path.to_owned(),
                    args,
                    entries: self.entries.clone(),
                }),
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
        fn compose(
            &self,
            ctx: &OperationContext,
            to: &str,
            args: OpCompose,
        ) -> Result<Self::Composer> {
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

    let (trace_layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    let cache = TinyCacheLayer::default();
    let op = Operator::new(Memory::default())
        .unwrap()
        .layer(trace_layer)
        .layer(cache)
        .layer(handle.cache_layer());

    for i in 0..8 {
        op.write(&format!("obj-{i}"), vec![42u8; 256])
            .await
            .unwrap();
    }
    // Prime obj-0 in cache (1 read miss + 1 stat miss).
    assert_eq!(op.read("obj-0").await.unwrap().len(), 256);
    assert_eq!(op.stat("obj-0").await.unwrap().content_length(), 256);

    // Concurrently run 16 cached reads on obj-0, 7 uncached reads on obj-1..7, and 4 cached stats on obj-0.
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let op = op.clone();
        tasks.push(tokio::spawn(async move {
            assert_eq!(op.read("obj-0").await.unwrap().len(), 256);
        }));
    }
    for i in 1..8 {
        let op = op.clone();
        tasks.push(tokio::spawn(async move {
            assert_eq!(op.read(&format!("obj-{i}")).await.unwrap().len(), 256);
        }));
    }
    for _ in 0..4 {
        let op = op.clone();
        tasks.push(tokio::spawn(async move {
            assert_eq!(op.stat("obj-0").await.unwrap().content_length(), 256);
        }));
    }
    // Plus 1 uncached stat.
    op.write("stat-only", vec![1u8; 64]).await.unwrap();
    assert_eq!(op.stat("stat-only").await.unwrap().content_length(), 64);

    for t in tasks {
        t.await.unwrap();
    }

    let snap = handle.snapshot();
    assert!(snap.cache.enabled);
    // Misses: 1 (prime read obj-0) + 1 (prime stat obj-0) + 7 (obj-1..7) + 1 (stat-only) = 10
    // Hits: 16 (read obj-0) + 4 (stat obj-0) = 20
    // Lookups: 10 + 20 = 30
    assert_eq!(snap.cache.hits, 20, "{:?}", snap.cache);
    assert_eq!(snap.cache.lookups, 30, "{:?}", snap.cache);
    // Operations include both cache hits and cache misses + writes (9 writes + 24 reads + 6 stats = 39).
    assert_eq!(snap.summary.ops, 39, "{:?}", snap.summary);

    let path = temp_file("cache-concurrent");
    handle.save(&path).unwrap();
    let doc = read_json(&path);
    let reqs = doc["rows"]["reqs"].as_array().unwrap();
    assert!(
        reqs.iter().all(|r| r["op_id"].as_u64().unwrap() != 0),
        "{reqs:#?}"
    );
}

mod host_transport {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use opendal_core::raw::{Layer, Servicer};
    use opendal_core::{
        Buffer, HttpBody, HttpTransport, HttpTransporter, OperationContext, Operator, Result,
    };

    /// The application's own HTTP client: it answers itself (no network) and counts.
    struct HostClient(Arc<AtomicUsize>);

    impl HttpTransport for HostClient {
        async fn fetch(&self, _: http::Request<Buffer>) -> Result<http::Response<HttpBody>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            let body = futures::stream::iter([Ok(Buffer::from("hello"))]);
            Ok(http::Response::builder()
                .status(200)
                .header("content-length", "5")
                .body(HttpBody::new(body, Some(5)))
                .unwrap())
        }
    }

    /// What `ConcurrentLimitLayer::with_http_concurrent_limit` does to the context: it wraps
    /// the transport the layers before it left (here it only counts).
    #[derive(Debug)]
    struct Gate(Arc<AtomicUsize>);

    struct Gated {
        inner: HttpTransporter,
        passed: Arc<AtomicUsize>,
    }

    impl HttpTransport for Gated {
        async fn fetch(&self, req: http::Request<Buffer>) -> Result<http::Response<HttpBody>> {
            self.passed.fetch_add(1, Ordering::Relaxed);
            self.inner.fetch(req).await
        }
    }

    impl Layer for Gate {
        fn apply_context(&self, _: Servicer, inner: OperationContext) -> OperationContext {
            let gated = Gated {
                inner: inner.http_transport().clone(),
                passed: self.0.clone(),
            };
            inner.with_http_transport(HttpTransporter::new(gated))
        }
    }

    /// An operator with its own client and an HTTP gate added *before* the trace layer.
    fn operator(trace: oprobe_trace::TraceLayer) -> (Operator, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (client, gate) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        // Port 1 on loopback: nothing listens, so a request that leaves the host's client
        // fails at once instead of reaching anything.
        let service = opendal_service_http::Http::default().endpoint("http://127.0.0.1:1");
        let context = OperationContext::new()
            .with_http_transport(HttpTransporter::new(HostClient(client.clone())));
        let op = Operator::new(service)
            .unwrap()
            .with_context(context)
            .layer(Gate(gate.clone()))
            .layer(trace);
        (op, client, gate)
    }

    /// `Config::keep_http_transport`: the application's client and the limiter beneath the
    /// trace layer carry every request, and the layer still records them (9.1 had no way to
    /// keep them: the layer always installed its own client in their place).
    #[tokio::test]
    async fn keep_http_transport_leaves_the_hosts_client_and_inner_limiters_in_charge() {
        let cfg = oprobe_trace::Config::default().keep_http_transport(true);
        let (trace, handle) = oprobe_trace::layer(cfg);
        assert!(handle.transport_error().is_none());
        assert_eq!(
            format!("{trace:?}"),
            r#"TraceLayer { http_transport: "the installed transport, observed", .. }"#
        );
        let (op, client, gate) = operator(trace);
        for _ in 0..3 {
            assert_eq!(op.read("k").await.unwrap().to_vec(), b"hello");
        }
        let snap = handle.snapshot();
        let requests: u64 = snap.apis.iter().map(|a| a.count).sum();
        assert!(requests >= 3, "{:?}", snap.apis);
        // Every request went through the gate and the host's client, and was recorded.
        assert_eq!(gate.load(Ordering::Relaxed) as u64, requests);
        assert_eq!(client.load(Ordering::Relaxed) as u64, requests);
        let down: u64 = snap.apis.iter().map(|a| a.bytes_down).sum();
        assert_eq!(down, 5 * requests);
        assert_eq!(snap.summary.ops, 3);
        assert_eq!(snap.resilience.network_errors, 0, "{:?}", snap.resilience);
        // The connections are the host client's own business: none is reported dialed, and
        // none reused either. What nobody observed is not a number (9.2 counted every answered
        // request as warm: "100 % reused").
        let c = &snap.connections;
        assert_eq!(
            (c.dialed, c.reused_requests, c.reuse_pct),
            (0, 0, 0.0),
            "{c:?}"
        );
        assert_eq!(c.http1_requests + c.http2_requests, requests, "{c:?}");
        assert!(!c.observed(), "{c:?}");
        assert!(
            snap.apis.iter().all(|a| a.reused_pct == 0.0),
            "{:?}",
            snap.apis
        );

        // The file says whose client carried the requests, and does not describe the
        // instrumented client it did not use (9.2 wrote that client's `http_version` and
        // `keep_alive`).
        let path = super::temp_file("keep-transport");
        handle.save(&path).unwrap();
        let doc = super::read_json(&path);
        let service = doc["service"].as_object().unwrap();
        assert_eq!(service["http_transport"], "installed", "{service:?}");
        assert!(!service.contains_key("http_version"), "{service:?}");
        assert!(!service.contains_key("keep_alive"), "{service:?}");
        let reqs = doc["rows"]["reqs"].as_array().unwrap();
        assert_eq!(reqs.len() as u64, requests);
        assert!(reqs.iter().all(|r| r["conn_id"] == 0), "{reqs:#?}");
    }

    /// The default is the opposite, and says so: the layer's instrumented client replaces
    /// whatever transport the operator had, so neither the host's client nor a limiter added
    /// before the layer sees a request (docs/embedding.md: add the trace layer first).
    #[tokio::test]
    async fn by_default_the_instrumented_client_replaces_the_installed_transport() {
        let cfg = oprobe_trace::Config::default().https(false).proxy(None);
        let (trace, handle) = oprobe_trace::layer(cfg);
        assert_eq!(
            format!("{trace:?}"),
            r#"TraceLayer { http_transport: "replaced by the instrumented client", .. }"#
        );
        let (op, client, gate) = operator(trace);
        assert!(op.read("k").await.is_err());
        assert_eq!(client.load(Ordering::Relaxed), 0);
        assert_eq!(gate.load(Ordering::Relaxed), 0);
        let snap = handle.snapshot();
        assert!(snap.resilience.network_errors >= 1);
        assert!(snap.connections.observed(), "{:?}", snap.connections);
        // The file describes the instrumented client, as every 9.x file does.
        let path = super::temp_file("own-transport");
        handle.save(&path).unwrap();
        let service = super::read_json(&path)["service"].clone();
        assert_eq!(service["http_version"], "auto", "{service}");
        assert_eq!(service["keep_alive"], true, "{service}");
        assert!(service.get("http_transport").is_none(), "{service}");
    }
}
