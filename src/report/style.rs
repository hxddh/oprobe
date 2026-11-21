//! Minimal ANSI styling that respects `NO_COLOR`, `CLICOLOR_FORCE` and non-TTY output.

use std::io::IsTerminal;
use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Decide once whether to colorize stdout.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return false;
        }
        if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
            return true;
        }
        std::io::stdout().is_terminal() && ansi_ok()
    })
}

#[cfg(windows)]
fn ansi_ok() -> bool {
    // Enable virtual-terminal processing (ANSI colors) on Windows 10+ consoles.
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };
    // SAFETY: console-mode calls on this process's stdout handle with a valid out-pointer.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if h.is_null() || h as isize == -1 || GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn ansi_ok() -> bool {
    std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// Force colors on/off (from `--color`).
pub fn set(on: bool) {
    let _ = ENABLED.set(on);
}

fn wrap(s: &str, code: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn bold(s: &str) -> String {
    wrap(s, "1")
}
pub fn dim(s: &str) -> String {
    wrap(s, "2")
}
pub fn red(s: &str) -> String {
    wrap(s, "31")
}
pub fn green(s: &str) -> String {
    wrap(s, "32")
}
pub fn yellow(s: &str) -> String {
    wrap(s, "33")
}
pub fn blue(s: &str) -> String {
    wrap(s, "34")
}
pub fn magenta(s: &str) -> String {
    wrap(s, "35")
}
pub fn cyan(s: &str) -> String {
    wrap(s, "36")
}
pub fn badge(s: &str, code: &str) -> String {
    if enabled() {
        format!("\x1b[1;{code}m {s} \x1b[0m")
    } else {
        format!("[{s}]")
    }
}

/// Horizontal bar of `width` cells for `frac` in 0..=1 (eighth-block resolution).
pub fn bar(frac: f64, width: usize) -> String {
    const PARTS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let cells = (frac.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full = cells / 8;
    let rem = cells % 8;
    let mut s = "█".repeat(full);
    if rem > 0 {
        s.push(PARTS[rem - 1]);
    }
    let len = s.chars().count();
    s.push_str(&" ".repeat(width.saturating_sub(len)));
    s
}

/// `s` without ANSI color sequences.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c == 'm' {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// Visible width of a string ignoring ANSI escape sequences.
pub fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c == 'm' {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            w += 1;
        }
    }
    w
}

pub fn pad(s: &str, width: usize) -> String {
    let w = visible_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

pub fn rpad(s: &str, width: usize) -> String {
    let w = visible_width(s);
    format!("{}{s}", " ".repeat(width.saturating_sub(w)))
}

/// Simple aligned table: first column left-aligned, the rest right-aligned.
pub fn table(header: &[&str], rows: &[Vec<String>], indent: usize) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(visible_width(c));
        }
    }
    let ind = " ".repeat(indent);
    let mut out = String::new();
    let line = |cells: Vec<String>| -> String {
        let mut l = ind.clone();
        for (i, c) in cells.iter().enumerate() {
            if i == 0 {
                l.push_str(&pad(c, widths[i]));
            } else {
                l.push_str("  ");
                l.push_str(&rpad(c, widths[i]));
            }
        }
        l.trim_end().to_owned()
    };
    out.push_str(&dim(&line(
        header.iter().map(|h| (*h).to_owned()).collect(),
    )));
    out.push('\n');
    for r in rows {
        out.push_str(&line(r.clone()));
        out.push('\n');
    }
    out
}
