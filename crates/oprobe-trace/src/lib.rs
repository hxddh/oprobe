//! `oprobe-trace`: the instrumentation of [oprobe](https://github.com/hxddh/oprobe), for any
//! Apache OpenDAL operator.
//!
//! Add one layer and every backend request is timed by stage (queue, DNS, TCP, proxy, TLS,
//! send, time to first byte, body, consumer stall), classified by storage API and aggregated
//! in bounded memory. Read the aggregates whenever you like:
//!
//! ```
//! # async fn demo() -> opendal_core::Result<()> {
//! use opendal_core::Operator;
//! use opendal_core::services::Memory;
//!
//! let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
//! let op = Operator::new(Memory::default())?.layer(layer);
//! op.write("hello.txt", "hi").await?;
//! let _ = op.read("hello.txt").await?;
//!
//! let snapshot = handle.snapshot();
//! let requests: u64 = snapshot.apis.iter().map(|a| a.count).sum();
//! assert!(requests >= 2);
//! # Ok(()) }
//! ```
//!
//! For HTTP services the layer installs oprobe's *fidelity* transport: OpenDAL's own reqwest
//! client, observed request by request (phase timing, connection reuse, TLS and kernel TCP
//! statistics). For in-process services (`memory`, `fs`) it records one request per backend
//! call. With the `otel` feature, [`otel::OtelExporter`] emits OpenTelemetry metrics
//! (`http.client.request.duration` with `oprobe.stage` and `oprobe.api`).
//!
//! The recorder is always on: there is no measurement window to open, requests outside any
//! operation scope are aggregated and sampled like the others, and memory stays bounded
//! ([`Config::max_rows`]).

pub mod dims;
pub mod error;
pub mod layer;
pub mod log;
pub mod net;
#[cfg(feature = "otel")]
pub mod otel;
pub mod record;
pub mod recorder;
pub mod rows;
pub mod sample;
pub mod scope;
pub mod snapshot;
pub mod util;

use std::sync::Arc;

pub use layer::{InProcessObserveLayer, TraceContextLayer};
pub use log::{DEFAULT_MAX_REQUESTS, TraceLog, TraceRows};
pub use record::{Api, BillingClass, Protocol, RequestRecord, RequestTiming, classify_http};
pub use recorder::{Budget, DialRecord, Exporter, InflightGuard, OpCompletion, Recorder, RowSink};
pub use rows::{OpBreakdownUs, OpRow, ReqDetail, ReqRow, TcpRow, TlsRow, TraceInfo, WireProtocol};
pub use sample::{OpSampler, SampleConfig, SamplingStats};
pub use scope::{OpScope, ScopePropagatingExecutor};
pub use snapshot::*;

/// How [`layer`] instruments an operator.
#[derive(Clone)]
pub struct Config {
    /// Trace rows kept (operations plus requests; whole operations are sampled beyond it).
    /// Aggregates are exact whatever this is; 0 keeps aggregates only.
    pub max_rows: usize,
    /// The HTTP transport's settings (HTTP version, keep-alive, TLS, proxy, in-flight cap).
    pub transport: net::TransportConfig,
    /// The endpoint is HTTPS (selects `HTTPS_PROXY` over `HTTP_PROXY` from the environment).
    pub https: bool,
    /// Called for every recorded request and operation (metrics, logs, your own sink).
    pub exporter: Option<Arc<dyn Exporter>>,
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
            .field("https", &self.https)
            .field("exporter", &self.exporter.is_some())
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn with_exporter(mut self, e: Arc<dyn Exporter>) -> Self {
        self.exporter = Some(e);
        self
    }
}

/// The OpenDAL layer of [`layer`]: the instrumented HTTP transport and scope propagation for
/// HTTP services, one record per backend call for in-process services.
#[derive(Clone)]
pub struct TraceLayer {
    recorder: Arc<Recorder>,
    context: Option<TraceContextLayer>,
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
        if in_process(srv.info().scheme()) {
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

/// Reads what the layer recorded.
#[derive(Clone)]
pub struct Handle {
    recorder: Arc<Recorder>,
    transport_error: Option<Arc<str>>,
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

    /// Take the sampled rows (whole operations with their requests) and start afresh.
    pub fn take_rows(&self) -> TraceRows {
        self.recorder.take_trace()
    }

    /// The recorder itself (op scopes, phases, budgets).
    pub fn recorder(&self) -> &Arc<Recorder> {
        &self.recorder
    }

    /// Why HTTP requests are not instrumented (the transport could not be built; only
    /// in-process services are observed then).
    pub fn transport_error(&self) -> Option<&str> {
        self.transport_error.as_deref()
    }
}

/// Instrument an operator: `op.layer(layer)`, then `handle.snapshot()`.
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
    (
        TraceLayer {
            recorder: recorder.clone(),
            context,
        },
        Handle {
            recorder,
            transport_error,
        },
    )
}
