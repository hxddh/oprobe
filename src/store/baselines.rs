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

#[cfg(test)]
mod tests {
    use super::*;

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
