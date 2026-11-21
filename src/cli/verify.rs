//! `oprobe verify RUNREF [CODE|--all] [--repeat N]` (design v4 §5): recommendations are
//! hypotheses. The run's spec is replayed with and without each finding's change, interleaved
//! (A B A B…), the two sides are compared, and each finding is answered `confirmed`,
//! `refuted` or `inconclusive` against its expectations, with numbers. Consecutive identical
//! runs (A/A pairs) teach the target's noise floor (`<data>/noise.toml`), and the outcomes
//! calibrate the capacity history (`capacity::calibrate`).

use anyhow::{Context, Result};
use clap::Parser;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use crate::analysis::capacity::{self, Observation};
use crate::analysis::compare::{Comparison, compare_with_noise, runs_t, significant};
use crate::analysis::stats::Interval;
use crate::analysis::verification::{Direction, Expectation, VerifyOutcome};
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
}

/// Result of `oprobe verify`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct VerifyResult {
    /// The verified run.
    pub run_id: String,
    /// Baseline / candidate pairs per finding.
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
    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD
    }

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
                    crate::model::RunSpec::rerun_from(&self.run_id, &f.overrides),
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
                )
                .with_mcp(
                    "compare_runs",
                    serde_json::json!({ "baseline": b, "candidate": c }),
                ),
            );
        }
        v
    }
}

/// A change to test.
#[derive(Clone, Debug)]
pub(super) struct Hypothesis {
    pub code: String,
    pub description: String,
    pub overrides: Overrides,
    pub expect: Vec<Expectation>,
}

/// How to replay and what to learn.
pub(super) struct VerifyCfg<'a> {
    pub repeat: usize,
    pub safety: &'a SafetyArgs,
    pub no_save: bool,
    pub calibrate: bool,
    /// The throttle budget the default throttling expectation uses.
    pub max_throttle_ratio: f64,
}

/// The safety flags as `oprobe run` arguments.
fn safety_args(s: &SafetyArgs) -> Vec<String> {
    let mut a = Vec::new();
    if s.read_only {
        a.push("--read-only".to_owned());
    }
    if let Some(v) = s.max_cost_usd {
        a.extend(["--max-cost-usd".to_owned(), v.to_string()]);
    }
    if let Some(v) = s.max_requests {
        a.extend(["--max-requests".to_owned(), v.to_string()]);
    }
    if let Some(v) = &s.max_bytes {
        a.extend(["--max-bytes".to_owned(), v.clone()]);
    }
    if let Some(v) = &s.max_duration {
        a.extend(["--max-duration".to_owned(), v.clone()]);
    }
    a
}

/// `oprobe run --from RUN --set …` arguments of a replay.
pub(super) fn replay_args(
    run_id: &str,
    o: &Overrides,
    label: &str,
    safety: &SafetyArgs,
    no_save: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "oprobe".into(),
        "run".into(),
        "--from".into(),
        run_id.into(),
    ];
    args.extend(o.to_set_args());
    args.extend(["--label".to_owned(), label.to_owned()]);
    args.extend(safety_args(safety));
    if no_save {
        args.push("--no-save".into());
    }
    args
}

/// Parse `oprobe run …` arguments into the run command's flags.
pub(super) fn parse_run(args: &[String]) -> Result<RunArgs> {
    let cli = Cli::try_parse_from(args).map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("cannot build the replay `{}`: {e}", args.join(" ")),
        )
    })?;
    match cli.command {
        Some(Command::Run(a)) => Ok(a),
        _ => unreachable!("replay arguments are a run command"),
    }
}

