//! Named baselines (`baselines.toml`): `[baselines.<name>] runs = [ids…], window = 10`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::{ErrorCode, OprobeError};

fn default_window() -> usize {
    10
}

/// A rolling window of runs new runs are compared against.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// Run ids, oldest first.
    #[serde(default)]
    pub runs: Vec<String>,
    /// How many of the newest runs the baseline keeps.
    #[serde(default = "default_window")]
    pub window: usize,
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            runs: Vec::new(),
            window: default_window(),
        }
    }
}

impl Baseline {
    /// Add a run, keeping only the newest `window` runs.
    pub fn push(&mut self, id: &str) {
        self.runs.retain(|r| r != id);
        self.runs.push(id.to_owned());
        let n = self.runs.len();
        if n > self.window.max(1) {
            self.runs.drain(..n - self.window.max(1));
        }
    }
}

/// `baselines.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Baselines {
    #[serde(default)]
    pub baselines: BTreeMap<String, Baseline>,
}

impl Baselines {
    /// Load `path`; a missing file has no baselines.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw).map_err(|e| {
                OprobeError::new(
                    ErrorCode::ConfigInvalid,
                    format!("invalid baselines file `{}`: {e}", path.display()),
                )
                .into()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read `{}`", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        super::atomic_write(path, text.as_bytes())
    }

    pub fn get(&self, name: &str) -> Option<&Baseline> {
        self.baselines.get(name)
    }

    /// Add `id` to baseline `name` (created with the default window when missing).
    pub fn add_run(&mut self, name: &str, id: &str) {
        self.baselines.entry(name.to_owned()).or_default().push(id);
    }

    pub fn remove(&mut self, name: &str) -> Option<Baseline> {
        self.baselines.remove(name)
    }

    /// Every run id referenced by a baseline (kept by `runs gc`).
    pub fn referenced(&self) -> std::collections::BTreeSet<String> {
        self.baselines
            .values()
            .flat_map(|b| b.runs.iter().cloned())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Noise floors: <data>/noise.toml (design v4 §7)
// ---------------------------------------------------------------------------------------

/// Metrics with a learned noise floor.
pub const NOISE_METRICS: [&str; 4] = ["p50_ms", "p99_ms", "ops_per_s", "mib_per_s"];

/// A/A pair deltas kept per metric (newest last).
pub const NOISE_SAMPLES_MAX: usize = 20;

/// Relative change below which a metric is noise when nothing was learned for the target:
/// 5 % for medians and throughput, 10 % for p99.
pub fn default_noise_ratio(metric: &str) -> f64 {
    match metric {
        "p99_ms" => 0.10,
        _ => 0.05,
    }
}

/// `<data>/noise.toml`.
pub fn noise_file(paths: &super::Paths) -> std::path::PathBuf {
    paths.data_dir.join("noise.toml")
}

/// What repeated identical runs of one target and workload showed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NoiseEntry {
    /// |b / a − 1| of each A/A pair, per metric (newest last, at most
    /// [`NOISE_SAMPLES_MAX`]).
    #[serde(default)]
    pub pair_deltas_ratio: BTreeMap<String, Vec<f64>>,
    /// A/A pairs seen (calibrate and verify).
    #[serde(default)]
    pub pairs_count: u64,
    /// RFC 3339.
    #[serde(default)]
    pub updated_at: String,
}

impl NoiseEntry {
    /// The learned floor of `metric`: 1.5 × the 90th percentile of its A/A deltas, within
    /// 1–50 % (`None` with fewer than 2 pairs).
    pub fn floor_ratio(&self, metric: &str) -> Option<f64> {
        let v = self.pair_deltas_ratio.get(metric)?;
        if v.len() < 2 {
            return None;
        }
        let mut s = v.clone();
        let q = crate::analysis::stats::quantile_mut(&mut s, 0.9);
        Some((1.5 * q).clamp(0.01, 0.5))
    }

    /// Add one A/A pair of `metric` values.
    pub fn add(&mut self, metric: &str, a: f64, b: f64) {
        if !(a.is_finite() && b.is_finite()) || a.abs() < 1e-12 {
            return;
        }
        let v = self.pair_deltas_ratio.entry(metric.to_owned()).or_default();
        v.push((b / a - 1.0).abs());
        if v.len() > NOISE_SAMPLES_MAX {
            let n = v.len() - NOISE_SAMPLES_MAX;
            v.drain(..n);
        }
    }
}

