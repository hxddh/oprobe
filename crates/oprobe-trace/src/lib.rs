//! `oprobe-trace`: profile any Apache OpenDAL operator in production.
//!
//! [oprobe](https://github.com/hxddh/oprobe) is a profiler for OpenDAL operators. This crate is
//! its instrumentation as one layer: add it to your `Operator` and every backend request is
//! timed by stage (queue, DNS, TCP, proxy, TLS, send, time to first byte, body, consumer
//! stall), classified by storage API and aggregated in bounded memory. Save a `.oprobe` file
//! and open it in the oprobe workbench (`oprobe show FILE`) like any other profile.
//!
//! ```
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! # use opendal_core::{Operator, services::Memory};
//! # let op = Operator::new(Memory::default())?;
//! let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
//! let op = op.layer(layer);
//! # op.write("hello.txt", "hi").await?;
//! // … use the operator as usual …
//! # let dir = std::env::temp_dir().join(format!("oprobe-trace-doc-{}", std::process::id()));
//! # std::fs::create_dir_all(&dir)?;
//! # let path = dir.join("prod.oprobe");
//! handle.save(&path)?; // then: oprobe show prod.oprobe
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok(()) }
//! ```
//!
//! # The API
//!
//! - [`layer`] returns the [`TraceLayer`] to add to an operator and a [`Handle`] to read it.
//! - [`Handle::snapshot`] returns the aggregates so far ([`Snapshot`]): per API and stage, per
//!   verb, connections, bytes and a timeline.
//! - [`Handle::save`] and [`Snapshot::save`] write a `.oprobe` file (format `oprobe.trace/9`).
//! - [`Exporter`] receives every request, operation and cache lookup as it completes; with the
//!   `otel` feature, [`otel::OtelExporter`] records every HTTP request once into the
//!   OpenTelemetry histogram `http.client.request.duration` and its stages into
//!   `oprobe.http.client.request.stage.duration`.
//! - [`Config`] sets the row budget, the HTTP client settings and the exporter.
//!
//! # How it works
//!
//! For HTTP services the layer installs an instrumented `HttpTransport`: OpenDAL's own reqwest
//! client (same TLS stack, same pool defaults), observed request by request: phase timing,
//! connection reuse, TLS and kernel TCP statistics. For in-process services (`memory`, `fs`)
//! it records one request per backend call. The recorder is always on: there is no measurement
//! window to open, and memory stays bounded ([`Config::max_rows`]). The cost is measured by the
//! `overhead` benchmark (`cargo bench -p oprobe-trace --bench overhead`), whose budget is 2 µs
//! per operation.
//!
//! # The HTTP transport and where the layer goes
//!
//! Dial, TLS and connection statistics can only be measured from inside the HTTP client, so by
//! default the layer brings its own and **replaces the transport installed on the operator**.
//! OpenDAL composes transports in the order of the `.layer(...)` calls: each layer receives
//! the transport the layers before it left and may wrap or replace it. Therefore:
//!
//! - **Add the trace layer first**, before any layer that touches the transport. A
//!   `ConcurrentLimitLayer::with_http_concurrent_limit` added *after* it wraps the instrumented
//!   client and keeps limiting; added *before* it, the limiter wraps a transport the trace
//!   layer then replaces, and stops limiting. ([`Handle::queue_layer`], added last, records
//!   the time requests wait for such a limiter.)
//! - **A custom client is not used.** A transport you installed yourself
//!   (`Operator::with_context(OperationContext::new().with_http_transport(..))`) with its own
//!   proxy, TLS roots, timeouts or pool is replaced as well; so is the process default
//!   (`HttpTransporter::install_default`). Express the settings on [`Config`]
//!   ([`Config::proxy`], [`Config::extra_ca_pem`], [`Config::http_version`],
//!   [`Config::keep_alive`], [`Config::max_inflight`]) or keep your client:
//! - **[`Config::keep_http_transport`]** makes the layer wrap the installed transport instead
//!   of replacing it. Your client, and every limiter beneath the trace layer, carries the
//!   requests unchanged, wherever the layer sits. The layer then sees each request from
//!   outside: queue, time to first byte, body, stall, status, bytes and errors are recorded,
//!   but DNS, TCP and TLS time are part of the time to first byte, and connections (reuse,
//!   dials, kernel TCP statistics) are not observed.
//!
//! `format!("{:?}", layer)` says which of the two a [`TraceLayer`] does.
//!
//! # Stability
//!
//! The items documented here are stable across 9.x: changes are additive, and public structs
//! and enums are `#[non_exhaustive]`. A trace file is read by the version that wrote it. The
//! `__internal` feature exists for the oprobe binary only and carries no semver promise.

