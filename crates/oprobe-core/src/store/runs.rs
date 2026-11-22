//! The run store: `runs/<id>.oprobe` (segmented zstd stream, format `oprobe.run/5`,
//! [`super::runfile`]) plus a
//! `runs/<id>.json` sidecar summary for fast listing, and RUNREF resolution (design §2).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::{ErrorCode, OprobeError};
use crate::model::{Report, Run, RunId, VerdictLevel};

/// Sidecar summary of a run (`runs/<id>.json`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    pub target_display: String,
    pub started_at: String,
    pub window_s: f64,
    pub interrupted: bool,
    pub verdict_level: VerdictLevel,
    pub headline: String,
    pub ops_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    /// Projected monthly cost, where cost applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_month_usd: Option<f64>,
    /// The run file (filled in when listing; not stored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Why the run is partial: `budget` (a safety budget stopped it) or `interrupted`
    /// (stopped early); absent for a complete run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<String>,
    /// The target identity of the run's spec (`store::TargetId`, empty when it does not
    /// resolve). Required, so sidecars written without it are rebuilt from the run file.
    pub target_id: String,
    /// `report.spec.workload_hash()`: runs with the same target and workload hash compare.
    pub workload_hash: String,
}

impl From<&Report> for RunSummary {
    fn from(r: &Report) -> Self {
        Self {
            id: r.run.id.clone(),
            label: r.run.label.clone(),
            target_name: r.target.name.clone(),
            target_display: r.target.display.clone(),
            started_at: r.run.started_at.clone(),
            window_s: r.run.window_s,
            interrupted: r.run.interrupted,
            verdict_level: r.verdict.level,
            headline: r.verdict.headline.clone(),
            ops_per_s: r.summary.ops_per_s,
            p50_ms: r.summary.latency.p50,
            p99_ms: r.summary.latency.p99,
            error_ratio: r.summary.error_rate,
            cost_per_month_usd: r.cost.applicable.then_some(r.cost.selected.total_usd),
            path: None,
            partial: if r.verdict.exit_code == crate::errors::exit::BUDGET {
                Some("budget".into())
            } else if r.run.interrupted {
                Some("interrupted".into())
            } else {
                None
            },
            target_id: r
                .spec
                .target_id()
                .map(|t| t.to_string())
                .unwrap_or_default(),
            workload_hash: r.spec.workload_hash(),
        }
    }
}

/// Encode a run as `oprobe.run/5` in memory (tests and small exports; the store streams to
/// disk with [`super::runfile::write_run`]).
pub fn encode_run(run: &Run) -> Result<Vec<u8>> {
    let dir = tempdir_for_encode()?;
    let path = dir.join("run.oprobe");
    super::runfile::write_run(&path, run)?;
    let bytes = std::fs::read(&path)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(bytes)
}

fn tempdir_for_encode() -> Result<PathBuf> {
    let d = std::env::temp_dir().join(format!(
        "oprobe-encode-{}-{}",
        std::process::id(),
        RunId::new()
    ));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Decode an `oprobe.run/5` stream (a v4 or older file is refused with a hint).
pub fn decode_run(r: impl Read) -> Result<Run> {
    super::runfile::read_run(r).map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("format") || msg.contains("not an oprobe run file") || msg.contains("corrupt") {
            crate::errors::OprobeError::new(crate::errors::ErrorCode::UsageInvalidArgument, msg)
                .with_hint("run files of other oprobe versions are not read: re-run the workload with this oprobe")
                .into()
        } else {
            e
        }
    })
}

/// Write a run file (streamed in segments, moved into place when complete).
pub fn write_run_file(path: &Path, run: &Run) -> Result<()> {
    super::runfile::write_run(path, run).map(|_| ())
}

/// Read a run file.
pub fn read_run_file(path: &Path) -> Result<Run> {
    let f = std::fs::File::open(path)
        .with_context(|| format!("cannot open run file `{}`", path.display()))?;
    decode_run(f).with_context(|| format!("cannot read run file `{}`", path.display()))
}

/// Resolve a RUNREF in the default store (`$OPROBE_DATA_DIR` or the platform data dir).
pub fn resolve_run(r: &str) -> Result<PathBuf> {
    super::Paths::resolve(None).run_store().resolve(r)
}

