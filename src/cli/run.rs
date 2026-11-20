//! `oprobe run` and `oprobe tune`: measure, build the Report v3, save the run.

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use super::args::*;
use super::{Ctx, apply_sla, install_ctrl_c, live, resolve_profile, trace_max_requests};
use crate::config::Profile;
use crate::engine::{Cancel, Progress, ProgressFn, RunOptions, Session, SessionOptions};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::events::EventKind;
use crate::guard::{Budgets, Guard, Plan};
use crate::model::{Report, Run, RunId, TuneReport};
use crate::output::{CommandOutput, OutputFormat};
use crate::report::style::{self, bold, dim, green, red, yellow};
use crate::report::{brief, markdown, write_file};
use crate::util::{ByteSize, parse_duration};

/// Result of `oprobe run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunOutput {
    pub run_id: String,
    /// The saved run file (absent with `--no-save`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub report: Report,
    /// The run with its trace rows (not serialized; the HTML and Markdown renderers use it).
    #[serde(skip)]
    #[schemars(skip)]
    pub full: Option<Box<Run>>,
}

impl CommandOutput for RunOutput {
    fn text(&self) -> String {
        brief::render(&self.report, &self.run_id)
    }
    fn markdown(&self) -> String {
        match &self.full {
            Some(run) => markdown::render(run),
            None => markdown::render_report(&self.report),
        }
    }
    fn html(&self) -> Option<String> {
        Some(match &self.full {
            Some(run) => crate::report::html::render(run),
            None => crate::report::html::render_report(&self.report),
        })
    }
    fn exit_code(&self) -> u8 {
        self.report.verdict.exit_code
    }
    fn result_event(&self) -> EventKind {
        EventKind::Result {
            run_id: Some(self.run_id.clone()),
            path: self.path.clone(),
            report: Some(Box::new(self.report.clone())),
            data: None,
        }
    }
}

/// Result of `--dry-run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PlanOutput {
    pub plan: Plan,
}

impl CommandOutput for PlanOutput {
    fn text(&self) -> String {
        let p = &self.plan;
        let mut o = format!(
            "{} {} · {} · {} workers · {:.0}s warmup + {:.0}s window{}\n",
            bold("plan"),
            p.target,
            p.preset,
            p.concurrency_count,
            p.warmup_s,
            p.duration_s,
            if p.read_only { " · read-only" } else { "" }
        );
        o += &format!(
            "  ~{} operations · ~{} requests · ~{} transferred\n",
            p.estimated_ops_count,
            p.estimated_requests_count,
            crate::util::fmt_bytes(p.estimated_bytes)
        );
        o += &format!(
            "  writes {} objects under {} · deletes {}\n",
            p.objects_written_count, p.run_prefix, p.objects_deleted_count
        );
        o += &format!(
            "  cost {} · ETA {:.0}s\n",
            p.estimated_cost_usd
                .map(crate::util::fmt_usd)
                .unwrap_or_else(|| "not applicable".into()),
            p.eta_s
        );
        for a in &p.assumptions {
            o += &format!("  {}\n", dim(&format!("assumes {a}")));
        }
        o += &dim("dry run: nothing was sent to the target\n");
        o
    }
}

/// Parse the safety flags into budgets.
pub(super) fn budgets(s: &SafetyArgs) -> Result<Budgets> {
    let bad = |flag: &str, e: anyhow::Error| -> anyhow::Error {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("invalid {flag}: {e:#}"),
        )
        .into()
    };
    if let Some(c) = s.max_cost_usd {
        anyhow::ensure!(
            c.is_finite() && c >= 0.0,
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                "--max-cost-usd must be >= 0"
            )
        );
    }
    Ok(Budgets {
        max_cost_usd: s.max_cost_usd,
        max_requests_count: s.max_requests,
        max_bytes: s
            .max_bytes
            .as_deref()
            .map(|v| v.parse::<ByteSize>().map(|b| b.0))
            .transpose()
            .map_err(|e| bad("--max-bytes", e))?,
        max_duration_s: s
            .max_duration
            .as_deref()
            .map(|v| parse_duration(v).map(|d| d.as_secs_f64()))
            .transpose()
            .map_err(|e| bad("--max-duration", e))?,
    })
}

