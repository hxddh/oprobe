//! The service layer (design v6 §8): the only API the front ends (the CLI and the app) call.
//!
//! ```text
//! impl App {
//!     fn plan(&self, req: &RunRequest) -> Result<RunPlan>;
//!     async fn run(&self, req: RunRequest, sink: EventSink, cancel: Cancel) -> Result<RunOutcome>;
//!     async fn prove(&self, req: ProveRequest, sink: EventSink, cancel: Cancel) -> Result<ProveOutcome>;
//!     fn show(&self, r: RunRef) -> Result<Profile>;
//!     fn list(&self) -> Result<RunList>;
//!     fn rm(&self, r: &[RunRef]) -> Result<()>;
//!     fn trash(&self, r: &[RunRef]) -> Result<TrashedRuns>;   // the app's Delete, with Undo
//!     fn restore(&self, t: &TrashedRuns) -> Result<()>;
//!     async fn lab(&self, req: LabRequest) -> Result<LabHandle>;
//! }
//! ```
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use oprobe_core::app::{App, Cancel, EventSink, RunRequest};
//!
//! let app = App::new(oprobe_core::store::Paths::resolve(None));
//! let req = RunRequest::new(["memory://"]);
//! let outcome = app.run(req, EventSink::silent(), Cancel::new()).await?;
//! # let _ = outcome; Ok(()) }
//! ```
//!
//! **Long work.** `run` and `prove` are `async` and take a [`Cancel`]; progress and log lines go
//! to the [`EventSink`] (`sink.bus()` carries the `oprobe.events/1` events the app draws from).
//!
//! The other modules hold the commands' work: [`run`] and [`runs`] (measuring and the run
//! history), [`prove`], [`lab`], [`profile`] (profile assembly).

pub mod input;
pub mod lab;
pub mod live;
pub mod measure;
pub mod next;
pub mod profile;
pub mod prove;
pub mod run;
pub mod runs;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError};
pub use crate::lab::LabHandle;
use crate::model::RunSpec;
use crate::store::{ConfigFile, Paths};
pub use input::*;
pub use lab::LabOutput;
pub use prove::{ProveObjective, ProveOutcome, ProveOutput, ProveRequest, ProveVerdict};
pub use run::{PlanOutput, RunOutput, RunPlan, RunSet};
pub use runs::{Comparison, Profile, RemovedRuns, RunList, TrashedRuns};

/// Where a command's log lines, warnings, progress and events go: [`crate::output::Output`].
/// Front ends that consume events in-process use `EventSink::silent()` and `sink.bus()`.
pub type EventSink = crate::output::Output;

/// A stored run: an id, an id prefix, `last`, `last~N`, a label, or the path of a `.oprobe`
/// (or report `.json`) file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RunRef(pub String);

impl From<&str> for RunRef {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for RunRef {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for RunRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type CachedConfig = Option<std::result::Result<Arc<ConfigFile>, String>>;

/// The service layer: paths, the (lazily loaded) config file. Cheap to clone; one per process
/// is enough.
#[derive(Clone, Debug)]
pub struct App {
    pub paths: Paths,
    /// Global settings given as flags.
    pub flags: BTreeMap<String, String>,
    config: Arc<RwLock<CachedConfig>>,
}

impl App {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            flags: BTreeMap::new(),
            config: Arc::new(RwLock::new(None)),
        }
    }

    /// Drop any cached `config.toml` so the next [`Self::config`] reloads from disk.
    pub fn invalidate_config(&self) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The config file (empty when missing; an invalid file is a `config.invalid` error).
    pub fn config(&self) -> Result<Arc<ConfigFile>> {
        if let Some(cached) = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return cached
                .clone()
                .map_err(|m| OprobeError::new(ErrorCode::ConfigInvalid, m).into());
        }
        let loaded = ConfigFile::load(&self.paths.config_file)
            .map(Arc::new)
            .map_err(|e| format!("{e:#}"));
        let mut slot = self.config.write().unwrap_or_else(|e| e.into_inner());
        let res = slot.get_or_insert(loaded).clone();
        res.map_err(|m| OprobeError::new(ErrorCode::ConfigInvalid, m).into())
    }

    /// The run store.
    pub fn store(&self) -> crate::store::RunStore {
        self.paths.run_store()
    }

    /// A command context: this app and `sink`.
    pub fn ctx(&self, sink: &EventSink) -> Ctx {
        Ctx {
            out: sink.clone(),
            app: self.clone(),
        }
    }
}

