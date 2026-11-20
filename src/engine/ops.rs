//! Execution of individual logical operations against the instrumented operator.

use std::ops::Range;
use std::sync::Arc;

use anyhow::Result;
use futures::TryStreamExt;
use opendal::{Buffer, Operator};
use rand::Rng;

use super::dataset::{Dataset, PayloadPool, Picker};
use crate::config::{Knobs, OpKind};
use crate::trace::Recorder;

/// Shared context for workers.
pub struct OpCtx {
    pub op: Operator,
    pub dataset: Arc<Dataset>,
    pub picker: Arc<Picker>,
    pub knobs: Knobs,
    pub pool: Arc<PayloadPool>,
    pub verify: bool,
    pub recorder: Arc<Recorder>,
    pub scratch_prefix: String,
    /// Sampled written objects to verify after the run: (path, crc).
    pub written: Arc<std::sync::Mutex<Vec<(String, u32)>>>,
}

/// Outcome of one logical operation.
pub struct OpOutcome {
    pub logical_read: u64,
    pub logical_write: u64,
}

pub fn verb(kind: &OpKind) -> &'static str {
    match kind {
        OpKind::Read { size: None } => "read.full",
        OpKind::Read { size: Some(_) } => "read.range",
        OpKind::ParquetScan { .. } => "parquet.scan",
        OpKind::Write { .. } => "write",
        OpKind::MultipartWrite { .. } => "write.multipart",
        OpKind::Stat => "stat",
        OpKind::List { .. } => "list",
    }
}

fn crc_of(buf: &Buffer) -> u32 {
    buf.clone()
        .fold(0u32, |acc, chunk| crate::util::crc32c_append(acc, &chunk))
}

fn aligned_offset(rng: &mut impl Rng, max: u64) -> u64 {
    if max == 0 {
        0
    } else {
        rng.gen_range(0..=max) & !4095
    }
}

/// Execute `kind`. `slot` selects the scratch key for writes (bounded footprint via overwrite).
pub async fn execute(
    ctx: &OpCtx,
    kind: &OpKind,
    rng: &mut (impl Rng + Send),
    slot: u64,
) -> Result<OpOutcome> {
    let k = &ctx.knobs;
    let chunk = k.chunk_size.as_usize();
    let io = k.io_concurrency.max(1);
    let ds = &ctx.dataset;
    let pick = |rng: &mut (dyn rand::RngCore + Send)| -> Result<&super::dataset::DataObject> {
        if ds.objects.is_empty() {
            anyhow::bail!("dataset has no objects to read");
        }
        Ok(&ds.objects[ctx.picker.pick(rng)])
    };
    match kind {
        OpKind::Read { size } => {
            let obj = pick(rng)?;
            let full = size.map(|s| s.0 >= obj.size).unwrap_or(true);
            let range: Range<u64> = if full {
                0..obj.size
            } else {
                let len = size.expect("range size").0;
                let off = aligned_offset(rng, obj.size - len);
                off..off + len
            };
            let reader = ctx
                .op
                .reader_with(&obj.path)
                .chunk(chunk)
                .concurrent(io)
                .await?;
            let buf = reader.read(range).await?;
            if full
                && ctx.verify
                && let Some(expected) = obj.crc
            {
                ctx.recorder.record_integrity(crc_of(&buf) == expected);
            }
            Ok(OpOutcome {
                logical_read: buf.len() as u64,
                logical_write: 0,
            })
        }
        OpKind::ParquetScan {
            columns,
            column_size,
            footer_size,
        } => {
            let obj = pick(rng)?;
            let meta = ctx.op.stat(&obj.path).await?;
            let size = meta.content_length();
            let footer = footer_size.0.min(size);
            let reader = ctx
                .op
                .reader_with(&obj.path)
                .chunk(chunk)
                .gap(k.range_gap.as_usize())
                .concurrent(io)
                .await?;
            let f = reader.read(size - footer..size).await?;
            let mut logical = f.len() as u64;
            let body = size - footer;
            let col = column_size.0.min(body.max(1));
            // One chunk per column, each in its own stripe of the file (column chunks never overlap).
            let n = (*columns as u64).max(1);
            let stripe = body / n;
            let ranges: Vec<Range<u64>> = (0..n)
                .filter(|_| stripe > 0)
                .map(|i| {
                    let len = col.min(stripe);
                    let off = i * stripe + aligned_offset(rng, stripe - len);
                    off..(off + len).min(body)
                })
                .filter(|r| r.end > r.start)
                .collect();
            if !ranges.is_empty() {
                let chunks = reader.fetch(ranges).await?;
                logical += chunks.iter().map(|c| c.len() as u64).sum::<u64>();
            }
            Ok(OpOutcome {
                logical_read: logical,
                logical_write: 0,
            })
        }
        OpKind::Write { size } => {
            let path = format!("{}w-{slot:05}.bin", ctx.scratch_prefix);
            let data = ctx.pool.slice(rng, size.as_usize());
            let len = data.len() as u64;
            let crc = (ctx.verify && slot.is_multiple_of(16)).then(|| crate::util::crc32c(&data));
            ctx.op.write(&path, data).await?;
            if let Some(crc) = crc {
                push_written(ctx, path, crc);
            }
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: len,
            })
        }
        OpKind::MultipartWrite { size } => {
            let path = format!("{}mp-{slot:05}.bin", ctx.scratch_prefix);
            let data = ctx.pool.slice(rng, size.as_usize());
            let len = data.len() as u64;
            let crc = (ctx.verify && slot.is_multiple_of(8)).then(|| crate::util::crc32c(&data));
            let mut w = ctx
                .op
                .writer_with(&path)
                .chunk(chunk)
                .concurrent(io)
                .await?;
            let mut off = 0;
            while off < data.len() {
                let end = (off + chunk).min(data.len());
                w.write(data.slice(off..end)).await?;
                off = end;
            }
            w.close().await?;
            if let Some(crc) = crc {
                push_written(ctx, path, crc);
            }
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: len,
            })
        }
        OpKind::Stat => {
            let obj = pick(rng)?;
            ctx.op.stat(&obj.path).await?;
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: 0,
            })
        }
        OpKind::List { recursive } => {
            let parts = &ds.partitions;
            let prefix = if parts.is_empty() {
                ds.prefix.clone()
            } else {
                parts[rng.gen_range(0..parts.len())].clone()
            };
            let mut lister = ctx.op.lister_with(&prefix).recursive(*recursive).await?;
            while lister.try_next().await?.is_some() {}
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: 0,
            })
        }
    }
}