// Half of the recorder (operation scopes, the whole-operation sampler, key dimensions, the
// certificate details) is driven by oprobe's own engine through `__internal`; an embedder's
// operator only issues requests. Without the feature that half is unreachable, not dead.
#![cfg_attr(not(feature = "__internal"), allow(dead_code))]

/// Declares modules that are private to the crate, or public (hidden) with `__internal`.
macro_rules! internal_mods {
    ($($name:ident),* $(,)?) => {$(
        #[cfg(feature = "__internal")]
        #[doc(hidden)]
        pub mod $name;
        #[cfg(not(feature = "__internal"))]
        mod $name;
    )*};
}

/// Re-exports items at the crate root for the oprobe binary (`__internal`), crate-private
/// otherwise.
macro_rules! internal_use {
    ($($path:tt)*) => {
        #[cfg(feature = "__internal")]
        #[doc(hidden)]
        pub use $($path)*;
        #[cfg(not(feature = "__internal"))]
        #[allow(unused_imports)]
        pub(crate) use $($path)*;
    };
}

internal_mods!(
    dims, file, layer, log, net, observe, record, recorder, rows, sample, scope, snapshot, util
);

#[cfg(feature = "otel")]
pub mod otel;

use std::path::Path;
use std::sync::Arc;

pub use dims::{DimensionStats, HotKeyStats, SliceStats};
pub use layer::{CacheObserveLayer, QueueStampLayer};
pub use net::HttpVersion;
pub use observe::RetryNotify;
pub use record::{Api, Protocol, RequestRecord, RequestTiming, ServiceLabels};
pub use recorder::Exporter;
pub use rows::{FailureCode, ReqDetail, TcpRow, TlsRow};
pub use snapshot::{
    ApiStats, ByteStats, CacheStats, ClientStats, ColdStartStats, ConnStats, EffectiveKnobs,
    IntegrityStats, LatencyHistogram, OpBreakdown, OpenLoopStats, Phase, Quantiles,
    RequestStageMeans, ResilienceStats, RetryLinking, SelfObservation, SlowOp, Snapshot,
    StageStats, Summary, TimelinePoint, VerbInterval, VerbStats,
};

internal_use!(file::{ServiceRecord, TRACE_FORMAT, TRACE_FORMATS_READ, TraceFile});
internal_use!(layer::{EnqueuedAt, InProcessObserveLayer, TraceContextLayer});
internal_use!(observe::{Linker, OperationObserveLayer, error_class, private_key});
internal_use!(log::{DEFAULT_MAX_REQUESTS, TraceLog, TraceRows});
internal_use!(record::classify_http);
internal_use!(recorder::{Budget, DialRecord, InflightGuard, OpCompletion, Recorder, RowSink});
internal_use!(rows::{OpBreakdownUs, OpRow, ReqRow, TraceInfo, WireProtocol});
internal_use!(sample::{OpSampler, SampleConfig, SamplingStats});
internal_use!(scope::{OpScope, ScopePropagatingExecutor});