/// The floor of `metric` for comparisons: learned when known, else the default.
pub fn noise_ratio(entry: Option<&NoiseEntry>, metric: &str) -> f64 {
    entry
        .and_then(|e| e.floor_ratio(metric))
        .unwrap_or_else(|| default_noise_ratio(metric))
}

/// The noise metrics of a report: `(metric, value)`.
pub fn noise_values(r: &crate::model::Report) -> [(&'static str, f64); 4] {
    [
        ("p50_ms", r.summary.latency.p50),
        ("p99_ms", r.summary.latency.p99),
        ("ops_per_s", crate::report::diff::good_ops_per_s(r)),
        ("mib_per_s", r.summary.mib_per_s),
    ]
}

/// `<data>/noise.toml`: noise per target identity, then workload hash.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NoiseFloors {
    #[serde(default)]
    pub targets: BTreeMap<String, BTreeMap<String, NoiseEntry>>,
}

impl NoiseFloors {
    /// Load `path`; a missing file knows nothing.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw).map_err(|e| {
                OprobeError::new(
                    ErrorCode::ConfigInvalid,
                    format!("invalid noise file `{}`: {e}", path.display()),
                )
                .with_hint("delete the file; `oprobe baseline calibrate` rebuilds it")
                .into()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read `{}`", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        super::atomic_write(path, text.as_bytes())
    }

    pub fn get(&self, target: &str, workload_hash: &str) -> Option<&NoiseEntry> {
        self.targets.get(target)?.get(workload_hash)
    }

    /// The entry of a report's target and workload.
    pub fn of(&self, r: &crate::model::Report) -> Option<&NoiseEntry> {
        self.get(
            &crate::report::diff::target_identity(r),
            &r.spec.workload_hash(),
        )
    }

    /// Learn from an A/A pair: two runs of the same spec.
    pub fn add_pair(&mut self, a: &crate::model::Report, b: &crate::model::Report) {
        let e = self
            .targets
            .entry(crate::report::diff::target_identity(a))
            .or_default()
            .entry(a.spec.workload_hash())
            .or_default();
        for ((m, va), (_, vb)) in noise_values(a).into_iter().zip(noise_values(b)) {
            e.add(m, va, vb);
        }
        e.pairs_count += 1;
        e.updated_at = crate::util::now_rfc3339();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_floor_learns_from_pairs() {
        let mut e = NoiseEntry::default();
        assert_eq!(noise_ratio(Some(&e), "p99_ms"), 0.10);
        e.add("p50_ms", 10.0, 10.2);
        assert!(e.floor_ratio("p50_ms").is_none());
        e.add("p50_ms", 10.0, 9.9);
        let f = e.floor_ratio("p50_ms").unwrap();
        assert!((0.01..=0.05).contains(&f), "{f}");
        for _ in 0..30 {
            e.add("p50_ms", 1.0, 1.01);
        }
        assert_eq!(e.pair_deltas_ratio["p50_ms"].len(), NOISE_SAMPLES_MAX);
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("noise.toml");
        let mut n = NoiseFloors::load(&p).unwrap();
        n.targets
            .entry("s3://b/".into())
            .or_default()
            .insert("sha256:aa".into(), e.clone());
        n.save(&p).unwrap();
        assert_eq!(
            NoiseFloors::load(&p).unwrap().get("s3://b/", "sha256:aa"),
            Some(&e)
        );
    }

    #[test]
    fn rolling_window_and_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("baselines.toml");
        let mut b = Baselines::load(&p).unwrap();
        b.baselines.insert(
            "main".into(),
            Baseline {
                runs: vec![],
                window: 2,
            },
        );
        for id in ["a", "b", "c", "b"] {
            b.add_run("main", id);
        }
        assert_eq!(b.get("main").unwrap().runs, ["c", "b"]);
        b.save(&p).unwrap();
        let back = Baselines::load(&p).unwrap();
        assert_eq!(back, b);
        assert!(
            std::fs::read_to_string(&p)
                .unwrap()
                .contains("[baselines.main]")
        );
        assert_eq!(back.referenced().len(), 2);
    }
}
