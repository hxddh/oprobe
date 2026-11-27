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

/// How long a front end waits for a job it cancelled before it gives up on it (the app
/// quitting): the abort of one multipart upload may take 30 s to give up on a service that
/// does not answer (the engine's abort timeout), and removing the test data gets as long
/// again.
pub const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

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

/// The error of a config file that did not load, from its cached message and hint.
fn config_error((message, hint): (String, Option<String>)) -> anyhow::Error {
    let mut e = OprobeError::new(ErrorCode::ConfigInvalid, message);
    e.hint = hint;
    e.into()
}

/// The loaded config file, or its error: the message and the hint.
type CachedConfig = Option<std::result::Result<Arc<ConfigFile>, (String, Option<String>)>>;

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
            return cached.clone().map_err(config_error);
        }
        // The error is kept whole, its hint too (9.3.3 kept its text only, and the config
        // file's error got the operator file's hint).
        let loaded = ConfigFile::load(&self.paths.config_file)
            .map(Arc::new)
            .map_err(|e| {
                let hint = e.downcast_ref::<OprobeError>().and_then(|x| x.hint.clone());
                (format!("{e:#}"), hint)
            });
        let mut slot = self.config.write().unwrap_or_else(|e| e.into_inner());
        let res = slot.get_or_insert(loaded).clone();
        res.map_err(config_error)
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

/// What a front end's New Run form shows and edits of a run: its operators, the preset, the
/// duration, the concurrency and an existing prefix. Everything else a run has (the open-loop
/// rate, the dataset, the seed, the operator's `--set` values) has no field: it comes from
/// the run being repeated, if there is one ([`RunForm::request`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunForm {
    /// URLs, operator files or configured names
    pub operators: Vec<String>,
    pub preset: Option<String>,
    pub duration: Option<String>,
    pub concurrency: Option<usize>,
    /// Profile existing objects under this prefix (read-only)
    pub existing: Option<String>,
}

impl RunForm {
    /// The fields as they show `req`.
    pub fn of(req: &RunRequest) -> Self {
        let w = &req.workload;
        Self {
            operators: req.operators.clone(),
            preset: w.preset.clone(),
            duration: w.duration.clone(),
            concurrency: w.concurrency,
            existing: w.existing.clone(),
        }
    }

    /// The request the form sends: its fields, over what `repeated` (the request of the run
    /// being repeated, [`App::replaying`]) has no field for.
    ///
    /// - While the form names the operators of `repeated`, the operator's values are kept
    ///   (its `--set` pairs: an endpoint, a layer) and the label. Dropping them would send
    ///   the run to another service than the one it repeats.
    /// - While it names its preset too, the rest of the workload is kept (the rate, the
    ///   dataset, the seed, the settings without a flag). Another preset is another workload:
    ///   it starts from its own defaults.
    /// - Other operators are another run: nothing of `repeated` is kept.
    pub fn request(&self, repeated: Option<&RunRequest>) -> RunRequest {
        let repeated = repeated.filter(|r| r.operators == self.operators);
        let same_workload = repeated.is_some_and(|r| r.workload.preset == self.preset);
        let mut req = RunRequest::new(self.operators.iter().cloned());
        if let Some(r) = repeated {
            req.label = r.label.clone();
            req.set = r
                .set
                .iter()
                .filter(|pair| same_workload || crate::model::spec::is_operator_set(pair))
                .cloned()
                .collect();
            if same_workload {
                req.workload = r.workload.clone();
            }
        }
        req.workload.preset = self.preset.clone();
        req.workload.duration = self.duration.clone();
        req.workload.concurrency = self.concurrency;
        req.workload.existing = self.existing.clone();
        req.safety.read_only = self.existing.is_some();
        req
    }

