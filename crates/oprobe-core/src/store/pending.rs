//! Pending-cleanup markers (design v7 review, P3).
//!
//! A run that may write writes `<data>/pending/<run id>` before its first write: the target it
//! writes to ([`TargetId`]) and its run prefix (`oprobe-run-<id>/`). The marker is removed once
//! the cleanup verified that nothing is left (or the user kept the data on purpose). A run that
//! was killed hard (SIGKILL, a crash, a power cut) leaves its marker, and the next run against
//! the same target deletes that prefix — only an `oprobe-run-*` prefix, never under
//! `--read-only` — and then the marker.
//!
//! A live run holds an exclusive lock on its marker, so a run in another process never sweeps
//! the prefix of a run that is still going.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::target_id::TargetId;
use crate::config::TargetSpec;
use crate::engine::cleanup::RUN_PREFIX;

/// What a marker records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// The target's identity (`s3://bucket@host/root`).
    pub target: String,
    /// The prefix the run writes under, relative to the target's root.
    pub run_prefix: String,
}

impl Marker {
    /// The marker of a run of `target` under `run_prefix`; `None` for a target whose data does
    /// not outlive the process (`memory://`, `lab://`) or that has no identity.
    pub fn of(target: &TargetSpec, run_prefix: &str) -> Option<Self> {
        let id = TargetId::of(target).ok()?;
        if matches!(id.scheme.as_str(), "memory" | "lab") {
            return None;
        }
        Some(Self {
            target: id.to_string(),
            run_prefix: run_prefix.to_owned(),
        })
    }

    /// The prefix may be deleted by a sweep: one `oprobe-run-*` segment and nothing else.
    pub fn sweepable(&self) -> bool {
        let p = &self.run_prefix;
        p.starts_with(RUN_PREFIX)
            && p.ends_with('/')
            && p.len() > RUN_PREFIX.len() + 1
            && !p[..p.len() - 1].contains(['/', '\\'])
            && !p.contains("..")
    }
}

/// The marker directory under a data directory.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("pending")
}

/// The marker of a live run, locked until [`Pending::finish`] (or the process ends).
#[derive(Debug)]
pub struct Pending {
    path: PathBuf,
    file: File,
}

impl Pending {
    /// Write and lock the marker `<dir>/<run id>`.
    pub fn create(dir: &Path, run_id: &str, marker: &Marker) -> Result<Self> {
        super::create_private_dir_all(dir)
            .with_context(|| format!("cannot create `{}`", dir.display()))?;
        let path = dir.join(run_id);
        let mut file = super::private_file(&path)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        file.lock()
            .with_context(|| format!("cannot lock `{}`", path.display()))?;
        file.write_all(serde_json::to_string(marker)?.as_bytes())?;
        file.sync_all()?;
        Ok(Self { path, file })
    }

    /// The run's data is gone (or kept on purpose): remove the marker.
    pub fn finish(self) {
        let Self { path, file } = self;
        drop(file);
        let _ = std::fs::remove_file(path);
    }
}

/// A marker left by a run that is no longer going (its lock is free), held locked while its
/// prefix is swept.
#[derive(Debug)]
pub struct Stale {
    pub marker: Marker,
    pending: Pending,
}

impl Stale {
    /// The prefix is gone: remove the marker.
    pub fn finish(self) {
        self.pending.finish();
    }
}

/// The markers in `dir` of runs against `target` that are no longer going, oldest run first
/// (`own`: the marker of the calling run, never returned). Unreadable markers are skipped.
pub fn stale(dir: &Path, target: &str, own: &str) -> Vec<Stale> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != own && crate::model::RunId::parse(n).is_some())
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let path = dir.join(&name);
        let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        // Locked: the run is still going (in this process or another).
        if file.try_lock().is_err() {
            continue;
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            continue;
        }
        let Ok(marker) = serde_json::from_str::<Marker>(&text) else {
            continue;
        };
        if marker.target != target {
            continue;
        }
        out.push(Stale {
            marker,
            pending: Pending { path, file },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(prefix: &str) -> Marker {
        Marker {
            target: "fs:///tmp/x".into(),
            run_prefix: prefix.into(),
        }
    }

    #[test]
    fn a_live_marker_is_never_stale_and_a_finished_one_is_gone() {
        let d = tempfile::tempdir().unwrap();
        let m = marker("oprobe-run-20261120-143005-a1b2/");
        let live = Pending::create(d.path(), "20261120-143005-a1b2", &m).unwrap();
        assert!(
            stale(d.path(), &m.target, "x").is_empty(),
            "locked: still going"
        );
        // The process that held it ended (its lock is released with the file).
        drop(live);
        let s = stale(d.path(), &m.target, "x");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].marker, m);
        // Another target's markers are not this one's; nor is the caller's own.
        assert!(stale(d.path(), "fs:///tmp/y", "x").is_empty());
        drop(s);
        assert!(stale(d.path(), &m.target, "20261120-143005-a1b2").is_empty());
        stale(d.path(), &m.target, "x").pop().unwrap().finish();
        assert!(!d.path().join("20261120-143005-a1b2").exists());
    }

    #[test]
    fn only_run_prefixes_are_swept() {
        assert!(marker("oprobe-run-20261120-143005-a1b2/").sweepable());
        for p in [
            "",
            "oprobe-run-/",
            "data/",
            "oprobe-run-x",
            "oprobe-run-x/../../",
            "oprobe-run-x/y/",
            "oprobe-dataset/",
        ] {
            assert!(!marker(p).sweepable(), "{p}");
        }
    }

    #[test]
    fn in_process_targets_need_no_marker() {
        let t = TargetSpec::from_url("memory://").unwrap();
        assert!(Marker::of(&t, "oprobe-run-x/").is_none());
        let t = TargetSpec::from_url("fs:///tmp/oprobe-pending-test").unwrap();
        let m = Marker::of(&t, "oprobe-run-x/").unwrap();
        assert!(m.target.starts_with("fs://"), "{m:?}");
    }
}
