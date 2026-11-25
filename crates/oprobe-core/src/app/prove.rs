//! `oprobe prove RUN [FINDING] [--with PATH=VALUE]…` (design v6 §8 "Prove"): the only source
//! of numbers.
//!
//! 1. **Choose the change**: the finding's remedy, or `--with`.
//! 2. **Size it**: when the remedy is a size (a limit, a gap, a concurrency), a bracket search
//!    over that one value with short trials picks the objective's knee — the smallest value
//!    within [`KNEE_RATIO`] of the best — subject to `--max-throttle`.
//! 3. **A/B test**: interleaved A/B pairs (each pair's order drawn from the run's seed) with a
//!    sequential test ([`Sprt`]) on the objective against a noise floor learned from each
//!    side's trial-to-trial spread, until a decision, the pair limit or the wall-time budget
//!    (`--max-duration`).
//! 4. **Decide**: `improved | worse | inconclusive` on the objective (goodput = successful
//!    operations per second, or p99). Intervals are log-ratio (never below −100 %) and
//!    Wilson/Newcombe for shares; secondary metrics are shown, never decisive.
//! 5. **Save**: every trial is a run whose `parent_id` is this prove (hidden from
//!    `show --list` unless `--all`); the result is stored as `<id>.prove.json`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Ctx;
use super::input::{OutputOptions, Safety};
use crate::analysis::stats::Interval;
use crate::analysis::verdict::{Sprt, SprtDecision, log_ratio, ratio_difference};
use crate::config::Profile;
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::model::{Finding, Run, RunId};
use crate::operator::{OperatorDiff, OperatorSpec};
use crate::output::{CommandOutput, NextStep};
use crate::report::style::{bold, dim, green, red, yellow};
use crate::util::ByteSize;

/// Schema id of [`ProveOutput`].
pub const PROVE_SCHEMA: &str = "oprobe.prove/9";

/// A value within this share of the best objective is as good: the knee is the smallest one.
pub const KNEE_RATIO: f64 = 0.03;
/// The smallest noise floor assumed before the A/A pairs have spoken (per-pair log ratio).
const MIN_NOISE_SD: f64 = 0.02;
/// The smallest effect the sequential test looks for.
const MIN_EFFECT: f64 = 0.03;
/// A/B pairs at least before a verdict other than `inconclusive`: with fewer, the spread of
/// the pairs cannot be estimated (`--pairs` is raised to this, and a budget that allows
/// fewer gives `inconclusive`).
pub const MIN_DECISIVE_PAIRS: usize = 3;
/// The first retry backoffs prove searches for transient failures (ms): from a short 20 ms up
/// to half a second.
const RETRY_MIN_DELAY_MS: (u64, u64) = (20, 500);
/// OpenDAL's TailCutLayer ignores its first 10 s: trials of a tail-cut change warm up longer.
const TAIL_CUT_WARMUP: Duration = Duration::from_millis(11_500);

/// What prove judges a change on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProveObjective {
    /// Successful operations per second.
    #[default]
    Goodput,
    /// The p99 operation latency.
    P99,
}

impl ProveObjective {
    /// The objective the remedy of finding `code` is proved on (the one its `next` names).
    pub fn of_finding(code: &str) -> Self {
        match crate::analysis::findings::proof_objective(code) {
            Some("p99") => Self::P99,
            _ => Self::Goodput,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Goodput => "goodput",
            Self::P99 => "p99",
        }
    }

    /// The objective of a trial.
    fn value(self, t: &ProveTrial) -> f64 {
        match self {
            Self::Goodput => t.goodput_ops_per_s,
            Self::P99 => t.p99_ms,
        }
    }

    /// `ln(b/a)` signed so that positive is better.
    fn signed_log_ratio(self, a: f64, b: f64) -> f64 {
        let (a, b) = (a.max(1e-9), b.max(1e-9));
        match self {
            Self::Goodput => (b / a).ln(),
            Self::P99 => (a / b).ln(),
        }
    }

    /// `v` is within the knee ratio of `best`.
    fn within(self, v: f64, best: f64) -> bool {
        match self {
            Self::Goodput => v >= best * (1.0 - KNEE_RATIO),
            Self::P99 => v <= best * (1.0 + KNEE_RATIO),
        }
    }

    /// `a` is better than `b`.
    fn better(self, a: f64, b: f64) -> bool {
        match self {
            Self::Goodput => a > b,
            Self::P99 => a < b,
        }
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
    /// Share of throttled requests a sized value may cause (default 0.001)
    pub max_throttle_ratio: Option<f64>,
    /// Wall-time budget of the whole proof, e.g. 5m (`safety.max_duration` when unset): no
    /// trial starts that would not end within it
    pub budget: Option<String>,
    /// A/B pairs at most (3–12; at least 3 are needed to decide)
    pub pairs: usize,
    /// Measurement window of one trial (default 2s, at most the run's own window)
    pub trial_duration: Option<String>,
    /// Write the proven operator to an `op.toml` file or `[operators.NAME]` when `improved`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<String>,
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
            pairs: crate::config::prove_defaults::MAX_PAIRS,
            trial_duration: None,
            save: None,
            safety: Safety::default(),
        }
    }
}

impl ProveRequest {
    /// Prove the remedy of finding `code` of run `run` as its `next` command does: sized, on
    /// the objective it is proved on ([`ProveObjective::of_finding`]), default budget.
    pub fn for_finding(run: impl Into<String>, code: impl Into<String>) -> Self {
        let code = code.into();
        Self {
            run: run.into(),
            objective: ProveObjective::of_finding(&code),
            finding: Some(code),
            ..Self::default()
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

/// What a trial was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrialKind {
    /// A sizing trial of the bracket search (the change at `value`).
    Sizing,
    /// The current operator (A).
    Baseline,
    /// The changed operator (B).
    Candidate,
}

/// One trial run of a prove (an event as it finishes, and a row of the result).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProveTrial {
    pub kind: TrialKind,
    /// The A/B pair (0-based) of a baseline or candidate trial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<u64>,
    /// The sized value of a sizing trial (or the candidate's), as `--set` writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The stored trial run (a child of the prove).
    pub run_id: String,
    pub goodput_ops_per_s: f64,
    pub p99_ms: f64,
    pub ops_count: u64,
    pub requests_count: u64,
    pub throttled_count: u64,
    pub throttled_request_ratio: f64,
    /// Operations that failed for reasons other than throttling, per operation.
    pub error_ratio: f64,
    /// Within `--max-throttle` and the error ratio a run allows (1 %).
    pub feasible: bool,
}

/// The sizing step: the path, the values tried and the knee.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Sizing {
    /// The sized operator path.
    pub path: String,
    /// The chosen value (the knee), as `--set` writes it.
    pub value: String,
    /// Where the search started and why.
    pub start: String,
    /// Some value met `--max-throttle` (otherwise the least throttled one was chosen).
    pub feasible: bool,
    /// Why no trial ran (the value was taken from the evidence instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmeasured: Option<String>,
}

/// A secondary metric: shown, never decisive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecondaryMetric {
    /// `p99_ms`, `goodput_ops_per_s`, `throttled_request_ratio`, `error_ratio`.
    pub metric: String,
    /// Mean over the baseline trials (in the metric's unit).
    pub baseline: f64,
    /// Mean over the candidate trials.
    pub candidate: f64,
    /// Relative change (log ratio) for positive metrics, absolute difference (Newcombe) for
    /// shares; always within range.
    pub change: Interval,
}

/// Result of `oprobe prove`: the proven layer diff with its evidence.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProveOutput {
    /// Always [`PROVE_SCHEMA`].
    pub schema: String,
    /// This prove (its trials are runs with this `parent_id`).
    pub prove_id: String,
    /// The run whose operator was changed.
    pub run_id: String,
    /// The run's operator as given.
    pub operator_ref: String,
    /// The proven candidate operator (secrets redacted), so `.prove.json` stands on its own.
    pub operator: OperatorSpec,
    /// Where it was validated (design v6 D3): the scheme and target.
    pub environment: String,
    /// The finding whose remedy was proven (absent for `--with` alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
    /// The change, as `PATH=VALUE` pairs (the sized value included).
    pub change: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sizing: Option<Sizing>,
    pub objective: ProveObjective,
    pub verdict: ProveVerdict,
    /// Relative change of the objective (candidate vs baseline) with its 95 % interval,
    /// signed in the metric's own direction (goodput +: more; p99 −: faster).
    pub objective_delta: Interval,
    /// The A/A noise floor: the per-pair spread of the objective's log ratio.
    pub noise_floor_ratio: f64,
    /// Interleaved A/B pairs that ran.
    pub pairs_count: u64,
    pub max_throttle_ratio: f64,
    /// The other metrics (shown, never decisive).
    pub secondary: Vec<SecondaryMetric>,
    /// Every trial: sizing, then the A/B pairs.
    pub trials: Vec<ProveTrial>,
    /// The operator change as Rust and TOML.
    pub diff: OperatorDiff,
    /// A budget (`--max-duration`, `--max-requests`, `--max-bytes`) stopped the
    /// prove before it could finish: the result reads what was measured (exit 4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budget_exceeded: bool,
    /// Where the proven operator was written (`--save`, when `verdict == improved`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_to: Option<String>,
    /// `oprobe run` of the run's workload on the proven operator: the next profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ProveOutput {
    /// Parse a stored or `-o json` prove result, verifying its schema version.
    pub fn from_json_slice(raw: &[u8]) -> Result<Self, String> {
        let mut v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
        if v.get("schema").is_none()
            && let Some(inner) = v.get_mut("data")
        {
            v = inner.take();
        }
        match v.get("schema").and_then(|s| s.as_str()) {
            Some(PROVE_SCHEMA) => serde_json::from_value(v).map_err(|e| e.to_string()),
            Some(other) => Err(format!(
                "prove schema `{other}` is not supported; this oprobe reads `{PROVE_SCHEMA}`"
            )),
            None => Err("not an oprobe v9 prove result (missing `schema`)".into()),
        }
    }

    fn metric_label(m: &str) -> &str {
        match m {
            "p99_ms" => "p99",
            "goodput_ops_per_s" => "goodput",
            "throttled_request_ratio" => "throttled",
            "error_ratio" => "errors",
            other => other,
        }
    }
}

fn pct(x: f64) -> String {
    format!("{:+.1}", x * 100.0)
}

impl CommandOutput for ProveOutput {
    fn text(&self) -> String {
        let d = &self.objective_delta;
        let measured = self.pairs_count > 0;
        let verdict = match self.verdict {
            ProveVerdict::Improved => green("improved"),
            ProveVerdict::Worse => red("worse"),
            ProveVerdict::Inconclusive if !measured => yellow("not measured"),
            ProveVerdict::Inconclusive => yellow("inconclusive"),
        };
        let mut o = if measured {
            format!(
                "{} on {}: {} %, 95 % interval [{}, {}] ({} pair{} of trials, noise ±{:.1} %) · {}\n",
                bold(&verdict),
                self.objective.label(),
                pct(d.estimate),
                pct(d.low),
                pct(d.high),
                self.pairs_count,
                if self.pairs_count == 1 { "" } else { "s" },
                self.noise_floor_ratio * 100.0,
                self.environment
            )
        } else {
            format!(
                "{} on {}: no pair of trials ran · {}\n",
                bold(&verdict),
                self.objective.label(),
                self.environment
            )
        };
        if let Some(s) = &self.sizing {
            let tried: Vec<String> = self
                .trials
                .iter()
                .filter(|t| t.kind == TrialKind::Sizing)
                .filter_map(|t| {
                    t.value
                        .as_ref()
                        .map(|v| format!("{v}{}", if t.feasible { "" } else { "✗" }))
                })
                .collect();
            o += &format!(
                "sized {} = {}{}\n",
                s.path,
                bold(&s.value),
                match &s.unmeasured {
                    Some(why) => format!(" ({why})"),
                    None => format!(
                        " (knee of {}; tried {}{})",
                        self.objective.label(),
                        tried.join(" → "),
                        if s.feasible {
                            ""
                        } else {
                            "; none met --max-throttle"
                        }
                    ),
                }
            );
        }
        let sec: Vec<String> = self
            .secondary
            .iter()
            .map(|s| {
                let label = Self::metric_label(&s.metric);
                if s.metric.ends_with("_ratio") {
                    format!(
                        "{label} {:.2} % → {:.2} %",
                        s.baseline * 100.0,
                        s.candidate * 100.0
                    )
                } else {
                    format!(
                        "{label} {} % [{}, {}]",
                        pct(s.change.estimate),
                        pct(s.change.low),
                        pct(s.change.high)
                    )
                }
            })
            .collect();
        if !sec.is_empty() {
            o += &dim(&format!(
                "secondary (not decisive; 95 % intervals): {}\n",
                sec.join(" · ")
            ));
        }
        for n in &self.notes {
            o += &dim(&format!("note: {n}\n"));
        }
        if !measured {
            // Nothing was compared: a diff here would read as a finished proof.
            o += &dim(&format!("change not tested: {}\n", self.change.join(" ")));
            return o;
        }
        o += &self.diff.rust;
        if !self.diff.rust.ends_with('\n') {
            o.push('\n');
        }
        if !self.diff.toml.is_empty() {
            o += &format!("{}\n{}", dim("op.toml:"), self.diff.toml);
        }
        if let Some(dest) = &self.saved_to {
            o += &format!("{} {dest}\n", green("saved"));
        }
        o
    }

