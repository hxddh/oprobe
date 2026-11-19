//! 内置零外部依赖终端工作台（基于 `ratatui` + `crossterm`）：
//! 在 SSH 服务器或本地终端执行 `oprobe` / `oprobe tui` 即可直接进入全功能交互界面，
//! 支持实时六段式耗时瀑布、Sparkline 吞吐趋势、分动词 HDR 分位谱、多云 FinOps 对比及 `/` Minibuffer 命令栏。

pub mod command;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::{Alignment, Color, Constraint, Direction, Layout, Line, Modifier, Rect, Span, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, Tabs, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::autotune::{
    generate_python_snippet, generate_rust_snippet, AutoTuneEngine, AutoTuneOutcome,
    CandidateTrial, TuneKnob, TuneSearchSpace,
};
use crate::cost::compare_multi_cloud_finops;
use crate::net_probe::{probe_endpoint_handshakes, EndpointHandshakeSummary};
use crate::report::BenchmarkReport;
use crate::telemetry::{Severity, TelemetryFrame};
use crate::workload::{execute_workload_round, ProfileConfig, WorkloadKind};
use command::{apply_state_mutation, parse_minibuffer_command, MinibufferCommand};

enum WorkerEvent {
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
    inspector_tab: usize,
    status_line: String,
    running: bool,
    minibuffer_active: bool,
    minibuffer_input: String,
    active_job: Option<JoinHandle<()>>,
}

impl WorkbenchState {
    fn new(profile: ProfileConfig) -> Self {
        let knob = TuneKnob::default();
        let rust_code = generate_rust_snippet(&profile.target.scheme, &knob);
        let python_code = generate_python_snippet(&profile.target.scheme, &knob);
        Self {
            profile,
            knob,
            frame: TelemetryFrame::default(),
            trials: Vec::new(),
            rust_code,
            python_code,
            inspector_tab: 0,
            status_line: "READY · Space 单点剖析 | O 自动寻优 | 1-4 切负载 | Tab 切视图 | E 导出报告 | / 命令栏 | Q 退出".to_owned(),
            running: false,
            minibuffer_active: false,
            minibuffer_input: String::new(),
            active_job: None,
        }
    }

    fn cancel_job(&mut self) {
        if let Some(handle) = self.active_job.take() {
            handle.abort();
            self.running = false;
            self.status_line = "CANCELLED · 已中止后台在途剖析流并释放句柄".to_owned();
        }
    }

    fn refresh_codegen(&mut self) {
        self.rust_code = generate_rust_snippet(&self.profile.target.scheme, &self.knob);
        self.python_code = generate_python_snippet(&self.profile.target.scheme, &self.knob);
    }

    fn spawn_probe(&mut self, tx: mpsc::UnboundedSender<WorkerEvent>) {
        self.cancel_job();
        self.running = true;
        self.status_line = format!(
            "PROFILING · 正在执行 [{}] (scheme={}, conc={}, chunk={}KiB)...",
            self.profile.workload.kind.title(),
            self.profile.target.scheme,
            self.knob.concurrent,
            self.knob.chunk_bytes / 1024
        );
        let prof = self.profile.clone();
        let knob = self.knob;
        self.active_job = Some(tokio::spawn(async move {
            let res = execute_workload_round(&prof, &knob, None)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(WorkerEvent::ProbeDone(res));
        }));
    }

    fn spawn_autotune(&mut self, tx: mpsc::UnboundedSender<WorkerEvent>) {
        self.cancel_job();
        self.running = true;
        self.status_line = format!(
            "AUTO-TUNING · 正在执行两阶段防 503 坐标寻优 [{}]...",
            self.profile.workload.kind.title()
        );
        let prof = self.profile.clone();
        self.active_job = Some(tokio::spawn(async move {
            let engine = AutoTuneEngine::new(prof.sla.clone(), TuneSearchSpace::default());
            let res = engine
                .run_adaptive_sweep(&prof)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(WorkerEvent::TuneDone(res));
        }));
    }

