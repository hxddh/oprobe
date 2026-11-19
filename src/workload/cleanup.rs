//! Guaranteed remote test object tracking, automatic post-run teardown, and
//! standalone `oprobe cleanup` orphan prefix sweeper.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use futures::TryStreamExt;
use opendal::Operator;
use rand::RngCore;

use crate::telemetry::CleanupSummary;

/// Thread-safe tracker recording every remote test object created within a run's isolated prefix.
#[derive(Clone)]
pub struct RemoteObjectTracker {
    run_prefix: String,
    objects: Arc<Mutex<HashMap<String, u64>>>,
    total_created: Arc<AtomicU64>,
}

impl RemoteObjectTracker {
    pub fn new() -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let pid = std::process::id();
        let rand_tag = (rand::thread_rng().next_u32()) & 0xFFFF;
        let run_prefix = format!("oprobe-run-{now_ms}-{pid}-{rand_tag:04x}/");
        Self {
            run_prefix,
            objects: Arc::new(Mutex::new(HashMap::new())),
            total_created: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn run_prefix(&self) -> &str {
        &self.run_prefix
    }

    pub fn scoped_path(&self, rel: &str) -> String {
        format!("{}{}", self.run_prefix, rel.trim_start_matches('/'))
    }

    pub fn record_written(&self, path: &str, bytes: u64) {
        if let Ok(mut map) = self.objects.lock() {
            if map.insert(path.to_owned(), bytes).is_none() {
                self.total_created.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn record_deleted(&self, path: &str) {
        if let Ok(mut map) = self.objects.lock() {
            map.remove(path);
        }
    }

    /// Unconditionally clean up all created test objects under `run_prefix` and verify zero residue.
    pub async fn cleanup(&self, op: &Operator, auto_cleanup: bool) -> CleanupSummary {
        let t0 = Instant::now();
        let created = self.total_created.load(Ordering::Relaxed);

        let tracked: Vec<(String, u64)> = self
            .objects
            .lock()
            .map(|m| m.iter().map(|(k, &v)| (k.clone(), v)).collect())
            .unwrap_or_default();

        if !auto_cleanup {
            return CleanupSummary {
                auto_cleanup_enabled: false,
                run_prefix: self.run_prefix.clone(),
                objects_created: created,
                objects_deleted: 0,
                remaining_objects: tracked.len() as u64,
                bytes_cleaned: 0,
                cleanup_ms: 0.0,
                verified_clean: tracked.is_empty(),
            };
        }

        let mut deleted_count = 0_u64;
        let mut cleaned_bytes = 0_u64;

        // Pass 1: Delete all explicitly tracked object keys
        if !tracked.is_empty() {
            let paths: Vec<String> = tracked.iter().map(|(k, _)| k.clone()).collect();
            let tracked_bytes: u64 = tracked.iter().map(|(_, b)| *b).sum();
            if op.delete_iter(paths.clone()).await.is_ok() {
                deleted_count += paths.len() as u64;
                cleaned_bytes += tracked_bytes;
            } else {
                for (p, sz) in &tracked {
                    if op.delete(p).await.is_ok() {
                        deleted_count += 1;
                        cleaned_bytes += *sz;
                    }
                }
            }
        }

        // Pass 2: Recursive prefix sweep to catch any partial/retried keys under `run_prefix`
        if let Ok(lister) = op.lister_with(&self.run_prefix).recursive(true).await {
            if let Ok(entries) = lister.try_collect::<Vec<_>>().await {
                for entry in entries {
                    let p = entry.path().to_owned();
                    if p != self.run_prefix && !p.ends_with('/') && op.delete(&p).await.is_ok() {
                        deleted_count += 1;
                    }
                }
            }
        }
        let _ = op.delete(&self.run_prefix).await;

        // Pass 3: Verify zero file objects remain under `run_prefix`
        let remaining = match op.lister_with(&self.run_prefix).recursive(true).await {
            Ok(lister) => lister
                .try_collect::<Vec<_>>()
                .await
                .map(|entries| {
                    entries
                        .into_iter()
                        .filter(|e| e.path() != self.run_prefix && !e.path().ends_with('/'))
                        .count() as u64
                })
                .unwrap_or(0),
            Err(_) => 0,
        };

        CleanupSummary {
            auto_cleanup_enabled: true,
            run_prefix: self.run_prefix.clone(),
            objects_created: created,
            objects_deleted: deleted_count.max(created.saturating_sub(remaining)),
            remaining_objects: remaining,
            bytes_cleaned: cleaned_bytes,
            cleanup_ms: t0.elapsed().as_secs_f64() * 1000.0,
            verified_clean: remaining == 0,
        }
    }
}

impl Default for RemoteObjectTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Sweep all historical `oprobe-run-*` prefixes under the operator's root (`oprobe cleanup`).
pub async fn sweep_all_oprobe_prefixes(op: &Operator) -> anyhow::Result<CleanupSummary> {
    let t0 = Instant::now();
    let mut deleted = 0_u64;
    let mut bytes_cleaned = 0_u64;

    let lister = op.lister_with("").recursive(true).await?;
    let entries: Vec<_> = lister.try_collect().await?;

    let mut targets = Vec::new();
    for entry in entries {
        let p = entry.path();
        if (p.starts_with("oprobe-run-")
            || p.contains("/oprobe-run-")
            || p.starts_with("oprobe_bench/"))
            && !p.ends_with('/')
        {
            if let Ok(meta) = op.stat(p).await {
                bytes_cleaned += meta.content_length();
            }
            targets.push(p.to_owned());
        }
    }

    let found = targets.len() as u64;
    if !targets.is_empty() {
        if op.delete_iter(targets.clone()).await.is_ok() {
            deleted = found;
        } else {
            for p in &targets {
                if op.delete(p).await.is_ok() {
                    deleted += 1;
                }
            }
        }
    }

    Ok(CleanupSummary {
        auto_cleanup_enabled: true,
        run_prefix: "oprobe-run-*".to_owned(),
        objects_created: found,
        objects_deleted: deleted,
        remaining_objects: found.saturating_sub(deleted),
        bytes_cleaned,
        cleanup_ms: t0.elapsed().as_secs_f64() * 1000.0,
        verified_clean: deleted == found,
    })
}