    fn exit_code(&self) -> u8 {
        if self.budget_exceeded {
            exit::BUDGET
        } else if self.verdict == ProveVerdict::Worse {
            exit::GATE
        } else {
            exit::OK
        }
    }

    fn next(&self) -> Vec<NextStep> {
        // The run's own workload (preset, `-c`, `-d`, `--existing`, …) on the proven operator;
        // a result stored without it falls back to the operator and the change alone.
        let command = self.run_command.clone().unwrap_or_else(|| {
            let mut sets = String::new();
            for c in &self.change {
                sets += &format!(" --set {}", shell_edit(c));
            }
            format!(
                "oprobe run {}{sets}",
                crate::model::spec::shell_quote(&self.operator_ref)
            )
        });
        vec![
            NextStep::new("profile the changed operator", command),
            NextStep::new("its trials", "oprobe show --list --all"),
        ]
    }
}

/// How long a whole proof takes: its sizing trials and its interleaved A/B pairs, each trial
/// a short run (`prove --dry-run`, and the first lines of every prove).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProofEstimate {
    /// The command that runs this proof.
    pub command: String,
    /// The change, as `PATH=VALUE` pairs (a sized value at its starting point).
    pub change: Vec<String>,
    /// Sizing trials at most (0 when the change has no size to search).
    pub sizing_trials_max_count: u64,
    /// The fewest and the most A/B pairs (a pair is two trials: A and B, in either order).
    pub pairs_min_count: u64,
    pub pairs_max_count: u64,
    pub trials_min_count: u64,
    pub trials_max_count: u64,
    /// Wall time of one baseline trial (warmup, window, dataset and cleanup).
    pub trial_eta_s: f64,
    /// Wall time of the whole proof: at least (the fewest trials) and at most (every trial,
    /// within the budget).
    pub eta_min_s: f64,
    pub eta_max_s: f64,
    /// The wall-time budget, when one is set (`--max-duration`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_s: Option<f64>,
    /// Trials the proof would run without the budget, when the budget leaves some out (the
    /// counts above are those that fit in it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unbudgeted_trials_max_count: Option<u64>,
}

/// `42s`, `1m 30s`.
fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0).round() as u64;
    if s < 60 {
        format!("{s}s")
    } else if s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

impl ProofEstimate {
    /// The estimate of a proof of `sizing` (most, fewest) sizing trials and `pairs` (fewest,
    /// most) A/B pairs, whose baseline trials take `a_eta_s` and whose sizing and candidate
    /// trials take `b_first_eta_s` — or `b_warm_eta_s` once the first candidate trial has
    /// warmed a shared layer — within `budget`.
    #[allow(clippy::too_many_arguments)]
    fn new(
        command: String,
        change: Vec<String>,
        sizing: (u64, u64),
        pairs: (u64, u64),
        a_eta_s: f64,
        b_first_eta_s: f64,
        b_warm_eta_s: f64,
        budget: Option<Duration>,
    ) -> Self {
        let pair_cold_s = a_eta_s + b_first_eta_s;
        let pair_warm_s = a_eta_s + b_warm_eta_s;
        let pairs_cost = |p: u64| -> f64 {
            if p == 0 {
                0.0
            } else {
                pair_cold_s + (p - 1) as f64 * pair_warm_s
            }
        };
        let budget_s = budget.map(|b| b.as_secs_f64());
        let unbudgeted = sizing.0 + 2 * pairs.1;
        // Plan only the trials that fit in the budget (K19): the sizing trials run first, the
        // A/B pairs in what is left; no trial starts that would not end in it.
        let fit = |s: u64, p: u64| -> (u64, u64) {
            let Some(b) = budget_s else {
                return (s, p);
            };
            let s = s.min((b / b_first_eta_s.max(1e-3)).floor() as u64);
            let left = (b - s as f64 * b_first_eta_s).max(0.0);
            let fit_p = if left < pair_cold_s {
                0
            } else {
                1 + ((left - pair_cold_s) / pair_warm_s.max(1e-3)).floor() as u64
            };
            (s, p.min(fit_p))
        };
        let (s_min, p_min) = fit(sizing.1, pairs.0);
        let (s_max, p_max) = fit(sizing.0, pairs.1);
        let min = s_min as f64 * b_first_eta_s + pairs_cost(p_min);
        let mut max = sizing.0 as f64 * b_first_eta_s + pairs_cost(pairs.1);
        if let Some(b) = budget_s {
            max = max.min(b);
        }
        let trials_min = s_min + 2 * p_min;
        let trials_max = (s_max + 2 * p_max).max(trials_min);
        Self {
            command,
            change,
            sizing_trials_max_count: s_max,
            pairs_min_count: p_min.min(p_max),
            pairs_max_count: p_max.max(p_min),
            trials_min_count: trials_min,
            trials_max_count: trials_max,
            trial_eta_s: a_eta_s,
            eta_min_s: min.min(max),
            eta_max_s: max,
            budget_s,
            unbudgeted_trials_max_count: (trials_max < unbudgeted).then_some(unbudgeted),
        }
    }

    /// `6–20 trials (up to 8 sizing, 3–6 pairs of A and B) · ETA 25s – 1m 10s`.
    pub fn line(&self) -> String {
        let sizing = if self.sizing_trials_max_count > 0 {
            format!("up to {} sizing, ", self.sizing_trials_max_count)
        } else {
            String::new()
        };
        let pairs = if self.pairs_min_count == self.pairs_max_count {
            format!("{} pairs", self.pairs_min_count)
        } else {
            format!("{}–{} pairs", self.pairs_min_count, self.pairs_max_count)
        };
        let trials = if self.trials_min_count == self.trials_max_count {
            format!("{} trials", self.trials_max_count)
        } else {
            format!("{}–{} trials", self.trials_min_count, self.trials_max_count)
        };
        let eta = if (self.eta_max_s - self.eta_min_s).abs() < 1.0 {
            fmt_secs(self.eta_max_s)
        } else {
            format!(
                "{} – {}",
                fmt_secs(self.eta_min_s),
                fmt_secs(self.eta_max_s)
            )
        };
        let budget = match self.budget_s {
            Some(b) => format!(" (budget {})", fmt_secs(b)),
            None => String::new(),
        };
        let cut = match self.unbudgeted_trials_max_count {
            Some(all) => {
                let mut c = format!("; the budget fits {} of {all}", self.trials_max_count);
                if (self.pairs_max_count as usize) < MIN_DECISIVE_PAIRS {
                    c += &format!(
                        ", too few pairs to decide (at least {MIN_DECISIVE_PAIRS}): raise --max-duration"
                    );
                }
                c
            }
            None => String::new(),
        };
        format!("{trials} ({sizing}{pairs} of A and B{cut}) · ETA {eta}{budget}")
    }

    /// The proof's plan as text (`prove --dry-run`).
    pub fn text(&self) -> String {
        format!(
            "{} {}\n  {}\n",
            bold("proof"),
            self.change.join(" "),
            self.line()
        )
    }
}

/// What [`prove`] produced.
#[derive(Clone, Debug)]
pub enum ProveOutcome {
    /// `safety.dry_run`: the plan of one trial (every trial runs the same workload).
    Plan(Box<super::run::PlanOutput>),
    Proved(Box<ProveOutput>),
}

/// The domain of a sized value.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Domain {
    Count { min: u64, max: u64 },
    Bytes { min: u64, max: u64 },
    Millis { min: u64, max: u64 },
}

impl Domain {
    fn clamp(self, v: u64) -> u64 {
        let (lo, hi) = self.bounds();
        v.clamp(lo, hi)
    }

    fn bounds(self) -> (u64, u64) {
        match self {
            Self::Count { min, max } | Self::Bytes { min, max } | Self::Millis { min, max } => {
                (min, max.max(min))
            }
        }
    }

    fn fmt(self, v: u64) -> String {
        match self {
            Self::Count { .. } => v.to_string(),
            Self::Bytes { .. } => ByteSize(v).to_string(),
            Self::Millis { .. } => format!("{v}ms"),
        }
    }

    /// The next value up while climbing (doubling; counts at least +1).
    fn up(self, v: u64) -> u64 {
        self.clamp(v.saturating_mul(2).max(v + 1))
    }

    fn down(self, v: u64) -> u64 {
        self.clamp(v / 2)
    }

    /// A value between `lo` and `hi`, or `None` when they are adjacent (counts) or within
    /// 25 % (sizes, durations).
    fn mid(self, lo: u64, hi: u64) -> Option<u64> {
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        match self {
            Self::Count { .. } => (hi - lo > 1).then_some(lo + (hi - lo) / 2),
            _ => (hi as f64 > lo.max(1) as f64 * 1.25)
                .then(|| ((lo.max(1) as f64 * hi as f64).sqrt()) as u64),
        }
    }
}

/// The change to prove: fixed `PATH=VALUE` pairs and, when the remedy is a size, the path to
/// size with its domain, start value and the pairs that follow the sized value.
#[derive(Clone, Debug)]
struct Change {
    finding: Option<String>,
    fixed: Vec<(String, String)>,
    sized: Option<SizedPath>,
    /// Trials need a long warmup (tail-cut's blind start).
    long_warmup: bool,
    notes: Vec<String>,
}

#[derive(Clone, Debug)]
struct SizedPath {
    path: String,
    domain: Domain,
    start: u64,
    start_why: String,
    /// Extra pairs that depend on the value (`read.chunk` for `read.concurrent`).
    follow: fn(u64, &Run) -> Vec<(String, String)>,
    /// Sizing trials cannot measure this value; take `start` as the size.
    unmeasured: Option<String>,
}

fn no_follow(_: u64, _: &Run) -> Vec<(String, String)> {
    Vec::new()
}

/// `read.chunk` for `read.concurrent = n`: the mean read split in `n`, at least 256 KiB.
fn read_chunk_follow(n: u64, run: &Run) -> Vec<(String, String)> {
    let reads: Vec<u64> = run
        .reqs
        .iter()
        .filter(|q| q.api == crate::trace::Api::GetObject && !q.is_error())
        .map(|q| q.bytes_down)
        .collect();
    let mean = reads.iter().sum::<u64>() / reads.len().max(1) as u64;
    let chunk = (mean / n.max(1)).max(256 * 1024).next_power_of_two();
    vec![("read.chunk".into(), ByteSize(chunk).to_string())]
}

/// `layers.throttle.burst` for a bandwidth: at least the bandwidth and the largest write.
fn throttle_burst_follow(bw: u64, run: &Run) -> Vec<(String, String)> {
    let largest = run.reqs.iter().map(|q| q.bytes_up).max().unwrap_or(0);
    let burst = bw.max(largest).max(16 << 20);
    vec![("layers.throttle.burst".into(), ByteSize(burst).to_string())]
}

fn evidence(f: &Finding, key: &str) -> Option<f64> {
    f.evidence
        .values
        .get(key)
        .copied()
        .filter(|v| v.is_finite())
}

/// Mean latency of successful requests (seconds).
fn mean_request_s(run: &Run) -> f64 {
    let ok: Vec<u64> = run
        .reqs
        .iter()
        .filter(|q| !q.is_error())
        .map(|q| q.total_us)
        .collect();
    if ok.is_empty() {
        return 0.01;
    }
    ok.iter().sum::<u64>() as f64 / ok.len() as f64 / 1e6
}

