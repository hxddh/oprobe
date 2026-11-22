//! The CLI side of the measurement pipeline ([`crate::engine::measure`]): progress for this
//! output mode, `--fail-on`, `--no-save` and the report files, budget-breach and Ctrl-C
//! messages. Used by `run`, `demo`, `run --repeat` / `--baseline`, `matrix` and `tune`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::Result;

use super::Ctx;
use super::input::OutputOptions;
use super::run::{fail_on_gate, progress_for, save_outputs};
use crate::config::Profile;
use crate::engine::Cancel;
use crate::engine::measure::{self as pipeline, Measured, Mode, Rails, Request, Stage};
use crate::errors::exit;
use crate::guard::Guard;
use crate::report::style::dim;

/// What one CLI measurement needs besides the profile.
pub struct Opts<'a> {
    pub target_name: Option<String>,
    pub rails: Rails,
    /// The command's guard: shared by every measurement of the command (one aggregate budget).
    pub guard: Option<Arc<Guard>>,
    pub output: &'a OutputOptions,
    pub cancel: &'a Cancel,
    pub mode: Mode,
}

/// Measure `profile` once through the pipeline and save the run (unless `--no-save`). A budget
/// breach saves the partial run and returns its error (exit 4).
pub async fn measure(ctx: &Ctx, profile: Profile, o: Opts<'_>) -> Result<Measured> {
    let total_s = match &o.mode {
        Mode::Run => profile.workload.duration.get().as_secs_f64(),
        Mode::Tune { opts, .. } => {
            (opts.trial.duration + opts.trial.warmup).as_secs_f64() * opts.max_trials as f64
        }
    };
    let (progress, line) = match &o.mode {
        Mode::Run => progress_for(ctx, total_s),
        Mode::Tune { .. } => (super::run::tune_progress(ctx), None),
    };
    let announce_cleanup = matches!(o.mode, Mode::Run);
    let l2 = line.clone();
    let out = ctx.out.clone();
    let on_stage: pipeline::StageFn = Arc::new(move |s| match s {
        Stage::Measured => {
            if let Some(l) = &l2 {
                l.finish();
            }
        }
        Stage::CleaningUp if announce_cleanup => out.info(dim("cleaning up…")),
        Stage::CleaningUp => {}
    });
    let gate = Arc::new(AtomicU8::new(0));
    let g2 = gate.clone();
    let fail_on = o.output.fail_on;
    let mut req = Request::new(profile, o.rails, o.cancel.clone(), progress);
    req.target_name = o.target_name;
    req.guard = o.guard;
    req.trace_max_requests = super::profile::trace_max_requests(ctx);
    req.store = (!o.output.no_save).then(|| ctx.store());
    req.mode = o.mode;
    req.on_stage = Some(on_stage);
    req.finalize = Some(Box::new(move |run| {
        if let Some(level) = fail_on
            && fail_on_gate(&mut run.report, level)
        {
            g2.store(1, Ordering::Relaxed);
        }
    }));
    let res = pipeline::measure(req).await;
    if let Some(l) = &line {
        l.finish();
    }
    let m = res?;
    for w in &m.warnings {
        ctx.out.warn(w.clone());
    }
    if gate.load(Ordering::Relaxed) == 1 {
        ctx.out.warn(format!(
            "`--fail-on`: the diagnosis has {} findings (exit {})",
            m.run.report.worst_severity().label(),
            exit::GATE
        ));
    }
    save_outputs(&m.run.report, o.output)?;
    if let Some(b) = &m.breach {
        if m.path.is_some() {
            ctx.out.warn(format!(
                "the partial run was saved: oprobe runs show {}",
                m.run.id()
            ));
        }
        return Err(b.to_error());
    }
    if m.cancelled {
        ctx.out
            .warn("interrupted — partial results: they cover only the time before Ctrl-C");
    }
    Ok(m)
}
