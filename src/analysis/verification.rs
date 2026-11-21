//! Recommendations are hypotheses (design v4 §5): a finding's [`Verification`] names the change
//! ([`Overrides`]), the commands that test it and what it should do to which metric
//! ([`Expectation`]). `oprobe verify` replays the run with and without the change and answers
//! [`VerifyOutcome`] per finding.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::spec::{Overrides, RunSpec};

/// Which way a metric should move.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Higher than the baseline.
    #[default]
    Increase,
    /// Lower than the baseline.
    Decrease,
    /// At most the predicted value (e.g. a throttle budget).
    AtMost,
    /// At least the predicted value.
    AtLeast,
    /// Within the interval of the baseline (a change that must not cost anything).
    Unchanged,
}

/// What a change should do to one metric: a predicted value with an interval.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Expectation {
    /// A report metric path, e.g. `summary.ops_per_s`, `throttled_request_ratio`, `p99_ms`.
    pub metric: String,
    pub direction: Direction,
    /// Predicted value, in `unit`.
    pub predicted: f64,
    /// Lower end of the prediction interval, in `unit`.
    pub lo: f64,
    /// Upper end of the prediction interval, in `unit`.
    pub hi: f64,
    /// Unit of `predicted`, `lo` and `hi` (`ops_per_s`, `ms`, `ratio`, …).
    pub unit: String,
}

/// How to test a finding's advice (design v4 §5).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Verification {
    /// The hypothesis in words.
    pub description: String,
    /// The change to the run's spec.
    pub overrides: Overrides,
    /// `oprobe verify RUN CODE`: replays the run with and without the change and judges it.
    pub command: String,
    /// `oprobe run --from RUN --set …`: the changed run alone.
    pub rerun: String,
    /// The changed run as a self-contained `oprobe run TARGET …` command (no stored run
    /// needed).
    pub replay: String,
    /// What the change should do (filled by the findings that can predict it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect: Vec<Expectation>,
}

impl Verification {
    /// The verification of finding `code` of run `run_id` (spec `spec`): apply `overrides`.
    pub fn new(
        run_id: &str,
        code: &str,
        spec: &RunSpec,
        overrides: Overrides,
        description: impl Into<String>,
    ) -> Self {
        Self {
            description: description.into(),
            command: crate::model::spec::command_line(&[
                "verify".into(),
                run_id.into(),
                code.into(),
            ]),
            rerun: RunSpec::rerun_from(run_id, &overrides),
            replay: spec.with(&overrides).command(),
            overrides,
            expect: Vec::new(),
        }
    }

    pub fn expecting(mut self, e: Expectation) -> Self {
        self.expect.push(e);
        self
    }
}

/// What `verify` concluded about one hypothesis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOutcome {
    /// The change moved the metrics as expected.
    Confirmed,
    /// The change did not help (or made things worse).
    Refuted,
    /// Noise or too few samples to tell.
    Inconclusive,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_name_the_run_and_the_change() {
        let spec = RunSpec::default();
        let o = Overrides::new().with_set("concurrency", "8");
        let v = Verification::new(
            "20261121-101010-abcd",
            "throttling",
            &spec,
            o,
            "fewer workers",
        );
        assert_eq!(v.command, "oprobe verify 20261121-101010-abcd throttling");
        assert_eq!(
            v.rerun,
            "oprobe run --from 20261121-101010-abcd --set concurrency=8"
        );
        assert!(v.replay.starts_with("oprobe run memory://"), "{}", v.replay);
        assert!(v.replay.contains("--concurrency 8"), "{}", v.replay);
    }
}
