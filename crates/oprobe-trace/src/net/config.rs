//! Settings of the instrumented HTTP transport ([`super::FidelityTransport`]). The pool keeps
//! reqwest's (= OpenDAL's) defaults: unlimited idle connections per host, 90 s idle timeout,
//! no connect timeout.

use serde::{Deserialize, Serialize};

use super::tls::TlsClientOptions;

/// HTTP protocol preference.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum HttpVersion {
    /// Negotiate via ALPN (h2 preferred for https), HTTP/1.1 for plain http.
    #[default]
    Auto,
    /// Force HTTP/1.1.
    H1,
    /// Require HTTP/2 (ALPN for https, prior knowledge for plain http).
    H2,
}

#[derive(Clone, Debug)]
pub struct TransportConfig {
    pub http: HttpVersion,
    pub keep_alive: bool,
    /// Cap on concurrent in-flight HTTP requests (None = unlimited).
    pub max_inflight: Option<usize>,
    pub tls: TlsClientOptions,
    /// Explicit proxy; `None` = resolve from environment; `Some(None)` = disabled.
    pub proxy: Option<Option<String>>,
    /// Read the kernel's TCP statistics of the connection at the end of every request
    /// (`net::tcpinfo`; Linux and macOS).
    pub tcp_info: bool,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            http: HttpVersion::Auto,
            keep_alive: true,
            max_inflight: None,
            tls: TlsClientOptions::default(),
            proxy: None,
            tcp_info: true,
        }
    }
}

