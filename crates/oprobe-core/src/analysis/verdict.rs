//! Two-sided net verdicts (design v5 §4, Verify 5 and compare): what a change did overall.
//!
//! Stream ANA computes them (an SPRT over interleaved pairs, bounded intervals); the foundation
//! defines the vocabulary that verify, compare and the App share, so they can never disagree.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The net effect of a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetVerdict {
    /// Better on the objective, nothing significantly worse.
    Improved,
    /// Worse on the objective.
    Regressed,
    /// Some metrics better, others worse (e.g. "throttling −4.6 pp, goodput −21 %").
    Traded,
    /// No significant change.
    NoChange,
    /// Not enough evidence either way (never shown as "same").
    Inconclusive,
}

impl NetVerdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Improved => "improved",
            Self::Regressed => "regressed",
            Self::Traded => "traded",
            Self::NoChange => "no change",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// One metric that moved, with its relative change.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MetricMove {
    /// `ops_per_s`, `p99_ms`, `throttled_request_ratio`, …
    pub metric: String,
    pub delta_ratio: f64,
}

/// A net verdict with what got better and what got worse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NetAssessment {
    pub verdict: NetVerdict,
    #[serde(default)]
    pub better: Vec<MetricMove>,
    #[serde(default)]
    pub worse: Vec<MetricMove>,
}

impl NetAssessment {
    /// One sentence: "traded: throttled_request_ratio −95.0 %, ops_per_s −21.0 %".
    pub fn summary(&self) -> String {
        let fmt = |m: &MetricMove| format!("{} {:+.1} %", m.metric, m.delta_ratio * 100.0);
        let moves: Vec<String> = self.better.iter().chain(&self.worse).map(fmt).collect();
        if moves.is_empty() {
            self.verdict.label().to_owned()
        } else {
            format!("{}: {}", self.verdict.label(), moves.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_name_both_sides_of_a_trade() {
        let a = NetAssessment {
            verdict: NetVerdict::Traded,
            better: vec![MetricMove {
                metric: "throttled_request_ratio".into(),
                delta_ratio: -0.95,
            }],
            worse: vec![MetricMove {
                metric: "ops_per_s".into(),
                delta_ratio: -0.21,
            }],
        };
        assert_eq!(
            a.summary(),
            "traded: throttled_request_ratio -95.0 %, ops_per_s -21.0 %"
        );
        assert_eq!(
            serde_json::to_value(NetVerdict::NoChange).unwrap(),
            "no_change"
        );
    }
}
