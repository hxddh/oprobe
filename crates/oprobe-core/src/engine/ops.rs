//! Execution of individual logical operations against the instrumented operator.

use std::ops::Range;
use std::sync::Arc;

use anyhow::Result;
use futures::TryStreamExt;
use opendal::{Buffer, Operator};
use rand::Rng;

use super::Cancel;
use super::dataset::{Dataset, PayloadPool, Picker};
use super::upload::{Aborts, Upload};
use super::workloads::WorkloadState;
use crate::config::OpKind;
use crate::operator::OperatorSpec;
use crate::trace::Recorder;

/// Shared context for workers.
pub struct OpCtx {
    pub op: Operator,
    pub dataset: Arc<Dataset>,
    pub picker: Arc<Picker>,
    /// The operator under test (read and write options).
    pub spec: OperatorSpec,
    pub pool: Arc<PayloadPool>,
    pub verify: bool,
    pub recorder: Arc<Recorder>,
    pub scratch_prefix: String,
    /// Sampled written objects to verify after the run: (path, crc).
    pub written: Arc<std::sync::Mutex<Vec<(String, u32)>>>,
    /// Service capability and the state of delete / conditional-write operations.
    pub state: WorkloadState,
    /// The session's aborts: every writer an operation opens is an [`Upload`] of these.
    pub aborts: Aborts,
    /// The run's cancellation: an operation it stopped did not fail.
    pub cancel: Cancel,
}

/// Outcome of one logical operation.
pub struct OpOutcome {
    pub logical_read: u64,
    pub logical_write: u64,
}

/// A reader of `path` with the operator's read options (OpenDAL's defaults where unset).
async fn reader(ctx: &OpCtx, path: &str) -> opendal::Result<opendal::Reader> {
    let r = &ctx.spec.read;
    let mut b = ctx.op.reader_with(path);
    if let Some(c) = r.chunk {
        b = b.chunk(c.as_usize());
    }
    if let Some(c) = r.concurrent {
        b = b.concurrent(c);
    }
    if let Some(g) = r.gap {
        b = b.gap(g.as_usize());
    }
    b.await
}

pub fn verb(kind: &OpKind) -> &'static str {
    match kind {
        OpKind::Read { size: None } => "read.full",
        OpKind::Read { size: Some(_) } => "read.range",
        OpKind::ParquetScan { suffix: false, .. } => "parquet.scan",
        OpKind::ParquetScan { suffix: true, .. } => "parquet.scan.suffix",
        OpKind::Write { .. } => "write",
        OpKind::MultipartWrite { .. } => "write.multipart",
        OpKind::Stat => "stat",
        OpKind::List { .. } => "list",
        OpKind::ListPaged { .. } => "list.paged",
        OpKind::Delete { .. } => "delete.batch",
        OpKind::Copy => "copy",
        OpKind::PresignedRead => "read.presigned",
        OpKind::ConditionalWrite { .. } => "write.conditional",
        OpKind::MetadataWrite { .. } => "write.metadata",
    }
}

/// The payload an operation of `kind` writes (`None`: it writes none). Every kind is listed,
/// so a new one cannot be forgotten: the payload pool is sized from this ([`max_payload`]) and
/// the operations take their payload through it ([`payload`]).
pub fn payload_size(kind: &OpKind) -> Option<usize> {
    match kind {
        OpKind::Write { size }
        | OpKind::MultipartWrite { size }
        | OpKind::ConditionalWrite { size }
        | OpKind::MetadataWrite { size, .. } => Some(size.as_usize()),
        OpKind::Read { .. }
        | OpKind::ParquetScan { .. }
        | OpKind::Stat
        | OpKind::List { .. }
        | OpKind::ListPaged { .. }
        | OpKind::Delete { .. }
        | OpKind::Copy
        | OpKind::PresignedRead => None,
    }
}

/// The largest payload any operation of `kinds` writes (0: none writes).
pub fn max_payload<'a>(kinds: impl IntoIterator<Item = &'a OpKind>) -> usize {
    kinds
        .into_iter()
        .filter_map(payload_size)
        .max()
        .unwrap_or(0)
}

/// The payload of one operation of `kind`, all of it.
fn payload(ctx: &OpCtx, kind: &OpKind, rng: &mut impl Rng) -> Result<bytes::Bytes> {
    let len = payload_size(kind).expect("only writing operations take a payload");
    ctx.pool.slice(rng, len)
}