/// The change of a finding's remedy.
fn remedy_change(run: &Run, f: &Finding) -> Result<Change> {
    // A remedy that is not a layer (the key layout, a cap on reads, the client, a failing
    // request) has nothing to prove: say where to look instead, as the finding's `next` does.
    if !f.remedy.provable() {
        let next =
            crate::analysis::findings::next_command(&crate::model::spec::shell_quote(run.id()), f);
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!(
                "finding `{}` has no operator remedy to prove: {}",
                f.code, f.remedy.text
            ),
        )
        .with_hint(format!(
            "see it with `{next}`, or pass a change to test with --with PATH=VALUE"
        ))
        .into());
    }
    let r = &run.report;
    let peak = r.capacity.observed.peak_inflight_count.max(1);
    let spec = &r.spec.profile.operator;
    let retry_on = spec.layers.retry.is_some();
    let mut c = Change {
        finding: Some(f.code.clone()),
        fixed: Vec::new(),
        sized: None,
        long_warmup: false,
        notes: Vec::new(),
    };
    let limit = |start: u64, why: String| SizedPath {
        path: "layers.concurrent_limit.http".into(),
        domain: Domain::Count {
            min: 1,
            max: (peak * 2).max(start * 2).max(4),
        },
        start,
        start_why: why,
        follow: no_follow,
        unmeasured: None,
    };
    match f.code.as_str() {
        "throttling" => {
            c.fixed.push(("layers.retry.jitter".into(), "on".into()));
            let (start, why) = match evidence(f, "fitted_inflight_count") {
                Some(n) => (n as u64, "the fitted in-flight limit".to_owned()),
                None => (
                    (peak / 2).max(1),
                    "half the requests in flight at the run's peak".to_owned(),
                ),
            };
            c.sized = Some(limit(start.max(1), why));
        }
        "rate_limit" => {
            c.fixed.push(("layers.retry.jitter".into(), "on".into()));
            let lat = mean_request_s(run);
            let (start, why) = match evidence(f, "fitted_requests_per_s") {
                Some(rps) => (
                    ((rps * lat).floor() as u64).max(1),
                    format!(
                        "the fitted rate × the mean request latency ({rps:.0}/s × {})",
                        crate::util::fmt_ms(lat * 1000.0)
                    ),
                ),
                None => (
                    (peak / 2).max(1),
                    "half the requests in flight at the run's peak".to_owned(),
                ),
            };
            c.sized = Some(limit(start, why));
        }
        "bandwidth_cap" => {
            let writes = r
                .workload
                .mix
                .iter()
                .any(|m| m.weight > 0 && m.op.contains("write"));
            if !writes {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    "finding `bandwidth_cap` on a read workload has no operator remedy to prove (OpenDAL's ThrottleLayer shapes writes only)",
                )
                .with_hint("pass the change to test with --with PATH=VALUE")
                .into());
            }
            let bps = evidence(f, "fitted_bytes_per_s")
                .unwrap_or(r.summary.mib_per_s * 1024.0 * 1024.0)
                .max(1024.0 * 1024.0);
            let mib = ((bps / (1024.0 * 1024.0)).floor() as u64).max(1) << 20;
            c.sized = Some(SizedPath {
                path: "layers.throttle.bandwidth".into(),
                domain: Domain::Bytes {
                    min: 1 << 20,
                    max: (mib * 4).max(64 << 20),
                },
                start: mib,
                start_why: "the fitted aggregate cap".into(),
                follow: throttle_burst_follow,
                unmeasured: None,
            });
        }
        "tail_stragglers" => {
            c.fixed.push(("layers.tail_cut".into(), "on".into()));
            let us = crate::analysis::findings::TAIL_CUT_REISSUE_US;
            if retry_on {
                c.fixed
                    .push(("layers.retry.min_delay".into(), format!("{}ms", us / 1000)));
            } else {
                c.fixed.push(("layers.retry".into(), "on".into()));
                c.fixed
                    .push(("layers.retry.min_delay".into(), format!("{}ms", us / 1000)));
            }
            c.long_warmup = true;
            c.notes.push(
                "TailCutLayer re-issues through RetryLayer: its first backoff is shortened so a cut request is retried at once; trials warm up 11.5 s because the layer ignores its first 10 s"
                    .into(),
            );
        }
        "hung_requests" => {
            let median = evidence(f, "median_request_ms").unwrap_or(100.0);
            let start = ((median * 20.0) as u64).clamp(200, 30_000);
            c.sized = Some(SizedPath {
                path: "layers.timeout.io".into(),
                domain: Domain::Millis {
                    min: 50,
                    max: 60_000,
                },
                start,
                start_why: "twenty times the median request".into(),
                follow: no_follow,
                unmeasured: None,
            });
        }
        "small_ranges" if f.remedy.path.as_deref() == Some("read.concurrent") => {
            c.sized = Some(SizedPath {
                path: "read.concurrent".into(),
                domain: Domain::Count { min: 2, max: 32 },
                start: 4,
                start_why: "four concurrent requests per read".into(),
                // A large serial stream is split in chunks; separate ranges are not.
                follow: if f.remedy.with.iter().any(|w| w == "read.chunk") {
                    read_chunk_follow
                } else {
                    no_follow
                },
                unmeasured: None,
            });
        }
        "small_ranges" => {
            c.sized = Some(SizedPath {
                path: "read.gap".into(),
                domain: Domain::Bytes {
                    min: 4 << 10,
                    max: 32 << 20,
                },
                start: 2 << 20,
                start_why: "twice OpenDAL's 1 MiB default coalescing gap".into(),
                follow: no_follow,
                unmeasured: None,
            });
        }
        "slow_multipart" => {
            c.sized = Some(SizedPath {
                path: "write.concurrent".into(),
                domain: Domain::Count { min: 2, max: 32 },
                start: 4,
                start_why: "four parts in flight per write".into(),
                follow: no_follow,
                unmeasured: None,
            });
        }
        "repeated_reads" => {
            let obj_bytes = r.workload.dataset.object_size_bytes;
            let mut seen: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
            for o in run.ops.iter().filter(|o| {
                o.t_us >= 0 && (o.verb.starts_with("read") || o.verb.contains("parquet"))
            }) {
                let entry = seen.entry(o.key.as_str()).or_insert(0);
                *entry = (*entry).max(o.bytes).max(obj_bytes);
            }
            let mut set: u64 = seen.values().sum();
            if r.run.sampled && r.sampling.kept_ratio > 0.0 && r.sampling.kept_ratio < 1.0 {
                let scaled = (set as f64 / r.sampling.kept_ratio.max(0.05)).round() as u64;
                let ds_cap = r
                    .workload
                    .dataset
                    .objects_count
                    .saturating_mul(obj_bytes.max(1));
                set = if ds_cap > 0 {
                    scaled.min(ds_cap).max(set)
                } else {
                    scaled
                };
            }
            // Foyer shards memory into 4 pieces: keep at least 2 whole objects per shard (8x).
            let min_foyer = obj_bytes.saturating_mul(8).max(16 << 20);
            let mem = ((set as f64 * 1.25) as u64).max(min_foyer).min(1 << 30);
            c.fixed
                .push(("layers.cache.memory".into(), ByteSize(mem).to_string()));
        }
        "cold_connections" => match f.remedy.path.as_deref() {
            Some("http.keep_alive") => c.fixed.push(("http.keep_alive".into(), "on".into())),
            _ => c.fixed.push(("http.version".into(), "h2".into())),
        },
        // Transient failures that retries recovered (K1): their cost is the retry backoff
        // (OpenDAL waits 1 s first). Size the first backoff from short to long, with jitter:
        // the search climbs only while a longer backoff does better (a service that needs
        // room to recover), and the knee is the shortest as good as the best.
        "errors" if f.remedy.path.as_deref() == Some("layers.retry.min_delay") => {
            if !retry_on {
                c.fixed.push(("layers.retry".into(), "on".into()));
            }
            c.fixed.push(("layers.retry.jitter".into(), "on".into()));
            c.sized = Some(SizedPath {
                path: "layers.retry.min_delay".into(),
                domain: Domain::Millis {
                    min: RETRY_MIN_DELAY_MS.0,
                    max: RETRY_MIN_DELAY_MS.1,
                },
                start: RETRY_MIN_DELAY_MS.0,
                start_why: "a short first backoff: the failures do not follow the load".into(),
                follow: no_follow,
                unmeasured: None,
            });
        }
        // A 7.0 run's remedy for retried 5xx.
        "errors" if f.remedy.path.as_deref() == Some("layers.retry.jitter") => {
            c.fixed.push(("layers.retry.jitter".into(), "on".into()));
        }
        code => {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("finding `{code}` has no operator remedy to prove"),
            )
            .with_hint("pass the change to test with --with PATH=VALUE")
            .into());
        }
    }
    Ok(c)
}

/// The change of `req`: `--with`, or the remedy of the requested (or first provable) finding.
fn choose_change(run: &Run, req: &ProveRequest) -> Result<Change> {
    if !req.with.is_empty() {
        let mut fixed = Vec::new();
        for kv in &req.with {
            let (k, v) = kv.split_once('=').ok_or_else(|| {
                OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("--with `{kv}` is not PATH=VALUE"),
                )
            })?;
            fixed.push((k.trim().to_owned(), v.trim().to_owned()));
        }
        // The order the pairs are given in does not matter (the app passes its edits as a
        // map): switches first, the fields of a layer switched off dropped.
        let fixed = crate::operator::normalize_edits(&fixed);
        let long_warmup = fixed.iter().any(|(k, v)| {
            k == "layers.tail_cut" && !matches!(v.as_str(), "off" | "false")
                || k.starts_with("layers.tail_cut.")
        });
        return Ok(Change {
            finding: req.finding.clone(),
            fixed,
            sized: None,
            long_warmup,
            notes: Vec::new(),
        });
    }
    let findings = &run.report.findings;
    let f = match &req.finding {
        Some(code) => {
            let want = code.trim().to_ascii_lowercase().replace('-', "_");
            findings.iter().find(|f| f.code == want).ok_or_else(|| {
                let have: Vec<&str> = findings.iter().map(|f| f.code.as_str()).collect();
                OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("run {} has no `{want}` finding", run.id()),
                )
                .with_hint(if have.is_empty() {
                    "it has no findings: pass the change to test with --with PATH=VALUE".to_owned()
                } else {
                    format!("its findings: {}", have.join(", "))
                })
            })?
        }
        None => findings
            .iter()
            .find(|f| f.remedy.provable())
            .ok_or_else(|| {
                OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("run {} has no finding with an operator remedy", run.id()),
                )
                .with_hint("pass the change to test with --with PATH=VALUE, e.g. --with layers.concurrent_limit.http=16")
            })?,
    };
    remedy_change(run, f)
}

/// Runs the trials of one prove: the run's profile with a change, under one guard and one
/// wall-time budget; every trial saved as a child of the prove.
struct Trials<'a> {
    ctx: &'a Ctx,
    run: &'a Run,
    base: Profile,
    prove_id: String,
    safety: Safety,
    guard: Option<std::sync::Arc<crate::guard::Guard>>,
    cancel: Cancel,
    deadline: Option<Instant>,
    /// Wall time of one normal (or already-warmed tail-cut) trial.
    per_trial: Duration,
    /// Wall time of one cold tail-cut trial (`TAIL_CUT_WARMUP`).
    per_cold_tail_cut: Duration,
    /// Short trial warmup used when no cold `TailCutLayer` needs 11.5 s priming.
    short_warmup: Duration,
    /// Warmed `TailCutLayer` for Baseline A trials (isolated from Candidate/Sizing).
    warmed_baseline_tc: Option<(OperatorSpec, opendal::layers::TailCutLayer, Instant)>,
    /// Warmed `TailCutLayer` for Candidate/Sizing trials matching the exact `OperatorSpec`.
    warmed_candidate_tc: Option<(OperatorSpec, opendal::layers::TailCutLayer, Instant)>,
    max_throttle: f64,
    max_error: f64,
    out: Vec<ProveTrial>,
    target_name: Option<String>,
    /// The budget breach that stopped the trials (the partial trial run is saved).
    breach: Option<anyhow::Error>,
}

