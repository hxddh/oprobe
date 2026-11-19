//! 带 `503 SlowDown` 熔断保护的两阶段坐标下降参数寻优器与多语言生产代码生成器：
//! - 阶段 1（粗粒度安全阶梯扫描）：探测并发上限与最佳分块量级，一旦捕获 `429/503` 立即熔断更高并发；
//! - 阶段 2（局部坐标精调）：围绕最优种子微调 `range_gap_bytes`、`FoyerLayer` 与 `TailCutLayer`；
//! - 输出可直接粘贴的 Rust (`opendal 0.59`)、Python (`opendal` async) 与 TOML 配置。

use serde::{Deserialize, Serialize};

use crate::cost::estimate_monthly_finops;
use crate::telemetry::TelemetryFrame;
use crate::workload::{execute_workload_round, ProfileConfig};

/// 可调存储栈旋钮组合
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuneKnob {
    pub concurrent: usize,
    pub chunk_bytes: usize,
    pub range_gap_bytes: usize,
    pub enable_foyer: bool,
    pub enable_tail_cut: bool,
    pub keep_alive: bool,
}

impl Default for TuneKnob {
    fn default() -> Self {
        Self {
            concurrent: 16,
            chunk_bytes: 1024 * 1024,
            range_gap_bytes: 512 * 1024,
            enable_foyer: true,
            enable_tail_cut: true,
            keep_alive: true,
        }
    }
}

/// 业务 SLA 约束策略
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlaPolicy {
    #[serde(default = "default_max_p99")]
    pub max_p99_ms: f64,
    #[serde(default = "default_min_mib_s")]
    pub min_throughput_mib_s: f64,
    #[serde(default = "default_max_read_amp")]
    pub max_read_amplification: f64,
}

fn default_max_p99() -> f64 {
    180.0
}
fn default_min_mib_s() -> f64 {
    20.0
}
fn default_max_read_amp() -> f64 {
    2.5
}

impl Default for SlaPolicy {
    fn default() -> Self {
        Self {
            max_p99_ms: default_max_p99(),
            min_throughput_mib_s: default_min_mib_s(),
            max_read_amplification: default_max_read_amp(),
        }
    }
}

/// 寻优候选空间
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuneSearchSpace {
    pub concurrencies: Vec<usize>,
    pub chunks_bytes: Vec<usize>,
    pub gaps_bytes: Vec<usize>,
}

impl Default for TuneSearchSpace {
    fn default() -> Self {
        Self {
            concurrencies: vec![8, 16, 32],
            chunks_bytes: vec![256 * 1024, 1024 * 1024, 4 * 1024 * 1024],
            gaps_bytes: vec![64 * 1024, 256 * 1024, 1024 * 1024],
        }
    }
}

/// 单组试验评估记录
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateTrial {
    pub trial_id: usize,
    pub phase: String,
    pub knob: TuneKnob,
    pub score: f64,
    pub worst_p99_ms: f64,
    pub aggregate_mib_s: f64,
    pub read_amplification: f64,
    pub rpc_amplification: f64,
    pub estimated_monthly_usd: f64,
    pub throttled_503: bool,
    pub sla_passed: bool,
}

/// 自动寻优最终产物
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoTuneOutcome {
    pub winner: CandidateTrial,
    pub trials: Vec<CandidateTrial>,
    pub best_frame: TelemetryFrame,
    pub circuit_breaker_triggered: bool,
    pub generated_rust_code: String,
    pub generated_python_code: String,
    pub generated_toml_profile: String,
}

pub struct AutoTuneEngine {
    pub sla: SlaPolicy,
    pub space: TuneSearchSpace,
}

impl AutoTuneEngine {
    pub fn new(sla: SlaPolicy, space: TuneSearchSpace) -> Self {
        Self { sla, space }
    }

