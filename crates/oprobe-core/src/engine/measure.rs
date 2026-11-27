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
}

/// A live guard enforcing `budgets` (none when empty). Share it between measurements for one
/// aggregate budget.
pub fn guard_for(budgets: &Budgets) -> Option<Arc<Guard>> {
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
    /// Where pending-cleanup markers live (`<data>/pending`, [`crate::store::pending`]): the
    /// run marks its prefix before its first write and sweeps what killed runs against the same
    /// target left. `None`: no markers.
    pub pending_dir: Option<PathBuf>,
    /// Warmed [`opendal::layers::TailCutLayer`] reused across candidate trials of one prove.
    pub tail_cut: Option<opendal::layers::TailCutLayer>,
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
            pending_dir: None,
            tail_cut: None,
        }
    }
}

/// Delete what runs against this target that were killed hard left behind (their markers are
/// unlocked), within the guard: only `oprobe-run-*` prefixes, never under `--read-only`. A
/// prefix verified gone loses its marker; the others are tried again by the next run.
///
/// A stale marker of this run's own prefix (an `--idempotency-key` retry of a killed run) is
/// only removed: this run's test data is already there, and its own cleanup deletes the prefix.
async fn sweep_pending(
    dir: &std::path::Path,
    target: &str,
    rails: &Rails,
    admin: &opendal::Operator,
    progress: &ProgressFn,
) -> Vec<String> {
    use crate::store::pending;
    let mut notes = Vec::new();
    if rails.read_only {
        return notes;
    }
    for s in pending::stale(dir, target, &rails.run_id.to_string()) {
        if s.marker.run_prefix == rails.run_prefix {
            s.finish();
            continue;
        }
        if !s.marker.sweepable() {
            continue;
        }
        let prefix = s.marker.run_prefix.clone();
        progress(crate::engine::Progress::Message(format!(
            "deleting the test data a stopped run left under {prefix}"
        )));
        let c = super::cleanup::delete_prefix(admin, &prefix).await;
        if c.verified {
            if c.deleted > 0 {
                notes.push(format!(
                    "deleted {} test objects a stopped run left under {prefix}",
                    c.deleted
                ));
            }
            s.finish();
        }
    }
    notes
}

/// The spec of measuring `profile` at `target_name` (or the profile's own URL).
pub fn spec_of(profile: &Profile, target_name: Option<&str>) -> RunSpec {
    let operator_ref = target_name
        .map(str::to_owned)
        .unwrap_or_else(|| profile.operator.url.clone());
    RunSpec::new(profile, operator_ref)
}

/// The error of a run whose file cannot be written.
const CANNOT_SAVE: &str = "cannot save the run (set OPROBE_DATA_DIR or pass --no-save)";

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
    open_with_tail_cut(
        profile,
        rails,
        guard,
        trace_max_requests,
        cancel,
        progress,
        None,
    )
    .await
}

async fn open_with_tail_cut(
    profile: Profile,
    rails: &Rails,
    guard: Option<Arc<Guard>>,
    trace_max_requests: usize,
    cancel: Cancel,
    progress: ProgressFn,
    tail_cut: Option<opendal::layers::TailCutLayer>,
) -> Result<Session> {
    if let Some(b) = guard.as_ref().and_then(|g| g.demand()) {
        return Err(b.to_error());
    }
    let opened = Session::open_with(
        profile,
        cancel,
        progress,
        SessionOptions {
            run_prefix: Some(rails.run_prefix.clone()),
            guard: guard.clone(),
            trace_max_requests,
            preflight: true,
            tail_cut,
        },
    )
    .await;
    // The preflight and the dataset upload count too: a budget that stopped them is the
    // error, not the request it refused.
    match (opened, guard.and_then(|g| g.breach())) {
        (Err(_), Some(b)) => Err(b.to_error()),
        (opened, _) => opened,
    }
}