    /// The command of the request the form sends while it repeats `repeated` (it still names
    /// its operators); `None` once it names others.
    pub fn repeat_command(&self, repeated: &RunRequest) -> Option<String> {
        (repeated.operators == self.operators).then(|| self.request(Some(repeated)).command())
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

    /// The arguments of the `oprobe` command that sends this request: `run`, the operators,
    /// the workload flags, `--set` for every workload setting without a flag and for every
    /// pair of `set`, then the safety flags. (The label has no flag.)
    pub fn args(&self) -> Vec<String> {
        let w = &self.workload;
        let mut a: Vec<String> = vec!["run".into()];
        a.extend(self.operators.iter().cloned());
        let flags = [
            ("--preset", w.preset.clone()),
            ("-c", w.concurrency.map(|c| c.to_string())),
            ("-d", w.duration.clone()),
            ("--rate", w.rate.map(|r| r.to_string())),
            ("--existing", w.existing.clone()),
            ("--objects", w.objects.map(|n| n.to_string())),
            ("--object-size", w.object_size.clone()),
            ("--seed", w.seed.map(|s| s.to_string())),
        ];
        let on = |b: bool, v: &str| b.then(|| v.to_owned());
        let sets = [
            ("warmup", w.warmup.clone()),
            ("max_ops", w.max_ops.map(|n| n.to_string())),
            ("access", w.access.clone()),
            ("persist", on(w.persist_dataset, "on")),
            ("verify", on(w.no_verify, "off")),
            ("cleanup", on(w.keep_data, "off")),
            ("arrivals", w.arrivals.clone()),
            ("shape", w.shape.clone()),
        ];
        let s = &self.safety;
        let safety = [
            ("--max-requests", s.max_requests.map(|n| n.to_string())),
            ("--max-bytes", s.max_bytes.clone()),
            ("--max-duration", s.max_duration.clone()),
        ];
        for (flag, value) in flags {
            if let Some(v) = value {
                a.extend([flag.to_owned(), v]);
            }
        }
        let sets = sets
            .into_iter()
            .filter_map(|(k, v)| Some(format!("{k}={}", v?)))
            .chain(self.set.iter().cloned());
        for set in sets {
            a.extend(["--set".to_owned(), set]);
        }
        // `--existing` implies it.
        if s.read_only && w.existing.is_none() {
            a.push("--read-only".into());
        }
        for (flag, value) in safety {
            if let Some(v) = value {
                a.extend([flag.to_owned(), v]);
            }
        }
        if s.dry_run {
            a.push("--dry-run".into());
        }
        a
    }

    /// The `oprobe run …` command line that sends this request.
    pub fn command(&self) -> String {
        crate::model::spec::command_line(&self.args())
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
    /// Run a stored run's `spec` again: its operator as it was given, its whole workload (the
    /// open-loop rate, the dataset, the seed, …) and every operator value that is not what
    /// the reference names today (the operator file's, the configured operator's, or the
    /// default operator of a URL: [`RunSpec::replay_on`]), under the same label. Secrets are
    /// not in a spec: they come from where the operator does (its file, the config file, the
    /// environment), as for any run. A reference that no longer resolves is kept as it is
    /// (running it says so), its values those that differ from its URL's defaults. One that
    /// now names another operator (another URL or endpoint) is replaced by the run's own URL:
    /// that operator's credentials are never sent to the run's endpoint.
    pub fn replaying(&self, spec: &RunSpec) -> RunRequest {
        let given = self.config().ok().map(|cfg| {
            crate::operator::resolve::replayed_operator(
                &spec.operator_ref,
                &spec.profile.operator,
                &cfg,
            )
        });
        let (operator, (workload, set)) = match &given {
            Some(Ok(Some(operator))) => (spec.operator_ref.clone(), spec.replay_on(operator)),
            // The reference names another operator now: run the run's own URL, with its
            // values, and none of that operator's credentials.
            Some(Err(_)) => (spec.profile.operator.url.clone(), spec.replay()),
            _ => (spec.operator_ref.clone(), spec.replay()),
        };
        RunRequest {
            operators: vec![operator],
            workload,
            set,
            label: spec.label.clone(),
            ..Default::default()
        }
    }

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
                    operator_ref: crate::operator::resolve::stored_ref(op),
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
        let guard = crate::engine::measure::guard_for(&run::budgets(&ctx, &req.safety)?);
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
    pub fn rm(&self, r: &[RunRef]) -> Result<RemovedRuns> {
        runs::rm(self, r)
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

    /// An app on a temporary directory whose config file declares `[operators.prod]` (with a
    /// secret and a layer of its own), and an operator file `op.toml` beside it.
    fn app_with_operators(d: &std::path::Path) -> (App, String) {
        let config_file = d.join("config.toml");
        std::fs::write(
            &config_file,
            "[operators.prod]\nurl = \"s3://bkt/data\"\n\n[operators.prod.options]\n\
             region = \"eu-west-2\"\nsecret_access_key = \"hunter2\"\n\n\
             [operators.prod.layers]\nretry = { max_times = 9 }\n",
        )
        .unwrap();
        let file = d.join("op.toml");
        std::fs::write(
            &file,
            "url = \"s3://bkt/files\"\n\n[options]\nendpoint = \"http://127.0.0.1:9000\"\n\
             region = \"us-east-1\"\n\n[layers]\ntimeout = { op = \"7s\" }\n\n[read]\nconcurrent = 3\n",
        )
        .unwrap();
        let app = App::new(Paths {
            config_file,
            data_dir: d.join("data"),
        });
        (app, file.display().to_string())
    }

    /// S1: a replay never sends the credentials of an operator other than the run's. 9.3.3
    /// stored an operator file as typed and took every redacted secret from whatever file the
    /// reference named at replay time (another directory's `op.toml`, or the file edited to
    /// another bucket), and sent it to the run's endpoint.
    #[test]
    fn a_replay_takes_secrets_only_from_the_operator_the_run_used() {
        let d = tempfile::tempdir().unwrap();
        let (app, _) = app_with_operators(d.path());
        let file = d.path().join("keys.toml");
        let write = |url: &str, key: &str| {
            std::fs::write(
                &file,
                format!(
                    "url = \"{url}\"\n\n[options]\nendpoint = \"http://127.0.0.1:9000\"\n\
                     access_key_id = \"{key}\"\nsecret_access_key = \"{key}-secret\"\n"
                ),
            )
            .unwrap();
        };
        write("s3://mine/data", "MYKEY");
        // A relative reference is stored as the file's canonical path.
        let rel = d.path().join("sub/../keys.toml").display().to_string();
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        let run = app.resolve(&RunRequest::new([rel])).unwrap().remove(0);
        let spec = run.spec();
        assert_eq!(
            spec.operator_ref,
            std::fs::canonicalize(&file).unwrap().display().to_string()
        );
        let ctx = app.ctx(&EventSink::silent());
        // The same operator: its secrets come back.
        let p = profile::spec_profile(&ctx, spec.clone()).unwrap();
        assert_eq!(p.operator.options.get("access_key_id").unwrap(), "MYKEY");
        // The file now names another bucket with other keys: they are not taken.
        write("s3://theirs/data", "OTHERPROJECTKEY");
        let e = profile::spec_profile(&ctx, spec.clone()).unwrap_err();
        assert!(
            e.to_string().contains("not the operator the run used"),
            "{e}"
        );
        let again = app.replaying(&spec);
        assert_eq!(again.operators, vec!["s3://mine/data".to_owned()]);
        let replayed = app.resolve(&again).unwrap().remove(0);
        assert!(
            replayed
                .profile
                .operator
                .options
                .values()
                .all(|v| !v.contains("OTHERPROJECTKEY")),
            "{:?}",
            replayed.profile.operator.options
        );
    }

    /// Every leaf of `v` as `(path, value)`.
    fn leaves(v: &serde_json::Value, path: &str, out: &mut BTreeMap<String, serde_json::Value>) {
        match v {
            serde_json::Value::Object(o) => {
                for (k, v) in o {
                    leaves(v, &format!("{path}.{k}"), out);
                }
            }
            // An array is one value (the mix: its order and its weights).
            other => {
                out.insert(path.to_owned(), other.clone());
            }
        }
    }

    /// Running a run again resolves to the spec it had (T8): every field of the workload (the
    /// open-loop rate, its arrival process and shape, the dataset with its partitions and
    /// size distribution, the mix, the seed, the settings without a flag) and the operator's
    /// options, layers and HTTP client. 9.1's app rebuilt the run from five form fields and
    /// lost the rest; 9.2's test left out `mix`, `shape`, `partitions` and the size
    /// distributions.
    #[test]
    fn replaying_a_spec_resolves_to_the_same_spec() {
        let d = tempfile::tempdir().unwrap();
        let (app, op_file) = app_with_operators(d.path());
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
        let with = |more: &[&str]| -> Vec<String> {
            set.iter()
                .cloned()
                .chain(more.iter().map(|s| (*s).to_owned()))
                .collect()
        };
        let requests = [
            RunRequest {
                operators: vec!["s3://bkt/prefix".into()],
                workload: workload.clone(),
                set: with(&[
                    "partitions=7",
                    r#"mix=[{"op":"read","size":"64KiB","weight":3},{"op":"stat"},{"op":"list","recursive":true,"weight":2}]"#,
                ]),
                label: Some("nightly".into()),
                ..Default::default()
            },
            // A rate that changes over the window, sizes drawn from a distribution.
            RunRequest {
                operators: vec!["s3://bkt/prefix".into()],
                workload: WorkloadInput {
                    preset: Some("small-files".into()),
                    rate: None,
                    arrivals: None,
                    shape: Some("ramp:10..500/30s".into()),
                    object_size: Some("dist:lognormal(256KiB,1.5)".into()),
                    ..workload.clone()
                },
                set: with(&["partitions=2"]),
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
                    ..workload.clone()
                },
                set: set.clone(),
                ..Default::default()
            },
            // An operator file with values of its own.
            RunRequest {
                operators: vec![op_file.clone()],
                workload: workload.clone(),
                set: vec!["layers.retry.max_times=2".into(), "read.gap=1MiB".into()],
                ..Default::default()
            },
            RunRequest::new(["lab://throttle"]),
        ];
        let mut specs = Vec::new();
        for req in requests {
            let first = app.resolve(&req).unwrap().remove(0);
            let spec = first.spec();
            let again = app.replaying(&spec);
            let second = app.resolve(&again).unwrap().remove(0);
            assert_eq!(second.spec(), spec, "{:?}", again.command());
            // The secret itself is back, not its redaction.
            assert_eq!(
                second.profile.operator.options.get("secret_access_key"),
                first.profile.operator.options.get("secret_access_key")
            );
            // The command of the request is a run of the same spec too.
            assert_eq!(again.args()[0], "run");
            specs.push(spec);
        }
        // Every field: each leaf of a spec's workload is varied by one of the requests above
        // (a field added to the workload fails here until a request sets it).
        let default = app
            .resolve(&RunRequest::new(["s3://bkt/prefix"]))
            .unwrap()
            .remove(0)
            .spec();
        let flat = |s: &RunSpec| {
            let mut out = BTreeMap::new();
            let mut v = serde_json::to_value(s).unwrap();
            // The dataset and the mix as the run has them, the preset's defaults resolved.
            let w = s.resolved_profile().workload;
            v["profile"]["workload"]["dataset"] =
                serde_json::to_value(w.effective_dataset()).unwrap();
            v["profile"]["workload"]["mix"] = serde_json::to_value(w.effective_mix()).unwrap();
            leaves(&v["profile"]["workload"], "workload", &mut out);
            for k in ["seed", "existing", "label"] {
                out.insert(k.to_owned(), v[k].clone());
            }
            out
        };
        let base = flat(&default);
        let varied: Vec<_> = specs.iter().map(flat).collect();
        let paths: std::collections::BTreeSet<&String> = varied
            .iter()
            .flat_map(|v| v.keys())
            .chain(base.keys())
            .collect();
        for field in [
            "workload.mix",
            "workload.shape",
            "workload.arrivals",
            "workload.dataset.partitions",
            "workload.dataset.size_dist",
            "workload.dataset.existing_prefix",
            "workload.rate",
            "seed",
        ] {
            assert!(paths.iter().any(|p| *p == field), "{field} of {paths:?}");
        }
        for path in paths {
            assert!(
                varied.iter().any(|v| v.get(path) != base.get(path)),
                "no request of this test sets `{path}`: the round trip does not cover it"
            );
        }
    }

    /// A configured operator and an operator file are read again when the run is repeated:
    /// the request names what the run changed on top of them, not what they say themselves.
    /// 9.2 compared the run's operator with the default operator of its URL, so every value
    /// of the file became a `--set` pair that then overrode the file.
    #[test]
    fn replaying_names_what_the_run_changed_not_what_its_operator_file_says() {
        let d = tempfile::tempdir().unwrap();
        let (app, op_file) = app_with_operators(d.path());
        for (operator, own) in [
            ("prod", ["options.region", "layers.retry.max_times"]),
            (op_file.as_str(), ["options.endpoint", "layers.timeout.op"]),
        ] {
            let req = RunRequest {
                operators: vec![operator.to_owned()],
                set: vec!["read.gap=1MiB".into(), "http.version=h1".into()],
                ..Default::default()
            };
            let spec = app.resolve(&req).unwrap().remove(0).spec();
            let again = app.replaying(&spec);
            assert_eq!(again.operators, [operator]);
            let mut set = again.set.clone();
            set.sort();
            assert_eq!(set, ["http.version=h1", "read.gap=1MiB"], "{operator}");
            let command = again.command();
            assert!(own.iter().all(|k| !command.contains(k)), "{command}");
            assert!(
                !command.contains("hunter2") && !command.contains("redacted"),
                "{command}"
            );
            // The file's values still reach the run, and a value edited in the file since
            // is the one the next run has.
            assert_eq!(app.resolve(&again).unwrap().remove(0).spec(), spec);
        }
        // An operator that no longer resolves is kept as it is: its values against its URL.
        let req = RunRequest {
            operators: vec![op_file.clone()],
            ..Default::default()
        };
        let spec = app.resolve(&req).unwrap().remove(0).spec();
        std::fs::remove_file(&op_file).unwrap();
        let again = app.replaying(&spec);
        assert_eq!(again.operators, [op_file]);
        assert!(
            again.set.iter().any(|s| s.starts_with("options.endpoint=")),
            "{again:?}"
        );
    }

    /// K1-3. The New Run form over a run being repeated. 9.2 dropped everything of the run
    /// once the preset changed: an `s3://` run on a local endpoint was sent, without its
    /// `options.endpoint`, to the real service.
    #[test]
    fn a_form_keeps_the_operators_values_while_it_names_the_operator() {
        let repeated = RunRequest {
            operators: vec!["s3://bench/x".into()],
            workload: WorkloadInput {
                preset: Some("range-reads".into()),
                concurrency: Some(3),
                duration: Some("5s".into()),
                rate: Some(150.0),
                objects: Some(6),
                seed: Some(7),
                ..Default::default()
            },
            set: [
                "warmup=0s",
                "partitions=2",
                "options.endpoint=http://127.0.0.1:9000",
                "layers.retry.max_times=5",
                "endpoint=http://127.0.0.1:9000",
            ]
            .map(str::to_owned)
            .to_vec(),
            label: Some("nightly".into()),
            ..Default::default()
        };
        let form = RunForm::of(&repeated);
        assert_eq!(form.operators, ["s3://bench/x"]);
        assert_eq!(form.preset.as_deref(), Some("range-reads"));
        // Unchanged, the form sends the run again; a field changes only itself.
        let same = form.request(Some(&repeated));
        assert_eq!(same.args(), repeated.args());
        assert_eq!(same.label.as_deref(), Some("nightly"));
        let longer = RunForm {
            duration: Some("9s".into()),
            concurrency: None,
            ..form.clone()
        };
        let r = longer.request(Some(&repeated));
        assert_eq!(r.workload.duration.as_deref(), Some("9s"));
        assert_eq!(
            (r.workload.concurrency, r.workload.rate),
            (None, Some(150.0))
        );
        assert_eq!(r.set, repeated.set);
        // Another preset is another workload on the same operator.
        let other_preset = RunForm {
            preset: Some("small-files".into()),
            ..form.clone()
        };
        let r = other_preset.request(Some(&repeated));
        assert_eq!(
            r.set,
            [
                "options.endpoint=http://127.0.0.1:9000",
                "layers.retry.max_times=5",
                "endpoint=http://127.0.0.1:9000"
            ]
        );
        assert_eq!(
            (r.workload.rate, r.workload.objects, r.workload.seed),
            (None, None, None)
        );
        assert_eq!(r.workload.duration.as_deref(), Some("5s"));
        let command = other_preset.repeat_command(&repeated).unwrap();
        assert_eq!(
            command,
            "oprobe run s3://bench/x --preset small-files -c 3 -d 5s \
             --set options.endpoint=http://127.0.0.1:9000 --set layers.retry.max_times=5 \
             --set endpoint=http://127.0.0.1:9000"
        );
        // An existing prefix is read-only, and the command says what the request does.
        let existing = RunForm {
            existing: Some("tables/".into()),
            ..form.clone()
        };
        let r = existing.request(Some(&repeated));
        assert!(r.safety.read_only);
        assert!(r.command().contains("--existing tables/") && !r.command().contains("--read-only"));
        // Other operators are another run: nothing is carried over.
        let other = RunForm {
            operators: vec!["s3://bench/y".into()],
            ..form.clone()
        };
        let r = other.request(Some(&repeated));
        assert!(r.set.is_empty() && r.label.is_none() && r.workload.rate.is_none());
        assert_eq!(other.repeat_command(&repeated), None);
        assert_eq!(r.args(), form_only(&other).args());
        fn form_only(f: &RunForm) -> RunRequest {
            f.request(None)
        }
    }

    /// A request's command names everything it sends: the workload settings without a flag
    /// and the safety rails too.
    #[test]
    fn a_requests_command_names_what_it_sends() {
        let req = RunRequest {
            operators: vec!["s3://a/b".into(), "my op.toml".into()],
            workload: WorkloadInput {
                preset: Some("list-stat".into()),
                warmup: Some("0s".into()),
                max_ops: Some(10),
                access: Some("zipf:1.1".into()),
                persist_dataset: true,
                no_verify: true,
                keep_data: true,
                arrivals: Some("poisson".into()),
                shape: Some("step:1,2/1s".into()),
                ..Default::default()
            },
            set: vec!["read.gap=1MiB".into()],
            safety: Safety {
                read_only: true,
                max_requests: Some(100),
                max_bytes: Some("1GiB".into()),
                max_duration: Some("5m".into()),
                dry_run: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            req.command(),
            "oprobe run s3://a/b 'my op.toml' --preset list-stat --set warmup=0s \
             --set max_ops=10 --set access=zipf:1.1 --set persist=on --set verify=off \
             --set cleanup=off --set arrivals=poisson --set shape=step:1,2/1s \
             --set read.gap=1MiB --read-only --max-requests 100 --max-bytes 1GiB \
             --max-duration 5m --dry-run"
        );
        // The command is the request: its workload resolves as the request's does.
        let d = tempfile::tempdir().unwrap();
        let (app, _) = app_with_operators(d.path());
        let one = RunRequest {
            operators: vec!["s3://a/b".into()],
            safety: Safety::default(),
            ..req.clone()
        };
        let args = one.args();
        let sets: Vec<String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "--set")
            .map(|(_, v)| v.clone())
            .collect();
        let typed = RunRequest {
            operators: vec!["s3://a/b".into()],
            workload: WorkloadInput {
                preset: Some("list-stat".into()),
                ..Default::default()
            },
            set: sets,
            ..Default::default()
        };
        assert_eq!(
            app.resolve(&typed).unwrap().remove(0).spec(),
            app.resolve(&one).unwrap().remove(0).spec()
        );
    }
}
