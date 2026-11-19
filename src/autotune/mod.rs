//! Closed-loop `429/503`-resilient SLA auto-tuner, binary concurrency cliff seeker,
//! and round-trip TOML / Rust / Python production configuration generator.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::cost::estimate_monthly_finops;
use crate::telemetry::TelemetryFrame;
use crate::workload::{execute_workload_with_options, ProfileConfig, WorkloadRunOptions};

fn default_conc() -> usize {
    16
}
fn default_chunk() -> usize {
    1024 * 1024
}
fn default_gap() -> usize {
    256 * 1024
}
fn default_true() -> bool {
    true
}

/// Tunable storage stack middleware & I/O parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuneKnob {
    #[serde(default = "default_conc")]
    pub concurrent: usize,
    #[serde(default = "default_chunk")]
    pub chunk_bytes: usize,
    #[serde(default = "default_gap")]
    pub range_gap_bytes: usize,
    #[serde(default = "default_true", alias = "enable_cache")]
    pub enable_foyer: bool,
    #[serde(default = "default_true")]
    pub enable_tail_cut: bool,
    #[serde(default = "default_true")]
    pub keep_alive: bool,
}

impl Default for TuneKnob {
    fn default() -> Self {
        Self {
            concurrent: default_conc(),
            chunk_bytes: default_chunk(),
            range_gap_bytes: default_gap(),
            enable_foyer: true,
            enable_tail_cut: true,
            keep_alive: true,
        }
    }
}

/// Service-Level Agreement (SLA) constraints used to score and filter candidate configurations.
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

/// Candidate parameter search space for [`AutoTuneEngine`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuneSearchSpace {
    pub concurrencies: Vec<usize>,
    pub chunks_bytes: Vec<usize>,
    pub gaps_bytes: Vec<usize>,
    pub trial_rounds: usize,
}

impl Default for TuneSearchSpace {
    fn default() -> Self {
        Self {
            concurrencies: vec![4, 8, 16, 32],
            chunks_bytes: vec![256 * 1024, 1024 * 1024, 4 * 1024 * 1024],
            gaps_bytes: vec![64 * 1024, 256 * 1024, 1024 * 1024],
            trial_rounds: 10,
        }
    }
}

/// Evaluation record for a single parameter trial.
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
    pub throttle_events: u64,
    pub throttled_503: bool,
    pub sla_passed: bool,
}

/// Final outcome of an adaptive auto-tuning sweep.
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