/// Run one measurement end to end. A budget breach is not an error here: the partial run is
/// built, saved and returned with [`Measured::breach`] set (callers turn it into exit 4).
/// The error of a panic caught in the measurement window.
fn panicked(payload: &(dyn std::any::Any + Send)) -> anyhow::Error {
    let what = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("unknown cause");
    crate::errors::OprobeError::new(
        crate::errors::ErrorCode::Internal,
        format!("the run stopped on an internal error: {what}"),
    )
    .with_hint("the test data was removed; please report this with the command you ran")
    .into()
}

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
        pending_dir,
        tail_cut,
    } = req;
    let spec = spec.unwrap_or_else(|| spec_of(&profile, target_name.as_deref()));
    // The pending-cleanup marker, before anything is written (the preflight writes).
    let marker = match (&pending_dir, rails.read_only) {
        (Some(_), false) => profile
            .operator
            .target()
            .ok()
            .and_then(|t| crate::store::pending::Marker::of(&t, &rails.run_prefix)),
        _ => None,
    };
    let pending = match (&pending_dir, &marker) {
        (Some(dir), Some(m)) => {
            match crate::store::pending::Pending::create(dir, &rails.run_id.to_string(), m) {
                Ok(p) => Some(p),
                Err(e) => {
                    progress(crate::engine::Progress::Message(format!(
                        "no pending-cleanup marker ({e:#}): if this run is killed, its test data \
                         stays under {}",
                        rails.run_prefix
                    )));
                    None
                }
            }
        }
        _ => None,
    };
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
    let session = match open_with_tail_cut(
        profile,
        &rails,
        guard,
        trace_max_requests,
        cancel.clone(),
        progress.clone(),
        tail_cut,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            stage(Stage::Measured);
            return Err(e);
        }
    };
    let swept = match (&pending_dir, &marker) {
        (Some(dir), Some(m)) => {
            sweep_pending(dir, &m.target, &rails, &session.cleaner, &progress).await
        }
        _ => Vec::new(),
    };
    let knobs = crate::config::Knobs::of(&session.profile);
    // The run file is written while the run is in progress (design v5 §2).
    if let Some(s) = &store {
        let header = crate::store::runfile::RunHeader::new(rails.run_id.to_string(), spec.clone());
        let created = crate::store::runfile::RunFileStream::create(
            &s.run_path(&rails.run_id.to_string()),
            &header,
            trace_max_requests,
        );
        let st = match created {
            Ok(st) => st,
            // The session is open: its dataset is on the target. The run cannot be saved, so
            // it is not measured, but what it wrote is removed all the same (9.2 returned
            // here and left the dataset).
            Err(e) => {
                stage(Stage::Measured);
                stage(Stage::CleaningUp);
                let cleanup = session.close().await;
                if let Some(p) = pending
                    && (cleanup.verified || !cleanup.enabled)
                {
                    p.finish();
                }
                let e = e.context(CANNOT_SAVE);
                return Err(match cleanup.error {
                    Some(left) => e.context(format!(
                        "the test data under {} could not be removed: {left}",
                        cleanup.prefix
                    )),
                    None => e,
                });
            }
        };
        session.stream_rows(st.clone());
        stream = Some(st);
    }
    // A panic inside the window is an error like any other: the dataset it uploaded is
    // removed below (9.3.3 unwound past the cleanup and left it on the target).
    let options = RunOptions::from_profile(&session.profile);
    let res = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(session.run(
        &knobs,
        &options,
        Some(progress.clone()),
    )))
    .await
    .unwrap_or_else(|panic| Err(panicked(panic.as_ref())));
    stage(Stage::Measured);
    let (ok, bad) = if res.is_ok() && session.profile.workload.verify {
        session.verify_writes(None).await
    } else {
        (0, 0)
    };
    stage(Stage::CleaningUp);
    let cleanup = session.close().await;
    // Nothing is left behind (or the user keeps it on purpose): the marker goes. Otherwise it
    // stays, and the next run against this target deletes the prefix.
    if let Some(p) = pending
        && (cleanup.verified || !cleanup.enabled)
    {
        p.finish();
    }
    let mut snap = res?;
    snap.integrity.verified += ok;
    snap.integrity.corrupted += bad;
    // A budget spent exactly by the work the run had to do stops nothing: the run is whole.
    let breach = session.guard().and_then(|g| g.breach());
    let spent_whole = breach.is_none() && session.guard().is_some_and(|g| g.tripped());
    if breach.is_some() {
        snap.interrupted = true;
    } else if spent_whole {
        snap.interrupted = false;
    }
    let cancelled = cancel.is_cancelled() && !spent_whole;
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
    if breach.is_some() {
        run.report.verdict.exit_code = exit::BUDGET;
        crate::model::build::mark_partial(&mut run, crate::model::PartialReason::Budget);
    } else if cancelled && run.report.run.partial.is_none() {
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
        Some(r) => Some(r.map_err(|e| e.context(CANNOT_SAVE))?),
        None => None,
    };
    Ok(Measured {
        run,
        path,
        breach,
        cancelled,
        warnings: session.warnings.iter().cloned().chain(swept).collect(),
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
        let guard = guard_for(&budgets);
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
        let g2 = guard_for(&tight).unwrap();
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

    /// E2: an open-loop window runs to its deadline. 9.3.3 closed it at the last arrival:
    /// at 2 ops/s for 1.5 s, a 1 s window.
    #[tokio::test]
    async fn an_open_loop_window_runs_to_its_deadline() {
        let mut p = quick("memory://");
        p.workload.rate = Some(2.0);
        p.workload.duration = crate::util::HumanDuration(std::time::Duration::from_millis(1500));
        let rails = Rails::new(&p, false, None, Budgets::default()).unwrap();
        let req = Request::new(p, rails, Cancel::new(), silent());
        let m = measure(req).await.unwrap();
        let w = m.run.report.run.window_s;
        assert!((1.45..1.6).contains(&w), "window {w} s");
        assert!(
            m.run.report.summary.ops_per_s < 2.1,
            "{}",
            m.run.report.summary.ops_per_s
        );
    }

    #[tokio::test]
    async fn a_budget_breach_returns_the_partial_run() {
        let mut p = quick("memory://");
        p.workload.verify = false;
        // The dataset's upload is charged too: the budget leaves the window 50 requests.
        let setup = {
            let mut one = p.clone();
            one.workload.concurrency = 1;
            one.workload.max_ops = Some(1);
            let budgets = Budgets {
                max_requests_count: Some(u64::MAX),
                ..Default::default()
            };
            let guard = guard_for(&budgets).unwrap();
            let rails = Rails::new(&one, false, None, budgets).unwrap();
            let mut req = Request::new(one, rails, Cancel::new(), silent());
            req.guard = Some(guard.clone());
            measure(req).await.unwrap();
            guard.usage().0 - 1
        };
        let budgets = Budgets {
            max_requests_count: Some(setup + 50),
            ..Default::default()
        };
        let rails = Rails::new(&p, false, None, budgets.clone()).unwrap();
        let mut req = Request::new(p.clone(), rails, Cancel::new(), silent());
        req.guard = guard_for(&budgets);
        let m = measure(req).await.unwrap();
        assert_eq!(m.breach.unwrap().budget, "requests");
        assert!(m.run.report.is_partial());
        assert_eq!(m.run.report.verdict.exit_code, exit::BUDGET);
    }

    /// A run that uses exactly its request budget is whole (no breach, not partial, not
    /// exit 4); one more request of work would be cut: the budget one short stops the run at
    /// the limit, without a request past it, and is a breach.
    #[tokio::test]
    async fn a_run_that_spends_exactly_its_budget_is_whole() {
        let mut p = quick("memory://");
        p.workload.concurrency = 1;
        p.workload.max_ops = Some(40);
        // The re-read of written objects after the window is charged too, and is skipped
        // (not a breach) when the budget is spent: leave it out, so the window is the edge.
        p.workload.verify = false;
        p.workload.duration = crate::util::HumanDuration(std::time::Duration::from_secs(30));
        let run = |max: u64| {
            let p = p.clone();
            async move {
                let budgets = Budgets {
                    max_requests_count: Some(max),
                    ..Default::default()
                };
                let guard = guard_for(&budgets).unwrap();
                let rails = Rails::new(&p, false, None, budgets).unwrap();
                let mut req = Request::new(p, rails, Cancel::new(), silent());
                req.guard = Some(guard.clone());
                let m = measure(req).await.unwrap();
                (m, guard.usage().0)
            }
        };
        let (m, used) = run(u64::MAX).await;
        assert!(m.breach.is_none());
        assert!(used > 0);
        // Exactly the budget: the run did all its work.
        let (m, at) = run(used).await;
        assert_eq!(at, used);
        assert!(m.breach.is_none(), "{:?}", m.breach);
        assert!(!m.cancelled);
        assert!(!m.run.report.is_partial());
        assert!(
            m.run.report.run.partial.is_none(),
            "{:?}",
            m.run.report.run.partial
        );
        assert_ne!(m.run.report.verdict.exit_code, exit::BUDGET);
        assert_eq!(m.run.report.summary.ops, 40);
        // One request short: stopped at the limit, never past it.
        let (m, at) = run(used - 1).await;
        assert_eq!(at, used - 1, "no request past the limit");
        assert_eq!(m.breach.as_ref().unwrap().budget, "requests");
        assert!(m.run.report.is_partial());
        assert_eq!(m.run.report.verdict.exit_code, exit::BUDGET);
    }

    /// K1-8: a run whose file cannot be created is not measured, but the dataset it had
    /// already uploaded is removed (9.2 returned the error and left it on the target).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_file_that_cannot_be_created_still_cleans_up() {
        let lab = crate::lab::LabServer::spawn("127.0.0.1:0", Default::default())
            .await
            .unwrap();
        let lab = Arc::new(lab);
        let mut p = lab.profile("bkt");
        p.workload = quick("memory://").workload;
        let dir = tempfile::tempdir().unwrap();
        // The store's directory cannot be made: a file is in its way.
        let blocked = dir.path().join("data");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let pending = dir.path().join("pending");
        let rails = Rails::new(&p, false, None, Budgets::default()).unwrap();
        let prefix = rails.run_prefix.clone();
        let own = rails.run_id.to_string();
        let uploaded = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (seen, target) = (uploaded.clone(), lab.clone());
        // The dataset is on the target when the run file is created: the upload reports it.
        let progress: ProgressFn = Arc::new(move |p| {
            if matches!(&p, crate::engine::Progress::Message(m) if m.starts_with("dataset ready")) {
                seen.store(
                    target.stored_object_count(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
        });
        let mut req = Request::new(p, rails, Cancel::new(), progress);
        req.store = Some(RunStore::new(blocked.join("runs")));
        req.pending_dir = Some(pending.clone());
        let e = measure(req).await.err().expect("the run cannot be saved");
        assert!(format!("{e:#}").contains("cannot save the run"), "{e:#}");
        assert!(
            uploaded.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "the dataset was uploaded before the run file failed"
        );
        assert_eq!(lab.stored_object_count(), 0, "left under {prefix}");
        assert_eq!(lab.open_uploads(), 0);
        // Nothing is left: no marker for the next run to sweep either.
        assert!(!pending.join(&own).exists());
    }

    /// A run killed hard leaves its marker; the next run against the same target deletes its
    /// prefix and the marker, and a run that cleans up leaves no marker (P3).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_next_run_sweeps_what_a_killed_run_left() {
        use crate::store::pending::{Marker, Pending};
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let pending = data.path().join("pending");
        let url = format!("fs://{}", root.path().display());
        let p = quick(&url);
        let target = p.operator.target().unwrap();
        // The killed run: objects under its prefix and an unlocked marker. A live run's
        // prefix (its marker locked) and a prefix outside `oprobe-run-*` stay.
        let killed = "oprobe-run-20200101-000000-dead/";
        std::fs::create_dir_all(root.path().join(killed).join("data")).unwrap();
        std::fs::write(root.path().join(killed).join("data/x"), b"left").unwrap();
        let m = Marker::of(&target, killed).unwrap();
        drop(Pending::create(&pending, "20200101-000000-dead", &m).unwrap());
        let live_prefix = "oprobe-run-20200101-000000-0001/";
        std::fs::create_dir_all(root.path().join(live_prefix)).unwrap();
        std::fs::write(root.path().join(live_prefix).join("y"), b"live").unwrap();
        let live = Pending::create(
            &pending,
            "20200101-000000-0001",
            &Marker::of(&target, live_prefix).unwrap(),
        )
        .unwrap();
        let mut odd = m.clone();
        odd.run_prefix = "data/".into();
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        std::fs::write(root.path().join("data/keep"), b"mine").unwrap();
        drop(Pending::create(&pending, "20200101-000000-0002", &odd).unwrap());

        let rails = Rails::new(&p, false, None, Budgets::default()).unwrap();
        let own = rails.run_id.to_string();
        let mut req = Request::new(p, rails, Cancel::new(), silent());
        req.pending_dir = Some(pending.clone());
        let m = measure(req).await.unwrap();
        assert!(m.run.report.cleanup.verified, "{:?}", m.run.report.cleanup);
        assert!(
            !root.path().join(killed).join("data/x").exists(),
            "the killed run's data is deleted"
        );
        assert!(!pending.join("20200101-000000-dead").exists());
        assert!(
            m.warnings.iter().any(|w| w.contains(killed)),
            "{:?}",
            m.warnings
        );
        // The run cleaned up after itself: no marker of its own.
        assert!(!pending.join(&own).exists());
        // A live run's data and anything outside a run prefix are never touched.
        assert!(root.path().join(live_prefix).join("y").exists());
        assert!(pending.join("20200101-000000-0001").exists());
        assert!(root.path().join("data/keep").exists());
        live.finish();
    }

    /// An `--idempotency-key` retry of a run that was killed hard shares its prefix: the
    /// killed run's marker is removed, but the sweep never deletes the retry's own dataset.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_retry_with_the_same_prefix_keeps_its_own_dataset() {
        use crate::store::pending::{Marker, Pending};
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let pending = data.path().join("pending");
        let url = format!("fs://{}", root.path().display());
        let p = quick(&url);
        let target = p.operator.target().unwrap();
        let rails = Rails::new(&p, false, Some("nightly"), Budgets::default()).unwrap();
        let prefix = rails.run_prefix.clone();
        // The killed attempt: same prefix, an unlocked marker.
        std::fs::create_dir_all(root.path().join(&prefix)).unwrap();
        std::fs::write(root.path().join(&prefix).join("old"), b"left").unwrap();
        let m = Marker::of(&target, &prefix).unwrap();
        drop(Pending::create(&pending, "20200101-000000-dead", &m).unwrap());

        let mut req = Request::new(p, rails, Cancel::new(), silent());
        req.pending_dir = Some(pending.clone());
        let m = measure(req).await.unwrap();
        let s = &m.run.report.summary;
        assert!(s.ops > 0);
        assert_eq!(s.failed_ops, 0, "the retry's dataset was deleted under it");
        assert!(
            !m.warnings.iter().any(|w| w.contains("a stopped run left")),
            "{:?}",
            m.warnings
        );
        assert!(!pending.join("20200101-000000-dead").exists());
        // The retry's own cleanup removed the prefix, the killed attempt's leftovers with it.
        assert!(m.run.report.cleanup.verified, "{:?}", m.run.report.cleanup);
        assert!(!root.path().join(&prefix).join("old").exists());
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
