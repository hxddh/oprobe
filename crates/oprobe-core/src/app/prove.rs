//! `oprobe prove RUN [FINDING] [--with PATH=VALUE]…` (design v6 §8 "Prove"): the only source
//! of numbers.
//!
//! 1. **Choose the change**: the finding's remedy, or `--with`.
//! 2. **Size it**: when the remedy is a size (a limit, a gap, a concurrency), a bracket search
//!    over that one value with short trials picks the objective's knee — the smallest value
//!    within [`KNEE_RATIO`] of the best — subject to `--max-throttle`.
//! 3. **A/B test**: interleaved A/B pairs in a balanced order (blocks of one `AB` and one
//!    `BA`, drawn from the run's seed) with a sequential test ([`AbTest`]) on the objective
//!    against a noise floor learned from each side's trial-to-trial spread, until a decision,
//!    the pair limit or the wall-time budget (`--max-duration`). A trial that ends throttled
//!    is followed by a cool-down ([`cool_down`]), so it does not drain the limit the next
//!    trial — often the other arm — starts against.
//! 4. **Decide**: `improved | worse | inconclusive` on the objective (goodput = successful
//!    operations per second, or p99): whether its sequential, order-adjusted 95 % interval
//!    excludes no change — the one rule every look and the verdict share. A changed operator
//!    that throttles or fails more than the run allows, and more than the current one, is
//!    `inconclusive` whatever its objective says ([`throttle_gate`], [`error_gate`]).
//!    Intervals are log-ratio (never below −100 %) and Wilson/Newcombe for shares; secondary
//!    metrics are shown, never decisive.
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
use crate::analysis::verdict::{
    AbDecision, AbTest, AbVerdict, P99_CLIFF_RATIO, candidate_first, count_resolution, log_ratio,
    noise_floor_ab, p99_on_a_cliff, p99_rank_band, p99_spread, ratio_difference,
};
pub use crate::analysis::verdict::{AbPair, MAX_PAIRS, MIN_DECISIVE_PAIRS};
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
/// The shortest cool-down after a throttled trial, and the longest when the service named no
/// `Retry-After` ([`cool_down`]).
const COOL_DOWN_DEFAULT: (Duration, Duration) =
    (Duration::from_millis(250), Duration::from_secs(2));
/// The longest cool-down, whatever `Retry-After` the service sent.
const COOL_DOWN_MAX: Duration = Duration::from_secs(5);
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

    /// More of the objective is better (goodput), or less is (p99).
    fn higher_is_better(self) -> bool {
        self == Self::Goodput
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
    /// Within `--max-throttle` and the error ratio a run allows (1 %), and, for the remedy of
    /// a `rate_limit` finding, sending no more requests per second than the fitted limit.
    pub feasible: bool,
    /// How far off this trial's p99 can be, as a log ratio: a quarter of the log width of the
    /// latencies at the ranks 0.99·n ± 2·√(0.0099·n) of its n operations (the p99's
    /// order-statistic interval). Above 0.17 those latencies differ by more than 2×: the p99
    /// sits on a cliff. Absent when 0, and in results of 9.2 and earlier.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub p99_spread_ratio: f64,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
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
    /// signed in the metric's own direction (goodput +: more; p99 −: faster). The interval is
    /// sequential (valid whenever the prove stopped) and adjusted for the order of the trials
    /// within a pair; the verdict is whether it excludes no change.
    pub objective_delta: Interval,
    /// The A/A noise floor: the per-pair spread of the objective's log ratio.
    pub noise_floor_ratio: f64,
    /// Interleaved A/B pairs that ran.
    pub pairs_count: u64,
    /// The A/B pairs the prove planned at most (`--pairs`): the interval's sequential boundary
    /// is the one of that plan. Absent in results of 9.1 and earlier, whose interval was the
    /// fixed-sample one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairs_max_count: Option<u64>,
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

    /// The plan this result's interval was computed under: the A/B pairs planned at most.
    /// `None` for a result of 9.1 or earlier, which had no plan: its `objective_delta` is a
    /// fixed-sample interval, and nothing computed from its trials with a plan
    /// ([`convergence`]) reproduces it. A view of such a result shows the stored verdict and
    /// interval as they are; it must not judge the trials again.
    pub fn plan(&self) -> Option<usize> {
        self.pairs_max_count
            .map(|n| (n as usize).clamp(MIN_DECISIVE_PAIRS, MAX_PAIRS))
    }

    /// The A/B pairs to draw the trials of this result with ([`convergence`]): its plan, or
    /// for a result without one ([`Self::plan`]) the default plan, or the pairs it ran if
    /// more. Only [`Self::plan`] says whether the drawing ends on the stored interval.
    pub fn planned_pairs(&self) -> usize {
        self.plan().unwrap_or_else(|| {
            crate::config::prove_defaults::MAX_PAIRS
                .max(self.pairs_count as usize)
                .clamp(MIN_DECISIVE_PAIRS, MAX_PAIRS)
        })
    }

    /// Add a note. Every note of a result goes through here: a note may quote an error of the
    /// storage library or of a save, and those quote URLs as configured, so credentials in a
    /// URL (`user:pass@`, a presigned signature) are removed before the note is printed or
    /// stored.
    fn add_note(&mut self, note: impl AsRef<str>) {
        self.notes
            .push(crate::util::strip_url_credentials(note.as_ref()));
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
                "{} on {}: no pair of trials was judged · {}\n",
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
        // Nothing was compared: there is no changed operator worth a profile yet. The trials
        // that did run are still stored.
        if self.pairs_count == 0 {
            return if self.trials.is_empty() {
                Vec::new()
            } else {
                vec![NextStep::new("its trials", "oprobe show --list --all")]
            };
        }
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

/// What the trials of a proof are expected to take (seconds of wall time).
#[derive(Clone, Copy, Debug, PartialEq)]
struct TrialTimes {
    /// A baseline trial (A).
    a_s: f64,
    /// The first sizing or candidate trial (B), which may have a shared layer to warm.
    b_first_s: f64,
    /// A later one.
    b_warm_s: f64,
    /// The cool-down that follows a trial that ends throttled ([`cool_down`]); 0 when the
    /// base run was not throttled.
    cool_s: f64,
}

impl TrialTimes {
    /// Every trial takes `s`, and none is followed by a cool-down.
    #[cfg(test)]
    fn even(s: f64) -> Self {
        Self {
            a_s: s,
            b_first_s: s,
            b_warm_s: s,
            cool_s: 0.0,
        }
    }
}

/// What the base run says about the wall time of a trial of its workload, beyond the trial's
/// own warm-up and window (K3-2). 9.2 estimated a trial from the plan's model alone: too low
/// when operations sat in retry backoff past the window and by every cool-down (`lab://hotkey`:
/// "23–45 s" for a proof of 54 s), too high on a fast target (a modelled dataset upload and
/// cleanup that took no time: "too few pairs to decide", and then it decided).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BaseTiming {
    /// The wall time the run spent around its warm-up, window and tail: opening the session,
    /// the dataset, cleanup, saving. `None` when its timestamps do not say.
    overhead_s: Option<f64>,
    /// How long its last operations kept draining after the window closed (an operation in a
    /// retry backoff finishes its retries).
    tail_s: f64,
    /// The cool-down a trial that ends as throttled as the run is followed by.
    cool_s: f64,
}

impl BaseTiming {
    /// From the run's own numbers: its wall time `wall_s` (seconds between its start and end
    /// stamps), its warm-up and window, the end of its last operation (seconds from the window
    /// start), and the cool-down `cool` its throttling asks for ([`cool_down`]).
    fn learn(
        wall_s: Option<f64>,
        warmup_s: f64,
        window_s: f64,
        last_op_end_s: f64,
        cool: Duration,
    ) -> Self {
        let tail_s = (last_op_end_s - window_s).max(0.0);
        Self {
            // The stamps are whole seconds, so their difference is right within a second: the
            // least the run can have spent. An estimate that is short is corrected by the
            // first trial; one that is long would keep a pair that fits from ever starting.
            overhead_s: wall_s
                .filter(|w| w.is_finite())
                .map(|w| (w - 1.0 - warmup_s - window_s - tail_s).max(0.0)),
            tail_s,
            cool_s: cool.as_secs_f64(),
        }
    }

    fn of(run: &Run, trial_warmup: Duration) -> Self {
        let r = &run.report;
        // `2026-11-22T10:00:00Z`, as the report writes it.
        let stamp = |s: &str| crate::util::parse_compact_utc(&s.replace(['-', ':'], ""));
        let wall_s = match (stamp(&r.run.started_at), stamp(&r.run.ended_at)) {
            (Some(a), Some(b)) => b.duration_since(a).ok().map(|d| d.as_secs_f64()),
            _ => None,
        };
        let last_op_end_s = run
            .ops
            .iter()
            .map(|o| o.t_us.saturating_add_unsigned(o.latency_us))
            .max()
            .unwrap_or(0) as f64
            / 1e6;
        Self::learn(
            wall_s,
            r.workload.warmup_s,
            r.run.window_s,
            last_op_end_s,
            cool_down(
                r.resilience.throttled_responses,
                r.resilience.max_retry_after_ms,
                trial_warmup,
            ),
        )
    }

    /// The wall time of a trial of `warmup_s` and `window_s`; `model_overhead_s` is the plan's
    /// own guess of the time around them, used when the run does not say.
    fn trial_s(&self, warmup_s: f64, window_s: f64, model_overhead_s: f64) -> f64 {
        warmup_s + window_s + self.tail_s + self.overhead_s.unwrap_or(model_overhead_s)
    }
}

/// An expected wall time that what was measured corrects: the estimate until the first
/// measurement, then the longest one measured (no trial starts that would not end within the
/// budget, so the plan leans long).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Expected {
    d: Duration,
    measured: bool,
}

impl Expected {
    fn new(estimate_s: f64) -> Self {
        Self {
            d: Duration::from_secs_f64(estimate_s.max(0.0)),
            measured: false,
        }
    }

    fn get(&self) -> Duration {
        self.d
    }

    fn observe(&mut self, took: Duration) {
        self.d = if self.measured {
            self.d.max(took)
        } else {
            took
        };
        self.measured = true;
    }
}

