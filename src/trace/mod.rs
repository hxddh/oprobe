//! Request-level tracing: raw records, per-operation scopes, aggregation and snapshots.

pub mod model;
pub mod recorder;
pub mod scope;
pub mod snapshot;

pub use model::{Api, BillingClass, Protocol, RequestRecord, RequestTiming, classify_http};
pub use recorder::{DialRecord, InflightGuard, OpCompletion, Recorder};
pub use scope::{OpScope, ScopePropagatingExecutor};
pub use snapshot::*;
