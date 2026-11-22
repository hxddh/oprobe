//! The commands: clap arguments in, [`crate::app::App`] calls, typed results out. Everything
//! here is presentation (what goes to stdout and stderr, Ctrl-C, interactive prompts); the
//! work is in the service layer.

use anyhow::Result;

use super::args::*;
use super::{Ctx, install_ctrl_c};
use crate::app::{self, Cancel, RunOutcome, RunRequest};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::output::OutputFormat;
use crate::report::style::dim;

/// A cancel token that the first Ctrl-C trips (the second exits).
fn ctrl_c() -> Cancel {
    let cancel = Cancel::new();
    install_ctrl_c(cancel.clone());
    cancel
}

/// `oprobe run`.
pub async fn run(mut a: RunArgs, ctx: &Ctx) -> Result<u8> {
    // `oprobe run` without a target in an interactive terminal: guided first run.
    if a.target.target.is_none()
        && a.target.profile.is_none()
        && a.from.is_none()
        && ctx.config()?.defaults.target.is_none()
        && app::onboard::should_offer(ctx)
    {
        match app::onboard::choose(ctx)? {
            app::onboard::Choice::Demo => {
                return demo(app::DemoRequest::default(), ctx).await;
            }
            app::onboard::Choice::Target(name) => a.target.target = Some(name),
            app::onboard::Choice::Saved => return Ok(exit::OK),
        }
    }
    let resolved = ctx.app().resolve(&super::convert::profile_request(&a))?;
    let req = RunRequest {
        repeat: a.repeat,
        baseline: a.baseline.clone(),
        client: match &a.load.client {
            Some(c) => app::ClientChoice::parse(c)?,
            None => app::ClientChoice::default(),
        },
        workers: match &a.load.workers {
            Some(w) => Some(app::WorkerSet {
                token_file: a.load.worker_token_file.clone(),
                ..app::WorkerSet::parse(w)?
            }),
            None => None,
        },
        ..RunRequest::new(resolved)
            .with_safety(a.safety.into())
            .with_output(a.output.into())
    };
    match ctx.app().run(req, &ctx.out, ctrl_c()).await? {
        RunOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        RunOutcome::Run(r) => ctx.out.finish(r.as_ref()),
        RunOutcome::Repeat(r) => ctx.out.finish(r.as_ref()),
    }
}

/// `oprobe demo`.
pub async fn demo(a: app::DemoRequest, ctx: &Ctx) -> Result<u8> {
    let out = app::demo::demo(ctx, a, ctrl_c()).await?;
    ctx.out.finish(&out)
}

/// `oprobe tune`: the trial table (report content) before the result in text mode.
pub async fn tune(a: TuneArgs, ctx: &Ctx) -> Result<u8> {
    match ctx.app().tune(a.into(), &ctx.out, ctrl_c()).await? {
        app::tune::TuneOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        app::tune::TuneOutcome::Tuned(out) => {
            if ctx.out.format == OutputFormat::Text {
                println!("\n{}", dim("TRIALS (★ recommended, ◆ Pareto front)"));
                print!("{}", app::tune::trial_table(&out.tune));
            }
            ctx.out.finish(out.as_ref())
        }
    }
}

/// `oprobe verify`.
pub async fn verify(a: VerifyArgs, ctx: &Ctx) -> Result<u8> {
    match ctx.app().verify(a.into(), &ctx.out, ctrl_c()).await? {
        app::verify::VerifyRun::Plan(p) => ctx.out.finish(p.as_ref()),
        app::verify::VerifyRun::Verified(v) => ctx.out.finish(v.as_ref()),
    }
}

/// `oprobe fix`.
pub async fn fix(a: FixArgs, ctx: &Ctx) -> Result<u8> {
    let out = ctx.app().fix(a.into(), &ctx.out, ctrl_c()).await?;
    ctx.out.finish(&out)
}

/// `oprobe compare`.
pub fn compare(a: CompareArgs, ctx: &Ctx) -> Result<u8> {
    let c = ctx.app().compare(a.into(), &ctx.out)?;
    ctx.out.finish(&c)
}

/// `oprobe baseline calibrate`.
pub async fn calibrate(a: CalibrateArgs, ctx: &Ctx) -> Result<u8> {
    match ctx
        .app()
        .baseline_calibrate(a.into(), &ctx.out, ctrl_c())
        .await?
    {
        app::compare::CalibrateOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        app::compare::CalibrateOutcome::Calibrated(c) => ctx.out.finish(c.as_ref()),
    }
}

/// `oprobe runs export`: a file, an OTLP push, or the export itself on stdout.
pub async fn export(a: ExportArgs, ctx: &Ctx) -> Result<u8> {
    use std::io::Write as _;
    match ctx.app().runs_export(a.into(), &ctx.out).await? {
        app::export::Exported::Written(o) => ctx.out.finish(o.as_ref()),
        app::export::Exported::Stdout(text) => {
            let mut out = std::io::stdout().lock();
            // A closed pipe (`| head`) is the reader's choice, not a failure.
            match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
                _ => Ok(exit::OK),
            }
        }
    }
}

