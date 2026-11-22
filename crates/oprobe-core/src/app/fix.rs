//! `oprobe fix TARGET` (design v4 §6, §9): diagnose → tune → verify. The same loop as MCP
//! `recommend_config`: one run of the target, the throttle-aware tuner on the same workload,
//! then `verify` of the recommendation against the diagnosis run (interleaved replays). The
//! result is a [`Recommendation`] (with `confirmed` set) and a profile patch.

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::*;
use super::verify::{Hypothesis, Replayer, VerifyCfg, VerifyResult, verify_hypotheses};
use crate::analysis::verification::{Direction, Expectation, VerifyOutcome};
use crate::config::Knobs;
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError};
use crate::output::{CommandOutput, NextStep};
use crate::report::style::{self, bold, dim, green, red, yellow};
use crate::report::write_file;
use crate::tune::recommend::Recommendation;

/// Result of `oprobe fix`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FixOutput {
    /// The diagnosis run (the workload as given).
    pub diagnosis_run_id: String,
    /// Finding codes of the diagnosis run.
    pub finding_codes: Vec<String>,
    /// The tuner's run (its confirmation trial).
    pub tune_run_id: String,
    /// What to change; `confirmed` is the verification's outcome.
    pub recommendation: Recommendation,
    /// The `[knobs]` that differ from the diagnosis run, as TOML to merge into a profile.
    pub profile_patch: String,
    /// The fix menu (design v5 §4, Fix 5): 2–4 Pareto options with goodput, p99, throttling,
    /// cost per month and verify status; the recommended one first.
    pub options: Vec<crate::analysis::menu::FixOption>,
    /// The verification of the recommendation (absent when nothing changes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyResult>,
    /// Files written (`--write-profile`): kind → path.
    pub wrote: Vec<(String, String)>,
    /// A self-contained replay of the recommendation, when the diagnosis was not saved
    /// (`--no-save`): `run --from` cannot name it.
    #[serde(skip)]
    #[schemars(skip)]
    pub unsaved_replay: Option<String>,
}

impl CommandOutput for FixOutput {
    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD
    }

    fn text(&self) -> String {
        let r = &self.recommendation;
        let verdict = match r.confirmed {
            Some(VerifyOutcome::Confirmed) => green("confirmed"),
            Some(VerifyOutcome::Refuted) => red("refuted"),
            Some(VerifyOutcome::Inconclusive) => yellow("inconclusive"),
            None => dim("not verified (no change)"),
        };
        let mut o = format!(
            "\n{} {} — verification {verdict}\n",
            style::badge("FIX", "42;30"),
            bold(&if r.overrides.is_empty() {
                "the current configuration is already the best measured".to_owned()
            } else {
                r.overrides.summary()
            })
        );
        o += &format!(
            "  {} diagnosis run {} (findings: {}) · tune run {}\n",
            dim("·"),
            self.diagnosis_run_id,
            if self.finding_codes.is_empty() {
                "none".to_owned()
            } else {
                self.finding_codes.join(", ")
            },
            self.tune_run_id
        );
        o += &format!(
            "  {} {} {:.1} → {:.1} {} vs baseline{}; {:+.0}% vs the best feasible trial\n",
            dim("·"),
            r.vs_baseline.metric,
            r.vs_baseline.baseline,
            r.vs_baseline.candidate,
            r.vs_baseline.unit,
            if r.vs_baseline.baseline_feasible {
                ""
            } else {
                " (baseline infeasible)"
            },
            r.vs_best.delta_ratio * 100.0
        );
        if let Some(v) = &self.verify {
            for f in &v.findings {
                o += &format!("  {} {}\n", dim("·"), f.reason);
            }
        }
        if !self.options.is_empty() {
            o += &format!("\n{}\n", bold("options"));
            for (i, opt) in self.options.iter().enumerate() {
                o += &format!(
                    "  {} {:<40} {} — {}{}\n",
                    if opt.recommended { green("▶") } else { dim(&format!("{}", i + 1)) },
                    opt.label,
                    opt.summary(),
                    if opt.overrides.is_empty() {
                        "no change".to_owned()
                    } else {
                        opt.overrides.summary()
                    },
                    match opt.verify {
                        crate::analysis::menu::OptionVerify::NotVerified => String::new(),
                        v => format!(" [{}]", serde_json::to_value(v).ok().and_then(|x| x.as_str().map(str::to_owned)).unwrap_or_default()),
                    }
                );
            }
        }
        if !self.profile_patch.is_empty() {
            o += &format!("\n{}\n{}", dim("profile patch:"), self.profile_patch);
        }
        for (kind, p) in &self.wrote {
            o += &format!("  {} wrote {kind}: {p}\n", green("✓"));
        }
        o
    }
    fn next(&self) -> Vec<NextStep> {
        if self.recommendation.overrides.is_empty() {
            return Vec::new();
        }
        let cli = self.unsaved_replay.clone().unwrap_or_else(|| {
            crate::model::RunSpec::rerun_from(
                &self.diagnosis_run_id,
                &self.recommendation.overrides,
            )
        });
        vec![NextStep::new("run with the recommended settings", cli)]
    }
}

