//! `oprobe prove RUN [FINDING] [--with PATH=VALUE]…`: A/B-test a change of a run's operator and
//! render the proven layer diff.
//!
//! **Temporary implementation (6.0 phase F).** The change is the finding's remedy (its
//! verification overrides) or `--with`; it is tested with the 5.x verify machinery: interleaved
//! baseline / candidate replays of the run with a sequential test (SPRT) against the learned
//! noise floor ([`super::verify::verify_hypotheses`]). The verdict is read from the objective's
//! run-level statistic (goodput: `ops_per_s`, p99: `p99_ms`). The sizing search, the
//! Wilson/beta intervals, `--max-throttle` as a constraint and child runs hidden from
//! `show --list` are phase A's (design v6 §8 "Prove").

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Ctx;
use super::input::Safety;
use super::verify::{Hypothesis, Replayer, VerifyCfg, default_expectations, hypotheses};
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::model::Overrides;
use crate::operator::OperatorDiff;
use crate::output::{CommandOutput, NextStep};
use crate::report::style::{bold, dim, green, red, yellow};

/// What prove judges a change on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProveObjective {
    /// Successful operations per second.
    #[default]
    Goodput,
    /// The worst per-operation p99 latency.
    P99,
}

impl ProveObjective {
    fn metric(self) -> &'static str {
        match self {
            Self::Goodput => "ops_per_s",
            Self::P99 => "p99_ms",
        }
    }

    fn higher_is_better(self) -> bool {
        self == Self::Goodput
    }
}

/// `oprobe prove`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProveRequest {
    /// The run whose operator to change (RUNREF)
    pub run: String,
    /// The finding whose remedy to prove (default: the first finding with a remedy)
    pub finding: Option<String>,
    /// The change to prove instead of the finding's remedy: `PATH=VALUE` pairs
    pub with: Vec<String>,
    pub objective: ProveObjective,
    /// Share of throttled requests a change may cause
    pub max_throttle_ratio: Option<f64>,
    /// Wall-time budget of the whole proof, e.g. 5m
    pub budget: Option<String>,
    /// Baseline / candidate pairs at most (2–8)
    pub pairs: usize,
    pub safety: Safety,
}

impl Default for ProveRequest {
    fn default() -> Self {
        Self {
            run: "last".into(),
            finding: None,
            with: Vec::new(),
            objective: ProveObjective::Goodput,
            max_throttle_ratio: None,
            budget: None,
            pairs: 4,
            safety: Safety::default(),
        }
    }
}

/// The verdict on the objective.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProveVerdict {
    Improved,
    Worse,
    Inconclusive,
}

/// Result of `oprobe prove`: the proven layer diff with its evidence.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ProveOutput {
    /// The run whose operator was changed.
    pub run_id: String,
    /// The run's operator as given.
    pub operator_ref: String,
    /// The finding whose remedy was proven (absent for `--with` alone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
    /// The change, as `--set` pairs.
    pub change: Overrides,
    pub objective: ProveObjective,
    pub verdict: ProveVerdict,
    /// Relative change of the objective (candidate vs baseline), with its 95 % interval.
    pub objective_delta: crate::analysis::stats::Interval,
    /// The change exceeded the noise floor and its interval excludes zero.
    pub significant: bool,
    /// Interleaved baseline / candidate pairs that ran.
    pub pairs_count: u64,
    pub baseline_run_ids: Vec<String>,
    pub candidate_run_ids: Vec<String>,
    /// The other metrics (shown, never decisive).
    pub secondary: Vec<crate::analysis::compare::MetricStat>,
    /// The operator change as Rust and TOML.
    pub diff: OperatorDiff,
}

impl CommandOutput for ProveOutput {
    fn text(&self) -> String {
        let label = match self.objective {
            ProveObjective::Goodput => "goodput",
            ProveObjective::P99 => "p99",
        };
        let d = &self.objective_delta;
        let verdict = match self.verdict {
            ProveVerdict::Improved => green("proved"),
            ProveVerdict::Worse => red("refuted"),
            ProveVerdict::Inconclusive => yellow("inconclusive"),
        };
        let mut o = format!(
            "{} on {label}: {:+.1} % [{:+.1}, {:+.1}] ({} A/B pairs, run {})\n",
            bold(&verdict),
            d.estimate * 100.0,
            d.low * 100.0,
            d.high * 100.0,
            self.pairs_count,
            self.run_id
        );
        for s in &self.secondary {
            o += &dim(&format!(
                "  {} {:+.1} %\n",
                s.metric,
                s.change.estimate * 100.0
            ));
        }
        o += &self.diff.rust;
        if !self.diff.toml.is_empty() {
            o += &format!("{}\n{}", dim("op.toml:"), self.diff.toml);
        }
        o
    }