/// Replay `run_id` with `o` applied and return the measured run.
pub(super) async fn replay(ctx: &Ctx, args: &[String]) -> Result<Run> {
    let ra = parse_run(args)?;
    let (profile, name) = super::run_profile(ctx, &ra)?;
    match super::run::measure(ctx, profile, name, &ra.safety, &ra.output).await? {
        super::run::Measured::Run(o) => o.full.map(|r| *r).context("the replay kept no run"),
        super::run::Measured::Plan(_) => unreachable!("replays are never dry runs"),
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
        "cost_per_million_ops_usd" => r.cost.selected.per_million_ops_usd,
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
    let (ia, ib, ch) = runs_t(a, b);
    let rel = e.unit == "baseline_ratio";
    let scale = |v: f64| if rel { v * ia.estimate } else { v };
    let (pred, lo, hi) = (scale(e.predicted), scale(e.lo), scale(e.hi));
    let sig = significant(&ch, floor);
    let (outcome, rule) = match e.direction {
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
pub(super) fn default_expectations(code: &str, budget: f64) -> (Vec<Expectation>, bool) {
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
pub(super) fn hypotheses(run: &Run, code: Option<&str>) -> Result<Vec<Hypothesis>> {
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
pub(super) async fn verify_hypotheses(
    ctx: &Ctx,
    run: &Run,
    hyps: Vec<Hypothesis>,
    cfg: VerifyCfg<'_>,
) -> Result<VerifyResult> {
    let id = run.id().to_owned();
    let mut a_runs: Vec<Run> = Vec::new();
    let mut b_runs: Vec<Vec<Run>> = vec![Vec::new(); hyps.len()];
    let total = cfg.repeat * (hyps.len() + 1);
    let mut n = 0;
    for i in 0..cfg.repeat {
        n += 1;
        ctx.out.info(dim(&format!(
            "verify {id}: run {n}/{total} — baseline replay (pair {}/{})",
            i + 1,
            cfg.repeat
        )));
        a_runs.push(
            replay(
                ctx,
                &replay_args(
                    &id,
                    &Overrides::new(),
                    &format!("verify-{id}-baseline"),
                    cfg.safety,
                    cfg.no_save,
                ),
            )
            .await?,
        );
        for (h, out) in hyps.iter().zip(b_runs.iter_mut()) {
            n += 1;
            ctx.out.info(dim(&format!(
                "verify {id}: run {n}/{total} — `{}` with {}",
                h.code,
                h.overrides.summary()
            )));
            out.push(
                replay(
                    ctx,
                    &replay_args(
                        &id,
                        &h.overrides,
                        &format!("verify-{id}-{}", h.code),
                        cfg.safety,
                        cfg.no_save,
                    ),
                )
                .await?,
            );
        }
    }
    // Noise: consecutive identical runs are A/A pairs.
    let nfile = noise_file(&ctx.paths);
    let mut floors = NoiseFloors::load(&nfile).unwrap_or_default();
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
    for (h, bs) in hyps.iter().zip(b_runs.iter()) {
        let t = Thresholds::default();
        let comparison = compare_with_noise(
            &a_runs,
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
        let refuted: Vec<&ExpectationCheck> = checks
            .iter()
            .filter(|c| c.outcome == VerifyOutcome::Refuted)
            .collect();
        let all_confirmed =
            !checks.is_empty() && checks.iter().all(|c| c.outcome == VerifyOutcome::Confirmed);
        let (outcome, reason) = if let Some(r) = refuted.first() {
            (VerifyOutcome::Refuted, r.note.clone())
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
                .knobs
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
            reason,
            expectations: checks,
            resolved,
            candidate_run_ids: bs.iter().map(|r| r.id().to_owned()).collect(),
            comparison,
        });
    }
    Ok(VerifyResult {
        run_id: id,
        repeat_count: cfg.repeat as u64,
        baseline_run_ids: a_runs.iter().map(|r| r.id().to_owned()).collect(),
        findings,
        noise_pairs_count: pairs,
        calibrated,
        warnings,
    })
}

pub async fn cmd_verify(a: VerifyArgs, ctx: &Ctx) -> Result<u8> {
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
        let ra = parse_run(&replay_args(
            run.id(),
            &Overrides::new(),
            "verify",
            &a.safety,
            true,
        ))?;
        let (profile, _) = super::run_profile(ctx, &ra)?;
        let rails = super::run::rails(ctx, &profile, &a.safety)?;
        let mut plan = rails.plan(&profile);
        let runs = a.repeat * (hyps.len() + 1);
        plan.eta_s *= runs as f64;
        plan.estimated_ops_count *= runs as u64;
        plan.estimated_requests_count *= runs as u64;
        plan.estimated_bytes *= runs as u64;
        plan.estimated_cost_usd = plan.estimated_cost_usd.map(|c| c * runs as f64);
        plan.assumptions.push(format!(
            "{runs} replays: {} baseline and {} candidate run(s) of {}",
            a.repeat,
            a.repeat * hyps.len(),
            hyps.iter()
                .map(|h| format!("`{}` ({})", h.code, h.overrides.summary()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return ctx.out.finish(&super::run::PlanOutput { plan });
    }
    ctx.out.info(format!(
        "{} {} · {} · {} pair(s): {}",
        bold("oprobe verify"),
        run.id(),
        shell_quote(&run.report.spec.target_ref),
        a.repeat,
        hyps.iter()
            .map(|h| format!("{} ({})", h.code, h.overrides.summary()))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let res = verify_hypotheses(
        ctx,
        &run,
        hyps,
        VerifyCfg {
            repeat: a.repeat,
            safety: &a.safety,
            no_save: a.no_save,
            calibrate: !a.no_calibrate,
            max_throttle_ratio: budget,
        },
    )
    .await?;
    ctx.out.finish(&res)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn replays_name_the_run_and_the_change() {
        let o = Overrides::new().with_set("concurrency", "5");
        let s = SafetyArgs {
            dry_run: false,
            read_only: false,
            max_cost_usd: Some(1.0),
            max_requests: None,
            max_bytes: None,
            max_duration: None,
            idempotency_key: None,
        };
        let args = replay_args("20261121-101010-abcd", &o, "verify-x", &s, true);
        let ra = parse_run(&args).unwrap();
        assert_eq!(ra.from.as_deref(), Some("20261121-101010-abcd"));
        assert_eq!(ra.set, vec!["concurrency=5".to_owned()]);
        assert_eq!(ra.output.label.as_deref(), Some("verify-x"));
        assert!(ra.output.no_save);
        assert_eq!(ra.safety.max_cost_usd, Some(1.0));
    }
}
