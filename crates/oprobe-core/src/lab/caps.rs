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

/// Bytes held by the store, estimated cheaply and race-freely: the last exact scan plus net
/// committed bytes since that scan plus every body currently streaming in flight. Failed,
/// throttled or aborted uploads drop their [`InflightGuard`] without incrementing `committed`,
/// and concurrent uploads atomically reserve against `max_store_bytes` via CAS on `inflight`.
#[derive(Debug, Default)]
pub(super) struct StoreUsage {
    base_bytes: AtomicU64,
    base_committed: AtomicU64,
    committed: AtomicU64,
    inflight: AtomicU64,
    rescan_lock: std::sync::Mutex<()>,
}

/// Reservation of `len` bytes for an in-flight upload or multipart merge.
/// Dropping without [`Self::commit`] releases the reservation immediately.
pub(super) struct InflightGuard<'a> {
    usage: &'a StoreUsage,
    len: u64,
}

impl InflightGuard<'_> {
    /// Call right after inserting the object or part into the store.
    pub(super) fn commit(mut self) {
        if self.len > 0 {
            self.usage.committed.fetch_add(self.len, Ordering::AcqRel);
            self.usage.inflight.fetch_sub(self.len, Ordering::AcqRel);
            self.len = 0;
        }
    }
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        if self.len > 0 {
            self.usage.inflight.fetch_sub(self.len, Ordering::AcqRel);
        }
    }
}

impl StoreUsage {
    fn current_used(&self, cur_inflight: u64) -> u64 {
        let c = self.committed.load(Ordering::Acquire);
        let bc = self.base_committed.load(Ordering::Acquire);
        let bb = self.base_bytes.load(Ordering::Acquire);
        bb.saturating_add(c.saturating_sub(bc))
            .saturating_add(cur_inflight)
    }

    fn rescan(&self, store: &super::S3Store) {
        let _g = self.rescan_lock.lock().unwrap_or_else(|e| e.into_inner());
        let c_before = self.committed.load(Ordering::Acquire);
        let exact = scan(store);
        self.base_bytes.store(exact, Ordering::Release);
        self.base_committed.store(c_before, Ordering::Release);
    }

    pub(super) fn try_reserve<'a>(
        &'a self,
        store: &super::S3Store,
        len: u64,
        max: u64,
    ) -> Result<InflightGuard<'a>, u64> {
        let mut rescanned = false;
        loop {
            let cur_inf = self.inflight.load(Ordering::Acquire);
            let used = self.current_used(cur_inf);
            if used.saturating_add(len) <= max {
                if self
                    .inflight
                    .compare_exchange_weak(
                        cur_inf,
                        cur_inf.saturating_add(len),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Ok(InflightGuard { usage: self, len });
                }
                continue;
            }
            if rescanned {
                return Err(used);
            }
            self.rescan(store);
            rescanned = true;
        }
    }

    /// Hold already-stored bytes in `inflight` while moving them (e.g. multipart completion
    /// between removing staged parts from `uploads` and inserting the merged object into
    /// `objects`) so a concurrent scan never misses them.
    pub(super) fn hold_inflight(&self, len: u64) -> InflightGuard<'_> {
        if len > 0 {
            self.inflight.fetch_add(len, Ordering::AcqRel);
        }
        InflightGuard { usage: self, len }
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

/// Admit a request before its body is read, returning an [`InflightGuard`] when it carries
/// object bytes, or `(status, S3 code, message)` when refused.
pub(super) fn admit<'a>(
    store: &'a super::S3Store,
    method: &Method,
    headers: &http::HeaderMap,
) -> Result<Option<InflightGuard<'a>>, (StatusCode, &'static str, String)> {
    let limits = &store.caps;
    // PutObject and UploadPart carry object data; POSTs (batch delete, multipart create and
    // complete) only small XML documents.
    let carries_object = *method == Method::PUT && headers.get("x-amz-copy-source").is_none();
    let control = *method == Method::POST;
    if !carries_object && !control {
        return Ok(None);
    }
    let len = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let Some(len) = len else {
        // A PUT of unknown length could stream without bound (S3 answers the same).
        if carries_object {
            return Err((
                StatusCode::LENGTH_REQUIRED,
                "MissingContentLength",
                "You must provide the Content-Length HTTP header.".to_owned(),
            ));
        }
        return Ok(None);
    };
    let max = if carries_object {
        limits.max_object_bytes
    } else {
        MAX_CONTROL_BODY_BYTES
    };
    if len > max {
        return Err((
            StatusCode::BAD_REQUEST,
            "EntityTooLarge",
            format!("Your proposed upload exceeds the maximum allowed size ({max} bytes)."),
        ));
    }
    if !carries_object {
        return Ok(None);
    }
    match store.usage.try_reserve(store, len, limits.max_store_bytes) {
        Ok(guard) => Ok(Some(guard)),
        Err(held) => Err((
            StatusCode::INSUFFICIENT_STORAGE,
            "LabStoreFull",
            format!(
                "The lab store is full ({held} bytes held, limit {} bytes): delete objects or restart it with a larger --max-store-size.",
                limits.max_store_bytes
            ),
        )),
    }
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
