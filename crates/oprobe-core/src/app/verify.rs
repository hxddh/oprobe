//! `oprobe verify RUNREF [CODE|--all] [--repeat N]` (design v4 §5): recommendations are
//! hypotheses. The run's spec is replayed with and without each finding's change, interleaved
//! (A B A B…), the two sides are compared, and each finding is answered `confirmed`,
//! `refuted` or `inconclusive` against its expectations, with numbers. Consecutive identical
//! runs (A/A pairs) teach the target's noise floor (`<data>/noise.toml`), and the outcomes
//! calibrate the capacity history (`capacity::calibrate`).

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::*;
use crate::analysis::capacity::{self, Observation};
use crate::analysis::compare::{Comparison, compare_with_noise, runs_t, significant};
use crate::analysis::stats::Interval;
use crate::analysis::verdict::{NetVerdict, Sprt, SprtDecision};
use crate::analysis::verification::{Direction, Expectation, VerifyOutcome};
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError};
use crate::model::spec::{Overrides, shell_quote};
use crate::model::{Report, Run};
use crate::output::{CommandOutput, NextStep};
use crate::report::diff::{Thresholds, good_ops_per_s, throttled_request_ratio};
use crate::report::style::{bold, cyan, dim, green, red, yellow};
use crate::store::baselines::{NoiseFloors, noise_file, noise_ratio};

/// One expectation, measured.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ExpectationCheck {
    pub metric: String,
    pub direction: Direction,
    /// Unit of `predicted`, `lo`, `hi` (`baseline_ratio`: a multiple of the baseline's value).
    pub unit: String,
    pub predicted: f64,
    pub lo: f64,
    pub hi: f64,
    /// The metric over the baseline runs (mean with a 95 % t-interval).
    pub baseline: Interval,
    /// The metric over the candidate runs.
    pub candidate: Interval,
    /// Relative change of the candidate (ratio).
    pub change: Interval,
    pub outcome: VerifyOutcome,
    /// The judgement in words, with numbers.
    pub note: String,
}

/// What verify concluded about one finding.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FindingVerdict {
    pub code: String,
    /// The hypothesis in words.
    pub description: String,
    /// The change that was tested.
    pub overrides: Overrides,
    pub outcome: VerifyOutcome,
    /// Why, in one sentence with numbers.
    pub reason: String,
    pub expectations: Vec<ExpectationCheck>,
    /// The finding no longer shows in most candidate runs.
    pub resolved: bool,
    pub candidate_run_ids: Vec<String>,
    /// Baseline runs vs candidate runs (run-level statistics).
    pub comparison: Comparison,
    /// The two-sided net verdict (design v5 §4): improved, regressed, traded, no change or
    /// inconclusive, with what got better and what got worse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<crate::analysis::verdict::NetAssessment>,
    /// Interleaved baseline / candidate pairs measured for this finding.
    #[serde(default)]
    pub pairs_count: u64,
    /// The sequential test decided before `--repeat` pairs ran.
    #[serde(default)]
    pub stopped_early: bool,
}

/// Result of `oprobe verify`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct VerifyResult {
    /// The verified run.
    pub run_id: String,
    /// Baseline / candidate pairs per finding (the most any finding needed: the sequential test
    /// stops a finding once the evidence is in, between 2 and 8 pairs).
    pub repeat_count: u64,
    /// The replays of the run without changes.
    pub baseline_run_ids: Vec<String>,
    pub findings: Vec<FindingVerdict>,
    /// A/A pairs added to the noise floor of the target and workload.
    pub noise_pairs_count: u64,
    /// The outcomes were stored and calibrated the capacity history.
    pub calibrated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

fn outcome_text(o: VerifyOutcome) -> String {
    match o {
        VerifyOutcome::Confirmed => green("confirmed"),
        VerifyOutcome::Refuted => red("refuted"),
        VerifyOutcome::Inconclusive => yellow("inconclusive"),
    }
}

impl CommandOutput for VerifyResult {

