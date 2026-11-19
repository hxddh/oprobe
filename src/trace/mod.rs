//! Request-level tracing: raw records, per-operation scopes, aggregation and snapshots.

pub mod model;
pub mod recorder;
pub mod scope;
pub mod snapshot;

pub use model::{classify_http, Api, BillingClass, Protocol, RequestRecord, RequestTiming};
pub use recorder::{DialRecord, OpCompletion, Recorder};
pub use scope::{OpScope, ScopePropagatingExecutor};
pub use snapshot::*;