/// Write `data` to `path` through a writer with the operator's write options, in the pieces a
/// streaming application hands it. The upload is aborted unless it completes.
async fn stream_write(ctx: &OpCtx, path: &str, data: bytes::Bytes) -> opendal::Result<()> {
    let write = &ctx.spec.write;
    let mut w = ctx.op.writer_with(path);
    if let Some(c) = write.chunk {
        w = w.chunk(c.as_usize());
    }
    if let Some(c) = write.concurrent {
        w = w.concurrent(c);
    }
    let mut upload = Upload::new(&ctx.op, w.await?, path, &ctx.aborts);
    let piece = write.chunk.map_or(8 << 20, |c| c.as_usize());
    upload.write_in_pieces(data, piece).await?;
    upload.close().await?;
    Ok(())
}

/// Write `data` to `path` in one call through `writer` (opened with the operation's
/// conditions) and complete it. `Operator::write_with` opens the same writer, but inside a
/// future the engine may drop (a cancelled or cut operation): a service that writes through
/// an upload session (a GCS resumable upload, an S3 multipart upload past the single-PUT
/// size) would then keep it. Owned by an [`Upload`], it is aborted unless it completes.
async fn upload_whole(
    ctx: &OpCtx,
    writer: opendal::Writer,
    path: &str,
    data: bytes::Bytes,
) -> opendal::Result<opendal::Metadata> {
    let mut upload = Upload::new(&ctx.op, writer, path, &ctx.aborts);
    upload.write(data).await?;
    upload.close().await
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
/// `key` receives the object key (or listed prefix) as soon as it is chosen, also on failure.
pub async fn execute(
    ctx: &OpCtx,
    kind: &OpKind,
    rng: &mut (impl Rng + Send),
    slot: u64,
    key: &mut String,
) -> Result<OpOutcome> {
    let spec = &ctx.spec;
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
            key.clone_from(&obj.path);
            let full = size.map(|s| s.0 >= obj.size).unwrap_or(true);
            let range: Range<u64> = if full {
                0..obj.size
            } else {
                let len = size.expect("range size").0;
                let off = aligned_offset(rng, obj.size - len);
                off..off + len
            };
            let buf = reader(ctx, &obj.path).await?.read(range).await?;
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
            suffix,
        } => {
            let obj = pick(rng)?;
            key.clone_from(&obj.path);
            let (size, f) = if *suffix {
                // One GET `Range: bytes=-N` and no HEAD: a reader learns the file size from the
                // response's Content-Range (here: from the dataset).
                let f = ctx
                    .op
                    .read_with(&obj.path)
                    .range(opendal::BytesRange::suffix(footer_size.0))
                    .await?;
                (obj.size, Some(f))
            } else {
                (ctx.op.stat(&obj.path).await?.content_length(), None)
            };
            let footer = footer_size.0.min(size);
            let reader = reader(ctx, &obj.path).await?;
            let f = match f {
                Some(f) => f,
                None => reader.read(size - footer..size).await?,
            };
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
        OpKind::Write { .. } => {
            let path = if ctx.state.unique_writes {
                // Deletes consume what writes create: every write makes a new key.
                format!(
                    "{}w-{slot:05}-{}.bin",
                    ctx.scratch_prefix,
                    ctx.state.next_seq()
                )
            } else {
                format!("{}w-{slot:05}.bin", ctx.scratch_prefix)
            };
            key.clone_from(&path);
            let data = payload(ctx, kind, rng)?;
            let len = data.len() as u64;
            let crc = (ctx.verify && slot.is_multiple_of(16)).then(|| crate::util::crc32c(&data));
            if spec.write.is_default() {
                // One request (OpenDAL splits nothing without a chunk size): nothing to abort.
                ctx.op.write(&path, data).await?;
            } else {
                stream_write(ctx, &path, data).await?;
            }
            if ctx.state.unique_writes {
                ctx.state.push_deletable(path.clone());
            } else if let Some(crc) = crc {
                push_written(ctx, path, crc);
            }
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: len,
            })
        }
        OpKind::MultipartWrite { .. } => {
            let path = format!("{}mp-{slot:05}.bin", ctx.scratch_prefix);
            key.clone_from(&path);
            let data = payload(ctx, kind, rng)?;
            let len = data.len() as u64;
            let crc = (ctx.verify && slot.is_multiple_of(8)).then(|| crate::util::crc32c(&data));
            stream_write(ctx, &path, data).await?;
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
            key.clone_from(&obj.path);
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
            key.clone_from(&prefix);
            let mut lister = ctx.op.lister_with(&prefix).recursive(*recursive).await?;
            while lister.try_next().await?.is_some() {}
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: 0,
            })
        }
        OpKind::ListPaged { .. }
        | OpKind::Delete { .. }
        | OpKind::Copy
        | OpKind::PresignedRead
        | OpKind::ConditionalWrite { .. }
        | OpKind::MetadataWrite { .. } => execute_more(ctx, kind, rng, slot, key).await,
    }
}

