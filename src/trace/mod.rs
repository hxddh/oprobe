//! Request-level tracing: raw records, per-operation scopes, aggregation and snapshots.

pub mod log;
pub mod model;
pub mod recorder;
pub mod scope;
pub mod snapshot;

pub use log::{DEFAULT_MAX_REQUESTS, TraceLog, TraceRows};
pub use model::{Api, BillingClass, Protocol, RequestRecord, RequestTiming, classify_http};
pub use recorder::{DialRecord, InflightGuard, OpCompletion, Recorder};
pub use scope::{OpScope, ScopePropagatingExecutor};
pub use snapshot::*;