    fn spawn_net_probe(&mut self, url: String, tx: mpsc::UnboundedSender<WorkerEvent>) {
        self.cancel_job();
        self.running = true;
        self.status_line = format!("NET-PROBE · 正在对 {url} 执行微秒级 DNS/TCP/TLS/ALPN 握手采样...");
        self.active_job = Some(tokio::spawn(async move {
            let res = probe_endpoint_handshakes(&url, 4, Duration::from_secs(4))
                .await
                .map_err(|e| e.to_string());
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

        let json_p = format!("{prefix}.json");
        let md_p = format!("{prefix}.md");
        let csv_p = format!("{prefix}.csv");
        match (
            rep.save_json(&json_p),
            rep.save_markdown(&md_p),
            rep.save_csv(&csv_p),
        ) {
            (Ok(()), Ok(()), Ok(())) => {
                self.status_line = format!("EXPORTED · 已落盘 {json_p}, {md_p}, {csv_p}");
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                self.status_line = format!("EXPORT ERROR · {e}");
            }
        }
    }
}

/// 启动交互式终端工作台并在进入后自动跑一轮初始基线采样
pub async fn run_tui_workbench(initial_profile: ProfileConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let loop_res = event_loop(&mut terminal, initial_profile).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    loop_res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    initial_profile: ProfileConfig,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorkerEvent>();
    let mut state = WorkbenchState::new(initial_profile);
    state.spawn_probe(tx.clone());

    loop {
        while let Ok(ev) = rx.try_recv() {
            state.running = false;
            state.active_job = None;
            match ev {
                WorkerEvent::ProbeDone(Ok(frame)) => {
                    state.status_line = format!(
                        "[{}] {} · 吞吐 {:.1} MiB/s",
                        frame.diagnosis.severity.badge(),
                        frame.diagnosis.headline,
                        frame.transport.aggregate_mib_s
                    );
                    state.frame = frame;
                }
                WorkerEvent::ProbeDone(Err(e)) => {
                    state.status_line = format!("PROBE FAILED · {e}");
                }
                WorkerEvent::TuneDone(Ok(outcome)) => {
                    state.knob = outcome.winner.knob;
                    state.frame = outcome.best_frame;
                    state.trials = outcome.trials;
                    state.rust_code = outcome.generated_rust_code;
                    state.python_code = outcome.generated_python_code;
                    state.inspector_tab = 1;
                    state.status_line = format!(
                        "TUNE COMPLETE · 最优参数 conc={} chunk={}KiB gap={}KiB | P99 {:.2}ms | {:.1} MiB/s",
                        state.knob.concurrent,
                        state.knob.chunk_bytes / 1024,
                        state.knob.range_gap_bytes / 1024,
                        outcome.winner.worst_p99_ms,
                        outcome.winner.aggregate_mib_s
                    );
                }
                WorkerEvent::TuneDone(Err(e)) => {
                    state.status_line = format!("AUTO-TUNE FAILED · {e}");
                }
                WorkerEvent::NetProbeDone(Ok(hs)) => {
                    state.status_line = format!(
                        "NET-PROBE OK · {} ({}) DNS {:.2}ms | TCP {:.2}ms | TLS {:.2}ms | ALPN {}",
                        hs.host,
                        hs.tls_version,
                        hs.dns_p50_ms,
                        hs.tcp_rtt_p50_ms,
                        hs.tls_p50_ms,
                        hs.negotiated_alpn
                    );
                    state.frame.handshake = hs;
                    state.inspector_tab = 3;
                }
                WorkerEvent::NetProbeDone(Err(e)) => {
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
                    state.cancel_job();
                    break;
                }

                if state.minibuffer_active {
                    match key.code {
                        KeyCode::Esc => {
                            state.minibuffer_active = false;
                            state.minibuffer_input.clear();
                        }
                        KeyCode::Backspace => {
                            state.minibuffer_input.pop();
                        }
                        KeyCode::Enter => {
                            let cmd_raw = state.minibuffer_input.clone();
                            state.minibuffer_active = false;
                            state.minibuffer_input.clear();
                            match parse_minibuffer_command(&cmd_raw) {
                                Ok(MinibufferCommand::Quit) => {
                                    state.cancel_job();
                                    break;
                                }
                                Ok(MinibufferCommand::ProbeOnce) => {
                                    state.spawn_probe(tx.clone());
                                }
                                Ok(MinibufferCommand::AutoTune) => {
                                    state.spawn_autotune(tx.clone());
                                }
                                Ok(MinibufferCommand::NetProbe(url)) => {
                                    state.spawn_net_probe(url, tx.clone());
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
                                            state.spawn_probe(tx.clone());
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
                        state.cancel_job();
                        break;
                    }
                    KeyCode::Esc => {
                        state.cancel_job();
                    }
                    KeyCode::Char('/') | KeyCode::Char(':') => {
                        state.minibuffer_active = true;
                        state.minibuffer_input.clear();
                    }
                    KeyCode::Char(' ') => {
                        if state.running {
                            state.cancel_job();
                        } else {
                            state.spawn_probe(tx.clone());
                        }
                    }
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        state.spawn_autotune(tx.clone());
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        state.export_all("oprobe_report");
                    }
                    KeyCode::Tab => {
                        state.inspector_tab = (state.inspector_tab + 1) % 4;
                    }
                    KeyCode::Char('1') => {
                        state.profile.workload.kind = WorkloadKind::ParquetPointLookup;
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char('2') => {
                        state.profile.workload.kind = WorkloadKind::CheckpointMultipart;
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char('3') => {
                        state.profile.workload.kind = WorkloadKind::MetadataPrefixStorm;
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char('4') => {
                        state.profile.workload.kind = WorkloadKind::MixedReadWrite;
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char('f') | KeyCode::Char('F') => {
                        state.knob.enable_foyer = !state.knob.enable_foyer;
                        state.refresh_codegen();
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char('t') | KeyCode::Char('T') => {
                        state.knob.enable_tail_cut = !state.knob.enable_tail_cut;
                        state.refresh_codegen();
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char('k') | KeyCode::Char('K') => {
                        state.knob.keep_alive = !state.knob.keep_alive;
                        state.refresh_codegen();
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char('[') => {
                        state.knob.concurrent = state.knob.concurrent.saturating_sub(4).max(2);
                        state.refresh_codegen();
                        state.spawn_probe(tx.clone());
                    }
                    KeyCode::Char(']') => {
                        state.knob.concurrent = (state.knob.concurrent + 8).min(128);
                        state.refresh_codegen();
                        state.spawn_probe(tx.clone());
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
            " oprobe v0.1.0 ",
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " target={} ({}) | ",
            state.profile.target.name, state.profile.target.scheme
        )),
        Span::styled(
            format!("[1]Parquet{} ", if wk == WorkloadKind::ParquetPointLookup { "*" } else { "" }),
            Style::default().fg(if wk == WorkloadKind::ParquetPointLookup { Color::Yellow } else { Color::Gray }),
        ),
        Span::styled(
            format!("[2]Ckpt{} ", if wk == WorkloadKind::CheckpointMultipart { "*" } else { "" }),
            Style::default().fg(if wk == WorkloadKind::CheckpointMultipart { Color::Yellow } else { Color::Gray }),
        ),
        Span::styled(
            format!("[3]Meta{} ", if wk == WorkloadKind::MetadataPrefixStorm { "*" } else { "" }),
            Style::default().fg(if wk == WorkloadKind::MetadataPrefixStorm { Color::Yellow } else { Color::Gray }),
        ),
        Span::styled(
            format!("[4]Mixed{} ", if wk == WorkloadKind::MixedReadWrite { "*" } else { "" }),
            Style::default().fg(if wk == WorkloadKind::MixedReadWrite { Color::Yellow } else { Color::Gray }),
        ),
        Span::raw(format!(
            "| [F]Foyer:{} [T]TailCut:{} [K]KeepAlive:{} [ [/] ]Conc:{} Chunk:{}KiB Gap:{}KiB",
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
            .title(" Apache OpenDAL White-Box Storage Stack Profiler & Auto-Tuner "),
    );
    f.render_widget(header_block, root_chunks[0]);

    // 2. Three KPI Cards Row
    let kpi_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .split(root_chunks[1]);

    let diag = &state.frame.diagnosis;
    let diag_card = Paragraph::new(vec![
        Line::from(Span::styled(
            if diag.headline.is_empty() {
                "等待首轮采样..."
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
        " 根因定责 [{}] ",
        diag.severity.badge()
    )));
    f.render_widget(diag_card, kpi_cols[0]);

    let tp = &state.frame.transport;
    let amp_card = Paragraph::new(vec![
        Line::from(format!(
            "连接复用 {:.1}% | 读放大 {:.2}x | RPC 放大 {:.2}x",
            tp.socket_reuse_pct, tp.read_amplification, tp.rpc_amplification
        )),
        Line::from(format!(
            "缓存命中 {:.1}% | 重试 {} 次 | 429/503 限流 {}",
            tp.cache_hit_ratio_pct,
            tp.retry_attempts,
            tp.http_429_count + tp.http_503_slowdown_count
        )),
    ])
    .block(Block::default().borders(Borders::ALL).title(" 传输复用与读写放大 "));
    f.render_widget(amp_card, kpi_cols[1]);

    let host_card = Paragraph::new(vec![
        Line::from(Span::styled(
            format!(
                "{:.1} MiB/s ({:.1} MiB/s/核 · {} 核)",
                tp.aggregate_mib_s, tp.mib_per_cpu_core, tp.cpu_cores
            ),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "RSS {} MiB | CRC32C 校验通过 {} / 异常 {}",
            tp.process_rss_mib, tp.crc32c_verified_ok, tp.crc32c_corrupted
        )),
    ])
    .block(Block::default().borders(Borders::ALL).title(" 吞吐与压测机自证 "));
    f.render_widget(host_card, kpi_cols[2]);

    // 3. Six-Stage Waterfall + Sparkline Row
    let mid_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(root_chunks[2]);

    draw_waterfall_panel(f, mid_cols[0], state);
    draw_sparkline_panel(f, mid_cols[1], state);

    // 4. Per-Verb HDR Histogram Table
    let header_cells = [
        "OPERATION VERB",
        "COUNT",
        "OPS/s",
        "P50(ms)",
        "P90(ms)",
        "P99(ms)",
        "P99.9(ms)",
        "STREAM",
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
            Cell::from(format!("{:.2}", v.p90_ms)),
            Cell::from(format!("{:.2}", v.p99_ms)),
            Cell::from(format!("{:.2}", v.p999_ms)),
            Cell::from(format!("{:.1} MiB/s", v.single_stream_mib_s)),
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
            Constraint::Length(12),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(header_cells))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" 分操作语义独立 HDR 延迟分位谱 (杜绝 stat 稀释大对象读写长尾) "),
    );
    f.render_widget(verb_table, root_chunks[3]);

    // 5. Multi-Tab Inspector Panel
    draw_inspector_tabs(f, root_chunks[4], state);

    // 6. Bottom Status / Interactive `/` Minibuffer Bar
    if state.minibuffer_active {
        let cmd_bar = Paragraph::new(Line::from(vec![
            Span::styled(
                " COMMAND / ",
                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" :"),
            Span::styled(
                format!("{}_", state.minibuffer_input),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   (Enter 执行 | Esc 取消 | 试试 `help`, `conc 32`, `scheme fs`, `export snap`)",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(cmd_bar, root_chunks[5]);
    } else {
        let status_bar = Paragraph::new(Line::from(vec![
            Span::styled(
                if state.running { " RUNNING " } else { " IDLE " },
                Style::default()
                    .fg(Color::Black)
                    .bg(if state.running { Color::Yellow } else { Color::Green })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {}", state.status_line)),
        ]))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(status_bar, root_chunks[5]);
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
        Color::Blue,
    ];
    let bar_width = area.width.saturating_sub(4) as usize;
    let mut bar_spans = Vec::new();
    for (idx, (_, _, pct)) in stages.iter().enumerate() {
        let cells = (((pct / 100.0) * bar_width as f64).round() as usize).max(if *pct > 1.0 { 1 } else { 0 });
        if cells > 0 {
            bar_spans.push(Span::styled("█".repeat(cells), Style::default().fg(colors[idx])));
        }
    }
    if bar_spans.is_empty() {
        bar_spans.push(Span::styled("░".repeat(bar_width.min(48)), Style::default().fg(Color::DarkGray)));
    }

    let legend_1 = Line::from(vec![
        Span::styled("■1.排队 ", Style::default().fg(Color::Yellow)),
        Span::raw(format!("{:.2}ms ({:.0}%)  ", stages[0].1, stages[0].2)),
        Span::styled("■2.缓存 ", Style::default().fg(Color::Green)),
        Span::raw(format!("{:.2}ms ({:.0}%)  ", stages[1].1, stages[1].2)),
        Span::styled("■3.建连/TLS ", Style::default().fg(Color::Magenta)),
        Span::raw(format!("{:.2}ms ({:.0}%)", stages[2].1, stages[2].2)),
    ]);
    let legend_2 = Line::from(vec![
        Span::styled("■4.网关TTFB ", Style::default().fg(Color::Red)),
        Span::raw(format!("{:.2}ms ({:.0}%)  ", stages[3].1, stages[3].2)),
        Span::styled("■5.分块流 ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{:.2}ms ({:.0}%)  ", stages[4].1, stages[4].2)),
        Span::styled("■6.合并/CRC ", Style::default().fg(Color::Blue)),
        Span::raw(format!("{:.2}ms ({:.0}%)", stages[5].1, stages[5].2)),
    ]);

    let p = Paragraph::new(vec![Line::from(bar_spans), legend_1, legend_2]).block(
        Block::default().borders(Borders::ALL).title(format!(
            " 六段式全链路白盒耗时瀑布 (单次均值 {:.2} ms) ",
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
        .map(|pt| pt.throughput_mib_s.round().max(0.0) as u64)
        .collect();
    let spark = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 实时吞吐趋势 (MiB/s) "),
        )
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
        " [Tab 1] AwaitTree 慢协程栈 ",
        " [Tab 2] 自动寻优矩阵与生产代码 ",
        " [Tab 3] 多云 TCO 与放大损耗 (S3/R2/B2) ",
        " [Tab 4] Socket/TLS 微秒握手与 Request-ID ",
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
                buf.push_str("最近自动寻优试验 (Top 5):\n");
                let mut sorted = state.trials.clone();
                sorted.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                for t in sorted.iter().take(5) {
                    buf.push_str(&format!(
                        "  #{} [{}] conc={} chunk={}KiB gap={}KiB -> P99 {:.2}ms | {:.1} MiB/s | ReadAmp {:.2}x | Score {:.1}\n",
                        t.trial_id,
                        t.phase,
                        t.knob.concurrent,
                        t.knob.chunk_bytes / 1024,
                        t.knob.range_gap_bytes / 1024,
                        t.worst_p99_ms,
                        t.aggregate_mib_s,
                        t.read_amplification,
                        t.score
                    ));
                }
                buf.push('\n');
            }
            buf.push_str(&state.rust_code);
            buf
        }
        2 => {
            let finops = compare_multi_cloud_finops(
                &state.frame.transport,
                state.profile.workload.kind.write_ratio(),
                state.profile.workload.internet_egress_ratio,
            );
            let mut lines = vec![format!(
                "{:<22} {:>14} {:>14} {:>14} {:>16} {:>18}",
                "CLOUD PROVIDER", "STORAGE/mo", "API FEES/mo", "EGRESS/mo", "TOTAL USD/mo", "AMP WASTE USD/mo"
            )];
            for c in finops {
                lines.push(format!(
                    "{:<22} {:>13.2}$ {:>13.2}$ {:>13.2}$ {:>15.2}$ {:>17.2}$",
                    c.provider_name,
                    c.monthly_storage_usd,
                    c.monthly_api_requests_usd,
                    c.monthly_egress_usd,
                    c.total_monthly_usd,
                    c.waste_from_amplification_usd
                ));
            }
            lines.push(String::new());
            lines.push(
                "注：按 10 TB 存储驻留 + 50 TB 月度逻辑扫描折算；调优 chunk 与 gap 可直接削减 API 请求费与公网流出放大损耗。"
                    .to_owned(),
            );
            lines.join("\n")
        }
        _ => {
            let hs = &state.frame.handshake;
            let mut s = format!(
                "Endpoint: {} (Host: {}:{}, TLS: {}, ALPN: {}, Cipher: {})\n\
                 DNS Lookup P50/P99: {:.2} / {:.2} ms | TCP SYN/ACK RTT P50/P99: {:.2} / {:.2} ms | TLS Handshake P50/P99: {:.2} / {:.2} ms\n\
                 Resolved IPs: {:?}  (可在 `/` 命令栏输入 `net https://s3.amazonaws.com` 发起实测握手剖析)\n\n\
                 Recent Gateway Request-IDs:\n",
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
                hs.resolved_ips,
            );
            if state.frame.transport.recent_request_ids.is_empty() {
                s.push_str("  (当前后端为进程内存储或尚无远端响应头 Request-ID)");
            } else {
                for req_id in &state.frame.transport.recent_request_ids {
                    s.push_str(&format!("  - {req_id}\n"));
                }
            }
            s
        }
    };

    let body = Paragraph::new(content_text)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(body, chunks[1]);
}