/// Everything the safety rails decide before a session opens.
pub(super) struct Rails {
    pub(super) run_id: RunId,
    pub(super) run_prefix: String,
    pub(super) budgets: Budgets,
    pub(super) read_only: bool,
}

pub(super) fn rails(profile: &Profile, s: &SafetyArgs) -> Result<Rails> {
    let run_id = RunId::new();
    let run_prefix = match &s.idempotency_key {
        Some(k) => crate::model::ids::idempotent_run_prefix(k),
        None => run_id.run_prefix(),
    };
    let existing = profile
        .workload
        .effective_dataset()
        .existing_prefix
        .is_some();
    let read_only = s.read_only || existing;
    if read_only {
        crate::guard::check_read_only(profile)?;
    }
    Ok(Rails {
        run_id,
        run_prefix,
        budgets: budgets(s)?,
        read_only,
    })
}

pub(super) fn guard_for(ctx: &Ctx, profile: &Profile, b: &Budgets) -> Option<Arc<Guard>> {
    if b.is_empty() {
        return None;
    }
    let price = crate::guard::usd_per_request(profile);
    if b.max_cost_usd.is_some() && price.is_none() {
        ctx.out.warn(
            "--max-cost-usd is not enforced: no price table applies to this target (set --provider)",
        );
    }
    Some(Guard::with_pricing(b.clone(), price))
}

/// Progress for this output mode: a live line on a terminal, events in ndjson mode,
/// messages on stderr otherwise.
pub(super) fn progress_for(ctx: &Ctx, total_s: f64) -> (ProgressFn, Option<Arc<live::LiveLine>>) {
    if ctx.out.format == OutputFormat::Ndjson {
        return (ctx.out.bus().progress_fn(Some(total_s)), None);
    }
    let quiet = ctx.out.quiet;
    if quiet || ctx.out.non_interactive || !std::io::stderr().is_terminal() {
        let msg: ProgressFn = Arc::new(move |p| {
            if let Progress::Message(m) = p
                && !quiet
            {
                eprintln!("{m}");
            }
        });
        return (msg, None);
    }
    let line = Arc::new(live::LiveLine::new(std::time::Duration::from_secs_f64(
        total_s,
    )));
    let l2 = line.clone();
    (Arc::new(move |p| l2.update(p)), Some(line))
}

fn save_outputs(r: &Report, o: &OutputArgs) -> Result<()> {
    if let Some(p) = &o.json {
        write_file(p, &serde_json::to_string_pretty(r)?)?;
    }
    if let Some(p) = &o.markdown {
        write_file(p, &markdown::render_report(r))?;
    }
    Ok(())
}

/// `--fail-on`: exit 3 when the diagnosis has findings of this severity or worse.
pub(super) fn apply_fail_on(r: &mut Report, o: &OutputArgs, ctx: &Ctx) {
    use crate::analysis::diagnosis::Severity;
    let Some(level) = o.fail_on else { return };
    let min = match level {
        FailOn::Warning => Severity::Warning,
        FailOn::Critical => Severity::Critical,
    };
    if r.worst_severity() >= min && r.verdict.exit_code == exit::OK {
        ctx.out.warn(format!(
            "`--fail-on {}`: the diagnosis has {} findings (exit {})",
            min.label(),
            r.worst_severity().label(),
            exit::GATE
        ));
        r.verdict.exit_code = exit::GATE;
    }
}

/// Save the run (unless `--no-save`); returns the run file path.
pub(super) fn save_run(ctx: &Ctx, run: &Run, o: &OutputArgs) -> Result<Option<String>> {
    save_outputs(&run.report, o)?;
    if o.no_save {
        return Ok(None);
    }
    let path = ctx
        .store()
        .save(run)
        .context("cannot save the run (set OPROBE_DATA_DIR or pass --no-save)")?;
    Ok(Some(path.display().to_string()))
}

