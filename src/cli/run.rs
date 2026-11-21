//! `oprobe run` and `oprobe tune`: measure, build the Report v3, save the run.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::args::*;
use super::{Ctx, install_ctrl_c, live};
use crate::config::Profile;
use crate::engine::measure::{Mode, Rails};
use crate::engine::{Cancel, Progress, ProgressFn};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::events::EventKind;
use crate::guard::{Budgets, Guard, Plan};
use crate::model::{Report, Run};
use crate::output::{CommandOutput, OutputFormat};
use crate::report::style::{bold, dim};
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
    fn next(&self) -> Vec<crate::output::NextStep> {
        crate::cli::next::after_run(&self.report)
    }

    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD_HTML
    }

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
    fn warnings(&self) -> Vec<String> {
        self.plan.warnings.clone()
    }
    fn next(&self) -> Vec<crate::output::NextStep> {
        let p = &self.plan;
        if p.command.is_empty() {
            return Vec::new();
        }
        vec![
            crate::output::NextStep::new("run it", p.command.clone()).with_mcp(
                "start_run",
                serde_json::json!({
                    "target": p.target_ref,
                    "preset": p.preset,
                    "duration": format!("{}s", p.duration_s),
                    "warmup": format!("{}s", p.warmup_s),
                    "concurrency": p.concurrency_count,
                }),
            ),
        ]
    }
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
        for w in &p.warnings {
            o += &format!("  {} {w}\n", crate::report::style::yellow("warning:"));
        }
        o += &dim("dry run: nothing was sent to the target\n");
        o
    }
}

/// Parse the safety flags into budgets; unset ones come from `[defaults.budgets]`.
pub(super) fn budgets(ctx: &Ctx, s: &SafetyArgs) -> Result<Budgets> {
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
    let mut b = Budgets {
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
    };
    ctx.config()?.defaults.budgets.apply(&mut b)?;
    Ok(b)
}

/// The safety rails of one measurement (a new run id each call).
pub(super) fn rails(ctx: &Ctx, profile: &Profile, s: &SafetyArgs) -> Result<Rails> {
    Rails::new(
        profile,
        s.read_only,
        s.idempotency_key.as_deref(),
        budgets(ctx, s)?,
    )
}

/// The command's live guard (one per command: every measurement of `run --repeat` or `matrix`
/// shares it, so its budgets cover the whole command).
pub(super) fn guard_for(ctx: &Ctx, profile: &Profile, b: &Budgets) -> Option<Arc<Guard>> {
    let g = crate::engine::measure::guard_for(b, profile)?;
    if b.max_cost_usd.is_some() && crate::guard::usd_per_request(profile).is_none() {
        ctx.out.warn(
            "--max-cost-usd is not enforced: no price table applies to this target (set --provider)",
        );
    }
    Some(g)
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

/// Progress of the tuner: messages on stderr, or events in ndjson mode.
pub(super) fn tune_progress(ctx: &Ctx) -> ProgressFn {
    if ctx.out.format == OutputFormat::Ndjson {
        return ctx.out.bus().progress_fn(None);
    }
    let quiet = ctx.out.quiet;
    Arc::new(move |p| {
        if let Progress::Message(m) = p
            && !quiet
        {
            eprintln!("{} {m}", dim("·"));
        }
    })
}

/// `--json` / `--markdown` report files.
pub(super) fn save_outputs(r: &Report, o: &OutputArgs) -> Result<()> {
    if let Some(p) = &o.json {
        write_file(p, &serde_json::to_string_pretty(r)?)?;
    }
    if let Some(p) = &o.markdown {
        write_file(p, &markdown::render_report(r))?;
    }
    Ok(())
}

/// `--fail-on`: exit 3 when the diagnosis has findings of this severity or worse. Returns
/// whether the gate changed the exit code.
pub(super) fn fail_on_gate(r: &mut Report, level: FailOn) -> bool {
    use crate::analysis::diagnosis::Severity;
    let min = match level {
        FailOn::Warning => Severity::Warning,
        FailOn::Critical => Severity::Critical,
    };
    if r.worst_severity() >= min && r.verdict.exit_code == exit::OK {
        r.verdict.exit_code = exit::GATE;
        return true;
    }
    false
}

pub async fn cmd_run(mut a: RunArgs, ctx: &Ctx) -> Result<u8> {
    // `oprobe run` without a target in an interactive terminal: guided first run.
    if a.target.target.is_none()
        && a.target.profile.is_none()
        && a.from.is_none()
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
    let (profile, target_name) = super::run_profile(ctx, &a)?;
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
    let guard = if safety.dry_run {
        None
    } else {
        guard_for(ctx, &profile, &budgets(ctx, safety)?)
    };
    measure_guarded(ctx, profile, target_name, safety, output, guard).await
}

/// [`measure`] under the command's guard: every measurement of a command that measures
/// several times (`verify`, `fix`, `baseline calibrate`) passes the same guard, so the budgets
/// cover the whole command; once it has tripped, the next measurement refuses at once.
pub(super) async fn measure_guarded(
    ctx: &Ctx,
    profile: Profile,
    target_name: Option<String>,
    safety: &SafetyArgs,
    output: &OutputArgs,
    guard: Option<Arc<Guard>>,
) -> Result<Measured> {
    let rails = rails(ctx, &profile, safety)?;
    if safety.dry_run {
        let plan = rails.plan(&profile);
        return Ok(Measured::Plan(Box::new(PlanOutput { plan })));
    }
    for w in crate::guard::workload_warnings(&profile) {
        ctx.out.warn(w);
    }
    let cancel = Cancel::new();
    install_ctrl_c(cancel.clone());
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
    let m = super::measure::measure(
        ctx,
        profile,
        super::measure::Opts {
            target_name,
            rails,
            guard,
            output,
            cancel: &cancel,
            mode: Mode::Run,
        },
    )
    .await?;
    let run = m.run;
    Ok(Measured::Run(Box::new(RunOutput {
        run_id: run.id().to_owned(),
        path: m.path.map(|p| p.display().to_string()),
        report: run.report.clone(),
        full: Some(Box::new(run)),
    })))
}