/// The line that corrects the upfront ETA once the first pair has run: `None` while the pair
/// took within a quarter of what was estimated. `pairs_left` is what the plan still allows,
/// `budget_left_s` what the budget does.
fn corrected_eta(
    estimated_pair_s: f64,
    measured_pair_s: f64,
    pairs_left: u64,
    budget_left_s: Option<f64>,
) -> Option<String> {
    let off = (measured_pair_s - estimated_pair_s).abs();
    if pairs_left == 0 || off <= 0.25 * estimated_pair_s.max(1e-3) {
        return None;
    }
    let fit = match budget_left_s {
        Some(b) => pairs_left.min((b.max(0.0) / measured_pair_s.max(1e-3)).floor() as u64),
        None => pairs_left,
    };
    Some(format!(
        "· a pair of trials takes {}, not the {} estimated: up to {} more for the {} pair{} that can still run",
        fmt_secs(measured_pair_s),
        fmt_secs(estimated_pair_s),
        fmt_secs(fit as f64 * measured_pair_s),
        fit,
        if fit == 1 { "" } else { "s" }
    ))
}

impl ProofEstimate {
    /// The estimate of a proof of `sizing` (most, fewest) sizing trials and `pairs` (fewest,
    /// most) A/B pairs whose trials take `t`, within `budget`. A trial that ends throttled is
    /// followed by a cool-down: the baseline trials are expected to, as the base run was; the
    /// changed operator's may (the most the proof takes) or may not (the least).
    fn new(
        command: String,
        change: Vec<String>,
        sizing: (u64, u64),
        pairs: (u64, u64),
        t: TrialTimes,
        budget: Option<Duration>,
    ) -> Self {
        // (the first pair, a later pair): the cool-down after A, and after B too at worst.
        let pair_s = |worst: bool| {
            let cool = t.cool_s * if worst { 2.0 } else { 1.0 };
            (t.a_s + t.b_first_s + cool, t.a_s + t.b_warm_s + cool)
        };
        let sizing_s = |worst: bool| t.b_first_s + if worst { t.cool_s } else { 0.0 };
        let cost = |s: u64, p: u64, worst: bool| -> f64 {
            let (cold, warm) = pair_s(worst);
            let pairs = if p == 0 {
                0.0
            } else {
                cold + (p - 1) as f64 * warm
            };
            s as f64 * sizing_s(worst) + pairs
        };
        let budget_s = budget.map(|b| b.as_secs_f64());
        let unbudgeted = sizing.0 + 2 * pairs.1;
        // Plan only the trials estimated to fit in the budget (K19): the sizing trials run
        // first, the A/B pairs in what is left; no trial starts that would not end in it.
        let fit = |s: u64, p: u64| -> (u64, u64) {
            let Some(b) = budget_s else {
                return (s, p);
            };
            let (cold, warm) = pair_s(false);
            let s = s.min((b / sizing_s(false).max(1e-3)).floor() as u64);
            let left = (b - s as f64 * sizing_s(false)).max(0.0);
            let fit_p = if left < cold {
                0
            } else {
                1 + ((left - cold) / warm.max(1e-3)).floor() as u64
            };
            (s, p.min(fit_p))
        };
        let (s_min, p_min) = fit(sizing.1, pairs.0);
        let (s_max, p_max) = fit(sizing.0, pairs.1);
        let min = cost(s_min, p_min, false);
        let mut max = cost(sizing.0, pairs.1, true);
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
            trial_eta_s: t.a_s,
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
            // An estimate, said as one: the trials run as long as they really fit, and the
            // proof may still decide (9.2 printed "too few pairs to decide" as a fact, then
            // decided).
            Some(all) => {
                let mut c = format!(
                    "; the budget is estimated to fit {} of {all}",
                    self.trials_max_count
                );
                if (self.pairs_max_count as usize) < MIN_DECISIVE_PAIRS {
                    c += &format!(
                        ", fewer than the {MIN_DECISIVE_PAIRS} pairs a verdict needs: raise --max-duration if the proof stops short"
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
    /// The request rate the finding fitted as the service's limit ([`Feasible`]).
    max_requests_per_s: Option<f64>,
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
        max_requests_per_s: None,
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
            c.max_requests_per_s = evidence(f, "fitted_requests_per_s").filter(|rps| *rps > 0.0);
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
            // A whole-object cache that would fetch more than the run read, or hold an
            // object above the ceiling, is advice against itself (K3-7): nothing is sized.
            let cache = crate::analysis::simulate::sized_cache(run)
                .map_err(|why| {
                    OprobeError::new(
                        ErrorCode::UsageInvalidArgument,
                        format!(
                            "finding `repeated_reads` has no cache to prove: a whole-object cache does not fit these reads ({why})"
                        ),
                    )
                    .with_hint(
                        "cache the ranges you re-read in the application; to test a cache anyway, pass --with layers.cache.memory=SIZE",
                    )
                })?;
            c.fixed
                .push(("layers.cache.memory".into(), cache.memory.to_string()));
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
            max_requests_per_s: None,
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
    /// The wall-time budget and the cool-downs between the trials.
    pace: Pacing,
    /// Wall time of one normal (or already-warmed tail-cut) trial of each arm ([`arm`]):
    /// learned from the base run, then corrected by the first trial of that arm.
    per_trial: [Expected; 2],
    /// Wall time of one cold tail-cut trial (`TAIL_CUT_WARMUP`).
    per_cold_tail_cut: Expected,
    /// Short trial warmup used when no cold `TailCutLayer` needs 11.5 s priming.
    short_warmup: Duration,
    /// Warmed `TailCutLayer` for Baseline A trials (isolated from Candidate/Sizing).
    warmed_baseline_tc: Option<(OperatorSpec, opendal::layers::TailCutLayer, Instant)>,
    /// Warmed `TailCutLayer` for Candidate/Sizing trials matching the exact `OperatorSpec`.
    warmed_candidate_tc: Option<(OperatorSpec, opendal::layers::TailCutLayer, Instant)>,
    /// What a trial must stay within to be feasible.
    limits: Feasible,
    out: Vec<ProveTrial>,
    target_name: Option<String>,
    /// The budget breach that stopped the trials (the partial trial run is saved).
    breach: Option<anyhow::Error>,
}

/// The wall-time budget of a proof and the cool-downs between its trials (K3-3). A cool-down
/// is part of the budget like a trial: it is reserved before the trial it precedes, also
/// before the second half of a pair, and it is never waited past the end of the budget. (9.2
/// reserved the cool-down a pair's first trial was expected to leave, then waited whatever it
/// did leave: up to 5 s and a trial past `--max-duration`.)
#[derive(Debug)]
struct Pacing {
    deadline: Option<tokio::time::Instant>,
    /// The cool-down owed before the next trial: the last one ended throttled
    /// ([`cool_down`]).
    cool: Duration,
    /// The cool-down that followed the last baseline trial and the last changed one (sizing or
    /// candidate): what the next trial of that arm is expected to need after it.
    cool_after: [Duration; 2],
    /// The cool-downs waited: how many, and how long in all.
    cooled: (u64, Duration),
}

impl Pacing {
    fn new(budget: Option<Duration>) -> Self {
        Self {
            deadline: budget.map(|b| tokio::time::Instant::now() + b),
            cool: Duration::ZERO,
            cool_after: [Duration::ZERO; 2],
            cooled: (0, Duration::ZERO),
        }
    }

    /// `need` more wall time ends within the budget.
    fn fits(&self, need: Duration) -> bool {
        self.deadline
            .is_none_or(|d| tokio::time::Instant::now() + need <= d)
    }

    /// The budget that is left (`None`: no budget).
    fn left(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(tokio::time::Instant::now()))
    }

    /// The cool-down owed and a trial of `trial` after it end within the budget.
    fn fits_trial(&self, trial: Duration) -> bool {
        self.fits(self.cool + trial)
    }

    /// A trial of `arm` ended and leaves `cool` to wait before the next trial.
    fn after_trial(&mut self, arm: usize, cool: Duration) {
        self.cool = cool;
        self.cool_after[arm] = cool;
    }

    /// Wait out the cool-down owed; Ctrl-C interrupts it. `false` when the budget ends before
    /// the cool-down would: nothing is waited, and no trial can follow.
    async fn cool_off(&mut self, cancel: &Cancel) -> Result<bool> {
        let wait = self.cool;
        if wait.is_zero() {
            return Ok(true);
        }
        if !self.fits(wait) {
            return Ok(false);
        }
        self.cool = Duration::ZERO;
        if !sleep_unless_cancelled(wait, cancel).await {
            return Err(OprobeError::new(ErrorCode::RunInterrupted, "prove interrupted").into());
        }
        self.cooled.0 += 1;
        self.cooled.1 += wait;
        Ok(true)
    }
}

/// What a trial must stay within for its value to be one the sizing may choose.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Feasible {
    /// The throttled share of its requests ([`sizing_throttle_limit`]).
    max_throttle: f64,
    /// The share of its operations that fail (the run's allowed error ratio).
    max_error: f64,
    /// The request rate the finding fitted as the service's limit (`rate_limit`): a value
    /// whose trial sends more is over the limit by the proof's own evidence, whether or not
    /// its two seconds were throttled. A token bucket's burst hides the limit from a short
    /// trial: on `lab://ratelimit` (fitted 270/s) a cap of 2 ran 311/s with no throttle
    /// while sizing, was chosen, then throttled 2.7 % in every A/B trial: `worse`.
    max_requests_per_s: Option<f64>,
}

impl Feasible {
    /// A trial of the changed operator that counted `ops` operations, `throttled_ratio` of
    /// its requests throttled, `error_ratio` of its operations failed, at `requests_per_s`.
    fn admits(
        &self,
        ops: u64,
        throttled_ratio: f64,
        error_ratio: f64,
        requests_per_s: f64,
    ) -> bool {
        ops > 0
            && throttled_ratio <= self.max_throttle
            && error_ratio <= self.max_error
            && self
                .max_requests_per_s
                .is_none_or(|max| requests_per_s <= max)
    }
}

/// How long to wait after a trial before the next one starts. A trial that ended throttled
/// (429/503) leaves the service's limit drained — an empty token bucket, a `Retry-After` still
/// running — and the next trial, often the other arm of its pair, would start against it and
/// be measured worse than it is. The wait follows what the trial saw: the longest
/// `Retry-After`, and at least twice the trials' warm-up (within [`COOL_DOWN_DEFAULT`]), never
/// more than [`COOL_DOWN_MAX`]. A trial that was not throttled needs none.
fn cool_down(throttled_count: u64, max_retry_after_ms: u64, warmup: Duration) -> Duration {
    if throttled_count == 0 && max_retry_after_ms == 0 {
        return Duration::ZERO;
    }
    let (shortest, longest) = COOL_DOWN_DEFAULT;
    (warmup * 2)
        .clamp(shortest, longest)
        .max(Duration::from_millis(max_retry_after_ms))
        .min(COOL_DOWN_MAX)
}

/// Sleep `wait`; `false` as soon as `cancel` fires instead.
async fn sleep_unless_cancelled(wait: Duration, cancel: &Cancel) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(wait) => true,
        _ = cancel.cancelled() => false,
    }
}

