//! `oprobe top`: interactive terminal workbench. Runs workloads against the target, shows
//! live throughput/latency, the time breakdown, request stages, connections, slow-op
//! timelines and findings, and lets you change knobs or launch the tuner on the fly.

use std::io::Stdout;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, Tabs, Wrap,
};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use crate::analysis::diagnosis::{diagnose, Severity};
use crate::config::{Knobs, Preset, Profile};
use crate::engine::{Cancel, Progress, ProgressFn, RunOptions, Session};
use crate::net::HttpVersion;
use crate::report::Report;
use crate::trace::{Phase, Protocol, Snapshot};
use crate::util::{fmt_bytes, fmt_ms, ByteSize};

enum Msg {
    Key(KeyEvent),
    Resize,
    Snapshot(Box<Snapshot>),
    Log(String),
    SessionReady(Result<Arc<Session>>),
    RunDone(Result<Box<Snapshot>>),
    TuneDone(Result<(Knobs, f64)>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
    Idle,
    Preparing,
    Running,
    Tuning,
}

const TABS: [&str; 5] = [
    "Operations",
    "Requests",
    "Connections",
    "Slowest",
    "Diagnosis",
];

struct App {
    profile: Profile,
    knobs: Knobs,
    session: Option<Arc<Session>>,
    cancel: Cancel,
    job: Job,
    snap: Option<Snapshot>,
    last_final: Option<Snapshot>,
    log: Vec<String>,
    tab: usize,
    help: bool,
    continuous: bool,
    tx: mpsc::UnboundedSender<Msg>,
    quit: bool,
    /// Instantaneous (ops/s, p99 ms) samples computed between live snapshots.
    history: std::collections::VecDeque<(f64, f64)>,
    last_sample: Option<(f64, u64)>,
}

impl App {
    fn log(&mut self, m: impl Into<String>) {
        let ts = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
        self.log.push(format!("{} {}", &ts[11..19], m.into()));
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn open_session(&mut self) {
        self.job = Job::Preparing;
        self.snap = None;
        let tx = self.tx.clone();
        let mut profile = self.profile.clone();
        profile.knobs = self.knobs.clone();
        let cancel = self.cancel.clone();
        self.log(format!(
            "preparing {} ({})",
            profile.target.display(),
            profile.workload.preset.slug()
        ));
        tokio::spawn(async move {
            let t2 = tx.clone();
            let progress: ProgressFn = Arc::new(move |p| {
                if let Progress::Message(m) = p {
                    let _ = t2.send(Msg::Log(m));
                }
            });
            let s = Session::open(profile, cancel, progress).await.map(Arc::new);
            let _ = tx.send(Msg::SessionReady(s));
        });
    }

    fn start_run(&mut self) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.cancel.reset();
        self.job = Job::Running;
        self.history.clear();
        self.last_sample = None;
        let tx = self.tx.clone();
        let knobs = self.knobs.clone();
        let mut opts = RunOptions::from_profile(&session.profile);
        if self.continuous {
            opts.duration = Duration::from_secs(24 * 3600);
        }
        self.log(format!(
            "run started: {} for {}",
            knobs.summary(),
            if self.continuous {
                "until stopped".to_owned()
            } else {
                format!("{:.0}s", opts.duration.as_secs_f64())
            }
        ));
        tokio::spawn(async move {
            let t2 = tx.clone();
            let progress: ProgressFn = Arc::new(move |p| {
                let _ = t2.send(match p {
                    Progress::Snapshot(s) => Msg::Snapshot(s),
                    Progress::Message(m) => Msg::Log(m),
                });
            });
            let r = session.run(&knobs, &opts, Some(progress)).await;
            let _ = tx.send(Msg::RunDone(r.map(Box::new)));
        });
    }