fn push_written(ctx: &OpCtx, path: String, crc: u32) {
    let mut w = ctx.written.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = w.iter_mut().find(|(p, _)| *p == path) {
        slot.1 = crc;
    } else if w.len() < 256 {
        w.push((path, crc));
    }
}

/// What a failure's source chain says about where it happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    Timeout,
    Connect,
}

/// Inspect the typed errors under an OpenDAL error: reqwest's (default transport), oprobe's
/// dial errors (`--microscope`), hyper's and I/O errors. No message text is matched.
fn source_cause(e: &opendal::Error) -> Option<Cause> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(err) = cur {
        if let Some(r) = err.downcast_ref::<reqwest::Error>() {
            if r.is_timeout() {
                return Some(Cause::Timeout);
            }
            if r.is_connect() {
                return Some(Cause::Connect);
            }
        }
        if err.downcast_ref::<crate::net::dial::DialError>().is_some() {
            return Some(Cause::Connect);
        }
        if let Some(h) = err.downcast_ref::<hyper::Error>()
            && h.is_timeout()
        {
            return Some(Cause::Timeout);
        }
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::TimedOut => return Some(Cause::Timeout),
                std::io::ErrorKind::ConnectionRefused => return Some(Cause::Connect),
                _ => {}
            }
        }
        cur = err.source();
    }
    None
}

/// The request never got a connection (refused, unreachable, DNS, dial timeout).
pub fn is_connect_failure(e: &opendal::Error) -> bool {
    source_cause(e) == Some(Cause::Connect)
}

