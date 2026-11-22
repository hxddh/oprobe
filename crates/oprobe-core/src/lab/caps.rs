//! Exposure limits of the lab server (design v5 §5, S4): loopback-only binding unless allowed,
//! and caps on request bodies, the in-memory store and open connections, so a lab reachable by
//! others cannot be used to exhaust the machine's memory.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use http::{Method, StatusCode};

use crate::errors::{ErrorCode, OprobeError};

/// Default largest object body accepted (PUT or one multipart part): 1 GiB.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 1 << 30;
/// Default largest store (objects plus staged multipart parts): 8 GiB.
pub const DEFAULT_MAX_STORE_BYTES: u64 = 8 << 30;
/// Default most open connections at once (others are closed at accept).
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;
/// Largest body of a request that carries no object data (batch delete, complete multipart).
pub const MAX_CONTROL_BODY_BYTES: u64 = 4 << 20;

/// How much a lab server accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabLimits {
    /// Bind addresses other than loopback are accepted (`oprobe lab --allow-remote`).
    pub allow_remote: bool,
    pub max_object_bytes: u64,
    pub max_store_bytes: u64,
    pub max_connections: usize,
}

impl Default for LabLimits {
    fn default() -> Self {
        Self {
            allow_remote: false,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_store_bytes: DEFAULT_MAX_STORE_BYTES,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

/// Refuse to serve on `addr` unless it is a loopback address or remote access is allowed.
pub fn check_bind(addr: SocketAddr, allow_remote: bool) -> anyhow::Result<()> {
    if addr.ip().is_loopback() || allow_remote {
        return Ok(());
    }
    Err(OprobeError::new(
        ErrorCode::SafetyRefused,
        format!(
            "refusing to serve the lab on non-loopback address {addr}: it has no authentication"
        ),
    )
    .with_hint("bind 127.0.0.1:PORT, or pass --allow-remote together with --bind to expose it")
    .into())
}

/// Bytes held by the store, estimated cheaply: the last exact count plus every body accepted
/// since. The exact count (a scan) is taken only when the estimate nears the cap, so deletes
/// are accounted for lazily and the common path costs one atomic add.
#[derive(Debug, Default)]
pub(super) struct StoreUsage {
    base: AtomicU64,
    pending: AtomicU64,
}

impl StoreUsage {
    fn estimate(&self) -> u64 {
        self.base
            .load(Ordering::Relaxed)
            .saturating_add(self.pending.load(Ordering::Relaxed))
    }

    fn reset(&self, exact: u64) {
        self.base.store(exact, Ordering::Relaxed);
        self.pending.store(0, Ordering::Relaxed);
    }

    fn add(&self, n: u64) {
        self.pending.fetch_add(n, Ordering::Relaxed);
    }
}

/// Exact bytes of the objects and staged multipart parts.
fn scan(store: &super::S3Store) -> u64 {
    let objects: u64 = store
        .objects
        .read()
        .map(|m| m.values().map(|o| o.data.len() as u64).sum())
        .unwrap_or(0);
    let staged: u64 = store
        .uploads
        .read()
        .map(|m| {
            m.values()
                .flat_map(|u| u.parts.values())
                .map(|p| p.len() as u64)
                .sum()
        })
        .unwrap_or(0);
    objects + staged
}

/// Why a request is refused before it is read, as `(status, S3 code, message)`.
pub(super) fn admit(
    store: &super::S3Store,
    method: &Method,
    headers: &http::HeaderMap,
) -> Option<(StatusCode, &'static str, String)> {
    let limits = &store.caps;
    // PutObject and UploadPart carry object data; POSTs (batch delete, multipart create and
    // complete) only small XML documents.
    let carries_object = *method == Method::PUT && headers.get("x-amz-copy-source").is_none();
    let control = *method == Method::POST;
    if !carries_object && !control {
        return None;
    }
    let len = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let Some(len) = len else {
        // A PUT of unknown length could stream without bound (S3 answers the same).
        return carries_object.then(|| {
            (
                StatusCode::LENGTH_REQUIRED,
                "MissingContentLength",
                "You must provide the Content-Length HTTP header.".to_owned(),
            )
        });
    };
    let max = if carries_object {
        limits.max_object_bytes
    } else {
        MAX_CONTROL_BODY_BYTES
    };
    if len > max {
        return Some((
            StatusCode::BAD_REQUEST,
            "EntityTooLarge",
            format!("Your proposed upload exceeds the maximum allowed size ({max} bytes)."),
        ));
    }
    if !carries_object {
        return None;
    }
    let usage = &store.usage;
    if usage.estimate().saturating_add(len) > limits.max_store_bytes {
        usage.reset(scan(store));
        if usage.estimate().saturating_add(len) > limits.max_store_bytes {
            return Some((
                StatusCode::INSUFFICIENT_STORAGE,
                "LabStoreFull",
                format!(
                    "The lab store is full ({} bytes held, limit {} bytes): delete objects or restart it with a larger --max-store-size.",
                    usage.estimate(),
                    limits.max_store_bytes
                ),
            ));
        }
    }
    usage.add(len);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_is_loopback_unless_allowed() {
        let lo: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let lo6: SocketAddr = "[::1]:9000".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:9000".parse().unwrap();
        check_bind(lo, false).unwrap();
        check_bind(lo6, false).unwrap();
        let e = check_bind(any, false).unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::SafetyRefused
        );
        check_bind(any, true).unwrap();
    }
}
