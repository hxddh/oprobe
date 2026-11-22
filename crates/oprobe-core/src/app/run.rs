//! `oprobe run` and `oprobe tune`: measure, build the Report v3, save the run.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::input::*;
use super::{Ctx, live};
use crate::config::Profile;
use crate::engine::measure::{Mode, Rails};
use crate::engine::{Cancel, Progress, ProgressFn};
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::guard::{Budgets, Guard, Plan};
use crate::model::{Report, Run};
use crate::output::{CommandOutput, OutputFormat};
use crate::report::style::{bold, dim};
use crate::report::{brief, write_file};
use crate::util::{ByteSize, parse_duration};

/// Result of `oprobe run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunOutput {
    pub run_id: String,
    /// The saved run file (absent with `--no-save`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub report: Report,
    /// The run with its trace rows (not serialized).
    #[serde(skip)]
    #[schemars(skip)]
    pub full: Option<Box<Run>>,
}

impl CommandOutput for RunOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        crate::app::next::after_run(&self.report)
    }

    fn text(&self) -> String {
        brief::render(&self.report, &self.run_id)
    }
    fn exit_code(&self) -> u8 {
        self.report.verdict.exit_code
    }
}

/// Result of `oprobe run OPERATOR…`: one run per operator, in order, on the same workload.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunSet {
    pub runs: Vec<RunOutput>,
}

impl CommandOutput for RunSet {
    fn next(&self) -> Vec<crate::output::NextStep> {
        match self.runs.as_slice() {
            [one] => one.next(),
            _ => vec![crate::app::next::list_runs()],
        }
    }

    fn text(&self) -> String {
        let mut o: String = self
            .runs
            .iter()
            .map(|r| r.text())
            .collect::<Vec<_>>()
            .join("\n");
        if self.runs.len() > 1 {
            // Side by side (phase A builds the comparison view).
            let rows: Vec<Vec<String>> = self
                .runs
                .iter()
                .map(|r| {
                    let rep = &r.report;
                    vec![
                        rep.spec.operator_ref.clone(),
                        r.run_id.clone(),
                        format!("{:.1}", rep.summary.ops_per_s),
                        format!("{:.1}", rep.summary.mib_per_s),
                        crate::util::fmt_ms(rep.summary.latency.p99),
                        rep.resilience.throttled_responses.to_string(),
                    ]
                })
                .collect();
            o += "\n";
            o += &crate::report::style::table(
                &["operator", "run", "ops/s", "MiB/s", "p99", "throttled"],
                &rows,
                0,
            );
        }
        o
    }

    fn exit_code(&self) -> u8 {
        self.runs.iter().map(|r| r.exit_code()).max().unwrap_or(0)
    }
}

/// Result of `oprobe run --dry-run`: the plan of every operator.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunPlan {
    pub plans: Vec<Plan>,
}

impl CommandOutput for RunPlan {
    fn warnings(&self) -> Vec<String> {
        self.plans.iter().flat_map(|p| p.warnings.clone()).collect()
    }

    fn text(&self) -> String {
        self.plans
            .iter()
            .map(|p| PlanOutput { plan: p.clone() }.text())
            .collect::<Vec<_>>()
            .join("\n")
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
        vec![crate::output::NextStep::new("run it", p.command.clone())]
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
        o += &format!("  ETA {:.0}s\n", p.eta_s);
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
pub fn budgets(ctx: &Ctx, s: &Safety) -> Result<Budgets> {
    let bad = |flag: &str, e: anyhow::Error| -> anyhow::Error {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("invalid {flag}: {e:#}"),
        )
        .into()
    };
    let mut b = Budgets {
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
pub fn rails(ctx: &Ctx, profile: &Profile, s: &Safety) -> Result<Rails> {
    Rails::new(
        profile,
        s.read_only,
        s.idempotency_key.as_deref(),
        budgets(ctx, s)?,
    )
}

/// The command's live guard (one per command: every measurement of `run --repeat` or `matrix`
/// shares it, so its budgets cover the whole command).
pub fn guard_for(_ctx: &Ctx, profile: &Profile, b: &Budgets) -> Option<Arc<Guard>> {
    crate::engine::measure::guard_for(b, profile)
}

/// Progress for this output mode: a live line on a terminal, messages on stderr otherwise.
pub fn progress_for(ctx: &Ctx, total_s: f64) -> (ProgressFn, Option<Arc<live::LiveLine>>) {
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

/// Progress of the tuner: messages on stderr.
pub fn tune_progress(ctx: &Ctx) -> ProgressFn {
    let quiet = ctx.out.quiet;
    Arc::new(move |p| {
        if let Progress::Message(m) = p
            && !quiet
        {
            eprintln!("{} {m}", dim("·"));
        }
    })
}

/// The `--json` report file.
pub fn save_outputs(r: &Report, o: &OutputOptions) -> Result<()> {
    if let Some(p) = &o.json {
        write_file(p, &serde_json::to_string_pretty(r)?)?;
    }
    Ok(())
}

/// `--fail-on`: exit 3 when the diagnosis has findings of this severity or worse. Returns
/// whether the gate changed the exit code.
pub fn fail_on_gate(r: &mut Report, level: FailOn) -> bool {
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

/// What [`measure`] produced.
pub enum Measured {
    /// `--dry-run`: the plan; nothing was sent.
    Plan(Box<PlanOutput>),
    Run(Box<RunOutput>),
}

/// Measure a resolved profile under the safety rails, build the report and save the run
/// (shared by `oprobe run` and `oprobe demo`).
pub async fn measure(
    ctx: &Ctx,
    profile: Profile,
    target_name: Option<String>,
    safety: &Safety,
    output: &OutputOptions,
    cancel: Cancel,
) -> Result<Measured> {
    let guard = if safety.dry_run {
        None
    } else {
        guard_for(ctx, &profile, &budgets(ctx, safety)?)
    };
    measure_guarded(
        ctx,
        profile,
        target_name,
        None,
        safety,
        output,
        guard,
        &cancel,
    )
    .await
}

/// [`measure`] under the command's guard: every measurement of a command that measures
/// several times (`verify`, `fix`, `baseline calibrate`) passes the same guard, so the budgets
/// cover the whole command; once it has tripped, the next measurement refuses at once.
#[allow(clippy::too_many_arguments)]
pub async fn measure_guarded(
    ctx: &Ctx,
    profile: Profile,
    target_name: Option<String>,
    operator_ref: Option<String>,
    safety: &Safety,
    output: &OutputOptions,
    guard: Option<Arc<Guard>>,
    cancel: &Cancel,
) -> Result<Measured> {
    let rails = rails(ctx, &profile, safety)?;
    if safety.dry_run {
        let plan = rails.plan(&profile);
        return Ok(Measured::Plan(Box::new(PlanOutput { plan })));
    }
    for w in crate::guard::workload_warnings(&profile) {
        ctx.out.warn(w);
    }
    if ctx.out.format == OutputFormat::Text {
        ctx.out.info(format!(
            "{} {} {}",
            bold("oprobe"),
            profile.target().display(),
            dim(&format!(
                "· {} · {} · run {}",
                profile.workload.preset.slug(),
                crate::config::Knobs::of(&profile).summary(),
                rails.run_id
            ))
        ));
    }
    let m = super::measure::measure(
        ctx,
        profile,
        super::measure::Opts {
            target_name,
            operator_ref,
            rails,
            guard,
            output,
            cancel,
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