/// How [`layer`] instruments an operator. Start from [`Config::default`] and change what you
/// need:
///
/// ```
/// let cfg = oprobe_trace::Config::default()
///     .max_rows(50_000)
///     .http_version(oprobe_trace::HttpVersion::H2);
/// # let _ = cfg;
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct Config {
    max_rows: usize,
    transport: net::TransportConfig,
    https: bool,
    keep_http_transport: bool,
    exporter: Option<Arc<dyn Exporter>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_rows: 10_000,
            transport: net::TransportConfig::default(),
            https: true,
            keep_http_transport: false,
            exporter: None,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("max_rows", &self.max_rows)
            .field("http_version", &self.transport.http)
            .field("keep_alive", &self.transport.keep_alive)
            .field("https", &self.https)
            .field("keep_http_transport", &self.keep_http_transport)
            .field("exporter", &self.exporter.is_some())
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Trace rows kept in memory (operations plus their requests; whole operations are sampled
    /// beyond it). Aggregates are exact whatever this is; 0 keeps aggregates only. Default
    /// 10 000.
    pub fn max_rows(mut self, rows: usize) -> Self {
        self.max_rows = rows;
        self
    }

    /// The HTTP version the client negotiates. Default [`HttpVersion::Auto`], which is what
    /// OpenDAL's default client does (HTTP/1.1).
    pub fn http_version(mut self, v: HttpVersion) -> Self {
        self.transport.http = v;
        self
    }

    /// Keep idle connections for reuse (default true, as OpenDAL does).
    pub fn keep_alive(mut self, on: bool) -> Self {
        self.transport.keep_alive = on;
        self
    }

    /// Cap concurrent in-flight HTTP requests (default: unlimited). Requests waiting for a slot
    /// show up in the `queue` stage.
    pub fn max_inflight(mut self, n: usize) -> Self {
        self.transport.max_inflight = Some(n.max(1));
        self
    }

    /// Trust an extra PEM bundle besides the system and WebPKI roots (a private S3, MinIO).
    pub fn extra_ca_pem(mut self, pem: impl Into<String>) -> Self {
        self.transport.tls.extra_ca_pem = Some(pem.into());
        self
    }

    /// Skip certificate verification (local self-signed endpoints only).
    pub fn insecure_skip_verify(mut self, on: bool) -> Self {
        self.transport.tls.insecure_skip_verify = on;
        self
    }

    /// The egress proxy: `Some("http://host:port")`, or `None` for no proxy. By default it is
    /// taken from `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`/`NO_PROXY`.
    pub fn proxy(mut self, url: Option<&str>) -> Self {
        self.transport.proxy = Some(url.map(str::to_owned));
        self
    }

    /// Whether the endpoint is HTTPS; it selects `HTTPS_PROXY` over `HTTP_PROXY` (default
    /// true).
    pub fn https(mut self, on: bool) -> Self {
        self.https = on;
        self
    }

    /// Read the kernel's TCP statistics of the connection at the end of every request (Linux
    /// and macOS; default true).
    pub fn tcp_info(mut self, on: bool) -> Self {
        self.transport.tcp_info = on;
        self
    }

    /// Keep the HTTP transport already installed on the operator and observe it from outside,
    /// instead of replacing it with oprobe's instrumented client (default false).
    ///
    /// Use it when the operator has its own HTTP client (a proxy, private TLS roots, timeouts,
    /// pool limits set in code) or when a layer added before this one wraps the transport (an
    /// HTTP concurrency limit): the requests keep going through all of it.
    ///
    /// ```
    /// # use opendal_core::{HttpTransporter, OperationContext, Operator, services::Memory};
    /// # fn demo(op: Operator, my_client: HttpTransporter) {
    /// let cfg = oprobe_trace::Config::default().keep_http_transport(true);
    /// let (trace, handle) = oprobe_trace::layer(cfg);
    /// let op = op
    ///     .with_context(OperationContext::new().with_http_transport(my_client))
    ///     .layer(trace);
    /// # let _ = (op, handle);
    /// # }
    /// ```
    ///
    /// The price is what only the client itself can see: DNS, TCP, proxy and TLS time and the
    /// time to send a request body are not split out (they are part of the time to first
    /// byte), and connections (reuse, dials, kernel TCP statistics) are not observed: the
    /// profile says so ([`ConnStats::observed`]) instead of reporting none dialed and none
    /// reused. The client settings of this `Config`
    /// ([`Config::http_version`], [`Config::keep_alive`], [`Config::proxy`],
    /// [`Config::extra_ca_pem`], [`Config::insecure_skip_verify`], [`Config::tcp_info`]) do
    /// not apply: the client is yours. [`Config::max_inflight`] still does.
    pub fn keep_http_transport(mut self, on: bool) -> Self {
        self.keep_http_transport = on;
        self
    }

    /// Call `e` for every recorded request and operation (metrics, logs, your own sink).
    /// Each HTTP request names the service of the operator that issued it
    /// ([`RequestRecord::service`]), so one exporter serves a layer applied to several
    /// operators.
    pub fn exporter(mut self, e: Arc<dyn Exporter>) -> Self {
        self.exporter = Some(e);
        self
    }

    /// The transport settings as one value (the oprobe binary builds them from `op.toml`).
    #[cfg(feature = "__internal")]
    #[doc(hidden)]
    pub fn transport(mut self, t: net::TransportConfig) -> Self {
        self.transport = t;
        self
    }
}

/// The OpenDAL layer of [`layer`]: every OpenDAL call as one operation, with the backend
/// requests it issued (the instrumented HTTP transport and scope propagation for HTTP services,
/// one record per backend call for in-process services).
///
/// Unless built with [`Config::keep_http_transport`], it **replaces** the operator's HTTP
/// transport with its instrumented client: add it before any layer that wraps the transport
/// (see [the crate documentation](crate#the-http-transport-and-where-the-layer-goes)). Its
/// `Debug` output names what it does to the transport.
#[derive(Clone)]
pub struct TraceLayer {
    recorder: Arc<Recorder>,
    context: Option<TraceContextLayer>,
    service: Arc<std::sync::OnceLock<file::ServiceRecord>>,
    linker: Arc<observe::Linker>,
    http_version: HttpVersion,
    keep_alive: bool,
    max_inflight_count: Option<usize>,
}

impl std::fmt::Debug for TraceLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let http_transport = match &self.context {
            Some(c) if c.keeps_installed() => "the installed transport, observed",
            Some(_) => "replaced by the instrumented client",
            None => "not instrumented",
        };
        f.debug_struct("TraceLayer")
            .field("http_transport", &http_transport)
            .finish_non_exhaustive()
    }
}

