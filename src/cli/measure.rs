//! One complete measurement (session → run → verify → cleanup → Report v3 → store), shared by
//! `run --repeat`, `run --baseline` and `matrix`. The same steps as `oprobe run`, with the
//! safety rails of `run.rs`.

use std::time::SystemTime;

use anyhow::Result;

use super::Ctx;
use super::args::{OutputArgs, SafetyArgs};
use super::run::{apply_fail_on, budget_error, guard_for, progress_for, rails, save_run};
use crate::config::Profile;
use crate::engine::{Cancel, RunOptions, Session, SessionOptions};
use crate::errors::exit;
use crate::model::Run;

/// A finished measurement.
pub(super) struct Measured {
    pub run: Run,
    /// The saved run file (absent with `--no-save`).
    pub path: Option<String>,
}

/// Measure `profile` once and save the run. A budget breach saves the partial run and returns
/// its error (exit 4).
pub(super) async fn measure(
    ctx: &Ctx,
    profile: Profile,
    target_name: Option<String>,
    safety: &SafetyArgs,
    output: &OutputArgs,
    cancel: &Cancel,
) -> Result<Measured> {
    let rails = rails(&profile, safety)?;
    let guard = guard_for(ctx, &profile, &rails.budgets);
    let total_s = profile.workload.duration.get().as_secs_f64();
    let (progress, line) = progress_for(ctx, total_s);
    let started_at = SystemTime::now();
    let session = Session::open_with(
        profile,
        cancel.clone(),
        progress.clone(),
        SessionOptions {
            run_prefix: Some(rails.run_prefix.clone()),
            guard,
            trace_max_requests: super::trace_max_requests(ctx),
            preflight: true,
        },
    )
    .await;
    let session = match session {
        Ok(s) => s,
        Err(e) => {
            if let Some(l) = &line {
                l.finish();
            }
            return Err(e);
        }
    };
    for w in &session.warnings {
        ctx.out.warn(w.clone());
    }
    let knobs = session.profile.knobs.clone();
    let res = session
        .run(
            &knobs,
            &RunOptions::from_profile(&session.profile),
            Some(progress.clone()),
        )
        .await;
    if let Some(l) = &line {
        l.finish();
    }
    let (ok, bad) = if res.is_ok() && session.profile.workload.verify {
        session.verify_writes(None).await
    } else {
        (0, 0)
    };
    let cleanup = session.close().await;
    let mut snap = res?;
    snap.integrity.verified += ok;
    snap.integrity.corrupted += bad;
    let budget_hit = session.guard().is_some_and(|g| g.tripped());
    if budget_hit {
        snap.interrupted = true;
    }
    let mut run = crate::model::build_run(crate::model::ReportInput {
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
    });
    if budget_hit {
        run.report.verdict.exit_code = exit::BUDGET;
    }
    apply_fail_on(&mut run.report, output, ctx);
    let path = save_run(ctx, &run, output)?;
    if let Some(e) = budget_error(&session, &rails.run_id, ctx, path.is_some()) {
        return Err(e);
    }
    Ok(Measured { run, path })
}
