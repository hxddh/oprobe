//! The one measurement pipeline (design §10 safety rails included). Every surface that measures
//! goes through here: `oprobe run` / `demo` / `run --repeat` / `matrix` / `tune`, the windows
//! of `oprobe watch` (through [`open`]), MCP jobs and the app's runs and tunes.
//!
//! Order: [`Rails`] (run id, prefix, read-only check, budgets) → `--dry-run` plan (callers
//! return [`Rails::plan`] instead of measuring) → [`open`] (refuses a spent budget, arms the
//! guard on the cancel token, preflight before any write) → the workload window (or the tuner)
//! → write verification → cleanup → interrupted marking → Report v3 → save.
//!
//! Budgets are per [`Guard`]: pass the same guard to several measurements (`run --repeat`,
//! `matrix`) to enforce one aggregate budget across the whole command.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Result;

use super::{Cancel, ProgressFn, RunOptions, Session, SessionOptions};
use crate::config::{Knobs, Profile};
use crate::errors::exit;
use crate::guard::{BudgetBreach, Budgets, Guard, Plan};
use crate::model::{Run, RunId, RunSpec, TuneReport};
use crate::report::TuneSummary;
use crate::store::RunStore;
use crate::tune::{TuneEvent, TuneOptions};

/// What the safety rails decide before a session opens.
#[derive(Clone, Debug)]
pub struct Rails {
    pub run_id: RunId,
    /// `oprobe-run-<id>/`, or the idempotent prefix of `--idempotency-key`.
    pub run_prefix: String,
    pub budgets: Budgets,
    /// `--read-only`, or implied by `--existing`.
    pub read_only: bool,
}

impl Rails {
    /// Decide the rails of one measurement of `profile`; refuses (`safety.read_only`) a
    /// read-only run that would write, and any run that would write when a read-only MCP server
    /// started this command ([`crate::guard::check_server_read_only`]).
    pub fn new(
        profile: &Profile,
        read_only: bool,
        idempotency_key: Option<&str>,
        budgets: Budgets,
    ) -> Result<Self> {
        let run_id = RunId::new();
        let run_prefix = match idempotency_key {
            Some(k) => crate::model::ids::idempotent_run_prefix(k),
            None => run_id.run_prefix(),
        };
        let existing = profile
            .workload
            .effective_dataset()
            .existing_prefix
            .is_some();
        let read_only = read_only || existing;
        if read_only {
            crate::guard::check_read_only(profile)?;
        }
        crate::guard::check_server_read_only(profile)?;
        Ok(Self {
            run_id,
            run_prefix,
            budgets,
            read_only,
        })
    }

    /// The `--dry-run` plan of measuring `profile` under these rails.
    pub fn plan(&self, profile: &Profile) -> Plan {
        crate::guard::plan(profile, &self.run_prefix, self.read_only, &self.budgets)
    }

    /// A live guard for these budgets (none when no budget is set).
    pub fn guard(&self, profile: &Profile) -> Option<Arc<Guard>> {
        guard_for(&self.budgets, profile)
    }
}

/// A live guard enforcing `budgets` (none when empty). Share it between measurements for one
/// aggregate budget.
pub fn guard_for(budgets: &Budgets, profile: &Profile) -> Option<Arc<Guard>> {
    (!budgets.is_empty())
        .then(|| Guard::with_pricing(budgets.clone(), crate::guard::usd_per_request(profile)))
}

/// Measure a workload window, or run the tuner.
// One per measurement; the tuner options are not worth a box.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum Mode {
    Run,
    Tune {
        opts: TuneOptions,
        on_event: Arc<dyn Fn(TuneEvent) + Send + Sync>,
    },
}

/// Steps the caller may want to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The workload (or the tuner) ended; verification and cleanup follow.
    Measured,
    /// Removing test data.
    CleaningUp,
}

/// Callback for [`Stage`]s.
pub type StageFn = Arc<dyn Fn(Stage) + Send + Sync>;
/// Adjust the run before it is saved (exit-code gates, display names, labels).
pub type FinalizeFn = Box<dyn FnOnce(&mut Run) + Send>;

