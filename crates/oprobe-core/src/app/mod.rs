//! The service layer (design v5 §1): the only entry point of the front ends.
//!
//! The CLI (`oprobe` binary), the MCP server and the App (TUI/GUI) all call [`App`]: they fill
//! a typed request ([`input`], [`RunRequest`]), call one method and render the typed result
//! (every result implements [`crate::output::CommandOutput`]). None of them builds clap structs
//! or reaches into another front end.
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use oprobe_core::app::{App, ProfileRequest, RunRequest};
//! use oprobe_core::output::Output;
//!
//! let app = App::new(oprobe_core::store::Paths::resolve(None));
//! let mut req = ProfileRequest::default();
//! req.target.target = Some("memory://".into());
//! let resolved = app.resolve(&req)?;
//! let sink = Output::silent();
//! let outcome = app
//!     .run(RunRequest::new(resolved), &sink, oprobe_core::engine::Cancel::new())
//!     .await?;
//! # let _ = outcome; Ok(()) }
//! ```
//!
//! **Long work.** `run`, `tune`, `verify`, `fix` and `baseline_calibrate` are `async` and take
//! a [`Cancel`]; progress and log lines go to the [`EventSink`] (`sink.bus()` carries the
//! `oprobe.events/1` stream). The MCP server runs them in-process; turning them into polled jobs
//! (design v5 §6) only wraps these calls.

pub mod fix;
pub mod input;
pub mod live;
pub mod measure;
pub mod lab;
pub mod next;
pub mod profile;
pub mod run;
pub mod runs;
pub mod tune;
pub mod verify;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Profile;
pub use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError};
use crate::model::{Overrides, RunSpec};
use crate::store::{ConfigFile, Paths};
pub use input::*;
pub use profile::{TuneSettings, tune_options};
pub use run::RunOutput;
pub use tune::TuneOutput;

/// Where a command's log lines, warnings, progress and events go: [`crate::output::Output`].
/// Front ends that consume events in-process use `Output::silent()` and `sink.bus().channel()`.
pub type EventSink = crate::output::Output;

/// The service layer: paths, the (lazily loaded) config file and the global settings given as
/// flags. Cheap to clone; one per process is enough.
#[derive(Clone, Debug)]
pub struct App {
    pub paths: Paths,
    /// Global settings given as flags (for `config show` sources).
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

// ---------------------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------------------

/// A resolved profile: credentials included (never serialized), and the configured target
/// name it came from.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub profile: Profile,
    pub target_name: Option<String>,
}

impl Resolved {
    /// The replayable spec of this profile (secrets redacted).
    pub fn spec(&self) -> RunSpec {
        crate::engine::measure::spec_of(&self.profile, self.target_name.as_deref())
    }
}

/// One measurement (design v5 §1): what to run ([`RunSpec`] plus [`Overrides`]), under which
/// safety rails, with which load generator and on which workers.
///
/// A request built from a [`Resolved`] profile keeps its credentials (`resolved`); one built
/// from a spec alone (a stored run, an MCP call) resolves the target again, as
/// `oprobe run --from` does.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunRequest {
    pub spec: RunSpec,
    /// Changes applied on top of the spec (the `--set` vocabulary).
    #[serde(default)]
    pub overrides: Overrides,
    /// Budgets, read-only, allow-write, dry run, idempotency key.
    #[serde(default)]
    pub safety: Safety,
    /// Saving, report files, `--fail-on`.
    #[serde(default)]
    pub output: OutputOptions,
    /// The profile the spec came from, with credentials (not serialized).
    #[serde(skip)]
    #[schemars(skip)]
    pub resolved: Option<Resolved>,
}

impl RunRequest {
    /// Run `resolved` as it is.
    pub fn new(resolved: Resolved) -> Self {
        Self {
            spec: resolved.spec(),
            resolved: Some(resolved),
            ..Self::from_spec(RunSpec::default())
        }
    }

    /// Replay `spec`: the target is resolved again (credentials from the configured target,
    /// the environment or a profile; never from the spec).
    pub fn from_spec(spec: RunSpec) -> Self {
        Self {
            spec,
            overrides: Overrides::default(),
            safety: Safety::default(),
            output: OutputOptions::default(),
            resolved: None,
        }
    }

    pub fn with_safety(mut self, safety: Safety) -> Self {
        self.safety = safety;
        self
    }

    pub fn with_output(mut self, output: OutputOptions) -> Self {
        self.output = output;
        self
    }
}

