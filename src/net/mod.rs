//! Networking: TLS, proxy tunnelling, instrumented dialing, the tracing HTTP transport and
//! the stand-alone handshake probe.

pub mod dial;
pub mod probe;
pub mod proxy;
pub mod tls;
pub mod transport;

pub use proxy::ProxyConfig;
pub use tls::TlsClientOptions;
pub use transport::{HttpVersion, TracingTransport, TransportConfig};
