//! oprobe-core — the engine of oprobe, a white-box microscope for cloud object storage built on
//! Apache OpenDAL.
//!
//! Every backend request issued through OpenDAL is traced at the transport level
//! ([`oprobe_trace`]: queue wait, DNS/TCP/proxy/TLS on fresh connections, request send,
//! time-to-first-byte, transfer and consumer stall) and attributed to the logical operation
//! that caused it. On top of that: bottleneck diagnosis, a measured cost model, a closed-loop
//! tuner and a CI regression gate.
//!
//! Front ends call the service layer, [`app::App`]; the other modules are its building blocks.

pub mod analysis;
pub mod app;
pub mod clients;
pub mod compat;
pub mod config;
pub mod distributed;
pub mod engine;
pub mod errors;
pub mod events;
pub mod export;
pub mod guard;
pub mod i18n;
pub mod input;
pub mod lab;
pub mod layers;
pub mod model;
pub mod output;
pub mod report;
pub mod service_options;
pub mod store;
pub mod tune;
pub mod util;

/// The instrumentation (re-exported): the recorder, op scopes, records and aggregates.
pub use oprobe_trace as trace;
/// Networking (re-exported from `oprobe-trace`): TLS, proxies, the fidelity and microscope
/// transports, the handshake probe.
pub use oprobe_trace::net;