    fn start_tune(&mut self) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.cancel.reset();
        self.job = Job::Tuning;
        let tx = self.tx.clone();
        self.log("tuning: 3s trials, throttle-aware concurrency ladder, then knob refinement");
        tokio::spawn(async move {
            use crate::tune::{tune, Objective, TuneEvent, TuneOptions};
            let opts = TuneOptions {
                objective: Objective::Throughput,
                trial: RunOptions {
                    duration: Duration::from_secs(3),
                    warmup: Duration::from_millis(500),
                    max_ops: None,
                    rate: session.profile.workload.rate,
                    trace_slow_ops: false,
                },
                max_concurrency: 256,
                max_trials: 16,
                cooldown: Duration::from_secs(3),
            };
            let t2 = tx.clone();
            let ev: Arc<dyn Fn(TuneEvent) + Send + Sync> = Arc::new(move |e| {
                let m = match e {
                    TuneEvent::Started { id, stage, knobs } => {
                        format!("trial {id} [{stage}] {}", knobs.summary())
                    }
                    TuneEvent::Finished(t) => format!(
                        "  → {:.1} ops/s {:.1} MiB/s p99 {} {}",
                        t.ops_per_s,
                        t.mib_per_s,
                        fmt_ms(t.p99_ms),
                        if t.throttled() {
                            format!("THROTTLED ×{}", t.throttled_responses)
                        } else if t.feasible {
                            "ok".into()
                        } else {
                            t.note.clone()
                        }
                    ),
                    TuneEvent::Message(m) => m,
                };
                let _ = t2.send(Msg::Log(m));
            });
            let r = tune(&session, &opts, ev)
                .await
                .map(|o| (o.winner, o.summary.improvement));
            let _ = tx.send(Msg::TuneDone(r));
        });
    }

    fn stop(&mut self) {
        if matches!(self.job, Job::Running | Job::Tuning) {
            self.cancel.cancel();
            self.log("stopping…");
        }
    }

    fn adjust(&mut self, f: impl FnOnce(&mut Knobs) -> String) {
        let msg = f(&mut self.knobs);
        let suffix = if self.job == Job::Running {
            " (applies to the next run)"
        } else {
            ""
        };
        self.log(format!("{msg}{suffix}"));
    }

    fn export(&mut self) {
        let Some(s) = self.last_final.clone().or_else(|| self.snap.clone()) else {
            self.log("nothing to export yet");
            return;
        };
        let p = self
            .session
            .as_ref()
            .map(|s| s.profile.clone())
            .unwrap_or_else(|| self.profile.clone());
        let r = Report::build(&p, &self.knobs, s, Default::default(), vec![]);
        let stamp = crate::util::compact_utc_now();
        let json = std::path::PathBuf::from(format!("oprobe-{stamp}.json"));
        let md = std::path::PathBuf::from(format!("oprobe-{stamp}.md"));
        match r
            .save_json(&json)
            .and_then(|_| crate::report::write_file(&md, &crate::report::markdown::render(&r)))
        {
            Ok(_) => self.log(format!("exported {} and {}", json.display(), md.display())),
            Err(e) => self.log(format!("export failed: {e:#}")),
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(' ') | KeyCode::Enter => match self.job {
                Job::Idle => self.start_run(),
                Job::Running | Job::Tuning => self.stop(),
                Job::Preparing => {}
            },
            KeyCode::Char('l') => {
                self.continuous = !self.continuous;
                let m = if self.continuous {
                    "continuous mode: runs until stopped"
                } else {
                    "timed mode: runs for the profile duration"
                };
                self.log(m);
            }
            KeyCode::Char('t') if self.job == Job::Idle => self.start_tune(),
            KeyCode::Char('e') => self.export(),
            KeyCode::Tab | KeyCode::Right => self.tab = (self.tab + 1) % TABS.len(),
            KeyCode::BackTab | KeyCode::Left => self.tab = (self.tab + TABS.len() - 1) % TABS.len(),
            KeyCode::Char(c @ '1'..='5') => self.tab = (c as usize) - ('1' as usize),
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust(|k| {
                k.concurrency = (k.concurrency * 2).min(1024);
                format!("concurrency → {}", k.concurrency)
            }),
            KeyCode::Char('-') => self.adjust(|k| {
                k.concurrency = (k.concurrency / 2).max(1);
                format!("concurrency → {}", k.concurrency)
            }),
            KeyCode::Char('i') => self.adjust(|k| {
                k.io_concurrency = (k.io_concurrency * 2).min(64);
                format!("io_concurrency → {}", k.io_concurrency)
            }),
            KeyCode::Char('I') => self.adjust(|k| {
                k.io_concurrency = (k.io_concurrency / 2).max(1);
                format!("io_concurrency → {}", k.io_concurrency)
            }),
            KeyCode::Char('k') => self.adjust(|k| {
                k.chunk_size = ByteSize((k.chunk_size.0 * 2).min(256 << 20));
                format!("chunk_size → {}", k.chunk_size)
            }),
            KeyCode::Char('K') => self.adjust(|k| {
                k.chunk_size = ByteSize((k.chunk_size.0 / 2).max(256 << 10));
                format!("chunk_size → {}", k.chunk_size)
            }),
            KeyCode::Char('g') => self.adjust(|k| {
                k.range_gap = ByteSize(if k.range_gap.0 == 0 {
                    16 << 10
                } else {
                    (k.range_gap.0 * 4).min(16 << 20)
                });
                format!("range_gap → {}", k.range_gap)
            }),
            KeyCode::Char('G') => self.adjust(|k| {
                k.range_gap = ByteSize(if k.range_gap.0 <= 16 << 10 {
                    0
                } else {
                    k.range_gap.0 / 4
                });
                format!("range_gap → {}", k.range_gap)
            }),
            KeyCode::Char('c') => self.adjust(|k| {
                k.cache = !k.cache;
                format!(
                    "cache → {}",
                    if k.cache {
                        k.cache_size.to_string()
                    } else {
                        "off".into()
                    }
                )
            }),
            KeyCode::Char('h') => self.adjust(|k| {
                k.http_version = match k.http_version {
                    HttpVersion::Auto => HttpVersion::H1,
                    HttpVersion::H1 => HttpVersion::H2,
                    HttpVersion::H2 => HttpVersion::Auto,
                };
                format!("http_version → {:?}", k.http_version)
            }),
            KeyCode::Char('a') => self.adjust(|k| {
                k.keep_alive = !k.keep_alive;
                format!("keep_alive → {}", k.keep_alive)
            }),
            KeyCode::Char('x') => self.adjust(|k| {
                k.tail_cut = !k.tail_cut;
                format!("tail_cut → {}", k.tail_cut)
            }),
            KeyCode::Char('p') if self.job == Job::Idle => {
                let idx = Preset::ALL
                    .iter()
                    .position(|p| *p == self.profile.workload.preset)
                    .unwrap_or(0);
                self.profile.workload.preset = Preset::ALL[(idx + 1) % Preset::ALL.len()];
                self.profile.workload.mix.clear();
                self.profile.workload.dataset = None;
                if let Some(s) = self.session.take() {
                    tokio::spawn(async move {
                        s.close().await;
                    });
                }
                self.open_session();
            }
            _ => {}
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    out.execute(EnterAlternateScreen)?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        prev(info);
    }));
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = std::io::stdout().execute(LeaveAlternateScreen);
}