/// One measurement.
pub struct Request {
    pub profile: Profile,
    /// Configured target name, when one was used.
    pub target_name: Option<String>,
    pub rails: Rails,
    /// Budgets enforced live (see [`guard_for`]; shared guards aggregate).
    pub guard: Option<Arc<Guard>>,
    pub trace_max_requests: usize,
    pub cancel: Cancel,
    pub progress: ProgressFn,
    /// Where the run is saved (none: not saved).
    pub store: Option<RunStore>,
    pub mode: Mode,
    pub on_stage: Option<StageFn>,
    pub finalize: Option<FinalizeFn>,
    /// The run's spec. `None`: `RunSpec::new(&profile, target)` with the target name, else the
    /// profile's URL (the pipeline fills it before the session normalises the profile).
    pub spec: Option<RunSpec>,
}

impl Request {
    /// A plain measurement of `profile` with no budgets, saved to `store`.
    pub fn new(profile: Profile, rails: Rails, cancel: Cancel, progress: ProgressFn) -> Self {
        Self {
            profile,
            target_name: None,
            rails,
            guard: None,
            trace_max_requests: crate::trace::DEFAULT_MAX_REQUESTS,
            cancel,
            progress,
            store: None,
            mode: Mode::Run,
            on_stage: None,
            finalize: None,
            spec: None,
        }
    }
}

/// The spec of measuring `profile` at `target_name` (or the profile's own URL).
pub fn spec_of(profile: &Profile, target_name: Option<&str>) -> RunSpec {
    let target_ref = target_name
        .map(str::to_owned)
        .or_else(|| profile.target.url.clone())
        .unwrap_or_else(|| profile.target.display());
    RunSpec::new(profile, target_ref)
}

/// What the tuner adds.
pub struct Tuned {
    pub winner: Knobs,
    pub summary: TuneSummary,
    /// The profile the session ran (normalized target), for code generation.
    pub profile: Profile,
    pub capability: opendal::Capability,
}

/// A finished measurement.
pub struct Measured {
    pub run: Run,
    /// The saved run file (none without a store).
    pub path: Option<PathBuf>,
    /// The budget that stopped the measurement (the partial run was still built and saved).
    pub breach: Option<BudgetBreach>,
    /// The cancel token fired (Ctrl-C, Stop, `cancel_run`, a budget).
    pub cancelled: bool,
    pub warnings: Vec<String>,
    pub tuned: Option<Tuned>,
}

/// Open a session under the rails: a spent (shared) budget refuses at once, the guard is armed
/// on `cancel`, and the preflight runs before anything is written.
pub async fn open(
    profile: Profile,
    rails: &Rails,
    guard: Option<Arc<Guard>>,
    trace_max_requests: usize,
    cancel: Cancel,
    progress: ProgressFn,
) -> Result<Session> {
    if let Some(b) = guard.as_ref().and_then(|g| {
        g.tick();
        g.breach()
    }) {
        return Err(b.to_error());
    }
    Session::open_with(
        profile,
        cancel,
        progress,
        SessionOptions {
            run_prefix: Some(rails.run_prefix.clone()),
            guard,
            trace_max_requests,
            preflight: true,
        },
    )
    .await
}

