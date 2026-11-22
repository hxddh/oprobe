//! Networking: TLS, proxy tunnelling, instrumented dialing, the fidelity HTTP transport and
//! the stand-alone handshake probe.

pub mod config;
pub mod dial;
pub mod fidelity;
pub mod proxy;
pub mod signals;
pub mod stats;
pub mod tcpinfo;
pub mod tls;
pub mod tlsinfo;

pub use config::{HttpVersion, TransportConfig};
pub use fidelity::FidelityTransport;
pub use proxy::ProxyConfig;
#[allow(unused_imports)]
pub use tls::TlsClientOptions;

/// The transport's settings are invalid (a malformed proxy URL, an unreadable CA bundle). The
/// oprobe CLI reports it as a usage error.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConfigError {
    message: String,
    hint: Option<String>,
}

impl ConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// What to do about it.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// `Retry-After` in milliseconds: delay-seconds (RFC 9110 allows only integers; fractions are
/// accepted) or an HTTP-date, relative to now. Nonsense (negative, non-finite, unparsable)
/// yields `None`; a date in the past yields 0. Capped at one day.
pub(crate) fn parse_retry_after(v: &str) -> Option<u64> {
    const DAY_MS: u64 = 86_400_000;
    let v = v.trim();
    if let Ok(s) = v.parse::<f64>() {
        return (s.is_finite() && s >= 0.0).then(|| ((s * 1000.0) as u64).min(DAY_MS));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    let ms = at
        .duration_since(std::time::SystemTime::now())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(ms.min(DAY_MS))
}

#[cfg(test)]
mod tests {
    use super::parse_retry_after;

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("2"), Some(2000));
        assert_eq!(parse_retry_after(" 0.5 "), Some(500));
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after("inf"), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), Some(0));
        let later = httpdate::fmt_http_date(
            std::time::SystemTime::now() + std::time::Duration::from_secs(30),
        );
        let ms = parse_retry_after(&later).unwrap();
        assert!((28_000..=30_000).contains(&ms), "{ms}");
    }
}
