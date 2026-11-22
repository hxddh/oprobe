//! The commands: clap arguments in, [`crate::app::App`] calls, typed results out. Everything
//! here is presentation (what goes to stdout and stderr, Ctrl-C); the work is in the service
//! layer.

use anyhow::Result;

use super::args::*;
use super::install_ctrl_c;
use crate::app::{Cancel, Ctx, RunOutcome, RunRequest, input as i};
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

fn profile_request(operator: &str, f: &RunFlags) -> i::ProfileRequest {
    let w = &f.workload;
    i::ProfileRequest {
        operator: operator.to_owned(),
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
        set: f.set.clone(),
        ..Default::default()
    }
}

/// `oprobe run OPERATOR…` (and `oprobe demo`): the operators one after another on the same
/// workload.
pub async fn run(operators: Vec<String>, f: RunFlags, ctx: &Ctx) -> Result<u8> {
    let cancel = ctrl_c();
    let mut code = exit::OK;
    for op in &operators {
        if cancel.is_cancelled() {
            break;
        }
        let resolved = ctx.app().resolve(&profile_request(op, &f))?;
        let req = RunRequest::new(resolved).with_safety(safety(&f.safety));
        let c = match ctx.app().run(req, &ctx.out, cancel.clone()).await? {
            RunOutcome::Plan(p) => ctx.out.finish(p.as_ref())?,
            RunOutcome::Run(r) => ctx.out.finish(r.as_ref())?,
        };
        code = code.max(c);
    }
    Ok(code)
}

/// `oprobe show`: a run, the history (`--list`) or removal (`--rm`).
pub fn show(a: ShowArgs, ctx: &Ctx) -> Result<u8> {
    let app = ctx.app();
    if a.list {
        return ctx
            .out
            .finish(&app.runs_list(i::RunsListRequest { limit: a.limit })?);
    }
    if !a.rm.is_empty() {
        return ctx
            .out
            .finish(&app.runs_rm(i::RunsRmRequest { runs: a.rm })?);
    }
    ctx.out.finish(&app.runs_show(i::RunsShowRequest {
        run: a.run.unwrap_or_else(|| "last".into()),
        detail: a.detail,
    })?)
}

/// `oprobe lab`: serve until Ctrl-C.
pub async fn lab(a: LabArgs, ctx: &Ctx) -> Result<u8> {
    let (lab, out) = ctx
        .app()
        .lab(i::LabRequest {
            bind: a.bind,
            scenario: a.scenario,
            ..Default::default()
        })
        .await?;
    ctx.out.finish(&out)?;
    tokio::signal::ctrl_c().await?;
    ctx.out.info(format!(
        "{} served {} requests",
        dim("lab stopped:"),
        lab.requests_served()
    ));
    Ok(exit::OK)
}
