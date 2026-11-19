//! `oprobe diff` 基线回归对比门禁：
//! 对比两份 `BenchmarkReport` 快照的分动词 P99 延迟、聚合吞吐、读放大、RPC 放大与月度云成本变化，
//! 在 CI/CD 流水线中自动拦截存储栈退化。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::BenchmarkReport;

/// CI 回归判定阈值
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegressionThresholds {
    pub max_p99_regression_pct: f64,
    pub min_throughput_ratio: f64,
    pub max_read_amp_delta: f64,
}

impl Default for RegressionThresholds {
    fn default() -> Self {
        Self {
            max_p99_regression_pct: 15.0,
            min_throughput_ratio: 0.90,
            max_read_amp_delta: 0.25,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerbDiffRow {
    pub verb: String,
    pub baseline_p99_ms: f64,
    pub candidate_p99_ms: f64,
    pub p99_delta_pct: f64,
    pub regressed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub passed_gate: bool,
    pub baseline_mib_s: f64,
    pub candidate_mib_s: f64,
    pub throughput_ratio: f64,
    pub baseline_read_amp: f64,
    pub candidate_read_amp: f64,
    pub baseline_rpc_amp: f64,
    pub candidate_rpc_amp: f64,
    pub monthly_usd_delta: f64,
    pub verb_diffs: Vec<VerbDiffRow>,
    pub reasons: Vec<String>,
}

impl DiffSummary {
    pub fn to_markdown(&self) -> String {
        let verdict = if self.passed_gate {
            "PASS (No SLA Regression)"
        } else {
            "FAIL (Regression Threshold Breached)"
        };
        let mut md = format!("# oprobe Baseline Regression Diff — **{verdict}**\n\n");
        md.push_str(&format!(
            "- **Aggregate Throughput**: `{:.2} MiB/s` -> `{:.2} MiB/s` (ratio `{:.2}x`)\n",
            self.baseline_mib_s, self.candidate_mib_s, self.throughput_ratio
        ));
        md.push_str(&format!(
            "- **Read Amplification**: `{:.2}x` -> `{:.2}x` | **RPC Amplification**: `{:.2}x` -> `{:.2}x`\n",
            self.baseline_read_amp, self.candidate_read_amp, self.baseline_rpc_amp, self.candidate_rpc_amp
        ));
        md.push_str(&format!(
            "- **Projected Monthly Cloud Cost Delta**: `{:+.2} USD/mo`\n\n",
            self.monthly_usd_delta
        ));

        md.push_str("| Operation Verb | Baseline P99 (ms) | Candidate P99 (ms) | Delta (%) | Gate |\n");
        md.push_str("| :--- | ---: | ---: | ---: | :---: |\n");
        for row in &self.verb_diffs {
            md.push_str(&format!(
                "| `{}` | {:.2} | {:.2} | {:+.1}% | {} |\n",
                row.verb,
                row.baseline_p99_ms,
                row.candidate_p99_ms,
                row.p99_delta_pct,
                if row.regressed { "REGRESSED" } else { "OK" }
            ));
        }

        if !self.reasons.is_empty() {
            md.push_str("\n## Gate Findings\n\n");
            for r in &self.reasons {
                md.push_str(&format!("- {r}\n"));
            }
        }
        md
    }
}

/// 对比基准报告与候选报告并判定是否触发 CI 回归拦截
pub fn diff_reports(
    baseline: &BenchmarkReport,
    candidate: &BenchmarkReport,
    thresholds: &RegressionThresholds,
) -> DiffSummary {
    let mut reasons = Vec::new();
    let base_verbs: BTreeMap<&str, f64> = baseline
        .frame
        .verbs
        .iter()
        .map(|v| (v.verb.as_str(), v.p99_ms))
        .collect();

    let mut verb_diffs = Vec::new();
    for cand_v in &candidate.frame.verbs {
        if let Some(&base_p99) = base_verbs.get(cand_v.verb.as_str()) {
            let denom = base_p99.max(0.001);
            let delta_pct = ((cand_v.p99_ms - base_p99) / denom) * 100.0;
            // 避免亚毫秒内存噪声误报：仅当绝对增量 > 0.5ms 且百分比超阈值时判定退化
            let regressed =
                (cand_v.p99_ms - base_p99) > 0.5 && delta_pct > thresholds.max_p99_regression_pct;
            if regressed {
                reasons.push(format!(
                    "Verb `{}` P99 regressed by {:+.1}% ({:.2}ms -> {:.2}ms, limit +{:.1}%).",
                    cand_v.verb, delta_pct, base_p99, cand_v.p99_ms, thresholds.max_p99_regression_pct
                ));
            }
            verb_diffs.push(VerbDiffRow {
                verb: cand_v.verb.clone(),
                baseline_p99_ms: base_p99,
                candidate_p99_ms: cand_v.p99_ms,
                p99_delta_pct: delta_pct,
                regressed,
            });
        }
    }

    let base_mib = baseline.frame.transport.aggregate_mib_s.max(0.001);
    let cand_mib = candidate.frame.transport.aggregate_mib_s;
    let throughput_ratio = cand_mib / base_mib;
    if throughput_ratio < thresholds.min_throughput_ratio {
        reasons.push(format!(
            "Aggregate throughput dropped to {:.2}x of baseline ({:.2} -> {:.2} MiB/s, min ratio {:.2}x).",
            throughput_ratio, base_mib, cand_mib, thresholds.min_throughput_ratio
        ));
    }

    let read_amp_delta =
        candidate.frame.transport.read_amplification - baseline.frame.transport.read_amplification;
    if read_amp_delta > thresholds.max_read_amp_delta {
        reasons.push(format!(
            "Read amplification increased by {:+.2}x ({:.2}x -> {:.2}x).",
            read_amp_delta,
            baseline.frame.transport.read_amplification,
            candidate.frame.transport.read_amplification
        ));
    }

    let base_usd = baseline
        .multi_cloud_finops
        .first()
        .map(|c| c.total_monthly_usd)
        .unwrap_or(0.0);
    let cand_usd = candidate
        .multi_cloud_finops
        .first()
        .map(|c| c.total_monthly_usd)
        .unwrap_or(0.0);

    DiffSummary {
        passed_gate: reasons.is_empty(),
        baseline_mib_s: baseline.frame.transport.aggregate_mib_s,
        candidate_mib_s: cand_mib,
        throughput_ratio,
        baseline_read_amp: baseline.frame.transport.read_amplification,
        candidate_read_amp: candidate.frame.transport.read_amplification,
        baseline_rpc_amp: baseline.frame.transport.rpc_amplification,
        candidate_rpc_amp: candidate.frame.transport.rpc_amplification,
        monthly_usd_delta: cand_usd - base_usd,
        verb_diffs,
        reasons,
    }
}
