//! The commands: clap arguments in, [`crate::app::App`] calls, typed results out. Everything
//! here is presentation (what goes to stdout and stderr, Ctrl-C); the work is in the service
//! layer.

use anyhow::Result;

use super::args::*;
use super::install_ctrl_c;
use crate::app::{Cancel, Ctx, RunOutcome, RunRef, RunRequest, input as i};
use crate::errors::exit;
use crate::report::style::dim;

/// A cancel token that the first Ctrl-C trips (the second exits).
fn ctrl_c() -> Cancel {
    let cancel = Cancel::new();
    install_ctrl_c(cancel.clone());
    cancel
}

fn safety(a: &SafetyArgs) -> i::Safety {
    i::Safety {
        dry_run: a.dry_run,
        read_only: a.read_only,
        max_requests: a.max_requests,
        max_bytes: a.max_bytes.clone(),
        max_duration: a.max_duration.clone(),
        idempotency_key: None,
    }
}

/// `oprobe run OPERATOR…` (and `oprobe demo`): the operators one after another on the same
/// workload.
pub async fn run(operators: Vec<String>, f: RunFlags, ctx: &Ctx) -> Result<u8> {
    let w = &f.workload;
    let req = RunRequest {
        operators,
        workload: i::WorkloadInput {
            preset: w.preset.clone(),
            concurrency: w.concurrency,
            seed: w.seed,
            duration: w.duration.clone(),
            warmup: w.warmup.clone(),
            rate: w.rate,
            existing: w.existing.clone(),
            objects: w.objects,
            object_size: w.object_size.clone(),
            ..Default::default()
        },
        set: f.set,
        safety: safety(&f.safety),
        ..Default::default()
    };
    match ctx.app().run(req, ctx.out.clone(), ctrl_c()).await? {
        RunOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        RunOutcome::Run(r) => {
            let code = ctx.out.finish(r.as_ref())?;
            // A spent budget: the runs it let finish are on stdout, the error on stderr (in
            // JSON, the document's `error`).
            if let Some(e) = &r.error
                && !ctx.out.format.is_machine()
            {
                ctx.out.error(e);
            }
            Ok(code)
        }
    }
}

/// `oprobe show`: a run, the history (`--list`) or removal (`--rm`).
pub fn show(a: ShowArgs, ctx: &Ctx) -> Result<u8> {
    let app = ctx.app();
    if a.list {
        let list = if a.all { app.list_all()? } else { app.list()? };
        return ctx.out.finish(&list.truncate(a.limit));
    }
    if !a.rm.is_empty() {
        let refs: Vec<RunRef> = a.rm.into_iter().map(RunRef).collect();
        let ids = app.run_ids(&refs)?;
        app.rm(&refs)?;
        return ctx.out.finish(&crate::app::RemovedRuns {
            removed: ids,
            dry_run: false,
        });
    }
    let by = a.by.map(|b| match b {
        ByArg::Stage => crate::analysis::slice::By::Stage,
        ByArg::Api => crate::analysis::slice::By::Api,
        ByArg::Conn => crate::analysis::slice::By::Conn,
    });
    let secs = |v: &Option<String>| -> Result<Option<f64>> {
        Ok(match v {
            Some(s) => Some(crate::input::duration(s)?.as_secs_f64()),
            None => None,
        })
    };
    let (since, until) = (secs(&a.since)?, secs(&a.until)?);
    if a.runs.len() >= 2 {
        if by.is_some() || since.is_some() || until.is_some() {
            return Err(crate::errors::OprobeError::new(
                crate::errors::ErrorCode::UsageInvalidArgument,
                "--by and --since/--until apply to a single run, not a comparison",
            )
            .with_hint("inspect one run at a time, e.g. `oprobe show RUN --by stage`")
            .into());
        }
        let refs: Vec<RunRef> = a.runs.into_iter().map(RunRef).collect();
        let cmp = app.compare(&refs, a.detail)?;
        return ctx.out.finish(&cmp);
    }
    let r = RunRef(a.runs.into_iter().next().unwrap_or_else(|| "last".into()));
    // A prove id (`show --list` lists them): its stored result, as `prove` printed it.
    if let Some(p) = app.show_prove(&r)? {
        if by.is_some() || since.is_some() || until.is_some() || a.detail {
            return Err(crate::errors::OprobeError::new(
                crate::errors::ErrorCode::UsageInvalidArgument,
                "--by, --since/--until and --detail apply to a run, not a prove result",
            )
            .with_hint("inspect one of the prove's trial runs with `oprobe show --list --all`")
            .into());
        }
        return ctx.out.finish(&p);
    }
    let p = app
        .show(r)?
        .with_range(since, until)?
        .with_detail(a.detail)
        .with_by(by)?;
    ctx.out.finish(&p)
}

/// `oprobe prove`.
pub async fn prove(a: ProveArgs, ctx: &Ctx) -> Result<u8> {
    use crate::app::prove::{ProveObjective, ProveOutcome, ProveRequest};
    let req = ProveRequest {
        run: a.run,
        finding: a.finding,
        with: a.with,
        objective: match a.objective {
            ObjectiveArg::Goodput => ProveObjective::Goodput,
            ObjectiveArg::P99 => ProveObjective::P99,
        },
        max_throttle_ratio: a
            .max_throttle
            .as_deref()
            .map(|v| crate::input::share("--max-throttle", v))
            .transpose()?,
        // One wall-time budget: `--max-duration` (alias `--budget`) bounds the whole proof.
        budget: a.safety.wall_budget(),
        trial_duration: a.trial,
        pairs: a.pairs.unwrap_or(crate::config::prove_defaults::MAX_PAIRS),
        save: a.save,
        no_save: false,
        safety: i::Safety {
            dry_run: a.safety.dry_run,
            read_only: a.safety.read_only,
            max_requests: a.safety.max_requests,
            max_bytes: a.safety.max_bytes.clone(),
            max_duration: None,
            idempotency_key: None,
        },
    };
    match ctx.app().prove(req, ctx.out.clone(), ctrl_c()).await? {
        ProveOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        ProveOutcome::Proved(p) => ctx.out.finish(p.as_ref()),
    }
}

/// `oprobe lab`: serve until Ctrl-C.
pub async fn lab(a: LabArgs, ctx: &Ctx) -> Result<u8> {
    let lab = ctx
        .app()
        .lab(i::LabRequest {
            bind: a.bind,
            scenario: a.scenario,
            allow_remote: a.allow_remote,
            max_store_size: a.max_store_size,
            ..Default::default()
        })
        .await?;
    ctx.out.finish(&crate::app::LabOutput::of(&lab))?;
    tokio::signal::ctrl_c().await?;
    ctx.out.info(format!(
        "{} served {} requests",
        dim("lab stopped:"),
        lab.requests_served()
    ));
    Ok(exit::OK)
}
