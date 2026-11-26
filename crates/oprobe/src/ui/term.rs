//! The app on the terminal grid (`nsh-platform-tui`): a bare `oprobe` without a display, over
//! SSH too. Event conversion follows NSH's own terminal host.

use std::io::{self, Write as _};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::{ExecutableCommand, terminal};
use nsh_core::clipboard::ClipboardBackend;
use nsh_core::element::Root;
use nsh_platform_tui::{
    AppOptions, AppView, ColorDepth, Input, Modifiers, MouseButton, MouseInput, MouseKind,
    RunEvent, run_app,
};
use ratatui_core::terminal::Terminal;
use ratatui_crossterm::CrosstermBackend;

use crate::ui::jobs;

/// Terminal clipboard backend: OSC-52 (works over SSH and inside tmux) + OS clipboard tools
/// (`pbcopy`/`pbpaste`, `wl-copy`/`wl-paste`, `xclip`, `xsel`, `clip.exe`) + an in-memory
/// fallback so copy/paste inside the TUI always succeeds.
#[derive(Default)]
struct TermClipboard {
    mem: Mutex<Option<String>>,
}

const MAX_OSC52_BYTES: usize = 100_000;

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn emit_osc52(text: &str) {
    if text.len() > MAX_OSC52_BYTES {
        return;
    }
    let b64 = base64_encode(text.as_bytes());
    let seq = if std::env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{b64}\x07")
    };
    #[cfg(unix)]
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(seq.as_bytes());
        let _ = tty.flush();
        return;
    }
    let _ = io::stderr().write_all(seq.as_bytes());
    let _ = io::stderr().flush();
}

fn os_clipboard_set(text: &str) {
    use std::process::{Command, Stdio};
    let is_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if is_ssh {
        return;
    }
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    for (bin, args) in candidates {
        if let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return;
            }
        }
    }
}

fn os_clipboard_get() -> Option<String> {
    use std::process::{Command, Stdio};
    let is_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if is_ssh {
        return None;
    }
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbpaste", &[])]
    } else if cfg!(windows) {
        return None;
    } else {
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    };
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            && out.status.success()
            && let Ok(s) = String::from_utf8(out.stdout)
        {
            return Some(s);
        }
    }
    None
}

impl ClipboardBackend for TermClipboard {
    fn text(&self) -> Option<String> {
        os_clipboard_get().or_else(|| self.mem.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    fn set_text(&self, text: Option<String>) {
        if let Some(ref s) = text {
            emit_osc52(s);
            os_clipboard_set(s);
        }
        *self.mem.lock().unwrap_or_else(|e| e.into_inner()) = text;
    }
}

fn modifiers(keys: KeyModifiers) -> Modifiers {
    Modifiers {
        ctrl: keys.contains(KeyModifiers::CONTROL),
        alt: keys.contains(KeyModifiers::ALT),
        shift: keys.contains(KeyModifiers::SHIFT),
        cmd: keys.contains(KeyModifiers::SUPER),
        alt_graph: false,
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

/// A crossterm event as the surface's input.
fn convert(ev: Event) -> RunEvent {
    match ev {
        Event::Resize(..) => RunEvent::Redraw,
        Event::FocusGained => RunEvent::Input(Input::Focus(true)),
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

/// Run the app full-screen in this terminal until Ctrl+C. Ctrl+C while a job runs stops it
/// and waits for its cleanup (`STOP_GRACE` at most, then says what was left); a second Ctrl+C
/// leaves at once.
pub fn run(monochrome: bool) -> io::Result<()> {
    let mut options = AppOptions::default();
    options.ui_sets = crate::ui::ui_sets();
    options.clipboard = Some(std::sync::Arc::new(TermClipboard::default()));
    if monochrome {
        options.capabilities.colors = ColorDepth::Monochrome;
    }
    let guard = Guard::enter()?;
    // A panic of the UI thread restores the terminal first; one of an engine thread goes to
    // the log file instead of over the screen.
    let ui_thread = std::thread::current().id();
    let prev: std::sync::Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync> =
        std::panic::take_hook().into();
    let hook_prev = prev.clone();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            let mut out = io::stdout();
            let _ = out.execute(event::DisableFocusChange);
            let _ = out.execute(event::DisableBracketedPaste);
            let _ = out.execute(event::DisableMouseCapture);
            let _ = out.execute(terminal::LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
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
    // A second Ctrl+C: leave without waiting any longer.
    let mut hurried = false;
    let res = run_app(&mut term, &mut view, |timeout: Duration| {
        if let Some(since) = stopping
            && (!jobs::busy() || since.elapsed() >= crate::app::STOP_GRACE)
        {
            return Ok(RunEvent::Exit);
        }
        if !event::poll(timeout)? {
            return Ok(RunEvent::Idle);
        }
        let ev = event::read()?;
        if is_ctrl_c(&ev) {
            if stopping.is_some() || !jobs::busy() {
                hurried = stopping.is_some();
                return Ok(RunEvent::Exit);
            }
            jobs::stop_all();
            stopping = Some(Instant::now());
            return Ok(RunEvent::Redraw);
        }
        Ok(convert(ev))
    });
    drop(term);
    drop(guard);
    std::panic::set_hook(Box::new(move |info| prev(info)));
    // What is left of the wait Ctrl+C started (none of it after a second Ctrl+C); what a job
    // then left behind is said on the restored terminal.
    let waited = match stopping {
        Some(_) if hurried => crate::app::STOP_GRACE,
        Some(since) => since.elapsed(),
        None => Duration::ZERO,
    };
    crate::ui::store::finish_jobs(crate::app::STOP_GRACE.saturating_sub(waited));
    res
}