/// The newer operations (capability-dependent; `workloads::adapt_mix` removed or replaced
/// those the service cannot run).
async fn execute_more(
    ctx: &OpCtx,
    kind: &OpKind,
    rng: &mut (impl Rng + Send),
    slot: u64,
    key: &mut String,
) -> Result<OpOutcome> {
    let ds = &ctx.dataset;
    let pick = |rng: &mut (dyn rand::RngCore + Send)| -> Result<&super::dataset::DataObject> {
        if ds.objects.is_empty() {
            anyhow::bail!("dataset has no objects to read");
        }
        Ok(&ds.objects[ctx.picker.pick(rng)])
    };
    let none = OpOutcome {
        logical_read: 0,
        logical_write: 0,
    };
    match kind {
        OpKind::ListPaged { limit } => {
            let parts = &ds.partitions;
            let prefix = if parts.is_empty() {
                ds.prefix.clone()
            } else {
                parts[rng.gen_range(0..parts.len())].clone()
            };
            key.clone_from(&prefix);
            let mut lister = ctx
                .op
                .lister_with(&prefix)
                .limit((*limit).max(1) as usize)
                .await?;
            while lister.try_next().await?.is_some() {}
            Ok(none)
        }
        OpKind::Delete { batch } => {
            let n = (*batch).max(1) as usize;
            let mut keys = ctx.state.take_deletable(n);
            if keys.is_empty() {
                // Nothing written yet: delete keys that do not exist (same request, billed
                // the same; deleting a missing key succeeds).
                keys = (0..n)
                    .map(|i| format!("{}d-{slot:05}-{i}.bin", ctx.scratch_prefix))
                    .collect();
            }
            key.clone_from(&keys[0]);
            ctx.op.delete_iter(keys).await?;
            Ok(none)
        }
        OpKind::Copy => {
            let obj = pick(rng)?;
            let to = format!("{}c-{slot:05}.bin", ctx.scratch_prefix);
            key.clone_from(&to);
            ctx.op.copy(&obj.path, &to).await?;
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: obj.size,
            })
        }
        OpKind::PresignedRead => {
            let obj = pick(rng)?;
            key.clone_from(&obj.path);
            let signed = ctx
                .op
                .presign_read(&obj.path, std::time::Duration::from_secs(900))
                .await?;
            let mut req = http::Request::builder()
                .method(signed.method().clone())
                .uri(signed.uri().clone())
                // The request row names it: the URL alone looks like any GetObject.
                .extension(opendal::raw::ServiceOperation("PresignedGetObject"))
                .body(Buffer::new())?;
            *req.headers_mut() = signed.header().clone();
            // Through the operator's own (traced) HTTP transport: no signing on this path.
            let resp = ctx.op.context().http_transport().fetch(req).await?;
            let status = resp.status();
            let mut body = resp.into_body();
            let buf = body.to_buffer().await?;
            if !status.is_success() {
                return Err(opendal::Error::new(
                    if status.as_u16() == 404 {
                        opendal::ErrorKind::NotFound
                    } else if status.as_u16() == 403 {
                        opendal::ErrorKind::PermissionDenied
                    } else if matches!(status.as_u16(), 429 | 503) {
                        opendal::ErrorKind::RateLimited
                    } else {
                        opendal::ErrorKind::Unexpected
                    },
                    format!("presigned GET answered {status}"),
                )
                .into());
            }
            if ctx.verify
                && let Some(expected) = obj.crc
            {
                ctx.recorder.record_integrity(crc_of(&buf) == expected);
            }
            Ok(OpOutcome {
                logical_read: buf.len() as u64,
                logical_write: 0,
            })
        }
        OpKind::ConditionalWrite { .. } => {
            let cap = &ctx.state.capability;
            let data = payload(ctx, kind, rng)?;
            let len = data.len() as u64;
            let slot_path = format!("{}cw-{slot:05}.bin", ctx.scratch_prefix);
            let meta = match ctx.state.etag(slot).filter(|e| !e.is_empty()) {
                // Optimistic update of the object this slot wrote last.
                Some(etag) if cap.write_with_if_match => {
                    key.clone_from(&slot_path);
                    let w = ctx.op.writer_with(&slot_path).if_match(&etag);
                    upload_whole(ctx, w.await?, &slot_path, data).await?
                }
                _ => {
                    // Conditional create. On the slot's key when an overwrite can follow
                    // (If-Match); a fresh key when the service only creates conditionally or
                    // the slot's object has no known etag.
                    let fresh = !cap.write_with_if_match || ctx.state.etag(slot).is_some();
                    let path = if fresh {
                        format!(
                            "{}cw-{slot:05}-{}.bin",
                            ctx.scratch_prefix,
                            ctx.state.next_seq()
                        )
                    } else {
                        slot_path.clone()
                    };
                    key.clone_from(&path);
                    let w = if cap.write_with_if_not_exists {
                        ctx.op.writer_with(&path).if_not_exists(true).await?
                    } else {
                        ctx.op.writer_with(&path).if_none_match("*").await?
                    };
                    upload_whole(ctx, w, &path, data).await?
                }
            };
            if *key == slot_path || ctx.state.etag(slot).is_none() {
                ctx.state.set_etag(slot, meta.etag());
            }
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: len,
            })
        }
        OpKind::MetadataWrite { entries, .. } => {
            let path = format!("{}m-{slot:05}.bin", ctx.scratch_prefix);
            key.clone_from(&path);
            let data = payload(ctx, kind, rng)?;
            let len = data.len() as u64;
            let meta: Vec<(String, String)> = (0..*entries)
                .map(|i| (format!("oprobe-{i}"), format!("value-{slot}-{i}")))
                .collect();
            ctx.op
                .write_with(&path, data)
                .content_type("application/octet-stream")
                .user_metadata(meta)
                .await?;
            Ok(OpOutcome {
                logical_read: 0,
                logical_write: len,
            })
        }
        _ => unreachable!("handled by execute"),
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
    match e.downcast_ref::<opendal::Error>() {
        Some(oe) => error_class_of(oe, last_status),
        None => "other".into(),
    }
}