fn is_rate_limit_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("429")
        || msg.contains("503")
        || msg.contains("slowdown")
        || msg.contains("ratelimited")
        || msg.contains("too many requests")
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
        let throttle_events =
            frame.transport.http_503_slowdown_count + frame.transport.http_429_count;
        let throttled = throttle_events > 0;
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

        // Multi-objective utility score: reward throughput & cache efficiency,
        // heavily penalize 429/503 throttling, SLA P99 breaches, and read/RPC amplification waste.
        let throughput_term = frame.transport.aggregate_mib_s * 1.45;
        let latency_penalty = worst_p99 * 0.65
            + if worst_p99 > self.sla.max_p99_ms {
                (worst_p99 - self.sla.max_p99_ms) * 4.5
            } else {
                0.0
            };
        let amp_penalty = (frame.transport.read_amplification - 1.0).max(0.0) * 38.0
            + (frame.transport.rpc_amplification - 1.0).max(0.0) * 6.5
            + (cost.waste_from_amplification_usd * 0.02);
        let throttle_penalty = if throttled {
            10_000.0 + (throttle_events as f64 * 250.0)
        } else {
            0.0
        };
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
            throttle_events,
            throttled_503: throttled,
            sla_passed,
        }
    }

    fn synthesized_hard_throttled_trial(
        &self,
        trial_id: usize,
        phase: &str,
        knob: TuneKnob,
    ) -> CandidateTrial {
        CandidateTrial {
            trial_id,
            phase: phase.to_owned(),
            knob,
            score: -15_000.0,
            worst_p99_ms: self.sla.max_p99_ms * 4.0,
            aggregate_mib_s: 0.0,
            read_amplification: 1.0,
            rpc_amplification: 1.0,
            estimated_monthly_usd: 0.0,
            throttle_events: 5,
            throttled_503: true,
            sla_passed: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_one_candidate(
        &self,
        trial_id: usize,
        phase: &str,
        profile: &ProfileConfig,
        knob: TuneKnob,
        rounds: usize,
        have_clean_baseline: bool,
        cancel_flag: &Option<Arc<AtomicBool>>,
        progress_tx: &Option<mpsc::UnboundedSender<(CandidateTrial, TelemetryFrame)>>,
    ) -> anyhow::Result<(CandidateTrial, Option<TelemetryFrame>)> {
        let res = execute_workload_with_options(
            profile,
            &knob,
            WorkloadRunOptions {
                rounds_override: Some(rounds.max(4)),
                duration_override: None,
                warmup_override: None,
                cancel_flag: cancel_flag.clone(),
                live_frame_tx: None,
            },
        )
        .await;

        match res {
            Ok(frame) => {
                let cand = self.evaluate_trial(trial_id, phase, profile, knob, &frame);
                if let Some(tx) = progress_tx {
                    let _ = tx.send((cand.clone(), frame.clone()));
                }
                Ok((cand, Some(frame)))
            }
            Err(err) if is_rate_limit_error(&err) || have_clean_baseline => {
                let cand = self.synthesized_hard_throttled_trial(trial_id, phase, knob);
                if let Some(tx) = progress_tx {
                    let _ = tx.send((cand.clone(), TelemetryFrame::default()));
                }
                Ok((cand, None))
            }
            Err(fatal) => Err(fatal),
        }
    }

    /// Run closed-loop adaptive sweep with optional real-time trial progress streaming.
    pub async fn run_adaptive_sweep_with_progress(
        &self,
        profile: &ProfileConfig,
        progress_tx: Option<mpsc::UnboundedSender<(CandidateTrial, TelemetryFrame)>>,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> anyhow::Result<AutoTuneOutcome> {
        let mut trials: Vec<CandidateTrial> = Vec::new();
        let mut best_frame: Option<TelemetryFrame> = None;
        let mut best_trial: Option<CandidateTrial> = None;
        let mut circuit_breaker_triggered = false;
        let mut safe_max_concurrent = usize::MAX;
        let mut last_clean_conc: Option<usize> = None;
        let mut first_throttled_conc: Option<usize> = None;
        let mut trial_seq = 0_usize;

        let mut sorted_concs = self.space.concurrencies.clone();
        sorted_concs.sort_unstable();
        sorted_concs.dedup();
        if sorted_concs.is_empty() {
            sorted_concs.push(8);
        }

        let base_rounds = self.space.trial_rounds.max(4);

        // Phase 1: Coarse staircase sweep (Concurrency × Chunk) with 429/503 circuit breaker
        'conc_loop: for &conc in &sorted_concs {
            if cancel_flag
                .as_ref()
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(false)
            {
                break;
            }
            if conc > safe_max_concurrent {
                continue;
            }
            let mut conc_stayed_clean = true;
            for &chunk in &self.space.chunks_bytes {
                if cancel_flag
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    break 'conc_loop;
                }
                trial_seq += 1;
                let knob = TuneKnob {
                    concurrent: conc,
                    chunk_bytes: chunk,
                    range_gap_bytes: 256 * 1024,
                    enable_foyer: profile.workload.enable_foyer_cache,
                    enable_tail_cut: true,
                    keep_alive: true,
                };
                let (candidate, maybe_frame) = self
                    .run_one_candidate(
                        trial_seq,
                        "Phase1-Coarse",
                        profile,
                        knob,
                        base_rounds,
                        best_frame.is_some(),
                        &cancel_flag,
                        &progress_tx,
                    )
                    .await?;

                let throttled = candidate.throttled_503;
                if let Some(frame) = maybe_frame {
                    if best_trial
                        .as_ref()
                        .map(|b| candidate.score > b.score)
                        .unwrap_or(true)
                    {
                        best_trial = Some(candidate.clone());
                        best_frame = Some(frame);
                    }
                }
                trials.push(candidate);

                if throttled {
                    circuit_breaker_triggered = true;
                    conc_stayed_clean = false;
                    safe_max_concurrent = conc.saturating_sub(1);
                    if first_throttled_conc.is_none() {
                        first_throttled_conc = Some(conc);
                    }
                    // Allow remote rate-limit token bucket to replenish before probing lower concurrency
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    break;
                }
            }
            if conc_stayed_clean {
                last_clean_conc = Some(conc);
            }
        }

        // Fallback safety probe at conc=2 if even the lowest configured concurrency hit 429/503
        if best_frame.is_none() || best_trial.as_ref().map(|t| t.throttled_503).unwrap_or(true) {
            let safe_conc = sorted_concs
                .first()
                .copied()
                .unwrap_or(4)
                .div_ceil(2)
                .max(1);
            trial_seq += 1;
            let fallback_knob = TuneKnob {
                concurrent: safe_conc,
                chunk_bytes: 1024 * 1024,
                range_gap_bytes: 256 * 1024,
                enable_foyer: profile.workload.enable_foyer_cache,
                enable_tail_cut: true,
                keep_alive: true,
            };
            let (cand, maybe_frame) = self
                .run_one_candidate(
                    trial_seq,
                    "Phase1-SafeFallback",
                    profile,
                    fallback_knob,
                    base_rounds,
                    false,
                    &cancel_flag,
                    &progress_tx,
                )
                .await?;
            if let Some(frame) = maybe_frame {
                if best_trial
                    .as_ref()
                    .map(|b| cand.score > b.score)
                    .unwrap_or(true)
                {
                    best_trial = Some(cand.clone());
                    best_frame = Some(frame);
                }
            }
            if !cand.throttled_503 {
                last_clean_conc = Some(safe_conc);
            }
            trials.push(cand);
        }

        // Phase 1.5: Midpoint binary concurrency probe right below a detected 429/503 cliff
        if let (Some(low), Some(high)) = (last_clean_conc, first_throttled_conc) {
            if high.saturating_sub(low) >= 4
                && !cancel_flag
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(false)
            {
                let mid_conc = low + (high - low) / 2;
                let seed_chunk = best_trial
                    .as_ref()
                    .map(|t| t.knob.chunk_bytes)
                    .unwrap_or(1024 * 1024);
                let mid_knob = TuneKnob {
                    concurrent: mid_conc,
                    chunk_bytes: seed_chunk,
                    range_gap_bytes: 256 * 1024,
                    enable_foyer: profile.workload.enable_foyer_cache,
                    enable_tail_cut: true,
                    keep_alive: true,
                };
                if !trials.iter().any(|t| t.knob == mid_knob) {
                    trial_seq += 1;
                    let (cand, maybe_frame) = self
                        .run_one_candidate(
                            trial_seq,
                            "Phase1.5-CliffSeek",
                            profile,
                            mid_knob,
                            base_rounds,
                            best_frame.is_some(),
                            &cancel_flag,
                            &progress_tx,
                        )
                        .await?;
                    if cand.throttled_503 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    } else if let Some(frame) = maybe_frame {
                        if best_trial
                            .as_ref()
                            .map(|b| cand.score > b.score)
                            .unwrap_or(true)
                        {
                            best_trial = Some(cand.clone());
                            best_frame = Some(frame);
                        }
                    }
                    trials.push(cand);
                }
            }
        }

        // Phase 2: Coordinate refinement over range_gap_bytes, local cache, and TailCutLayer
        let seed_knob = best_trial.as_ref().map(|t| t.knob).unwrap_or_default();
        'refine_loop: for &gap in &self.space.gaps_bytes {
            for &cache_on in &[true, false] {
                if cache_on && !profile.workload.enable_foyer_cache {
                    continue;
                }
                if cancel_flag
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    break 'refine_loop;
                }
                let knob = TuneKnob {
                    range_gap_bytes: gap,
                    enable_foyer: cache_on,
                    ..seed_knob
                };
                if trials.iter().any(|t| t.knob == knob) {
                    continue;
                }
                trial_seq += 1;
                let (candidate, maybe_frame) = self
                    .run_one_candidate(
                        trial_seq,
                        "Phase2-Refine",
                        profile,
                        knob,
                        base_rounds + 2,
                        best_frame.is_some(),
                        &cancel_flag,
                        &progress_tx,
                    )
                    .await?;
                if let Some(frame) = maybe_frame {
                    if best_trial
                        .as_ref()
                        .map(|b| candidate.score > b.score)
                        .unwrap_or(true)
                    {
                        best_trial = Some(candidate.clone());
                        best_frame = Some(frame);
                    }
                }
                trials.push(candidate);
            }
        }

        let winner = best_trial
            .or_else(|| trials.first().cloned())
            .ok_or_else(|| anyhow::anyhow!("auto-tuner executed zero trials"))?;
        let best_frame = best_frame.unwrap_or_default();

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

    /// Run closed-loop adaptive sweep without a progress channel.
    pub async fn run_adaptive_sweep(&self, profile: &ProfileConfig) -> anyhow::Result<AutoTuneOutcome> {
        self.run_adaptive_sweep_with_progress(profile, None, None)
            .await
    }
}

