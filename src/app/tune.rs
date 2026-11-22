//! `oprobe tune` (design v4 §6): the throttle-aware search, its trial table and the
//! [`Recommendation`].

use std::sync::Arc;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::*;
use super::run::{PlanOutput, guard_for, rails};
use crate::config::Profile;
use crate::engine::Cancel;
use crate::engine::measure::Mode;
use crate::errors::{ErrorCode, OprobeError};
use crate::events::EventKind;
use crate::model::{Report, TuneReport};
use crate::output::{CommandOutput, NextStep, OutputFormat};
use crate::report::style::{self, bold, dim, green, red, yellow};
use crate::report::{brief, markdown, write_file};
use crate::tune::recommend::Recommendation;
use crate::tune::{TuneEvent, TuneOptions, codegen};
use crate::util::parse_duration;

/// Result of `oprobe tune`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TuneOutput {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub tune: TuneReport,
    /// The recommended configuration (knobs, profile, code, deltas).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<Recommendation>,
    /// Files written (`--emit-*`): kind → path.
    pub wrote: Vec<(String, String)>,
    /// Report of the winner's confirmation run.
    pub report: Report,
    /// The session's normalised target, winner and service capability (code generation).
    #[serde(skip)]
    #[schemars(skip)]
    pub session: Option<(
        crate::config::TargetSpec,
        crate::config::Knobs,
        opendal::Capability,
    )>,
}

/// The trial table: one line per trial, with a header for every column.
pub fn trial_table(t: &TuneReport) -> String {
    let rows: Vec<Vec<String>> = t
        .trials
        .iter()
        .map(|tr| {
            let k = &tr.knobs;
            let mark = if tr.id == t.recommended_trial_id {
                "★"
            } else if t.pareto_trial_ids.contains(&tr.id) {
                "◆"
            } else {
                ""
            };
            vec![
                format!("{}{mark}", tr.id),
                tr.stage.clone(),
                k.concurrency_count.to_string(),
                k.io_concurrency_count.to_string(),
                crate::util::ByteSize(k.chunk_bytes).to_string(),
                crate::util::ByteSize(k.range_gap_bytes).to_string(),
                format!("{:.1}", tr.ops_per_s),
                format!("{:.1}", tr.mib_per_s),
                crate::util::fmt_ms(tr.p99_ms),
                format!("{:.2}%", tr.throttled_request_ratio * 100.0),
                tr.peak_inflight_count.to_string(),
                if tr.feasible {
                    green("ok")
                } else {
                    yellow(&tr.note)
                },
            ]
        })
        .collect();
    style::table(
        &[
            "#",
            "stage",
            "conc",
            "io",
            "chunk",
            "gap",
            "ops/s",
            "MiB/s",
            "p99",
            "throttled",
            "peak",
            "result",
        ],
        &rows,
        2,
    )
}