/// The arm of a trial (in [`Pacing::cool_after`], [`Trials::per_trial`]): the baseline, or the
/// changed operator.
fn arm(kind: TrialKind) -> usize {
    usize::from(kind != TrialKind::Baseline)
}

/// The spread of a trial's p99 ([`p99_spread`]) from its latency histogram: every operation
/// of the window, never sampled, in bins 12 % wide — the cheapest source that is exact in the
/// ranks. The latencies at the two ranks of [`p99_rank_band`] are read as the upper edges of
/// their bins, so a band inside one bin has no spread.
fn p99_spread_of(h: &crate::trace::LatencyHistogram) -> f64 {
    let n = h.total();
    let Some((lo, hi)) = p99_rank_band(n) else {
        return 0.0;
    };
    // The middle of a rank's step, so rounding cannot move the quantile to the next rank.
    let at = |rank: u64| h.quantile_ms((rank as f64 - 0.5) / n as f64);
    match (at(lo), at(hi)) {
        (Some(lo), Some(hi)) => p99_spread(lo, hi),
        _ => 0.0,
    }
}

impl Trials<'_> {
    /// Expected wall duration of running `(sets, kind)` next (cold vs warmed `TailCutLayer`).
    fn expected_duration(&self, sets: &[(String, String)], kind: TrialKind) -> Duration {
        let mut op = self.base.operator.clone();
        if kind != TrialKind::Baseline {
            let _ = op.apply_edits(sets);
        }
        let per_trial = self.per_trial[arm(kind)].get();
        let Some(tc) = &op.layers.tail_cut else {
            return per_trial;
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
            per_trial
        } else {
            self.per_cold_tail_cut.get()
        }
    }

    fn has_time_for(&self, need: Duration) -> bool {
        self.breach.is_none() && !self.cancel.is_cancelled() && self.pace.fits(need)
    }

    /// There is time for the cool-down owed and a trial of `(sets, kind)` after it.
    fn time_left_for_sets(&self, sets: &[(String, String)], kind: TrialKind) -> bool {
        self.breach.is_none()
            && !self.cancel.is_cancelled()
            && self.pace.fits_trial(self.expected_duration(sets, kind))
    }

    /// Expected wall duration of an A/B pair run in `order` with the candidate's `sets`: the
    /// cool-down owed, both trials, and the cool-down the first is expected to need before
    /// the second (what last followed a trial of its arm).
    fn expected_pair(&self, order: [TrialKind; 2], sets: &[(String, String)]) -> Duration {
        let trial = |kind| {
            let sets = if kind == TrialKind::Baseline {
                &[]
            } else {
                sets
            };
            self.expected_duration(sets, kind)
        };
        self.pace.cool + trial(order[0]) + self.pace.cool_after[arm(order[0])] + trial(order[1])
    }

    /// [`Self::trial`] after the cool-down owed, with a budget breach recorded instead of
    /// returned: `None` once a budget has stopped the trials (no more run), or when the
    /// cool-down does not fit in what is left of the wall-time budget. User cancellation
    /// (`RunInterrupted`) aborts immediately as an error (exit 130).
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
        if self.breach.is_some() || !self.pace.cool_off(&self.cancel).await? {
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
        // A budget breach came back as the error above ([`super::measure::measure`]).
        if m.cancelled {
            return Err(OprobeError::new(ErrorCode::RunInterrupted, "prove interrupted").into());
        }
        let elapsed = started.elapsed();
        if cold_tail_cut {
            self.per_cold_tail_cut.observe(elapsed);
        } else {
            self.per_trial[arm(kind)].observe(elapsed);
        }
        if m.run.report.summary.ops > 0
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
        self.pace.after_trial(
            arm(kind),
            cool_down(thr, r.resilience.max_retry_after_ms, self.short_warmup),
        );
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
            feasible: self.limits.admits(
                ops,
                throttled_request_ratio,
                error_ratio,
                requests as f64 / r.run.window_s.max(1e-3),
            ),
            p99_spread_ratio: p99_spread_of(&r.latency_histogram),
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

/// The complete A/B pairs of a prove's `trials`, by pair number, on `objective`: each pair's
/// order is the order its two trials ran in (the trials are stored as they ran). A trial whose
/// other half did not run is left out.
pub fn ab_pairs(objective: ProveObjective, trials: &[ProveTrial]) -> Vec<AbPair> {
    let mut by: BTreeMap<u64, (Option<&ProveTrial>, Option<&ProveTrial>, bool)> = BTreeMap::new();
    for t in trials {
        let Some(pair) = t.pair else { continue };
        let e = by.entry(pair).or_default();
        match t.kind {
            TrialKind::Baseline => e.0 = Some(t),
            TrialKind::Candidate => {
                e.1 = Some(t);
                e.2 = e.0.is_none();
            }
            TrialKind::Sizing => {}
        }
    }
    by.into_values()
        .filter_map(|(a, b, candidate_first)| {
            let (a, b) = (a?, b?);
            Some(ab_pair(objective, a, b, candidate_first))
        })
        // A pair that could not be compared was not judged ([`AbPair::comparable`]).
        .filter(AbPair::comparable)
        .collect()
}

/// The A/B pair of a baseline trial `a` and a candidate trial `b` on `objective`.
fn ab_pair(
    objective: ProveObjective,
    a: &ProveTrial,
    b: &ProveTrial,
    candidate_first: bool,
) -> AbPair {
    AbPair {
        baseline: objective.value(a),
        candidate: objective.value(b),
        ops_count: a.ops_count.min(b.ops_count),
        resolution: match objective {
            ProveObjective::Goodput => 0.0,
            ProveObjective::P99 => a.p99_spread_ratio.hypot(b.p99_spread_ratio),
        },
        candidate_first,
    }
}

/// The `kind` trials of the first `pairs` completed A/B pairs: a trailing trial whose other
/// half did not run (the budget ended between them) is left out of every metric.
fn paired(trials: &[ProveTrial], kind: TrialKind, pairs: u64) -> Vec<&ProveTrial> {
    trials
        .iter()
        .filter(|x| x.kind == kind && x.pair.is_some_and(|p| p < pairs))
        .collect()
}

/// Whether a share of the changed operator's (`kb` events in `nb`) is the change's own doing:
/// over `limit`, more than the current operator's (`ka` in `na`) by more than `limit` allows,
/// and beyond chance (a one-sided two-proportion test, z > 2). `(current, changed)` shares
/// when so. What both operators suffer alike — service errors that follow no load
/// (`lab://noisy`) — passes. The one test of [`throttle_gate`] and [`error_gate`].
fn share_gate((ka, na): (f64, f64), (kb, nb): (f64, f64), limit: f64) -> Option<(f64, f64)> {
    if nb == 0.0 {
        return None;
    }
    let b = kb / nb;
    let a = if na > 0.0 { ka / na } else { 0.0 };
    let z = if na > 0.0 {
        let p = (ka + kb) / (na + nb);
        let se = (p * (1.0 - p) * (1.0 / na + 1.0 / nb)).sqrt();
        if se > 0.0 {
            (b - a) / se
        } else {
            f64::INFINITY
        }
    } else {
        f64::INFINITY
    };
    (b > limit && b - a > limit && z > 2.0).then_some((a, b))
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
        (throttled as f64, requests as f64)
    };
    let Some((a, b)) = share_gate(counts(a_trials), counts(b_trials), max_throttle) else {
        return (verdict, None);
    };
    let note = format!(
        "the changed operator throttled {:.2} % of its requests (the current one {:.2} %), over --max-throttle {:.2} %: the verdict is inconclusive",
        b * 100.0,
        a * 100.0,
        max_throttle * 100.0
    );
    (ProveVerdict::Inconclusive, Some(note))
}

/// The same gate on failures (K3-1): a candidate whose operations fail (for reasons other than
/// throttling) beyond the error ratio the run allows, and more than the current operator's,
/// proves nothing on any objective. Operations that fail fast lower a p99 — 9.2 called
/// `layers.timeout.io=200ms` "improved" on p99 for cutting the slow operations off — and what
/// a goodput keeps is not the workload the run measured.
fn error_gate(
    verdict: ProveVerdict,
    a_trials: &[&ProveTrial],
    b_trials: &[&ProveTrial],
    max_error: f64,
) -> (ProveVerdict, Option<String>) {
    let counts = |ts: &[&ProveTrial]| {
        let ops: u64 = ts.iter().map(|t| t.ops_count).sum();
        let failed: f64 = ts
            .iter()
            .map(|t| (t.error_ratio * t.ops_count as f64).round())
            .sum();
        (failed, ops as f64)
    };
    let Some((a, b)) = share_gate(counts(a_trials), counts(b_trials), max_error) else {
        return (verdict, None);
    };
    let note = format!(
        "the changed operator failed {:.2} % of its operations (the current one {:.2} %), over the {:.2} % of errors the run allows: the verdict is inconclusive",
        b * 100.0,
        a * 100.0,
        max_error * 100.0
    );
    (ProveVerdict::Inconclusive, Some(note))
}

/// The note of a p99 proof whose trials' p99 sits on a cliff (K3-9): the latencies around it
/// differ by more than [`P99_CLIFF_RATIO`] within one trial, so which side a trial's p99 lands
/// on is chance and the noise floor is wide. `None` when no trial's does.
fn cliff_note(trials: &[&ProveTrial]) -> Option<String> {
    let on: Vec<f64> = trials
        .iter()
        .map(|t| t.p99_spread_ratio)
        .filter(|s| p99_on_a_cliff(*s))
        .collect();
    let widest = on.iter().copied().reduce(f64::max)?;
    Some(format!(
        "p99 sits on a cliff: lengthen the trial (--trial). In {} of {} trials the latencies around the p99 differ by more than {P99_CLIFF_RATIO:.0}× (up to {:.1}×), so the p99 of a trial is one side or the other by chance",
        on.len(),
        trials.len(),
        (4.0 * widest).exp()
    ))
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

/// The objective's interval after each A/B pair, as the prove saw it grow (the convergence
/// chart): the estimate, interval and noise floor of the verdict's own test ([`AbTest`]) over
/// the first 1, 2, … n of `pairs`, of a prove planned for at most `max_pairs`
/// ([`ProveOutput::planned_pairs`]). Its last point is the verdict's interval.
pub fn convergence(
    objective: ProveObjective,
    pairs: &[AbPair],
    max_pairs: usize,
) -> Vec<Converged> {
    let mut test = AbTest::new(objective.higher_is_better(), MIN_DECISIVE_PAIRS, max_pairs);
    pairs
        .iter()
        .map(|p| {
            test.push(*p);
            Converged {
                interval: test.interval(),
                noise_floor_ratio: test.noise_floor(),
            }
        })
        .collect()
}

/// The objective after some pairs (see [`convergence`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Converged {
    /// Relative change of the objective (candidate vs baseline) with its sequential,
    /// order-adjusted 95 % interval.
    pub interval: Interval,
    /// The A/A noise floor (a per-pair log-ratio spread).
    pub noise_floor_ratio: f64,
}