/// Short, stable error class for aggregation (`rate_limited`, `not_found`, `timeout`, ...).
///
/// Every OpenDAL 0.59 [`opendal::ErrorKind`] maps to its own class. `Unexpected` errors are
/// refined from `last_status` (the HTTP status of the operation's last backend request, when
/// one got a response), the typed source chain (timeouts and connection failures from reqwest,
/// hyper, oprobe's dialer and I/O), the exact messages of OpenDAL's own timeout and tail-cut
/// layers, and [`opendal::Error::is_temporary`]. Substrings of the formatted error are never
/// matched: object names (`obj-00429.bin`), dates and ports contain digits and words too.
pub fn error_class(e: &anyhow::Error, last_status: Option<u16>) -> String {
    use opendal::ErrorKind as K;
    let Some(oe) = e.downcast_ref::<opendal::Error>() else {
        return "other".into();
    };
    let class = match oe.kind() {
        K::RateLimited => "rate_limited",
        K::PermissionDenied => "permission_denied",
        K::NotFound => "not_found",
        K::ConfigInvalid => "config_invalid",
        K::Unsupported => "unsupported",
        K::IsADirectory => "is_a_directory",
        K::NotADirectory => "not_a_directory",
        K::AlreadyExists => "already_exists",
        K::IsSameFile => "is_same_file",
        K::ConditionNotMatch => "condition_not_match",
        K::RangeNotSatisfied => "range_not_satisfied",
        K::Conflict => "conflict",
        K::Unexpected => {
            return match (last_status, source_cause(oe), oe.message()) {
                (Some(429 | 503), ..) => "rate_limited",
                (Some(401 | 403), ..) => "permission_denied",
                // OpenDAL's TimeoutLayer / TailCutLayer errors (exact messages, set by OpenDAL).
                (_, _, "operation timeout reached" | "io operation timeout reached") => "timeout",
                (_, _, "cancelled by tail cut" | "io cancelled by tail cut") => "tail_cut",
                (_, Some(Cause::Timeout), _) => "timeout",
                (_, Some(Cause::Connect), _) => "connect",
                (Some(s), ..) if s >= 500 => "server_error",
                // A transient transport failure (reset, broken body, ...): OpenDAL retries these.
                _ if oe.is_temporary() => "network",
                _ => "unexpected",
            }
            .into();
        }
        // `ErrorKind` is non-exhaustive: a kind added by a later OpenDAL keeps its own name.
        other => return other.into_static().to_ascii_lowercase().replace(' ', "_"),
    };
    class.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(kind: opendal::ErrorKind, msg: &str) -> anyhow::Error {
        opendal::Error::new(kind, msg).into()
    }

    #[test]
    fn error_class_ignores_digits_in_paths() {
        use opendal::ErrorKind::*;
        // An object name that looks like a status code is not throttling or auth.
        let e = err(
            Unexpected,
            "read failed: path oprobe-run-x/data/obj-00429.bin port 4030",
        );
        // The status the request got decides, never the digits in the text.
        assert_eq!(error_class(&e, Some(500)), "server_error");
        let e = err(Unexpected, "obj-00503.bin 2026-04-03T401");
        assert_eq!(error_class(&e, None), "unexpected");
        assert_eq!(error_class(&err(RateLimited, "x"), None), "rate_limited");
        assert_eq!(
            error_class(&err(Unexpected, "x"), Some(503)),
            "rate_limited"
        );
        assert_eq!(
            error_class(&err(Unexpected, "x"), Some(403)),
            "permission_denied"
        );
        assert_eq!(
            error_class(&err(PermissionDenied, "x"), None),
            "permission_denied"
        );
        assert_eq!(error_class(&err(NotFound, "x"), Some(404)), "not_found");
    }

    #[test]
    fn every_error_kind_has_its_own_class() {
        use opendal::ErrorKind::*;
        for (kind, class) in [
            (Unsupported, "unsupported"),
            (ConfigInvalid, "config_invalid"),
            (NotFound, "not_found"),
            (PermissionDenied, "permission_denied"),
            (IsADirectory, "is_a_directory"),
            (NotADirectory, "not_a_directory"),
            (AlreadyExists, "already_exists"),
            (RateLimited, "rate_limited"),
            (IsSameFile, "is_same_file"),
            (ConditionNotMatch, "condition_not_match"),
            (RangeNotSatisfied, "range_not_satisfied"),
            (Conflict, "conflict"),
        ] {
            assert_eq!(error_class(&err(kind, "x"), None), class, "{kind:?}");
        }
    }

    #[test]
    fn unexpected_errors_are_refined_structurally() {
        use opendal::ErrorKind::Unexpected;
        // OpenDAL's own layers.
        assert_eq!(
            error_class(&err(Unexpected, "operation timeout reached"), None),
            "timeout"
        );
        assert_eq!(
            error_class(&err(Unexpected, "io cancelled by tail cut"), None),
            "tail_cut"
        );
        // A path that merely mentions a timeout is not one.
        let e = err(Unexpected, "read obj-timed out-timeout.bin");
        assert_eq!(error_class(&e, None), "unexpected");
        // Typed sources.
        let io = |k: std::io::ErrorKind| -> anyhow::Error {
            opendal::Error::new(Unexpected, "send request")
                .set_source(std::io::Error::new(k, "boom"))
                .into()
        };
        assert_eq!(error_class(&io(std::io::ErrorKind::TimedOut), None), "timeout");
        assert_eq!(
            error_class(&io(std::io::ErrorKind::ConnectionRefused), None),
            "connect"
        );
        let dial: anyhow::Error = opendal::Error::new(Unexpected, "connect")
            .set_source(crate::net::dial::DialError {
                stage: "tcp",
                target: "h:1".into(),
                source: std::io::Error::other("unreachable"),
            })
            .into();
        assert_eq!(error_class(&dial, None), "connect");
        // Server errors, then transient vs persistent.
        assert_eq!(error_class(&err(Unexpected, "x"), Some(500)), "server_error");
        let temp: anyhow::Error = opendal::Error::new(Unexpected, "x").set_temporary().into();
        assert_eq!(error_class(&temp, None), "network");
        assert_eq!(error_class(&err(Unexpected, "x"), None), "unexpected");
        assert_eq!(error_class(&anyhow::anyhow!("plain"), None), "other");
    }
}
