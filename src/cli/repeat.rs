//! `oprobe run --repeat N` (N saved runs sharing one label, summarised with confidence
//! intervals) and `oprobe run --baseline NAME` (compare against a named baseline after the
//! run, exit 3 on a regression; a passing run joins the baseline's rolling window).

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use super::compare::{RunSetStats, baseline_runs};
use super::measure::{Opts, measure};
use crate::analysis::compare::{Comparison, compare};
use crate::engine::Cancel;
use crate::engine::measure::Mode;
use crate::errors::exit;
use crate::events::EventKind;
use crate::output::CommandOutput;
use crate::report::diff::Thresholds;
use crate::report::style::{bold, dim, green, red};
use crate::store::{Baselines, RunSummary};

/// Result of `run --repeat` / `run --baseline`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RepeatOutput {
    /// The label every run of the group carries (`compare all:LABEL` selects them).
    pub group: String,
    pub runs: Vec<RunSummary>,
    pub stats: RunSetStats,
    /// Saved run files, in run order.
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// Comparison with the baseline (absent when the baseline was just created).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Comparison>,
    /// The runs were added to the baseline.
    pub added_to_baseline: bool,
    pub exit_code: u8,
}

impl CommandOutput for RepeatOutput {
    fn text(&self) -> String {
        let mut o = String::new();
        if self.runs.len() > 1 {
            o += &format!(
                "{} {} ({} runs)\n",
                bold("repeat group"),
                self.group,
                self.runs.len()
            );
            let rows: Vec<Vec<String>> = self
                .runs
                .iter()
                .map(|r| {
                    vec![
                        r.id.clone(),
                        r.verdict_level.as_str().to_owned(),
                        format!("{:.1}", r.ops_per_s),
                        crate::util::fmt_ms(r.p50_ms),
                        crate::util::fmt_ms(r.p99_ms),
                        format!("{:.2}%", r.error_ratio * 100.0),
                    ]
                })
                .collect();
            o += &crate::report::style::table(
                &["run", "verdict", "ops/s", "p50", "p99", "errors"],
                &rows,
                2,
            );
            o += "  ";
            o += &self.stats.text();
        } else if let Some(r) = self.runs.first() {
            o += &format!(
                "{} run {} · {:.1} ops/s · p50 {} · p99 {}\n",
                bold("measured"),
                r.id,
                r.ops_per_s,
                crate::util::fmt_ms(r.p50_ms),
                crate::util::fmt_ms(r.p99_ms)
            );
        }
        match (&self.baseline, &self.comparison) {
            (Some(b), Some(c)) => {
                o += &format!("\n{} baseline {b}\n", bold("compared with"));
                o += &c.render_terminal();
            }
            (Some(b), None) => {
                o += &format!("{} baseline {b} created with these runs\n", green("✓"));
            }
            _ => {}
        }
        if self.added_to_baseline && self.comparison.is_some() {
            o += &dim("runs added to the baseline\n");
        }
        if self.runs.len() > 1 {
            o += &dim(&format!(
                "compare later: oprobe compare all:{} last\n",
                self.group
            ));
        }
        o
    }
    fn markdown(&self) -> String {
        match &self.comparison {
            Some(c) => c.render_markdown(),
            None => format!("```text\n{}```\n", self.text()),
        }
    }
    fn exit_code(&self) -> u8 {
        self.exit_code
    }
    fn result_event(&self) -> EventKind {
        EventKind::Result {
            run_id: self.runs.last().map(|r| r.id.clone()),
            path: self.paths.last().cloned(),
            report: None,
            data: serde_json::to_value(self).ok(),
        }
    }
}

pub async fn cmd_run_repeat(a: RunArgs, ctx: &Ctx) -> Result<u8> {
    let (mut profile, target_name) =
        super::resolve_profile(ctx, &a.target, &a.workload, &a.knobs, &a.cost)?;
    super::apply_sla(&mut profile, &a.sla)?;
    let first = super::run::rails(ctx, &profile, &a.safety)?;
    if a.safety.dry_run {
        let mut plan = first.plan(&profile);
        if a.repeat > 1 {
            plan.assumptions.push(format!(
                "this plan is for one of {} repeated runs; budgets cover all of them together",
                a.repeat
            ));
        }
        return ctx.out.finish(&super::run::PlanOutput { plan });
    }
    // One guard for the whole command: the budgets cap all repeated runs together.
    let guard = super::run::guard_for(ctx, &profile, &first.budgets);
    let mut next_rails = Some(first);
    // A repeat group needs a shared label (`compare all:LABEL`); a single run keeps the user's.
    let mut group = a.output.label.clone().unwrap_or_else(|| {
        if a.repeat > 1 {
            format!("repeat-{}", crate::model::RunId::new())
        } else {
            String::new()
        }
    });
    if !group.is_empty() {
        profile.name = Some(group.clone());
    }
    let cancel = Cancel::new();
    super::install_ctrl_c(cancel.clone());
    let mut runs = Vec::new();
    let mut paths = Vec::new();
    for i in 0..a.repeat {
        if cancel.is_cancelled() {
            break;
        }
        if a.repeat > 1 {
            ctx.out.info(dim(&format!(
                "run {}/{} of {group} · {}",
                i + 1,
                a.repeat,
                profile.target.display()
            )));
        }
        let rails = match next_rails.take() {
            Some(r) => r,
            None => super::run::rails(ctx, &profile, &a.safety)?,
        };
        let m = measure(
            ctx,
            profile.clone(),
            Opts {
                target_name: target_name.clone(),
                rails,
                guard: guard.clone(),
                output: &a.output,
                cancel: &cancel,
                mode: Mode::Run,
            },
        )
        .await?;
        if let Some(p) = m.path {
            paths.push(p.display().to_string());
        }
        runs.push(m.run);
    }
    if group.is_empty() {
        group = runs.first().map(|r| r.id().to_owned()).unwrap_or_default();
    }
    let summaries: Vec<RunSummary> = runs.iter().map(|r| RunSummary::from(&r.report)).collect();
    let mut exit_code = runs
        .iter()
        .map(|r| r.report.verdict.exit_code)
        .max()
        .unwrap_or(exit::OK);
    if cancel.is_cancelled() {
        exit_code = exit::INTERRUPTED;
    }
    let mut comparison = None;
    let mut added = false;
    if let Some(name) = &a.baseline {
        let file = ctx.paths.baselines_file();
        let existing = Baselines::load(&file)?;
        if existing.get(name).is_some() && !a.output.no_save {
            let base = baseline_runs(ctx, name)?;
            let c = compare(
                &base,
                &runs,
                &format!("baseline {name}"),
                &group,
                &Thresholds::default(),
            );
            if !c.passed {
                ctx.out
                    .info(red(&bold("regression against the baseline (exit 3)")));
                exit_code = exit_code.max(exit::GATE);
            }
            added = c.passed;
            comparison = Some(c);
        } else {
            added = !a.output.no_save;
        }
        if added && exit_code == exit::OK {
            let mut b = Baselines::load(&file)?;
            for r in &runs {
                b.add_run(name, r.id());
            }
            b.save(&file)?;
        } else {
            added = false;
        }
    }
    ctx.out.finish(&RepeatOutput {
        group,
        stats: RunSetStats::of(&summaries),
        runs: summaries,
        paths,
        baseline: a.baseline.clone(),
        comparison,
        added_to_baseline: added,
        exit_code,
    })
}