    fn evaluate_trial(
        &self,
        trial_id: usize,
        phase: &str,
        profile: &ProfileConfig,
        knob: TuneKnob,
        frame: &TelemetryFrame,
    ) -> CandidateTrial {
        let worst_p99 = frame
            .verbs
            .iter()
            .map(|v| v.p99_ms)
            .fold(0.0_f64, f64::max);
        let throttled =
            frame.transport.http_503_slowdown_count > 0 || frame.transport.http_429_count > 0;
        let cost = estimate_monthly_finops(
            &profile.pricing,
            &frame.transport,
            profile.workload.kind.write_ratio(),
            profile.workload.internet_egress_ratio,
        );

        let sla_passed = !throttled
            && frame.transport.crc32c_corrupted == 0
            && worst_p99 <= self.sla.max_p99_ms
            && frame.transport.read_amplification <= self.sla.max_read_amplification;

        // 综合效用函数：奖励有效吞吐，惩罚 P99 超标、读写放大与 503 限流
        let throughput_term = frame.transport.aggregate_mib_s * 1.4;
        let latency_penalty = worst_p99 * 0.65
            + if worst_p99 > self.sla.max_p99_ms {
                (worst_p99 - self.sla.max_p99_ms) * 4.0
            } else {
                0.0
            };
        let amp_penalty =
            (frame.transport.read_amplification - 1.0) * 35.0 + (frame.transport.rpc_amplification - 1.0) * 6.0;
        let throttle_penalty = if throttled { 10_000.0 } else { 0.0 };
        let score = throughput_term - latency_penalty - amp_penalty - throttle_penalty;

        CandidateTrial {
            trial_id,
            phase: phase.to_owned(),
            knob,
            score,
            worst_p99_ms: worst_p99,
            aggregate_mib_s: frame.transport.aggregate_mib_s,
            read_amplification: frame.transport.read_amplification,
            rpc_amplification: frame.transport.rpc_amplification,
            estimated_monthly_usd: cost.total_monthly_usd,
            throttled_503: throttled,
            sla_passed,
        }
    }

    /// 执行两阶段自适应寻优
    pub async fn run_adaptive_sweep(&self, profile: &ProfileConfig) -> anyhow::Result<AutoTuneOutcome> {
        let mut trials = Vec::new();
        let mut best_frame: Option<TelemetryFrame> = None;
        let mut best_trial: Option<CandidateTrial> = None;
        let mut circuit_breaker_triggered = false;
        let mut safe_max_concurrent = usize::MAX;
        let mut trial_seq = 0_usize;

        // Phase 1: 粗粒度阶梯扫描 (Concurrency × Chunk)
        for &conc in &self.space.concurrencies {
            if conc > safe_max_concurrent {
                continue;
            }
            for &chunk in &self.space.chunks_bytes {
                trial_seq += 1;
                let knob = TuneKnob {
                    concurrent: conc,
                    chunk_bytes: chunk,
                    range_gap_bytes: 256 * 1024,
                    enable_foyer: profile.workload.enable_foyer_cache,
                    enable_tail_cut: true,
                    keep_alive: true,
                };
                let frame = execute_workload_round(profile, &knob, Some(10)).await?;
                let candidate = self.evaluate_trial(trial_seq, "Phase1-Coarse", profile, knob, &frame);

                if candidate.throttled_503 {
                    circuit_breaker_triggered = true;
                    safe_max_concurrent = conc.saturating_sub(1);
                }

                if best_trial
                    .as_ref()
                    .map(|b| candidate.score > b.score)
                    .unwrap_or(true)
                {
                    best_trial = Some(candidate.clone());
                    best_frame = Some(frame);
                }
                trials.push(candidate);

                if conc > safe_max_concurrent {
                    break;
                }
            }
        }

        // Phase 2: 围绕阶段 1 最优种子精调 Gap 合并窗口与缓存/长尾开关
        let seed_knob = best_trial.as_ref().map(|t| t.knob).unwrap_or_default();
        for &gap in &self.space.gaps_bytes {
            for &foyer_on in &[true, false] {
                if foyer_on && !profile.workload.enable_foyer_cache {
                    continue;
                }
                let knob = TuneKnob {
                    range_gap_bytes: gap,
                    enable_foyer: foyer_on,
                    ..seed_knob
                };
                if trials.iter().any(|t| t.knob == knob) {
                    continue;
                }
                trial_seq += 1;
                let frame = execute_workload_round(profile, &knob, Some(12)).await?;
                let candidate = self.evaluate_trial(trial_seq, "Phase2-Refine", profile, knob, &frame);
                if best_trial
                    .as_ref()
                    .map(|b| candidate.score > b.score)
                    .unwrap_or(true)
                {
                    best_trial = Some(candidate.clone());
                    best_frame = Some(frame);
                }
                trials.push(candidate);
            }
        }

        let winner = best_trial.expect("at least one trial executed");
        let best_frame = best_frame.expect("at least one telemetry frame captured");

        Ok(AutoTuneOutcome {
            generated_rust_code: generate_rust_snippet(&profile.target.scheme, &winner.knob),
            generated_python_code: generate_python_snippet(&profile.target.scheme, &winner.knob),
            generated_toml_profile: generate_toml_snippet(profile, &winner.knob),
            winner,
            trials,
            best_frame,
            circuit_breaker_triggered,
        })
    }
}

