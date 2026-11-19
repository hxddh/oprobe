//! Real-time streaming interactive terminal workbench (`oprobe tui`, powered by `ratatui` + `crossterm`):
//! Streams live 100ms white-box telemetry frames during benchmark windows and live trial results
//! during closed-loop auto-tuning, with cooperative cancellation that preserves remote auto-cleanup.

pub mod command;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::{
    Alignment, Color, Constraint, Direction, Layout, Line, Modifier, Rect, Span, Style,
};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, Tabs, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::autotune::{
    generate_python_snippet, generate_rust_snippet, generate_toml_snippet, AutoTuneEngine,
    AutoTuneOutcome, CandidateTrial, TuneKnob, TuneSearchSpace,
};
use crate::cost::compare_multi_cloud_finops_scaled;
use crate::net_probe::{probe_endpoint_handshakes, EndpointHandshakeSummary};
use crate::report::BenchmarkReport;
use crate::telemetry::{Severity, TelemetryFrame};
use crate::workload::{
    execute_workload_with_options, ProfileConfig, WorkloadKind, WorkloadRunOptions,
};
use command::{apply_state_mutation, parse_minibuffer_command, MinibufferCommand};

enum WorkerEvent {
    LiveFrame(TelemetryFrame),
    LiveTrial(CandidateTrial, TelemetryFrame),
    ProbeDone(Result<TelemetryFrame, String>),
    TuneDone(Result<AutoTuneOutcome, String>),
    NetProbeDone(Result<EndpointHandshakeSummary, String>),
}

struct WorkbenchState {
    profile: ProfileConfig,
    knob: TuneKnob,
    frame: TelemetryFrame,
    trials: Vec<CandidateTrial>,
    rust_code: String,
    python_code: String,
    toml_profile: String,
    inspector_tab: usize,
    status_line: String,
    running: bool,
    show_help_modal: bool,
    minibuffer_active: bool,
    minibuffer_input: String,
    cmd_history: Vec<String>,
    history_cursor: Option<usize>,
    cancel_flag: Option<Arc<AtomicBool>>,
    active_job: Option<JoinHandle<()>>,
}

impl WorkbenchState {
    fn new(profile: ProfileConfig) -> Self {
        let knob = profile.effective_knob();
        let rust_code = generate_rust_snippet(&profile.target.scheme, &knob);
        let python_code = generate_python_snippet(&profile.target.scheme, &knob);
        let toml_profile = generate_toml_snippet(&profile, &knob);
        Self {
            profile,
            knob,
            frame: TelemetryFrame::default(),
            trials: Vec::new(),
            rust_code,
            python_code,
            toml_profile,
            inspector_tab: 0,
            status_line:
                "READY · Space Run/Stop | S 10s Stream | O Auto-Tune | 1-4 Workload | Tab Views | E Export | / Cmd | ? Help | Q Quit"
                    .to_owned(),
            running: false,
            show_help_modal: false,
            minibuffer_active: false,
            minibuffer_input: String::new(),
            cmd_history: Vec::new(),
            history_cursor: None,
            cancel_flag: None,
            active_job: None,
        }
    }

    async fn cancel_job_gracefully(&mut self) {
        if let Some(flag) = self.cancel_flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(mut handle) = self.active_job.take() {
            // Give cooperative cancellation up to 1.5s to complete remote auto-cleanup cleanly
            if tokio::time::timeout(Duration::from_millis(1500), &mut handle)
                .await
                .is_err()
            {
                handle.abort();
            }
            self.running = false;
            self.status_line =
                "STOPPED · Active run cancelled cleanly (remote test objects cleaned up)".to_owned();
        }
    }

    fn refresh_codegen(&mut self) {
        self.rust_code = generate_rust_snippet(&self.profile.target.scheme, &self.knob);
        self.python_code = generate_python_snippet(&self.profile.target.scheme, &self.knob);
        self.toml_profile = generate_toml_snippet(&self.profile, &self.knob);
    }

    async fn spawn_probe(
        &mut self,
        duration_override: Option<Duration>,
        tx: mpsc::UnboundedSender<WorkerEvent>,
    ) {
        self.cancel_job_gracefully().await;
        self.running = true;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());

        let mode_desc = match duration_override {
            Some(d) => format!("stream={:.1}s", d.as_secs_f64()),
            None => match self.profile.workload.duration_secs {
                Some(s) if s > 0.0 => format!("stream={s:.1}s"),
                _ => format!("rounds={}", self.profile.workload.rounds),
            },
        };
        self.status_line = format!(
            "PROFILING · [{}] ({}, scheme={}, conc={}, chunk={}KiB)...",
            self.profile.workload.kind.title(),
            mode_desc,
            self.profile.target.scheme,
            self.knob.concurrent,
            self.knob.chunk_bytes / 1024
        );