    fn text(&self) -> String {
        let mut o = format!(
            "{} run {} · {} pair(s) per finding · baseline runs {}\n",
            bold("oprobe verify"),
            self.run_id,
            self.repeat_count,
            self.baseline_run_ids.join(", ")
        );
        for f in &self.findings {
            o += &format!(
                "\n{} {} — {}\n  {} {}\n  {}\n",
                bold(&cyan(&f.code)),
                outcome_text(f.outcome),
                f.reason,
                dim("change:"),
                f.overrides.summary(),
                dim(&f.description)
            );
            if let Some(n) = &f.net {
                o += &format!(
                    "  {} {}{}\n",
                    dim("net:"),
                    n.summary(),
                    dim(&format!(
                        " ({} pair(s){})",
                        f.pairs_count,
                        if f.stopped_early {
                            ", stopped early"
                        } else {
                            ""
                        }
                    ))
                );
            }
            for e in &f.expectations {
                o += &format!("    {} {}\n", outcome_text(e.outcome), e.note);
            }
            for s in &f.comparison.stats {
                o += &format!(
                    "    {} {} {:+.1}% [{:+.1}%, {:+.1}%]{}\n",
                    dim("·"),
                    s.metric,
                    s.change.estimate * 100.0,
                    s.change.low * 100.0,
                    s.change.high * 100.0,
                    if s.significant {
                        ""
                    } else {
                        " (not significant)"
                    }
                );
            }
        }
        if self.noise_pairs_count > 0 {
            o += &dim(&format!(
                "\nlearned the noise floor from {} A/A pair(s)\n",
                self.noise_pairs_count
            ));
        }
        o
    }
    fn warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }
    fn next(&self) -> Vec<NextStep> {
        let mut v = Vec::new();
        for f in &self.findings {
            if f.outcome == VerifyOutcome::Confirmed {
                v.push(NextStep::new(
                    format!("apply the confirmed change of `{}`", f.code),
                    crate::model::RunSpec::prove_command(&self.run_id, &f.overrides),
                ));
            }
        }
        if let (Some(b), Some(f)) = (self.baseline_run_ids.first(), self.findings.first())
            && let Some(c) = f.candidate_run_ids.first()
        {
            v.push(
                NextStep::new(
                    "compare a baseline replay with a candidate",
                    format!("oprobe compare {b} {c}"),
                ),
            );
        }
        v
    }
}

/// A change to test.
#[derive(Clone, Debug)]
pub struct Hypothesis {
    pub code: String,
    pub description: String,
    pub overrides: Overrides,
    pub expect: Vec<Expectation>,
}

/// How to replay and what to learn.
pub struct VerifyCfg<'a> {
    pub repeat: usize,
    pub replayer: &'a Replayer<'a>,
    pub calibrate: bool,
    /// The throttle budget the default throttling expectation uses.
    pub max_throttle_ratio: f64,
}

/// Replays of one run (`run --from RUN --set …`, in process): the run's resolved profile and
/// one guard for the whole command, so `--max-requests`, `--max-bytes` and
/// `--max-duration` cap every replay together (as with `run --repeat`).
pub struct Replayer<'a> {
    /// The profile the run measured, its target resolved (credentials included).
    pub base: crate::config::Profile,
    pub target_name: Option<String>,
    /// The replayed run's operator as given (the replays' spec keeps it).
    pub operator_ref: Option<String>,
    pub safety: &'a Safety,
    pub no_save: bool,
    /// The command's guard (none without budgets).
    pub guard: Option<std::sync::Arc<crate::guard::Guard>>,
    /// Cancels every replay of the command.
    pub cancel: Cancel,
}

impl<'a> Replayer<'a> {
    /// Replays of a stored run: its spec, the target resolved again (as `run --from RUN`).
    pub fn of_run(
        ctx: &Ctx,
        run: &Run,
        safety: &'a Safety,
        no_save: bool,
        cancel: Cancel,
    ) -> Result<Self> {
        let base = super::profile::spec_profile(ctx, run.report.spec.clone())?;
        let target_name =
            crate::operator::resolve::resolve(&run.report.spec.operator_ref, ctx.config()?)
                .ok()
                .and_then(|(_, s)| s.name());
        let mut r = Self::new(ctx, base, target_name, safety, no_save, cancel)?;
        r.operator_ref = Some(run.report.spec.operator_ref.clone());
        Ok(r)
    }

    /// Replays of `base` (a profile in memory, e.g. the diagnosis of `fix --no-save`) under a
    /// new command guard.
    pub fn new(
        ctx: &Ctx,
        base: crate::config::Profile,
        target_name: Option<String>,
        safety: &'a Safety,
        no_save: bool,
        cancel: Cancel,
    ) -> Result<Self> {
        let guard = super::run::guard_for(ctx, &base, &super::run::budgets(ctx, safety)?);
        Ok(Self {
            base,
            target_name,
            operator_ref: None,
            safety,
            no_save,
            guard,
            cancel,
        })
    }

    /// Replay with `o` applied, labelled `label`, and return the measured run.
    pub async fn replay(&self, ctx: &Ctx, o: &Overrides, label: &str) -> Result<Run> {
        let mut p = self.base.clone();
        let mut name = Some(label.to_owned());
        o.apply(&mut p, &mut name)?;
        p.name = name;
        p.validate()?;
        let output = OutputOptions {
            no_save: self.no_save,
            ..OutputOptions::default()
        };
        let mut safety = self.safety.clone();
        safety.dry_run = false;
        match super::run::measure_guarded(
            ctx,
            p,
            self.target_name.clone(),
            self.operator_ref.clone(),
            &safety,
            &output,
            self.guard.clone(),
            &self.cancel,
        )
        .await?
        {
            super::run::Measured::Run(o) => o.full.map(|r| *r).context("the replay kept no run"),
            super::run::Measured::Plan(_) => unreachable!("replays are never dry runs"),
        }
    }
}