impl CommandOutput for TuneOutput {
    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD
    }

    fn text(&self) -> String {
        let s = &self.tune;
        let mut o = String::new();
        o += &format!(
            "\n{} {}\n",
            if s.no_feasible_trial {
                style::badge("NO FEASIBLE TRIAL", "43;30")
            } else {
                style::badge("TUNED", "42;30")
            },
            bold(&s.headline)
        );
        if let Some(r) = &self.recommendation {
            if !s.baseline_feasible {
                o += &format!(
                    "  {} the baseline is infeasible, so deltas are taken against the best feasible trial #{}\n",
                    dim("·"),
                    r.vs_best.baseline_trial
                );
            }
            if !r.overrides.is_empty() {
                o += &format!("  {} change: {}\n", dim("·"), r.overrides.summary());
            }
        }
        for n in &s.notes {
            o += &format!("  {} {n}\n", dim("·"));
        }
        if !s.pareto_trial_ids.is_empty() {
            o += &format!(
                "  {} Pareto front (goodput, p99, cost, throttling): trials {}\n",
                dim("·"),
                s.pareto_trial_ids
                    .iter()
                    .map(|i| format!("#{i}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
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
        let mut md = format!("## oprobe tune — {}\n\n", self.tune.headline);
        if let Some(r) = &self.recommendation {
            md += &format!("```toml\n{}```\n\n", r.profile_toml);
        }
        md + &markdown::render_report(&self.report)
    }
    fn exit_code(&self) -> u8 {
        self.report.verdict.exit_code
    }
    fn next(&self) -> Vec<NextStep> {
        let mut v = vec![NextStep::new(
            "run with the recommended settings",
            format!("oprobe run --from {}", self.run_id),
        )];
        if self.tune.recommendation.is_some() {
            v.push(NextStep::new(
                "save the recommended profile",
                format!(
                    "oprobe tune {} --emit-profile tuned.toml",
                    crate::model::spec::shell_quote(&self.report.spec.target_ref)
                ),
            ));
        }
        v
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

/// Time one tuner trial takes (warmup, window and cooldown).
fn trial_wall(opts: &TuneOptions) -> std::time::Duration {
    opts.trial.duration + opts.trial.warmup + opts.cooldown
}

/// `tune --budget`: cap the trials so the estimated wall time fits `budget`.
pub fn fit_trials(opts: &mut TuneOptions, budget: std::time::Duration) -> Result<()> {
    let per = trial_wall(opts).as_secs_f64().max(1e-3);
    let fit = (budget.as_secs_f64() / per).floor() as usize;
    if fit < tune_defaults::MIN_TRIALS {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!(
                "--budget {:.0}s fits {fit} trial(s) of {per:.0}s; the tuner needs at least {}",
                budget.as_secs_f64(),
                tune_defaults::MIN_TRIALS
            ),
        )
        .with_hint("raise --budget, or shorten --trial-duration / --trial-warmup / --cooldown")
        .into());
    }
    opts.max_trials = opts.max_trials.min(fit);
    Ok(())
}

/// The tuner options of `s` for `profile`: flags, then the profile's SLA, then the built-in
/// defaults; the target's remembered capacity as the prior.
pub fn options_for(
    ctx: &Ctx,
    profile: &Profile,
    target_name: Option<&str>,
    s: &TuneSearch,
) -> Result<TuneOptions> {
    let mut opts = super::profile::tune_options(
        s.objective,
        &super::profile::TuneSettings {
            trial_duration: &s.trial_duration,
            trial_warmup: &s.trial_warmup,
            max_trials: s.max_trials,
            max_concurrency: s.max_concurrency,
            cooldown: &s.cooldown,
            max_throttle_ratio: s.max_throttle_ratio.or(profile.sla.max_throttle_rate),
            boundary_repeats: s.boundary_repeats,
            headroom_ratio: s.headroom_ratio,
        },
        profile.workload.rate,
    )
    .map_err(|e| OprobeError::new(ErrorCode::UsageInvalidArgument, format!("{e:#}")))?;
    let spec = crate::engine::measure::spec_of(profile, target_name);
    if let Some(id) = spec.target_id() {
        opts.prior = crate::analysis::capacity::load(
            &ctx.paths.capacity_file(),
            &id,
            &crate::analysis::capacity::history_key(&spec),
        );
    }
    Ok(opts)
}

/// Run the tuner on `profile` under the safety rails and build the result (no output).
/// `guard` is the command's guard ([`super::run::guard_for`]): `fix` shares one across its
/// diagnosis, tuning and verification.
#[allow(clippy::too_many_arguments)]
pub async fn run_tune(
    ctx: &Ctx,
    profile: Profile,
    target_name: Option<String>,
    opts: TuneOptions,
    safety: &Safety,
    output: &OutputOptions,
    guard: Option<Arc<crate::guard::Guard>>,
    cancel: &Cancel,
) -> Result<TuneOutput> {
    let rails = rails(ctx, &profile, safety)?;
    let quiet = ctx.out.quiet;
    let ndjson = ctx.out.format == OutputFormat::Ndjson;
    // The trial lines are report content: stdout in text mode (even with `-q`); otherwise
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
        profile.target.display(),
        dim(&format!(
            "· {} · objective {} · throttle budget {:.2}% · ≤{} trials of {:.1}s",
            profile.workload.preset.slug(),
            opts.objective.label(),
            opts.max_throttle_ratio * 100.0,
            opts.max_trials,
            opts.trial.duration.as_secs_f64()
        ))
    ));
    table_line(dim(
        "   #  stage            conc  io  chunk   gap     cache  http     ops/s    MiB/s     p99  throttled  result",
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
            let verdict = if t.feasible {
                green("ok")
            } else if t.throttled() {
                red(&t.note)
            } else {
                yellow(&t.note)
            };
            table_line(format!(
                "  {:>2}  {:<15} {:>5} {:>3}  {:<7} {:<7} {:<6} {:<6} {:>8.1} {:>8.1} {:>7} {:>9.2}%  {verdict}",
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
                t.throttled_request_ratio * 100.0,
            ));
        }
    });
    let m = super::measure::measure(
        ctx,
        profile,
        super::measure::Opts {
            target_name,
            rails,
            guard,
            output,
            cancel,
            mode: Mode::Tune { opts, on_event },
        },
    )
    .await?;
    let report = m.run.report;
    let tune = report
        .tune
        .clone()
        .context("the tuner returned no report")?;
    Ok(TuneOutput {
        run_id: report.run.id.clone(),
        path: m.path.map(|p| p.display().to_string()),
        recommendation: tune.recommendation.clone(),
        tune,
        wrote: Vec::new(),
        report,
        session: m.tuned.map(|t| (t.profile.target, t.winner, t.capability)),
    })
}

