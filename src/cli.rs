//! `clap` command-line interface & subcommand dispatcher for `oprobe v1.0.0`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::mpsc;

use crate::autotune::{AutoTuneEngine, CandidateTrial, TuneSearchSpace};
use crate::net_probe::probe_endpoint_handshakes_with_tls;
use crate::report::diff::{diff_reports, RegressionThresholds};
use crate::report::terminal::{
    render_diff_terminal, render_error_terminal, render_handshake_terminal, render_report_terminal,
};
use crate::report::BenchmarkReport;
use crate::telemetry::{diagnose_runtime_error, TelemetryFrame};
use crate::testbed::{ChaosFaultConfig, S3ChaosServer, TESTBED_TLS_CERT_PEM};
use crate::tls::TlsClientOptions;
use crate::tui::run_tui_workbench;
use crate::workload::{
    build_admin_operator, execute_workload_with_options, parse_duration_str,
    sweep_all_oprobe_prefixes, ProfileConfig, WorkloadKind, WorkloadRunOptions,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Terminal,
    Markdown,
    Json,
}

/// Shared storage target resolution flags usable across `run`, `tune`, `tui`, and `cleanup`.
#[derive(Args, Clone, Debug, Default)]
pub struct TargetSelectionArgs {
    /// Path to a TOML profile (e.g. `profiles/s3-prod.toml`)
    #[arg(short, long)]
    pub profile: Option<PathBuf>,

    /// Zero-config storage URL (e.g. `s3://my-bucket/bench`, `fs:///tmp/bench`, `memory://`)
    #[arg(long)]
    pub url: Option<String>,

    /// Custom HTTP/HTTPS endpoint URL override (e.g. `http://127.0.0.1:9000`)
    #[arg(long)]
    pub endpoint: Option<String>,

    /// Cloud region override (e.g. `us-east-1`, `ap-northeast-1`, `auto`)
    #[arg(long)]
    pub region: Option<String>,

    /// Workload model override (`parquet`, `checkpoint`, `metadata`, `mixed`)
    #[arg(short, long)]
    pub workload: Option<String>,

    /// Trust a custom PEM CA certificate file for private HTTPS endpoints
    #[arg(long)]
    pub ca_cert: Option<PathBuf>,

    /// Skip TLS X.509 certificate verification (local dev/self-signed endpoints only)
    #[arg(long)]
    pub insecure: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "oprobe",
    version,
    about = "Production white-box cloud storage stack microscope, TCP/TLS/HTTP profiler, FinOps analyzer & SLA auto-tuner powered by Apache OpenDAL",
    after_help = "Examples:\n  \
      oprobe run --url memory:// --duration 2s\n  \
      oprobe run --url s3://my-bucket/bench --endpoint https://s3.amazonaws.com --duration 10s --json baseline.json\n  \
      oprobe tune --profile profiles/s3-prod.toml --emit-profile tuned.toml --emit-rust operator.rs\n  \
      oprobe diff baseline.json candidate.json --max-p99-regression-pct 15\n  \
      oprobe net-probe https://s3.ap-northeast-1.amazonaws.com --samples 4"
)]
pub struct Cli {
    /// Legacy CI alias for `oprobe tune --profile <TOML_PATH>`
    #[arg(long = "headless-profile", value_name = "TOML_PATH", hide = true)]
    pub headless_profile: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Launch the real-time streaming interactive terminal workbench (default in a TTY)
    Tui {
        #[command(flatten)]
        target: TargetSelectionArgs,
    },