impl Trials<'_> {
    /// Expected wall duration of running `(sets, kind)` next (cold vs warmed `TailCutLayer`).
    fn expected_duration(&self, sets: &[(String, String)], kind: TrialKind) -> Duration {
        let mut op = self.base.operator.clone();
        if kind != TrialKind::Baseline {
            let _ = op.apply_edits(sets);
        }
        let Some(tc) = &op.layers.tail_cut else {
            return self.per_trial;
        };
        let window = tc.window.get();
        let slot = if kind == TrialKind::Baseline {
            &self.warmed_baseline_tc
        } else {
            &self.warmed_candidate_tc
        };
        let warm = window >= Duration::from_secs(20)
            && slot
                .as_ref()
                .is_some_and(|(prev, _, used_at)| *prev == op && used_at.elapsed() < window / 2);
        if warm {
            self.per_trial
        } else {
            self.per_cold_tail_cut
        }
    }

    fn has_time_for(&self, need: Duration) -> bool {
        self.breach.is_none()
            && !self.cancel.is_cancelled()
            && self.deadline.is_none_or(|d| Instant::now() + need <= d)
    }

    fn time_left_for_sets(&self, sets: &[(String, String)], kind: TrialKind) -> bool {
        self.has_time_for(self.expected_duration(sets, kind))
    }

    /// [`Self::trial`], with a budget breach recorded instead of returned: `None` once a
    /// budget has stopped the trials (no more run). User cancellation (`RunInterrupted`)
    /// aborts immediately as an error (exit 130).
    async fn try_trial(
        &mut self,
        sets: &[(String, String)],
        kind: TrialKind,
        pair: Option<u64>,
        value: Option<String>,
    ) -> Result<Option<ProveTrial>> {
        if self.cancel.is_cancelled() && self.breach.is_none() {
            return Err(OprobeError::new(ErrorCode::RunInterrupted, "prove interrupted").into());
        }
        if self.breach.is_some() {
            return Ok(None);
        }
        match self.trial(sets, kind, pair, value).await {
            Ok(t) => Ok(Some(t)),
            Err(e) if is_budget_breach(&e) => {
                self.ctx
                    .out
                    .warn(format!("{e:#}: the prove stops with what it measured"));
                self.breach = Some(e);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    async fn trial(
        &mut self,
        sets: &[(String, String)],
        kind: TrialKind,
        pair: Option<u64>,
        value: Option<String>,
    ) -> Result<ProveTrial> {
        if self.cancel.is_cancelled() {
            return Err(OprobeError::new(ErrorCode::RunInterrupted, "prove interrupted").into());
        }
        let mut p = self.base.clone();
        if kind != TrialKind::Baseline {
            p.operator.apply_edits(sets)?;
        }
        let mut shared_tail_cut: Option<(OperatorSpec, opendal::layers::TailCutLayer)> = None;
        let mut cold_tail_cut = false;
        if let Some(params) = crate::operator::build::layer_params(&p.operator, "tail_cut") {
            let window = p
                .operator
                .layers
                .tail_cut
                .as_ref()
                .map(|t| t.window.get())
                .unwrap_or(Duration::from_secs(60));
            let reusable = window >= Duration::from_secs(20);
            let slot = if kind == TrialKind::Baseline {
                &self.warmed_baseline_tc
            } else {
                &self.warmed_candidate_tc
            };
            if reusable
                && let Some((prev_spec, layer, used_at)) = slot
                && *prev_spec == p.operator
                && used_at.elapsed() < window / 2
            {
                p.workload.warmup = crate::util::HumanDuration(self.short_warmup);
                shared_tail_cut = Some((p.operator.clone(), layer.clone()));
            } else {
                p.workload.warmup =
                    crate::util::HumanDuration(self.short_warmup.max(TAIL_CUT_WARMUP));
                cold_tail_cut = true;
                if reusable {
                    let layer = crate::operator::build::tail_cut_layer(&params);
                    shared_tail_cut = Some((p.operator.clone(), layer));
                }
            }
        } else {
            p.workload.warmup = crate::util::HumanDuration(self.short_warmup);
        }
        p.name = Some(format!("prove {} {}", self.prove_id, kind_label(kind)));
        p.validate()?;
        let rails = super::run::rails(self.ctx, &p, &self.safety)?;
        let output = OutputOptions::default();
        let started = Instant::now();
        let m = super::measure::measure(
            self.ctx,
            p,
            super::measure::Opts {
                target_name: self.target_name.clone(),
                operator_ref: Some(self.run.report.spec.operator_ref.clone()),
                rails,
                guard: self.guard.clone(),
                output: &output,
                cancel: &self.cancel,
                parent_id: Some(self.prove_id.clone()),
                tail_cut: shared_tail_cut.as_ref().map(|(_, l)| l.clone()),
            },
        )
        .await?;
        if m.cancelled && m.breach.is_none() {
            return Err(OprobeError::new(ErrorCode::RunInterrupted, "prove interrupted").into());
        }
        let elapsed = started.elapsed();
        if cold_tail_cut {
            self.per_cold_tail_cut = self.per_cold_tail_cut.max(elapsed);
        } else {
            self.per_trial = self.per_trial.max(elapsed);
        }
        if !m.cancelled
            && m.breach.is_none()
            && m.run.report.summary.ops > 0
            && let Some((spec_key, layer)) = shared_tail_cut
        {
            let entry = Some((spec_key, layer, Instant::now()));
            if kind == TrialKind::Baseline {
                self.warmed_baseline_tc = entry;
            } else {
                self.warmed_candidate_tc = entry;
            }
        }
        let r = &m.run.report;
        let failed = crate::analysis::findings::non_throttle_failures(r);
        let ops = r.summary.ops;
        let requests = r.connections.requests;
        let thr = r.resilience.throttled_responses;
        let throttled_request_ratio = thr as f64 / requests.max(1) as f64;
        let error_ratio = failed as f64 / ops.max(1) as f64;
        let t = ProveTrial {
            kind,
            pair,
            value,
            run_id: m.run.id().to_owned(),
            goodput_ops_per_s: r.good_ops_per_s(),
            p99_ms: r.summary.latency.p99,
            ops_count: ops,
            requests_count: requests,
            throttled_count: thr,
            throttled_request_ratio,
            error_ratio,
            feasible: ops > 0
                && throttled_request_ratio <= self.max_throttle
                && error_ratio <= self.max_error,
        };
        self.ctx
            .out
            .bus()
            .emit(crate::events::EventKind::Trial { trial: t.clone() });
        self.ctx.out.info(dim(&format!(
            "· {} {}{}: {:.1} ops/s · p99 {} · {:.2} % throttled",
            kind_label(kind),
            pair.map(|p| format!("#{} ", p + 1)).unwrap_or_default(),
            t.value.clone().unwrap_or_default(),
            t.goodput_ops_per_s,
            crate::util::fmt_ms(t.p99_ms),
            t.throttled_request_ratio * 100.0
        )));
        self.out.push(t.clone());
        Ok(t)
    }
}

/// Deletes the trial runs of prove `prove_id` when dropped armed: a prove that ends before its
/// result is saved (Ctrl-C, an error, a front end dropping the job) leaves no orphan trials.
struct OrphanTrials {
    store: crate::store::RunStore,
    prove_id: String,
    armed: bool,
}

impl Drop for OrphanTrials {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for id in self.store.children(&self.prove_id).unwrap_or_default() {
            let _ = self.store.delete(&id);
        }
    }
}

/// A guard's budget stopped a trial (`safety.budget_exceeded`).
fn is_budget_breach(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<OprobeError>()
            .is_some_and(|o| o.code == ErrorCode::SafetyBudgetExceeded)
    })
}

fn kind_label(k: TrialKind) -> &'static str {
    match k {
        TrialKind::Sizing => "sizing",
        TrialKind::Baseline => "A",
        TrialKind::Candidate => "B",
    }
}

/// The sets of the change at sized value `v` (or with no sized value).
fn sets_at(c: &Change, v: Option<u64>, run: &Run) -> Vec<(String, String)> {
    let mut sets = c.fixed.clone();
    if let (Some(s), Some(v)) = (&c.sized, v) {
        sets.push((s.path.clone(), s.domain.fmt(v)));
        sets.extend((s.follow)(v, run));
    }
    sets
}

/// The knee of the evaluated values: the smallest feasible value within [`KNEE_RATIO`] of the
/// best feasible objective; without a feasible value, the least throttled one.
fn knee(evals: &BTreeMap<u64, ProveTrial>, obj: ProveObjective) -> (u64, bool) {
    let feasible: Vec<(u64, f64)> = evals
        .iter()
        .filter(|(_, t)| t.feasible)
        .map(|(v, t)| (*v, obj.value(t)))
        .collect();
    if let Some(best) = feasible
        .iter()
        .map(|x| x.1)
        .reduce(|a, b| if obj.better(a, b) { a } else { b })
    {
        let v = feasible
            .iter()
            .find(|(_, o)| obj.within(*o, best))
            .map(|x| x.0)
            .expect("the best is within itself");
        return (v, true);
    }
    let v = evals
        .iter()
        .min_by(|a, b| {
            a.1.throttled_request_ratio
                .total_cmp(&b.1.throttled_request_ratio)
        })
        .map(|x| *x.0)
        .unwrap_or(0);
    (v, false)
}

/// Measures one value of a sized path ([`search`]): `None` once no more trial can run.
trait Measure {
    fn measure(
        &mut self,
        v: u64,
    ) -> impl std::future::Future<Output = Result<Option<ProveTrial>>> + Send;
}

/// The sizing trials of a change.
struct SizingTrials<'t, 'a> {
    t: &'t mut Trials<'a>,
    c: &'t Change,
    d: Domain,
}

impl<M: Measure + Send> Measure for &mut M {
    fn measure(
        &mut self,
        v: u64,
    ) -> impl std::future::Future<Output = Result<Option<ProveTrial>>> + Send {
        (**self).measure(v)
    }
}

impl Measure for SizingTrials<'_, '_> {
    async fn measure(&mut self, v: u64) -> Result<Option<ProveTrial>> {
        let sets = sets_at(self.c, Some(v), self.t.run);
        if !self.t.time_left_for_sets(&sets, TrialKind::Sizing) {
            return Ok(None);
        }
        self.t
            .try_trial(&sets, TrialKind::Sizing, None, Some(self.d.fmt(v)))
            .await
    }
}

/// One sizing trial at `v` (cached; `None` when the trial limit or the budget is reached).
async fn eval_at(
    evals: &mut BTreeMap<u64, ProveTrial>,
    m: &mut impl Measure,
    v: u64,
    max_trials: usize,
) -> Result<Option<ProveTrial>> {
    if let Some(r) = evals.get(&v) {
        return Ok(Some(r.clone()));
    }
    if evals.len() >= max_trials {
        return Ok(None);
    }
    let Some(r) = m.measure(v).await? else {
        return Ok(None);
    };
    evals.insert(v, r.clone());
    Ok(Some(r))
}

/// The bracket search over one value (step 2) with sizing trials of the change.
async fn size(
    t: &mut Trials<'_>,
    c: &Change,
    s: &SizedPath,
    obj: ProveObjective,
    max_trials: usize,
) -> Result<(u64, bool)> {
    let d = s.domain;
    search(d, s.start, obj, max_trials, SizingTrials { t, c, d }).await
}

/// The bracket search over one value of domain `d` from `start`, measuring each value with
/// `trial`: climb (doubling) while feasible and improving, or descend (halving) while
/// infeasible, then bisect between the bracket's ends; the answer is the [`knee`] of
/// everything measured. When the first step up is no better than the start, the best is at or
/// below the start: the search descends while a smaller value is as good, and bisects nothing
/// above (S7: 9.0 sized a 20 ms backoff 20 → 40 → 28 → 33 ms, every step worse than 20).
async fn search(
    d: Domain,
    start: u64,
    obj: ProveObjective,
    max_trials: usize,
    mut trial: impl Measure,
) -> Result<(u64, bool)> {
    let mut evals: BTreeMap<u64, ProveTrial> = BTreeMap::new();
    let v0 = d.clamp(start);
    let Some(first) = eval_at(&mut evals, &mut trial, v0, max_trials).await? else {
        return Ok((v0, false));
    };
    let (mut lo, mut hi);
    if first.feasible {
        let (mut prev, mut prev_t) = (v0, first);
        loop {
            let v = d.up(prev);
            if v == prev {
                return Ok(knee(&evals, obj));
            }
            let Some(r) = eval_at(&mut evals, &mut trial, v, max_trials).await? else {
                return Ok(knee(&evals, obj));
            };
            // Keep climbing only while the value is feasible and better than the previous one
            // by more than the knee ratio (9.0 compared the wrong way round, so it never
            // climbed past the first step).
            if r.feasible
                && obj.better(obj.value(&r), obj.value(&prev_t))
                && !obj.within(obj.value(&prev_t), obj.value(&r))
            {
                prev = v;
                prev_t = r;
                continue;
            }
            if prev == v0 && r.feasible {
                // The first step up did not help: look below the start instead.
                let mut cur_t = prev_t;
                loop {
                    let v = d.down(prev);
                    if v == prev {
                        break;
                    }
                    let Some(r) = eval_at(&mut evals, &mut trial, v, max_trials).await? else {
                        break;
                    };
                    let as_good = obj.within(obj.value(&r), obj.value(&cur_t))
                        || obj.better(obj.value(&r), obj.value(&cur_t));
                    if !r.feasible || !as_good {
                        break;
                    }
                    prev = v;
                    cur_t = r;
                }
                return Ok(knee(&evals, obj));
            }
            (lo, hi) = (prev, v);
            break;
        }
    } else {
        let mut prev = v0;
        loop {
            let v = d.down(prev);
            if v == prev {
                return Ok(knee(&evals, obj));
            }
            let Some(r) = eval_at(&mut evals, &mut trial, v, max_trials).await? else {
                return Ok(knee(&evals, obj));
            };
            if r.feasible {
                (lo, hi) = (v, prev);
                break;
            }
            prev = v;
        }
    }
    // Refine: `lo` is feasible. Under an infeasible `hi`, move `lo` up to a feasible middle;
    // under a feasible (worse) `hi`, only to a middle better than `lo` (S7: 9.0 also moved it to
    // a middle merely better than `hi`, walking away from the best). Else move `hi` down.
    while let Some(m) = d.mid(lo, hi) {
        let Some(r) = eval_at(&mut evals, &mut trial, m, max_trials).await? else {
            break;
        };
        let hi_infeasible = evals.get(&hi).is_some_and(|h| !h.feasible);
        let beats_lo = evals
            .get(&lo)
            .is_some_and(|l| !obj.within(obj.value(l), obj.value(&r)));
        if r.feasible && (hi_infeasible || beats_lo) {
            lo = m;
        } else {
            hi = m;
        }
    }
    Ok(knee(&evals, obj))
}

