//! Remote test-data cleanup: per-run teardown with verification, and a sweeper for
//! leftovers of interrupted runs (`oprobe cleanup`).

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use futures::TryStreamExt;
use opendal::{EntryMode, Operator};
use serde::{Deserialize, Serialize};

pub const RUN_PREFIX: &str = "oprobe-run-";
pub const DATASET_PREFIX: &str = "oprobe-dataset/";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CleanupSummary {
    pub enabled: bool,
    pub prefix: String,
    pub deleted: u64,
    pub remaining: u64,
    pub duration_ms: f64,
    /// Re-listed after deletion and found nothing left.
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_dataset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn new_run_prefix() -> String {
    let tag: u16 = rand::random();
    format!("{RUN_PREFIX}{}-{tag:04x}/", crate::util::compact_utc_now())
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
            op.delete_iter(files).await?;
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
        s.error = Some(format!("{e:#}"));
    }
    s.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    s
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SweepEntry {
    pub prefix: String,
    pub age_s: Option<u64>,
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
        let age = prefix
            .strip_prefix(RUN_PREFIX)
            .and_then(|r| r.get(..16))
            .and_then(crate::util::parse_compact_utc)
            .and_then(|t| now.duration_since(t).ok());
        if let Some(a) = age {
            if a < older_than {
                continue;
            }
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
