//! **Internal: no semver promise.** oprobe-core is the engine of the `oprobe` binary (a
//! profiler for OpenDAL operators): the operator spec, workloads, the profile and its findings,
//! `prove`, the run store and the lab. It is published only so that `cargo install oprobe`
//! works, and `oprobe` pins it with an exact version. To profile your own operator, use
//! [`oprobe_trace`].
//!
//! Front ends call the service layer, [`app::App`]; the other modules are its building blocks.

pub mod analysis;
pub mod app;
pub mod config;
pub mod engine;
pub mod errors;
pub mod events;
pub mod guard;
pub mod input;
pub mod lab;
pub mod model;
pub mod operator;
pub mod output;
pub mod report;
pub mod service_options;
pub mod store;
pub mod tune;
pub mod util;

/// The instrumentation (re-exported): the recorder, op scopes, records and aggregates.
pub use oprobe_trace as trace;
/// Networking (re-exported from `oprobe-trace`): TLS, proxies, the instrumented transport.
pub use oprobe_trace::net;