pub async fn run(profile: Profile) -> Result<()> {
    use std::io::IsTerminal;
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "`oprobe top` needs an interactive terminal; use `oprobe run` for scripts and CI"
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stop_input = Arc::new(Mutex::new(false));
    {
        let tx = tx.clone();
        let stop = stop_input.clone();
        std::thread::spawn(move || loop {
            if *stop.lock().unwrap_or_else(|e| e.into_inner()) {
                break;
            }
            if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Key(k)) => {
                        if tx.send(Msg::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(..)) => {
                        let _ = tx.send(Msg::Resize);
                    }
                    _ => {}
                }
            }
        });
    }
    let mut term = setup_terminal()?;
    let mut app = App {
        knobs: profile.knobs.clone(),
        profile,
        session: None,
        cancel: Cancel::new(),
        job: Job::Idle,
        snap: None,
        last_final: None,
        log: Vec::new(),
        tab: 0,
        help: false,
        continuous: false,
        tx: tx.clone(),
        quit: false,
        history: std::collections::VecDeque::new(),
        last_sample: None,
    };
    app.log("welcome to oprobe top — press ? for keys");
    app.open_session();

    let result: Result<()> = async {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            term.draw(|f| draw(f, &app))?;
            tokio::select! {
                _ = tick.tick() => {}
                m = rx.recv() => {
                    let Some(m) = m else { break };
                    match m {
                        Msg::Key(k) => app.on_key(k),
                        Msg::Resize => {}
                        Msg::Snapshot(s) => {
                            if s.phase == Phase::Measure {
                                let (t, n) = (s.elapsed_s, s.summary.ops);
                                if let Some((t0, n0)) = app.last_sample {
                                    if t > t0 && n >= n0 {
                                        let p99 = s.timeline.last().map(|p| p.p99_ms).unwrap_or(s.summary.latency.p99);
                                        app.history.push_back(((n - n0) as f64 / (t - t0), p99));
                                        if app.history.len() > 600 {
                                            app.history.pop_front();
                                        }
                                    }
                                }
                                app.last_sample = Some((t, n));
                            }
                            app.snap = Some(*s);
                        }
                        Msg::Log(l) => app.log(l),
                        Msg::SessionReady(Ok(s)) => {
                            for w in &s.warnings {
                                app.log(format!("warning: {w}"));
                            }
                            app.session = Some(s);
                            app.job = Job::Idle;
                            app.log("ready — press space to start a run, t to auto-tune");
                        }
                        Msg::SessionReady(Err(e)) => {
                            app.job = Job::Idle;
                            app.log(format!("error: {e:#}"));
                            if let Some(h) = crate::analysis::hints::hint_for(&format!("{e:#}"), &app.profile.target.scheme) {
                                app.log(format!("hint: {}", h.title));
                                for s in h.steps { app.log(format!("  • {s}")); }
                            }
                        }
                        Msg::RunDone(r) => {
                            app.job = Job::Idle;
                            match r {
                                Ok(s) => {
                                    let s = *s;
                                    app.log(format!("run finished: {:.1} ops/s · {:.1} MiB/s · p99 {}", s.summary.ops_per_s, s.summary.mib_per_s, fmt_ms(s.summary.latency.p99)));
                                    app.snap = Some(s.clone());
                                    app.last_final = Some(s);
                                }
                                Err(e) => app.log(format!("run failed: {e:#}")),
                            }
                        }
                        Msg::TuneDone(r) => {
                            app.job = Job::Idle;
                            match r {
                                Ok((k, imp)) => {
                                    app.log(format!("tuned: {} ({:+.0}% vs baseline) — applied", k.summary(), imp * 100.0));
                                    app.knobs = k;
                                }
                                Err(e) => app.log(format!("tune failed: {e:#}")),
                            }
                        }
                    }
                }
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    }
    .await;

    app.cancel.cancel();
    if let Some(s) = app.session.take() {
        term.draw(|f| {
            let a = centered(f.area(), 50, 5);
            f.render_widget(Clear, a);
            f.render_widget(
                Paragraph::new("cleaning up test data…")
                    .block(Block::bordered().border_type(BorderType::Rounded)),
                a,
            );
        })?;
        // Let running jobs observe the cancellation before deleting their data.
        tokio::time::sleep(Duration::from_millis(300)).await;
        s.close().await;
    }
    *stop_input.lock().unwrap_or_else(|e| e.into_inner()) = true;
    restore_terminal();
    result
}

