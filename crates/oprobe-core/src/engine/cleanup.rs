//! Remote test-data cleanup: per-run teardown with verification, and a sweeper for
//! leftovers of interrupted runs (`oprobe cleanup`).

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use futures::TryStreamExt;
use opendal::{EntryMode, Operator};
use serde::{Deserialize, Serialize};

pub const RUN_PREFIX: &str = "oprobe-run-";
pub const DATASET_PREFIX: &str = "oprobe-dataset/";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CleanupSummary {
    pub enabled: bool,
    pub prefix: String,
    #[serde(rename = "deleted_count")]
    pub deleted: u64,
    #[serde(rename = "remaining_count")]
    pub remaining: u64,
    pub duration_ms: f64,
    /// Re-listed after deletion and found nothing left.
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_dataset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `oprobe-run-<run id>/` for a fresh run id.
pub fn new_run_prefix() -> String {
    crate::model::RunId::new().run_prefix()
}

/// Start time encoded in a run prefix (`oprobe-run-<run id>/` or the 2.x
/// `oprobe-run-<compact utc>-<tag>/`).
fn prefix_started_at(prefix: &str) -> Option<SystemTime> {
    let rest = prefix.strip_prefix(RUN_PREFIX)?;
    rest.get(..20)
        .and_then(crate::model::RunId::parse)
        .and_then(|id| id.started_at())
        .or_else(|| rest.get(..16).and_then(crate::util::parse_compact_utc))
}

async fn list_files(op: &Operator, prefix: &str) -> Result<Vec<String>> {
    let mut lister = op.lister_with(prefix).recursive(true).await?;
    let mut out = Vec::new();
    while let Some(e) = lister.try_next().await? {
        if e.metadata().mode() == EntryMode::FILE {
            out.push(e.path().to_owned());
        }
    }
    Ok(out)
}

/// Delete every object under `prefix` and verify nothing remains.
pub async fn delete_prefix(op: &Operator, prefix: &str) -> CleanupSummary {
    let t0 = Instant::now();
    let mut s = CleanupSummary {
        enabled: true,
        prefix: prefix.to_owned(),
        ..Default::default()
    };
    let res: Result<()> = async {
        for _ in 0..3 {
            let files = list_files(op, prefix).await?;
            if files.is_empty() {
                break;
            }
            let n = files.len() as u64;
            if op.delete_iter(files.clone()).await.is_err() {
                // Batch delete is optional on many S3-compatible stores, proxies and emulators:
                // fall back to one DELETE per object rather than leave test data behind.
                use futures::{StreamExt, TryStreamExt};
                futures::stream::iter(files)
                    .map(|f| async move { op.delete(&f).await })
                    .buffer_unordered(16)
                    .try_collect::<Vec<_>>()
                    .await?;
            }
            s.deleted += n;
        }
        let _ = op.delete_with(prefix).recursive(true).await;
        let left = list_files(op, prefix).await?;
        s.remaining = left.len() as u64;
        s.verified = left.is_empty();
        Ok(())
    }
    .await;
    if let Err(e) = res {
        s.error = Some(crate::util::strip_url_credentials(&format!("{e:#}")));
    }
    s.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    s
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SweepEntry {
    pub prefix: String,
    pub age_s: Option<u64>,
    #[serde(rename = "objects_count")]
    pub objects: u64,
    pub deleted: bool,
}

/// Find (and unless `dry_run`, delete) leftover `oprobe-run-*` prefixes older than `older_than`.
pub async fn sweep(
    op: &Operator,
    older_than: Duration,
    include_datasets: bool,
    dry_run: bool,
) -> Result<Vec<SweepEntry>> {
    let mut lister = op
        .lister("")
        .await
        .context("cannot list the storage root")?;
    let mut candidates = Vec::new();
    while let Some(e) = lister.try_next().await? {
        let p = e.path().to_owned();
        if p.starts_with(RUN_PREFIX) || (include_datasets && p == DATASET_PREFIX) {
            candidates.push(p);
        }
    }
    let now = SystemTime::now();
    let mut out = Vec::new();
    for prefix in candidates {
        let age = prefix_started_at(&prefix).and_then(|t| now.duration_since(t).ok());
        if let Some(a) = age
            && a < older_than
        {
            continue;
        }
        let objects = list_files(op, &prefix)
            .await
            .map(|v| v.len() as u64)
            .unwrap_or(0);
        let mut deleted = false;
        if !dry_run {
            let s = delete_prefix(op, &prefix).await;
            deleted = s.verified;
        }
        out.push(SweepEntry {
            prefix,
            age_s: age.map(|a| a.as_secs()),
            objects,
            deleted,
        });
    }
    Ok(out)
}