    /// Execute a white-box storage benchmark (round-bounded or continuous time-window streaming)
    Run {
        #[command(flatten)]
        target: TargetSelectionArgs,

        /// Continuous streaming measurement window (e.g. `10s`, `500ms`, `1m`; overrides `--rounds`)
        #[arg(short, long)]
        duration: Option<String>,

        /// Optional steady-state warmup window before measurement counters reset (e.g. `1s`)
        #[arg(long)]
        warmup: Option<String>,

        /// Number of logical workload operations when `--duration` is not specified
        #[arg(long)]
        rounds: Option<usize>,

        /// Seeded / shard object size in MiB
        #[arg(long)]
        object_size_mib: Option<usize>,

        /// Client concurrency limit (`ConcurrentLimitLayer`)
        #[arg(long)]
        concurrent: Option<usize>,

        /// Streaming reader/writer chunk size in KiB
        #[arg(long)]
        chunk_kib: Option<usize>,

        /// Sparse range-read coalescing gap threshold in KiB
        #[arg(long)]
        gap_kib: Option<usize>,

        /// Disable local W-TinyLFU cache layer
        #[arg(long)]
        no_cache: bool,

        /// Disable adaptive `TailCutLayer` straggler cancellation
        #[arg(long)]
        no_tail_cut: bool,

        /// Disable HTTP connection pool reuse (`Connection: close` per request)
        #[arg(long)]
        no_keep_alive: bool,

        /// Keep remote benchmark objects instead of running automatic post-run cleanup
        #[arg(long)]
        keep_remote_data: bool,

        /// Export machine-readable report snapshot to JSON
        #[arg(long)]
        json: Option<PathBuf>,

        /// Export full white-box diagnostic report to Markdown
        #[arg(long)]
        markdown: Option<PathBuf>,

        /// Export metrics row to CSV
        #[arg(long)]
        csv: Option<PathBuf>,

        /// Stdout display format (`terminal`, `markdown`, `json`)
        #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
        format: OutputFormat,

        /// Suppress live stderr progress updates
        #[arg(short, long)]
        quiet: bool,
    },

    /// Run closed-loop 429/503-resilient SLA auto-tuner and emit reusable configs & Operator code
    Tune {
        #[command(flatten)]
        target: TargetSelectionArgs,

        /// Operations executed per candidate trial
        #[arg(long)]
        trial_rounds: Option<usize>,

        /// Override SLA maximum P99 latency target (ms)
        #[arg(long)]
        max_p99_ms: Option<f64>,

        /// Override SLA maximum read amplification factor
        #[arg(long)]
        max_read_amp: Option<f64>,

        /// Export reusable round-trip TOML profile containing `[knobs]`
        #[arg(long)]
        emit_profile: Option<PathBuf>,

        /// Export production Rust (`opendal 0.59`) `Operator` code
        #[arg(long)]
        emit_rust: Option<PathBuf>,

        /// Export production Python (`opendal.AsyncOperator`) code
        #[arg(long)]
        emit_python: Option<PathBuf>,

        /// Export full sweep report to JSON
        #[arg(long)]
        json: Option<PathBuf>,

        /// Export full sweep report to Markdown
        #[arg(long)]
        markdown: Option<PathBuf>,

        /// Export candidate sweep matrix to CSV
        #[arg(long)]
        csv: Option<PathBuf>,

        /// Stdout display format (`terminal`, `markdown`, `json`)
        #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
        format: OutputFormat,

        /// Suppress live stderr progress updates
        #[arg(short, long)]
        quiet: bool,
    },

    /// Compare baseline vs candidate JSON snapshots for CI regression gating (exit code 2 on breach)
    Diff {
        /// Baseline JSON report path
        baseline: PathBuf,

        /// Candidate JSON report path
        candidate: PathBuf,

        /// Maximum allowed per-verb P99 regression percentage
        #[arg(long, default_value_t = 15.0)]
        max_p99_regression_pct: f64,

        /// Minimum absolute P99 latency increase (ms) before triggering regression
        #[arg(long, default_value_t = 0.75)]
        min_latency_delta_ms: f64,

        /// Minimum required candidate/baseline aggregate throughput ratio
        #[arg(long, default_value_t = 0.90)]
        min_throughput_ratio: f64,

        /// Maximum allowed read amplification increase
        #[arg(long, default_value_t = 0.25)]
        max_read_amp_delta: f64,

        /// Maximum allowed RPC split amplification increase
        #[arg(long, default_value_t = 0.75)]
        max_rpc_amp_delta: f64,

        /// Maximum allowed monthly cloud bill increase percentage
        #[arg(long, default_value_t = 20.0)]
        max_cost_increase_pct: f64,

        /// Write Markdown diff report to file
        #[arg(long)]
        markdown: Option<PathBuf>,

        /// Write JSON diff summary to file
        #[arg(long)]
        json: Option<PathBuf>,

        /// Stdout display format (`terminal`, `markdown`, `json`)
        #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
        format: OutputFormat,
    },

