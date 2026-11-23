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
use std::sync::{Arc, OnceLock};

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
pub use run::{RunOutput, RunPlan, RunSet};
pub use runs::{Profile, RemovedRuns, RunList, TrashedRuns};

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

/// The service layer: paths, the (lazily loaded) config file. Cheap to clone; one per process
/// is enough.
#[derive(Clone, Debug)]
pub struct App {
    pub paths: Paths,
    /// Global settings given as flags.
    pub flags: BTreeMap<String, String>,
    config: Arc<OnceLock<std::result::Result<ConfigFile, String>>>,
}

impl App {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            flags: BTreeMap::new(),
            config: Arc::new(OnceLock::new()),
        }
    }

    /// The config file (empty when missing; an invalid file is a `config.invalid` error).
    pub fn config(&self) -> Result<&ConfigFile> {
        self.config
            .get_or_init(|| ConfigFile::load(&self.paths.config_file).map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|m| OprobeError::new(ErrorCode::ConfigInvalid, m.clone()).into())
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
        req.operators
            .iter()
            .map(|op| {
                let (profile, source) = profile::run_profile(&ctx, &req.profile_request(op))?;
                Ok(Resolved {
                    profile,
                    target_name: source.name(),
                    operator_ref: op.clone(),
                })
            })
            .collect()
    }

    /// The plan of a run (`--dry-run`): per operator the requests, bytes, objects written and
    /// deleted and the ETA. Nothing is sent.
    pub fn plan(&self, req: &RunRequest) -> Result<RunPlan> {
        let ctx = self.ctx(&EventSink::silent());
        let plans = self
            .resolve(req)?
            .iter()
            .map(|r| Ok(run::rails(&ctx, &r.profile, &req.safety)?.plan(&r.profile)))
            .collect::<Result<Vec<_>>>()?;
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
                run::Measured::Run(o) => runs.push(*o),
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

    /// A stored run (or a run file) with its rows.
    pub fn show(&self, r: RunRef) -> Result<Profile> {
        runs::show(self, &r)
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
