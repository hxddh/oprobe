//! Persistent state (design §3): the config file (targets, defaults), the run store
//! (`runs/<id>.oprobe` + `.json` sidecars) and named baselines.
//!
//! Locations: `$OPROBE_CONFIG` or `<config_dir>/oprobe/config.toml`; data in
//! `$OPROBE_DATA_DIR` or `<data_dir>/oprobe/` (platform dirs from the `dirs` crate). Every
//! file is written atomically (temporary file + rename).
//!
//! Permissions (design v5 §5, S5): on unix every file oprobe writes here is created `0600` and
//! every directory it creates `0700` ([`private_file`], [`create_private_dir_all`]): the config
//! may hold inline secrets, and run files name buckets, endpoints and object keys.

pub mod baselines;
pub mod config;
pub mod runfile;
pub mod runs;
pub mod target_id;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use baselines::{Baseline, Baselines};
pub use config::{ConfigFile, Defaults, TargetEntry};
pub use runs::{RunStore, RunSummary, read_run_file, resolve_run, write_run_file};
pub use target_id::TargetId;

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

    /// Ceilings learned per target and workload (`analysis::capacity`).
    pub fn capacity_file(&self) -> PathBuf {
        self.data_dir.join("capacity.toml")
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
    create_private_dir_all(&dir)
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
        let mut f = private_file(&tmp)?;
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

/// Create (or truncate) `path` for writing, readable by its owner only (`0600` on unix).
pub fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let f = o.open(path)?;
    // `mode` applies only when the file is created: tighten a file that already existed.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(f)
}

/// `create_dir_all`, with the directories it creates accessible to their owner only (`0700`
/// on unix). Existing directories keep their permissions.
pub fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Write a small secret (a token) to `path`: `0600`, atomically.
pub fn write_secret(path: &Path, secret: &str) -> Result<()> {
    atomic_write(path, secret.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S5: files `0600`, created directories `0700`, also when a file already existed with
    /// wider permissions.
    #[cfg(unix)]
    #[test]
    fn files_are_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new/dir/config.toml");
        atomic_write(&p, b"x").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(&d.path().join("new")), 0o700);
        assert_eq!(mode(&d.path().join("new/dir")), 0o700);
        let wide = d.path().join("wide.toml");
        std::fs::write(&wide, b"old").unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&wide, b"new").unwrap();
        assert_eq!(mode(&wide), 0o600);
        let f = d.path().join("direct");
        drop(private_file(&f).unwrap());
        assert_eq!(mode(&f), 0o600);
    }

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