/// One command's context inside the service layer: the [`App`] and the command's sink.
/// Dereferences to the [`App`] (`ctx.paths`, `ctx.config()`, `ctx.store()`).
pub struct Ctx {
    pub out: EventSink,
    app: App,
}

impl Ctx {
    pub fn new(out: EventSink, paths: Paths) -> Self {
        Self {
            out,
            app: App::new(paths),
        }
    }

    pub fn with_app(out: EventSink, app: App) -> Self {
        Self { out, app }
    }

    pub fn app(&self) -> &App {
        &self.app
    }
}

impl std::ops::Deref for Ctx {
    type Target = App;
    fn deref(&self) -> &App {
        &self.app
    }
}

impl std::ops::DerefMut for Ctx {
    fn deref_mut(&mut self) -> &mut App {
        &mut self.app
    }
}

/// A resolved profile: the operator with its credentials (never serialized) and where it
/// came from.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub profile: crate::config::Profile,
    /// The configured operator's name, when one was used.
    pub target_name: Option<String>,
    /// The operator as given (URL, operator file, configured name).
    pub operator_ref: String,
}

impl Resolved {
    /// The replayable spec of this profile (secrets redacted).
    pub fn spec(&self) -> RunSpec {
        RunSpec::new(&self.profile, self.operator_ref.clone())
    }
}

/// `oprobe run OPERATOR…`: the operators, run one after another on the same workload.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RunRequest {
    /// URLs, operator files or configured names (at least one)
    pub operators: Vec<String>,
    pub workload: WorkloadInput,
    pub sla: SlaInput,
    /// `--set PATH=VALUE` pairs (operator paths, workload and SLA keys), applied to every
    /// operator
    pub set: Vec<String>,
    /// Label stored in the reports
    pub label: Option<String>,
    /// Budgets (one for the whole command), read-only, dry run
    pub safety: Safety,
    /// Saving and report files
    pub output: OutputOptions,
}

impl RunRequest {
    /// Run `operators` with the default workload.
    pub fn new<I, S>(operators: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            operators: operators.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    pub fn with_safety(mut self, safety: Safety) -> Self {
        self.safety = safety;
        self
    }

    /// The profile request of one of the operators.
    fn profile_request(&self, operator: &str) -> ProfileRequest {
        ProfileRequest {
            operator: operator.to_owned(),
            workload: self.workload.clone(),
            sla: self.sla.clone(),
            set: self.set.clone(),
            label: self.label.clone(),
        }
    }
}

/// What [`App::run`] produced.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    /// `safety.dry_run`: the plans; nothing was sent.
    Plan(Box<RunPlan>),
    /// The runs, one per operator, in order.
    Run(Box<RunSet>),
}