        let prof = self.profile.clone();
        let knob = self.knob;
        let (live_tx, mut live_rx) = mpsc::unbounded_channel::<TelemetryFrame>();
        let tx_live = tx.clone();
        tokio::spawn(async move {
            while let Some(frame) = live_rx.recv().await {
                let _ = tx_live.send(WorkerEvent::LiveFrame(frame));
            }
        });

        self.active_job = Some(tokio::spawn(async move {
            let res = execute_workload_with_options(
                &prof,
                &knob,
                WorkloadRunOptions {
                    rounds_override: None,
                    duration_override,
                    warmup_override: None,
                    cancel_flag: Some(cancel),
                    live_frame_tx: Some(live_tx),
                },
            )
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(WorkerEvent::ProbeDone(res));
        }));
    }

    async fn spawn_autotune(&mut self, tx: mpsc::UnboundedSender<WorkerEvent>) {
        self.cancel_job_gracefully().await;
        self.running = true;
        self.trials.clear();
        self.inspector_tab = 1;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());
        self.status_line = format!(
            "AUTO-TUNING · Running 429/503-resilient sweep on [{}]...",
            self.profile.workload.kind.title()
        );

        let prof = self.profile.clone();
        let (prog_tx, mut prog_rx) = mpsc::unbounded_channel::<(CandidateTrial, TelemetryFrame)>();
        let tx_prog = tx.clone();
        tokio::spawn(async move {
            while let Some((trial, frame)) = prog_rx.recv().await {
                let _ = tx_prog.send(WorkerEvent::LiveTrial(trial, frame));
            }
        });

        self.active_job = Some(tokio::spawn(async move {
            let engine = AutoTuneEngine::new(prof.sla.clone(), TuneSearchSpace::default());
            let res = engine
                .run_adaptive_sweep_with_progress(&prof, Some(prog_tx), Some(cancel))
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(WorkerEvent::TuneDone(res));
        }));
    }

    async fn spawn_net_probe(&mut self, url: String, tx: mpsc::UnboundedSender<WorkerEvent>) {
        self.cancel_job_gracefully().await;
        self.running = true;
        self.status_line = format!(
            "NET-PROBE · Sampling cold & warm DNS/TCP/TLS/ALPN handshakes on {url}..."
        );
        self.active_job = Some(tokio::spawn(async move {
            let res = probe_endpoint_handshakes(&url, 4, Duration::from_secs(5))
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(WorkerEvent::NetProbeDone(res));
        }));
    }

    fn export_all(&mut self, prefix: &str) {
        let mut rep = BenchmarkReport::from_single_run(
            self.profile.clone(),
            self.knob,
            self.frame.clone(),
        );
        rep.trials = self.trials.clone();
        rep.generated_rust_code = self.rust_code.clone();
        rep.generated_python_code = self.python_code.clone();
        rep.generated_toml_profile = self.toml_profile.clone();

        let json_p = format!("{prefix}.json");
        let md_p = format!("{prefix}.md");
        let csv_p = format!("{prefix}.csv");
        let toml_p = format!("{prefix}.profile.toml");
        match (
            rep.save_json(&json_p),
            rep.save_markdown(&md_p),
            rep.save_csv(&csv_p),
            std::fs::write(&toml_p, &self.toml_profile).map_err(anyhow::Error::from),
        ) {
            (Ok(()), Ok(()), Ok(()), Ok(())) => {
                self.status_line =
                    format!("EXPORTED · Saved {json_p}, {md_p}, {csv_p}, {toml_p}");
            }
            (Err(e), _, _, _) | (_, Err(e), _, _) | (_, _, Err(e), _) | (_, _, _, Err(e)) => {
                self.status_line = format!("EXPORT ERROR · {e}");
            }
        }
    }
}