fn centered(r: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(r.width);
    let h = h.min(r.height);
    Rect::new(r.x + (r.width - w) / 2, r.y + (r.height - h) / 2, w, h)
}

fn block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(10),
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(f, rows[0], app);
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(rows[1]);
    draw_sparks(f, mid[0], app);
    draw_breakdown(f, mid[1], app);
    draw_tabs(f, rows[2], app);
    draw_log(f, rows[3], app);
    let keys = if area.width >= 120 {
        " space run/stop · t tune · +/- conc · i/I io · k/K chunk · g/G gap · c cache · h http · p preset · e export · ? help · q quit"
    } else {
        " space run/stop · t tune · +/- conc · e export · ? help · q quit"
    };
    f.render_widget(
        Paragraph::new(keys).style(Style::default().fg(Color::DarkGray)),
        rows[4],
    );
    if app.help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let (state, color) = match app.job {
        Job::Idle => ("idle", Color::Gray),
        Job::Preparing => ("preparing", Color::Yellow),
        Job::Running => ("running", Color::Green),
        Job::Tuning => ("tuning", Color::Magenta),
    };
    let phase = app
        .snap
        .as_ref()
        .filter(|_| app.job == Job::Running)
        .map(|s| format!(" · {} {:.1}s", s.phase.label(), s.elapsed_s))
        .unwrap_or_default();
    let p = &app.profile;
    let title = Line::from(vec![
        Span::styled(
            " oprobe ",
            Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw(" "),
        Span::styled(p.target.display(), Style::default().bold()),
        Span::raw(format!("  {} · ", p.workload.preset.slug())),
        Span::styled(
            format!("● {state}{phase}"),
            Style::default().fg(color).bold(),
        ),
        Span::raw(if app.continuous { "  ∞" } else { "" }),
    ]);
    let knobs = Line::from(Span::styled(
        format!(" {}", app.knobs.summary()),
        Style::default().fg(Color::Gray),
    ));
    let mut lines = vec![title, knobs];
    if app.job == Job::Running && !app.continuous {
        if let Some(s) = &app.snap {
            if s.phase == Phase::Measure {
                let total = p.workload.duration.get().as_secs_f64().max(1e-3);
                let pct = (s.elapsed_s / total * 100.0).clamp(0.0, 100.0);
                lines[1] = Line::from(vec![
                    Span::styled(
                        format!(" {}", app.knobs.summary()),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(format!("   {pct:>3.0}%"), Style::default().fg(Color::Green)),
                ]);
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn draw_sparks(f: &mut Frame, area: Rect, app: &App) {
    let b = block("Live");
    let inner = b.inner(area);
    f.render_widget(b, area);
    let Some(s) = &app.snap else {
        f.render_widget(
            Paragraph::new("No data yet. Press space to run.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(inner);
    let w = inner.width as usize;
    let (ops, p99): (Vec<u64>, Vec<u64>) = if app.history.len() >= 2 {
        app.history
            .iter()
            .rev()
            .take(w)
            .rev()
            .map(|(o, p)| (*o as u64, (*p * 100.0) as u64))
            .unzip()
    } else {
        s.timeline
            .iter()
            .rev()
            .take(w)
            .rev()
            .map(|p| (p.ops_per_s as u64, (p.p99_ms * 100.0) as u64))
            .unzip()
    };
    let sm = &s.summary;
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{:.1} ops/s", sm.ops_per_s),
                Style::default().fg(Color::Green).bold(),
            ),
            Span::raw(format!(
                "  {:.1} MiB/s  in-flight {}",
                sm.mib_per_s, sm.inflight_ops
            )),
            if sm.failed_ops > 0 {
                Span::styled(
                    format!("  errors {}", sm.failed_ops),
                    Style::default().fg(Color::Red),
                )
            } else {
                Span::raw("")
            },
            if s.resilience.throttled_responses > 0 {
                Span::styled(
                    format!("  429/503 {}", s.resilience.throttled_responses),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                Span::raw("")
            },
        ])),
        parts[0],
    );
    f.render_widget(
        Sparkline::default()
            .data(&ops)
            .style(Style::default().fg(Color::Green)),
        parts[1],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(
                    "p50 {}  p99 {}",
                    fmt_ms(sm.latency.p50),
                    fmt_ms(sm.latency.p99)
                ),
                Style::default().fg(Color::Magenta).bold(),
            ),
            Span::raw(format!(
                "  p99.9 {}  max {}",
                fmt_ms(sm.latency.p999),
                fmt_ms(sm.latency.max)
            )),
        ])),
        parts[2],
    );
    f.render_widget(
        Sparkline::default()
            .data(&p99)
            .style(Style::default().fg(Color::Magenta)),
        parts[3],
    );
}

