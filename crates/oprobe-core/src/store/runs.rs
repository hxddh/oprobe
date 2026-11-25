//! The run store: `runs/<id>.oprobe` (segmented zstd stream, format `oprobe.run/7`,
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
    /// The `oprobe prove` this run is a trial of (hidden from `show --list` unless `--all`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    pub target_display: String,
    pub started_at: String,
    pub window_s: f64,
    pub verdict_level: VerdictLevel,
    pub headline: String,
    pub ops_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
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
    /// The workload preset (absent in summaries written before 5.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Throttled responses / requests (absent in summaries written before 5.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttled_request_ratio: Option<f64>,
}

impl From<&Report> for RunSummary {
    fn from(r: &Report) -> Self {
        Self {
            id: r.run.id.clone(),
            label: r.run.label.clone(),
            parent_id: r.run.parent_id.clone(),
            target_name: r.target.name.clone(),
            target_display: r.target.display.clone(),
            started_at: r.run.started_at.clone(),
            window_s: r.run.window_s,
            verdict_level: r.verdict.level,
            headline: r.verdict.headline.clone(),
            ops_per_s: r.summary.ops_per_s,
            p50_ms: r.summary.latency.p50,
            p99_ms: r.summary.latency.p99,
            error_ratio: r.summary.error_rate,
            path: None,
            partial: r.run.partial.as_ref().map(|p| p.reason.as_str().to_owned()),
            target_id: r
                .spec
                .target_id()
                .map(|t| t.to_string())
                .unwrap_or_default(),
            workload_hash: r.spec.workload_hash(),
            preset: Some(r.workload.preset.clone()),
            throttled_request_ratio: Some(r.summary.throttled_request_ratio),
        }
    }
}

/// Encode a run as `oprobe.run/7` in memory (tests and small exports; the store streams to
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