/// Whether pair `pair` runs its candidate (B) first: a fair coin drawn from `seed`
/// (splitmix64), so the order is random across pairs yet the same for the same run.
fn candidate_first(seed: u64, pair: u64) -> bool {
    let mut z = seed.wrapping_add((pair + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) >> 63 == 1
}

/// The `kind` trials of the first `pairs` completed A/B pairs: a trailing trial whose other
/// half did not run (the budget ended between them) is left out of every metric.
fn paired(trials: &[ProveTrial], kind: TrialKind, pairs: u64) -> Vec<&ProveTrial> {
    trials
        .iter()
        .filter(|x| x.kind == kind && x.pair.is_some_and(|p| p < pairs))
        .collect()
}

/// A candidate that throttles beyond `--max-throttle`, and more than the current operator,
/// proves nothing: what it gained on the objective it took from a limit the service enforces
/// (sized or not). The verdict becomes `inconclusive`, with a note saying why. A candidate
/// that throttles less than the baseline (a cap on a hot key that the service still limits)
/// is judged on its objective.
fn throttle_gate(
    verdict: ProveVerdict,
    a_trials: &[&ProveTrial],
    b_trials: &[&ProveTrial],
    max_throttle: f64,
) -> (ProveVerdict, Option<String>) {
    let counts = |ts: &[&ProveTrial]| {
        let requests: u64 = ts.iter().map(|t| t.requests_count).sum();
        let throttled: u64 = ts.iter().map(|t| t.throttled_count).sum();
        (requests as f64, throttled as f64)
    };
    let (nb, tb) = counts(b_trials);
    if nb == 0.0 {
        return (verdict, None);
    }
    let (na, ta) = counts(a_trials);
    let b = tb / nb;
    let a = if na > 0.0 { ta / na } else { 0.0 };
    // The change is what throttles: over the limit, more than the current operator by more
    // than the limit allows, and beyond chance (a one-sided two-proportion test, z > 2).
    // Service errors that follow no load (`lab://noisy`) throttle both alike and pass.
    let z = if na > 0.0 {
        let p = (ta + tb) / (na + nb);
        let se = (p * (1.0 - p) * (1.0 / na + 1.0 / nb)).sqrt();
        if se > 0.0 {
            (b - a) / se
        } else {
            f64::INFINITY
        }
    } else {
        f64::INFINITY
    };
    if b <= max_throttle || b - a <= max_throttle || z <= 2.0 {
        return (verdict, None);
    }
    let note = format!(
        "the changed operator throttled {:.2} % of its requests (the current one {:.2} %), over --max-throttle {:.2} %: the verdict is inconclusive",
        b * 100.0,
        a * 100.0,
        max_throttle * 100.0
    );
    (ProveVerdict::Inconclusive, Some(note))
}

/// The throttled share a sizing trial may reach. A remedy for throttling must bring it under
/// `--max-throttle`; any other remedy must only not add more than `--max-throttle` to what the
/// run already had (503s that follow no load, `lab://noisy`, are not the change's doing).
fn sizing_throttle_limit(run: &crate::model::Run, finding: Option<&str>, max_throttle: f64) -> f64 {
    if matches!(finding, Some("throttling" | "rate_limit")) {
        return max_throttle;
    }
    let r = &run.report;
    let had = r.resilience.throttled_responses as f64 / r.connections.requests.max(1) as f64;
    max_throttle.max(had + max_throttle)
}

/// Mean of a trial metric.
fn mean(ts: &[&ProveTrial], f: impl Fn(&ProveTrial) -> f64) -> f64 {
    if ts.is_empty() {
        return 0.0;
    }
    ts.iter().map(|t| f(t)).sum::<f64>() / ts.len() as f64
}

/// The smallest change a trial of `ops` operations can resolve, as a log ratio: one operation
/// more or less is `ln(1 + 1/ops)` (2.7 % at 37 operations). Below it, a run of A/A trials that
/// happen to agree exactly says nothing about the noise (0 when nothing was counted).
pub fn count_resolution(ops: u64) -> f64 {
    if ops == 0 {
        0.0
    } else {
        (1.0 + 1.0 / ops as f64).ln()
    }
}

/// The A/A noise floor: the RMS log ratio of consecutive baseline trials (a per-pair spread),
/// at least [`MIN_NOISE_SD`] and at least the trials' count `resolution`
/// ([`count_resolution`]). Fewer than two A/A differences cannot say how precise the floor is,
/// so it is doubled (and with none, it is twice the minimum). [`noise_floor_ab`] with no B.
#[cfg(test)]
fn noise_floor(a: &[f64], resolution: f64) -> f64 {
    noise_floor_ab(a, &[], resolution)
}

/// The mean squared log ratio of consecutive trials of one side, with how many ratios it
/// averages: twice that side's trial-to-trial variance (von Neumann), robust to a slow drift.
fn successive_msd(x: &[f64]) -> (f64, usize) {
    let d: Vec<f64> = x
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0)
        .map(|w| (w[1] / w[0]).ln())
        .collect();
    let msd = if d.is_empty() {
        0.0
    } else {
        d.iter().map(|x| x * x).sum::<f64>() / d.len() as f64
    };
    (msd, d.len())
}

/// The noise floor of a pair's log ratio `ln(b/a)`: its variance is A's trial-to-trial
/// variance plus B's, each estimated from that side's consecutive trials
/// ([`successive_msd`]: `σ² ≈ MSSD / 2`), so `floor² = (MSSD_A + MSSD_B) / 2`. Until B has
/// two trials it is assumed as noisy as A (an A/A pair: `floor² = MSSD_A`, [`noise_floor`]).
/// W8: 9.0 took the A/A spread alone, which overstates a pair's spread by up to √2 when the
/// change steadies B (a cap that ends the throttling of a throttled baseline). At least
/// [`MIN_NOISE_SD`] and the count `resolution`, and doubled while A has fewer than two
/// differences.
fn noise_floor_ab(a: &[f64], b: &[f64], resolution: f64) -> f64 {
    let (msd_a, na) = successive_msd(a);
    let (msd_b, nb) = successive_msd(b);
    let var = if nb == 0 {
        msd_a
    } else {
        (msd_a + msd_b) / 2.0
    };
    let floor = var.sqrt().max(MIN_NOISE_SD).max(resolution);
    if na < 2 { 2.0 * floor } else { floor }
}

/// The objective's relative change (candidate vs baseline) over the interleaved pairs: the
/// mean paired log ratio ± t(0.975, n − 1) σ/√n (Student's t: few pairs give a wide
/// interval), where σ is the larger of the pairs' own spread and the A/A noise floor (the
/// spread the sequential test assumes), as `exp(·) − 1` — never below −100 %.
fn objective_interval(obj: ProveObjective, a: &[f64], b: &[f64], floor: f64) -> Interval {
    let d: Vec<f64> = a
        .iter()
        .zip(b)
        .map(|(x, y)| obj.signed_log_ratio(*x, *y))
        .collect();
    if d.is_empty() {
        return Interval::default();
    }
    let n = d.len() as f64;
    let m = d.iter().sum::<f64>() / n;
    let sd = if d.len() > 1 {
        (d.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
    } else {
        0.0
    };
    let h = crate::analysis::verdict::t975(n - 1.0) * sd.max(floor) / n.sqrt();
    // Back to the metric's own direction (p99: lower is better, so the sign flips).
    let (est, lo, hi) = match obj {
        ProveObjective::Goodput => (m, m - h, m + h),
        ProveObjective::P99 => (-m, -m - h, -m + h),
    };
    Interval {
        estimate: est.exp() - 1.0,
        low: lo.exp() - 1.0,
        high: hi.exp() - 1.0,
    }
}

/// The objective's interval after each A/B pair, as the prove saw it grow (the convergence
/// chart): the same estimate and interval as the verdict's, over the first 1, 2, … n pairs,
/// with the A/A noise floor known at that point. `a` and `b` are the paired objective values;
/// `ops[k]` is the fewest operations a trial of the first `k + 1` pairs counted (missing: no
/// resolution floor).
pub fn convergence(obj: ProveObjective, a: &[f64], b: &[f64], ops: &[u64]) -> Vec<Converged> {
    let n = a.len().min(b.len());
    (1..=n)
        .map(|k| {
            let res = ops.get(k - 1).map(|o| count_resolution(*o)).unwrap_or(0.0);
            let floor = noise_floor_ab(&a[..k], &b[..k], res);
            Converged {
                interval: objective_interval(obj, &a[..k], &b[..k], floor),
                noise_floor_ratio: floor,
            }
        })
        .collect()
}

/// The objective after some pairs (see [`convergence`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Converged {
    /// Relative change of the objective (candidate vs baseline) with its 95 % interval.
    pub interval: Interval,
    /// The A/A noise floor (a per-pair log-ratio spread).
    pub noise_floor_ratio: f64,
}

/// The secondary metrics of the A/B trials.
fn secondary(
    obj: ProveObjective,
    a: &[&ProveTrial],
    b: &[&ProveTrial],
    floor: f64,
) -> Vec<SecondaryMetric> {
    let mut v = Vec::new();
    let positive = |metric: &str, f: fn(&ProveTrial) -> f64| SecondaryMetric {
        metric: metric.to_owned(),
        baseline: mean(a, f),
        candidate: mean(b, f),
        change: log_ratio(
            &a.iter().map(|t| f(t)).collect::<Vec<_>>(),
            &b.iter().map(|t| f(t)).collect::<Vec<_>>(),
            true,
            floor,
        ),
    };
    match obj {
        ProveObjective::Goodput => v.push(positive("p99_ms", |t| t.p99_ms)),
        ProveObjective::P99 => v.push(positive("goodput_ops_per_s", |t| t.goodput_ops_per_s)),
    }
    let sum =
        |ts: &[&ProveTrial], f: fn(&ProveTrial) -> u64| ts.iter().map(|t| f(t)).sum::<u64>() as f64;
    let (ka, na) = (sum(a, |t| t.throttled_count), sum(a, |t| t.requests_count));
    let (kb, nb) = (sum(b, |t| t.throttled_count), sum(b, |t| t.requests_count));
    v.push(SecondaryMetric {
        metric: "throttled_request_ratio".into(),
        baseline: ka / na.max(1.0),
        candidate: kb / nb.max(1.0),
        change: ratio_difference(ka, na, kb, nb),
    });
    let err = |ts: &[&ProveTrial]| {
        ts.iter()
            .map(|t| t.error_ratio * t.ops_count as f64)
            .sum::<f64>()
    };
    let (ea, oa) = (err(a), sum(a, |t| t.ops_count));
    let (eb, ob) = (err(b), sum(b, |t| t.ops_count));
    v.push(SecondaryMetric {
        metric: "error_ratio".into(),
        baseline: ea / oa.max(1.0),
        candidate: eb / ob.max(1.0),
        change: ratio_difference(ea.round(), oa, eb.round(), ob),
    });
    v
}

/// Wall time a trial adds to its plan's ETA: opening the session, saving the trial run.
const TRIAL_OVERHEAD_S: f64 = 0.5;

/// The warmup of the baseline trials (A): the trial warmup, without the candidate's long one
/// (a tail-cut change warms its own layer; the baseline has none to warm).
fn baseline_warmup(base: &Profile, change: &Change) -> Duration {
    let w = base.workload.warmup.get();
    if change.long_warmup {
        crate::config::defaults().prove.trial_warmup.get().min(w)
    } else {
        w
    }
}

/// A redacted `PATH=VALUE` for a command line: a secret is read from the environment when the
/// command runs (`options.secret_access_key=${SECRET_ACCESS_KEY}`), and the pair is quoted.
fn shell_edit(redacted: &str) -> String {
    let c = match redacted.split_once('=') {
        Some((k, "<redacted>")) => {
            let var = k.rsplit('.').next().unwrap_or(k).to_ascii_uppercase();
            format!("{k}=${{{var}}}")
        }
        _ => redacted.to_owned(),
    };
    crate::model::spec::shell_quote(&c)
}

/// `oprobe prove RUN [FINDING] [--with …] [--objective p99]`: the command of a request.
fn prove_command(req: &ProveRequest, run: &Run) -> String {
    let q = crate::model::spec::shell_quote;
    let mut c = format!("oprobe prove {}", q(run.id()));
    if let Some(f) = &req.finding {
        c += &format!(" {}", q(f));
    }
    for w in &req.with {
        let shown = match w.split_once('=') {
            Some((k, v)) => crate::operator::redacted_edit(k.trim(), v.trim()),
            None => w.clone(),
        };
        c += &format!(" --with {}", shell_edit(&shown));
    }
    if req.objective != ProveObjective::Goodput {
        c += &format!(" --objective {}", req.objective.label());
    }
    if let Some(b) = req.budget.as_deref().or(req.safety.max_duration.as_deref()) {
        c += &format!(" --max-duration {}", q(b));
    }
    c
}