    /// Microsecond DNS + TCP + TLS 1.2/1.3 + ALPN + Cold-vs-Warm HTTP HEAD microscope
    NetProbe {
        /// HTTP or HTTPS endpoint URL (e.g. `https://s3.amazonaws.com`)
        endpoint: String,

        /// Number of independent cold connection samples
        #[arg(short, long, default_value_t = 4)]
        samples: usize,

        /// Per-stage socket/TLS timeout in milliseconds
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,

        /// Advertise `http/1.1` only in ALPN (disable `h2`)
        #[arg(long)]
        http1_only: bool,

        /// Trust a custom PEM CA certificate file
        #[arg(long)]
        ca_cert: Option<PathBuf>,

        /// Skip TLS X.509 certificate verification
        #[arg(long)]
        insecure: bool,

        /// Save handshake summary JSON to file
        #[arg(long)]
        json: Option<PathBuf>,

        /// Stdout display format (`terminal`, `json`)
        #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
        format: OutputFormat,
    },

    /// Sweep and delete any orphaned `oprobe-run-*` test objects on the target storage
    Cleanup {
        #[command(flatten)]
        target: TargetSelectionArgs,
    },

    /// Start a local S3-compatible HTTP/HTTPS chaos & rate-limit testbed server for realistic verification
    ServeS3 {
        /// Listen address (e.g. `127.0.0.1:9000`)
        #[arg(long, default_value = "127.0.0.1:9000")]
        bind: String,

        /// Enable TLS 1.3 HTTPS with embedded localhost/127.0.0.1 certificate
        #[arg(long)]
        tls: bool,

        /// Enable HTTP/2 multiplexing (`h2`)
        #[arg(long, default_value_t = true)]
        http2: bool,

        /// Simulated base gateway TTFB latency in milliseconds
        #[arg(long, default_value_t = 0)]
        ttfb_ms: u64,

        /// Simulated `CompleteMultipartUpload` merge latency in milliseconds
        #[arg(long, default_value_t = 0)]
        merge_ms: u64,

        /// Inject a straggler tail delay every N requests (`0` = disabled)
        #[arg(long, default_value_t = 0)]
        straggler_every: usize,

        /// Straggler delay duration in milliseconds
        #[arg(long, default_value_t = 120)]
        straggler_ms: u64,

        /// Return `503 SlowDown` when concurrent server inflight exceeds this threshold
        #[arg(long)]
        max_inflight_503: Option<usize>,

        /// Cap response body streaming bandwidth (KiB/s)
        #[arg(long)]
        bandwidth_kib_s: Option<u64>,

        /// Write the embedded CA certificate PEM to disk for `--ca-cert`
        #[arg(long)]
        write_ca_cert: Option<PathBuf>,
    },
}

pub async fn resolve_profile(sel: &TargetSelectionArgs) -> Result<ProfileConfig> {
    let mut prof = if let Some(ref p) = sel.profile {
        let raw = tokio::fs::read_to_string(p)
            .await
            .with_context(|| format!("failed to read profile `{}`", p.display()))?;
        toml::from_str::<ProfileConfig>(&raw)
            .with_context(|| format!("invalid TOML syntax in `{}`", p.display()))?
    } else if let Some(ref u) = sel.url {
        ProfileConfig::from_storage_url(
            u,
            sel.endpoint.as_deref(),
            sel.region.as_deref(),
            sel.insecure,
            sel.ca_cert.as_deref(),
        )?
    } else {
        ProfileConfig::memory_demo(WorkloadKind::ParquetPointLookup)
    };

    if let Some(ref ep) = sel.endpoint {
        prof.target.options.insert("endpoint".to_owned(), ep.clone());
    }
    if let Some(ref reg) = sel.region {
        prof.target.options.insert("region".to_owned(), reg.clone());
    }
    if sel.insecure {
        prof.target
            .options
            .insert("allow_insecure_tls".to_owned(), "true".to_owned());
    }
    if let Some(ref ca) = sel.ca_cert {
        prof.target.options.insert(
            "ca_cert_path".to_owned(),
            ca.to_string_lossy().into_owned(),
        );
    }
    if let Some(ref wk_str) = sel.workload {
        prof.workload.kind = WorkloadKind::from_slug(wk_str)?;
    }

    Ok(prof)
}