/// Decode an `oprobe.run/7` stream (other versions are refused with a hint; a damaged or
/// truncated file of this version says so, not that it is of another version).
pub fn decode_run(r: impl Read) -> Result<Run> {
    super::runfile::read_run(r).map_err(|e| {
        let msg = format!("{e:#}");
        let invalid = |hint: &str| -> anyhow::Error {
            crate::errors::OprobeError::new(
                crate::errors::ErrorCode::UsageInvalidArgument,
                msg.clone(),
            )
            .with_hint(hint.to_owned())
            .into()
        };
        // An I/O error (a sharing violation, a permission) is what it says, not a damaged file;
        // a file that ends early is.
        let io = e.chain().find_map(|c| c.downcast_ref::<std::io::Error>());
        if io.is_some_and(|io| io.kind() != std::io::ErrorKind::UnexpectedEof) {
            return e;
        }
        if msg.contains("is not supported") {
            invalid(
                "run files of other oprobe versions are not read: re-run the workload with this oprobe",
            )
        } else if msg.contains("not an oprobe run file") {
            invalid("pass an `.oprobe` run file, a report `.json` or an `oprobe-trace` file")
        } else {
            invalid("the run file is damaged or truncated: copy it again, or re-run the workload")
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
                .with_hint("list runs with `oprobe show --list`")
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
            // `last` names the runs the user made, never prove's trials (U4).
            let ids = self.top_level_ids()?;
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

    /// Delete a stored run (file and sidecar; stored proves keep standing on their own).
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

    /// Ids of the runs the user made (not prove trials), oldest first.
    pub fn top_level_ids(&self) -> Result<Vec<String>> {
        let mut v: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|s| s.parent_id.is_none())
            .map(|s| s.id)
            .collect();
        v.reverse();
        Ok(v)
    }

    /// Ids of the trial runs of prove `parent`.
    pub fn children(&self, parent: &str) -> Result<Vec<String>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|s| s.parent_id.as_deref() == Some(parent))
            .map(|s| s.id)
            .collect())
    }

    fn prove_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.prove.json"))
    }

    /// Save the result of a prove (`<id>.prove.json`).
    pub fn save_prove(&self, id: &str, result: &serde_json::Value) -> Result<PathBuf> {
        anyhow::ensure!(RunId::parse(id).is_some(), "invalid prove id `{id}`");
        let path = self.prove_path(id);
        super::atomic_write(&path, serde_json::to_string_pretty(result)?.as_bytes())?;
        Ok(path)
    }

    /// The stored proves (their JSON results), newest first.
    pub fn proves(&self) -> Result<Vec<serde_json::Value>> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut v: Vec<(String, serde_json::Value)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".prove.json")?.to_owned();
                let raw = std::fs::read(e.path()).ok()?;
                Some((id, serde_json::from_slice(&raw).ok()?))
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(v.into_iter().map(|x| x.1).collect())
    }

    /// Ids of stored proves, oldest first.
    pub fn prove_ids(&self) -> Result<Vec<String>> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut ids: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".prove.json")?;
                RunId::parse(id).map(|_| id.to_owned())
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Stored prove ids whose baseline `run_id` is `run_id`, newest first.
    pub fn proves_of_run(&self, run_id: &str) -> Result<Vec<String>> {
        Ok(self
            .proves()?
            .into_iter()
            .filter_map(|v| {
                (v.get("run_id").and_then(|x| x.as_str()) == Some(run_id))
                    .then(|| {
                        v.get("prove_id")
                            .and_then(|x| x.as_str())
                            .map(str::to_owned)
                    })
                    .flatten()
            })
            .collect())
    }

    /// Resolve a prove reference: a `.prove.json` path, `last-prove`, an exact `prove_id`, or
    /// an unambiguous `prove_id` prefix (when no stored run matches `r`).
    pub fn resolve_prove(&self, r: &str) -> Result<Option<PathBuf>> {
        let r = r.trim();
        if r.is_empty() {
            return Ok(None);
        }
        let p = Path::new(r);
        if r.ends_with(".prove.json") {
            return if p.is_file() {
                Ok(Some(p.to_path_buf()))
            } else {
                Err(
                    OprobeError::new(ErrorCode::UsageUnknownRun, format!("no prove file `{r}`"))
                        .with_hint("list proves with `oprobe show --list`")
                        .into(),
                )
            };
        }
        if (r.contains('/') || r.contains(std::path::MAIN_SEPARATOR) || r.ends_with(".json"))
            && p.is_file()
            && let Ok(raw) = std::fs::read(p)
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw)
            && v.get("prove_id").is_some()
            && v.get("verdict").is_some()
        {
            return Ok(Some(p.to_path_buf()));
        }
        let pids = self.prove_ids()?;
        if r == "last-prove" {
            return pids
                .last()
                .map(|id| Ok(Some(self.prove_path(id))))
                .unwrap_or_else(|| {
                    Err(
                        OprobeError::new(ErrorCode::UsageUnknownRun, "no proves yet")
                            .with_hint("run `oprobe prove RUN` first")
                            .into(),
                    )
                });
        }
        if pids.iter().any(|id| id == r) {
            return Ok(Some(self.prove_path(r)));
        }
        if r == "last" || r.starts_with("last~") {
            return Ok(None);
        }
        // Only treat `r` as a prove prefix when no stored run starts with `r`.
        if self.ids()?.iter().any(|id| id.starts_with(r)) {
            return Ok(None);
        }
        let matches: Vec<&String> = pids.iter().filter(|id| id.starts_with(r)).collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(self.prove_path(matches[0]))),
            n => Err(OprobeError::new(
                ErrorCode::UsageUnknownRun,
                format!("prove reference `{r}` is ambiguous ({n} proves match)"),
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
            .into()),
        }
    }

    /// Delete a stored prove and its trial runs. `false` when `id` is not a prove.
    pub fn delete_prove(&self, id: &str) -> Result<bool> {
        let p = self.prove_path(id);
        if !p.is_file() {
            return Ok(false);
        }
        for c in self.children(id)? {
            self.delete(&c)?;
        }
        std::fs::remove_file(&p)?;
        Ok(true)
    }

    /// The files of stored run (or prove) `id` that exist: its run file and sidecar, or a
    /// prove's result and its trial runs' files.
    fn files_of(&self, id: &str) -> Result<Vec<PathBuf>> {
        let mut v = Vec::new();
        let mut push = |p: PathBuf| {
            if !v.contains(&p) {
                v.push(p);
            }
        };
        let prove = self.prove_path(id);
        if prove.is_file() {
            for c in self.children(id)? {
                for f in self.files_of(&c)? {
                    push(f);
                }
            }
            push(prove);
        }
        let run_file = self.run_path(id);
        if run_file.is_file() {
            push(run_file);
        }
        let sidecar = self.sidecar_path(id);
        if sidecar.is_file() {
            push(sidecar);
        }
        Ok(v)
    }

    /// Move the files of runs `ids` into `trash` (a new directory), where
    /// [`Self::restore_from`] finds them again. Nothing is deleted.
    pub fn move_to(&self, ids: &[String], trash: &Path) -> Result<()> {
        super::create_private_dir_all(trash)
            .with_context(|| format!("cannot create `{}`", trash.display()))?;
        // All or nothing: a move that fails puts the files already moved back.
        let mut moved: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
        let undo = |moved: &[(std::path::PathBuf, std::path::PathBuf)]| {
            for (from, to) in moved.iter().rev() {
                let _ = std::fs::rename(to, from);
            }
            let _ = std::fs::remove_dir(trash);
        };
        for id in ids {
            let files = match self.files_of(id) {
                Ok(f) => f,
                Err(e) => {
                    undo(&moved);
                    return Err(e);
                }
            };
            if files.is_empty() {
                undo(&moved);
                return Err(
                    OprobeError::new(ErrorCode::UsageUnknownRun, format!("no run `{id}`")).into(),
                );
            }
            for f in files {
                let to = trash.join(f.file_name().expect("a stored file has a name"));
                if let Err(e) = std::fs::rename(&f, &to) {
                    undo(&moved);
                    return Err(anyhow::Error::new(e).context(format!(
                        "cannot move `{}` to `{}`",
                        f.display(),
                        to.display()
                    )));
                }
                moved.push((f, to));
            }
        }
        Ok(())
    }

    /// Move every file of `trash` (made by [`Self::move_to`]) back into the store, then remove
    /// `trash`. A file the store has again meanwhile is kept and the trashed one dropped.
    pub fn restore_from(&self, trash: &Path) -> Result<()> {
        super::create_private_dir_all(&self.dir)?;
        let rd = std::fs::read_dir(trash)
            .with_context(|| format!("cannot read `{}`", trash.display()))?;
        for e in rd {
            let from = e?.path();
            let Some(name) = from.file_name() else {
                continue;
            };
            let to = self.dir.join(name);
            if !to.exists() {
                std::fs::rename(&from, &to).with_context(|| {
                    format!("cannot move `{}` to `{}`", from.display(), to.display())
                })?;
            }
        }
        std::fs::remove_dir_all(trash)
            .with_context(|| format!("cannot remove `{}`", trash.display()))?;
        Ok(())
    }
}