    fn exit_code(&self) -> u8 {
        if self.verdict == ProveVerdict::Worse {
            exit::GATE
        } else {
            exit::OK
        }
    }

    fn next(&self) -> Vec<NextStep> {
        vec![NextStep::new(
            "profile the changed operator",
            format!(
                "oprobe run {} {}",
                crate::model::spec::shell_quote(&self.operator_ref),
                self.change.to_set_args().join(" ")
            ),
        )]
    }
}

/// What [`prove`] produced.
#[derive(Clone, Debug)]
pub enum ProveOutcome {
    /// `safety.dry_run`: the plan of one replay (every replay runs the same workload).
    Plan(Box<super::run::PlanOutput>),
    Proved(Box<ProveOutput>),
}

/// `oprobe prove`.
pub async fn prove(ctx: &Ctx, a: ProveRequest, cancel: Cancel) -> Result<ProveOutcome> {
    let run = ctx.store().load(&a.run)?;
    let budget = a
        .max_throttle_ratio
        .or(run.report.spec.profile.sla.max_throttle_rate)
        .unwrap_or(crate::config::defaults().tune.max_throttle_ratio);
    let hyp = if a.with.is_empty() {
        let mut hs = hypotheses(&run, a.finding.as_deref())?;
        hs.remove(0)
    } else {
        let overrides = Overrides::parse(&a.with)?;
        let code = a.finding.clone().unwrap_or_else(|| "change".into());
        Hypothesis {
            description: format!("the change {}", overrides.summary()),
            expect: default_expectations(&code, budget).0,
            code,
            overrides,
        }
    };
    if hyp.overrides.is_empty() {
        return Err(OprobeError::new(ErrorCode::UsageInvalidArgument, "nothing to prove")
            .with_hint("pass --with PATH=VALUE")
            .into());
    }
    let base = run.report.spec.profile.operator.clone();
    let mut changed = run.report.spec.resolved_profile();
    let mut label = None;
    hyp.overrides.apply(&mut changed, &mut label)?;
    let diff = crate::operator::diff::diff(&base, &changed.operator);
    if a.safety.dry_run {
        let profile = super::profile::spec_profile(ctx, run.report.spec.clone())?;
        let rails = super::run::rails(ctx, &profile, &a.safety)?;
        let mut plan = rails.plan(&profile);
        let runs = 2 * a.pairs.clamp(2, 8);
        plan.assumptions.push(format!(
            "up to {runs} replays of this plan: interleaved baseline and candidate runs of {}",
            hyp.overrides.summary()
        ));
        return Ok(ProveOutcome::Plan(Box::new(super::run::PlanOutput { plan })));
    }
    let mut safety = a.safety.clone();
    if safety.max_duration.is_none() {
        safety.max_duration = a.budget.clone();
    }
    let replayer = Replayer::of_run(ctx, &run, &safety, false, cancel)?;
    let res = super::verify::verify_hypotheses(
        ctx,
        &run,
        vec![hyp.clone()],
        VerifyCfg {
            repeat: a.pairs,
            replayer: &replayer,
            calibrate: true,
            max_throttle_ratio: budget,
        },
    )
    .await?;
    let f = res
        .findings
        .first()
        .ok_or_else(|| OprobeError::new(ErrorCode::Internal, "prove judged nothing"))?;
    let metric = a.objective.metric();
    let stat = f.comparison.stats.iter().find(|s| s.metric == metric);
    let (delta, significant) = match stat {
        Some(s) => (s.change, s.significant),
        None => (crate::analysis::stats::Interval::default(), false),
    };
    let better = if a.objective.higher_is_better() {
        delta.estimate > 0.0
    } else {
        delta.estimate < 0.0
    };
    let verdict = match (significant, better) {
        (true, true) => ProveVerdict::Improved,
        (true, false) => ProveVerdict::Worse,
        (false, _) => ProveVerdict::Inconclusive,
    };
    Ok(ProveOutcome::Proved(Box::new(ProveOutput {
        run_id: run.id().to_owned(),
        operator_ref: run.report.spec.operator_ref.clone(),
        finding: (!a.with.is_empty() || a.finding.is_some() || hyp.code != "change")
            .then(|| hyp.code.clone())
            .filter(|c| c != "change"),
        change: hyp.overrides.clone(),
        objective: a.objective,
        verdict,
        objective_delta: delta,
        significant,
        pairs_count: f.pairs_count,
        baseline_run_ids: res.baseline_run_ids.clone(),
        candidate_run_ids: f.candidate_run_ids.clone(),
        secondary: f
            .comparison
            .stats
            .iter()
            .filter(|s| s.metric != metric)
            .cloned()
            .collect(),
        diff,
    })))
}
