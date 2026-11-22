//! The one measurement pipeline (safety rails included). Every surface that measures goes
//! through here: `oprobe run` (each operator), `demo`, every trial of `oprobe prove` and the
//! app's runs.
//!
//! Order: [`Rails`] (run id, prefix, read-only check, budgets) → `--dry-run` plan (callers
//! return [`Rails::plan`] instead of measuring) → [`open`] (refuses a spent budget, arms the
//! guard on the cancel token, preflight before any write) → the workload window → write
//! verification → cleanup → interrupted marking → the report → save.
//!
//! Budgets are per [`Guard`]: pass the same guard to several measurements (several operators,
//! prove's trials) to enforce one aggregate budget across the whole command.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Result;

use super::{Cancel, ProgressFn, RunOptions, Session, SessionOptions};
use crate::config::Profile;
use crate::errors::exit;
use crate::guard::{BudgetBreach, Budgets, Guard, Plan};
use crate::model::{Run, RunId, RunSpec};
use crate::store::RunStore;

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
    /// read-only run that would write.
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
pub fn guard_for(budgets: &Budgets, _profile: &Profile) -> Option<Arc<Guard>> {
    (!budgets.is_empty()).then(|| Guard::new(budgets.clone()))
}

/// Steps the caller may want to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The workload ended; verification and cleanup follow.
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
    /// The `oprobe prove` this measurement is a trial of (`report.run.parent_id`).
    pub parent_id: Option<String>,
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
            parent_id: None,
            on_stage: None,
            finalize: None,
            spec: None,
        }
    }
}

/// The spec of measuring `profile` at `target_name` (or the profile's own URL).
pub fn spec_of(profile: &Profile, target_name: Option<&str>) -> RunSpec {
    let operator_ref = target_name
        .map(str::to_owned)
        .unwrap_or_else(|| profile.operator.url.clone());
    RunSpec::new(profile, operator_ref)
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
        parent_id,
        on_stage,
        finalize,
        spec,
    } = req;
    let spec = spec.unwrap_or_else(|| spec_of(&profile, target_name.as_deref()));
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
    let knobs = crate::config::Knobs::of(&session.profile);
    // The run file is written while the run is in progress (design v5 §2).
    if let Some(s) = &store {
        let header = crate::store::runfile::RunHeader::new(rails.run_id.to_string(), spec.clone());
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
    let mut run = crate::model::build::build_run(crate::model::ReportInput {
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
    });
    run.report.run.parent_id = parent_id;
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
    Ok(Measured {
        run,
        path,
        breach,
        cancelled: cancel.is_cancelled(),
        warnings: session.warnings.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(url: &str) -> Profile {
        let mut p = Profile::from_url(url);
        p.workload.duration = crate::util::HumanDuration(std::time::Duration::from_millis(200));
        p.workload.warmup = crate::util::HumanDuration(std::time::Duration::ZERO);
        p.workload.concurrency = 2;
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