/// Services that serve requests in this process (no HTTP to observe).
fn in_process(scheme: &str) -> bool {
    matches!(scheme, "memory" | "fs")
}

impl opendal_core::raw::Layer for TraceLayer {
    fn apply_service(&self, srv: opendal_core::raw::Servicer) -> opendal_core::raw::Servicer {
        use opendal_core::raw::*;
        let info = srv.info();
        self.service.get_or_init(|| {
            // The client settings describe the instrumented client: with the operator's own
            // transport kept, that client does not exist.
            let installed = self.context.as_ref().is_some_and(|c| c.keeps_installed());
            let s = file::ServiceRecord {
                scheme: info.scheme().to_owned(),
                name: info.name().to_string(),
                root: info.root().to_string(),
                http_version: (!installed).then_some(self.http_version),
                keep_alive: (!installed).then_some(self.keep_alive),
                max_inflight_count: self.max_inflight_count,
                http_transport: installed.then(|| file::HTTP_TRANSPORT_INSTALLED.to_owned()),
            };
            if let Some(e) = self.recorder.exporter() {
                e.on_service(&s.scheme, &s.name, &s.root);
            }
            s
        });
        // In-process services: one request per backend call, beneath the operation.
        let srv = if in_process(info.scheme()) {
            InProcessObserveLayer::new(self.recorder.clone()).apply_service(srv)
        } else {
            srv
        };
        observe::OperationObserveLayer::new(self.recorder.clone(), self.linker.clone())
            .apply_service(srv)
    }