/// The `[knobs]` entries of `to` that differ from `from`, as a TOML table.
pub fn knobs_patch(from: &Knobs, to: &Knobs) -> String {
    let (Ok(toml::Value::Table(a)), Ok(toml::Value::Table(b))) =
        (toml::Value::try_from(from), toml::Value::try_from(to))
    else {
        return String::new();
    };
    let mut t = toml::map::Map::new();
    for (k, v) in b {
        if a.get(&k) != Some(&v) {
            t.insert(k, v);
        }
    }
    if t.is_empty() {
        return String::new();
    }
    let mut root = toml::map::Map::new();
    root.insert("knobs".into(), toml::Value::Table(t));
    toml::to_string(&toml::Value::Table(root)).unwrap_or_default()
}

/// What [`fix`] produced (fix has no dry run).
pub type FixOutcome = FixOutput;

/// `oprobe fix`: diagnose → tune → verify.
pub async fn fix(ctx: &Ctx, a: FixRequest, cancel: Cancel) -> Result<FixOutcome> {
    let (mut profile, target_name) = if a.from.is_some() || !a.set.is_empty() {
        // `--from RUN --set …`: the workload exactly as `run --from` replays it (MCP
        // `recommend_config` passes a stored run this way, `--existing` prefix included).
        super::profile::run_profile(
            ctx,
            &ProfileRequest {
                target: a.target.clone(),
                workload: a.workload.clone(),
                knobs: a.knobs.clone(),
                cost: a.cost.clone(),
                sla: a.sla.clone(),
                from: a.from.clone(),
                set: a.set.clone(),
                label: None,
            },
        )?
    } else {
        let (mut p, name) =
            super::profile::resolve_profile(ctx, &a.target, &a.workload, &a.knobs, &a.cost)?;
        super::profile::apply_sla(&mut p, &a.sla)?;
        (p, name)
    };
    if a.output.label.is_some() {
        profile.name = a.output.label.clone();
    }
    if a.safety.dry_run {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            "`oprobe fix` has no dry run: it measures, tunes and verifies",
        )
        .with_hint("plan the pieces: `oprobe run TARGET --dry-run`, `oprobe tune TARGET --dry-run`")
        .into());
    }
    let mut opts = super::tune::options_for(ctx, &profile, target_name.as_deref(), &a.search)?;
    if let Some(b) = a.budget.as_deref() {
        let b = crate::input::positive_duration(b).map_err(anyhow::Error::from)?;
        super::tune::fit_trials(&mut opts, b)?;
    }
    let budget = opts.max_throttle_ratio;
    // One guard for the whole command: diagnosis, tuning and verification share the budgets.
    let guard = super::run::guard_for(ctx, &profile, &super::run::budgets(ctx, &a.safety)?);
    // 1. Diagnose.
    ctx.out.info(bold("fix 1/3 · diagnose"));
    let diag = match super::run::measure_guarded(
        ctx,
        profile.clone(),
        target_name.clone(),
        &a.safety,
        &a.output,
        guard.clone(),
        &cancel,
    )
    .await?
    {
        super::run::Measured::Run(o) => o.full.map(|r| *r).context("the run kept no rows")?,
        super::run::Measured::Plan(_) => unreachable!("not a dry run"),
    };
    // 2. Tune the same workload.
    ctx.out.info(bold("fix 2/3 · tune"));
    let tuned = super::tune::run_tune(
        ctx,
        profile.clone(),
        target_name.clone(),
        opts,
        &a.safety,
        &a.output,
        guard.clone(),
        &cancel,
    )
    .await?;
    let mut rec = tuned
        .recommendation
        .clone()
        .context("the tuner produced no recommendation")?;
    // The change relative to the diagnosis run.
    let winner = tuned.report.spec.profile.knobs.clone();
    rec.overrides =
        crate::tune::recommend::knob_overrides(&diag.report.spec.profile.knobs, &winner);
    if let Some(r) = tuned
        .tune
        .trials
        .iter()
        .find(|x| x.id == tuned.tune.recommended_trial_id)
        .and_then(|x| x.rate_per_s)
        && Some(r) != diag.report.spec.profile.workload.rate
    {
        let _ = rec.overrides.set("rate", &format!("{r:.0}"));
    }
    // 3. Verify the recommendation against the diagnosis run.
    let verify = if rec.overrides.is_empty() {
        None
    } else {
        ctx.out.info(bold("fix 3/3 · verify"));
        let t = &tuned.tune;
        let rt = t
            .trials
            .iter()
            .find(|x| x.id == t.recommended_trial_id)
            .context("the recommended trial is missing")?;
        let mib = t.metric_unit == "mib_per_s";
        let value = if mib { rt.mib_per_s } else { rt.ops_per_s };
        let expect = vec![
            Expectation {
                metric: "throttled_request_ratio".into(),
                direction: Direction::AtMost,
                predicted: budget,
                lo: 0.0,
                hi: budget,
                unit: "ratio".into(),
            },
            Expectation {
                metric: if mib {
                    "summary.mib_per_s"
                } else {
                    "summary.ops_per_s"
                }
                .into(),
                direction: Direction::AtLeast,
                predicted: value,
                lo: value * 0.8,
                hi: value * 1.2,
                unit: if mib { "mib_per_s" } else { "ops_per_s" }.into(),
            },
        ];
        // Replays of the diagnosis as it ran (in memory: `--no-save` stores nothing to
        // replay from), under the same guard.
        let replayer = Replayer {
            base: profile.clone(),
            target_name: target_name.clone(),
            safety: &a.safety,
            no_save: a.output.no_save,
            guard: guard.clone(),
            cancel: cancel.clone(),
        };
        let v = verify_hypotheses(
            ctx,
            &diag,
            vec![Hypothesis {
                code: "recommendation".into(),
                description: format!("apply the tuned configuration: {}", t.headline),
                overrides: rec.overrides.clone(),
                expect,
            }],
            VerifyCfg {
                repeat: a.repeat,
                replayer: &replayer,
                calibrate: true,
                max_throttle_ratio: budget,
            },
        )
        .await?;
        rec.confirmed = v.findings.first().map(|f| f.outcome);
        Some(v)
    };
    // The menu of Pareto options over the tuner's trials, against the diagnosis.
    let options = {
        let t = &tuned.tune;
        let reference = crate::tune::Trial {
            id: usize::MAX,
            stage: "diagnosis".into(),
            ops_per_s: crate::report::diff::good_ops_per_s(&diag.report),
            mib_per_s: diag.report.summary.mib_per_s,
            p99_ms: diag.report.verbs.iter().map(|v| v.latency.p99).fold(0.0, f64::max),
            error_rate: diag.report.summary.error_rate,
            throttled_request_ratio: crate::report::diff::throttled_request_ratio(&diag.report),
            throttled_responses: diag.report.resilience.throttled_responses,
            requests: diag.report.connections.requests,
            cost_per_million_ops: diag.report.cost.selected.per_million_ops_usd,
            feasible: crate::report::diff::throttled_request_ratio(&diag.report) <= budget
                && diag.report.summary.error_rate <= diag.report.sla.max_error_ratio,
            ..Default::default()
        };
        let from = diag.report.spec.profile.knobs.clone();
        let diag_rate = diag.report.spec.profile.workload.rate;
        let ov = move |tr: &crate::tune::Trial| {
            let mut o = crate::tune::recommend::knob_overrides(&from, &tr.knobs);
            if tr.rate_per_s != diag_rate
                && let Some(r) = tr.rate_per_s
            {
                let _ = o.set("rate", &format!("{r:.0}"));
            }
            o
        };
        let trials: Vec<crate::tune::Trial> = t.trials.iter().map(|x| x.to_trial()).collect();
        let mut m = crate::analysis::menu::menu(&crate::analysis::menu::MenuInputs {
            trials: &trials,
            reference: &reference,
            use_bytes: t.metric_unit == "mib_per_s",
            max_throttle_ratio: budget,
            monthly_ops: diag
                .report
                .cost
                .applicable
                .then_some(profile.cost.monthly_ops),
            overrides_of: &ov,
        });
        // The verified option carries the outcome.
        if let Some(outcome) = rec.confirmed {
            for o in &mut m {
                if o.overrides == rec.overrides {
                    o.verify = outcome.into();
                }
            }
        }
        m
    };
    let mut wrote = Vec::new();
    if let Some(p) = &a.write_profile {
        write_file(p, &rec.profile_toml)?;
        wrote.push(("profile".to_owned(), p.display().to_string()));
    }
    let rec_overrides = rec.overrides.clone();
    Ok(FixOutput {
        diagnosis_run_id: diag.id().to_owned(),
        finding_codes: diag
            .report
            .findings
            .iter()
            .map(|f| f.code.clone())
            .collect(),
        tune_run_id: tuned.run_id.clone(),
        profile_patch: knobs_patch(&diag.report.spec.profile.knobs, &winner),
        recommendation: rec,
        options,
        verify,
        wrote,
        unsaved_replay: a
            .output
            .no_save
            .then(|| diag.report.spec.with(&rec_overrides).command()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_lists_only_changed_knobs() {
        let a = Knobs::default();
        let mut b = a.clone();
        b.concurrency = 5;
        let p = knobs_patch(&a, &b);
        assert!(p.contains("[knobs]"), "{p}");
        assert!(p.contains("concurrency = 5"), "{p}");
        assert!(!p.contains("io_concurrency"), "{p}");
        assert_eq!(knobs_patch(&a, &a), "");
    }
}