/// `oprobe target add`: save, then a quick read-only check in text mode.
pub async fn target_add(a: TargetAddArgs, ctx: &Ctx) -> Result<u8> {
    let no_test = a.no_test;
    let name = a.name.clone();
    let saved = ctx.app().target_add(a.into(), &ctx.out).await?;
    let code = ctx.out.finish(&saved)?;
    if no_test {
        return Ok(code);
    }
    if ctx.out.format != OutputFormat::Text {
        ctx.out
            .info(format!("check it with `oprobe target test {name}`"));
        return Ok(code);
    }
    // A quick read-only doctor: reachability, TLS, clock, credentials and listing.
    ctx.out.info(dim(
        "quick check (read-only; `oprobe target test --write-probe` also writes a probe object):",
    ));
    let checked = ctx
        .app()
        .target_test(
            app::TargetTestRequest {
                name: name.clone(),
                read_only: true,
                write_probe: false,
            },
            &ctx.out,
        )
        .await
        .and_then(|d| ctx.out.finish(&d));
    let warn = || {
        ctx.out.warn(format!(
            "the target is saved, but the check failed: fix it, then `oprobe target test {name}`"
        ))
    };
    match checked {
        Ok(0) => Ok(code),
        Ok(c) => {
            warn();
            Ok(c)
        }
        Err(e) => {
            warn();
            Err(e)
        }
    }
}

/// `oprobe lab`: serve until Ctrl-C.
pub async fn lab(a: LabArgs, ctx: &Ctx) -> Result<u8> {
    let (lab, out) = app::misc::lab(ctx, a.into()).await?;
    ctx.out.finish(&out)?;
    tokio::signal::ctrl_c().await?;
    ctx.out.info(format!(
        "{} served {} requests",
        dim("lab stopped:"),
        lab.requests_served()
    ));
    Ok(exit::OK)
}

/// `oprobe watch`.
pub async fn watch(a: WatchArgs, ctx: &Ctx) -> Result<u8> {
    match app::watch::watch(ctx, a.into(), ctrl_c()).await? {
        app::watch::WatchOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        app::watch::WatchOutcome::Watched(w) => ctx.out.finish(w.as_ref()),
    }
}

/// `oprobe matrix`.
pub async fn matrix(a: MatrixArgs, ctx: &Ctx) -> Result<u8> {
    match app::matrix::matrix(ctx, a.into(), ctrl_c()).await? {
        app::matrix::MatrixOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        app::matrix::MatrixOutcome::Measured(m) => ctx.out.finish(m.as_ref()),
    }
}

/// `oprobe compat`.
pub async fn compat(a: CompatArgs, ctx: &Ctx) -> Result<u8> {
    match app::compat::run(ctx, a.into()).await? {
        app::compat::CompatOutcome::Plan(p) => ctx.out.finish(p.as_ref()),
        app::compat::CompatOutcome::Checked(c) => ctx.out.finish(c.as_ref()),
    }
}

/// `oprobe runs open`: start the app on a stored run (a window, else the terminal workbench).
pub fn runs_open(a: RunRefArg, ctx: &Ctx) -> Result<u8> {
    let path = ctx.store().resolve(&a.run)?;
    let path = path.canonicalize().unwrap_or(path);
    #[cfg(feature = "tui")]
    let window = super::gui_available();
    #[cfg(not(feature = "tui"))]
    let window: std::result::Result<(), &str> = Err("this oprobe was built without native windows");
    if let Err(why) = window {
        // No window here (no `gui` feature, no display): the terminal workbench shows the same
        // run when this is an interactive terminal.
        if cfg!(feature = "tui")
            && ctx.out.format == crate::output::OutputFormat::Text
            && crate::output::interactive_terminal()
        {
            ctx.out.info(format!(
                "cannot open a window ({why}); opening the run in the terminal workbench: oprobe top --open {}",
                path.display()
            ));
            let status = std::process::Command::new(std::env::current_exe()?)
                .arg("top")
                .arg("--open")
                .arg(&path)
                .status()
                .map_err(|e| anyhow::anyhow!("cannot start the terminal workbench: {e}"))?;
            return Ok(status.code().map_or(1, |c| c.clamp(0, 255) as u8));
        }
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("cannot open a window: {why}"),
        )
        .with_hint(format!(
            "read it here with `oprobe runs show {}`, or in the terminal workbench: `oprobe top --open {}`",
            a.run,
            path.display()
        ))
        .into());
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("gui")
        .arg("--open")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start the app: {e}"))?;
    ctx.out.finish(&crate::app::runs::OpenedRun {
        path: path.display().to_string(),
        launched: true,
    })
}
