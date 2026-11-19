//! 诊断报告落盘与导出引擎：
//! 支持将完整剖析快照与寻优矩阵导出为 JSON（机器可读基线）、Markdown（架构评审报告）与 CSV（电子表格分析），
//! 并通过 [`diff`] 子模块提供 CI 性能回归门禁对比。

pub mod diff;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::autotune::{AutoTuneOutcome, CandidateTrial, TuneKnob};
use crate::cost::{compare_multi_cloud_finops, CostProjection};
use crate::telemetry::TelemetryFrame;
use crate::workload::ProfileConfig;

/// 可持久化落盘的完整剖析基线报告（`oprobe diff` 的输入契约）
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: String,
    pub generated_unix_secs: u64,
    pub profile: ProfileConfig,
    pub active_knob: TuneKnob,
    pub frame: TelemetryFrame,
    pub multi_cloud_finops: Vec<CostProjection>,
    #[serde(default)]
    pub trials: Vec<CandidateTrial>,
    #[serde(default)]
    pub generated_rust_code: String,
}

impl BenchmarkReport {
    pub fn from_single_run(profile: ProfileConfig, knob: TuneKnob, frame: TelemetryFrame) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let finops = compare_multi_cloud_finops(
            &frame.transport,
            profile.workload.kind.write_ratio(),
            profile.workload.internet_egress_ratio,
        );
        let rust_code = crate::autotune::generate_rust_snippet(&profile.target.scheme, &knob);
        Self {
            schema_version: "oprobe.report.v1".to_owned(),
            generated_unix_secs: now,
            profile,
            active_knob: knob,
            frame,
            multi_cloud_finops: finops,
            trials: Vec::new(),
            generated_rust_code: rust_code,
        }
    }

    pub fn from_autotune(profile: ProfileConfig, outcome: AutoTuneOutcome) -> Self {
        let mut rep = Self::from_single_run(profile, outcome.winner.knob, outcome.best_frame);
        rep.trials = outcome.trials;
        rep.generated_rust_code = outcome.generated_rust_code;
        rep
    }

    pub fn save_json(&self, path: impl AsRef<Path>) -> Result<()> {
        let payload = serde_json::to_string_pretty(self)?;
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, payload)?;
        Ok(())
    }

    pub fn load_json(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn to_markdown(&self) -> String {
        let mut md = String::with_capacity(4096);
        md.push_str(&format!(
            "# oprobe White-Box Diagnostic Report — `{}` (`{}`)\n\n",
            self.profile.target.name, self.profile.target.scheme
        ));
        md.push_str(&format!(
            "- **Workload**: `{}` | **Severity**: `{}`\n",
            self.profile.workload.kind.title(),
            self.frame.diagnosis.severity.badge()
        ));
        md.push_str(&format!(
            "- **Headline Diagnosis**: {}\n- **Actionable Recommendation**: {}\n- **Active Parameters**: `concurrent={}`, `chunk={} KiB`, `gap={} KiB`, `foyer={}`, `tail_cut={}`, `keep_alive={}`\n\n",
            self.frame.diagnosis.headline,
            self.frame.diagnosis.recommendation,
            self.active_knob.concurrent,
            self.active_knob.chunk_bytes / 1024,
            self.active_knob.range_gap_bytes / 1024,
            self.active_knob.enable_foyer,
            self.active_knob.enable_tail_cut,
            self.active_knob.keep_alive,
        ));

        md.push_str("## 1. Six-Stage Latency Waterfall\n\n");
        md.push_str("| Stage | Mean Latency (ms) | Share (%) |\n| :--- | ---: | ---: |\n");
        for (stage, ms, pct) in self.frame.waterfall.breakdown_pct() {
            md.push_str(&format!("| {stage} | {ms:.3} | {pct:.1}% |\n"));
        }
        md.push_str(&format!(
            "| **Total Pipeline** | **{:.3}** | **100.0%** |\n\n",
            self.frame.waterfall.total_ms()
        ));

        md.push_str("## 2. Per-Verb Isolated HDR Latency Quantiles\n\n");
        md.push_str("| Operation Verb | Count | OPS/s | P50 (ms) | P90 (ms) | P99 (ms) | P99.9 (ms) | Stream (MiB/s) | 429/503 (%) |\n");
        md.push_str("| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for v in &self.frame.verbs {
            md.push_str(&format!(
                "| `{}` | {} | {:.1} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1}% |\n",
                v.verb,
                v.count,
                v.ops_per_sec,
                v.p50_ms,
                v.p90_ms,
                v.p99_ms,
                v.p999_ms,
                v.single_stream_mib_s,
                v.throttle_429_503_pct
            ));
        }
        md.push('\n');

        let tp = &self.frame.transport;
        md.push_str("## 3. Transport Efficiency & Loadgen Self-Check\n\n");
        md.push_str(&format!(
            "- **Aggregate Throughput**: `{:.2} MiB/s` (`{:.2} MiB/s/Core` across `{}` logical cores, RSS `{} MiB`)\n",
            tp.aggregate_mib_s, tp.mib_per_cpu_core, tp.cpu_cores, tp.process_rss_mib
        ));
        md.push_str(&format!(
            "- **Amplification & Reuse**: Socket Reuse `{:.1}%` | Read Amplification `{:.2}x` | RPC Amplification `{:.2}x` | CRC32C Verified `{} ok / {} bad`\n\n",
            tp.socket_reuse_pct, tp.read_amplification, tp.rpc_amplification, tp.crc32c_verified_ok, tp.crc32c_corrupted
        ));

        md.push_str("## 4. Multi-Cloud Monthly FinOps Projection (10 TB Stored / 50 TB Logical Access)\n\n");
        md.push_str("| Provider | Storage ($/mo) | API Fees ($/mo) | Egress ($/mo) | Total ($/mo) | Amplification Waste ($/mo) |\n");
        md.push_str("| :--- | ---: | ---: | ---: | ---: | ---: |\n");
        for c in &self.multi_cloud_finops {
            md.push_str(&format!(
                "| {} | ${:.2} | ${:.2} | ${:.2} | **${:.2}** | ${:.2} |\n",
                c.provider_name,
                c.monthly_storage_usd,
                c.monthly_api_requests_usd,
                c.monthly_egress_usd,
                c.total_monthly_usd,
                c.waste_from_amplification_usd
            ));
        }

        if !self.trials.is_empty() {
            md.push_str("\n## 5. Auto-Tune Candidate Sweep Matrix\n\n");
            md.push_str("| # | Phase | Concurrent | Chunk (KiB) | Gap (KiB) | Foyer | Worst P99 (ms) | MiB/s | Read Amp | Score |\n");
            md.push_str("| ---: | :--- | ---: | ---: | ---: | :---: | ---: | ---: | ---: | ---: |\n");
            for t in &self.trials {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2}x | {:.1} |\n",
                    t.trial_id,
                    t.phase,
                    t.knob.concurrent,
                    t.knob.chunk_bytes / 1024,
                    t.knob.range_gap_bytes / 1024,
                    if t.knob.enable_foyer { "on" } else { "off" },
                    t.worst_p99_ms,
                    t.aggregate_mib_s,
                    t.read_amplification,
                    t.score
                ));
            }
        }

        md.push_str("\n## 6. Recommended Production Rust `Operator` Code\n\n```rust\n");
        md.push_str(&self.generated_rust_code);
        md.push_str("\n```\n");
        md
    }

    pub fn save_markdown(&self, path: impl AsRef<Path>) -> Result<()> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, self.to_markdown())?;
        Ok(())
    }

    pub fn to_csv(&self) -> String {
        let mut out = String::from(
            "trial_id,phase,concurrent,chunk_kib,gap_kib,foyer,tail_cut,worst_p99_ms,aggregate_mib_s,read_amp,rpc_amp,monthly_usd,throttled_503,sla_passed,score\n",
        );
        if self.trials.is_empty() {
            let worst_p99 = self
                .frame
                .verbs
                .iter()
                .map(|v| v.p99_ms)
                .fold(0.0_f64, f64::max);
            let usd = self
                .multi_cloud_finops
                .first()
                .map(|c| c.total_monthly_usd)
                .unwrap_or(0.0);
            out.push_str(&format!(
                "1,SingleRun,{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.2},false,true,0.0\n",
                self.active_knob.concurrent,
                self.active_knob.chunk_bytes / 1024,
                self.active_knob.range_gap_bytes / 1024,
                self.active_knob.enable_foyer,
                self.active_knob.enable_tail_cut,
                worst_p99,
                self.frame.transport.aggregate_mib_s,
                self.frame.transport.read_amplification,
                self.frame.transport.rpc_amplification,
                usd,
            ));
        } else {
            for t in &self.trials {
                out.push_str(&format!(
                    "{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.2},{},{},{:.2}\n",
                    t.trial_id,
                    t.phase,
                    t.knob.concurrent,
                    t.knob.chunk_bytes / 1024,
                    t.knob.range_gap_bytes / 1024,
                    t.knob.enable_foyer,
                    t.knob.enable_tail_cut,
                    t.worst_p99_ms,
                    t.aggregate_mib_s,
                    t.read_amplification,
                    t.rpc_amplification,
                    t.estimated_monthly_usd,
                    t.throttled_503,
                    t.sla_passed,
                    t.score,
                ));
            }
        }
        out
    }

    pub fn save_csv(&self, path: impl AsRef<Path>) -> Result<()> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, self.to_csv())?;
        Ok(())
    }
}
