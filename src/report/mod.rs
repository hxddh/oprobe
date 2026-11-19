//! Diagnostic report persistence, multi-format exporters (JSON / Markdown / CSV / Terminal),
//! and `oprobe diff` CI regression gate contracts.

pub mod diff;
pub mod terminal;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::autotune::{
    generate_python_snippet, generate_rust_snippet, generate_toml_snippet, AutoTuneOutcome,
    CandidateTrial, TuneKnob,
};
use crate::cost::{compare_multi_cloud_finops_scaled, CostProjection};
use crate::telemetry::TelemetryFrame;
use crate::workload::ProfileConfig;

/// Machine-readable & human-exportable white-box benchmark report (`oprobe.report.v1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: String,
    pub generated_unix_secs: u64,
    pub profile: ProfileConfig,
    pub active_knob: TuneKnob,
    pub frame: TelemetryFrame,
    pub multi_cloud_finops: Vec<CostProjection>,
    #[serde(default)]
    pub circuit_breaker_triggered: bool,
    #[serde(default)]
    pub trials: Vec<CandidateTrial>,
    #[serde(default)]
    pub generated_rust_code: String,
    #[serde(default)]
    pub generated_python_code: String,
    #[serde(default)]
    pub generated_toml_profile: String,
}

impl BenchmarkReport {
    pub fn from_single_run(profile: ProfileConfig, knob: TuneKnob, frame: TelemetryFrame) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let finops = compare_multi_cloud_finops_scaled(
            &frame.transport,
            profile.workload.kind.write_ratio(),
            profile.workload.internet_egress_ratio,
            profile.pricing.baseline_stored_tb,
            profile.pricing.monthly_logical_access_tb,
        );
        let rust_code = generate_rust_snippet(&profile.target.scheme, &knob);
        let python_code = generate_python_snippet(&profile.target.scheme, &knob);
        let toml_profile = generate_toml_snippet(&profile, &knob);
        Self {
            schema_version: "oprobe.report.v1".to_owned(),
            generated_unix_secs: now,
            profile,
            active_knob: knob,
            frame,
            multi_cloud_finops: finops,
            circuit_breaker_triggered: false,
            trials: Vec::new(),
            generated_rust_code: rust_code,
            generated_python_code: python_code,
            generated_toml_profile: toml_profile,
        }
    }

    pub fn from_autotune(profile: ProfileConfig, outcome: AutoTuneOutcome) -> Self {
        let mut rep = Self::from_single_run(profile, outcome.winner.knob, outcome.best_frame);
        rep.circuit_breaker_triggered = outcome.circuit_breaker_triggered;
        rep.trials = outcome.trials;
        rep.generated_rust_code = outcome.generated_rust_code;
        rep.generated_python_code = outcome.generated_python_code;
        rep.generated_toml_profile = outcome.generated_toml_profile;
        rep
    }

    pub fn save_json(&self, path: impl AsRef<Path>) -> Result<()> {
        let payload = serde_json::to_string_pretty(self)?;
        ensure_parent_dir(path.as_ref())?;
        std::fs::write(path, payload)?;
        Ok(())
    }

    pub fn load_json(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn to_markdown(&self) -> String {
        let mut md = String::with_capacity(6144);
        md.push_str(&format!(
            "# oprobe v1.0.0 White-Box Storage Diagnostic Report — `{}` (`{}`)\n\n",
            self.profile.target.name, self.profile.target.scheme
        ));
        md.push_str(&format!(
            "- **Workload**: `{}` | **Severity**: `{}` | **Elapsed**: `{:.2}s`\n",
            self.profile.workload.kind.title(),
            self.frame.diagnosis.severity.badge(),
            self.frame.transport.elapsed_secs
        ));
        md.push_str(&format!(
            "- **Root-Cause Diagnosis**: {}\n- **Prescription**: {}\n- **Active Knobs**: `concurrent={}`, `chunk={} KiB`, `gap={} KiB`, `cache={}`, `tail_cut={}`, `keep_alive={}`\n",
            self.frame.diagnosis.headline,
            self.frame.diagnosis.recommendation,
            self.active_knob.concurrent,
            self.active_knob.chunk_bytes / 1024,
            self.active_knob.range_gap_bytes / 1024,
            self.active_knob.enable_foyer,
            self.active_knob.enable_tail_cut,
            self.active_knob.keep_alive,
        ));
        if self.frame.cleanup.auto_cleanup_enabled {
            md.push_str(&format!(
                "- **Remote Cleanup**: `{}` (`{}/{} objects deleted`, `{}` remaining under `{}` in `{:.1} ms`)\n",
                if self.frame.cleanup.verified_clean {
                    "VERIFIED CLEAN"
                } else {
                    "PARTIAL"
                },
                self.frame.cleanup.objects_deleted,
                self.frame.cleanup.objects_created,
                self.frame.cleanup.remaining_objects,
                self.frame.cleanup.run_prefix,
                self.frame.cleanup.cleanup_ms
            ));
        }
        md.push('\n');

        md.push_str("## 1. Seven-Stage White-Box Latency Waterfall\n\n");
        md.push_str("| Stage | Mean Latency (ms) | Share (%) |\n| :--- | ---: | ---: |\n");
        for (stage, ms, pct) in self.frame.waterfall.breakdown_pct() {
            md.push_str(&format!("| {stage} | {ms:.3} | {pct:.1}% |\n"));
        }
        md.push_str(&format!(
            "| **Total Pipeline** | **{:.3}** | **100.0%** |\n\n",
            self.frame.waterfall.total_ms()
        ));

        let sq = &self.frame.stage_quantiles;
        if self.frame.transport.total_http_requests > 0 {
            md.push_str(&format!(
                "> **Wire Sub-Stage Quantiles**: Cold DNS P50/P99 `{:.2}/{:.2} ms` · Cold TCP RTT P50/P99 `{:.2}/{:.2} ms` · Cold TLS P50/P99 `{:.2}/{:.2} ms` · Gateway TTFB P50/P95/P99 `{:.2}/{:.2}/{:.2} ms` · Body Stream P50/P99 `{:.2}/{:.2} ms` · Multipart Merge P50/P99 `{:.2}/{:.2} ms`\n\n",
                sq.cold_dns_p50_ms,
                sq.cold_dns_p99_ms,
                sq.cold_tcp_p50_ms,
                sq.cold_tcp_p99_ms,
                sq.cold_tls_p50_ms,
                sq.cold_tls_p99_ms,
                sq.gateway_ttfb_p50_ms,
                sq.gateway_ttfb_p95_ms,
                sq.gateway_ttfb_p99_ms,
                sq.body_stream_p50_ms,
                sq.body_stream_p99_ms,
                sq.multipart_merge_p50_ms,
                sq.multipart_merge_p99_ms,
            ));
        }

        md.push_str("## 2. Per-Verb Isolated HDR Latency Quantiles\n\n");
        md.push_str("| Operation Verb | Count | OPS/s | P50 (ms) | P95 (ms) | P99 (ms) | P99.9 (ms) | Max (ms) | MiB/s | 429/503 (%) | Retries |\n");
        md.push_str("| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for v in &self.frame.verbs {
            md.push_str(&format!(
                "| `{}` | {} | {:.1} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1}% | {} |\n",
                v.verb,
                v.count,
                v.ops_per_sec,
                v.p50_ms,
                v.p95_ms,
                v.p99_ms,
                v.p999_ms,
                v.max_ms,
                v.throughput_mib_s,
                v.throttle_429_503_pct,
                v.retry_count
            ));
        }
        md.push('\n');

        let tp = &self.frame.transport;
        md.push_str("## 3. Transport Reuse, Amplification & Host Self-Check\n\n");
        md.push_str(&format!(
            "- **Effective Throughput**: `{:.2} MiB/s` logical (`{:.2} MiB/s` wire | `{:.2} MiB/s/Core` across `{}` cores, RSS `{} MiB`)\n",
            tp.aggregate_mib_s, tp.wire_mib_s, tp.mib_per_cpu_core, tp.cpu_cores, tp.process_rss_mib
        ));
        md.push_str(&format!(
            "- **Connection Pool & TLS**: Socket Reuse `{:.1}%` (`{}` warm / `{}` cold dials) | Protocol `{}` (`{}` / `{}`)\n",
            tp.socket_reuse_pct,
            tp.reused_connections,
            tp.cold_connections_dialed,
            tp.negotiated_alpn,
            tp.tls_version,
            tp.cipher_suite
        ));
        md.push_str(&format!(
            "- **Amplification & Cache**: Read Amp `{:.2}x` (Net Wire/Logical `{:.2}x`, Gap Waste `{} KiB`) | RPC Amp `{:.2}x` (`{} Class A` / `{} Class B`) | Local Cache Hit `{:.1}%` | CRC32C `{} ok / {} bad`\n\n",
            tp.read_amplification,
            tp.net_wire_read_ratio,
            tp.coalesced_gap_waste_bytes / 1024,
            tp.rpc_amplification,
            tp.class_a_rpcs,
            tp.class_b_rpcs,
            tp.cache_hit_ratio_pct,
            tp.crc32c_verified_ok,
            tp.crc32c_corrupted
        ));

        md.push_str(&format!(
            "## 4. Multi-Cloud Monthly FinOps Projection ({:.0} TB Stored / {:.0} TB Logical Access)\n\n",
            self.profile.pricing.baseline_stored_tb,
            self.profile.pricing.monthly_logical_access_tb
        ));
        md.push_str("| Cloud Provider | Storage ($/mo) | Class A+B API ($/mo) | Egress ($/mo) | Total ($/mo) | Amp Waste ($/mo) | Cache Saved ($/mo) |\n");
        md.push_str("| :--- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for c in &self.multi_cloud_finops {
            md.push_str(&format!(
                "| {} | ${:.2} | ${:.2} | ${:.2} | **${:.2}** | ${:.2} | ${:.2} |\n",
                c.provider_name,
                c.monthly_storage_usd,
                c.monthly_api_requests_usd,
                c.monthly_egress_usd,
                c.total_monthly_usd,
                c.waste_from_amplification_usd,
                c.saved_by_cache_usd
            ));
        }
        md.push('\n');

        md.push_str("## 5. Slow-Request `AwaitTree` Call-Stack Microscope\n\n```text\n");
        md.push_str(&self.frame.await_tree_dump);
        md.push_str("\n```\n");

        if !self.trials.is_empty() {
            md.push_str(&format!(
                "\n## 6. Auto-Tune Sweep Matrix (429/503 Circuit Breaker: `{}`)\n\n",
                if self.circuit_breaker_triggered {
                    "TRIGGERED & AVOIDED"
                } else {
                    "CLEAN"
                }
            ));
            md.push_str("| # | Phase | Conc | Chunk (KiB) | Gap (KiB) | Cache | Worst P99 (ms) | MiB/s | Read Amp | 429/503 | Score |\n");
            md.push_str("| ---: | :--- | ---: | ---: | ---: | :---: | ---: | ---: | ---: | ---: | ---: |\n");
            for t in &self.trials {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2}x | {} | {:.1} |\n",
                    t.trial_id,
                    t.phase,
                    t.knob.concurrent,
                    t.knob.chunk_bytes / 1024,
                    t.knob.range_gap_bytes / 1024,
                    if t.knob.enable_foyer { "on" } else { "off" },
                    t.worst_p99_ms,
                    t.aggregate_mib_s,
                    t.read_amplification,
                    t.throttle_events,
                    t.score
                ));
            }
        }

        md.push_str("\n## 7. Production Apache OpenDAL `Operator` Configuration\n\n```rust\n");
        md.push_str(&self.generated_rust_code);
        md.push_str("\n```\n");
        md
    }

    pub fn save_markdown(&self, path: impl AsRef<Path>) -> Result<()> {
        ensure_parent_dir(path.as_ref())?;
        std::fs::write(path, self.to_markdown())?;
        Ok(())
    }

    pub fn to_csv(&self) -> String {
        let mut out = String::from(
            "trial_id,phase,concurrent,chunk_kib,gap_kib,cache,tail_cut,worst_p99_ms,aggregate_mib_s,read_amp,rpc_amp,monthly_usd,throttle_events,throttled_503,sla_passed,score\n",
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
            let throttles = self.frame.transport.http_429_count
                + self.frame.transport.http_503_slowdown_count;
            out.push_str(&format!(
                "1,SingleRun,{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.2},{},{},{},0.0\n",
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
                throttles,
                throttles > 0,
                throttles == 0 && self.frame.transport.crc32c_corrupted == 0,
            ));
        } else {
            for t in &self.trials {
                out.push_str(&format!(
                    "{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.2},{},{},{},{:.2}\n",
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
                    t.throttle_events,
                    t.throttled_503,
                    t.sla_passed,
                    t.score,
                ));
            }
        }
        out
    }

    pub fn save_csv(&self, path: impl AsRef<Path>) -> Result<()> {
        ensure_parent_dir(path.as_ref())?;
        std::fs::write(path, self.to_csv())?;
        Ok(())
    }
}

pub(crate) fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}