pub async fn load_or_default_profile(path: Option<&Path>) -> Result<ProfileConfig> {
    let sel = TargetSelectionArgs {
        profile: path.map(Path::to_path_buf),
        ..Default::default()
    };
    resolve_profile(&sel).await
}

fn spawn_ctrl_c_watcher() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let flag_bg = flag.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            flag_bg.store(true, Ordering::Relaxed);
        }
    });
    flag
}

fn emit_report_stdout(rep: &BenchmarkReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Terminal => print!("{}", render_report_terminal(rep)),
        OutputFormat::Markdown => println!("{}", rep.to_markdown()),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(rep)?),
    }
    Ok(())
}

/// Dispatch parsed CLI arguments and format any runtime failure with actionable diagnostics.
pub async fn run_cli_with_diagnostics(cli: Cli) -> ExitCode {
    let inferred_scheme = infer_scheme_hint(&cli);
    match dispatch_cli(cli).await {
        Ok(code) => code,
        Err(err) => {
            let err_str = format!("{err:#}");
            let hint = diagnose_runtime_error(&err_str, &inferred_scheme);
            eprint!("{}", render_error_terminal(&err, &hint));
            ExitCode::FAILURE
        }
    }
}

fn infer_scheme_hint(cli: &Cli) -> String {
    let url_opt = match &cli.command {
        Some(Commands::Run { target, .. })
        | Some(Commands::Tune { target, .. })
        | Some(Commands::Tui { target })
        | Some(Commands::Cleanup { target }) => target.url.as_deref(),
        Some(Commands::NetProbe { endpoint, .. }) => Some(endpoint.as_str()),
        _ => None,
    };
    if let Some(u) = url_opt {
        if let Some((sch, _)) = u.split_once("://") {
            return sch.to_owned();
        }
    }
    "s3".to_owned()
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
                let knob = profile.effective_knob();
                let frame =
                    execute_workload_with_options(&profile, &knob, WorkloadRunOptions::default())
                        .await?;
                let report = BenchmarkReport::from_single_run(profile, knob, frame);
                print!("{}", render_report_terminal(&report));
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Tui { target }) => {
            let prof = resolve_profile(&target).await?;
            run_tui_workbench(prof).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Run {
            target,
            duration,
            warmup,
            rounds,
            object_size_mib,
            concurrent,
            chunk_kib,
            gap_kib,
            no_cache,
            no_tail_cut,
            no_keep_alive,
            keep_remote_data,
            json,
            markdown,
            csv,
            format,
            quiet,
        }) => {
            let mut prof = resolve_profile(&target).await?;
            if let Some(sz) = object_size_mib {
                prof.workload.object_size_mib = sz.clamp(1, 256);
            }
            if keep_remote_data {
                prof.workload.auto_cleanup = false;
            }

            let mut knob = prof.effective_knob();
            if let Some(c) = concurrent {
                knob.concurrent = c.clamp(1, 256);
            }
            if let Some(ck) = chunk_kib {
                knob.chunk_bytes = ck.max(64) * 1024;
            }
            if let Some(gk) = gap_kib {
                knob.range_gap_bytes = gk * 1024;
            }
            if no_cache {
                knob.enable_foyer = false;
            }
            if no_tail_cut {
                knob.enable_tail_cut = false;
            }
            if no_keep_alive {
                knob.keep_alive = false;
            }

            let duration_override = match duration {
                Some(ref s) => Some(parse_duration_str(s)?),
                None => None,
            };
            let warmup_override = match warmup {
                Some(ref s) => Some(parse_duration_str(s)?),
                None => None,
            };

            let cancel_flag = spawn_ctrl_c_watcher();
            let show_progress = !quiet && std::io::stderr().is_terminal();
            let (live_tx, mut live_rx) = mpsc::unbounded_channel::<TelemetryFrame>();

            let ticker_handle = if show_progress {
                let wk_title = prof.workload.kind.title().to_owned();
                Some(tokio::spawn(async move {
                    let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                    let mut idx = 0_usize;
                    while let Some(f) = live_rx.recv().await {
                        let spin = frames[idx % frames.len()];
                        idx += 1;
                        eprint!(
                            "\r\x1b[2K{spin} [{:.1}s] {wk_title} · {} ops · {:.1} MiB/s · Inflight {} · 429/503: {}",
                            f.transport.elapsed_secs,
                            f.transport.logical_ops,
                            f.transport.aggregate_mib_s,
                            f.transport.active_inflight,
                            f.transport.http_429_count + f.transport.http_503_slowdown_count
                        );
                        let _ = std::io::stderr().flush();
                    }
                    eprint!("\r\x1b[2K");
                    let _ = std::io::stderr().flush();
                }))
            } else {
                drop(live_rx);
                None
            };

            let frame = execute_workload_with_options(
                &prof,
                &knob,
                WorkloadRunOptions {
                    rounds_override: rounds,
                    duration_override,
                    warmup_override,
                    cancel_flag: Some(cancel_flag),
                    live_frame_tx: if show_progress { Some(live_tx) } else { None },
                },
            )
            .await?;

            if let Some(h) = ticker_handle {
                let _ = h.await;
            }

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
            emit_report_stdout(&report, format)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Tune {
            target,
            trial_rounds,
            max_p99_ms,
            max_read_amp,
            emit_profile,
            emit_rust,
            emit_python,
            json,
            markdown,
            csv,
            format,
            quiet,
        }) => {
            let mut prof = resolve_profile(&target).await?;
            if let Some(p99) = max_p99_ms {
                prof.sla.max_p99_ms = p99;
            }
            if let Some(ra) = max_read_amp {
                prof.sla.max_read_amplification = ra;
            }

            let mut space = TuneSearchSpace::default();
            if let Some(tr) = trial_rounds {
                space.trial_rounds = tr.clamp(4, 200);
            }

            let cancel_flag = spawn_ctrl_c_watcher();
            let show_progress = !quiet && std::io::stderr().is_terminal();
            let (prog_tx, mut prog_rx) = mpsc::unbounded_channel::<(CandidateTrial, TelemetryFrame)>();

            let prog_handle = if show_progress {
                Some(tokio::spawn(async move {
                    while let Some((t, _)) = prog_rx.recv().await {
                        let status = if t.throttled_503 {
                            "THROTTLED (429/503 -> circuit breaker)"
                        } else if t.sla_passed {
                            "SLA PASS"
                        } else {
                            "SLA MISS"
                        };
                        eprintln!(
                            "  ↳ Trial #{:<2} [{:<18}] conc={:<3} chunk={:>5}KiB gap={:>4}KiB -> {:>7.1} MiB/s | P99 {:>7.2}ms | {status}",
                            t.trial_id,
                            t.phase,
                            t.knob.concurrent,
                            t.knob.chunk_bytes / 1024,
                            t.knob.range_gap_bytes / 1024,
                            t.aggregate_mib_s,
                            t.worst_p99_ms,
                        );
                    }
                }))
            } else {
                drop(prog_rx);
                None
            };

            let engine = AutoTuneEngine::new(prof.sla.clone(), space);
            let outcome = engine
                .run_adaptive_sweep_with_progress(
                    &prof,
                    if show_progress { Some(prog_tx) } else { None },
                    Some(cancel_flag),
                )
                .await?;

            if let Some(h) = prog_handle {
                let _ = h.await;
            }

            if let Some(p) = emit_profile {
                crate::report::ensure_parent_dir(&p)?;
                std::fs::write(&p, &outcome.generated_toml_profile)?;
            }
            if let Some(p) = emit_rust {
                crate::report::ensure_parent_dir(&p)?;
                std::fs::write(&p, &outcome.generated_rust_code)?;
            }
            if let Some(p) = emit_python {
                crate::report::ensure_parent_dir(&p)?;
                std::fs::write(&p, &outcome.generated_python_code)?;
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
            emit_report_stdout(&report, format)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Diff {
            baseline,
            candidate,
            max_p99_regression_pct,
            min_latency_delta_ms,
            min_throughput_ratio,
            max_read_amp_delta,
            max_rpc_amp_delta,
            max_cost_increase_pct,
            markdown,
            json,
            format,
        }) => {
            let base_rep = BenchmarkReport::load_json(&baseline)?;
            let cand_rep = BenchmarkReport::load_json(&candidate)?;
            let thresholds = RegressionThresholds {
                max_p99_regression_pct,
                min_latency_delta_ms,
                min_throughput_ratio,
                max_read_amp_delta,
                max_rpc_amp_delta,
                max_cost_increase_pct,
            };
            let diff = diff_reports(&base_rep, &cand_rep, &thresholds);
            if let Some(p) = markdown {
                diff.save_markdown(&p)?;
            }
            if let Some(p) = json {
                diff.save_json(&p)?;
            }
            match format {
                OutputFormat::Terminal => print!("{}", render_diff_terminal(&diff)),
                OutputFormat::Markdown => println!("{}", diff.to_markdown()),
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&diff)?),
            }
            if diff.passed_gate {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(2))
            }
        }
        Some(Commands::NetProbe {
            endpoint,
            samples,
            timeout_ms,
            http1_only,
            ca_cert,
            insecure,
            json,
            format,
        }) => {
            let extra_ca_pem = match ca_cert {
                Some(ref p) => Some(
                    std::fs::read_to_string(p)
                        .with_context(|| format!("failed to read `--ca-cert {}`", p.display()))?,
                ),
                None => None,
            };
            let tls_opts = TlsClientOptions {
                alpn_protocols: if http1_only {
                    vec![b"http/1.1".to_vec()]
                } else {
                    vec![b"h2".to_vec(), b"http/1.1".to_vec()]
                },
                extra_ca_pem,
                insecure_skip_verify: insecure,
            };
            let summary = probe_endpoint_handshakes_with_tls(
                &endpoint,
                samples,
                Duration::from_millis(timeout_ms.max(200)),
                &tls_opts,
            )
            .await?;

            if let Some(p) = json {
                crate::report::ensure_parent_dir(&p)?;
                std::fs::write(&p, serde_json::to_string_pretty(&summary)?)?;
            }
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&summary)?),
                _ => print!("{}", render_handshake_terminal(&summary)),
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Cleanup { target }) => {
            let prof = resolve_profile(&target).await?;
            let op = build_admin_operator(&prof)?;
            let summary = sweep_all_oprobe_prefixes(&op).await?;
            println!(
                "● oprobe cleanup ({}) — found {} objects, deleted {} ({} bytes) in {:.1} ms [clean={}]",
                prof.target.name,
                summary.objects_created,
                summary.objects_deleted,
                summary.bytes_cleaned,
                summary.cleanup_ms,
                summary.verified_clean
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::ServeS3 {
            bind,
            tls,
            http2,
            ttfb_ms,
            merge_ms,
            straggler_every,
            straggler_ms,
            max_inflight_503,
            bandwidth_kib_s,
            write_ca_cert,
        }) => {
            if let Some(ca_out) = write_ca_cert {
                crate::report::ensure_parent_dir(&ca_out)?;
                std::fs::write(&ca_out, TESTBED_TLS_CERT_PEM)?;
            }
            let handle = S3ChaosServer::spawn(
                &bind,
                ChaosFaultConfig {
                    enable_tls: tls,
                    enable_http2: http2,
                    base_ttfb_delay: Duration::from_millis(ttfb_ms),
                    multipart_merge_delay: Duration::from_millis(merge_ms),
                    straggler_every_n: straggler_every,
                    straggler_delay: Duration::from_millis(straggler_ms),
                    max_inflight_before_503: max_inflight_503,
                    throttle_every_n_requests: None,
                    throttle_status_code: 503,
                    stream_bandwidth_kib_s: bandwidth_kib_s,
                },
            )
            .await?;
            println!(
                "● oprobe S3 Chaos Testbed listening at {} (Ctrl-C to stop)",
                handle.endpoint_url
            );
            println!(
                "  Try: AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test oprobe run --url s3://bench/lab --endpoint {} {}",
                handle.endpoint_url,
                if tls { "--insecure" } else { "" }
            );
            tokio::signal::ctrl_c().await?;
            drop(handle);
            Ok(ExitCode::SUCCESS)
        }
    }
}