/// The value of a report metric: the names of [`Expectation::metric`] (`summary.ops_per_s`,
/// `ops_per_s`, `p99_ms`, `throttled_request_ratio`, …), else a dotted path into the report's
/// JSON.
pub fn metric_value(r: &Report, metric: &str) -> Option<f64> {
    let m = metric.trim();
    let short = m
        .strip_prefix("summary.")
        .or_else(|| m.strip_prefix("resilience."))
        .unwrap_or(m);
    let v = match short {
        "ops_per_s" | "goodput" | "goodput_ops_per_s" => good_ops_per_s(r),
        "attempted_ops_per_s" => r.summary.ops_per_s,
        "mib_per_s" | "goodput_mib_per_s" => r.summary.mib_per_s,
        "p50_ms" | "latency.p50" | "latency.p50_ms" => r.summary.latency.p50,
        "p99_ms" | "latency.p99" | "latency.p99_ms" => r.summary.latency.p99,
        "throttled_request_ratio" | "throttle_ratio" => throttled_request_ratio(r),
        "throttled_op_ratio" => {
            let thr: u64 = r.verbs.iter().map(|v| v.throttled).sum();
            crate::util::ratio(thr as f64, r.summary.ops as f64)
        }
        "error_ratio" => r.summary.error_rate,
        "requests_per_op" | "requests_per_op_mean" | "bytes.request_amplification" => {
            r.bytes.request_amplification
        }
        _ => {
            let mut v = serde_json::to_value(r).ok()?;
            for part in m.split('.') {
                v = match v {
                    serde_json::Value::Object(mut o) => o.remove(part)?,
                    serde_json::Value::Array(a) => a.into_iter().nth(part.parse().ok()?)?,
                    _ => return None,
                };
            }
            return v.as_f64();
        }
    };
    Some(v)
}

/// The noise-floor key of a metric.
fn noise_key(metric: &str) -> &'static str {
    let m = metric.rsplit('.').next().unwrap_or(metric);
    match m {
        "p50_ms" | "p50" => "p50_ms",
        "p99_ms" | "p99" => "p99_ms",
        "mib_per_s" => "mib_per_s",
        _ => "ops_per_s",
    }
}

fn fmt_unit(v: f64, metric: &str) -> String {
    if metric.ends_with("_ratio") {
        format!("{:.2}%", v * 100.0)
    } else if metric.ends_with("_ms") {
        crate::util::fmt_ms(v)
    } else {
        format!("{v:.1}")
    }
}

/// Judge one expectation over per-run values of the baseline (`a`) and candidate (`b`).
pub fn judge(e: &Expectation, a: &[f64], b: &[f64], floor: f64) -> ExpectationCheck {
    let (mut ia, mut ib, mut ch) = runs_t(a, b);
    // Positive metrics: a log-ratio interval (never below −100 %); ratios stay in [0, 1]
    // (design v5 §4: no impossible intervals).
    if a.iter().chain(b).all(|v| *v > 0.0) && a.len() == b.len() {
        ch = crate::analysis::verdict::log_ratio(a, b, true);
    } else if a.iter().chain(b).all(|v| *v > 0.0) {
        ch = crate::analysis::verdict::log_ratio(a, b, false);
    }
    if e.metric.ends_with("_ratio") {
        for i in [&mut ia, &mut ib] {
            i.low = i.low.max(0.0);
            i.high = i.high.min(1.0);
        }
    }
    let rel = e.unit == "baseline_ratio";
    let scale = |v: f64| if rel { v * ia.estimate } else { v };
    let (pred, lo, hi) = (scale(e.predicted), scale(e.lo), scale(e.hi));
    let sig = significant(&ch, floor);
    // A predicted interval the measured one misses entirely refutes the prediction, whatever
    // the direction (A2: "predicted 1474 [1122, 1826], measured 866" is not a confirmation).
    let predicted_interval = !rel && e.hi > e.lo && e.hi > 0.0;
    let missed = predicted_interval
        && matches!(e.direction, Direction::Increase | Direction::Decrease)
        && (ib.high < lo || ib.low > hi);
    let (outcome, rule) = match e.direction {
        _ if missed => (
            VerifyOutcome::Refuted,
            format!(
                "missed the predicted [{}, {}]",
                fmt_unit(lo, &e.metric),
                fmt_unit(hi, &e.metric)
            ),
        ),
        Direction::Increase => {
            if sig && ch.estimate > 0.0 {
                (VerifyOutcome::Confirmed, "rose beyond the noise".to_owned())
            } else if (sig && ch.estimate <= 0.0) || ch.high < floor {
                (VerifyOutcome::Refuted, "did not rise".into())
            } else {
                (VerifyOutcome::Inconclusive, "within the noise".into())
            }
        }
        Direction::Decrease => {
            if sig && ch.estimate < 0.0 {
                (VerifyOutcome::Confirmed, "fell beyond the noise".to_owned())
            } else if (sig && ch.estimate >= 0.0) || ch.low > -floor {
                (VerifyOutcome::Refuted, "did not fall".into())
            } else {
                (VerifyOutcome::Inconclusive, "within the noise".into())
            }
        }
        Direction::AtMost => {
            let lim = pred.max(hi);
            if ib.estimate <= lim && (ib.high <= lim || b.iter().all(|v| *v <= lim)) {
                (
                    VerifyOutcome::Confirmed,
                    format!("≤ {} expected", fmt_unit(lim, &e.metric)),
                )
            } else if ib.low > lim || b.iter().all(|v| *v > lim) {
                (
                    VerifyOutcome::Refuted,
                    format!("above the expected ≤ {}", fmt_unit(lim, &e.metric)),
                )
            } else {
                (
                    VerifyOutcome::Inconclusive,
                    format!("around the expected ≤ {}", fmt_unit(lim, &e.metric)),
                )
            }
        }
        Direction::AtLeast => {
            let lim = pred.min(lo);
            if ib.estimate >= lim && (ib.low >= lim || b.iter().all(|v| *v >= lim)) {
                (
                    VerifyOutcome::Confirmed,
                    format!("≥ {} expected", fmt_unit(lim, &e.metric)),
                )
            } else if ib.high < lim || b.iter().all(|v| *v < lim) {
                (
                    VerifyOutcome::Refuted,
                    format!("below the expected ≥ {}", fmt_unit(lim, &e.metric)),
                )
            } else {
                (
                    VerifyOutcome::Inconclusive,
                    format!("around the expected ≥ {}", fmt_unit(lim, &e.metric)),
                )
            }
        }
        Direction::Unchanged => {
            if ch.low >= -floor && ch.high <= floor {
                (
                    VerifyOutcome::Confirmed,
                    "unchanged within the noise".to_owned(),
                )
            } else if sig {
                (VerifyOutcome::Refuted, "changed beyond the noise".into())
            } else {
                (VerifyOutcome::Inconclusive, "too noisy to tell".into())
            }
        }
    };
    let note = format!(
        "{} {} → {} ({:+.1}% [{:+.1}%, {:+.1}%], noise ±{:.0}%): {rule}",
        e.metric,
        fmt_unit(ia.estimate, &e.metric),
        fmt_unit(ib.estimate, &e.metric),
        ch.estimate * 100.0,
        ch.low * 100.0,
        ch.high * 100.0,
        floor * 100.0
    );
    ExpectationCheck {
        metric: e.metric.clone(),
        direction: e.direction,
        unit: e.unit.clone(),
        predicted: e.predicted,
        lo: e.lo,
        hi: e.hi,
        baseline: ia,
        candidate: ib,
        change: ch,
        outcome,
        note,
    }
}