/// The value of the first `"format": "…"` member in `head` (the start of a JSON document;
/// oprobe and oprobe-trace write it first).
fn format_in_prefix(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok().or_else(|| {
        // A prefix cut inside a multi-byte character: read up to it.
        let e = std::str::from_utf8(head).err()?;
        std::str::from_utf8(&head[..e.valid_up_to()]).ok()
    })?;
    let rest = &text[text.find("\"format\"")? + 8..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let v = &rest[..end];
    (!v.contains('\\')).then(|| v.to_owned())
}

/// The whole file and its `format`, when `path` starts like a JSON document (a run file is a
/// zstd stream: only its first bytes are read).
fn json_document(path: &Path) -> Option<(Vec<u8>, String)> {
    use std::io::Read;
    let mut head = [0u8; 512];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    if !crate::trace::TraceFile::sniff(&head[..n]) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let format = format_in_prefix(&bytes[..bytes.len().min(4096)]).or_else(|| {
        #[derive(serde::Deserialize)]
        struct Format {
            #[serde(default)]
            format: Option<String>,
        }
        serde_json::from_slice::<Format>(&bytes)
            .ok()
            .map(|f| f.format.unwrap_or_default())
    });
    Some((bytes, format.unwrap_or_default()))
}

/// Load a run file, a production trace written by `oprobe-trace` (`oprobe.trace/9`; whatever
/// its extension: recognised by its content), or a report `.json` (without trace rows).
pub fn load_path(path: &Path) -> Result<Run> {
    let doc = json_document(path);
    let format = doc.as_ref().map(|(_, f)| f.clone());
    if let Some((bytes, f)) = doc.filter(|(_, f)| f.starts_with("oprobe.trace/")) {
        if !crate::trace::TraceFile::reads(&f) {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!(
                    "`{}` is a trace of format `{f}`; this oprobe reads `{}`",
                    path.display(),
                    crate::trace::TRACE_FORMATS_READ.join("` and `")
                ),
            )
            .with_hint("open it with the oprobe release that matches the oprobe-trace version that wrote it")
            .into());
        }
        let file = crate::trace::TraceFile::from_slice(&bytes, &path.display().to_string())
            .map_err(|e| {
                anyhow::Error::new(
                    OprobeError::new(
                        ErrorCode::UsageInvalidArgument,
                        format!("cannot read trace file `{}`: {e}", path.display()),
                    )
                    .with_hint("save the profile again with oprobe-trace's `Handle::save`"),
                )
            })?;
        drop(bytes);
        return Ok(crate::model::trace_file::trace_run(file, path)?);
    }
    let is_json = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_json || format.is_some() {
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
pub(crate) mod tests {
    use super::*;

    #[test]
    fn the_format_is_read_from_the_start_of_a_document() {
        let f = |s: &str| format_in_prefix(s.as_bytes());
        assert_eq!(
            f(r#"{"format":"oprobe.trace/9","x":1}"#).as_deref(),
            Some("oprobe.trace/9")
        );
        assert_eq!(
            f("{ \"format\" : \"oprobe.report/7\" }").as_deref(),
            Some("oprobe.report/7")
        );
        assert_eq!(f(r#"{"x":1}"#), None);
        assert_eq!(f(r#"{"format":"oprobe.tr"#), None);
        // Cut inside a multi-byte character.
        let mut b = r#"{"format":"a","k":"é"#.as_bytes().to_vec();
        b.pop();
        assert_eq!(format_in_prefix(&b).as_deref(), Some("a"));
    }
    /// A truncated run file of this version is damaged, not "of another version".
    #[test]
    fn a_truncated_run_file_is_called_damaged() {
        let bytes = encode_run(&sample_run("20261120-143005-a1b2", None)).unwrap();
        for cut in [bytes.len() / 2, bytes.len() - 3] {
            let e = decode_run(&bytes[..cut]).unwrap_err();
            let b = crate::errors::classify(&e, "s3");
            assert_eq!(
                b.code,
                crate::errors::ErrorCode::UsageInvalidArgument,
                "{e:#}"
            );
            let hint = b.hint.unwrap_or_default();
            assert!(hint.contains("damaged or truncated"), "{cut}: {hint}");
            assert!(!hint.contains("other oprobe versions"), "{hint}");
        }
    }

    use crate::model::*;

    pub(crate) fn sample_run(id: &str, label: Option<&str>) -> Run {
        let p = crate::config::Profile::from_url("memory://");
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
            knobs: &crate::config::Knobs::of(&p),
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
                api: crate::trace::Api::HeadObject,
                service_op: None,
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
        // A prove trial is a child: `last` skips it, and deleting the prove removes it.
        let mut trial = sample_run("20261122-100000-a0a0", None);
        trial.report.run.parent_id = Some("20261122-095959-b1b1".into());
        store.save(&trial).unwrap();
        assert_eq!(
            store.resolve("last").unwrap(),
            store.run_path("20261121-090000-e5f6")
        );
        assert_eq!(
            store.children("20261122-095959-b1b1").unwrap(),
            vec!["20261122-100000-a0a0".to_owned()]
        );
        store
            .save_prove(
                "20261122-095959-b1b1",
                &serde_json::json!({"prove_id": "20261122-095959-b1b1"}),
            )
            .unwrap();
        assert_eq!(store.proves().unwrap().len(), 1);
        assert!(store.delete_prove("20261122-095959-b1b1").unwrap());
        assert!(store.children("20261122-095959-b1b1").unwrap().is_empty());
        assert!(!store.delete_prove("20261122-095959-b1b1").unwrap());
        store.delete("20261120-150000-c3d4").unwrap();
        assert_eq!(store.ids().unwrap().len(), 2);
        assert!(store.delete("20261120-150000-c3d4").is_err());
    }
}
