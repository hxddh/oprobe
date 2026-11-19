//! `oprobe diff` CI performance, amplification, throttle, and FinOps regression gate.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{ensure_parent_dir, BenchmarkReport};

/// Configurable CI regression gate thresholds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegressionThresholds {
    pub max_p99_regression_pct: f64,
    pub min_latency_delta_ms: f64,
    pub min_throughput_ratio: f64,
    pub max_read_amp_delta: f64,
    pub max_rpc_amp_delta: f64,
    pub max_cost_increase_pct: f64,
}

impl Default for RegressionThresholds {
    fn default() -> Self {
        Self {
            max_p99_regression_pct: 15.0,
            min_latency_delta_ms: 0.75,
            min_throughput_ratio: 0.90,
            max_read_amp_delta: 0.25,
            max_rpc_amp_delta: 0.75,
            max_cost_increase_pct: 20.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerbDiffRow {
    pub verb: String,
    pub baseline_p95_ms: f64,
    pub candidate_p95_ms: f64,
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
    pub baseline_monthly_usd: f64,
    pub candidate_monthly_usd: f64,
    pub monthly_usd_delta: f64,
    pub monthly_usd_delta_pct: f64,
    pub candidate_throttles: u64,
    pub candidate_crc_errors: u64,
    pub verb_diffs: Vec<VerbDiffRow>,
    pub reasons: Vec<String>,
}

impl DiffSummary {
    pub fn save_json(&self, path: impl AsRef<Path>) -> Result<()> {
        ensure_parent_dir(path.as_ref())?;
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn save_markdown(&self, path: impl AsRef<Path>) -> Result<()> {
        ensure_parent_dir(path.as_ref())?;
        std::fs::write(path, self.to_markdown())?;
        Ok(())
    }

    pub fn to_markdown(&self) -> String {
        let verdict = if self.passed_gate {
            "PASS (No SLA or FinOps Regression)"
        } else {
            "FAIL (Regression Gate Breached)"
        };
        let mut md = format!("# oprobe CI Regression Gate — **{verdict}**\n\n");
        md.push_str(&format!(
            "- **Aggregate Throughput**: `{:.2} MiB/s` -> `{:.2} MiB/s` (ratio `{:.2}x`)\n",
            self.baseline_mib_s, self.candidate_mib_s, self.throughput_ratio
        ));
        md.push_str(&format!(
            "- **Read Amplification**: `{:.2}x` -> `{:.2}x` | **RPC Amplification**: `{:.2}x` -> `{:.2}x`\n",
            self.baseline_read_amp, self.candidate_read_amp, self.baseline_rpc_amp, self.candidate_rpc_amp
        ));
        md.push_str(&format!(
            "- **Projected Monthly Cloud Cost**: `${:.2}/mo` -> `${:.2}/mo` (`{:+.2} USD/mo`, `{:+.1}%`)\n",
            self.baseline_monthly_usd,
            self.candidate_monthly_usd,
            self.monthly_usd_delta,
            self.monthly_usd_delta_pct
        ));
        md.push_str(&format!(
            "- **Candidate Health**: `429/503 Throttles = {}` | `CRC32C Corruptions = {}`\n\n",
            self.candidate_throttles, self.candidate_crc_errors
        ));

        md.push_str("| Operation Verb | Base P95 (ms) | Cand P95 (ms) | Base P99 (ms) | Cand P99 (ms) | P99 Delta (%) | Gate |\n");
        md.push_str("| :--- | ---: | ---: | ---: | ---: | ---: | :---: |\n");
        for row in &self.verb_diffs {
            md.push_str(&format!(
                "| `{}` | {:.2} | {:.2} | {:.2} | {:.2} | {:+.1}% | {} |\n",
                row.verb,
                row.baseline_p95_ms,
                row.candidate_p95_ms,
                row.baseline_p99_ms,
                row.candidate_p99_ms,
                row.p99_delta_pct,
                if row.regressed { "REGRESSED" } else { "OK" }
            ));
        }

        if !self.reasons.is_empty() {
            md.push_str("\n## Gate Breach Findings\n\n");
            for r in &self.reasons {
                md.push_str(&format!("- {r}\n"));
            }
        }
        md
    }
}

/// Compare baseline vs candidate [`BenchmarkReport`] snapshots and evaluate CI regression rules.
pub fn diff_reports(
    baseline: &BenchmarkReport,
    candidate: &BenchmarkReport,
    thresholds: &RegressionThresholds,
) -> DiffSummary {
    let mut reasons = Vec::new();
    let base_verbs: BTreeMap<&str, (f64, f64)> = baseline
        .frame
        .verbs
        .iter()
        .map(|v| (v.verb.as_str(), (v.p95_ms, v.p99_ms)))
        .collect();

    let mut verb_diffs = Vec::new();
    for cand_v in &candidate.frame.verbs {
        if let Some(&(base_p95, base_p99)) = base_verbs.get(cand_v.verb.as_str()) {
            let denom = base_p99.max(0.001);
            let delta_pct = ((cand_v.p99_ms - base_p99) / denom) * 100.0;
            let abs_delta = cand_v.p99_ms - base_p99;
            let regressed = abs_delta > thresholds.min_latency_delta_ms
                && delta_pct > thresholds.max_p99_regression_pct;
            if regressed {
                reasons.push(format!(
                    "Verb `{}` P99 regressed by {:+.1}% ({:.2} ms -> {:.2} ms, max allowed {:+.1}%).",
                    cand_v.verb, delta_pct, base_p99, cand_v.p99_ms, thresholds.max_p99_regression_pct
                ));
            }
            verb_diffs.push(VerbDiffRow {
                verb: cand_v.verb.clone(),
                baseline_p95_ms: base_p95,
                candidate_p95_ms: cand_v.p95_ms,
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
            "Aggregate throughput dropped to {:.2}x of baseline ({:.2} -> {:.2} MiB/s, min required {:.2}x).",
            throughput_ratio, base_mib, cand_mib, thresholds.min_throughput_ratio
        ));
    }

    let read_amp_delta =
        candidate.frame.transport.read_amplification - baseline.frame.transport.read_amplification;
    if read_amp_delta > thresholds.max_read_amp_delta {
        reasons.push(format!(
            "Read amplification increased by {:+.2}x ({:.2}x -> {:.2}x, limit {:+.2}x).",
            read_amp_delta,
            baseline.frame.transport.read_amplification,
            candidate.frame.transport.read_amplification,
            thresholds.max_read_amp_delta
        ));
    }

    let rpc_amp_delta =
        candidate.frame.transport.rpc_amplification - baseline.frame.transport.rpc_amplification;
    if rpc_amp_delta > thresholds.max_rpc_amp_delta {
        reasons.push(format!(
            "RPC split amplification increased by {:+.2}x ({:.2}x -> {:.2}x, limit {:+.2}x).",
            rpc_amp_delta,
            baseline.frame.transport.rpc_amplification,
            candidate.frame.transport.rpc_amplification,
            thresholds.max_rpc_amp_delta
        ));
    }

    let cand_crc = candidate.frame.transport.crc32c_corrupted;
    if cand_crc > 0 {
        reasons.push(format!(
            "Candidate encountered {cand_crc} end-to-end CRC32C payload corruption(s)."
        ));
    }

    let base_throttles =
        baseline.frame.transport.http_429_count + baseline.frame.transport.http_503_slowdown_count;
    let cand_throttles = candidate.frame.transport.http_429_count
        + candidate.frame.transport.http_503_slowdown_count;
    if cand_throttles > base_throttles {
        reasons.push(format!(
            "Candidate triggered HTTP 429/503 rate-limiting ({cand_throttles} events vs {base_throttles} in baseline)."
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
    let usd_delta = cand_usd - base_usd;
    let usd_delta_pct = if base_usd > 0.01 {
        (usd_delta / base_usd) * 100.0
    } else {
        0.0
    };
    if usd_delta > 5.0 && usd_delta_pct > thresholds.max_cost_increase_pct {
        reasons.push(format!(
            "Projected monthly cloud bill increased by {:+.1}% (${:.2} -> ${:.2}/mo, limit {:+.1}%).",
            usd_delta_pct, base_usd, cand_usd, thresholds.max_cost_increase_pct
        ));
    }

    DiffSummary {
        passed_gate: reasons.is_empty(),
        baseline_mib_s: baseline.frame.transport.aggregate_mib_s,
        candidate_mib_s: cand_mib,
        throughput_ratio,
        baseline_read_amp: baseline.frame.transport.read_amplification,
        candidate_read_amp: candidate.frame.transport.read_amplification,
        baseline_rpc_amp: baseline.frame.transport.rpc_amplification,
        candidate_rpc_amp: candidate.frame.transport.rpc_amplification,
        baseline_monthly_usd: base_usd,
        candidate_monthly_usd: cand_usd,
        monthly_usd_delta: usd_delta,
        monthly_usd_delta_pct: usd_delta_pct,
        candidate_throttles: cand_throttles,
        candidate_crc_errors: cand_crc,
        verb_diffs,
        reasons,
    }
}