/// Generate production-ready Apache OpenDAL `0.59` Rust `Operator` initialization code.
pub fn generate_rust_snippet(scheme: &str, knob: &TuneKnob) -> String {
    let io_conc = (knob.concurrent / 2).max(1);
    let chunk_bytes = knob.chunk_bytes;
    let gap_kib = knob.range_gap_bytes / 1024;

    let mut lines = vec![
        "use std::collections::HashMap;".to_owned(),
        "use std::time::Duration;".to_owned(),
        "use opendal::layers::{AwaitTreeLayer, ConcurrentLimitLayer, RetryLayer, TimeoutLayer};"
            .to_owned(),
    ];
    if knob.enable_tail_cut {
        lines.push("use opendal::layers::TailCutLayer;".to_owned());
    }
    lines.push("use opendal::{Operator, Reader, Writer};".to_owned());
    lines.push(String::new());
    lines.push(
        "/// Tuned Apache OpenDAL Operator generated by `oprobe v1.0.0` SLA Auto-Tuner."
            .to_owned(),
    );
    lines.push(
        "pub fn build_tuned_operator(cfg: HashMap<String, String>) -> opendal::Result<Operator> {"
            .to_owned(),
    );
    lines.push("    opendal::init_default_registry();".to_owned());
    lines.push(format!(
        "    let op = Operator::via_iter(\"{scheme}\", cfg)?"
    ));
    lines.push(
        "        .layer(TimeoutLayer::new().with_io_timeout(Duration::from_secs(15)))".to_owned(),
    );
    if knob.enable_tail_cut {
        lines.push(
            "        .layer(TailCutLayer::builder().percentile(95).safety_factor(1.35).build())"
                .to_owned(),
        );
    }
    lines.push(
        "        .layer(RetryLayer::default().with_jitter().with_min_delay(Duration::from_millis(35)).with_max_times(4))"
            .to_owned(),
    );
    lines.push("        .layer(AwaitTreeLayer::new())".to_owned());
    lines.push(format!(
        "        .layer(ConcurrentLimitLayer::new({}));",
        knob.concurrent
    ));
    lines.push("    Ok(op)".to_owned());
    lines.push("}".to_owned());
    lines.push(String::new());
    lines.push(format!(
        "/// Recommended Reader builder (chunk = {} KiB, concurrent = {io_conc}, range_coalesce_gap = {gap_kib} KiB).",
        chunk_bytes / 1024
    ));
    lines.push(
        "pub async fn open_tuned_reader(op: &Operator, path: &str) -> opendal::Result<Reader> {"
            .to_owned(),
    );
    lines.push(format!(
        "    op.reader_with(path).chunk({chunk_bytes}).gap({}).concurrent({io_conc}).await",
        knob.range_gap_bytes
    ));
    lines.push("}".to_owned());
    lines.push(String::new());
    lines.push(
        "pub async fn open_tuned_writer(op: &Operator, path: &str) -> opendal::Result<Writer> {"
            .to_owned(),
    );
    lines.push(format!(
        "    op.writer_with(path).chunk({chunk_bytes}).concurrent({io_conc}).await"
    ));
    lines.push("}".to_owned());
    lines.join("\n")
}

