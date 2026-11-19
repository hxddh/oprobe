//! `clap` 命令行子命令定义与调度入口：
//! 支持 `tui`、`run`、`tune`、`diff`、`net-probe` 五大模式及旧版 `--headless-profile` 兼容参数。

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::autotune::{AutoTuneEngine, TuneKnob, TuneSearchSpace};
use crate::net_probe::probe_endpoint_handshakes;
use crate::report::diff::{diff_reports, RegressionThresholds};
use crate::report::BenchmarkReport;
use crate::tui::run_tui_workbench;
use crate::workload::{execute_workload_round, ProfileConfig, WorkloadKind};

#[derive(Parser, Debug)]
#[command(
    name = "oprobe",
    version,
    about = "Apache OpenDAL white-box storage stack profiler, socket/TLS probe, FinOps analyzer & SLA auto-tuner"
)]
pub struct Cli {
    /// Legacy CI flag alias for `oprobe tune --profile <PATH>`
    #[arg(long = "headless-profile", value_name = "TOML_PATH")]
    pub headless_profile: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Launch interactive terminal workbench (default when invoked in a TTY)
    Tui {
        /// Optional TOML profile path (defaults to in-memory sandbox demo)
        #[arg(short, long)]
        profile: Option<PathBuf>,
    },
    /// Execute a deterministic single-configuration white-box profiling run
    Run {
        #[arg(short, long)]
        profile: Option<PathBuf>,
        #[arg(long)]
        rounds: Option<usize>,
        #[arg(long)]
        concurrent: Option<usize>,
        #[arg(long)]
        chunk_kib: Option<usize>,
        #[arg(long)]
        gap_kib: Option<usize>,
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long)]
        markdown: Option<PathBuf>,
        #[arg(long)]
        csv: Option<PathBuf>,
    },
    /// Run 2-phase 503-circuit-breaking SLA auto-tuner and generate Operator code
    Tune {
        #[arg(short, long)]
        profile: Option<PathBuf>,
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long)]
        markdown: Option<PathBuf>,
        #[arg(long)]
        csv: Option<PathBuf>,
        #[arg(long)]
        emit_rust: Option<PathBuf>,
        #[arg(long)]
        emit_python: Option<PathBuf>,
    },
    /// Compare baseline vs candidate JSON reports and enforce CI regression gates (exit code 2 on breach)
    Diff {
        baseline: PathBuf,
        candidate: PathBuf,
        #[arg(long, default_value_t = 15.0)]
        max_p99_regression_pct: f64,
        #[arg(long, default_value_t = 0.90)]
        min_throughput_ratio: f64,
        #[arg(long)]
        markdown: Option<PathBuf>,
    },
    /// Microsecond DNS + TCP + TLS 1.3 + ALPN handshake profiler against an HTTP/HTTPS endpoint
    NetProbe {
        endpoint: String,
        #[arg(short, long, default_value_t = 4)]
        samples: usize,
        #[arg(long)]
        json: Option<PathBuf>,
    },
}

pub async fn load_or_default_profile(path: Option<&Path>) -> Result<ProfileConfig> {
    if let Some(p) = path {
        let raw = tokio::fs::read_to_string(p).await?;
        Ok(toml::from_str::<ProfileConfig>(&raw)?)
    } else {
        Ok(ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup))
    }
}

pub async fn dispatch_cli(cli: Cli) -> Result<ExitCode> {
    if let Some(legacy_path) = cli.headless_profile {
        let profile = if legacy_path.exists() {
            load_or_default_profile(Some(&legacy_path)).await?
        } else {
            ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup)
        };
        let engine = AutoTuneEngine::new(profile.sla.clone(), TuneSearchSpace::default());
        let outcome = engine.run_adaptive_sweep(&profile).await?;
        let report = BenchmarkReport::from_autotune(profile, outcome);
        println!("{}", report.to_markdown());
        return Ok(ExitCode::SUCCESS);
    }

    match cli.command {
        None => {
            let profile = ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup);
            if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
                run_tui_workbench(profile).await?;
            } else {
                let knob = TuneKnob::default();
                let frame = execute_workload_round(&profile, &knob, None).await?;
                let report = BenchmarkReport::from_single_run(profile, knob, frame);
                println!("{}", report.to_markdown());
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Tui { profile }) => {
            let prof = load_or_default_profile(profile.as_deref()).await?;
            run_tui_workbench(prof).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Run {
            profile,
            rounds,
            concurrent,
            chunk_kib,
            gap_kib,
            json,
            markdown,
            csv,
        }) => {
            let prof = load_or_default_profile(profile.as_deref()).await?;
            let mut knob = TuneKnob::default();
            if let Some(c) = concurrent {
                knob.concurrent = c.clamp(1, 256);
            }
            if let Some(ck) = chunk_kib {
                knob.chunk_bytes = ck.max(64) * 1024;
            }
            if let Some(gk) = gap_kib {
                knob.range_gap_bytes = gk * 1024;
            }

            let frame = execute_workload_round(&prof, &knob, rounds).await?;
            let report = BenchmarkReport::from_single_run(prof, knob, frame);

            if let Some(p) = json {
                report.save_json(&p)?;
            }
            if let Some(p) = markdown {
                report.save_markdown(&p)?;
            }
            if let Some(p) = csv {
                report.save_csv(&p)?;
            }
            println!("{}", report.to_markdown());
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Tune {
            profile,
            json,
            markdown,
            csv,
            emit_rust,
            emit_python,
        }) => {
            let prof = load_or_default_profile(profile.as_deref()).await?;
            let engine = AutoTuneEngine::new(prof.sla.clone(), TuneSearchSpace::default());
            let outcome = engine.run_adaptive_sweep(&prof).await?;

            if let Some(p) = emit_rust {
                std::fs::write(p, &outcome.generated_rust_code)?;
            }
            if let Some(p) = emit_python {
                std::fs::write(p, &outcome.generated_python_code)?;
            }

            let report = BenchmarkReport::from_autotune(prof, outcome);
            if let Some(p) = json {
                report.save_json(&p)?;
            }
            if let Some(p) = markdown {
                report.save_markdown(&p)?;
            }
            if let Some(p) = csv {
                report.save_csv(&p)?;
            }
            println!("{}", report.to_markdown());
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Diff {
            baseline,
            candidate,
            max_p99_regression_pct,
            min_throughput_ratio,
            markdown,
        }) => {
            let base_rep = BenchmarkReport::load_json(&baseline)?;
            let cand_rep = BenchmarkReport::load_json(&candidate)?;
            let thresholds = RegressionThresholds {
                max_p99_regression_pct,
                min_throughput_ratio,
                ..Default::default()
            };
            let diff = diff_reports(&base_rep, &cand_rep, &thresholds);
            let md = diff.to_markdown();
            if let Some(p) = markdown {
                std::fs::write(p, &md)?;
            }
            println!("{md}");
            if diff.passed_gate {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(2))
            }
        }
        Some(Commands::NetProbe {
            endpoint,
            samples,
            json,
        }) => {
            let summary =
                probe_endpoint_handshakes(&endpoint, samples, Duration::from_secs(5)).await?;
            let serialized = serde_json::to_string_pretty(&summary)?;
            if let Some(p) = json {
                std::fs::write(p, &serialized)?;
            }
            println!("{serialized}");
            Ok(ExitCode::SUCCESS)
        }
    }
}