/// 生成可直接用于生产的 Rust (`opendal 0.59`) `Operator` 初始化代码
pub fn generate_rust_snippet(scheme: &str, knob: &TuneKnob) -> String {
    let mut lines = vec![
        "use std::collections::HashMap;".to_owned(),
        "use std::time::Duration;".to_owned(),
        "use opendal::layers::{AwaitTreeLayer, ConcurrentLimitLayer, RetryLayer, TimeoutLayer};".to_owned(),
    ];
    if knob.enable_tail_cut {
        lines.push("use opendal::layers::TailCutLayer;".to_owned());
    }
    if knob.enable_foyer {
        lines.push("use foyer::HybridCacheBuilder;".to_owned());
        lines.push("use opendal::layers::FoyerLayer;".to_owned());
    }
    lines.push("use opendal::Operator;".to_owned());
    lines.push(String::new());
    lines.push("pub async fn build_tuned_operator(cfg: HashMap<String, String>) -> anyhow::Result<Operator> {".to_owned());
    lines.push("    opendal::init_default_registry();".to_owned());
    lines.push(format!(
        "    let op = Operator::via_iter(\"{scheme}\", cfg)?"
    ));
    lines.push("        .layer(TimeoutLayer::new().with_io_timeout(Duration::from_secs(15)))".to_owned());
    lines.push("        .layer(RetryLayer::default().with_jitter().with_max_times(4))".to_owned());
    lines.push(format!(
        "        .layer(ConcurrentLimitLayer::new({}))",
        knob.concurrent
    ));
    if knob.enable_tail_cut {
        lines.push("        .layer(TailCutLayer::builder().build())".to_owned());
    }
    lines.push("        .layer(AwaitTreeLayer::new());".to_owned());

    if knob.enable_foyer {
        lines.push("    let cache = HybridCacheBuilder::new()".to_owned());
        lines.push("        .memory(128 * 1024 * 1024)".to_owned());
        lines.push("        .with_shards(4)".to_owned());
        lines.push("        .storage()".to_owned());
        lines.push("        .build()".to_owned());
        lines.push("        .await?;".to_owned());
        lines.push("    let op = op.layer(FoyerLayer::new(cache));".to_owned());
    }

    lines.push(format!(
        "    // Tuned Reader/Writer options: chunk = {} KiB, concurrent = {}, coalesced gap = {} KiB",
        knob.chunk_bytes / 1024,
        (knob.concurrent / 2).max(1),
        knob.range_gap_bytes / 1024
    ));
    lines.push("    Ok(op)".to_owned());
    lines.push("}".to_owned());
    lines.join("\n")
}

/// 生成 Python (`opendal` PyO3 异步绑定) 参考配置代码
pub fn generate_python_snippet(scheme: &str, knob: &TuneKnob) -> String {
    format!(
        "import opendal\n\n\
def build_tuned_async_operator(**kwargs) -> opendal.AsyncOperator:\n\
    op = opendal.AsyncOperator(\"{scheme}\", **kwargs)\n\
    op = op.layer(opendal.layers.RetryLayer(max_times=4, jitter=True))\n\
    op = op.layer(opendal.layers.ConcurrentLimitLayer({conc}))\n\
    # Recommended read/write chunk={chunk_kib}KiB, range_coalesce_gap={gap_kib}KiB\n\
    return op\n",
        conc = knob.concurrent,
        chunk_kib = knob.chunk_bytes / 1024,
        gap_kib = knob.range_gap_bytes / 1024,
    )
}

/// 导出最优参数对应的 TOML Profile
pub fn generate_toml_snippet(profile: &ProfileConfig, knob: &TuneKnob) -> String {
    format!(
        "# Generated by oprobe v0.1.0 SLA Auto-Tuner\n\
[target]\n\
name = \"{name}\"\n\
scheme = \"{scheme}\"\n\n\
[tuned_knobs]\n\
concurrent = {conc}\n\
chunk_bytes = {chunk}\n\
range_gap_bytes = {gap}\n\
enable_foyer = {foyer}\n\
enable_tail_cut = {tail_cut}\n\
keep_alive = {keep_alive}\n",
        name = profile.target.name,
        scheme = profile.target.scheme,
        conc = knob.concurrent,
        chunk = knob.chunk_bytes,
        gap = knob.range_gap_bytes,
        foyer = knob.enable_foyer,
        tail_cut = knob.enable_tail_cut,
        keep_alive = knob.keep_alive,
    )
}