/// The one wall-time budget of a proof: `budget`, else `safety.max_duration` (the CLI's
/// `--max-duration`, whose older spelling is `--budget`).
fn proof_budget(req: &ProveRequest) -> Result<Option<Duration>> {
    Ok(req
        .budget
        .as_deref()
        .or(req.safety.max_duration.as_deref())
        .map(crate::input::duration)
        .transpose()?)
}

/// The hint of a refusal to prove a production trace: the run to make instead, on the service
/// the trace recorded (`oprobe run s3://bucket/root`), when the file names it.
fn trace_hint(r: &crate::model::Report) -> String {
    match crate::model::trace_file::run_command(r) {
        // With its endpoint when the trace's requests went to a custom one (K8).
        Some(cmd) => format!(
            "prove needs a run oprobe drove: profile the same service with `{cmd}`, then \
             prove that run"
        ),
        None => "prove needs a run oprobe drove: profile the operator with `oprobe run URL`, \
                 then prove that run"
            .to_owned(),
    }
}

/// `oprobe prove`.
/// A run that cannot be judged on performance (invalid, or partial and too short: its
/// `invalid_run` finding) is no baseline: its findings are leads, not remedies to size.
fn refuse_unjudged(run: &Run) -> Result<()> {
    let r = &run.report;
    let v = crate::analysis::findings::run_validity(r);
    let why = v.unjudged(r).or_else(|| {
        r.findings
            .iter()
            .find(|f| f.code == "invalid_run")
            .map(|f| f.cause.clone())
    });
    let Some(why) = why else {
        return Ok(());
    };
    Err(OprobeError::new(
        ErrorCode::UsageInvalidArgument,
        format!(
            "run {} cannot be judged on performance: {}",
            run.id(),
            why.trim_start_matches("The run cannot be judged on performance: ")
                .trim_end_matches('.')
        ),
    )
    .with_hint(format!(
        "profile the operator again until the run completes: oprobe run {}",
        crate::model::spec::shell_quote(&r.spec.operator_ref)
    ))
    .into())
}

pub async fn prove(ctx: &Ctx, req: ProveRequest, cancel: Cancel) -> Result<ProveOutcome> {
    let run = ctx.store().load(&req.run)?;
    if run.report.is_trace() {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!(
                "`{}` is a production trace: there is no workload to replay",
                req.run
            ),
        )
        .with_hint(trace_hint(&run.report))
        .into());
    }
    if run.report.run.parent_id.is_some() {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("run {} is a trial of a prove", run.id()),
        )
        .with_hint("prove a run you made: oprobe show --list")
        .into());
    }
    refuse_unjudged(&run)?;
    let defaults = crate::config::defaults().prove;
    let max_throttle = req
        .max_throttle_ratio
        .or(run.report.spec.profile.sla.max_throttle_rate)
        .unwrap_or(defaults.max_throttle_ratio);
    let change = choose_change(&run, &req)?;
    // A `--save` that cannot be written fails now, not after the trials.
    if let Some(dest) = &req.save {
        ctx.app().check_save(
            dest,
            &run.report.spec.profile.operator,
            &run.report.spec.operator_ref,
        )?;
    }
    // The base profile, with credentials resolved again, on a short trial window.
    let mut base = super::profile::spec_profile(ctx, run.report.spec.clone())?;
    let trial_window = match &req.trial_duration {
        Some(d) => crate::input::positive_duration(d)?,
        None => defaults.trial_duration.get(),
    }
    .min(base.workload.duration.get().max(Duration::from_millis(500)));
    base.workload.duration = crate::util::HumanDuration(trial_window);
    let mut warmup = defaults
        .trial_warmup
        .get()
        .min(base.workload.warmup.get().max(Duration::from_millis(100)));
    if change.long_warmup {
        warmup = warmup.max(TAIL_CUT_WARMUP);
    }
    base.workload.warmup = crate::util::HumanDuration(warmup);
    base.workload.max_ops = None;
    // Validate the change on the base before anything runs.
    let probe_sets = sets_at(
        &change,
        change.sized.as_ref().map(|s| s.domain.clamp(s.start)),
        &run,
    );
    let mut changed = base.operator.clone();
    changed.apply_edits(&probe_sets)?;
    let budget = proof_budget(&req)?;
    let estimate = {
        let rails = super::run::rails(ctx, &base, &req.safety)?;
        let mut plan = rails.plan(&base);
        // The trials' warmup is prove's choice (tail-cut's blind start), not the user's (U6).
        if let Some(w) = crate::guard::warmup_warning(&base) {
            plan.warnings.retain(|x| *x != w);
        }
        let sizing = match &change.sized {
            Some(s) if s.unmeasured.is_none() => (defaults.max_sizing_trials as u64, 2),
            _ => (0, 0),
        };
        let max_pairs = req.pairs.clamp(MIN_DECISIVE_PAIRS, 12) as u64;
        let min_pairs = (defaults.min_pairs.max(MIN_DECISIVE_PAIRS) as u64).min(max_pairs);
        // A candidate that needs a long warmup takes longer on its first trial, then reuses
        // its warmed TailCutLayer across the remaining pairs.
        let a_eta = plan.eta_s - plan.warmup_s + baseline_warmup(&base, &change).as_secs_f64();
        let tc_reusable = change.long_warmup
            && changed
                .layers
                .tail_cut
                .as_ref()
                .map(|t| t.window.get())
                .unwrap_or(Duration::from_secs(60))
                >= Duration::from_secs(20);
        let b_warm_eta = if tc_reusable { a_eta } else { plan.eta_s };
        let estimate = ProofEstimate::new(
            prove_command(&req, &run),
            probe_sets
                .iter()
                .map(|(k, v)| crate::operator::redacted_edit(k, v))
                .collect(),
            sizing,
            (min_pairs, max_pairs),
            a_eta + TRIAL_OVERHEAD_S,
            plan.eta_s + TRIAL_OVERHEAD_S,
            b_warm_eta + TRIAL_OVERHEAD_S,
            budget,
        );
        if req.safety.dry_run {
            return Ok(ProveOutcome::Plan(Box::new(super::run::PlanOutput {
                plan,
                proof: Some(estimate),
            })));
        }
        estimate
    };
    let prove_id = RunId::new().to_string();
    let mut safety = req.safety.clone();
    // The budget also bounds the trials themselves (with room for the last one's cleanup).
    safety.max_duration = budget.map(|b| format!("{}ms", (b.as_millis() as u64) * 2));
    let guard = super::run::guard_for(ctx, &base, &super::run::budgets(ctx, &safety)?);
    let target_name =
        crate::operator::resolve::resolve(&run.report.spec.operator_ref, &*ctx.config()?)
            .ok()
            .and_then(|(_, s)| s.name());
    let short_warmup = baseline_warmup(&base, &change);
    // Until the prove result is saved, its trial runs belong to nothing: an interruption, an
    // error or a dropped future deletes them, or no `show --rm` could ever reach them.
    let mut orphans = OrphanTrials {
        store: ctx.store(),
        prove_id: prove_id.clone(),
        armed: true,
    };
    let mut t = Trials {
        ctx,
        run: &run,
        base: base.clone(),
        prove_id: prove_id.clone(),
        safety,
        guard,
        cancel: cancel.clone(),
        deadline: budget.map(|b| Instant::now() + b),
        per_trial: trial_window + short_warmup,
        per_cold_tail_cut: trial_window + short_warmup.max(TAIL_CUT_WARMUP),
        short_warmup,
        warmed_baseline_tc: None,
        warmed_candidate_tc: None,
        max_throttle: sizing_throttle_limit(&run, change.finding.as_deref(), max_throttle),
        max_error: run.report.sla.max_error_ratio,
        out: Vec::new(),
        target_name,
        breach: None,
    };
    let obj = req.objective;
    ctx.out.info(format!(
        "{} {} · {} · run {} · prove {}",
        bold("prove"),
        change.finding.as_deref().unwrap_or("change"),
        obj.label(),
        run.id(),
        prove_id
    ));
    ctx.out.info(dim(&estimate.line()));
    // Step 2: size.
    let mut notes = change.notes.clone();
    let mut sizing: Option<Sizing> = None;
    let mut value: Option<u64> = None;
    if let Some(s) = &change.sized {
        let (v, feasible, unmeasured) = match &s.unmeasured {
            Some(why) => (s.domain.clamp(s.start), true, Some(why.clone())),
            None => {
                let (v, f) = size(&mut t, &change, s, obj, defaults.max_sizing_trials).await?;
                (v, f, None)
            }
        };
        if !feasible {
            notes.push(format!(
                "no value met --max-throttle {:.2} %: the least throttled one is tested",
                max_throttle * 100.0
            ));
        }
        value = Some(v);
        sizing = Some(Sizing {
            path: s.path.clone(),
            value: s.domain.fmt(v),
            start: format!(
                "{} ({})",
                s.domain.fmt(s.domain.clamp(s.start)),
                s.start_why
            ),
            feasible,
            unmeasured,
        });
    }
    let sets = sets_at(&change, value, &run);
    // Step 3: interleaved A/B with the sequential test.
    let max_pairs = req.pairs.clamp(MIN_DECISIVE_PAIRS, 12);
    let min_pairs = defaults.min_pairs.max(MIN_DECISIVE_PAIRS);
    let mut a_vals: Vec<f64> = Vec::new();
    let mut b_vals: Vec<f64> = Vec::new();
    let mut pairs = 0u64;
    let mut decision = SprtDecision::Continue;
    // The fewest operations a paired trial counted: the objective's resolution.
    let mut min_ops = u64::MAX;
    let candidate_value = sizing.as_ref().map(|s| s.value.clone());
    let seed = run.report.spec.seed;
    notes.push(if change.long_warmup {
        format!(
            "the A/B order of each pair is drawn from the run's seed ({seed}); the changed operator's TailCutLayer keeps its warmed state from one of its trials to the next, as a long-lived operator's does"
        )
    } else {
        format!("the A/B order of each pair is drawn from the run's seed ({seed})")
    });
    while (pairs as usize) < max_pairs {
        let need_pair = t.expected_duration(&[], TrialKind::Baseline)
            + t.expected_duration(&sets, TrialKind::Candidate);
        if !t.has_time_for(need_pair) {
            break;
        }
        // The order within the pair is drawn from the run's seed (W11): a drift that favours
        // whichever trial runs first or second is spread over both.
        let order = if candidate_first(seed, pairs) {
            [TrialKind::Candidate, TrialKind::Baseline]
        } else {
            [TrialKind::Baseline, TrialKind::Candidate]
        };
        let (mut a, mut b) = (None, None);
        for (i, kind) in order.into_iter().enumerate() {
            // `need_pair` already reserved time for both halves before the first started; run
            // the second to complete the pair unless the first ran past the deadline, breached
            // a guard, or was cancelled.
            if i == 1 && !t.has_time_for(Duration::ZERO) {
                break;
            }
            let trial = if kind == TrialKind::Baseline {
                t.try_trial(&[], kind, Some(pairs), None).await?
            } else {
                t.try_trial(&sets, kind, Some(pairs), candidate_value.clone())
                    .await?
            };
            let Some(trial) = trial else { break };
            if kind == TrialKind::Baseline {
                a = Some(trial);
            } else {
                b = Some(trial);
            }
        }
        let (Some(a), Some(b)) = (a, b) else {
            break;
        };
        a_vals.push(obj.value(&a));
        b_vals.push(obj.value(&b));
        min_ops = min_ops.min(a.ops_count).min(b.ops_count);
        pairs += 1;
        let floor = noise_floor_ab(&a_vals, &b_vals, count_resolution(min_ops));
        let mut sprt = Sprt::new((2.0 * floor).max(MIN_EFFECT), floor, 0.05, 0.10);
        for (x, y) in a_vals.iter().zip(&b_vals) {
            sprt.push(obj.signed_log_ratio(*x, *y));
        }
        decision = sprt.decision(min_pairs);
        // Stop on a decision the interval agrees with (an early "better" whose interval still
        // includes no change keeps sampling).
        let i = objective_interval(obj, &a_vals, &b_vals, floor);
        let agrees = match decision {
            SprtDecision::Better | SprtDecision::Worse => i.low > 0.0 || i.high < 0.0,
            SprtDecision::Same => true,
            SprtDecision::Continue => false,
        };
        if agrees {
            break;
        }
    }
    // Ctrl-C between two trials stops the loop like one during a trial: the prove is
    // interrupted (exit 130, its trials deleted), not a result of the pairs that happened to run.
    if cancel.is_cancelled() && t.breach.is_none() {
        return Err(OprobeError::new(ErrorCode::RunInterrupted, "prove interrupted").into());
    }
    // No trial ran at all (the wall-time budget): nothing to show. A breach saved its trial,
    // so the prove is saved too.
    if pairs == 0 && t.out.is_empty() && t.breach.is_none() {
        return Err(t.breach.take().unwrap_or_else(|| {
            OprobeError::new(
                ErrorCode::SafetyBudgetExceeded,
                "the budget ran out before one pair of trials could run",
            )
            .with_hint("raise --max-duration")
            .into()
        }));
    }
    // Trials ran but a budget stopped the prove early: the result reads what was measured,
    // and is saved, so its trials stay attached (`show --rm PROVE` removes them).
    let budget_exceeded = t.breach.is_some() || pairs == 0;
    if let Some(e) = &t.breach {
        notes.push(format!(
            "a budget stopped the prove after {pairs} pair{} of trials ({e:#}): the verdict reads what was measured",
            if pairs == 1 { "" } else { "s" }
        ));
    } else if pairs == 0 {
        notes.push("the budget ran out before one pair of trials could run".to_owned());
    }
    // Step 4: decide.
    let floor = noise_floor_ab(
        &a_vals,
        &b_vals,
        if pairs == 0 {
            0.0
        } else {
            count_resolution(min_ops)
        },
    );
    let delta = objective_interval(obj, &a_vals, &b_vals, floor);
    let better = |x: f64| match obj {
        ProveObjective::Goodput => x > 0.0,
        ProveObjective::P99 => x < 0.0,
    };
    let excludes_zero = delta.low > 0.0 || delta.high < 0.0;
    let too_few = (pairs as usize) < MIN_DECISIVE_PAIRS;
    let verdict = match decision {
        _ if too_few || !excludes_zero => ProveVerdict::Inconclusive,
        SprtDecision::Better => ProveVerdict::Improved,
        SprtDecision::Worse => ProveVerdict::Worse,
        _ if (1.0 + delta.estimate).max(1e-9).ln().abs() > floor => {
            if better(delta.estimate) {
                ProveVerdict::Improved
            } else {
                ProveVerdict::Worse
            }
        }
        _ => ProveVerdict::Inconclusive,
    };
    if too_few {
        notes.push(format!(
            "the budget allowed only {pairs} pair{} of trials: at least {MIN_DECISIVE_PAIRS} are needed to decide",
            if pairs == 1 { "" } else { "s" }
        ));
    } else if decision == SprtDecision::Continue {
        notes.push(format!(
            "the sequential test did not decide within {pairs} pairs: the verdict reads the interval"
        ));
    }
    let a_trials = paired(&t.out, TrialKind::Baseline, pairs);
    let b_trials = paired(&t.out, TrialKind::Candidate, pairs);
    let (verdict, throttle_note) = throttle_gate(verdict, &a_trials, &b_trials, max_throttle);
    notes.extend(throttle_note);
    let secondary = secondary(obj, &a_trials, &b_trials, floor);
    let base_spec: &OperatorSpec = &run.report.spec.profile.operator;
    // What the result shows and stores: the operator as its file gives it (a relative
    // `http.ca_cert` stays relative, not this machine's absolute path).
    let shown_base = base_spec.as_given(&run.report.spec.operator_ref);
    let mut changed = shown_base.clone();
    changed
        .apply_edits(&sets)
        .context("apply the proven change")?;
    let diff = crate::operator::diff::diff(&shown_base, &changed);
    let r = &run.report;
    let environment = if r.spec.operator_ref.starts_with("lab://") {
        format!("{} (local lab)", r.spec.operator_ref)
    } else {
        match &r.target.endpoint {
            Some(e) => format!("{} at {e}", r.target.display),
            None => r.target.display.clone(),
        }
    };
    let raw_edits: Vec<String> = sets.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let save_now = req
        .save
        .as_ref()
        .filter(|_| verdict == ProveVerdict::Improved);
    if let Some(dest) = &req.save
        && save_now.is_none()
    {
        let v_label = match verdict {
            ProveVerdict::Improved => "improved",
            ProveVerdict::Worse => "worse",
            ProveVerdict::Inconclusive => "inconclusive",
        };
        notes.push(format!(
            "--save `{dest}` skipped: verdict is {v_label}, not improved"
        ));
    }
    let run_command = profile_command(&r.spec, &sets);
    let mut out = ProveOutput {
        schema: PROVE_SCHEMA.to_owned(),
        prove_id: prove_id.clone(),
        run_id: run.id().to_owned(),
        operator_ref: r.spec.operator_ref.clone(),
        operator: changed.redacted(),
        environment,
        finding: change.finding.clone(),
        // Secret option values are redacted (the result is printed and stored).
        change: sets
            .iter()
            .map(|(k, v)| crate::operator::redacted_edit(k, v))
            .collect(),
        sizing,
        objective: obj,
        verdict,
        objective_delta: delta,
        noise_floor_ratio: floor,
        pairs_count: pairs,
        max_throttle_ratio: max_throttle,
        secondary,
        trials: t.out.clone(),
        diff,
        budget_exceeded,
        saved_to: None,
        run_command,
        notes,
    };
    // The proof first: a `--save` that fails after it must not lose it.
    let store_prove = |out: &ProveOutput| -> Result<bool> {
        match ctx
            .store()
            .save_prove(&prove_id, &serde_json::to_value(out)?)
        {
            Ok(_) => Ok(true),
            Err(e) => {
                ctx.out.warn(format!("cannot save the prove result: {e:#}"));
                Ok(false)
            }
        }
    };
    // Once stored, its trials belong to the prove (`show --rm PROVE` removes them).
    if store_prove(&out)? {
        orphans.armed = false;
    }
    if let Some(dest) = save_now {
        match ctx
            .app()
            .save_operator(dest, base_spec, &run.report.spec.operator_ref, &raw_edits)
        {
            Ok(saved) => out.saved_to = Some(saved.to),
            Err(e) => {
                ctx.out.warn(format!("--save `{dest}` failed: {e:#}"));
                out.notes
                    .push(format!("--save `{dest}` failed: {e:#}; the proof is kept"));
            }
        }
        // Stored now, if not before: the trials belong to it.
        if store_prove(&out)? {
            orphans.armed = false;
        }
    }
    Ok(ProveOutcome::Proved(Box::new(out)))
}