/// Launch the interactive Ratatui terminal workbench and trigger an initial baseline run.
pub async fn run_tui_workbench(initial_profile: ProfileConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let loop_res = event_loop(&mut terminal, initial_profile).await;

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    loop_res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    initial_profile: ProfileConfig,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorkerEvent>();
    let mut state = WorkbenchState::new(initial_profile);
    state.spawn_probe(None, tx.clone()).await;

    loop {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                WorkerEvent::LiveFrame(frame) => {
                    state.status_line = format!(
                        "STREAMING · {:.1}s | Ops={} | {:.1} MiB/s | Inflight={}",
                        frame.transport.elapsed_secs,
                        frame.transport.logical_ops,
                        frame.transport.aggregate_mib_s,
                        frame.transport.active_inflight
                    );
                    state.frame = frame;
                }
                WorkerEvent::LiveTrial(trial, frame) => {
                    state.status_line = format!(
                        "AUTO-TUNING · Trial #{} [{}] conc={} chunk={}KiB -> {:.1} MiB/s | P99 {:.2}ms | 429/503={}",
                        trial.trial_id,
                        trial.phase,
                        trial.knob.concurrent,
                        trial.knob.chunk_bytes / 1024,
                        trial.aggregate_mib_s,
                        trial.worst_p99_ms,
                        trial.throttle_events
                    );
                    if !trial.throttled_503 && !frame.verbs.is_empty() {
                        state.frame = frame;
                    }
                    state.trials.push(trial);
                }
                WorkerEvent::ProbeDone(Ok(frame)) => {
                    state.running = false;
                    state.active_job = None;
                    state.cancel_flag = None;
                    state.status_line = format!(
                        "[{}] {} · {:.1} MiB/s | Cleanup: {}",
                        frame.diagnosis.severity.badge(),
                        frame.diagnosis.headline,
                        frame.transport.aggregate_mib_s,
                        if frame.cleanup.verified_clean {
                            "CLEAN"
                        } else {
                            "KEPT"
                        }
                    );
                    state.frame = frame;
                }
                WorkerEvent::ProbeDone(Err(e)) => {
                    state.running = false;
                    state.active_job = None;
                    state.cancel_flag = None;
                    state.status_line = format!("PROBE FAILED · {e}");
                }
                WorkerEvent::TuneDone(Ok(outcome)) => {
                    state.running = false;
                    state.active_job = None;
                    state.cancel_flag = None;
                    state.knob = outcome.winner.knob;
                    state.frame = outcome.best_frame;
                    state.trials = outcome.trials;
                    state.rust_code = outcome.generated_rust_code;
                    state.python_code = outcome.generated_python_code;
                    state.toml_profile = outcome.generated_toml_profile;
                    state.inspector_tab = 1;
                    state.status_line = format!(
                        "TUNE COMPLETE · Winner conc={} chunk={}KiB gap={}KiB | P99 {:.2}ms | {:.1} MiB/s",
                        state.knob.concurrent,
                        state.knob.chunk_bytes / 1024,
                        state.knob.range_gap_bytes / 1024,
                        outcome.winner.worst_p99_ms,
                        outcome.winner.aggregate_mib_s
                    );
                }
                WorkerEvent::TuneDone(Err(e)) => {
                    state.running = false;
                    state.active_job = None;
                    state.cancel_flag = None;
                    state.status_line = format!("AUTO-TUNE FAILED · {e}");
                }
                WorkerEvent::NetProbeDone(Ok(hs)) => {
                    state.running = false;
                    state.active_job = None;
                    state.cancel_flag = None;
                    state.status_line = format!(
                        "NET-PROBE OK · {} ({}) DNS {:.2}ms | TCP {:.2}ms | TLS {:.2}ms | Cold/Warm {:.1}x ({})",
                        hs.host,
                        hs.tls_version,
                        hs.dns_p50_ms,
                        hs.tcp_rtt_p50_ms,
                        hs.tls_p50_ms,
                        hs.cold_vs_warm_ratio,
                        hs.negotiated_alpn
                    );
                    state.frame.handshake = hs;
                    state.inspector_tab = 3;
                }
                WorkerEvent::NetProbeDone(Err(e)) => {
                    state.running = false;
                    state.active_job = None;
                    state.cancel_flag = None;
                    state.status_line = format!("NET-PROBE FAILED · {e}");
                }
            }
        }

        terminal.draw(|f| draw_workbench(f, &state))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    state.cancel_job_gracefully().await;
                    break;
                }

                if state.show_help_modal {
                    if matches!(
                        key.code,
                        KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter | KeyCode::Char('q')
                    ) {
                        state.show_help_modal = false;
                    }
                    continue;
                }

                if state.minibuffer_active {
                    match key.code {
                        KeyCode::Esc => {
                            state.minibuffer_active = false;
                            state.minibuffer_input.clear();
                            state.history_cursor = None;
                        }
                        KeyCode::Backspace => {
                            state.minibuffer_input.pop();
                        }
                        KeyCode::Up => {
                            if !state.cmd_history.is_empty() {
                                let next_idx = match state.history_cursor {
                                    Some(idx) => idx.saturating_sub(1),
                                    None => state.cmd_history.len() - 1,
                                };
                                state.history_cursor = Some(next_idx);
                                state.minibuffer_input = state.cmd_history[next_idx].clone();
                            }
                        }
                        KeyCode::Down => {
                            if let Some(idx) = state.history_cursor {
                                if idx + 1 < state.cmd_history.len() {
                                    state.history_cursor = Some(idx + 1);
                                    state.minibuffer_input = state.cmd_history[idx + 1].clone();
                                } else {
                                    state.history_cursor = None;
                                    state.minibuffer_input.clear();
                                }
                            }
                        }
                        KeyCode::Enter => {
                            let cmd_raw = state.minibuffer_input.clone();
                            state.minibuffer_active = false;
                            state.minibuffer_input.clear();
                            state.history_cursor = None;
                            if !cmd_raw.trim().is_empty() {
                                state.cmd_history.push(cmd_raw.clone());
                            }
                            match parse_minibuffer_command(&cmd_raw) {
                                Ok(MinibufferCommand::Quit) => {
                                    state.cancel_job_gracefully().await;
                                    break;
                                }
                                Ok(MinibufferCommand::ProbeOnce) => {
                                    state.spawn_probe(None, tx.clone()).await;
                                }
                                Ok(MinibufferCommand::StreamRun(dur)) => {
                                    state.spawn_probe(Some(dur), tx.clone()).await;
                                }
                                Ok(MinibufferCommand::AutoTune) => {
                                    state.spawn_autotune(tx.clone()).await;
                                }
                                Ok(MinibufferCommand::NetProbe(url)) => {
                                    state.spawn_net_probe(url, tx.clone()).await;
                                }
                                Ok(MinibufferCommand::ExportReport(prefix)) => {
                                    state.export_all(&prefix);
                                }
                                Ok(other) => {
                                    if let Some(msg) = apply_state_mutation(
                                        &other,
                                        &mut state.profile,
                                        &mut state.knob,
                                    ) {
                                        state.refresh_codegen();
                                        state.status_line = msg;
                                        if !matches!(other, MinibufferCommand::Help) {
                                            state.spawn_probe(None, tx.clone()).await;
                                        }
                                    }
                                }
                                Err(err_msg) => {
                                    state.status_line = format!("COMMAND ERROR · {err_msg}");
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            state.minibuffer_input.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => {
                        state.cancel_job_gracefully().await;
                        break;
                    }
                    KeyCode::Char('?') => {
                        state.show_help_modal = true;
                    }
                    KeyCode::Esc => {
                        state.cancel_job_gracefully().await;
                    }
                    KeyCode::Char('/') | KeyCode::Char(':') => {
                        state.minibuffer_active = true;
                        state.minibuffer_input.clear();
                    }
                    KeyCode::Char(' ') => {
                        if state.running {
                            state.cancel_job_gracefully().await;
                        } else {
                            state.spawn_probe(None, tx.clone()).await;
                        }
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        state
                            .spawn_probe(Some(Duration::from_secs(10)), tx.clone())
                            .await;
                    }
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        state.spawn_autotune(tx.clone()).await;
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        state.export_all("oprobe_report");
                    }
                    KeyCode::Tab => {
                        state.inspector_tab = (state.inspector_tab + 1) % 5;
                    }
                    KeyCode::BackTab => {
                        state.inspector_tab = (state.inspector_tab + 4) % 5;
                    }
                    KeyCode::Char('1') => {
                        state.profile.workload.kind = WorkloadKind::ParquetPointLookup;
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('2') => {
                        state.profile.workload.kind = WorkloadKind::CheckpointMultipart;
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('3') => {
                        state.profile.workload.kind = WorkloadKind::MetadataPrefixStorm;
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('4') => {
                        state.profile.workload.kind = WorkloadKind::MixedReadWrite;
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('f') | KeyCode::Char('F') => {
                        state.knob.enable_foyer = !state.knob.enable_foyer;
                        state.refresh_codegen();
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('t') | KeyCode::Char('T') => {
                        state.knob.enable_tail_cut = !state.knob.enable_tail_cut;
                        state.refresh_codegen();
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('k') | KeyCode::Char('K') => {
                        state.knob.keep_alive = !state.knob.keep_alive;
                        state.refresh_codegen();
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('[') => {
                        state.knob.concurrent = state.knob.concurrent.saturating_sub(4).max(2);
                        state.refresh_codegen();
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char(']') => {
                        state.knob.concurrent = (state.knob.concurrent + 8).min(128);
                        state.refresh_codegen();
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('-') => {
                        state.knob.chunk_bytes = (state.knob.chunk_bytes / 2).max(64 * 1024);
                        state.refresh_codegen();
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    KeyCode::Char('=') | KeyCode::Char('+') => {
                        state.knob.chunk_bytes = (state.knob.chunk_bytes * 2).min(32 * 1024 * 1024);
                        state.refresh_codegen();
                        state.spawn_probe(None, tx.clone()).await;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn severity_color(sev: Severity) -> Color {
    match sev {
        Severity::Healthy => Color::Green,
        Severity::Advisory => Color::Cyan,
        Severity::Warning => Color::Yellow,
        Severity::Critical => Color::Red,
    }
}

fn draw_workbench(f: &mut ratatui::Frame<'_>, state: &WorkbenchState) {
    let root_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Min(9),
            Constraint::Length(3),
        ])
        .split(f.area());

    // 1. Header Bar
    let wk = state.profile.workload.kind;
    let header_line = Line::from(vec![
        Span::styled(
            " oprobe v1.0.0 ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {} ({}) | ",
            state.profile.target.name, state.profile.target.scheme
        )),
        Span::styled(
            format!(
                "[1]Parquet{} ",
                if wk == WorkloadKind::ParquetPointLookup {
                    "*"
                } else {
                    ""
                }
            ),
            Style::default().fg(if wk == WorkloadKind::ParquetPointLookup {
                Color::Yellow
            } else {
                Color::Gray
            }),
        ),
        Span::styled(
            format!(
                "[2]Ckpt{} ",
                if wk == WorkloadKind::CheckpointMultipart {
                    "*"
                } else {
                    ""
                }
            ),
            Style::default().fg(if wk == WorkloadKind::CheckpointMultipart {
                Color::Yellow
            } else {
                Color::Gray
            }),
        ),
        Span::styled(
            format!(
                "[3]Meta{} ",
                if wk == WorkloadKind::MetadataPrefixStorm {
                    "*"
                } else {
                    ""
                }
            ),
            Style::default().fg(if wk == WorkloadKind::MetadataPrefixStorm {
                Color::Yellow
            } else {
                Color::Gray
            }),
        ),
        Span::styled(
            format!(
                "[4]Mixed{} ",
                if wk == WorkloadKind::MixedReadWrite {
                    "*"
                } else {
                    ""
                }
            ),
            Style::default().fg(if wk == WorkloadKind::MixedReadWrite {
                Color::Yellow
            } else {
                Color::Gray
            }),
        ),
        Span::raw(format!(
            "| [F]Cache:{} [T]TailCut:{} [K]Pool:{} [ [/] ]Conc:{} [-/+]Chunk:{}KiB Gap:{}KiB",
            if state.knob.enable_foyer { "ON" } else { "OFF" },
            if state.knob.enable_tail_cut { "ON" } else { "OFF" },
            if state.knob.keep_alive { "ON" } else { "OFF" },
            state.knob.concurrent,
            state.knob.chunk_bytes / 1024,
            state.knob.range_gap_bytes / 1024,
        )),
    ]);
    let header_block = Paragraph::new(header_line).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Apache OpenDAL White-Box Cloud Storage Stack Microscope & Auto-Tuner "),
    );
    f.render_widget(header_block, root_chunks[0]);

    // 2. Three KPI Cards
    let kpi_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(32),
            Constraint::Percentage(28),
        ])
        .split(root_chunks[1]);

    let diag = &state.frame.diagnosis;
    let diag_card = Paragraph::new(vec![
        Line::from(Span::styled(
            if diag.headline.is_empty() {
                "Sampling initial white-box telemetry..."
            } else {
                &diag.headline
            },
            Style::default()
                .fg(severity_color(diag.severity))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            &diag.recommendation,
            Style::default().fg(Color::Gray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL).title(format!(
        " Root-Cause Diagnosis [{}] ",
        diag.severity.badge()
    )));
    f.render_widget(diag_card, kpi_cols[0]);

    let tp = &state.frame.transport;
    let amp_card = Paragraph::new(vec![
        Line::from(format!(
            "Socket Reuse {:.1}% | ReadAmp {:.2}x | RpcAmp {:.2}x",
            tp.socket_reuse_pct, tp.read_amplification, tp.rpc_amplification
        )),
        Line::from(format!(
            "Cache Hit {:.1}% | Retries {} | 429/503 Throttles {}",
            tp.cache_hit_ratio_pct,
            tp.retry_attempts,
            tp.http_429_count + tp.http_503_slowdown_count
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Connection Pool, Cache & Amplification "),
    );
    f.render_widget(amp_card, kpi_cols[1]);

    let host_card = Paragraph::new(vec![
        Line::from(Span::styled(
            format!(
                "{:.1} MiB/s ({:.1}/core · {}c · Inflight {})",
                tp.aggregate_mib_s, tp.mib_per_cpu_core, tp.cpu_cores, tp.active_inflight
            ),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "RSS {} MiB | CRC32C {} ok / {} bad",
            tp.process_rss_mib, tp.crc32c_verified_ok, tp.crc32c_corrupted
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Throughput & Host Self-Check "),
    );
    f.render_widget(host_card, kpi_cols[2]);

    // 3. Seven-Stage Waterfall + Live Sparkline
    let mid_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(root_chunks[2]);

    draw_waterfall_panel(f, mid_cols[0], state);
    draw_sparkline_panel(f, mid_cols[1], state);

    // 4. Per-Verb Isolated HDR Table
    let header_cells = [
        "OPERATION VERB",
        "COUNT",
        "OPS/s",
        "P50(ms)",
        "P95(ms)",
        "P99(ms)",
        "P99.9(ms)",
        "MiB/s",
        "429/503",
    ]
    .into_iter()
    .map(|h| Cell::from(h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let rows = state.frame.verbs.iter().map(|v| {
        Row::new(vec![
            Cell::from(v.verb.clone()),
            Cell::from(format!("{}", v.count)),
            Cell::from(format!("{:.1}", v.ops_per_sec)),
            Cell::from(format!("{:.2}", v.p50_ms)),
            Cell::from(format!("{:.2}", v.p95_ms)),
            Cell::from(format!("{:.2}", v.p99_ms)),
            Cell::from(format!("{:.2}", v.p999_ms)),
            Cell::from(format!("{:.1}", v.throughput_mib_s)),
            Cell::from(format!("{:.1}%", v.throttle_429_503_pct)),
        ])
    });
    let verb_table = Table::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(header_cells))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Per-Verb Isolated HDR Latency Quantiles "),
    );
    f.render_widget(verb_table, root_chunks[3]);

    // 5. Five-Tab Inspector Panel
    draw_inspector_tabs(f, root_chunks[4], state);

    // 6. Status / `/` Command Bar
    if state.minibuffer_active {
        let cmd_bar = Paragraph::new(Line::from(vec![
            Span::styled(
                " COMMAND ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" :"),
            Span::styled(
                format!("{}_", state.minibuffer_input),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   (Enter Run | Up/Down History | Esc Cancel | Try `url s3://bucket/p`, `stream 10s`, `conc 32`, `export snap`)",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(cmd_bar, root_chunks[5]);
    } else {
        let status_bar = Paragraph::new(Line::from(vec![
            Span::styled(
                if state.running { " STREAMING " } else { " IDLE " },
                Style::default()
                    .fg(Color::Black)
                    .bg(if state.running {
                        Color::Yellow
                    } else {
                        Color::Green
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {}", state.status_line)),
        ]))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(status_bar, root_chunks[5]);
    }

    if state.show_help_modal {
        draw_help_modal(f);
    }
}

fn draw_waterfall_panel(f: &mut ratatui::Frame<'_>, area: Rect, state: &WorkbenchState) {
    let stages = state.frame.waterfall.breakdown_pct();
    let colors = [
        Color::Yellow,
        Color::Green,
        Color::Magenta,
        Color::Red,
        Color::Cyan,
        Color::LightBlue,
        Color::Blue,
    ];
    let bar_width = area.width.saturating_sub(4) as usize;
    let mut bar_spans = Vec::new();
    for (idx, (_, _, pct)) in stages.iter().enumerate() {
        let cells =
            (((pct / 100.0) * bar_width as f64).round() as usize).max(if *pct > 1.2 { 1 } else { 0 });
        if cells > 0 {
            bar_spans.push(Span::styled(
                "█".repeat(cells),
                Style::default().fg(colors[idx]),
            ));
        }
    }
    if bar_spans.is_empty() {
        bar_spans.push(Span::styled(
            "░".repeat(bar_width.min(56)),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let legend_1 = Line::from(vec![
        Span::styled("■1.Queue ", Style::default().fg(Color::Yellow)),
        Span::raw(format!("{:.2}ms({:.0}%) ", stages[0].1, stages[0].2)),
        Span::styled("■2.Cache ", Style::default().fg(Color::Green)),
        Span::raw(format!("{:.2}ms({:.0}%) ", stages[1].1, stages[1].2)),
        Span::styled("■3.DNS/TCP/TLS ", Style::default().fg(Color::Magenta)),
        Span::raw(format!("{:.2}ms({:.0}%) ", stages[2].1, stages[2].2)),
        Span::styled("■4.TTFB ", Style::default().fg(Color::Red)),
        Span::raw(format!("{:.2}ms({:.0}%)", stages[3].1, stages[3].2)),
    ]);
    let legend_2 = Line::from(vec![
        Span::styled("■5.Stream ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{:.2}ms({:.0}%)  ", stages[4].1, stages[4].2)),
        Span::styled("■6.MPU-Merge ", Style::default().fg(Color::LightBlue)),
        Span::raw(format!("{:.2}ms({:.0}%)  ", stages[5].1, stages[5].2)),
        Span::styled("■7.CRC32C ", Style::default().fg(Color::Blue)),
        Span::raw(format!("{:.2}ms({:.0}%)", stages[6].1, stages[6].2)),
    ]);

    let p = Paragraph::new(vec![Line::from(bar_spans), legend_1, legend_2]).block(
        Block::default().borders(Borders::ALL).title(format!(
            " Seven-Stage White-Box Latency Waterfall (Mean {:.2} ms) ",
            state.frame.waterfall.total_ms()
        )),
    );
    f.render_widget(p, area);
}

fn draw_sparkline_panel(f: &mut ratatui::Frame<'_>, area: Rect, state: &WorkbenchState) {
    let vals: Vec<u64> = state
        .frame
        .timeseries
        .iter()
        .map(|pt| (pt.throughput_mib_s * 10.0).round().max(0.0) as u64)
        .collect();
    let spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Live Throughput ({:.1} MiB/s) ",
            state.frame.transport.aggregate_mib_s
        )))
        .data(&vals)
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(spark, area);
}

fn draw_inspector_tabs(f: &mut ratatui::Frame<'_>, area: Rect, state: &WorkbenchState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(5)])
        .split(area);

    let tab_titles = [
        " [1] AwaitTree Slow Stacks ",
        " [2] Auto-Tune & Rust Code ",
        " [3] 5-Cloud FinOps & Cache ",
        " [4] Socket/TLS & Req-IDs ",
        " [5] Cleanup & TOML Config ",
    ];
    let tabs = Tabs::new(tab_titles.into_iter().map(Line::from).collect::<Vec<_>>())
        .select(state.inspector_tab)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[0]);

    let content_text = match state.inspector_tab {
        0 => state.frame.await_tree_dump.clone(),
        1 => {
            let mut buf = String::new();
            if !state.trials.is_empty() {
                buf.push_str("Auto-Tune Candidate Matrix (Top 6 by Score):\n");
                let mut sorted = state.trials.clone();
                sorted.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for t in sorted.iter().take(6) {
                    buf.push_str(&format!(
                        "  #{} [{}] conc={} chunk={}KiB gap={}KiB cache={} -> {:.1} MiB/s | P99 {:.2}ms | ReadAmp {:.2}x | 429/503={} | Score {:.1}\n",
                        t.trial_id,
                        t.phase,
                        t.knob.concurrent,
                        t.knob.chunk_bytes / 1024,
                        t.knob.range_gap_bytes / 1024,
                        if t.knob.enable_foyer { "on" } else { "off" },
                        t.aggregate_mib_s,
                        t.worst_p99_ms,
                        t.read_amplification,
                        t.throttle_events,
                        t.score
                    ));
                }
                buf.push('\n');
            }
            buf.push_str(&state.rust_code);
            buf
        }
        2 => {
            let finops = compare_multi_cloud_finops_scaled(
                &state.frame.transport,
                state.profile.workload.kind.write_ratio(),
                state.profile.workload.internet_egress_ratio,
                state.profile.pricing.baseline_stored_tb,
                state.profile.pricing.monthly_logical_access_tb,
            );
            let mut lines = vec![format!(
                "{:<22} {:>12} {:>12} {:>12} {:>14} {:>14} {:>14}",
                "CLOUD PROVIDER",
                "STORAGE/mo",
                "API(A+B)/mo",
                "EGRESS/mo",
                "TOTAL USD/mo",
                "AMP WASTE/mo",
                "CACHE SAVED"
            )];
            for c in finops {
                lines.push(format!(
                    "{:<22} {:>11.2}$ {:>11.2}$ {:>11.2}$ {:>13.2}$ {:>13.2}$ {:>13.2}$",
                    c.provider_name,
                    c.monthly_storage_usd,
                    c.monthly_api_requests_usd,
                    c.monthly_egress_usd,
                    c.total_monthly_usd,
                    c.waste_from_amplification_usd,
                    c.saved_by_cache_usd
                ));
            }
            lines.push(String::new());
            lines.push(format!(
                "Scale: {:.0} TB stored + {:.0} TB/mo logical access (egress ratio {:.0}%). Use `:pricing s3|r2|gcs|oss|b2` to change baseline.",
                state.profile.pricing.baseline_stored_tb,
                state.profile.pricing.monthly_logical_access_tb,
                state.profile.workload.internet_egress_ratio * 100.0
            ));
            lines.join("\n")
        }
        3 => {
            let hs = &state.frame.handshake;
            let mut s = format!(
                "Endpoint: {} (Host: {}:{}, TLS: {}, ALPN: {}, Cipher: {})\n\
                 DNS P50/P99: {:.2}/{:.2} ms | TCP RTT P50/P99: {:.2}/{:.2} ms | TLS P50/P99: {:.2}/{:.2} ms\n\
                 Cold HEAD Total: {:.2} ms vs Warm Reused HEAD TTFB: {:.2} ms ({:.1}x Keep-Alive speedup)\n\
                 Resolved IPs: {:?}   (Run `:net https://s3.amazonaws.com` to probe any live endpoint)\n\n\
                 Recent Storage Gateway Request-IDs:\n",
                hs.endpoint,
                hs.host,
                hs.port,
                hs.tls_version,
                hs.negotiated_alpn,
                hs.cipher_suite,
                hs.dns_p50_ms,
                hs.dns_p99_ms,
                hs.tcp_rtt_p50_ms,
                hs.tcp_rtt_p99_ms,
                hs.tls_p50_ms,
                hs.tls_p99_ms,
                hs.cold_total_ms,
                hs.warm_ttfb_p50_ms,
                hs.cold_vs_warm_ratio,
                hs.resolved_ips,
            );
            if state.frame.transport.recent_request_ids.is_empty() {
                s.push_str("  (In-process storage backend or no remote gateway request-id headers yet)");
            } else {
                for req_id in &state.frame.transport.recent_request_ids {
                    s.push_str(&format!("  - {req_id}\n"));
                }
            }
            s
        }
        _ => {
            let cl = &state.frame.cleanup;
            format!(
                "Remote Test Data Auto-Cleanup Status:\n  \
                 Enabled: {} | Prefix: `{}` | Created: {} | Deleted: {} | Remaining: {} | Clean: {} ({:.1} ms)\n\n\
                 Reusable Round-Trip TOML Profile:\n{}",
                cl.auto_cleanup_enabled,
                cl.run_prefix,
                cl.objects_created,
                cl.objects_deleted,
                cl.remaining_objects,
                cl.verified_clean,
                cl.cleanup_ms,
                state.toml_profile
            )
        }
    };

    let body = Paragraph::new(content_text)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(body, chunks[1]);
}

fn draw_help_modal(f: &mut ratatui::Frame<'_>) {
    let area = f.area();
    let popup_w = area.width.min(86);
    let popup_h = area.height.min(20);
    let popup_rect = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup_rect);
    let help_text = "\
Keyboard Shortcuts:
  Space        Run single benchmark burst / Gracefully stop active run
  S            Run 10-second continuous streaming window
  O            Run closed-loop 429/503-resilient SLA Auto-Tuner
  1 / 2 / 3 / 4 Switch workload (Parquet / Checkpoint / Metadata / Mixed)
  [ / ]        Decrease / Increase concurrency limit
  - / +        Halve / Double I/O chunk size
  F / T / K    Toggle Local Cache / TailCutLayer / HTTP Keep-Alive
  Tab / S-Tab  Cycle through 5 white-box inspector tabs
  E            Export .json, .md, .csv, and .profile.toml snapshots
  / or :       Open command bar (`url s3://...`, `set k=v`, `net <url>`)
  Q / Ctrl-C   Cleanly stop active run, clean remote data, and quit

Press Esc or `?` to close.";

    let modal = Paragraph::new(help_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" oprobe v1.0.0 Interactive Workbench Help "),
    );
    f.render_widget(modal, popup_rect);
}