/// The secondary metrics of the A/B trials. Each interval is on the metric's own spread
/// (K3-5): its own trial-to-trial noise floor ([`noise_floor_ab`]) over its own resolution
/// (a count's, or a p99's order-statistic spread). 9.2 gave every metric the objective's
/// floor: a goodput steady at 300/s printed "[−17 %, +20 %]" beside a noisy p99.
fn secondary(obj: ProveObjective, a: &[&ProveTrial], b: &[&ProveTrial]) -> Vec<SecondaryMetric> {
    let mut v = Vec::new();
    let positive = |metric: &str, f: fn(&ProveTrial) -> f64, resolution: f64| {
        let values = |ts: &[&ProveTrial]| ts.iter().map(|t| f(t)).collect::<Vec<_>>();
        let (va, vb) = (values(a), values(b));
        SecondaryMetric {
            metric: metric.to_owned(),
            baseline: mean(a, f),
            candidate: mean(b, f),
            change: log_ratio(&va, &vb, true, noise_floor_ab(&va, &vb, resolution)),
        }
    };
    match obj {
        ProveObjective::Goodput => {
            let sq = |t: &ProveTrial| t.p99_spread_ratio.powi(2);
            v.push(positive(
                "p99_ms",
                |t| t.p99_ms,
                (mean(a, sq) + mean(b, sq)).sqrt(),
            ));
        }
        ProveObjective::P99 => {
            let fewest = a.iter().chain(b).map(|t| t.ops_count).min();
            v.push(positive(
                "goodput_ops_per_s",
                |t| t.goodput_ops_per_s,
                fewest.map_or(0.0, count_resolution),
            ));
        }
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
    // The run's whole command (K1-2): the operator reference alone drops its `--set` values,
    // and a run against a local endpoint would be repeated against the real service.
    .with_hint(format!(
        "profile the operator again until the run completes: {}",
        profile_command(&r.spec, &[])
    ))
    .into())
}

/// The secret options of `spec` that were given by value: oprobe stores no secret, and where
/// the operator came from (a configured operator, an operator file) does not give them back.
fn unstored_secrets(ctx: &Ctx, spec: &crate::model::RunSpec) -> Vec<String> {
    let redacted = |v: &String| v == crate::model::spec::REDACTED;
    let Ok(replayed) = super::profile::spec_profile(ctx, spec.clone()) else {
        return Vec::new();
    };
    spec.profile
        .operator
        .options
        .iter()
        .filter(|(k, v)| redacted(v) && !replayed.operator.options.contains_key(*k))
        .map(|(k, _)| format!("options.{k}"))
        .collect()
}

/// A prove that fails for want of credentials, of a run whose secrets were given by value
/// (`unstored`, [`unstored_secrets`]): the error says they were not stored, and how to give
/// them again. Any other error is returned as it is.
fn explain_unstored_secrets(e: anyhow::Error, scheme: &str, unstored: &[String]) -> anyhow::Error {
    if unstored.is_empty() {
        return e;
    }
    let body = crate::errors::classify(&e, scheme);
    if !matches!(
        body.code,
        ErrorCode::AuthNoCredentials | ErrorCode::AuthDenied
    ) {
        return e;
    }
    OprobeError::new(
        body.code,
        format!(
            "{}: the run was given {} by value, and oprobe stores no secret value",
            body.message,
            unstored.join(", ")
        ),
    )
    .with_hint(
        "provide the credentials through the environment or a configured operator, then prove again",
    )
    .into()
}

/// `oprobe prove`.
pub async fn prove(ctx: &Ctx, req: ProveRequest, cancel: Cancel) -> Result<ProveOutcome> {
    let run = ctx.store().load(&req.run)?;
    let unstored = unstored_secrets(ctx, &run.report.spec);
    let scheme = run.report.spec.profile.operator.scheme();
    prove_run(ctx, &run, req, cancel)
        .await
        .map_err(|e| explain_unstored_secrets(e, &scheme, &unstored))
}

