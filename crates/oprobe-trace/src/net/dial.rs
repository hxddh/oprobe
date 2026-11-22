//! Classified dial failures.

use std::io;

/// A connection could not be made: which stage failed (`dns`, `tcp`, `proxy`, `tls`), for which
/// target, and the underlying I/O error.
#[derive(Debug)]
pub struct DialError {
    pub stage: &'static str,
    pub target: String,
    pub source: io::Error,
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed for {}: {}",
            self.stage, self.target, self.source
        )
    }
}

impl std::error::Error for DialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}