impl App {
    /// Resolve and validate the profile of every operator of `req`.
    pub fn resolve(&self, req: &RunRequest) -> Result<Vec<Resolved>> {
        if req.operators.is_empty() {
            return Err(OprobeError::new(ErrorCode::UsageUnknownTarget, "no operator given")
                .with_hint("pass a URL (s3://bucket/prefix, lab://demo), an op.toml file or a configured name")
                .into());
        }
        let ctx = self.ctx(&EventSink::silent());
        let mut resolved = req
            .operators
            .iter()
            .map(|op| {
                let (profile, source) = profile::run_profile(&ctx, &req.profile_request(op))?;
                Ok(Resolved {
                    profile,
                    target_name: source.name(),
                    operator_ref: op.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // One workload for every operator (K2): the first operator's (its `lab://` scenario's
        // defaults, then `[defaults]`, the flags and `--set`), never each lab's own defaults.
        if let Some((first, rest)) = resolved.split_first_mut() {
            for r in rest {
                r.profile.workload = first.profile.workload.clone();
                r.profile.validate()?;
            }
        }
        Ok(resolved)
    }

    /// The plan of a run (`--dry-run`): per operator the requests, bytes, objects written and
    /// deleted and the ETA. Nothing is sent.
    pub fn plan(&self, req: &RunRequest) -> Result<RunPlan> {
        let ctx = self.ctx(&EventSink::silent());
        let mut plans = self
            .resolve(req)?
            .iter()
            .map(|r| Ok(run::rails(&ctx, &r.profile, &req.safety)?.plan(&r.profile)))
            .collect::<Result<Vec<_>>>()?;
        // The budgets cover the whole command: what all the operators spend together.
        if plans.len() > 1 {
            let requests = plans.iter().map(|p| p.estimated_requests_count).sum();
            let bytes = plans.iter().map(|p| p.estimated_bytes).sum();
            let eta_s = plans.iter().map(|p| p.eta_s).sum();
            let n = plans.len();
            if let Some(last) = plans.last_mut() {
                let together = crate::guard::budget_warnings(&last.budgets, requests, bytes, eta_s);
                last.warnings.extend(
                    together
                        .into_iter()
                        .map(|w| format!("the {n} operators together: {w}")),
                );
            }
        }
        Ok(RunPlan { plans })
    }

    /// Measure every operator of `req`, one after another, under one guard (the budgets cover
    /// the whole command); or plan them (`safety.dry_run`).
    pub async fn run(
        &self,
        req: RunRequest,
        sink: EventSink,
        cancel: Cancel,
    ) -> Result<RunOutcome> {
        if req.safety.dry_run {
            return Ok(RunOutcome::Plan(Box::new(self.plan(&req)?)));
        }
        let resolved = self.resolve(&req)?;
        let ctx = self.ctx(&sink);
        let guard = run::guard_for(
            &ctx,
            &resolved[0].profile,
            &run::budgets(&ctx, &req.safety)?,
        );
        let mut runs = Vec::new();
        for r in resolved {
            if cancel.is_cancelled() {
                break;
            }
            match run::measure_guarded(
                &ctx,
                r.profile,
                r.target_name,
                Some(r.operator_ref),
                &req.safety,
                &req.output,
                guard.clone(),
                &cancel,
            )
            .await?
            {
                run::Measured::Run(o) => {
                    // A spent budget covers the whole command: the operators after it do
                    // not run (K10: what ran is printed, then the error, exit 4).
                    let stop = o.breach.is_some();
                    runs.push(*o);
                    if stop {
                        break;
                    }
                }
                run::Measured::Plan(_) => unreachable!("not a dry run"),
            }
        }
        Ok(RunOutcome::Run(Box::new(RunSet::new(runs))))
    }

    /// `oprobe prove`: A/B-test a change of a run's operator.
    pub async fn prove(
        &self,
        req: ProveRequest,
        sink: EventSink,
        cancel: Cancel,
    ) -> Result<ProveOutcome> {
        prove::prove(&self.ctx(&sink), req, cancel).await
    }

    /// Prove the remedy of finding `code` of run `run` exactly as `oprobe prove RUN CODE` does:
    /// the remedy's size is searched with short trials, then A and B are interleaved, within
    /// `budget` (a wall-time budget such as `2m`; `None`: none). The one path for a front end's
    /// "prove this finding".
    pub async fn prove_finding(
        &self,
        run: RunRef,
        code: &str,
        budget: Option<String>,
        sink: EventSink,
        cancel: Cancel,
    ) -> Result<ProveOutput> {
        let req = ProveRequest {
            budget,
            ..ProveRequest::for_finding(run.0, code)
        };
        match self.prove(req, sink, cancel).await? {
            ProveOutcome::Proved(p) => Ok(*p),
            ProveOutcome::Plan(_) => unreachable!("not a dry run"),
        }
    }

    /// A stored run (or a run file) with its rows.
    pub fn show(&self, r: RunRef) -> Result<Profile> {
        runs::show(self, &r)
    }

    /// Compare two or more stored runs or `.oprobe` files side by side (`oprobe show A B`).
    pub fn compare(&self, r: &[RunRef], detail: bool) -> Result<Comparison> {
        runs::compare(self, r, detail)
    }

    /// The stored prove `r` names (`oprobe show PROVE_ID`), or `None` when it names no prove.
    pub fn show_prove(&self, r: &RunRef) -> Result<Option<runs::ShownProve>> {
        runs::show_prove(self, r)
    }

    /// The stored runs you made, newest first, and the proves (trials hidden).
    pub fn list(&self) -> Result<RunList> {
        runs::list(self, false)
    }

    /// [`App::list`] with prove trial runs included (`show --list --all`).
    pub fn list_all(&self) -> Result<RunList> {
        runs::list(self, true)
    }

    /// Delete stored runs (a file outside the store is refused).
    pub fn rm(&self, r: &[RunRef]) -> Result<()> {
        runs::rm(self, r).map(|_| ())
    }

    /// Move stored runs to oprobe's trash (`<data>/trash/`, kept a week) instead of deleting
    /// them: [`App::restore`] puts them back (the app's Undo).
    pub fn trash(&self, r: &[RunRef]) -> Result<TrashedRuns> {
        runs::trash(self, r)
    }

    /// Put runs [`App::trash`] moved back into the store.
    pub fn restore(&self, t: &TrashedRuns) -> Result<()> {
        runs::restore(self, t)
    }

    /// The names of the operators the config file declares (`[operators.NAME]`), sorted.
    pub fn configured_operators(&self) -> Result<Vec<String>> {
        Ok(self.config()?.operators.keys().cloned().collect())
    }

    /// Apply `PATH=VALUE` `edits` (such as a proven change) to an existing `op.toml` file in
    /// place, preserving comments, whitespace, unedited options, `${ENV}` placeholders and
    /// relative paths.
    pub fn save_operator_edits(
        &self,
        path: &std::path::Path,
        edits: &[String],
    ) -> Result<crate::operator::OperatorSpec> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("cannot read operator file `{}`: {e}", path.display()),
            )
        })?;
        // Validate existing file first so syntax/schema errors report against it cleanly.
        let _ = crate::operator::OperatorSpec::parse(&raw).map_err(|e| {
            let mut o = OprobeError::new(e.code, format!("`{}`: {}", path.display(), e.message));
            o.hint = e.hint;
            o
        })?;
        let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("`{}`: {e}", path.display()),
            )
        })?;
        let mut pairs = Vec::with_capacity(edits.len());
        for e in edits {
            let (k, v) = crate::input::key_value("edit", e)?;
            if v.trim() != crate::model::spec::REDACTED {
                pairs.push((k, v));
            }
        }
        crate::operator::patch_toml_table(doc.as_table_mut(), &pairs)?;
        let out_text = doc.to_string();
        let spec = crate::operator::OperatorSpec::parse(&out_text).map_err(|e| {
            let mut o = OprobeError::new(e.code, format!("`{}`: {}", path.display(), e.message));
            o.hint = e.hint;
            o
        })?;
        spec.validate()?;
        crate::store::atomic_write(path, out_text.as_bytes())?;
        Ok(spec)
    }

    /// Write an operator `base` + `edits` to an explicit `.toml` file `path` (used by the UI's
    /// "Save operator as…"). When `source_file` is the same file as `path`, patches in place to
    /// preserve comments and unredacted secrets; otherwise writes a clean operator file from
    /// `base` + `edits`.
    pub fn write_operator_file(
        &self,
        source_file: Option<&std::path::Path>,
        path: &std::path::Path,
        base: &crate::operator::OperatorSpec,
        edits: &[String],
    ) -> Result<crate::operator::OperatorSpec> {
        let same_file = source_file.is_some_and(|src| {
            src == path
                || (src.canonicalize().ok().zip(path.canonicalize().ok()))
                    .is_some_and(|(a, b)| a == b)
        });
        if same_file && path.is_file() {
            return self.save_operator_edits(path, edits);
        }
        let mut pairs = Vec::with_capacity(edits.len());
        for e in edits {
            let (k, v) = crate::input::key_value("edit", e)?;
            if v.trim() != crate::model::spec::REDACTED {
                pairs.push((k, v));
            }
        }
        let mut spec = base.clone();
        spec.options
            .retain(|_, v| v.trim() != crate::model::spec::REDACTED);
        spec.apply_edits(&pairs)?;
        spec.validate()?;
        crate::store::atomic_write(path, spec.to_toml().as_bytes())?;
        Ok(spec)
    }

    /// Save an operator `base` with `edits` applied either to an `op.toml` file (creating or
    /// updating it in place) or to `[operators.NAME]` in `config.toml` when `dest` is a bare
    /// operator name. Returns a short human description of where it was written.
    pub fn save_operator_target(
        &self,
        dest: &str,
        base: &crate::operator::OperatorSpec,
        edits: &[String],
    ) -> Result<String> {
        let dest = dest.trim();
        if dest.is_empty() {
            return Err(
                OprobeError::new(ErrorCode::UsageInvalidArgument, "empty --save target")
                    .with_hint("pass an op.toml path or an operator name for config.toml")
                    .into(),
            );
        }
        let path = std::path::Path::new(dest);
        let is_file_target = dest.ends_with(".toml")
            || dest.contains('/')
            || dest.contains(std::path::MAIN_SEPARATOR)
            || path.is_file();
        let mut pairs = Vec::with_capacity(edits.len());
        for e in edits {
            let (k, v) = crate::input::key_value("edit", e)?;
            if v.trim() != crate::model::spec::REDACTED {
                pairs.push((k, v));
            }
        }
        if is_file_target {
            // Patch in place when `path` already describes the same service URL (preserving its
            // comments and unredacted secrets); otherwise write `base + edits` cleanly.
            let same_service = path.is_file()
                && std::fs::read_to_string(path)
                    .ok()
                    .and_then(|r| crate::operator::OperatorSpec::parse(&r).ok())
                    .is_some_and(|existing| existing.display_url() == base.display_url());
            if same_service {
                self.save_operator_edits(path, edits)?;
            } else {
                self.write_operator_file(None, path, base, edits)?;
            }
            return Ok(dest.to_owned());
        }

        // Bare operator name -> `[operators.NAME]` in `config.toml`, preserving comments and
        // `[defaults]`.
        let _ = ConfigFile::load(&self.paths.config_file)?;
        let raw = match std::fs::read_to_string(&self.paths.config_file) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!(
                    "invalid config file `{}`: {e}",
                    self.paths.config_file.display()
                ),
            )
        })?;
        if !doc.contains_key("operators") || !doc["operators"].is_table() {
            let mut ops = toml_edit::Table::new();
            ops.set_implicit(true);
            doc["operators"] = toml_edit::Item::Table(ops);
        }
        let ops_tbl = doc["operators"].as_table_mut().expect("operators table");
        if let Some(toml_edit::Item::Value(toml_edit::Value::InlineTable(it))) = ops_tbl.get(dest) {
            let tbl = it.clone().into_table();
            ops_tbl[dest] = toml_edit::Item::Table(tbl);
        }
        if let Some(toml_edit::Item::Table(existing_tbl)) = ops_tbl.get_mut(dest) {
            crate::operator::patch_toml_table(existing_tbl, &pairs)?;
        } else {
            let mut spec = base.clone();
            spec.options
                .retain(|_, v| v.trim() != crate::model::spec::REDACTED);
            spec.apply_edits(&pairs)?;
            spec.validate()?;
            let sub: toml_edit::DocumentMut = spec
                .to_toml()
                .parse()
                .expect("serialized OperatorSpec is valid TOML");
            ops_tbl[dest] = toml_edit::Item::Table(sub.as_table().clone());
        }
        let out_text = doc.to_string();
        // Validate the full updated config before writing to disk.
        let validated: ConfigFile = toml::from_str(&out_text).map_err(|e| {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!(
                    "invalid config file `{}`: {e}",
                    self.paths.config_file.display()
                ),
            )
        })?;
        validated.defaults.validate()?;
        for (name, op) in &validated.operators {
            op.validate().map_err(|e| {
                let mut o = OprobeError::new(
                    ErrorCode::ConfigInvalid,
                    format!(
                        "operator `{name}` in `{}`: {}",
                        self.paths.config_file.display(),
                        e.message
                    ),
                );
                o.hint = e.hint;
                o
            })?;
        }
        crate::store::atomic_write(&self.paths.config_file, out_text.as_bytes())?;
        self.invalidate_config();
        Ok(format!(
            "[operators.{dest}] in {}",
            self.paths.config_file.display()
        ))
    }

    /// The run ids `r` names (to report what [`App::rm`] removes).
    pub fn run_ids(&self, r: &[RunRef]) -> Result<Vec<String>> {
        runs::stored_ids(self, r)
    }

    /// `oprobe lab`: start the server (it serves until the returned handle is dropped).
    pub async fn lab(&self, req: LabRequest) -> Result<LabHandle> {
        lab::lab(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_operator_edits_preserves_secrets_env_and_relative_ca_cert() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("op.toml");
        std::fs::write(
            &p,
            "# keep user header comment\nurl = \"s3://b/x\"\n[options]\n# keep secret placeholder\nregion = \"us-east-1\"\nsecret_access_key = \"${AWS_SECRET_ACCESS_KEY}\"\n[http]\nca_cert = \"ca.pem\"\n",
        )
        .unwrap();
        let app = App::new(crate::store::Paths {
            config_file: dir.path().join("config.toml"),
            data_dir: dir.path().join("data"),
        });
        let updated = app
            .save_operator_edits(
                &p,
                &[
                    "layers.concurrent_limit.http=24".to_owned(),
                    "layers.retry.jitter=on".to_owned(),
                    "options.secret_access_key=<redacted>".to_owned(),
                ],
            )
            .unwrap();
        assert_eq!(updated.http_limit(), Some(24));
        let disk = std::fs::read_to_string(&p).unwrap();
        assert!(disk.contains("# keep user header comment"), "{disk}");
        assert!(disk.contains("# keep secret placeholder"), "{disk}");
        assert!(
            disk.contains("secret_access_key = \"${AWS_SECRET_ACCESS_KEY}\""),
            "{disk}"
        );
        assert!(disk.contains("ca_cert = \"ca.pem\""), "{disk}");
        assert!(disk.contains("http = 24"), "{disk}");
        assert!(disk.contains("jitter = true"), "{disk}");
    }

    #[test]
    fn save_operator_target_preserves_config_comments_and_invalidates_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "# global defaults comment\n[defaults]\nduration = \"5s\"\n\n# prod operator comment\n[operators.prod]\nurl = \"s3://b/x\"\n[operators.prod.options]\nregion = \"us-east-1\"\nsecret_access_key = \"${AWS_SECRET}\"\n",
        )
        .unwrap();
        let app = App::new(crate::store::Paths {
            config_file: cfg_path.clone(),
            data_dir: dir.path().join("data"),
        });
        // Prime cache before saving.
        assert_eq!(app.config().unwrap().operators["prod"].http_limit(), None);

        let base = crate::operator::OperatorSpec::from_url("s3://b/x");
        app.save_operator_target(
            "prod",
            &base,
            &["layers.concurrent_limit.http=32".to_owned()],
        )
        .unwrap();

        // Cache was invalidated so subsequent `app.config()` reads the new limit immediately.
        assert_eq!(
            app.config().unwrap().operators["prod"].http_limit(),
            Some(32)
        );
        let disk = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(disk.contains("# global defaults comment"), "{disk}");
        assert!(disk.contains("# prod operator comment"), "{disk}");
        assert!(
            disk.contains("secret_access_key = \"${AWS_SECRET}\""),
            "{disk}"
        );
        assert!(disk.contains("http = 32"), "{disk}");
    }
}
