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

/// Columns a character takes in a terminal: 2 for East Asian wide and full-width characters
/// (CJK ideographs, kana, Hangul, full-width forms), else 1.
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// Terminal columns of `s`, ignoring ANSI escapes and counting wide characters as 2.
pub fn display_width(s: &str) -> usize {
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
            w += char_width(c);
        }
    }
    w
}

/// Width for text output on stdout (design v5 §6): `COLUMNS` when set, else the terminal's
/// width when stdout is a terminal, else 100 columns. At least 40.
pub fn term_width() -> usize {
    if let Some(c) = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|c| *c > 0)
    {
        return c.max(40);
    }
    if std::io::stdout().is_terminal()
        && let Some(w) = tty_columns()
    {
        return w.max(40);
    }
    100
}

#[cfg(unix)]
fn tty_columns() -> Option<usize> {
    // SAFETY: TIOCGWINSZ fills a `winsize` for a terminal file descriptor; stdout is checked to
    // be a terminal and the struct is zero-initialised.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        (libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0)
            .then_some(ws.ws_col as usize)
    }
}

#[cfg(windows)]
fn tty_columns() -> Option<usize> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    };
    // SAFETY: console query on this process's stdout handle with a valid, zeroed out-struct.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if h.is_null() || GetConsoleScreenBufferInfo(h, &mut info) == 0 {
            return None;
        }
        let w = (info.srWindow.Right - info.srWindow.Left + 1) as i32;
        (w > 0).then_some(w as usize)
    }
}

#[cfg(not(any(unix, windows)))]
fn tty_columns() -> Option<usize> {
    None
}

/// Word-wrap plain text to `width` columns: the first line starts with `first`, the others with
/// `rest` (indents count toward the width). Words longer than a line are broken; wide (CJK)
/// characters may break anywhere. Never drops text.
pub fn wrap_text(s: &str, width: usize, first: &str, rest: &str) -> Vec<String> {
    // Units: a run of narrow non-space characters, or one wide character, each with whether a
    // space separated it from the previous unit.
    let mut units: Vec<(String, bool)> = Vec::new();
    let (mut space, mut cur, mut cur_space) = (false, String::new(), false);
    for c in s.chars() {
        if c.is_whitespace() {
            if !cur.is_empty() {
                units.push((std::mem::take(&mut cur), cur_space));
            }
            space = true;
        } else if char_width(c) == 2 {
            if !cur.is_empty() {
                units.push((std::mem::take(&mut cur), cur_space));
            }
            units.push((c.to_string(), space));
            space = false;
        } else {
            if cur.is_empty() {
                cur_space = space;
                space = false;
            }
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        units.push((cur, cur_space));
    }
    let mut lines = Vec::new();
    let mut line = first.to_owned();
    let mut empty = true;
    for (u, sp) in units {
        let room = width.saturating_sub(display_width(&line));
        let sep = sp && !empty;
        if display_width(&u) + usize::from(sep) <= room {
            if sep {
                line.push(' ');
            }
            line.push_str(&u);
            empty = false;
            continue;
        }
        if !empty {
            lines.push(std::mem::take(&mut line));
            line = rest.to_owned();
        }
        // The unit starts a line; break it when it is longer than a line.
        for c in u.chars() {
            if display_width(&line) + char_width(c) > width
                && display_width(&line) > display_width(rest)
            {
                lines.push(std::mem::take(&mut line));
                line = rest.to_owned();
            }
            line.push(c);
        }
        empty = false;
    }
    lines.push(line);
    lines
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    #[test]
    fn wraps_words_and_wide_characters_without_losing_text() {
        let l = wrap_text(
            "the quick brown fox jumps over the lazy dog",
            16,
            "  ",
            "    ",
        );
        assert!(l.iter().all(|x| display_width(x) <= 16), "{l:?}");
        assert_eq!(l[0], "  the quick");
        let joined: Vec<&str> = l.iter().flat_map(|x| x.split_whitespace()).collect();
        assert_eq!(
            joined.join(" "),
            "the quick brown fox jumps over the lazy dog"
        );
        let zh = wrap_text("服务对请求限流，降低并发或速率", 12, "", "");
        assert!(zh.iter().all(|x| display_width(x) <= 12), "{zh:?}");
        assert_eq!(zh.concat(), "服务对请求限流，降低并发或速率");
        let long = wrap_text(&"x".repeat(25), 10, "", "");
        assert_eq!(long.concat(), "x".repeat(25));
        assert!(long.iter().all(|x| x.len() <= 10));
        assert_eq!(display_width("a中b"), 4);
        assert_eq!(wrap_text("", 10, "> ", ""), vec!["> ".to_owned()]);
    }
}