/// Expectations of a finding that names none: throttling must stay within the budget at
/// ≥ 70 % of the goodput; anything else must not cost goodput (and should resolve the
/// finding). Returns `(expectations, the finding must resolve)`.
pub fn default_expectations(code: &str, budget: f64) -> (Vec<Expectation>, bool) {
    let goodput = |min: f64| Expectation {
        metric: "summary.ops_per_s".into(),
        direction: Direction::AtLeast,
        predicted: min,
        lo: min,
        hi: 1.0,
        unit: "baseline_ratio".into(),
    };
    match code {
        "throttling" => (
            vec![
                Expectation {
                    metric: "throttled_request_ratio".into(),
                    direction: Direction::AtMost,
                    predicted: budget,
                    lo: 0.0,
                    hi: budget,
                    unit: "ratio".into(),
                },
                goodput(0.7),
            ],
            false,
        ),
        _ => (vec![goodput(0.95)], true),
    }
}

/// The hypotheses of `run`: one finding (`code`) or every finding with a verification.
pub fn hypotheses(run: &Run, code: Option<&str>) -> Result<Vec<Hypothesis>> {
    let with: Vec<&crate::model::Finding> = run
        .report
        .findings
        .iter()
        .filter(|f| {
            f.verification
                .as_ref()
                .is_some_and(|v| !v.overrides.is_empty())
        })
        .collect();
    let pick: Vec<&crate::model::Finding> = match code {
        Some(c) => {
            let Some(f) = run.report.findings.iter().find(|f| f.code == c) else {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("run {} has no finding `{c}`", run.id()),
                )
                .with_hint(format!(
                    "its findings: {}",
                    run.report
                        .findings
                        .iter()
                        .map(|f| f.code.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .into());
            };
            if !with.iter().any(|w| w.code == f.code) {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!(
                        "finding `{c}` of run {} names no change to verify",
                        run.id()
                    ),
                )
                .with_hint(format!(
                    "verifiable findings: {}",
                    with.iter()
                        .map(|f| f.code.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .into());
            }
            vec![f]
        }
        None => with,
    };
    if pick.is_empty() {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("run {} has no finding with a change to verify", run.id()),
        )
        .with_hint("`oprobe explain RUN` lists its findings; `oprobe tune` searches for a better configuration")
        .into());
    }
    Ok(pick
        .into_iter()
        .map(|f| {
            let v = f.verification.as_ref().expect("filtered");
            Hypothesis {
                code: f.code.clone(),
                description: v.description.clone(),
                overrides: v.overrides.clone(),
                expect: v.expect.clone(),
            }
        })
        .collect())
}

fn has_code(r: &Run, code: &str) -> bool {
    r.report.findings.iter().any(|f| f.code == code)
}

