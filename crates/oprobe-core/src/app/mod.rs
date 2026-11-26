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
//! to the [`EventSink`] (`sink.bus()` carries the `oprobe.events/9` events the app draws from).
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
mod save;

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
pub use save::SavedOperator;

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

    /// Run a stored run's `spec` again: its operator as it was given, its whole workload (the
    /// open-loop rate, the dataset, the seed, …) and every operator value that is not the
    /// default of its URL ([`RunSpec::replay`]), under the same label. Secrets are not in a
    /// spec: they come from where the operator does (its file, the config file, the
    /// environment), as for any run.
    pub fn replaying(spec: &RunSpec) -> Self {
        let (workload, set) = spec.replay();
        Self {
            operators: vec![spec.operator_ref.clone()],
            workload,
            set,
            label: spec.label.clone(),
            ..Default::default()
        }
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

    /// Running a run again resolves to the spec it had: the open-loop rate, the dataset, the
    /// seed, the workload settings without a flag, and the operator's options, layers and
    /// HTTP client (9.1's app rebuilt the run from five form fields and lost the rest).
    #[test]
    fn replaying_a_spec_resolves_to_the_same_spec() {
        let d = tempfile::tempdir().unwrap();
        let config_file = d.path().join("config.toml");
        std::fs::write(
            &config_file,
            "[operators.prod]\nurl = \"s3://bkt/data\"\n\n[operators.prod.options]\n\
             region = \"eu-west-2\"\nsecret_access_key = \"hunter2\"\n\n\
             [operators.prod.layers]\nretry = { max_times = 9 }\n",
        )
        .unwrap();
        let app = App::new(Paths {
            config_file,
            data_dir: d.path().join("data"),
        });
        let workload = WorkloadInput {
            preset: Some("range-reads".into()),
            concurrency: Some(7),
            seed: Some(99),
            duration: Some("3s".into()),
            warmup: Some("1s".into()),
            max_ops: Some(5000),
            rate: Some(250.0),
            objects: Some(12),
            object_size: Some("2MiB".into()),
            access: Some("zipf:1.1".into()),
            persist_dataset: true,
            no_verify: true,
            keep_data: true,
            arrivals: Some("poisson".into()),
            ..Default::default()
        };
        let set = [
            "options.endpoint=http://127.0.0.1:9000",
            "http.version=h1",
            "http.keep_alive=false",
            "layers.concurrent_limit.http=12",
            "layers.timeout=off",
            "read.gap=1MiB",
            "write.concurrent=3",
        ]
        .map(str::to_owned)
        .to_vec();
        let requests = [
            RunRequest {
                operators: vec!["s3://bkt/prefix".into()],
                workload: workload.clone(),
                set: set.clone(),
                label: Some("nightly".into()),
                ..Default::default()
            },
            // A configured operator: its secret is redacted in the spec and comes from the
            // config file again.
            RunRequest {
                operators: vec!["prod".into()],
                workload: WorkloadInput {
                    existing: Some("tables/".into()),
                    objects: None,
                    object_size: None,
                    persist_dataset: false,
                    keep_data: false,
                    ..workload
                },
                set,
                ..Default::default()
            },
            RunRequest::new(["lab://throttle"]),
        ];
        for req in requests {
            let first = app.resolve(&req).unwrap().remove(0);
            let spec = first.spec();
            let again = RunRequest::replaying(&spec);
            let second = app.resolve(&again).unwrap().remove(0);
            assert_eq!(second.spec(), spec, "{:?}", spec.command());
            // The secret itself is back, not its redaction.
            assert_eq!(
                second.profile.operator.options.get("secret_access_key"),
                first.profile.operator.options.get("secret_access_key")
            );
        }
    }
}
