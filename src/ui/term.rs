//! `oprobe top`: the same NSH UI drawn on the terminal grid (`nsh-platform-tui`).
//! Event conversion follows NSH's own terminal host (`nsh-tui-host`, Apache-2.0).

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::{ExecutableCommand, terminal};
use nsh_core::element::Root;
use nsh_platform_tui::{
    AppOptions, AppView, ColorDepth, Input, Modifiers, MouseButton, MouseInput, MouseKind,
    RunEvent, run_app,
};
use ratatui_core::terminal::Terminal;
use ratatui_crossterm::CrosstermBackend;

use crate::ui::bridge;

fn modifiers(keys: KeyModifiers) -> Modifiers {
    Modifiers {
        ctrl: keys.contains(KeyModifiers::CONTROL),
        alt: keys.contains(KeyModifiers::ALT),
        shift: keys.contains(KeyModifiers::SHIFT),
        cmd: keys.contains(KeyModifiers::SUPER),
    }
}

fn button(b: event::MouseButton) -> MouseButton {
    match b {
        event::MouseButton::Left => MouseButton::Left,
        event::MouseButton::Right => MouseButton::Right,
        event::MouseButton::Middle => MouseButton::Middle,
    }
}

fn key_name(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Escape".into(),
        KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Left => "ArrowLeft".into(),
        KeyCode::Right => "ArrowRight".into(),
        KeyCode::Up => "ArrowUp".into(),
        KeyCode::Down => "ArrowDown".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => "Unidentified".into(),
    }
}

/// Convert a crossterm event into the surface's input.
pub fn convert(ev: Event) -> RunEvent {
    match ev {
        Event::Resize(..) => RunEvent::Redraw,
        Event::FocusGained => {
            // Coming back to the terminal: runs may have been saved meanwhile.
            crate::ui::store::request_refresh();
            RunEvent::Input(Input::Focus(true))
        }
        Event::FocusLost => RunEvent::Input(Input::Focus(false)),
        Event::Paste(t) => RunEvent::Input(Input::Paste(t)),
        Event::Mouse(m) => {
            let mut input = MouseInput::default();
            input.column = m.column;
            input.row = m.row;
            input.modifiers = modifiers(m.modifiers);
            input.kind = match m.kind {
                MouseEventKind::Down(b) => MouseKind::Down(button(b)),
                MouseEventKind::Up(b) => MouseKind::Up(button(b)),
                MouseEventKind::Moved | MouseEventKind::Drag(_) => MouseKind::Move,
                MouseEventKind::ScrollUp => MouseKind::Wheel { x: 0, y: -1 },
                MouseEventKind::ScrollDown => MouseKind::Wheel { x: 0, y: 1 },
                MouseEventKind::ScrollLeft => MouseKind::Wheel { x: -1, y: 0 },
                MouseEventKind::ScrollRight => MouseKind::Wheel { x: 1, y: 0 },
            };
            RunEvent::Input(Input::Mouse(input))
        }
        Event::Key(k) => {
            let key = key_name(&k.code);
            let mut mods = modifiers(k.modifiers);
            if k.code == KeyCode::BackTab {
                mods.shift = true;
            }
            if k.kind == KeyEventKind::Repeat {
                RunEvent::Input(Input::KeyRepeat {
                    key,
                    modifiers: mods,
                })
            } else {
                RunEvent::Input(Input::Key {
                    key,
                    modifiers: mods,
                    release: k.kind == KeyEventKind::Release,
                })
            }
        }
    }
}

struct Guard;

impl Guard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(terminal::EnterAlternateScreen)?;
        out.execute(event::EnableMouseCapture)?;
        out.execute(event::EnableBracketedPaste)?;
        out.execute(event::EnableFocusChange)?;
        Ok(Self)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.execute(event::DisableFocusChange);
        let _ = out.execute(event::DisableBracketedPaste);
        let _ = out.execute(event::DisableMouseCapture);
        let _ = out.execute(terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn is_ctrl_c(ev: &Event) -> bool {
    matches!(ev, Event::Key(k)
        if k.kind == KeyEventKind::Press
            && k.code == KeyCode::Char('c')
            && k.modifiers == KeyModifiers::CONTROL)
}

/// Run the app full-screen in the current terminal until the user quits (Ctrl+C).
///
/// Ctrl+C while a job runs does not abandon it: the job is stopped, the UI shows that it is
/// cleaning up its test data, and the app exits once it has (30 s at most). A second Ctrl+C
/// exits at once.
pub fn run() -> io::Result<()> {
    let mut options = AppOptions::default();
    options.ui_sets = crate::ui::ui_sets();
    if crate::ui::launch().monochrome {
        options.capabilities.colors = ColorDepth::Monochrome;
    }
    let guard = Guard::enter()?;
    // Restore the terminal when the UI thread panics (it is going down). A panic on another
    // thread (an engine task: the job reports it as a failure) must not leave the UI drawing
    // over a cooked terminal, nor print over the UI: it goes to the log file instead.
    let ui_thread = std::thread::current().id();
    let prev: std::sync::Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync> =
        std::panic::take_hook().into();
    let hook_prev = prev.clone();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            let _ = terminal::disable_raw_mode();
            let _ = io::stdout().execute(terminal::LeaveAlternateScreen);
            hook_prev(info);
        } else {
            let name = std::thread::current().name().unwrap_or("?").to_owned();
            crate::ui::log_fatal(&format!("panic on thread {name}: {info}"));
        }
    }));
    let mut view = AppView::new(options);
    let mut term = Terminal::new(CrosstermBackend::new(io::BufWriter::new(io::stdout())))?;
    term.draw(|f| view.render(f, f.area()))?;
    view.load(Root::of::<crate::ui::App>())
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut stopping: Option<Instant> = None;
    let mut aborted = false;
    let res = run_app(&mut term, &mut view, |timeout: Duration| {
        if let Some(since) = stopping {
            if !bridge::busy() {
                return Ok(RunEvent::Exit);
            }
            if since.elapsed() >= bridge::SHUTDOWN_TIMEOUT {
                bridge::abandon();
                aborted = true;
                return Ok(RunEvent::Exit);
            }
        }
        if !event::poll(timeout)? {
            return Ok(RunEvent::Idle);
        }
        let ev = event::read()?;
        if is_ctrl_c(&ev) {
            if stopping.is_some() {
                // Second Ctrl+C: leave now.
                bridge::abandon();
                aborted = true;
                return Ok(RunEvent::Exit);
            }
            if bridge::busy() {
                bridge::begin_shutdown();
                stopping = Some(Instant::now());
                return Ok(RunEvent::Redraw);
            }
        }
        Ok(convert(ev))
    });
    drop(term);
    drop(guard);
    std::panic::set_hook(Box::new(move |info| prev(info)));
    if aborted && bridge::busy() {
        eprintln!(
            "oprobe: quit before the running job finished cleaning up; leftover test data \
             (oprobe-run-*) can be removed with `oprobe cleanup`"
        );
    } else {
        crate::ui::finish_jobs();
    }
    res
}