/// Replay `run` interleaved (A B₁ B₂ … A B₁ B₂ …), judge every hypothesis and learn from it.
pub async fn verify_hypotheses(
    ctx: &Ctx,
    run: &Run,
    hyps: Vec<Hypothesis>,
    cfg: VerifyCfg<'_>,
) -> Result<VerifyResult> {
    let id = run.id().to_owned();
    // The noise floor known so far sets the sequential test's minimum effect.
    let nfile = noise_file(&ctx.paths);
    let mut floors = NoiseFloors::load(&nfile).unwrap_or_default();
    let prior_noise = floors.of(&run.report).cloned();
    let delta = noise_ratio(prior_noise.as_ref(), "ops_per_s").max(0.05);
    // Sequential: at least 2 pairs, at most `--repeat` (2–8); a finding stops once its SPRT
    // decides (design v5 §4, Verify 5).
    let max_pairs = cfg.repeat.clamp(2, 8);
    let min_pairs = 2;
    let mut a_runs: Vec<Run> = Vec::new();
    // Per hypothesis: (pair index, candidate run).
    let mut b_pairs: Vec<Vec<(usize, Run)>> = vec![Vec::new(); hyps.len()];
    let mut sprts: Vec<Sprt> = hyps.iter().map(|_| Sprt::new(delta, 0.05, 0.10)).collect();
    let mut decided = vec![false; hyps.len()];
    let mut n = 0;
    for i in 0..max_pairs {
        if i >= min_pairs && decided.iter().all(|d| *d) {
            break;
        }
        let open = decided.iter().filter(|d| !**d).count();
        n += 1;
        ctx.out.info(dim(&format!(
            "verify {id}: run {n} — baseline replay (pair {}/≤{max_pairs}, {open} finding(s) undecided)",
            i + 1,
        )));
        a_runs.push(
            cfg.replayer
                .replay(ctx, &Overrides::new(), &format!("verify-{id}-baseline"))
                .await?,
        );
        let base_goodput = good_ops_per_s(&a_runs[i].report);
        for (k, h) in hyps.iter().enumerate() {
            if decided[k] && i >= min_pairs {
                continue;
            }
            n += 1;
            ctx.out.info(dim(&format!(
                "verify {id}: run {n} — `{}` with {}",
                h.code,
                h.overrides.summary()
            )));
            let r = cfg
                .replayer
                .replay(ctx, &h.overrides, &format!("verify-{id}-{}", h.code))
                .await?;
            sprts[k].push(base_goodput, good_ops_per_s(&r.report));
            b_pairs[k].push((i, r));
            if sprts[k].decision() != SprtDecision::Continue {
                decided[k] = true;
            }
        }
    }
    let b_runs: Vec<Vec<Run>> = b_pairs
        .iter()
        .map(|v| v.iter().map(|(_, r)| r.clone()).collect())
        .collect();
    // Noise: consecutive identical runs are A/A pairs.
    let mut pairs = 0u64;
    let mut warnings = Vec::new();
    if cfg.calibrate {
        for w in a_runs.windows(2) {
            floors.add_pair(&w[0].report, &w[1].report);
            pairs += 1;
        }
        for side in &b_runs {
            for w in side.windows(2) {
                floors.add_pair(&w[0].report, &w[1].report);
                pairs += 1;
            }
        }
        if pairs > 0
            && let Err(e) = floors.save(&nfile)
        {
            warnings.push(format!("cannot save the noise floor: {e:#}"));
        }
    }
    let noise = floors.of(&a_runs[0].report).cloned();
    let history = ctx.paths.capacity_file();
    let mut calibrated = false;
    let mut findings = Vec::new();
    for (k, (h, bs)) in hyps.iter().zip(b_runs.iter()).enumerate() {
        let t = Thresholds::default();
        // The baselines this finding was paired with.
        let a_mine: Vec<Run> = b_pairs[k].iter().map(|(i, _)| a_runs[*i].clone()).collect();
        let a_runs = &a_mine;
        let comparison = compare_with_noise(
            a_runs,
            bs,
            &format!("{id} replayed"),
            &format!("{id} with {}", h.overrides.summary()),
            &t,
            noise.as_ref(),
        );
        let (mut expect, must_resolve) = default_expectations(&h.code, cfg.max_throttle_ratio);
        if !h.expect.is_empty() {
            expect = h.expect.clone();
        }
        let resolved = bs.iter().filter(|r| !has_code(r, &h.code)).count() * 2 > bs.len();
        let mut checks = Vec::new();
        let mut missing = Vec::new();
        for e in &expect {
            let a: Option<Vec<f64>> = a_runs
                .iter()
                .map(|r| metric_value(&r.report, &e.metric))
                .collect();
            let b: Option<Vec<f64>> = bs
                .iter()
                .map(|r| metric_value(&r.report, &e.metric))
                .collect();
            match (a, b) {
                (Some(a), Some(b)) => checks.push(judge(
                    e,
                    &a,
                    &b,
                    noise_ratio(noise.as_ref(), noise_key(&e.metric)),
                )),
                _ => missing.push(e.metric.clone()),
            }
        }
        // One net verdict for verify and compare (design v5 §4): the comparison's, over the same
        // runs — then without the losses the finding predicted and its checks confirmed.
        let (net, tolerated) = tolerate_predicted(
            comparison
                .net
                .clone()
                .expect("a comparison carries its net verdict"),
            &checks,
        );
        let refuted: Vec<&ExpectationCheck> = checks
            .iter()
            .filter(|c| c.outcome == VerifyOutcome::Refuted)
            .collect();
        let all_confirmed =
            !checks.is_empty() && checks.iter().all(|c| c.outcome == VerifyOutcome::Confirmed);
        let (outcome, reason) = if let Some(r) = refuted.first() {
            (VerifyOutcome::Refuted, r.note.clone())
        } else if net.verdict == NetVerdict::Regressed {
            // Two-sided: a change that only made things worse is refuted whatever its
            // expectations said.
            (
                VerifyOutcome::Refuted,
                format!("the change made things worse: {}", net.summary()),
            )
        } else if all_confirmed && (!must_resolve || resolved) {
            (
                VerifyOutcome::Confirmed,
                checks
                    .iter()
                    .map(|c| c.note.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        } else if all_confirmed {
            (
                VerifyOutcome::Inconclusive,
                format!(
                    "the expectations held, but `{}` still shows in most candidate runs",
                    h.code
                ),
            )
        } else if checks.is_empty() {
            (
                VerifyOutcome::Inconclusive,
                format!(
                    "no expected metric could be measured ({})",
                    missing.join(", ")
                ),
            )
        } else {
            (
                VerifyOutcome::Inconclusive,
                checks
                    .iter()
                    .filter(|c| c.outcome == VerifyOutcome::Inconclusive)
                    .map(|c| c.note.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        };
        // Store the outcome and calibrate the capacity history.
        if cfg.calibrate
            && let Some(target) = run.report.spec.target_id()
        {
            let wh = capacity::history_key(&run.report.spec);
            let base = capacity::load(&history, &target, &wh)
                .unwrap_or_else(|| run.report.capacity.clone());
            let c = h
                .overrides
                .workload
                .get("concurrency")
                .and_then(|v| v.parse::<u32>().ok())
                .or_else(|| {
                    bs.first()
                        .map(|r| r.report.knobs.requested.concurrency_count as u32)
                })
                .unwrap_or(1);
            let (thr, req) = bs.iter().fold((0u64, 0u64), |(x, y), r| {
                (
                    x + r.report.resilience.throttled_responses,
                    y + r.report.connections.requests,
                )
            });
            let obs = Observation {
                code: h.code.clone(),
                outcome,
                concurrency_count: u64::from(c),
                predicted_ops_per_s: base.predict(c).ops_per_s,
                measured_ops_per_s: crate::analysis::stats::mean(
                    &bs.iter()
                        .map(|r| good_ops_per_s(&r.report))
                        .collect::<Vec<_>>(),
                ),
                peak_inflight_count: bs
                    .iter()
                    .map(|r| r.report.capacity.observed.peak_inflight_count)
                    .max()
                    .unwrap_or(0),
                throttled_request_ratio: crate::util::ratio(thr as f64, req as f64),
                max_throttle_ratio: cfg.max_throttle_ratio,
                run_id: Some(id.clone()),
                at: crate::util::now_rfc3339(),
            };
            match capacity::calibrate(&history, &target, &wh, &base, &obs) {
                Ok(_) => calibrated = true,
                Err(e) => warnings.push(format!("cannot calibrate the capacity history: {e:#}")),
            }
        }
        findings.push(FindingVerdict {
            code: h.code.clone(),
            description: h.description.clone(),
            overrides: h.overrides.clone(),
            outcome,
            expectations: checks,
            resolved,
            candidate_run_ids: bs.iter().map(|r| r.id().to_owned()).collect(),
            comparison,
            pairs_count: bs.len() as u64,
            stopped_early: bs.len() < max_pairs,
            reason: {
                let reason =
                    if net.verdict == NetVerdict::Traded && outcome != VerifyOutcome::Refuted {
                        format!("{reason} — net {}", net.summary())
                    } else {
                        reason
                    };
                if tolerated.is_empty() {
                    reason
                } else {
                    let t: Vec<String> = tolerated
                        .iter()
                        .map(|m| format!("{} {:+.1} %", m.metric, m.delta_ratio * 100.0))
                        .collect();
                    format!("{reason} (within the prediction: {})", t.join(", "))
                }
            },
            net: Some(net),
        });
    }
    Ok(VerifyResult {
        run_id: id,
        repeat_count: a_runs.len() as u64,
        baseline_run_ids: a_runs.iter().map(|r| r.id().to_owned()).collect(),
        findings,
        noise_pairs_count: pairs,
        calibrated,
        warnings,
    })
}

/// The metric family of a report metric or net-verdict metric: goodput (`ops_per_s` and
/// `mib_per_s` move together in a replay of the same workload), p99, or the name itself.
fn metric_family(m: &str) -> &str {
    let m = m.strip_prefix("summary.").unwrap_or(m);
    match m {
        "ops_per_s" | "mib_per_s" => "goodput",
        "p99_ms" | "latency.p99" => "p99",
        other => other,
    }
}

/// `net` without the losses a confirmed expectation predicted: a finding that expects goodput
/// ≥ 90 % of the baseline accepts a 3 % loss, which must not make the change "worse" (CI: a
/// confirmed `mib_per_s ≥ 0.9×` and a −2.9 % `ops_per_s` refuted `download_bound`). A loss
/// beyond the predicted tolerance still counts. Returns the net and the tolerated moves.
fn tolerate_predicted(
    mut net: crate::analysis::verdict::NetAssessment,
    checks: &[ExpectationCheck],
) -> (
    crate::analysis::verdict::NetAssessment,
    Vec<crate::analysis::verdict::MetricMove>,
) {
    use crate::analysis::verdict::NetVerdict;
    let covers = |w: &crate::analysis::verdict::MetricMove| {
        checks.iter().any(|c| {
            c.outcome == VerifyOutcome::Confirmed
                && c.unit == "baseline_ratio"
                && metric_family(&c.metric) == metric_family(&w.metric)
                && match c.direction {
                    Direction::AtLeast => w.delta_ratio >= c.predicted.min(c.lo) - 1.0 - 1e-12,
                    Direction::AtMost => w.delta_ratio <= c.predicted.max(c.hi) - 1.0 + 1e-12,
                    _ => false,
                }
        })
    };
    let (tolerated, worse): (Vec<_>, Vec<_>) = net.worse.drain(..).partition(|w| covers(w));
    net.worse = worse;
    if !tolerated.is_empty() {
        net.verdict = match (net.better.is_empty(), net.worse.is_empty()) {
            (false, true) => NetVerdict::Improved,
            (true, false) => NetVerdict::Regressed,
            (false, false) => NetVerdict::Traded,
            // Only predicted losses: neither better nor worse than the finding said.
            (true, true) => NetVerdict::Inconclusive,
        };
    }
    (net, tolerated)
}

/// What [`verify`] produced.
#[derive(Clone, Debug)]
pub enum VerifyRun {
    /// `safety.dry_run`: the plan of every replay.
    Plan(Box<super::run::PlanOutput>),
    Verified(Box<VerifyResult>),
}

/// `oprobe verify`: replay a run's findings interleaved with baselines and judge them.
pub async fn verify(ctx: &Ctx, a: VerifyRequest, cancel: Cancel) -> Result<VerifyRun> {
    let run = ctx.store().load(&a.run)?;
    let hyps = hypotheses(&run, if a.all { None } else { a.code.as_deref() })?;
    let budget = run
        .report
        .spec
        .profile
        .sla
        .max_throttle_rate
        .unwrap_or(crate::config::defaults().tune.max_throttle_ratio);
    if a.safety.dry_run {
        let profile = super::profile::spec_profile(ctx, run.report.spec.clone())?;
        let rails = super::run::rails(ctx, &profile, &a.safety)?;
        let mut plan = rails.plan(&profile);
        let runs = a.repeat * (hyps.len() + 1);
        plan.eta_s *= runs as f64;
        plan.estimated_ops_count *= runs as u64;
        plan.estimated_requests_count *= runs as u64;
        plan.estimated_bytes *= runs as u64;
        plan.assumptions.push(format!(
            "{runs} replays: {} baseline and {} candidate run(s) of {}",
            a.repeat,
            a.repeat * hyps.len(),
            hyps.iter()
                .map(|h| format!("`{}` ({})", h.code, h.overrides.summary()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return Ok(VerifyRun::Plan(Box::new(super::run::PlanOutput { plan })));
    }
    ctx.out.info(format!(
        "{} {} · {} · {} pair(s): {}",
        bold("oprobe verify"),
        run.id(),
        shell_quote(&run.report.spec.operator_ref),
        a.repeat,
        hyps.iter()
            .map(|h| format!("{} ({})", h.code, h.overrides.summary()))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let replayer = Replayer::of_run(ctx, &run, &a.safety, a.no_save, cancel)?;
    let res = verify_hypotheses(
        ctx,
        &run,
        hyps,
        VerifyCfg {
            repeat: a.repeat,
            replayer: &replayer,
            calibrate: !a.no_calibrate,
            max_throttle_ratio: budget,
        },
    )
    .await?;
    Ok(VerifyRun::Verified(Box::new(res)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(metric: &str, dir: Direction, min: f64, outcome: VerifyOutcome) -> ExpectationCheck {
        ExpectationCheck {
            metric: metric.into(),
            direction: dir,
            unit: "baseline_ratio".into(),
            predicted: min,
            lo: min,
            hi: 1.0,
            baseline: Interval::default(),
            candidate: Interval::default(),
            change: Interval::default(),
            outcome,
            note: String::new(),
        }
    }

    fn moved(metric: &str, d: f64) -> crate::analysis::verdict::MetricMove {
        crate::analysis::verdict::MetricMove {
            metric: metric.into(),
            delta_ratio: d,
        }
    }

    /// CI (Linux, download_bound under a shared bandwidth cap, concurrency 4, 2 pairs):
    /// `mib_per_s ≥ 0.9×` confirmed (17.1 → 16.6 MiB/s, −2.9 %) while ops/s −2.9 % was the only
    /// "worse" move: refuted for a loss inside its own prediction.
    #[test]
    fn a_loss_the_finding_predicted_does_not_refute_it() {
        use crate::analysis::verdict::{NetAssessment, NetVerdict};
        let keep = check(
            "summary.mib_per_s",
            Direction::AtLeast,
            0.9,
            VerifyOutcome::Confirmed,
        );
        let net = NetAssessment {
            verdict: NetVerdict::Regressed,
            better: vec![],
            worse: vec![moved("ops_per_s", -0.029)],
        };
        let (n, t) = tolerate_predicted(net, std::slice::from_ref(&keep));
        assert_ne!(n.verdict, NetVerdict::Regressed, "{n:?}");
        assert!(n.worse.is_empty() && t.len() == 1, "{n:?}");
        // With the p99 gain the comparison saw: improved.
        let net = NetAssessment {
            verdict: NetVerdict::Traded,
            better: vec![moved("p99_ms", -0.48)],
            worse: vec![moved("ops_per_s", -0.029)],
        };
        let (n, _) = tolerate_predicted(net, std::slice::from_ref(&keep));
        assert_eq!(n.verdict, NetVerdict::Improved, "{n:?}");
        // A genuine regression beyond the predicted tolerance still refutes.
        let net = NetAssessment {
            verdict: NetVerdict::Regressed,
            better: vec![],
            worse: vec![moved("ops_per_s", -0.15)],
        };
        let (n, t) = tolerate_predicted(net, std::slice::from_ref(&keep));
        assert_eq!(n.verdict, NetVerdict::Regressed, "{n:?}");
        assert!(t.is_empty());
        // An expectation that was not confirmed tolerates nothing, nor one on another metric.
        for c in [
            check(
                "summary.mib_per_s",
                Direction::AtLeast,
                0.9,
                VerifyOutcome::Inconclusive,
            ),
            check(
                "throttled_request_ratio",
                Direction::AtLeast,
                0.9,
                VerifyOutcome::Confirmed,
            ),
        ] {
            let net = NetAssessment {
                verdict: NetVerdict::Regressed,
                better: vec![],
                worse: vec![moved("ops_per_s", -0.029)],
            };
            let (n, _) = tolerate_predicted(net, std::slice::from_ref(&c));
            assert_eq!(n.verdict, NetVerdict::Regressed, "{c:?}");
        }
    }

    fn exp(metric: &str, direction: Direction, predicted: f64, unit: &str) -> Expectation {
        Expectation {
            metric: metric.into(),
            direction,
            predicted,
            lo: predicted,
            hi: predicted,
            unit: unit.into(),
        }
    }

    #[test]
    fn judges_each_direction() {
        let up = exp("ops_per_s", Direction::Increase, 0.0, "ops_per_s");
        let c = judge(&up, &[100.0, 101.0, 99.0], &[130.0, 131.0, 129.0], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Confirmed, "{}", c.note);
        let c = judge(&up, &[100.0, 101.0, 99.0], &[100.0, 101.0, 99.5], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Refuted, "{}", c.note);
        let c = judge(&up, &[100.0, 140.0, 60.0], &[110.0, 150.0, 70.0], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Inconclusive, "{}", c.note);

        let down = exp("p99_ms", Direction::Decrease, 0.0, "ms");
        let c = judge(&down, &[50.0, 51.0, 49.0], &[20.0, 21.0, 19.0], 0.1);
        assert_eq!(c.outcome, VerifyOutcome::Confirmed);

        let at_most = exp("throttled_request_ratio", Direction::AtMost, 0.001, "ratio");
        let c = judge(&at_most, &[0.05, 0.04], &[0.0, 0.0], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Confirmed, "{}", c.note);
        let c = judge(&at_most, &[0.05, 0.04], &[0.03, 0.04], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Refuted);

        let keep = exp("ops_per_s", Direction::AtLeast, 0.7, "baseline_ratio");
        let c = judge(&keep, &[100.0, 100.0], &[80.0, 82.0], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Confirmed, "{}", c.note);
        let c = judge(&keep, &[100.0, 100.0], &[40.0, 42.0], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Refuted);

        let same = exp("p50_ms", Direction::Unchanged, 0.0, "ms");
        let c = judge(&same, &[10.0, 10.1, 9.9], &[10.0, 10.05, 9.95], 0.05);
        assert_eq!(c.outcome, VerifyOutcome::Confirmed, "{}", c.note);
    }

    #[test]
    fn metric_names_resolve() {
        let r = crate::report::fixture::run().report;
        assert_eq!(
            metric_value(&r, "summary.ops_per_s"),
            Some(good_ops_per_s(&r))
        );
        assert_eq!(metric_value(&r, "p99_ms"), Some(r.summary.latency.p99));
        assert_eq!(
            metric_value(&r, "throttled_request_ratio"),
            Some(throttled_request_ratio(&r))
        );
        assert_eq!(
            metric_value(&r, "run.window_s"),
            Some(r.run.window_s),
            "dotted paths into the report"
        );
        assert!(metric_value(&r, "no.such.metric").is_none());
        let (e, resolve) = default_expectations("throttling", 0.001);
        assert_eq!(e.len(), 2);
        assert!(!resolve);
        assert!(default_expectations("small_objects", 0.001).1);
    }

}