    fn apply_context(
        &self,
        srv: opendal_core::raw::Servicer,
        inner: opendal_core::OperationContext,
    ) -> opendal_core::OperationContext {
        let Some(context) = &self.context else {
            return inner;
        };
        let ctx = context.apply_context(srv.clone(), inner);
        if self.recorder.exporter().is_none() {
            return ctx;
        }
        // A layer may be applied to several operators (it is `Clone`): its exporter learns
        // from each request which operator's service it went to.
        let labelled = layer::ServiceLabelTransport::around(ctx.http_transport().clone(), &srv);
        ctx.with_http_transport(labelled)
    }
}

/// Reads what the layer recorded. Cheap to clone; every clone reads the same recorder.
#[derive(Clone)]
pub struct Handle {
    recorder: Arc<Recorder>,
    transport_error: Option<Arc<str>>,
    service: Arc<std::sync::OnceLock<file::ServiceRecord>>,
    linker: Arc<observe::Linker>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("transport_error", &self.transport_error)
            .finish_non_exhaustive()
    }
}

impl Handle {
    /// The aggregates so far: per API and stage, per verb, connections, bytes, timeline.
    pub fn snapshot(&self) -> Snapshot {
        self.linker.flush(&self.recorder);
        self.recorder.snapshot()
    }

    /// The retry notifier for OpenDAL's `RetryLayer`, so retries are part of the profile (the
    /// Retries section: count, backoff, throttling) and the attempts of one call are one
    /// operation:
    ///
    /// ```
    /// # use opendal_core::{Operator, services::Memory};
    /// # fn demo(op: Operator) {
    /// let (trace, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    /// let op = op
    ///     .layer(trace)
    ///     .layer(opendal_layer_retry::RetryLayer::new().with_notify(handle.retry_notify()));
    /// # }
    /// ```
    pub fn retry_notify(&self) -> RetryNotify {
        RetryNotify::new(self.recorder.clone(), self.linker.clone())
    }

    /// A layer to place directly outside a local cache layer (`FoyerLayer`, `MokaLayer`) so
    /// cache hits, hit bytes and lookup latency are recorded in [`Snapshot::cache`]:
    ///
    /// ```no_run
    /// # use opendal_core::{Operator, services::Memory};
    /// # fn demo<L: opendal_core::raw::Layer>(op: Operator, foyer_layer: L) {
    /// let (trace, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    /// let op = op.layer(trace).layer(foyer_layer).layer(handle.cache_layer());
    /// # let _ = op;
    /// # }
    /// ```
    pub fn cache_layer(&self) -> CacheObserveLayer {
        CacheObserveLayer::with_linker(self.recorder.clone(), self.linker.clone())
    }

    /// Outermost layer to place outside `ConcurrentLimitLayer` so time spent waiting for an
    /// HTTP concurrency permit is recorded in the `queue` stage of every request:
    ///
    /// ```
    /// # use opendal_core::{Operator, raw::Layer};
    /// # fn demo<L: Layer>(op: Operator, concurrent_limit_layer: L) {
    /// let (trace, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
    /// let op = op
    ///     .layer(trace) // first: it installs the client the limiter then wraps
    ///     .layer(concurrent_limit_layer) // ConcurrentLimitLayer::with_http_concurrent_limit
    ///     .layer(handle.queue_layer()); // last: stamps requests before they wait
    /// # let _ = op;
    /// # }
    /// ```
    pub fn queue_layer(&self) -> QueueStampLayer {
        QueueStampLayer
    }

