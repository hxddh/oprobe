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

/// Short, stable error class for aggregation (`rate_limited`, `not_found`, `timeout`, ...).
pub fn error_class(e: &anyhow::Error) -> String {
    if let Some(oe) = e.downcast_ref::<opendal::Error>() {
        let msg = oe.to_string().to_ascii_lowercase();
        if msg.contains("timeout") || msg.contains("timed out") {
            return "timeout".into();
        }
        if msg.contains("connect:") {
            return "connect".into();
        }
        return match oe.kind() {
            opendal::ErrorKind::NotFound => "not_found",
            opendal::ErrorKind::PermissionDenied => "permission_denied",
            opendal::ErrorKind::RateLimited => "rate_limited",
            opendal::ErrorKind::ConfigInvalid => "config_invalid",
            opendal::ErrorKind::Unsupported => "unsupported",
            opendal::ErrorKind::ConditionNotMatch => "condition_not_match",
            _ => {
                if msg.contains("503") || msg.contains("slowdown") || msg.contains("429") {
                    "rate_limited"
                } else if msg.contains("send request") || msg.contains("response body") {
                    "network"
                } else {
                    "unexpected"
                }
            }
        }
        .into();
    }
    "other".into()
}
