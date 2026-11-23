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
//! - [`Handle::save`] and [`Snapshot::save`] write a `.oprobe` file (format `oprobe.trace/6`).
//! - [`Exporter`] receives every request as it completes; with the `otel` feature,
//!   [`otel::OtelExporter`] turns them into OpenTelemetry metrics
//!   (`http.client.request.duration` with `oprobe.stage` and `oprobe.api`).
//! - [`Config`] sets the row budget, the HTTP client settings and the exporter.
//!
//! # How it works
//!
//! For HTTP services the layer installs an instrumented `HttpFetch`: OpenDAL's own reqwest
//! client (same TLS stack, same pool defaults), observed request by request: phase timing,
//! connection reuse, TLS and kernel TCP statistics. For in-process services (`memory`, `fs`)
//! it records one request per backend call. The recorder is always on: there is no measurement
//! window to open, and memory stays bounded ([`Config::max_rows`]). The cost is measured by the
//! `overhead` benchmark (`cargo bench -p oprobe-trace --bench overhead`), whose budget is 2 µs
//! per operation.
//!
//! # Stability
//!
//! The items documented here, the `.oprobe` format and the meaning of every field are stable
//! across 6.x: changes are additive, and public structs and enums are `#[non_exhaustive]`. The
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
    dims, file, layer, log, net, record, recorder, rows, sample, scope, snapshot, util
);

#[cfg(feature = "otel")]
pub mod otel;

use std::path::Path;
use std::sync::Arc;

pub use dims::{DimensionStats, HotKeyStats, SliceStats};
pub use net::HttpVersion;
pub use record::{Api, Protocol, RequestRecord, RequestTiming};
pub use recorder::Exporter;
pub use rows::{FailureCode, ReqDetail, TcpRow, TlsRow};
pub use snapshot::{
    ApiStats, ByteStats, CacheStats, ClientStats, ColdStartStats, ConnStats, EffectiveKnobs,
    IntegrityStats, LatencyHistogram, OpBreakdown, OpenLoopStats, Phase, Quantiles,
    RequestStageMeans, ResilienceStats, SelfObservation, SlowOp, Snapshot, StageStats, Summary,
    TimelinePoint, VerbInterval, VerbStats,
};

internal_use!(file::{ServiceRecord, TRACE_FORMAT, TraceFile});
internal_use!(layer::{InProcessObserveLayer, TraceContextLayer});
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
    exporter: Option<Arc<dyn Exporter>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_rows: 10_000,
            transport: net::TransportConfig::default(),
            https: true,
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

    /// Call `e` for every recorded request and operation (metrics, logs, your own sink).
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

/// The OpenDAL layer of [`layer`]: the instrumented HTTP transport and scope propagation for
/// HTTP services, one record per backend call for in-process services.
#[derive(Clone)]
pub struct TraceLayer {
    recorder: Arc<Recorder>,
    context: Option<TraceContextLayer>,
    service: Arc<std::sync::OnceLock<file::ServiceRecord>>,
}

impl std::fmt::Debug for TraceLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TraceLayer")
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
        self.service.get_or_init(|| file::ServiceRecord {
            scheme: info.scheme().to_owned(),
            name: info.name().to_string(),
            root: info.root().to_string(),
        });
        if in_process(info.scheme()) {
            InProcessObserveLayer::new(self.recorder.clone()).apply_service(srv)
        } else {
            srv
        }
    }

    fn apply_context(
        &self,
        srv: opendal_core::raw::Servicer,
        inner: opendal_core::OperationContext,
    ) -> opendal_core::OperationContext {
        match &self.context {
            Some(c) => c.apply_context(srv, inner),
            None => inner,
        }
    }
}

/// Reads what the layer recorded. Cheap to clone; every clone reads the same recorder.
#[derive(Clone)]
pub struct Handle {
    recorder: Arc<Recorder>,
    transport_error: Option<Arc<str>>,
    service: Arc<std::sync::OnceLock<file::ServiceRecord>>,
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
        self.recorder.snapshot()
    }

    /// Write a `.oprobe` profile to `path`: the aggregates so far plus the sampled operations
    /// with their requests (the request waterfall). The rows are moved out of memory, so
    /// successive saves hold the rows recorded since the previous one; the aggregates are
    /// cumulative. Open the file with `oprobe show FILE` or in the workbench.
    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let snapshot = self.snapshot();
        let rows = self.recorder.take_trace();
        file::write(path.as_ref(), self.service.get(), &snapshot, Some(&rows))
    }

    /// Why HTTP requests are not instrumented (the transport could not be built, for example
    /// an invalid proxy URL); only in-process services are observed then.
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
    if let Some(e) = cfg.exporter {
        recorder.set_exporter(e);
    }
    let (context, transport_error) =
        match net::FidelityTransport::new(cfg.transport, cfg.https, Some(recorder.clone())) {
            Ok(t) => (Some(TraceContextLayer::new(t)), None),
            Err(e) => (None, Some(Arc::from(format!("{e:#}")))),
        };
    let service = Arc::new(std::sync::OnceLock::new());
    (
        TraceLayer {
            recorder: recorder.clone(),
            context,
            service: service.clone(),
        },
        Handle {
            recorder,
            transport_error,
            service,
        },
    )
}