/// `oprobe run` of the run's workload (its preset, `-c`, `-d`, `--existing`, …) on the operator
/// with `sets` applied: the command that profiles the proven operator as the run did.
fn profile_command(spec: &crate::model::RunSpec, sets: &[(String, String)]) -> Option<String> {
    // The run's own command, then the change as `--set` pairs (the last value of a path wins;
    // a secret value given as an environment variable of its name, as `next` always did).
    let mut args = spec.args();
    for (k, v) in sets {
        let e = crate::operator::redacted_edit(k, v);
        let e = match e.split_once('=') {
            Some((k, crate::model::spec::REDACTED)) => {
                let var = k.rsplit('.').next().unwrap_or(k).to_ascii_uppercase();
                format!("{k}=${{{var}}}")
            }
            _ => e,
        };
        args.push("--set".into());
        args.push(e);
    }
    Some(crate::model::spec::command_line(&args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial(v: f64, p99: f64, feasible: bool) -> ProveTrial {
        ProveTrial {
            kind: TrialKind::Sizing,
            pair: None,
            value: None,
            run_id: String::new(),
            goodput_ops_per_s: v,
            p99_ms: p99,
            ops_count: 100,
            requests_count: 100,
            throttled_count: if feasible { 0 } else { 10 },
            throttled_request_ratio: if feasible { 0.0 } else { 0.1 },
            error_ratio: 0.0,
            feasible,
        }
    }

    /// S7: a value best at its start (a 20 ms retry backoff) was sized 20 → 40 → 28 → 33 ms,
    /// every step after the first worse than 20. The first step up that does no better ends
    /// the climb, and the search looks below the start instead.
    #[tokio::test]
    async fn sizing_stops_when_the_first_step_up_is_worse() {
        struct F(fn(u64) -> f64, Vec<u64>);
        impl Measure for F {
            async fn measure(&mut self, v: u64) -> Result<Option<ProveTrial>> {
                self.1.push(v);
                Ok(Some(trial((self.0)(v), 10.0, true)))
            }
        }
        let d = Domain::Millis { min: 20, max: 500 };
        // Goodput falls as the backoff grows: the best is the smallest value.
        let goodput = |v: u64| 1000.0 / (1.0 + v as f64 / 100.0);
        let mut f = F(goodput, Vec::new());
        let got = search(d, 20, ProveObjective::Goodput, 8, &mut f).await;
        assert_eq!(got.unwrap(), (20, true));
        assert_eq!(f.1, [20, 40]);
        // From a start above the best, it descends while smaller values do better.
        let mut f = F(goodput, Vec::new());
        let got = search(d, 80, ProveObjective::Goodput, 8, &mut f).await;
        assert_eq!(got.unwrap(), (20, true));
        assert_eq!(f.1, [80, 160, 40, 20]);
        // A value that keeps improving still climbs, then bisects below the worse step.
        let mut f = F(|v| 1000.0 - 5.0 * (v as f64 - 100.0).abs(), Vec::new());
        let (v, _) = search(d, 20, ProveObjective::Goodput, 8, &mut f)
            .await
            .unwrap();
        assert_eq!(&f.1[..4], [20, 40, 80, 160]);
        assert!((80..=126).contains(&v), "{v}: {:?}", f.1);
    }

    /// W11: the A/B order of the pairs is a fair coin from the run's seed: both orders occur,
    /// and the same seed gives the same orders.
    #[test]
    fn the_ab_order_is_drawn_from_the_seed() {
        let orders = |seed: u64| {
            (0..12)
                .map(|p| candidate_first(seed, p))
                .collect::<Vec<_>>()
        };
        for seed in [0, 1, 42, u64::MAX] {
            let o = orders(seed);
            assert!(
                o.iter().any(|b| *b) && o.iter().any(|b| !*b),
                "{seed}: {o:?}"
            );
            assert_eq!(o, orders(seed));
        }
        assert_ne!(orders(1), orders(2));
        let firsts = (0..1000u64).filter(|s| candidate_first(*s, 0)).count();
        assert!((400..=600).contains(&firsts), "{firsts}");
    }

    fn output(pairs: u64, verdict: ProveVerdict) -> ProveOutput {
        let a = OperatorSpec::from_url("s3://b/x");
        let mut b = a.clone();
        b.set("layers.concurrent_limit.http", "24").unwrap();
        ProveOutput {
            schema: PROVE_SCHEMA.to_owned(),
            prove_id: "p".into(),
            run_id: "r".into(),
            operator_ref: "lab://demo".into(),
            operator: b.redacted(),
            environment: "lab://demo (local lab)".into(),
            finding: Some("throttling".into()),
            change: vec!["layers.concurrent_limit.http=24".into()],
            sizing: None,
            objective: ProveObjective::Goodput,
            verdict,
            objective_delta: Interval {
                estimate: 0.3,
                low: 0.2,
                high: 0.4,
            },
            noise_floor_ratio: 0.02,
            pairs_count: pairs,
            max_throttle_ratio: 0.001,
            secondary: Vec::new(),
            trials: Vec::new(),
            diff: crate::operator::diff::diff(&a, &b),
            budget_exceeded: pairs == 0,
            saved_to: None,
            run_command: None,
            notes: vec!["the budget ran out before one pair of trials could run".into()],
        }
    }

    /// 6.3.0 proved a run interrupted after 4 s. A run that cannot be judged is refused.
    #[test]
    fn prove_refuses_a_run_that_cannot_be_judged() {
        let mut run = crate::analysis::simulate::tests::straggler_run();
        run.report.run.window_s = 5.0;
        run.report.spec.operator_ref = "lab://demo".into();
        refuse_unjudged(&run).unwrap();
        crate::model::build::mark_partial(&mut run, crate::model::PartialReason::Interrupted);
        let e = refuse_unjudged(&run).unwrap_err();
        let b = crate::errors::classify(&e, "s3");
        assert_eq!(b.code, ErrorCode::UsageInvalidArgument);
        assert!(
            b.message.contains("cannot be judged") && b.message.contains("interrupted"),
            "{}",
            b.message
        );
        assert!(!b.message.ends_with('.'), "{}", b.message);
        assert_eq!(
            b.hint.as_deref(),
            Some("profile the operator again until the run completes: oprobe run lab://demo")
        );
    }

    /// 6.3.0 said "improved" while the candidate throttled 4.36 % against a 0.10 % limit.
    #[test]
    fn a_candidate_over_max_throttle_is_inconclusive() {
        let b = |requests: u64, throttled: u64| ProveTrial {
            kind: TrialKind::Candidate,
            pair: Some(0),
            requests_count: requests,
            throttled_count: throttled,
            throttled_request_ratio: throttled as f64 / requests as f64,
            ..trial(10.0, 5.0, true)
        };
        let a = [b(1000, 3), b(1000, 3)];
        let a: Vec<&ProveTrial> = a.iter().collect();
        let over = [b(1000, 40), b(1000, 47)];
        let over: Vec<&ProveTrial> = over.iter().collect();
        for v in [ProveVerdict::Improved, ProveVerdict::Worse] {
            let (verdict, note) = throttle_gate(v, &a, &over, 0.001);
            assert_eq!(verdict, ProveVerdict::Inconclusive);
            let note = note.unwrap();
            assert!(
                note.contains("4.35 %") && note.contains("0.30 %") && note.contains("0.10 %"),
                "{note}"
            );
        }
        let within = [b(1000, 1), b(1000, 0)];
        let within: Vec<&ProveTrial> = within.iter().collect();
        assert_eq!(
            throttle_gate(ProveVerdict::Improved, &a, &within, 0.001),
            (ProveVerdict::Improved, None)
        );
        // Less throttled than the baseline (a hot key the service still limits): judged on
        // the objective.
        let hot = [b(1000, 280), b(1000, 290)];
        let hot: Vec<&ProveTrial> = hot.iter().collect();
        assert_eq!(
            throttle_gate(ProveVerdict::Improved, &hot, &over, 0.001),
            (ProveVerdict::Improved, None)
        );
        // Service errors that follow no load throttle both alike: a difference within chance,
        // or within the limit, is not the change's doing.
        let noisy_a = [b(1000, 10), b(1000, 9)];
        let noisy_a: Vec<&ProveTrial> = noisy_a.iter().collect();
        let noisy_b = [b(1000, 13), b(1000, 12)];
        let noisy_b: Vec<&ProveTrial> = noisy_b.iter().collect();
        assert_eq!(
            throttle_gate(ProveVerdict::Improved, &noisy_a, &noisy_b, 0.001),
            (ProveVerdict::Improved, None)
        );
        // No candidate ran: nothing to gate.
        assert_eq!(
            throttle_gate(ProveVerdict::Inconclusive, &a, &[], 0.001),
            (ProveVerdict::Inconclusive, None)
        );
    }

    #[test]
    fn a_proof_with_no_pair_says_not_measured_and_prints_no_diff() {
        crate::report::style::set(false);
        let none = output(0, ProveVerdict::Inconclusive).text();
        assert!(none.starts_with("not measured on goodput"), "{none}");
        assert!(!none.contains("ConcurrentLimitLayer"), "{none}");
        assert!(!none.contains("op.toml"), "{none}");
        assert!(none.contains("change not tested: layers.concurrent_limit.http=24"));
        let done = output(4, ProveVerdict::Improved).text();
        assert!(done.starts_with("improved on goodput: +30.0 %"), "{done}");
        assert!(done.contains("4 pairs of trials"), "{done}");
        // The interval says what it is (U5).
        assert!(done.contains("+30.0 %, 95 % interval ["), "{done}");
        assert!(done.contains("ConcurrentLimitLayer"), "{done}");
        assert!(
            output(4, ProveVerdict::Worse)
                .text()
                .starts_with("worse on goodput")
        );
    }

    #[test]
    fn the_estimate_covers_the_whole_proof() {
        // 8 sizing trials at most (2 at least), 3–6 pairs, trials of 3 s.
        let e = ProofEstimate::new(
            "oprobe prove r".into(),
            vec!["layers.concurrent_limit.http=24".into()],
            (8, 2),
            (3, 6),
            3.0,
            3.0,
            3.0,
            None,
        );
        assert_eq!((e.trials_min_count, e.trials_max_count), (8, 20));
        assert_eq!((e.eta_min_s, e.eta_max_s), (24.0, 60.0));
        assert_eq!(
            e.line(),
            "8–20 trials (up to 8 sizing, 3–6 pairs of A and B) · ETA 24s – 1m"
        );
        // A long candidate warmup, no sizing, within a budget.
        let e = ProofEstimate::new(
            "oprobe prove r".into(),
            vec!["layers.tail_cut=on".into()],
            (0, 0),
            (3, 6),
            3.0,
            14.0,
            14.0,
            Some(Duration::from_secs(90)),
        );
        assert_eq!(e.eta_min_s, 51.0);
        assert_eq!(e.eta_max_s, 90.0);
        assert!(
            e.line().ends_with("ETA 51s – 1m 30s (budget 1m 30s)"),
            "{}",
            e.line()
        );
        // Only the pairs that fit: 5 of 17 s in 90 s (K19: 7.0 planned 6).
        assert_eq!((e.pairs_min_count, e.pairs_max_count), (3, 5));
        assert_eq!(e.trials_max_count, 10);
        assert!(
            e.line().contains("the budget fits 10 of 12"),
            "{}",
            e.line()
        );
        // Reusing a warmed TailCutLayer warms only on the first pair: 17 s + 5 × 6 s = 47 s.
        let reused = ProofEstimate::new(
            "oprobe prove r".into(),
            vec!["layers.tail_cut=on".into()],
            (0, 0),
            (3, 6),
            3.0,
            14.0,
            3.0,
            None,
        );
        assert_eq!((reused.eta_min_s, reused.eta_max_s), (29.0, 47.0));
    }

    /// 7.0.0 planned 6–20 trials under a budget that fits one pair (K19).
    #[test]
    fn the_estimate_plans_only_what_fits_in_the_budget() {
        let e = ProofEstimate::new(
            "oprobe prove r".into(),
            vec!["layers.concurrent_limit.http=24".into()],
            (8, 2),
            (3, 6),
            3.0,
            3.0,
            3.0,
            Some(Duration::from_secs(12)),
        );
        // 2 sizing trials (6 s), then one pair (6 s): 4 trials at most.
        assert_eq!(e.trials_max_count, 4, "{e:?}");
        assert_eq!(e.pairs_max_count, 1);
        assert!(e.eta_max_s <= 12.0);
        let line = e.line();
        assert!(line.contains("the budget fits 4 of 20"), "{line}");
        assert!(line.contains("raise --max-duration"), "{line}");
        // Without a budget, nothing is cut.
        let free = ProofEstimate::new(
            "oprobe prove r".into(),
            vec![],
            (8, 2),
            (3, 6),
            3.0,
            3.0,
            3.0,
            None,
        );
        assert_eq!(free.unbudgeted_trials_max_count, None);
    }

    #[test]
    fn the_knee_is_the_smallest_value_near_the_best() {
        let mut e = BTreeMap::new();
        e.insert(4, trial(400.0, 10.0, true));
        e.insert(7, trial(700.0, 10.0, true));
        e.insert(8, trial(800.0, 10.0, true));
        e.insert(9, trial(805.0, 11.0, true));
        e.insert(16, trial(300.0, 900.0, false));
        assert_eq!(knee(&e, ProveObjective::Goodput), (8, true));
        // p99: lowest within 3 %.
        assert_eq!(knee(&e, ProveObjective::P99).0, 4);
        // Nothing feasible: the least throttled.
        let mut e = BTreeMap::new();
        e.insert(2, trial(10.0, 10.0, false));
        assert_eq!(knee(&e, ProveObjective::Goodput), (2, false));
    }

    #[test]
    fn domains_step_and_bisect() {
        let d = Domain::Count { min: 1, max: 64 };
        assert_eq!(d.up(1), 2);
        assert_eq!(d.up(40), 64);
        assert_eq!(d.down(1), 1);
        assert_eq!(d.mid(8, 16), Some(12));
        assert_eq!(d.mid(8, 9), None);
        let b = Domain::Bytes {
            min: 1 << 20,
            max: 1 << 30,
        };
        assert_eq!(b.fmt(16 << 20), "16MiB");
        assert!(b.mid(1 << 20, 1 << 21).is_some());
        assert!(b.mid(1 << 20, (1 << 20) + 1000).is_none());
    }

    #[test]
    fn a_one_operation_difference_is_not_a_change() {
        // The macOS CI trials: ~37 operations each, the A/A trials agreeing exactly and the
        // candidates one or two operations short. Without the count resolution the interval
        // excluded no change (−6.0 %, −0.4 %): "worse" from rounding.
        let a = [37.0, 37.0, 37.0, 38.0, 37.0];
        let b = [36.0, 35.0, 37.0, 37.0, 35.0];
        let loose = objective_interval(ProveObjective::Goodput, &a, &b, noise_floor(&a, 0.0));
        assert!(loose.high < 0.0, "{loose:?}");
        let floor = noise_floor(&a, count_resolution(35));
        assert!(floor > MIN_NOISE_SD);
        let i = objective_interval(ProveObjective::Goodput, &a, &b, floor);
        assert!(i.low < 0.0 && i.high > 0.0, "{i:?}");
        // Thousands of operations resolve far below the minimum floor: nothing changes.
        assert_eq!(
            noise_floor(&a, count_resolution(10_000)),
            noise_floor(&a, 0.0)
        );
    }

    #[test]
    fn convergence_ends_on_the_verdicts_interval() {
        let a = [100.0, 102.0, 99.0, 101.0, 100.0];
        let b = [120.0, 121.0, 118.0, 122.0, 119.0];
        let c = convergence(ProveObjective::Goodput, &a, &b, &[]);
        assert_eq!(c.len(), 5);
        let last = c.last().unwrap();
        let floor = noise_floor_ab(&a, &b, 0.0);
        assert_eq!(last.noise_floor_ratio, floor);
        assert_eq!(
            last.interval,
            objective_interval(ProveObjective::Goodput, &a, &b, floor)
        );
        // More pairs, a narrower interval.
        let w = |i: &Interval| i.high - i.low;
        assert!(w(&c[4].interval) < w(&c[1].interval));
        assert!(last.interval.low > 0.0);
    }

    #[test]
    fn the_noise_floor_has_a_minimum() {
        // No A/A difference yet: the minimum, widened.
        assert_eq!(noise_floor(&[100.0], 0.0), 2.0 * MIN_NOISE_SD);
        let f = noise_floor(&[100.0, 120.0, 100.0], 0.0);
        assert!(f > 0.15 && f < 0.2, "{f}");
    }

    /// W8: a steady candidate against a noisy (throttled) baseline: the pair's spread is A's
    /// alone, not the A/A spread of two noisy trials (√2 more).
    #[test]
    fn a_steady_candidate_does_not_double_the_baselines_noise() {
        let a = [100.0, 150.0, 100.0, 150.0];
        let steady = [60.0, 60.0, 60.0, 60.0];
        let aa = noise_floor(&a, 0.0);
        let ab = noise_floor_ab(&a, &steady, 0.0);
        assert!((ab - aa / 2f64.sqrt()).abs() < 1e-12, "{ab} vs {aa}");
        // As noisy as A: the A/A floor.
        assert!((noise_floor_ab(&a, &a, 0.0) - aa).abs() < 1e-12);
        // One B trial says nothing about B: A/A.
        assert_eq!(noise_floor_ab(&a, &[60.0], 0.0), aa);
        // Minimums hold.
        assert_eq!(noise_floor_ab(&[100.0; 4], &[100.0; 4], 0.0), MIN_NOISE_SD);
    }

    #[test]
    fn one_aa_difference_is_not_a_precise_floor() {
        let one = noise_floor(&[100.0, 110.0], 0.0);
        let d = (110.0f64 / 100.0).ln();
        assert!((one - 2.0 * d).abs() < 1e-12, "{one}");
    }

    #[test]
    fn few_pairs_give_a_t_interval() {
        // Three pairs, each +10 % with a little spread: t(0.975, 2) = 4.303, not 1.96.
        let a = [100.0, 100.0, 100.0];
        let b = [109.0, 110.0, 111.0];
        let i = objective_interval(ProveObjective::Goodput, &a, &b, 0.0);
        let d: Vec<f64> = a.iter().zip(&b).map(|(x, y)| (y / x).ln()).collect();
        let m = d.iter().sum::<f64>() / 3.0;
        let sd = (d.iter().map(|x| (x - m).powi(2)).sum::<f64>() / 2.0).sqrt();
        let want = (m + 4.303 * sd / 3f64.sqrt()).exp() - 1.0;
        assert!((i.high - want).abs() < 1e-9, "{i:?} vs {want}");
        // One pair: the widest quantile, whatever the floor.
        let one = objective_interval(ProveObjective::Goodput, &[100.0], &[110.0], 0.02);
        assert!(one.high - one.low > 0.4, "{one:?}");
    }

    #[test]
    fn an_unpaired_baseline_is_left_out_of_the_secondary_metrics() {
        let ab = |kind, pair, throttled: u64| ProveTrial {
            kind,
            pair: Some(pair),
            throttled_count: throttled,
            throttled_request_ratio: throttled as f64 / 100.0,
            ..trial(100.0, 10.0, true)
        };
        let trials = vec![
            trial(50.0, 10.0, true), // sizing
            ab(TrialKind::Baseline, 0, 0),
            ab(TrialKind::Candidate, 0, 0),
            ab(TrialKind::Baseline, 1, 0),
            ab(TrialKind::Candidate, 1, 0),
            // The budget ended before this baseline's candidate ran.
            ab(TrialKind::Baseline, 2, 90),
        ];
        let a = paired(&trials, TrialKind::Baseline, 2);
        let b = paired(&trials, TrialKind::Candidate, 2);
        assert_eq!((a.len(), b.len()), (2, 2));
        let sec = secondary(ProveObjective::Goodput, &a, &b, 0.02);
        let thr = sec
            .iter()
            .find(|m| m.metric == "throttled_request_ratio")
            .unwrap();
        assert_eq!(thr.baseline, 0.0, "{thr:?}");
    }
}