/// Run one measurement end to end. A budget breach is not an error here: the partial run is
/// built, saved and returned with [`Measured::breach`] set (callers turn it into exit 4).
pub async fn measure(req: Request) -> Result<Measured> {
    let Request {
        profile,
        target_name,
        rails,
        guard,
        trace_max_requests,
        cancel,
        progress,
        store,
        mode,
        on_stage,
        finalize,
        spec,
    } = req;
    let mut spec = spec.unwrap_or_else(|| spec_of(&profile, target_name.as_deref()));
    let stage = |s: Stage| {
        if let Some(f) = &on_stage {
            f(s)
        }
    };
    let started_at = SystemTime::now();
    let mut stream: Option<Arc<crate::store::runfile::RunFileStream>> = None;
    // Unsaved runs keep only the in-memory sample a streamed run keeps: nothing would ever
    // read more rows (bounded memory, design v5 §2).
    let trace_max_requests = if store.is_none() {
        trace_max_requests.min(crate::store::runfile::STREAM_MEMORY_ROWS)
    } else {
        trace_max_requests
    };
    let session = match open(
        profile,
        &rails,
        guard,
        trace_max_requests,
        cancel.clone(),
        progress.clone(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            stage(Stage::Measured);
            return Err(e);
        }
    };
    let (mut run, tuned) = match mode {
        Mode::Run => {
            let knobs = session.profile.knobs.clone();
            // The run file is written while the run is in progress (design v5 §2).
            if let Some(s) = &store {
                let header = crate::store::runfile::RunHeader::new(
                    rails.run_id.to_string(),
                    spec.clone(),
                    crate::model::ClientInfo::opendal(),
                );
                let st = crate::store::runfile::RunFileStream::create(
                    &s.run_path(&rails.run_id.to_string()),
                    &header,
                    trace_max_requests,
                )?;
                session.stream_rows(st.clone());
                stream = Some(st);
            }
            let res = session
                .run(
                    &knobs,
                    &RunOptions::from_profile(&session.profile),
                    Some(progress.clone()),
                )
                .await;
            stage(Stage::Measured);
            let (ok, bad) = if res.is_ok() && session.profile.workload.verify {
                session.verify_writes(None).await
            } else {
                (0, 0)
            };
            stage(Stage::CleaningUp);
            let cleanup = session.close().await;
            let mut snap = res?;
            snap.integrity.verified += ok;
            snap.integrity.corrupted += bad;
            if session.guard().is_some_and(|g| g.tripped()) {
                snap.interrupted = true;
            }
            // The ceiling remembered for this target and workload (design v4 §3).
            let history = store.as_ref().and_then(|s| {
                let dir = s.dir().parent()?;
                crate::analysis::capacity::load(
                    &dir.join("capacity.toml"),
                    &spec.target_id()?,
                    &crate::analysis::capacity::history_key(&spec),
                )
            });
            let run = crate::model::build::build_run_with(
                crate::model::ReportInput {
                    run_id: &rails.run_id,
                    target_name,
                    profile: &session.profile,
                    knobs: &knobs,
                    snapshot: snap,
                    trace: session.take_trace().unwrap_or_default(),
                    cleanup,
                    warnings: session.warnings.clone(),
                    started_at,
                    ended_at: SystemTime::now(),
                    spec: Some(spec),
                },
                history,
            );
            (run, None)
        }
        Mode::Tune { opts, on_event } => {
            let outcome = crate::tune::tune(&session, &opts, on_event).await;
            stage(Stage::Measured);
            stage(Stage::CleaningUp);
            let cleanup = session.close().await;
            let outcome = outcome?;
            let mut snap = outcome.winner_snapshot.clone();
            if session.guard().is_some_and(|g| g.tripped()) {
                snap.interrupted = true;
            }
            let recommendation = crate::tune::recommend::Recommendation::from_tune(
                &spec,
                &session.profile,
                &outcome.winner,
                &outcome.summary,
                &session.capability,
            )
            .ok();
            // The tuner's run replays as its winner.
            spec.profile.knobs = outcome.winner.clone();
            let mut run = crate::model::build_run(crate::model::ReportInput {
                run_id: &rails.run_id,
                target_name,
                profile: &session.profile,
                knobs: &outcome.winner,
                snapshot: snap,
                // The winner's window is a trial: its rows were not kept (slow ops still are).
                trace: Default::default(),
                cleanup,
                warnings: session.warnings.clone(),
                started_at,
                ended_at: SystemTime::now(),
                spec: Some(spec),
            });
            // The winner's window kept no rows: the report's capacity is the tuner's pooled
            // estimate (`source: tune`).
            if let Some(c) = &outcome.summary.capacity {
                run.report.capacity = c.clone();
            }
            let mut tr = TuneReport::from(&outcome.summary);
            tr.recommendation = recommendation;
            run.report.tune = Some(tr);
            let tuned = Tuned {
                winner: outcome.winner,
                summary: outcome.summary,
                profile: session.profile.clone(),
                capability: session.capability,
            };
            (run, Some(tuned))
        }
    };
    // The load generator (design v5 §7): its name, version and stages, and no finding that
    // needs a stage it cannot observe.
    if let Some(c) = session.client() {
        run.report.client = c.info();
        crate::clients::suppress_unsupported(&mut run.report);
    }
    let breach = session.guard().and_then(|g| g.breach());
    if breach.is_some() {
        run.report.verdict.exit_code = exit::BUDGET;
        crate::model::build::mark_partial(&mut run, crate::model::PartialReason::Budget);
    } else if cancel.is_cancelled() && run.report.run.partial.is_none() {
        crate::model::build::mark_partial(&mut run, crate::model::PartialReason::Interrupted);
    }
    if let Some(f) = finalize {
        f(&mut run);
    }
    let saved = match (&store, &stream) {
        (Some(s), Some(st)) => {
            st.describe(&mut run.report);
            Some(s.save_streamed(&run, st))
        }
        (Some(s), None) => Some(s.save(&run)),
        (None, _) => None,
    };
    let path = match saved {
        Some(r) => Some(r.map_err(|e| {
            e.context("cannot save the run (set OPROBE_DATA_DIR or pass --no-save)")
        })?),
        None => None,
    };
    // Remember the ceiling for later runs of the same target and workload (best effort).
    if let (Some(s), Some(_)) = (&store, run.report.capacity.inflight_ceiling)
        && !run.report.run.interrupted
        && let Some(id) = run.report.spec.target_id()
        && let Some(dir) = s.dir().parent()
    {
        let _ = crate::analysis::capacity::save(
            &dir.join("capacity.toml"),
            &id,
            &crate::analysis::capacity::history_key(&run.report.spec),
            &run.report.capacity,
            Some(run.id()),
        );
    }
    Ok(Measured {
        run,
        path,
        breach,
        cancelled: cancel.is_cancelled(),
        warnings: session.warnings.clone(),
        tuned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TargetSpec;

    fn quick(url: &str) -> Profile {
        let mut p = Profile::from_target(TargetSpec::from_url(url).unwrap());
        p.workload.duration = crate::util::HumanDuration(std::time::Duration::from_millis(200));
        p.workload.warmup = crate::util::HumanDuration(std::time::Duration::ZERO);
        p.knobs.concurrency = 2;
        p
    }

    fn silent() -> ProgressFn {
        Arc::new(|_| {})
    }

    #[tokio::test]
    async fn measures_saves_and_shares_one_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path().join("runs"));
        let p = quick("memory://");
        let budgets = Budgets {
            max_requests_count: Some(1_000_000_000),
            ..Default::default()
        };
        let guard = guard_for(&budgets, &p);
        let rails = Rails::new(&p, false, None, budgets.clone()).unwrap();
        let mut req = Request::new(p.clone(), rails, Cancel::new(), silent());
        req.store = Some(store.clone());
        req.guard = guard.clone();
        let m = measure(req).await.unwrap();
        assert!(m.breach.is_none());
        assert!(m.path.as_ref().is_some_and(|p| p.exists()));
        assert!(m.run.report.summary.ops > 0);
        // The same guard carries the first measurement's usage into the next one.
        let (used, _) = guard.as_ref().unwrap().usage();
        assert!(used > 0);
        let tight = Budgets {
            max_requests_count: Some(used),
            ..Default::default()
        };
        let g2 = guard_for(&tight, &p).unwrap();
        g2.add(used + 1, 0);
        let rails = Rails::new(&p, false, None, tight).unwrap();
        let mut req = Request::new(p, rails, Cancel::new(), silent());
        req.guard = Some(g2);
        let e = measure(req).await.err().expect("spent budget refuses");
        assert_eq!(
            crate::errors::classify(&e, "memory").code,
            crate::errors::ErrorCode::SafetyBudgetExceeded
        );
    }

    #[tokio::test]
    async fn a_budget_breach_returns_the_partial_run() {
        let p = quick("memory://");
        let budgets = Budgets {
            max_requests_count: Some(50),
            ..Default::default()
        };
        let rails = Rails::new(&p, false, None, budgets.clone()).unwrap();
        let mut req = Request::new(p.clone(), rails, Cancel::new(), silent());
        req.guard = guard_for(&budgets, &p);
        let m = measure(req).await.unwrap();
        assert_eq!(m.breach.unwrap().budget, "requests");
        assert!(m.run.report.run.interrupted);
        assert_eq!(m.run.report.verdict.exit_code, exit::BUDGET);
    }

    #[test]
    fn read_only_rails_refuse_writes() {
        let p = quick("memory://");
        assert!(Rails::new(&p, true, None, Budgets::default()).is_err());
        let r = Rails::new(&p, false, Some("ci-42"), Budgets::default()).unwrap();
        assert_eq!(
            r.run_prefix,
            crate::model::ids::idempotent_run_prefix("ci-42")
        );
        assert!(r.plan(&p).estimated_requests_count > 0);
    }
}