/// What [`tune`] produced.
#[derive(Clone, Debug)]
pub enum TuneOutcome {
    /// `safety.dry_run`: the plan of the whole search.
    Plan(Box<PlanOutput>),
    Tuned(Box<TuneOutput>),
}

/// `oprobe tune`: search, write the requested files, and return the result (the CLI prints
/// [`trial_table`] before it in text mode).
pub async fn tune(ctx: &Ctx, a: TuneRequest, cancel: Cancel) -> Result<TuneOutcome> {
    let (mut profile, target_name) =
        super::profile::resolve_profile(ctx, &a.target, &a.workload, &a.knobs, &a.cost)?;
    super::profile::apply_sla(&mut profile, &a.sla)?;
    if a.output.label.is_some() {
        profile.name = a.output.label.clone();
    }
    let mut opts = options_for(ctx, &profile, target_name.as_deref(), &a.search)?;
    let budget = a
        .budget
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e| {
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("invalid --budget: {e:#}"),
            )
        })?;
    if let Some(b) = budget {
        fit_trials(&mut opts, b)?;
    }
    let eta = trial_wall(&opts).as_secs_f64() * opts.max_trials as f64;
    if a.safety.dry_run {
        // The plan of one trial times the trial budget.
        let r = rails(ctx, &profile, &a.safety)?;
        let mut p = profile.clone();
        p.workload.duration =
            crate::util::HumanDuration(opts.trial.duration * opts.max_trials as u32);
        p.workload.warmup = crate::util::HumanDuration(opts.trial.warmup * opts.max_trials as u32);
        let mut plan = r.plan(&p);
        plan.eta_s = plan.eta_s.max(eta);
        return Ok(TuneOutcome::Plan(Box::new(PlanOutput { plan })));
    }
    ctx.out.info(dim(&format!(
        "tuning: up to {} trials of {}{} · ETA ≤ {:.0}s plus dataset upload",
        opts.max_trials,
        a.search.trial_duration,
        budget
            .map(|b| format!(" (budget {:.0}s)", b.as_secs_f64()))
            .unwrap_or_default(),
        eta
    )));
    let guard = guard_for(ctx, &profile, &super::run::budgets(ctx, &a.safety)?);
    let mut out = run_tune(
        ctx,
        profile,
        target_name,
        opts,
        &a.safety,
        &a.output,
        guard,
        &cancel,
    )
    .await?;
    let header = format!("{} · {}", out.tune.headline, crate::util::now_rfc3339());
    let rec = out.recommendation.clone();
    if let Some(p) = &a.emit_profile {
        let text = match &rec {
            Some(r) => r.profile_toml.clone(),
            None => {
                let tp = crate::tune::recommend::portable_profile(&out.report.spec);
                codegen::profile_toml(&tp, &out.report.spec.profile.knobs, &header)?
            }
        };
        write_file(p, &text)?;
        out.wrote
            .push(("profile".to_owned(), p.display().to_string()));
    }
    let (session_target, knobs, capability) = out.session.clone().unwrap_or_else(|| {
        (
            out.report.spec.profile.target.clone(),
            out.report.spec.profile.knobs.clone(),
            opendal::Capability::default(),
        )
    });
    if let Some(p) = &a.emit_rust {
        let text = match &rec {
            Some(r) => r.rust_snippet.clone(),
            None => codegen::rust_snippet(&session_target, &knobs, &capability),
        };
        write_file(p, &text)?;
        out.wrote.push(("rust".to_owned(), p.display().to_string()));
    }
    if let Some(p) = &a.emit_python {
        write_file(
            p,
            &codegen::python_snippet(&session_target, &knobs, &capability),
        )?;
        out.wrote
            .push(("python".to_owned(), p.display().to_string()));
    }
    Ok(TuneOutcome::Tuned(Box::new(out)))
}