    /// Write a `.oprobe` profile to `path`: the aggregates so far plus the sampled operations
    /// with their requests (the request waterfall). The rows are moved out of memory, so
    /// successive saves hold the rows recorded since the previous one; the aggregates are
    /// cumulative. Open the file with `oprobe show FILE` or in the workbench.
    ///
    /// A path that cannot be written (a missing directory, a directory, no permission) or a
    /// disk without room for the file fails before anything is taken: the rows stay for the
    /// next save. The room is reserved first, so once the rows are taken only an I/O error of
    /// the disk itself can still fail the save.
    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let pending = self.prepare_save(path.as_ref())?;
        let snapshot = self.snapshot();
        let rows = self.recorder.take_trace();
        pending.finish(self.service.get(), &snapshot, Some(&rows))
    }

    /// The file a save writes, with room reserved for what it will hold (an estimate from
    /// now, with a margin for what is recorded until the save takes it).
    fn prepare_save(&self, path: &Path) -> std::io::Result<file::Pending> {
        let mut pending = file::Pending::create(path)?;
        let now = file::json_len(&self.recorder.snapshot()) + self.recorder.trace_json_len();
        pending.reserve(now + now / 4 + (1 << 20))?;
        Ok(pending)
    }

    /// [`Handle::save`], then start a fresh window: the aggregates start again from zero, so
    /// each file profiles only its own period (hourly files are hourly profiles). The reset is
    /// atomic with the snapshot: nothing recorded in between is lost.
    ///
    /// ```no_run
    /// # fn demo(handle: oprobe_trace::Handle) -> std::io::Result<()> {
    /// // Once an hour:
    /// handle.save_and_reset("prod-1400.oprobe")?;
    /// # Ok(()) }
    /// ```
    ///
    /// A path that cannot be written (a missing directory, a directory, no permission) or a
    /// disk without room for the file fails before the reset: the window goes on, and the
    /// next save holds it.
    pub fn save_and_reset(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let pending = self.prepare_save(path.as_ref())?;
        self.linker.flush(&self.recorder);
        // The rows of the same window as the aggregates (one critical section).
        let (snapshot, rows) = self.recorder.snapshot_and_reset_with_rows();
        pending.finish(self.service.get(), &snapshot, Some(&rows))
    }

    /// Why HTTP requests are not instrumented (the transport could not be built, for example
    /// an invalid proxy URL); only in-process services are observed then, and the operator
    /// keeps the transport it had.
    pub fn transport_error(&self) -> Option<&str> {
        self.transport_error.as_deref()
    }

    /// Take the sampled rows (whole operations with their requests) and start afresh.
    #[cfg(feature = "__internal")]
    #[doc(hidden)]
    pub fn take_rows(&self) -> TraceRows {
        self.recorder.take_trace()
    }

    /// The recorder itself (op scopes, phases, budgets).
    #[cfg(feature = "__internal")]
    #[doc(hidden)]
    pub fn recorder(&self) -> &Arc<Recorder> {
        &self.recorder
    }
}

impl Snapshot {
    /// Write this snapshot to `path` as a `.oprobe` profile without request rows (the
    /// aggregates only). [`Handle::save`] also writes the sampled requests.
    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        file::write(path.as_ref(), None, self, None)
    }
}

/// Instrument an operator: `op.layer(layer)`, then `handle.snapshot()` or `handle.save(path)`.
pub fn layer(cfg: Config) -> (TraceLayer, Handle) {
    let recorder = Recorder::always_on(cfg.max_rows);
    // Unlinked until `Handle::retry_notify` makes a notifier (the snapshot says which).
    recorder.set_retries_linked(false);
    if let Some(e) = cfg.exporter {
        recorder.set_exporter(e);
    }
    let http_version = cfg.transport.http;
    let keep_alive = cfg.transport.keep_alive;
    let max_inflight_count = cfg.transport.max_inflight;
    let (context, transport_error) = if cfg.keep_http_transport {
        let observed = TraceContextLayer::around_installed(recorder.clone(), max_inflight_count);
        (Some(observed), None)
    } else {
        match net::FidelityTransport::new(cfg.transport, cfg.https, Some(recorder.clone())) {
            Ok(t) => (Some(TraceContextLayer::new(t)), None),
            // A proxy URL may carry `user:password@`.
            Err(e) => {
                let why = util::strip_url_credentials(&format!("{e:#}"));
                (None, Some(Arc::from(why)))
            }
        }
    };
    let service = Arc::new(std::sync::OnceLock::new());
    let linker = Arc::new(observe::Linker::default());
    (
        TraceLayer {
            recorder: recorder.clone(),
            context,
            service: service.clone(),
            linker: linker.clone(),
            http_version,
            keep_alive,
            max_inflight_count,
        },
        Handle {
            recorder,
            transport_error,
            service,
            linker,
        },
    )
}