/// Runs under one directory.
#[derive(Clone, Debug)]
pub struct RunStore {
    dir: PathBuf,
}

impl RunStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn run_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.oprobe"))
    }

    fn sidecar_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Save a run (file + sidecar). Returns the run file path.
    pub fn save(&self, run: &Run) -> Result<PathBuf> {
        let id = run.id();
        anyhow::ensure!(RunId::parse(id).is_some(), "invalid run id `{id}`");
        let path = self.run_path(id);
        write_run_file(&path, run)?;
        let summary = RunSummary::from(&run.report);
        super::atomic_write(
            &self.sidecar_path(id),
            serde_json::to_string_pretty(&summary)?.as_bytes(),
        )?;
        Ok(path)
    }

    /// Save a run whose rows were streamed to its run file during the run
    /// ([`super::runfile::RunFileStream`]): the report is appended and the file moved into
    /// place; `run.ops` / `run.reqs` (the in-memory sample) are not written again.
    pub fn save_streamed(
        &self,
        run: &Run,
        stream: &super::runfile::RunFileStream,
    ) -> Result<PathBuf> {
        let id = run.id();
        anyhow::ensure!(RunId::parse(id).is_some(), "invalid run id `{id}`");
        anyhow::ensure!(
            stream.path() == self.run_path(id),
            "the streamed run file is not `{}`",
            self.run_path(id).display()
        );
        let path = stream.finish(&run.report)?;
        let summary = RunSummary::from(&run.report);
        super::atomic_write(
            &self.sidecar_path(id),
            serde_json::to_string_pretty(&summary)?.as_bytes(),
        )?;
        Ok(path)
    }

    /// Ids of stored runs, oldest first.
    pub fn ids(&self) -> Result<Vec<String>> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("cannot list `{}`", self.dir.display()));
            }
        };
        let mut ids: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".oprobe")?;
                RunId::parse(id).map(|_| id.to_owned())
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Summaries of stored runs, newest first (sidecars are rebuilt when missing).
    pub fn list(&self) -> Result<Vec<RunSummary>> {
        let mut out = Vec::new();
        for id in self.ids()?.into_iter().rev() {
            let side = self.sidecar_path(&id);
            let summary = match std::fs::read(&side)
                .ok()
                .and_then(|b| serde_json::from_slice::<RunSummary>(&b).ok())
            {
                Some(s) => s,
                None => match read_run_file(&self.run_path(&id)) {
                    Ok(run) => {
                        let s = RunSummary::from(&run.report);
                        let _ = super::atomic_write(
                            &side,
                            serde_json::to_string_pretty(&s)?.as_bytes(),
                        );
                        s
                    }
                    Err(_) => continue,
                },
            };
            out.push(RunSummary {
                path: Some(self.run_path(&id).display().to_string()),
                ..summary
            });
        }
        Ok(out)
    }

    /// Resolve a RUNREF: a run id, a unique id prefix, `last`, `last~N`, a label (newest run
    /// with it), or a path to a `.oprobe` / report `.json` file.
    pub fn resolve(&self, r: &str) -> Result<PathBuf> {
        let unknown = |msg: String| -> anyhow::Error {
            OprobeError::new(ErrorCode::UsageUnknownRun, msg)
                .with_hint("list runs with `oprobe runs list`")
                .into()
        };
        let p = Path::new(r);
        let looks_like_path = r.contains('/')
            || r.contains(std::path::MAIN_SEPARATOR)
            || r.ends_with(".oprobe")
            || r.ends_with(".json");
        if looks_like_path {
            return if p.is_file() {
                Ok(p.to_path_buf())
            } else {
                Err(unknown(format!("no run file `{r}`")))
            };
        }
        let ids = self.ids()?;
        if let Some(rest) = r.strip_prefix("last") {
            let n = match rest {
                "" => 0,
                _ => rest
                    .strip_prefix('~')
                    .and_then(|n| n.parse::<usize>().ok())
                    .ok_or_else(|| {
                        unknown(format!("invalid run reference `{r}` (last, last~N)"))
                    })?,
            };
            return ids
                .iter()
                .rev()
                .nth(n)
                .map(|id| self.run_path(id))
                .ok_or_else(|| {
                    unknown(if ids.is_empty() {
                        "no runs yet".to_owned()
                    } else {
                        format!("`{r}`: only {} runs are stored", ids.len())
                    })
                });
        }
        if ids.iter().any(|i| i == r) {
            return Ok(self.run_path(r));
        }
        let matches: Vec<&String> = ids.iter().filter(|i| i.starts_with(r)).collect();
        match matches.len() {
            1 => return Ok(self.run_path(matches[0])),
            n if n > 1 => {
                return Err(OprobeError::new(
                    ErrorCode::UsageUnknownRun,
                    format!("run reference `{r}` is ambiguous ({n} runs match)"),
                )
                .with_hint(format!(
                    "use more characters: {}",
                    matches
                        .iter()
                        .rev()
                        .take(5)
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .into());
            }
            _ => {}
        }
        if let Some(s) = self
            .list()?
            .into_iter()
            .find(|s| s.label.as_deref() == Some(r))
        {
            return Ok(self.run_path(&s.id));
        }
        Err(unknown(format!("no run matches `{r}`")))
    }

    /// Load a run by RUNREF. A report `.json` file loads without trace rows.
    pub fn load(&self, r: &str) -> Result<Run> {
        let path = self.resolve(r)?;
        load_path(&path)
    }

    /// Delete a stored run (file and sidecar).
    pub fn delete(&self, id: &str) -> Result<()> {
        let p = self.run_path(id);
        std::fs::remove_file(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::new(OprobeError::new(
                    ErrorCode::UsageUnknownRun,
                    format!("no run `{id}`"),
                ))
            } else {
                anyhow::Error::new(e).context(format!("cannot delete `{}`", p.display()))
            }
        })?;
        let _ = std::fs::remove_file(self.sidecar_path(id));
        Ok(())
    }

    /// Runs `gc` would delete: all but the newest `keep` runs, and (when given) only those
    /// older than `older_than`; ids in `protect` (baselines) are kept.
    pub fn gc_candidates(
        &self,
        keep: usize,
        older_than: Option<std::time::Duration>,
        protect: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<String>> {
        let ids = self.ids()?;
        let now = std::time::SystemTime::now();
        let n = ids.len();
        Ok(ids
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i + keep < n)
            .map(|(_, id)| id)
            .filter(|id| !protect.contains(id))
            .filter(|id| match older_than {
                None => true,
                Some(age) => RunId::parse(id)
                    .and_then(|r| r.started_at())
                    .and_then(|t| now.duration_since(t).ok())
                    .is_some_and(|a| a >= age),
            })
            .collect())
    }
}

