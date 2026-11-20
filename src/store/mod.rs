//! Persistent state (design §3): the config file (targets, defaults), the run store
//! (`runs/<id>.oprobe` + `.json` sidecars) and named baselines.
//!
//! Locations: `$OPROBE_CONFIG` or `<config_dir>/oprobe/config.toml`; data in
//! `$OPROBE_DATA_DIR` or `<data_dir>/oprobe/` (platform dirs from the `dirs` crate). Every
//! file is written atomically (temporary file + rename).

pub mod baselines;
pub mod config;
pub mod runs;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use baselines::{Baseline, Baselines};
pub use config::{ConfigFile, Defaults, TargetEntry};
pub use runs::{RunStore, RunSummary, read_run_file, resolve_run, write_run_file};

/// Environment variable overriding the config file path.
pub const ENV_CONFIG: &str = "OPROBE_CONFIG";
/// Environment variable overriding the data directory.
pub const ENV_DATA_DIR: &str = "OPROBE_DATA_DIR";

/// Where oprobe keeps its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    /// Resolve: `config_flag` (`--config`) > `$OPROBE_CONFIG` > platform config dir; data:
    /// `$OPROBE_DATA_DIR` > platform data dir.
    pub fn resolve(config_flag: Option<&Path>) -> Self {
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let config_file = config_flag
            .map(Path::to_path_buf)
            .or_else(|| env(ENV_CONFIG))
            .unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("oprobe")
                    .join("config.toml")
            });
        let data_dir = env(ENV_DATA_DIR).unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("oprobe")
        });
        Self {
            config_file,
            data_dir,
        }
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.data_dir.join("runs")
    }

    pub fn baselines_file(&self) -> PathBuf {
        self.data_dir.join("baselines.toml")
    }

    /// The run store under this data directory.
    pub fn run_store(&self) -> RunStore {
        RunStore::new(self.runs_dir())
    }
}

/// Write `bytes` to `path` atomically: a temporary file in the same directory, flushed and
/// synced, then renamed over `path`. Parent directories are created.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create directory `{}`", dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{:04x}",
        std::process::id(),
        rand::random::<u16>()
    ));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot write `{}`", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b/c.txt");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }
}