/// What [`App::run`] produced.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    /// `safety.dry_run`: the plan; nothing was sent.
    Plan(Box<run::PlanOutput>),
    /// One run.
    Run(Box<run::RunOutput>),
}

impl App {
    /// Assemble and validate a profile (target names, `[defaults]`, flags, `--from`, `--set`).
    pub fn resolve(&self, req: &ProfileRequest) -> Result<Resolved> {
        let ctx = self.ctx(&EventSink::silent());
        let (profile, target_name) = profile::run_profile(&ctx, req)?;
        Ok(Resolved {
            profile,
            target_name,
        })
    }

    /// The profile a [`RunRequest`] measures: its resolved profile or the spec replayed, with
    /// the overrides applied and validated.
    pub fn profile_of(&self, req: &RunRequest) -> Result<Resolved> {
        let mut r = match &req.resolved {
            Some(r) => r.clone(),
            None => {
                let ctx = self.ctx(&EventSink::silent());
                let (profile, target_name) =
                    profile::spec_profile(&ctx, req.spec.clone(), &ProfileRequest::default())?;
                Resolved {
                    profile,
                    target_name,
                }
            }
        };
        if !req.overrides.is_empty() {
            let mut label = r.profile.name.clone();
            req.overrides.apply(&mut r.profile, &mut label)?;
            r.profile.name = label;
        }
        if req.output.label.is_some() {
            r.profile.name = req.output.label.clone();
        }
        r.profile.validate()?;
        Ok(r)
    }

    /// The plan of a run (`--dry-run`): requests, bytes, objects written and deleted, cost,
    /// ETA. Nothing is sent.
    pub fn plan(&self, req: &RunRequest) -> Result<crate::guard::Plan> {
        let r = self.profile_of(req)?;
        let ctx = self.ctx(&EventSink::silent());
        let rails = run::rails(&ctx, &r.profile, &req.safety)?;
        Ok(rails.plan(&r.profile))
    }

    /// Measure: one run, a repeat group, or the plan of a dry run.
    pub async fn run(
        &self,
        req: RunRequest,
        sink: &EventSink,
        cancel: Cancel,
    ) -> Result<RunOutcome> {
        let ctx = self.ctx(sink);
        let r = self.profile_of(&req)?;
        Ok(
            match run::measure(
                &ctx,
                r.profile,
                r.target_name,
                &req.safety,
                &req.output,
                cancel,
            )
            .await?
            {
                run::Measured::Plan(p) => RunOutcome::Plan(p),
                run::Measured::Run(o) => RunOutcome::Run(o),
            },
        )
    }

    /// `oprobe tune`: the throttle-aware search and its recommendation.
    pub async fn tune(
        &self,
        req: TuneRequest,
        sink: &EventSink,
        cancel: Cancel,
    ) -> Result<tune::TuneOutcome> {
        tune::tune(&self.ctx(sink), req, cancel).await
    }

    /// `oprobe verify`: replay a run's findings and judge them.
    pub async fn verify(
        &self,
        req: VerifyRequest,
        sink: &EventSink,
        cancel: Cancel,
    ) -> Result<verify::VerifyRun> {
        verify::verify(&self.ctx(sink), req, cancel).await
    }

    /// `oprobe fix`: diagnose → tune → verify.
    pub async fn fix(
        &self,
        req: FixRequest,
        sink: &EventSink,
        cancel: Cancel,
    ) -> Result<fix::FixOutcome> {
        fix::fix(&self.ctx(sink), req, cancel).await
    }

    /// `oprobe lab`: start the server (it serves until the returned handle is dropped).
    pub async fn lab(&self, req: LabRequest) -> Result<(crate::lab::LabHandle, lab::LabOutput)> {
        lab::lab(&self.ctx(&EventSink::silent()), req).await
    }

    // --- runs ------------------------------------------------------------------------

    pub fn runs_list(&self, req: RunsListRequest) -> Result<runs::RunsList> {
        runs::list(&self.ctx(&EventSink::silent()), req)
    }

    pub fn runs_show(&self, req: RunsShowRequest) -> Result<runs::RunShow> {
        runs::show(&self.ctx(&EventSink::silent()), req)
    }

    pub fn runs_rm(&self, req: RunsRmRequest) -> Result<runs::RemovedRuns> {
        runs::rm(&self.ctx(&EventSink::silent()), req)
    }

}