async fn prove_run(
    ctx: &Ctx,
    run: &Run,
    req: ProveRequest,
    cancel: Cancel,
) -> Result<ProveOutcome> {
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
    refuse_unjudged(run)?;
    // The boundary is calibrated for 3 to 12 pairs: a plan outside it is refused, not
    // silently replaced by another (9.2 ran `--pairs 40` as 12).
    if !(MIN_DECISIVE_PAIRS..=MAX_PAIRS).contains(&req.pairs) {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!(
                "--pairs {} is out of range: a proof plans {MIN_DECISIVE_PAIRS} to {MAX_PAIRS} pairs",
                req.pairs
            ),
        )
        .with_hint(format!(
            "pass --pairs {MIN_DECISIVE_PAIRS} to {MAX_PAIRS} (at least {MIN_DECISIVE_PAIRS} are needed to decide; the sequential test is calibrated up to {MAX_PAIRS})"
        ))
        .into());
    }
    let max_pairs = req.pairs;
    let defaults = crate::config::defaults().prove;
    let max_throttle = req
        .max_throttle_ratio
        .or(run.report.spec.profile.sla.max_throttle_rate)
        .unwrap_or(defaults.max_throttle_ratio);
    let change = choose_change(run, &req)?;
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
        run,
    );
    let mut changed = base.operator.clone();
    changed.apply_edits(&probe_sets)?;
    let budget = proof_budget(&req)?;
    let short_warmup = baseline_warmup(&base, &change);
    let (estimate, times) = {
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
        let max_pairs = max_pairs as u64;
        let min_pairs = (defaults.min_pairs.max(MIN_DECISIVE_PAIRS) as u64).min(max_pairs);
        // What a trial takes around its warm-up and window is learned from the base run
        // (K3-2); the plan's own model of the dataset and the cleanup stands in when the run
        // does not say.
        let timing = BaseTiming::of(run, short_warmup);
        let model_overhead_s = plan.eta_s - plan.warmup_s - plan.duration_s + TRIAL_OVERHEAD_S;
        let trial_s = |warmup: Duration| {
            timing.trial_s(warmup.as_secs_f64(), plan.duration_s, model_overhead_s)
        };
        // A candidate that needs a long warmup takes longer on its first trial, then reuses
        // its warmed TailCutLayer across the remaining pairs.
        let a_s = trial_s(short_warmup);
        let b_first_s = trial_s(warmup);
        let tc_reusable = change.long_warmup
            && changed
                .layers
                .tail_cut
                .as_ref()
                .map(|t| t.window.get())
                .unwrap_or(Duration::from_secs(60))
                >= Duration::from_secs(20);
        let times = TrialTimes {
            a_s,
            b_first_s,
            b_warm_s: if tc_reusable { a_s } else { b_first_s },
            cool_s: timing.cool_s,
        };
        let estimate = ProofEstimate::new(
            prove_command(&req, run),
            probe_sets
                .iter()
                .map(|(k, v)| crate::operator::redacted_edit(k, v))
                .collect(),
            sizing,
            (min_pairs, max_pairs),
            times,
            budget,
        );
        if req.safety.dry_run {
            return Ok(ProveOutcome::Plan(Box::new(super::run::PlanOutput {
                plan,
                proof: Some(estimate),
            })));
        }
        (estimate, times)
    };
    let prove_id = RunId::new().to_string();
    let mut safety = req.safety.clone();
    // The budget also bounds the trials themselves (with room for the last one's cleanup).
    safety.max_duration = budget.map(|b| format!("{}ms", (b.as_millis() as u64) * 2));
    let guard = crate::engine::measure::guard_for(&super::run::budgets(ctx, &safety)?);
    let target_name =
        crate::operator::resolve::resolve(&run.report.spec.operator_ref, &*ctx.config()?)
            .ok()
            .and_then(|(_, s)| s.name());
    // Until the prove result is saved, its trial runs belong to nothing: an interruption, an
    // error or a dropped future deletes them, or no `show --rm` could ever reach them.
    let mut orphans = OrphanTrials {
        store: ctx.store(),
        prove_id: prove_id.clone(),
        armed: true,
    };
    let mut t = Trials {
        ctx,
        run,
        base: base.clone(),
        prove_id: prove_id.clone(),
        safety,
        guard,
        cancel: cancel.clone(),
        pace: Pacing::new(budget),
        // The changed operator's trials are expected to take what a warmed one does; a cold
        // TailCutLayer's (its long warm-up) have their own expectation.
        per_trial: [Expected::new(times.a_s), Expected::new(times.b_warm_s)],
        per_cold_tail_cut: Expected::new(
            times
                .b_first_s
                .max(times.a_s - short_warmup.as_secs_f64() + TAIL_CUT_WARMUP.as_secs_f64()),
        ),
        short_warmup,
        warmed_baseline_tc: None,
        warmed_candidate_tc: None,
        limits: Feasible {
            max_throttle: sizing_throttle_limit(run, change.finding.as_deref(), max_throttle),
            max_error: run.report.sla.max_error_ratio,
            max_requests_per_s: change.max_requests_per_s,
        },
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
            notes.push(match change.max_requests_per_s {
                Some(rps) => format!(
                    "no value met --max-throttle {:.2} % under the fitted rate limit ({rps:.0} requests/s): the least throttled one is tested",
                    max_throttle * 100.0
                ),
                None => format!(
                    "no value met --max-throttle {:.2} %: the least throttled one is tested",
                    max_throttle * 100.0
                ),
            });
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
    let sets = sets_at(&change, value, run);
    // Step 3: interleaved A/B with the sequential test.
    let min_pairs = defaults.min_pairs.max(MIN_DECISIVE_PAIRS);
    let mut test = AbTest::new(obj.higher_is_better(), min_pairs, max_pairs);
    let mut decision = AbDecision::Continue;
    let candidate_value = sizing.as_ref().map(|s| s.value.clone());
    let seed = run.report.spec.seed;
    let mut method = format!(
        "the A/B order of the pairs is balanced in blocks of two (one AB, one BA), the order within a block drawn from the run's seed ({seed}); the interval is the sequential, order-adjusted 95 % interval of a prove of at most {max_pairs} pairs, and the verdict is whether it excludes no change"
    );
    if change.long_warmup {
        method += "; the changed operator's TailCutLayer keeps its warmed state from one of its trials to the next, as a long-lived operator's does";
    }
    notes.push(method);
    // A pair that ran but cannot be compared (a trial without the objective): why. It ends
    // the proof, and is judged by nothing but the gates.
    let mut unjudged: Option<String> = None;
    while decision == AbDecision::Continue {
        // Balanced in blocks of one AB and one BA (W11, 9.2): a drift or a carryover that
        // favours whichever trial runs first or second falls on both arms alike, and the
        // test's estimate cancels it.
        let first = candidate_first(seed, test.pairs() as u64);
        let order = if first {
            [TrialKind::Candidate, TrialKind::Baseline]
        } else {
            [TrialKind::Baseline, TrialKind::Candidate]
        };
        let expected_pair = t.expected_pair(order, &sets);
        if !t.has_time_for(expected_pair) {
            break;
        }
        let pair_started = Instant::now();
        let (mut a, mut b) = (None, None);
        for (i, kind) in order.into_iter().enumerate() {
            // The pair's time was reserved for both halves before the first started, with the
            // cool-down the first was expected to leave. What it did leave is reserved again
            // now, with the second half after it (K3-3): a pair whose second half would end
            // past the budget is left incomplete, and its first trial out of every metric.
            let second_sets: &[(String, String)] = if kind == TrialKind::Baseline {
                &[]
            } else {
                &sets
            };
            if i == 1 && !t.time_left_for_sets(second_sets, kind) {
                break;
            }
            let trial = if kind == TrialKind::Baseline {
                t.try_trial(&[], kind, Some(test.pairs() as u64), None)
                    .await?
            } else {
                t.try_trial(
                    &sets,
                    kind,
                    Some(test.pairs() as u64),
                    candidate_value.clone(),
                )
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
        let pair = ab_pair(obj, &a, &b, first);
        if !pair.comparable() {
            // Never clamped into a ratio (K3-1: a candidate with no latency at all was
            // "improved by 100 %" on p99).
            let (who, value) = if obj.value(&b).is_finite() && obj.value(&b) > 0.0 {
                ("current", obj.value(&a))
            } else {
                ("changed", obj.value(&b))
            };
            unjudged = Some(format!(
                "pair {} was not judged: the {who} operator's trial has no {} to compare (it measured {value}); the proof stops at the pairs before it",
                test.pairs() + 1,
                obj.label()
            ));
            break;
        }
        decision = test.push(pair);
        // The first pair corrects the upfront ETA when it was off (K3-2).
        if test.pairs() == 1
            && decision == AbDecision::Continue
            && let Some(line) = corrected_eta(
                expected_pair.as_secs_f64(),
                pair_started.elapsed().as_secs_f64(),
                (max_pairs - 1) as u64,
                t.pace.left().map(|d| d.as_secs_f64()),
            )
        {
            ctx.out.info(dim(&line));
        }
    }
    let pairs = test.pairs() as u64;
    // Complete pairs that ran, judged or not: what the gates read.
    let ran = pairs + u64::from(unjudged.is_some());
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
    let budget_exceeded = t.breach.is_some() || ran == 0;
    if let Some(e) = &t.breach {
        notes.push(format!(
            "a budget stopped the prove after {pairs} pair{} of trials ({e:#}): the verdict reads what was measured",
            if pairs == 1 { "" } else { "s" }
        ));
    } else if ran == 0 {
        notes.push("the budget ran out before one pair of trials could run".to_owned());
    }
    let cooled = t.pace.cooled;
    if cooled.0 > 0 {
        notes.push(format!(
            "{} trial{} ended throttled: the prove cooled down before the next one ({} in all), so no trial started against a limit the one before it had drained",
            cooled.0,
            if cooled.0 == 1 { "" } else { "s" },
            crate::util::fmt_ms(cooled.1.as_secs_f64() * 1000.0)
        ));
    }
    // Step 4: decide. The verdict is the test's own: whether the interval of the look the
    // prove stopped at excludes no change, whatever stopped it.
    let floor = test.noise_floor();
    let delta = test.interval();
    let verdict = match test.verdict() {
        AbVerdict::Improved => ProveVerdict::Improved,
        AbVerdict::Worse => ProveVerdict::Worse,
        AbVerdict::Inconclusive => ProveVerdict::Inconclusive,
    };
    let few = (pairs as usize) < MIN_DECISIVE_PAIRS;
    if let Some(why) = unjudged {
        notes.push(why);
        if few {
            notes.push(format!(
                "only {pairs} pair{} of trials could be judged: at least {MIN_DECISIVE_PAIRS} are needed to decide",
                if pairs == 1 { "" } else { "s" }
            ));
        }
    } else if few {
        notes.push(format!(
            "the budget allowed only {pairs} pair{} of trials: at least {MIN_DECISIVE_PAIRS} are needed to decide",
            if pairs == 1 { "" } else { "s" }
        ));
    } else if decision == AbDecision::Futile {
        // A futility stop says what it gave up on (K3-4).
        notes.push(format!(
            "the proof stopped after {pairs} of at most {max_pairs} pairs: at the noise it measured (±{:.1} % a pair), {max_pairs} pairs could not show a change smaller than about {:.0} %, and the pairs so far rule out one that large",
            floor * 100.0,
            test.resolvable_change().unwrap_or_default() * 100.0
        ));
    } else if verdict == ProveVerdict::Inconclusive {
        notes.push(format!(
            "the sequential test did not decide within {pairs} pairs: the interval still includes no change"
        ));
    }
    let a_trials = paired(&t.out, TrialKind::Baseline, pairs);
    let b_trials = paired(&t.out, TrialKind::Candidate, pairs);
    if obj == ProveObjective::P99 {
        let judged: Vec<&ProveTrial> = a_trials.iter().chain(&b_trials).copied().collect();
        notes.extend(cliff_note(&judged));
    }
    // The gates read every complete pair that ran, the unjudged one too: it is the changed
    // operator failing every operation that leaves a pair with nothing to compare.
    let (a_ran, b_ran) = (
        paired(&t.out, TrialKind::Baseline, ran),
        paired(&t.out, TrialKind::Candidate, ran),
    );
    let (verdict, throttle_note) = throttle_gate(verdict, &a_ran, &b_ran, max_throttle);
    notes.extend(throttle_note);
    let (verdict, error_note) = error_gate(verdict, &a_ran, &b_ran, t.limits.max_error);
    notes.extend(error_note);
    let secondary = secondary(obj, &a_trials, &b_trials);
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
    let run_command = Some(profile_command(&r.spec, &sets));
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
        pairs_max_count: Some(max_pairs as u64),
        max_throttle_ratio: max_throttle,
        secondary,
        trials: t.out.clone(),
        diff,
        budget_exceeded,
        saved_to: None,
        run_command,
        notes: Vec::new(),
    };
    for note in notes {
        out.add_note(note);
    }
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
                out.add_note(format!("--save `{dest}` failed: {e:#}; the proof is kept"));
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
fn profile_command(spec: &crate::model::RunSpec, sets: &[(String, String)]) -> String {
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
    crate::model::spec::command_line(&args)
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
            p99_spread_ratio: 0.0,
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

    fn ab(kind: TrialKind, pair: u64, goodput: f64, ops: u64) -> ProveTrial {
        ProveTrial {
            kind,
            pair: Some(pair),
            ops_count: ops,
            ..trial(goodput, 10.0, true)
        }
    }

    /// The order of a stored pair is the order its trials ran in, so a stored result gives
    /// the test the pairs the prove gave it.
    #[test]
    fn stored_trials_give_back_the_pairs_and_their_order() {
        use TrialKind::{Baseline, Candidate};
        let trials = vec![
            trial(50.0, 10.0, true), // sizing
            ab(Baseline, 0, 100.0, 900),
            ab(Candidate, 0, 120.0, 1000),
            ab(Candidate, 1, 121.0, 1000),
            ab(Baseline, 1, 101.0, 800),
            // The budget ended before this candidate's baseline ran.
            ab(Candidate, 2, 119.0, 1000),
        ];
        let pairs = ab_pairs(ProveObjective::Goodput, &trials);
        assert_eq!(
            pairs,
            [
                AbPair {
                    baseline: 100.0,
                    candidate: 120.0,
                    ops_count: 900,
                    resolution: 0.0,
                    candidate_first: false
                },
                AbPair {
                    baseline: 101.0,
                    candidate: 121.0,
                    ops_count: 800,
                    resolution: 0.0,
                    candidate_first: true
                },
            ]
        );
        assert_eq!(ab_pairs(ProveObjective::P99, &trials)[0].baseline, 10.0);
    }

    /// A trial that ended throttled is followed by a cool-down taken from what it saw; one
    /// that was not is followed by none.
    #[test]
    fn a_throttled_trial_is_followed_by_a_bounded_cool_down() {
        let ms = Duration::from_millis;
        // Not throttled: the next trial starts at once.
        assert_eq!(cool_down(0, 0, ms(300)), Duration::ZERO);
        // Throttled, no Retry-After: twice the warm-up, within 250 ms .. 2 s.
        assert_eq!(cool_down(1, 0, ms(300)), ms(600));
        assert_eq!(cool_down(4000, 0, ms(300)), ms(600), "not by how much");
        assert_eq!(cool_down(1, 0, ms(100)), ms(250));
        assert_eq!(cool_down(1, 0, TAIL_CUT_WARMUP), ms(2000));
        // The service named a wait: at least that, and never more than 5 s.
        assert_eq!(cool_down(1, 1500, ms(300)), ms(1500));
        assert_eq!(cool_down(1, 100, ms(300)), ms(600));
        assert_eq!(cool_down(1, 60_000, ms(300)), ms(5000));
        // A Retry-After alone (a redirect or 5xx that asked to wait) counts too.
        assert_eq!(cool_down(0, 1000, ms(300)), ms(1000));
    }

    /// The cool-down is waited out, and Ctrl-C ends it instead (no wall-clock race: a wait
    /// that is cancelled never ends by itself within the test).
    #[tokio::test]
    async fn the_cool_down_can_be_interrupted() {
        let cancel = Cancel::new();
        assert!(sleep_unless_cancelled(Duration::from_millis(1), &cancel).await);
        let waiter = {
            let cancel = cancel.clone();
            tokio::spawn(
                async move { sleep_unless_cancelled(Duration::from_secs(3600), &cancel).await },
            )
        };
        cancel.cancel();
        assert!(!waiter.await.unwrap());
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
            pairs_max_count: Some(6),
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
        // The run's whole command (9.2 printed `oprobe run lab://demo`: the operator alone).
        let hint = b.hint.unwrap();
        assert_eq!(
            hint,
            format!(
                "profile the operator again until the run completes: {}",
                run.report.spec.command()
            )
        );
        assert!(hint.contains("oprobe run lab://demo "), "{hint}");
    }

    /// K1-2: an interrupted run against a local endpoint (`s3://bench/x --set
    /// options.endpoint=http://127.0.0.1:9000`) was hinted as `oprobe run s3://bench/x`: the
    /// real service, with ambient credentials. The hint is the run's whole command.
    #[test]
    fn the_hint_after_an_unjudged_run_keeps_its_endpoint() {
        let mut run = crate::analysis::simulate::tests::straggler_run();
        run.report.run.window_s = 5.0;
        let mut profile = run.report.spec.resolved_profile();
        profile.operator = OperatorSpec::from_url("s3://bench/x");
        profile
            .operator
            .set("options.endpoint", "http://127.0.0.1:9000")
            .unwrap();
        run.report.spec = crate::model::RunSpec::new(&profile, "s3://bench/x");
        crate::model::build::mark_partial(&mut run, crate::model::PartialReason::Interrupted);
        let e = refuse_unjudged(&run).unwrap_err();
        let hint = crate::errors::classify(&e, "s3").hint.unwrap();
        assert!(
            hint.contains("oprobe run s3://bench/x")
                && hint.contains("--set options.endpoint=http://127.0.0.1:9000"),
            "{hint}"
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
            TrialTimes::even(3.0),
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
            TrialTimes {
                b_first_s: 14.0,
                b_warm_s: 14.0,
                ..TrialTimes::even(3.0)
            },
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
            e.line().contains("the budget is estimated to fit 10 of 12"),
            "{}",
            e.line()
        );
        // Reusing a warmed TailCutLayer warms only on the first pair: 17 s + 5 × 6 s = 47 s.
        let reused = ProofEstimate::new(
            "oprobe prove r".into(),
            vec!["layers.tail_cut=on".into()],
            (0, 0),
            (3, 6),
            TrialTimes {
                b_first_s: 14.0,
                ..TrialTimes::even(3.0)
            },
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
            TrialTimes::even(3.0),
            Some(Duration::from_secs(12)),
        );
        // 2 sizing trials (6 s), then one pair (6 s): 4 trials at most.
        assert_eq!(e.trials_max_count, 4, "{e:?}");
        assert_eq!(e.pairs_max_count, 1);
        assert!(e.eta_max_s <= 12.0);
        let line = e.line();
        // An estimate, said as one (9.2: "the budget fits 4 of 20, too few pairs to decide (at
        // least 3): raise --max-duration", printed as a fact before a proof that then decided).
        assert!(
            line.contains("the budget is estimated to fit 4 of 20"),
            "{line}"
        );
        assert!(
            line.contains("raise --max-duration if the proof stops short"),
            "{line}"
        );
        assert!(!line.contains("too few pairs to decide"), "{line}");
        // Without a budget, nothing is cut.
        let free = ProofEstimate::new(
            "oprobe prove r".into(),
            vec![],
            (8, 2),
            (3, 6),
            TrialTimes::even(3.0),
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
    fn convergence_ends_on_the_verdicts_interval() {
        let a = [100.0, 102.0, 99.0, 101.0, 100.0];
        let b = [120.0, 121.0, 118.0, 122.0, 119.0];
        for obj in [ProveObjective::Goodput, ProveObjective::P99] {
            let pairs: Vec<AbPair> = (0..5)
                .map(|i| AbPair {
                    baseline: a[i],
                    candidate: b[i],
                    ops_count: 10_000,
                    resolution: 0.0,
                    candidate_first: candidate_first(7, i as u64),
                })
                .collect();
            let c = convergence(obj, &pairs, 6);
            assert_eq!(c.len(), 5);
            // The last point is what the prove's own test says after the same pairs.
            let mut test = AbTest::new(obj.higher_is_better(), 3, 6);
            for p in &pairs {
                test.push(*p);
            }
            let last = c.last().unwrap();
            assert_eq!(last.interval, test.interval());
            assert_eq!(last.noise_floor_ratio, test.noise_floor());
            // More pairs, a narrower interval; +20 % on either objective's own sign.
            let w = |i: &Interval| i.high - i.low;
            assert!(w(&c[4].interval) < w(&c[2].interval));
            assert!(w(&c[2].interval) < w(&c[0].interval));
            assert!(last.interval.low > 0.0, "{last:?}");
            // The interval is the plan's: a longer plan looks more often, so each look is wider.
            let longer = convergence(obj, &pairs, 12);
            assert!(w(&longer[4].interval) > w(&c[4].interval));
        }
        assert!(convergence(ProveObjective::Goodput, &[], 6).is_empty());
    }

    /// A result of 9.1 has no `pairs_max_count`: it still reads, as the default plan.
    #[test]
    fn a_9_1_result_reads_without_the_planned_pairs() {
        let mut v = serde_json::to_value(output(4, ProveVerdict::Improved)).unwrap();
        assert_eq!(v["pairs_max_count"], 6);
        v.as_object_mut().unwrap().remove("pairs_max_count");
        let old = ProveOutput::from_json_slice(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(old.pairs_max_count, None);
        assert_eq!(old.planned_pairs(), 6);
        // Not written back when absent.
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("pairs_max_count")
                .is_none()
        );
        let mut long = old.clone();
        long.pairs_count = 9;
        assert_eq!(long.planned_pairs(), 9);
        long.pairs_max_count = Some(12);
        assert_eq!(long.planned_pairs(), 12);
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
        let sec = secondary(ProveObjective::Goodput, &a, &b);
        let thr = sec
            .iter()
            .find(|m| m.metric == "throttled_request_ratio")
            .unwrap();
        assert_eq!(thr.baseline, 0.0, "{thr:?}");
    }

    fn ab_trial(kind: TrialKind, pair: u64, ops: u64, failed: u64, p99: f64) -> ProveTrial {
        ProveTrial {
            kind,
            pair: Some(pair),
            ops_count: ops,
            requests_count: ops,
            error_ratio: failed as f64 / ops.max(1) as f64,
            goodput_ops_per_s: (ops - failed) as f64 / 2.0,
            ..trial(0.0, p99, true)
        }
    }

    /// K3-1: `--with layers.timeout.io=200ms --objective p99` lowered the p99 by failing the
    /// slow operations, and 9.2 said `improved`; a candidate with no latency at all was
    /// "improved by 100 %". The error gate is the throttle gate's twin, on every objective.
    #[test]
    fn a_candidate_that_fails_its_operations_is_never_improved() {
        use TrialKind::{Baseline, Candidate};
        let a: Vec<ProveTrial> = (0..3)
            .map(|i| ab_trial(Baseline, i, 1000, 2, 900.0))
            .collect();
        let a: Vec<&ProveTrial> = a.iter().collect();
        // Every operation fails, fast: a p99 of 3 ms against 900 ms.
        let all_fail: Vec<ProveTrial> = (0..3)
            .map(|i| ab_trial(Candidate, i, 1000, 1000, 3.0))
            .collect();
        let all_fail: Vec<&ProveTrial> = all_fail.iter().collect();
        // The objective alone calls it improved …
        let mut test = AbTest::new(false, 3, 6);
        for (i, (x, y)) in a.iter().zip(&all_fail).enumerate() {
            test.push(ab_pair(ProveObjective::P99, x, y, i % 2 == 1));
        }
        assert_eq!(test.verdict(), AbVerdict::Improved);
        // … and the gate does not, whatever the verdict was.
        for v in [ProveVerdict::Improved, ProveVerdict::Worse] {
            let (verdict, note) = error_gate(v, &a, &all_fail, 0.01);
            assert_eq!(verdict, ProveVerdict::Inconclusive);
            let note = note.unwrap();
            assert!(
                note.contains("failed 100.00 %")
                    && note.contains("0.20 %")
                    && note.contains("1.00 %"),
                "{note}"
            );
        }
        // A fifth of them fail (the timeout case).
        let some: Vec<ProveTrial> = (0..3)
            .map(|i| ab_trial(Candidate, i, 1000, 200, 200.0))
            .collect();
        let some: Vec<&ProveTrial> = some.iter().collect();
        assert_eq!(
            error_gate(ProveVerdict::Improved, &a, &some, 0.01).0,
            ProveVerdict::Inconclusive
        );
        // Failures both operators suffer alike are not the change's doing; nor is a
        // difference within what the run allows, or within chance.
        let noisy_a: Vec<ProveTrial> = (0..3)
            .map(|i| ab_trial(Baseline, i, 1000, 50, 900.0))
            .collect();
        let noisy_a: Vec<&ProveTrial> = noisy_a.iter().collect();
        let noisy_b: Vec<ProveTrial> = (0..3)
            .map(|i| ab_trial(Candidate, i, 1000, 55, 400.0))
            .collect();
        let noisy_b: Vec<&ProveTrial> = noisy_b.iter().collect();
        assert_eq!(
            error_gate(ProveVerdict::Improved, &noisy_a, &noisy_b, 0.01),
            (ProveVerdict::Improved, None)
        );
        let few: Vec<ProveTrial> = (0..3)
            .map(|i| ab_trial(Candidate, i, 1000, 8, 400.0))
            .collect();
        let few: Vec<&ProveTrial> = few.iter().collect();
        assert_eq!(
            error_gate(ProveVerdict::Improved, &a, &few, 0.01),
            (ProveVerdict::Improved, None)
        );
        assert_eq!(
            error_gate(ProveVerdict::Improved, &a, &[], 0.01),
            (ProveVerdict::Improved, None)
        );
        // A trial with nothing to compare makes no pair: stored results give the same pairs.
        let none = [
            ab_trial(Baseline, 0, 1000, 0, 900.0),
            ab_trial(Candidate, 0, 1000, 1000, 0.0),
        ];
        assert!(ab_pairs(ProveObjective::P99, &none).is_empty());
        assert!(ab_pairs(ProveObjective::Goodput, &none).is_empty());
    }

    /// K3-2, the review's first case (`lab://hotkey`: "23–45 s" for a proof of 54 s): the
    /// base run's operations kept retrying 2 s past its window and it was throttled, so every
    /// trial drains as long and is followed by a cool-down. 9.2's model knew neither.
    #[test]
    fn the_estimate_learns_the_retry_tail_and_the_cool_downs_from_the_base_run() {
        let ms = Duration::from_millis;
        // A 10 s window after 1 s of warm-up; the last operation ended 12 s in; the run took
        // 15 s in all by its stamps (whole seconds: at least 14); throttled, no Retry-After.
        let base = BaseTiming::learn(Some(15.0), 1.0, 10.0, 12.0, cool_down(40, 0, ms(300)));
        assert_eq!(
            base,
            BaseTiming {
                overhead_s: Some(1.0),
                tail_s: 2.0,
                cool_s: 0.6
            }
        );
        // A trial: 0.3 s warm-up, 2 s window, the 2 s tail, 1 s around them.
        let trial_s = base.trial_s(0.3, 2.0, 0.9);
        assert!((trial_s - 5.3).abs() < 1e-9, "{trial_s}");
        let times = TrialTimes {
            cool_s: base.cool_s,
            ..TrialTimes::even(trial_s)
        };
        let e = ProofEstimate::new("oprobe prove r".into(), vec![], (0, 0), (3, 6), times, None);
        // 9.2: 3–6 pairs of two 3.2 s trials (the model: 2.3 s + 0.9 s), 19–38 s.
        let old = ProofEstimate::new(
            "oprobe prove r".into(),
            vec![],
            (0, 0),
            (3, 6),
            TrialTimes::even(2.3 + 0.9),
            None,
        );
        assert!((old.eta_max_s - 38.4).abs() < 1e-9, "{old:?}");
        // Now: the least is three pairs with a cool-down after each baseline, the most six
        // pairs with one after every trial.
        assert!(
            (e.eta_min_s - 3.0 * (2.0 * 5.3 + 0.6)).abs() < 1e-9,
            "{e:?}"
        );
        assert!(
            (e.eta_max_s - 6.0 * (2.0 * 5.3 + 1.2)).abs() < 1e-9,
            "{e:?}"
        );
        assert!(e.eta_min_s > old.eta_min_s * 1.5 && e.eta_max_s > old.eta_max_s * 1.5);
        // A run that was not throttled owes no cool-down, and one with no tail has none.
        let calm = BaseTiming::learn(Some(11.0), 1.0, 10.0, 9.99, cool_down(0, 0, ms(300)));
        assert_eq!(
            (calm.tail_s, calm.cool_s, calm.overhead_s),
            (0.0, 0.0, Some(0.0))
        );
    }

    /// K3-2, the second case (a fast lab): 9.2's model charged every trial a dataset upload
    /// and a cleanup that took no time, planned one pair in the budget, printed "too few
    /// pairs to decide: raise --max-duration" — and the proof decided.
    #[test]
    fn the_estimate_does_not_overcharge_a_fast_target() {
        let budget = Some(Duration::from_secs(16));
        // The plan's model: 1.6 s around a 2.3 s trial.
        let model_overhead_s = 1.6;
        let unlearned = BaseTiming::default().trial_s(0.3, 2.0, model_overhead_s);
        assert!((unlearned - 3.9).abs() < 1e-9);
        let old = ProofEstimate::new(
            "oprobe prove r".into(),
            vec![],
            (0, 0),
            (3, 6),
            TrialTimes::even(unlearned),
            budget,
        );
        assert_eq!(old.pairs_max_count, 2, "{old:?}");
        // The base run: 8.3 s of warm-up and window, 9 s between its stamps, nothing draining.
        let base = BaseTiming::learn(Some(9.0), 0.3, 8.0, 7.9, Duration::ZERO);
        let trial_s = base.trial_s(0.3, 2.0, model_overhead_s);
        assert!((trial_s - 2.3).abs() < 1e-9, "{trial_s}");
        let e = ProofEstimate::new(
            "oprobe prove r".into(),
            vec![],
            (0, 0),
            (3, 6),
            TrialTimes::even(trial_s),
            budget,
        );
        assert_eq!((e.pairs_min_count, e.pairs_max_count), (3, 3), "{e:?}");
        assert!(!e.line().contains("raise --max-duration"), "{}", e.line());
        // A run that began and ended within one second (stamps 0 s apart) is still read.
        let short = BaseTiming::learn(Some(0.0), 0.0, 0.3, 0.3005, Duration::ZERO);
        assert_eq!(short.overhead_s, Some(0.0));
        assert!((short.trial_s(0.1, 0.3, model_overhead_s) - 0.4005).abs() < 1e-9);
        // Stamps that do not parse: the model stands in.
        let blind = BaseTiming::learn(None, 0.3, 8.0, 7.9, Duration::ZERO);
        assert_eq!(blind.trial_s(0.3, 2.0, model_overhead_s), unlearned);
    }

    /// K3-2: the first trial of each kind replaces the estimate, in either direction; later
    /// ones only lengthen it. And the first pair says so when the upfront ETA was off.
    #[test]
    fn the_first_trial_corrects_the_estimate() {
        let ms = Duration::from_millis;
        let mut fast = Expected::new(3.9);
        assert_eq!(fast.get(), ms(3900));
        fast.observe(ms(2400));
        assert_eq!(fast.get(), ms(2400), "corrected down");
        fast.observe(ms(2300));
        assert_eq!(fast.get(), ms(2400));
        fast.observe(ms(2600));
        assert_eq!(fast.get(), ms(2600));
        let mut slow = Expected::new(2.3);
        slow.observe(ms(4500));
        assert_eq!(slow.get(), ms(4500), "corrected up");
        // Within a quarter of the estimate: nothing to say.
        assert_eq!(corrected_eta(8.0, 9.5, 5, None), None);
        assert_eq!(corrected_eta(8.0, 3.0, 0, None), None);
        let line = corrected_eta(6.0, 10.0, 5, None).unwrap();
        assert!(
            line.contains("takes 10s, not the 6s estimated") && line.contains("up to 50s"),
            "{line}"
        );
        // The budget bounds what is left.
        let line = corrected_eta(6.0, 10.0, 5, Some(25.0)).unwrap();
        assert!(line.contains("up to 20s more for the 2 pairs"), "{line}");
    }

    /// K3-3: a cool-down is part of the budget. It is reserved with the trial after it, and
    /// never waited past the end of the budget (9.2 slept the whole cool-down a pair's first
    /// trial left, up to 5 s, and then ran the second trial: both past `--max-duration`).
    #[tokio::test(start_paused = true)]
    async fn a_cool_down_never_runs_past_the_budget() {
        let s = Duration::from_secs;
        let cancel = Cancel::new();
        let started = tokio::time::Instant::now();
        let mut pace = Pacing::new(Some(s(10)));
        assert!(pace.fits_trial(s(10)) && !pace.fits_trial(s(11)));
        // The first half of a pair took 6 s and ended throttled: 5 s to cool down.
        tokio::time::advance(s(6)).await;
        pace.after_trial(arm(TrialKind::Baseline), s(5));
        // The second half (2 s) fits by itself, not after the cool-down: it is not started …
        assert!(pace.fits(s(2)));
        assert!(!pace.fits_trial(s(2)));
        // … and the cool-down, which would end past the budget, is not waited.
        assert!(!pace.cool_off(&cancel).await.unwrap());
        assert_eq!(started.elapsed(), s(6), "nothing was waited");
        assert_eq!(pace.cooled, (0, Duration::ZERO));
        // A cool-down that fits is waited in full, once.
        pace.after_trial(arm(TrialKind::Candidate), s(3));
        assert!(pace.fits_trial(s(1)) && !pace.fits_trial(s(2)));
        assert!(pace.cool_off(&cancel).await.unwrap());
        assert_eq!(started.elapsed(), s(9));
        assert_eq!(pace.cooled, (1, s(3)));
        assert!(pace.cool_off(&cancel).await.unwrap());
        assert_eq!(started.elapsed(), s(9));
        assert_eq!(pace.left(), Some(s(1)));
        // What the arms last left is what a pair reserves for them.
        assert_eq!(pace.cool_after, [s(5), s(3)]);
        // Without a budget everything fits, and Ctrl-C still ends a cool-down.
        let mut free = Pacing::new(None);
        assert!(free.fits_trial(s(86_400)) && free.left().is_none());
        free.after_trial(0, s(5));
        cancel.cancel();
        assert!(free.cool_off(&cancel).await.is_err());
    }

    /// K3-5: on a p99 proof whose p99 moved a lot between trials, a goodput steady at 300/s
    /// printed "[−17 %, +20 %]": the objective's noise floor on another metric.
    #[test]
    fn secondary_metrics_use_their_own_spread() {
        use TrialKind::{Baseline, Candidate};
        let mk = |kind, pair: u64, goodput: f64, p99: f64| ProveTrial {
            kind,
            pair: Some(pair),
            ops_count: 600,
            ..trial(goodput, p99, true)
        };
        let a = [
            mk(Baseline, 0, 300.0, 900.0),
            mk(Baseline, 1, 301.0, 1400.0),
            mk(Baseline, 2, 299.0, 800.0),
            mk(Baseline, 3, 300.0, 1500.0),
        ];
        let b = [
            mk(Candidate, 0, 300.0, 40.0),
            mk(Candidate, 1, 299.0, 60.0),
            mk(Candidate, 2, 301.0, 35.0),
            mk(Candidate, 3, 300.0, 55.0),
        ];
        let (a, b): (Vec<&ProveTrial>, Vec<&ProveTrial>) = (a.iter().collect(), b.iter().collect());
        // The objective's floor is tens of percent.
        let p99 = |ts: &[&ProveTrial]| ts.iter().map(|t| t.p99_ms).collect::<Vec<_>>();
        assert!(noise_floor_ab(&p99(&a), &p99(&b), 0.0) > 0.3);
        let sec = secondary(ProveObjective::P99, &a, &b);
        let goodput = sec
            .iter()
            .find(|m| m.metric == "goodput_ops_per_s")
            .unwrap();
        assert!(
            goodput.change.low > -0.05 && goodput.change.high < 0.05,
            "{goodput:?}"
        );
        assert!(goodput.change.low < 0.0 && goodput.change.high > 0.0);
        // And the other way round: a p99 keeps its own (wide) spread beside a steady goodput.
        let sec = secondary(ProveObjective::Goodput, &a, &b);
        let p99 = sec.iter().find(|m| m.metric == "p99_ms").unwrap();
        assert!(p99.change.high - p99.change.low > 0.05, "{p99:?}");
        assert!(p99.change.high < 0.0, "{p99:?}");
    }

    /// K3-9: a p99 on a cliff. 1000 operations, 985 at 10 ms and 15 retried ones at 1 s: the
    /// ranks 0.99·n ± 2·√(0.0099·n) (983..997) straddle the edge, so the p99 of a trial is
    /// 10 ms or 1 s by chance. The trial says so, the note says so, and the pair's resolution
    /// is the floor's.
    #[test]
    fn a_p99_on_a_cliff_is_said_and_widens_the_noise_floor() {
        use crate::trace::LatencyHistogram;
        let cliff = LatencyHistogram::from_counts([(10_000, 985), (1_000_000, 15)]);
        let spread = p99_spread_of(&cliff);
        assert!(p99_on_a_cliff(spread), "{spread}");
        // About a quarter of ln(100): the band spans 10 ms to 1 s.
        assert!((spread - 100f64.ln() / 4.0).abs() < 0.05, "{spread}");
        // The edge well below the band (5 % retried): the p99 is inside the slow mode.
        let inside = LatencyHistogram::from_counts([(10_000, 950), (1_000_000, 50)]);
        assert_eq!(p99_spread_of(&inside), 0.0);
        // A smooth tail: a few bins wide, no cliff.
        let smooth = LatencyHistogram::from_counts(
            (0..1000).map(|i| ((10_000.0 * 1.01f64.powi(i)) as u64, 1)),
        );
        let s = p99_spread_of(&smooth);
        assert!(s > 0.0 && !p99_on_a_cliff(s), "{s}");
        assert_eq!(p99_spread_of(&LatencyHistogram::default()), 0.0);

        let mk = |kind, pair: u64, p99: f64, spread: f64| ProveTrial {
            kind,
            pair: Some(pair),
            ops_count: 1000,
            p99_spread_ratio: spread,
            ..trial(500.0, p99, true)
        };
        use TrialKind::{Baseline, Candidate};
        // The trials happen to agree: without the resolution the floor would be the minimum.
        let trials: Vec<ProveTrial> = (0..3)
            .flat_map(|i| [mk(Baseline, i, 10.0, spread), mk(Candidate, i, 10.0, 0.0)])
            .collect();
        let pairs = ab_pairs(ProveObjective::P99, &trials);
        assert_eq!(pairs[0].resolution, spread);
        let floor = convergence(ProveObjective::P99, &pairs, 6)[2].noise_floor_ratio;
        assert!((floor - spread).abs() < 1e-12, "{floor} vs {spread}");
        // Goodput is not a p99: its pairs carry no such resolution.
        assert_eq!(
            ab_pairs(ProveObjective::Goodput, &trials)[0].resolution,
            0.0
        );
        let judged: Vec<&ProveTrial> = trials.iter().collect();
        let note = cliff_note(&judged).unwrap();
        assert!(
            note.starts_with("p99 sits on a cliff: lengthen the trial (--trial)"),
            "{note}"
        );
        assert!(note.contains("In 3 of 6 trials"), "{note}");
        let calm: Vec<ProveTrial> = (0..3).map(|i| mk(Baseline, i, 10.0, s)).collect();
        assert_eq!(cliff_note(&calm.iter().collect::<Vec<_>>()), None);
        // The spread is stored only when there is one, and a 9.2 trial reads without it.
        let v = serde_json::to_value(&trials[1]).unwrap();
        assert!(v.get("p99_spread_ratio").is_none(), "{v}");
        let back: ProveTrial = serde_json::from_value(v).unwrap();
        assert_eq!(back.p99_spread_ratio, 0.0);
        assert!(serde_json::to_value(&trials[0]).unwrap()["p99_spread_ratio"].is_f64());
    }

    /// K5-3: every note of a result is redacted where it enters the result.
    #[test]
    fn notes_never_carry_url_credentials() {
        let mut out = output(4, ProveVerdict::Improved);
        out.notes.clear();
        out.add_note(
            "--save `op.toml` failed: cannot reach https://AKIAEXAMPLE:s3cr3t@minio.internal:9000/bucket; the proof is kept",
        );
        out.add_note("a budget stopped the prove (GET https://h/b/k?X-Amz-Signature=abcdef0123&X-Amz-Expires=60)");
        let all = out.notes.join("\n");
        assert!(
            !all.contains("s3cr3t") && !all.contains("abcdef0123"),
            "{all}"
        );
        assert!(all.contains("minio.internal:9000/bucket"), "{all}");
        assert_eq!(out.notes.len(), 2);
    }

    /// A proof in which nothing was compared offers no profile of a change it did not test
    /// (9.2: `next: profile the changed operator` after a 0-pair, budget-stopped prove).
    #[test]
    fn a_proof_with_no_pair_offers_no_profile_of_the_change() {
        let mut none = output(0, ProveVerdict::Inconclusive);
        assert!(none.next().is_empty());
        // A sizing trial ran before the budget ended: its runs can still be listed.
        none.trials.push(trial(50.0, 10.0, true));
        let next = none.next();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].cli, "oprobe show --list --all");
        let done = output(4, ProveVerdict::Improved).next();
        assert_eq!(done.len(), 2);
        assert!(done[0].cli.starts_with("oprobe run "), "{}", done[0].cli);
    }

    /// A run whose secret was given by value (`--set options.secret_access_key=…`) cannot be
    /// replayed with it: oprobe stored no secret. When the prove then fails for want of
    /// credentials, the error says so (9.2: the plain `auth.no_credentials` of a first run).
    #[test]
    fn a_prove_without_the_runs_secret_says_it_was_not_stored() {
        let no_creds = || -> anyhow::Error {
            OprobeError::new(
                ErrorCode::AuthNoCredentials,
                "no credentials found for s3://b",
            )
            .with_hint("set AWS_ACCESS_KEY_ID")
            .into()
        };
        let lost = vec!["options.secret_access_key".to_owned()];
        let e = explain_unstored_secrets(no_creds(), "s3", &lost);
        let b = crate::errors::classify(&e, "s3");
        assert_eq!(b.code, ErrorCode::AuthNoCredentials);
        assert!(
            b.message.contains("no credentials found")
                && b.message.contains("options.secret_access_key by value")
                && b.message.contains("stores no secret"),
            "{}",
            b.message
        );
        assert!(b.hint.unwrap().starts_with("provide the credentials"));
        // Nothing was given by value, or the failure is something else: the error as it is.
        let e = explain_unstored_secrets(no_creds(), "s3", &[]);
        assert_eq!(
            crate::errors::classify(&e, "s3").hint.as_deref(),
            Some("set AWS_ACCESS_KEY_ID")
        );
        let other: anyhow::Error =
            OprobeError::new(ErrorCode::UsageInvalidArgument, "bad value").into();
        let e = explain_unstored_secrets(other, "s3", &lost);
        assert_eq!(crate::errors::classify(&e, "s3").message, "bad value");
    }

    /// CI on 9.2.0, `lab://ratelimit` (300 requests/s, burst 20; fitted 270/s): the sizing
    /// trial of `layers.concurrent_limit.http=2` ran 311/s with no throttle in its two
    /// seconds and was chosen; in the A/B pairs it throttled 2.7 % and the verdict was
    /// `worse`. A value whose own trial sends more than the fitted limit is not feasible.
    #[tokio::test]
    async fn sizing_a_rate_limit_stays_under_the_fitted_rate() {
        // (requests/s, throttled share, p99 ms) at each cap, as CI measured them.
        fn at(v: u64) -> (f64, f64, f64) {
            match v {
                1 => (155.0, 0.0, 8.0),
                2 => (311.0, 0.0, 7.0),
                3 => (300.0, 0.027, 1500.0),
                _ => (300.0, 0.05, 1700.0),
            }
        }
        struct Lab(Feasible, Vec<u64>);
        impl Measure for Lab {
            async fn measure(&mut self, v: u64) -> Result<Option<ProveTrial>> {
                self.1.push(v);
                let (rps, throttled, p99) = at(v);
                let feasible = self.0.admits(1000, throttled, 0.0, rps);
                Ok(Some(ProveTrial {
                    throttled_request_ratio: throttled,
                    ..trial(rps, p99, feasible)
                }))
            }
        }
        let d = Domain::Count { min: 1, max: 4 };
        let limits = Feasible {
            max_throttle: 0.001,
            max_error: 0.01,
            max_requests_per_s: Some(270.0),
        };
        for obj in [ProveObjective::P99, ProveObjective::Goodput] {
            let mut lab = Lab(limits, Vec::new());
            assert_eq!(search(d, 1, obj, 8, &mut lab).await.unwrap(), (1, true));
            assert_eq!(lab.1, [1, 2], "2 is over the fitted rate: the climb ends");
        }
        // 9.2's rule (the throttle of the trial alone) chose 2.
        let mut lab = Lab(
            Feasible {
                max_requests_per_s: None,
                ..limits
            },
            Vec::new(),
        );
        let got = search(d, 1, ProveObjective::P99, 8, &mut lab).await;
        assert_eq!(got.unwrap(), (2, true));
        // No value under the rate: none is feasible, and the result says so.
        let mut lab = Lab(
            Feasible {
                max_requests_per_s: Some(100.0),
                ..limits
            },
            Vec::new(),
        );
        let (_, feasible) = search(d, 1, ProveObjective::P99, 8, &mut lab)
            .await
            .unwrap();
        assert!(!feasible);
        // Exactly at the limit is within it; a trial that counted nothing never is.
        assert!(limits.admits(10, 0.0, 0.0, 270.0));
        assert!(!limits.admits(0, 0.0, 0.0, 1.0));
    }

    /// K3-6: a result of 9.1 has no plan. Core says so, and offers no way to read its trials
    /// as if it had one without asking.
    #[test]
    fn a_result_without_a_plan_says_so() {
        let mut p = output(4, ProveVerdict::Improved);
        assert_eq!(p.plan(), Some(6));
        p.pairs_max_count = Some(40);
        assert_eq!(p.plan(), Some(12), "clamped to the calibrated range");
        p.pairs_max_count = None;
        assert_eq!(p.plan(), None);
        assert_eq!(p.planned_pairs(), 6);
    }
}