/// [`error_class`] of an OpenDAL error.
pub fn error_class_of(oe: &opendal::Error, last_status: Option<u16>) -> String {
    use opendal::ErrorKind as K;
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

    /// The payload pool is sized from every operation that writes (9.1 forgot conditional and
    /// metadata writes, and the pool cut their payloads short without a word).
    #[test]
    fn the_payload_pool_holds_the_largest_write_of_any_kind() {
        use crate::util::ByteSize;
        let size = ByteSize::mib(3);
        for kind in [
            OpKind::Write { size },
            OpKind::MultipartWrite { size },
            OpKind::ConditionalWrite { size },
            OpKind::MetadataWrite { size, entries: 2 },
        ] {
            let mix = [OpKind::Stat, kind.clone(), OpKind::Copy];
            assert_eq!(max_payload(&mix), size.as_usize(), "{kind:?}");
            let pool = PayloadPool::new(max_payload(&mix));
            let mut rng = rand::rngs::mock::StepRng::new(1, 7);
            let data = pool.slice(&mut rng, payload_size(&kind).unwrap()).unwrap();
            assert_eq!(data.len(), size.as_usize(), "{kind:?}");
        }
        assert_eq!(max_payload(&[OpKind::Stat, OpKind::Copy]), 0);
        // A pool too small for a payload says so instead of handing out less.
        let pool = PayloadPool::new(0);
        let mut rng = rand::rngs::mock::StepRng::new(1, 7);
        let e = pool.slice(&mut rng, 64 << 20).unwrap_err();
        assert!(e.to_string().contains("payload pool"), "{e}");
    }

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
        assert_eq!(
            error_class(&io(std::io::ErrorKind::TimedOut), None),
            "timeout"
        );
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
        assert_eq!(
            error_class(&err(Unexpected, "x"), Some(500)),
            "server_error"
        );
        let temp: anyhow::Error = opendal::Error::new(Unexpected, "x").set_temporary().into();
        assert_eq!(error_class(&temp, None), "network");
        assert_eq!(error_class(&err(Unexpected, "x"), None), "unexpected");
        assert_eq!(error_class(&anyhow::anyhow!("plain"), None), "other");
    }
}