/// Load a run file, or a report `.json` (without trace rows).
pub fn load_path(path: &Path) -> Result<Run> {
    let is_json = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_json {
        let report = crate::report::load_json(path)?;
        return Ok(Run {
            report,
            ops: Vec::new(),
            reqs: Vec::new(),
        });
    }
    read_run_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    pub(crate) fn sample_run(id: &str, label: Option<&str>) -> Run {
        let p = crate::config::Profile::from_target(
            crate::config::TargetSpec::from_url("memory://").unwrap(),
        );
        let mut snap = crate::trace::Snapshot {
            elapsed_s: 2.0,
            ..Default::default()
        };
        snap.summary.ops = 2;
        let rid = RunId::parse(id).unwrap();
        let mut run = build_run(ReportInput {
            run_id: &rid,
            target_name: Some("local".into()),
            profile: &p,
            knobs: &p.knobs,
            snapshot: snap,
            trace: Default::default(),
            cleanup: Default::default(),
            warnings: vec![],
            started_at: std::time::SystemTime::now(),
            ended_at: std::time::SystemTime::now(),
            spec: None,
        });
        run.report.run.label = label.map(str::to_owned);
        run.ops = (1..=2)
            .map(|i| {
                crate::trace::log::op_row(
                    i,
                    "stat",
                    format!("k{i}"),
                    i as i64 * 1000,
                    500,
                    0,
                    None,
                    false,
                    0,
                    [0, 0, 400, 0, 100],
                    1,
                )
            })
            .collect();
        run.reqs = run
            .ops
            .iter()
            .map(|o| ReqRow {
                t_us: o.t_us,
                op_id: o.op_id,
                attempt: 1,
                api: "HeadObject".into(),
                method: "HEAD".into(),
                status: 200,
                protocol: WireProtocol::H1,
                reused: true,
                conn_id: 1,
                remote: Some("127.0.0.1:9000".parse().unwrap()),
                queue_us: 0,
                dns_us: 0,
                tcp_us: 0,
                proxy_us: 0,
                tls_us: 0,
                send_us: 10,
                ttfb_us: 380,
                body_us: 10,
                stall_us: 0,
                total_us: 400,
                bytes_up: 0,
                bytes_down: 0,
                request_id: Some("abc".into()),
                retry_after_ms: None,
                error_code: None,
                error: None,
                tls: None,
                tcp: None,
                detail: None,
            })
            .collect();
        run
    }

    #[test]
    fn run_files_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let store = RunStore::new(d.path().join("runs"));
        let run = sample_run("20261120-143005-a1b2", Some("nightly"));
        let path = store.save(&run).unwrap();
        assert!(path.ends_with("20261120-143005-a1b2.oprobe"));
        let back = read_run_file(&path).unwrap();
        assert_eq!(back, run);
        // A segmented stream: the header first, the report last.
        let mut rd = super::super::runfile::RunFileReader::open(&path).unwrap();
        let mut kinds = Vec::new();
        while let Some(b) = rd.next_block().unwrap() {
            kinds.push(match b {
                super::super::runfile::Block::Header(h) => {
                    assert_eq!(h.format, crate::model::RUN_FORMAT);
                    "header"
                }
                super::super::runfile::Block::Segment(_) => "segment",
                super::super::runfile::Block::Interval(_) => "interval",
                super::super::runfile::Block::Report(_) => "report",
                super::super::runfile::Block::Unknown(_) => "unknown",
            });
        }
        assert_eq!(kinds.first(), Some(&"header"));
        assert_eq!(kinds.last(), Some(&"report"));
        assert!(kinds.contains(&"segment"));
        // In memory too.
        assert_eq!(decode_run(&encode_run(&run).unwrap()[..]).unwrap(), run);
    }

    #[test]
    fn resolves_ids_prefixes_last_labels_and_paths() {
        let d = tempfile::tempdir().unwrap();
        let store = RunStore::new(d.path().join("runs"));
        for (id, label) in [
            ("20261120-143005-a1b2", Some("base")),
            ("20261120-150000-c3d4", None),
            ("20261121-090000-e5f6", Some("base")),
        ] {
            store.save(&sample_run(id, label)).unwrap();
        }
        let name = |p: PathBuf| p.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(name(store.resolve("last").unwrap()), "20261121-090000-e5f6");
        assert_eq!(
            name(store.resolve("last~2").unwrap()),
            "20261120-143005-a1b2"
        );
        assert_eq!(
            name(store.resolve("20261120-150000-c3d4").unwrap()),
            "20261120-150000-c3d4"
        );
        assert_eq!(
            name(store.resolve("20261121").unwrap()),
            "20261121-090000-e5f6"
        );
        assert_eq!(name(store.resolve("base").unwrap()), "20261121-090000-e5f6");
        let p = store.run_path("20261120-150000-c3d4");
        assert_eq!(store.resolve(p.to_str().unwrap()).unwrap(), p);
        for (bad, code) in [
            ("2026112", ErrorCode::UsageUnknownRun), // ambiguous
            ("last~9", ErrorCode::UsageUnknownRun),
            ("nope", ErrorCode::UsageUnknownRun),
            ("/no/such.oprobe", ErrorCode::UsageUnknownRun),
        ] {
            let e = store.resolve(bad).unwrap_err();
            assert_eq!(crate::errors::classify(&e, "s3").code, code, "{bad}");
        }
        let list = store.list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, "20261121-090000-e5f6");
        // Sidecars are rebuilt when missing.
        std::fs::remove_file(d.path().join("runs/20261120-150000-c3d4.json")).unwrap();
        assert_eq!(store.list().unwrap().len(), 3);
        // gc keeps the newest and protected runs.
        let protect = ["20261120-143005-a1b2".to_owned()].into_iter().collect();
        assert_eq!(
            store.gc_candidates(1, None, &protect).unwrap(),
            vec!["20261120-150000-c3d4".to_owned()]
        );
        store.delete("20261120-150000-c3d4").unwrap();
        assert_eq!(store.ids().unwrap().len(), 2);
        assert!(store.delete("20261120-150000-c3d4").is_err());
    }
}