/// Generate production Python (`opendal.AsyncOperator`) snippet.
pub fn generate_python_snippet(scheme: &str, knob: &TuneKnob) -> String {
    format!(
        "import opendal\n\n\
# Tuned by oprobe v1.0.0 SLA Auto-Tuner\n\
TUNED_CHUNK_BYTES = {chunk}\n\
TUNED_IO_CONCURRENCY = {io_conc}\n\
TUNED_RANGE_GAP_BYTES = {gap}\n\n\
def build_tuned_async_operator(**kwargs) -> opendal.AsyncOperator:\n\
    op = opendal.AsyncOperator(\"{scheme}\", **kwargs)\n\
    op = op.layer(opendal.layers.RetryLayer(max_times=4, jitter=True))\n\
    op = op.layer(opendal.layers.ConcurrentLimitLayer({conc}))\n\
    return op\n",
        conc = knob.concurrent,
        io_conc = (knob.concurrent / 2).max(1),
        chunk = knob.chunk_bytes,
        gap = knob.range_gap_bytes,
    )
}

/// Generate a complete, round-trip executable TOML profile embedding `[knobs]`,
/// directly consumable via `oprobe run --profile <tuned.toml>`.
pub fn generate_toml_snippet(profile: &ProfileConfig, knob: &TuneKnob) -> String {
    let mut tuned_profile = profile.clone();
    tuned_profile.knobs = Some(*knob);
    let body = toml::to_string_pretty(&tuned_profile).unwrap_or_default();
    format!(
        "# Generated by oprobe v1.0.0 Closed-Loop SLA Auto-Tuner\n\
         # Directly reusable with: `oprobe run --profile <this-file.toml>`\n\n\
         {body}"
    )
}