fn draw_breakdown(f: &mut Frame, area: Rect, app: &App) {
    let b = block("Where the time goes");
    let inner = b.inner(area);
    f.render_widget(b, area);
    let Some(s) = &app.snap else { return };
    let sh = crate::analysis::diagnosis::time_shares(s);
    let rows = [
        ("queue", sh.queue, Color::Yellow),
        ("connect", sh.connect, Color::Red),
        ("upload", sh.upload, Color::Blue),
        ("server", sh.server, Color::Magenta),
        ("download", sh.download, Color::Cyan),
        ("client", sh.client + sh.cache, Color::Gray),
    ];
    let bar_w = (inner.width as usize).saturating_sub(18).max(4);
    let max = rows.iter().map(|r| r.1).fold(0.0, f64::max).max(1e-9);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(name, v, c)| {
            Line::from(vec![
                Span::styled(format!("{name:<9}"), Style::default().fg(Color::Gray)),
                Span::styled(
                    crate::report::style::bar(v / max, bar_w),
                    Style::default().fg(*c),
                ),
                Span::styled(format!("{:>6.1}%", v * 100.0), Style::default().bold()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let b = block("Details");
    let inner = b.inner(area);
    f.render_widget(b, area);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    f.render_widget(
        Tabs::new(
            TABS.iter()
                .enumerate()
                .map(|(i, t)| format!("{} {t}", i + 1))
                .collect::<Vec<_>>(),
        )
        .select(app.tab)
        .highlight_style(Style::default().fg(Color::Cyan).bold().underlined())
        .style(Style::default().fg(Color::Gray)),
        parts[0],
    );
    let area = parts[1];
    let Some(s) = &app.snap else { return };
    let head = |cols: &[&str]| {
        Row::new(cols.iter().map(|c| Cell::from(c.to_string())))
            .style(Style::default().fg(Color::DarkGray))
    };
    match app.tab {
        0 => {
            let rows: Vec<Row> = s
                .verbs
                .iter()
                .map(|v| {
                    Row::new(vec![
                        v.verb.clone(),
                        v.count.to_string(),
                        format!("{:.1}", v.ops_per_s),
                        format!("{:.1}", v.mib_per_s),
                        fmt_ms(v.latency.p50),
                        fmt_ms(v.latency.p99),
                        fmt_ms(v.latency.max),
                        format!("{:.2}", v.requests_per_op),
                        format!(
                            "{}/{}/{}",
                            fmt_ms(v.breakdown.queue_ms),
                            fmt_ms(v.breakdown.backend_ms),
                            fmt_ms(v.breakdown.client_ms + v.breakdown.cache_ms)
                        ),
                        v.errors.to_string(),
                    ])
                })
                .collect();
            let widths = [14, 8, 9, 8, 8, 8, 8, 7, 24, 6].map(Constraint::Length);
            f.render_widget(
                Table::new(rows, widths).header(head(&[
                    "operation",
                    "count",
                    "ops/s",
                    "MiB/s",
                    "p50",
                    "p99",
                    "max",
                    "req/op",
                    "queue/backend/client",
                    "err",
                ])),
                area,
            );
        }
        1 => {
            let rows: Vec<Row> = s
                .apis
                .iter()
                .map(|a| {
                    let st = &a.stages;
                    Row::new(vec![
                        a.api.label().to_owned(),
                        a.count.to_string(),
                        format!("{:.0}%", a.reused_pct),
                        fmt_ms(st.queue.mean),
                        if st.connect.count > 0 {
                            fmt_ms(st.connect.mean)
                        } else {
                            "-".into()
                        },
                        fmt_ms(st.send.mean),
                        fmt_ms(st.ttfb.mean),
                        fmt_ms(st.body.mean),
                        fmt_ms(st.stall.mean),
                        fmt_ms(st.total.p99),
                        a.status
                            .iter()
                            .map(|(c, n)| format!("{c}×{n}"))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ])
                })
                .collect();
            let widths = [24, 8, 6, 8, 8, 8, 8, 9, 8, 8, 20].map(Constraint::Length);
            f.render_widget(
                Table::new(rows, widths).header(head(&[
                    "request", "count", "warm", "queue", "connect", "send", "ttfb", "transfer",
                    "stall", "p99", "status",
                ])),
                area,
            );
        }
        2 => {
            let c = &s.connections;
            let b = &s.bytes;
            let lines = vec![
                Line::from(format!(
                    "protocol {} · {} · {}{}",
                    if c.alpn.is_empty() { "-" } else { &c.alpn },
                    if c.tls_version.is_empty() { "plain" } else { &c.tls_version },
                    c.cipher,
                    c.proxy.as_ref().map(|p| format!(" · via {p}")).unwrap_or_default()
                )),
                Line::from(format!("{} new connections · {:.1}% requests warm · peak {} in-flight requests · {} dial failures", c.dialed, c.reuse_pct, c.peak_inflight_requests, c.dial_failures)),
                Line::from(format!("dial p50/p99: dns {}/{} · tcp {}/{} · proxy {}/{} · tls {}/{}", fmt_ms(c.dns.p50), fmt_ms(c.dns.p99), fmt_ms(c.tcp.p50), fmt_ms(c.tcp.p99), fmt_ms(c.proxy_connect.p50), fmt_ms(c.proxy_connect.p99), fmt_ms(c.tls.p50), fmt_ms(c.tls.p99))),
                Line::from(format!("remote {}", c.remote_addrs.join(", "))),
                Line::from(""),
                Line::from(format!("bytes: logical read {} write {} · backend down {} up {} · wire rx {} tx {}", fmt_bytes(b.logical_read), fmt_bytes(b.logical_write), fmt_bytes(b.backend_down), fmt_bytes(b.backend_up), fmt_bytes(b.wire_rx), fmt_bytes(b.wire_tx))),
                Line::from(format!("read amplification {:.2}× · over-read {:.2}× · {:.2} requests/op · cache hit {:.1}%", b.read_amplification, b.overread_ratio, b.request_amplification, s.cache.hit_ratio * 100.0)),
                Line::from(format!("client cpu {:.0}% of one core ({} cores) · rss {:.0} MiB", s.client.cpu_pct, s.client.cpu_cores, s.client.rss_mib)),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
        }
        3 => {
            let mut lines = Vec::new();
            for op in s.slow_ops.iter().take(3) {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} {}", op.verb, fmt_ms(op.latency_ms)),
                        Style::default().bold(),
                    ),
                    Span::styled(
                        format!(
                            "  at {:.1}s · queue {} backend {} client {}",
                            op.at_s,
                            fmt_ms(op.breakdown.queue_ms),
                            fmt_ms(op.breakdown.backend_ms),
                            fmt_ms(op.breakdown.client_ms)
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                for r in op.requests.iter().take(6) {
                    let t = &r.timing;
                    let ms = |us: u64| fmt_ms(us as f64 / 1000.0);
                    let conn = if r.protocol == Protocol::InProcess {
                        "local".into()
                    } else if r.reused {
                        format!("warm #{}", r.conn_id)
                    } else {
                        format!("cold #{} connect {}", r.conn_id, ms(t.connect_us()))
                    };
                    lines.push(Line::from(format!(
                        "  +{:<7} {:<22} {:>3} {conn:<18} queue {} send {} ttfb {} transfer {} = {}",
                        fmt_ms(r.start_offset_us as f64 / 1000.0),
                        r.api.label(),
                        r.status,
                        ms(t.queue_us),
                        ms(t.send_us),
                        ms(t.ttfb_us),
                        ms(t.body_us),
                        ms(t.total_us)
                    )));
                }
            }
            if lines.is_empty() {
                lines.push(Line::from("no operations yet"));
            }
            f.render_widget(Paragraph::new(lines), area);
        }
        _ => {
            let d = diagnose(
                s,
                &app.knobs,
                &app.profile.sla,
                app.profile.target.is_network(),
            );
            let mut lines = vec![Line::from(Span::styled(
                d.headline.clone(),
                Style::default().bold(),
            ))];
            for fi in &d.findings {
                let (icon, c) = match fi.severity {
                    Severity::Critical => ("✖", Color::Red),
                    Severity::Warning => ("▲", Color::Yellow),
                    Severity::Info => ("●", Color::Blue),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{icon} "), Style::default().fg(c)),
                    Span::styled(fi.title.clone(), Style::default().bold()),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("   {}", fi.evidence),
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(format!("   → {}", fi.action)));
            }
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        }
    }
}

fn draw_log(f: &mut Frame, area: Rect, app: &App) {
    let b = block("Log");
    let inner = b.inner(area);
    f.render_widget(b, area);
    let n = inner.height as usize;
    let lines: Vec<Line> = app
        .log
        .iter()
        .rev()
        .take(n)
        .rev()
        .map(|l| {
            let style = if l.contains("error") || l.contains("failed") || l.contains("THROTTLED") {
                Style::default().fg(Color::Red)
            } else if l.contains("warning") || l.contains("hint") {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let a = centered(area, 64, 22);
    f.render_widget(Clear, a);
    let rows = [
        ("space / enter", "start or stop a run"),
        ("l", "toggle continuous mode (run until stopped)"),
        ("t", "auto-tune (throttle-aware) and apply the winner"),
        ("+ / -", "double / halve concurrency"),
        ("i / I", "double / halve per-op io concurrency"),
        ("k / K", "double / halve chunk size"),
        ("g / G", "increase / decrease range gap"),
        ("c", "toggle read cache"),
        ("h", "cycle HTTP version auto → h1 → h2"),
        ("a", "toggle keep-alive"),
        ("x", "toggle tail-cut (straggler retry)"),
        ("p", "next workload preset (re-prepares data)"),
        ("tab / 1-5", "switch detail panel"),
        ("e", "export JSON + Markdown report"),
        ("q / esc", "quit (test data is cleaned up)"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!("  {k:<16}"),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::raw(*d),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(block("Keys — press any key to close")),
        a,
    );
}