/// Shared tail of `run` / `tune`: budget breach handling.
pub(super) fn budget_error(
    session: &Session,
    run_id: &RunId,
    ctx: &Ctx,
    saved: bool,
) -> Option<anyhow::Error> {
    let breach = session.guard()?.breach()?;
    if saved {
        ctx.out.warn(format!(
            "the partial run was saved: oprobe runs show {run_id}"
        ));
    }
    Some(breach.to_error())
}

pub async fn cmd_run(mut a: RunArgs, ctx: &Ctx) -> Result<u8> {
    // `oprobe run` without a target in an interactive terminal: guided first run.
    if a.target.target.is_none()
        && a.target.profile.is_none()
        && ctx.config()?.defaults.target.is_none()
        && super::onboard::should_offer(ctx)
    {
        match super::onboard::choose(ctx)? {
            super::onboard::Choice::Demo => {
                return super::demo::cmd_demo(DemoArgs::default(), ctx).await;
            }
            super::onboard::Choice::Target(name) => a.target.target = Some(name),
            super::onboard::Choice::Saved => return Ok(exit::OK),
        }
    }
    if a.repeat > 1 || a.baseline.is_some() {
        return super::repeat::cmd_run_repeat(a, ctx).await;
    }
    let (mut profile, target_name) =
        resolve_profile(ctx, &a.target, &a.workload, &a.knobs, &a.cost)?;
    apply_sla(&mut profile, &a.sla)?;
    if a.output.label.is_some() {
        profile.name = a.output.label.clone();
    }
    match measure(ctx, profile, target_name, &a.safety, &a.output).await? {
        Measured::Plan(plan) => ctx.out.finish(plan.as_ref()),
        Measured::Run(out) => ctx.out.finish(out.as_ref()),
    }
}

/// What [`measure`] produced.
pub(super) enum Measured {
    /// `--dry-run`: the plan; nothing was sent.
    Plan(Box<PlanOutput>),
    Run(Box<RunOutput>),
}

