//! Pure-Rust white-box HTTP/1.1 & HTTP/2 transport stack built directly on `hyper`,
//! `hyper-util`, and `tokio-rustls` to eliminate black-box client blind spots.

pub mod connector;
pub mod hyper_transport;

pub use connector::{ConnectionTelemetry, ProbedHttpConnector};
pub use hyper_transport::{HyperTransportOptions, HyperWhiteboxTransport};