/// Measure a resolved profile under the safety rails, build the report and save the run
/// (shared by `oprobe run` and `oprobe demo`).
pub(super) async fn measure(
    ctx: &Ctx,
    profile: Profile,
    target_name: Option<String>,
    safety: &SafetyArgs,
    output: &OutputArgs,
) -> Result<Measured> {
    let rails = rails(&profile, safety)?;
    if safety.dry_run {
        let plan = crate::guard::plan(&profile, &rails.run_prefix, rails.read_only, &rails.budgets);
        return Ok(Measured::Plan(Box::new(PlanOutput { plan })));
    }
    let guard = guard_for(ctx, &profile, &rails.budgets);
    let cancel = Cancel::new();
    install_ctrl_c(cancel.clone());
    let total_s = profile.workload.duration.get().as_secs_f64();
    let (progress, line) = progress_for(ctx, total_s);
    if ctx.out.format == OutputFormat::Text {
        ctx.out.info(format!(
            "{} {} {}",
            bold("oprobe"),
            profile.target.display(),
            dim(&format!(
                "· {} · {} · run {}",
                profile.workload.preset.slug(),
                profile.knobs.summary(),
                rails.run_id
            ))
        ));
    }
    let started_at = SystemTime::now();
    let session = Session::open_with(
        profile,
        cancel.clone(),
        progress.clone(),
        SessionOptions {
            run_prefix: Some(rails.run_prefix.clone()),
            guard,
            trace_max_requests: trace_max_requests(ctx),
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
    ctx.out.info(dim("cleaning up…"));
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
    if run.report.run.interrupted || cancel.is_cancelled() {
        ctx.out
            .warn("run interrupted — partial results: they cover only the time before Ctrl-C");
    }
    Ok(Measured::Run(Box::new(RunOutput {
        run_id: rails.run_id.to_string(),
        path,
        report: run.report.clone(),
        full: Some(Box::new(run)),
    })))
}

/// Result of `oprobe tune`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TuneOutput {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub tune: TuneReport,
    /// Files written (`--emit-*`): kind → path.
    pub wrote: Vec<(String, String)>,
    /// Report of the winner's confirmation run.
    pub report: Report,
}

impl CommandOutput for TuneOutput {
    fn text(&self) -> String {
        let s = &self.tune;
        let mut o = String::new();
        let winner = s
            .trials
            .iter()
            .find(|t| t.id == s.winner_trial)
            .map(|t| t.knobs.to_knobs().summary())
            .unwrap_or_default();
        o += &format!(
            "\n{} {}\n",
            style::badge("TUNED", "42;30"),
            bold(&format!(
                "{winner} → {:+.0}% {} vs baseline",
                s.improvement_pct, s.objective
            ))
        );
        for n in &s.notes {
            o += &format!("  {} {n}\n", dim("·"));
        }
        for (kind, p) in &self.wrote {
            o += &format!("  {} wrote {kind}: {p}\n", green("✓"));
        }
        o += &format!(
            "\n{}\n",
            dim("Confirmation run of the winning configuration:")
        );
        o += &brief::render(&self.report, &self.run_id);
        o
    }
    fn markdown(&self) -> String {
        markdown::render_report(&self.report)
    }
    fn exit_code(&self) -> u8 {
        self.report.verdict.exit_code
    }
    fn result_event(&self) -> EventKind {
        EventKind::Result {
            run_id: Some(self.run_id.clone()),
            path: self.path.clone(),
            report: Some(Box::new(self.report.clone())),
            data: None,
        }
    }
}

pub async fn cmd_tune(a: TuneArgs, ctx: &Ctx) -> Result<u8> {
    use crate::tune::{Objective, TuneEvent, codegen, tune};
    let (mut profile, target_name) =
        resolve_profile(ctx, &a.target, &a.workload, &a.knobs, &a.cost)?;
    apply_sla(&mut profile, &a.sla)?;
    if a.output.label.is_some() {
        profile.name = a.output.label.clone();
    }
    let rails = rails(&profile, &a.safety)?;
    let objective = match a.objective {
        ObjectiveArg::Throughput => Objective::Throughput,
        ObjectiveArg::Latency => Objective::Latency,
        ObjectiveArg::Cost => Objective::Cost,
    };
    let opts = super::tune_options(
        objective,
        &super::TuneSettings {
            trial_duration: &a.trial_duration,
            trial_warmup: &a.trial_warmup,
            max_trials: a.max_trials,
            max_concurrency: a.max_concurrency,
            cooldown: &a.cooldown,
        },
        profile.workload.rate,
    )?;
    if a.safety.dry_run {
        // The plan of one trial times the trial budget.
        let mut p = profile.clone();
        p.workload.duration =
            crate::util::HumanDuration(opts.trial.duration * opts.max_trials as u32);
        p.workload.warmup = crate::util::HumanDuration(opts.trial.warmup * opts.max_trials as u32);
        let plan = crate::guard::plan(&p, &rails.run_prefix, rails.read_only, &rails.budgets);
        return ctx.out.finish(&PlanOutput { plan });
    }
    let guard = guard_for(ctx, &profile, &rails.budgets);
    let cancel = Cancel::new();
    install_ctrl_c(cancel.clone());
    let quiet = ctx.out.quiet;
    let ndjson = ctx.out.format == OutputFormat::Ndjson;
    let msg: ProgressFn = if ndjson {
        ctx.out.bus().progress_fn(None)
    } else {
        Arc::new(move |p| {
            if let Progress::Message(m) = p
                && !quiet
            {
                eprintln!("{} {m}", dim("·"));
            }
        })
    };
    let eta = (opts.trial.duration + opts.trial.warmup).as_secs_f64() * opts.max_trials as f64;
    ctx.out.info(dim(&format!(
        "tuning: up to {} trials of {} · ETA ≤ {:.0}s plus dataset upload",
        opts.max_trials, a.trial_duration, eta
    )));
    let started_at = SystemTime::now();
    let session = Session::open_with(
        profile,
        cancel.clone(),
        msg,
        SessionOptions {
            run_prefix: Some(rails.run_prefix.clone()),
            guard,
            trace_max_requests: trace_max_requests(ctx),
            preflight: true,
        },
    )
    .await?;
    // The trial table is report content: stdout in text mode (even with `-q`); otherwise it is
    // progress (stderr, or `trial` events in ndjson mode).
    let table_on_stdout = ctx.out.format == OutputFormat::Text;
    let table_line = move |line: String| {
        if table_on_stdout {
            println!("{line}");
        } else if !quiet && !ndjson {
            eprintln!("{line}");
        }
    };
    table_line(format!(
        "{} {} {}",
        bold("oprobe tune"),
        session.profile.target.display(),
        dim(&format!(
            "· {} · objective {} · ≤{} trials of {}",
            session.profile.workload.preset.slug(),
            opts.objective.label(),
            opts.max_trials,
            a.trial_duration
        ))
    ));
    table_line(dim(
        "   #  stage            conc  io  chunk   gap     cache  http     ops/s    MiB/s     p99       result",
    ));
    let bus = ctx.out.bus().clone();
    let on_event: Arc<dyn Fn(TuneEvent) + Send + Sync> = Arc::new(move |ev| match ev {
        TuneEvent::Started { .. } => {}
        TuneEvent::Message(m) => {
            if ndjson {
                bus.log(crate::events::LogLevel::Info, m);
            } else if !quiet {
                eprintln!("   {}", dim(&m));
            }
        }
        TuneEvent::Finished(t) => {
            if ndjson {
                bus.emit(EventKind::Trial { trial: (&t).into() });
                return;
            }
            let k = &t.knobs;
            let verdict = if t.throttled() {
                red(&format!("throttled ({})", t.throttled_responses))
            } else if t.feasible {
                green("ok")
            } else {
                yellow(&t.note)
            };
            table_line(format!(
                "  {:>2}  {:<15} {:>5} {:>3}  {:<7} {:<7} {:<6} {:<6} {:>8.1} {:>8.1} {:>9}  {verdict}",
                t.id,
                t.stage,
                k.concurrency,
                k.io_concurrency,
                k.chunk_size.to_string(),
                k.range_gap.to_string(),
                if k.cache { "on" } else { "off" },
                format!("{:?}", k.http_version).to_ascii_lowercase(),
                t.ops_per_s,
                t.mib_per_s,
                crate::util::fmt_ms(t.p99_ms),
            ));
        }
    });
    let outcome = tune(&session, &opts, on_event).await;
    let cleanup = session.close().await;
    let outcome = outcome?;
    let mut snap = outcome.winner_snapshot.clone();
    let budget_hit = session.guard().is_some_and(|g| g.tripped());
    if budget_hit {
        snap.interrupted = true;
    }
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
    });
    let tune_report = TuneReport::from(&outcome.summary);
    run.report.tune = Some(tune_report.clone());
    if budget_hit {
        run.report.verdict.exit_code = exit::BUDGET;
    }
    apply_fail_on(&mut run.report, &a.output, ctx);

    let header = format!(
        "objective {} · {:+.0}% vs baseline · {}",
        outcome.summary.objective,
        outcome.summary.improvement * 100.0,
        crate::util::now_rfc3339()
    );
    let mut wrote = Vec::new();
    if let Some(p) = &a.emit_profile {
        write_file(
            p,
            &codegen::profile_toml(&session.profile, &outcome.winner, &header)?,
        )?;
        wrote.push(("profile".to_owned(), p.display().to_string()));
    }
    if let Some(p) = &a.emit_rust {
        write_file(
            p,
            &codegen::rust_snippet(
                &session.profile.target,
                &outcome.winner,
                &session.capability,
            ),
        )?;
        wrote.push(("rust".to_owned(), p.display().to_string()));
    }
    if let Some(p) = &a.emit_python {
        write_file(
            p,
            &codegen::python_snippet(
                &session.profile.target,
                &outcome.winner,
                &session.capability,
            ),
        )?;
        wrote.push(("python".to_owned(), p.display().to_string()));
    }
    let path = save_run(ctx, &run, &a.output)?;
    if let Some(e) = budget_error(&session, &rails.run_id, ctx, path.is_some()) {
        return Err(e);
    }
    if cancel.is_cancelled() {
        ctx.out.warn("tuning interrupted — partial results");
    }
    ctx.out.finish(&TuneOutput {
        run_id: rails.run_id.to_string(),
        path,
        tune: tune_report,
        wrote,
        report: run.report,
    })
}
