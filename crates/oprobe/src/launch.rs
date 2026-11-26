//! The bare `oprobe` (design v6 §8): the app. A native window where there is a desktop to draw
//! on (feature `gui`), else the app in this terminal (over SSH too). Off a terminal and off a
//! desktop (a script, a pipe) it prints the help instead; `-o json` asks for machine output,
//! which the app has none of (a usage error).
//!
//! A desktop launch has no terminal either: an app bundle opened from Finder (stdin is not a
//! TTY; LaunchServices may add `-psn_…`, see [`crate::cli::launch_args`]) and a program started
//! from Explorer (its own console) open the window.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::cli::Cli;
use crate::output::OutputFormat;

/// Start the app for a bare `oprobe` (with `file`: a run file to open, see
/// [`crate::cli::launch_file`]); returns the process exit code.
pub fn bare(cli: &Cli, file: Option<PathBuf>) -> ExitCode {
    if cli.output_format() == Some(OutputFormat::Json) {
        return refuse(
            cli,
            "the app has no machine output",
            "use the commands with -o json (`oprobe --help`)",
        );
    }
    // Where the app cannot open, the file can still be read here.
    let show = file
        .as_ref()
        .map(|f| {
            format!(
                "\nto read the run file here: oprobe show {}",
                crate::model::spec::shell_quote(&f.to_string_lossy())
            )
        })
        .unwrap_or_default();
    if cli.non_interactive {
        return help(&format!(
            "the app needs an interactive terminal or a desktop{show}"
        ));
    }
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    open(cli, terminal, file, &show)
}

#[cfg(not(feature = "app"))]
fn open(_: &Cli, _: bool, _: Option<PathBuf>, show: &str) -> ExitCode {
    help(&format!(
        "this oprobe was built without its app; use the commands{show}"
    ))
}

#[cfg(feature = "app")]
fn open(cli: &Cli, terminal: bool, file: Option<PathBuf>, show: &str) -> ExitCode {
    if let Some(f) = file {
        crate::ui::set_launch(crate::ui::Launch {
            paths: None,
            open: Some(f.to_string_lossy().into_owned()),
            stop_grace: None,
        });
    }
    let window = desktop(terminal);
    #[cfg(feature = "gui")]
    if window.is_ok() {
        detach_console();
        return crate::ui::run_gui();
    }
    if !terminal {
        let why = window.err().unwrap_or_default();
        return help(&format!(
            "the app opens when `oprobe` runs in a terminal or on a desktop (no window: {why}){show}"
        ));
    }
    let monochrome = match cli.color {
        crate::cli::args::ColorChoice::Always => false,
        crate::cli::args::ColorChoice::Never => true,
        crate::cli::args::ColorChoice::Auto => {
            std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        }
    };
    match crate::ui::run_tui(monochrome) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            crate::cli::print_error(&anyhow::Error::new(e).context("the app failed"), "s3");
            ExitCode::FAILURE
        }
    }
}

/// Whether a native window can open here: built with `gui`, a display, and either a terminal
/// (the user typed `oprobe`) or a desktop launch. The error says why not.
pub fn desktop(terminal: bool) -> Result<(), &'static str> {
    if !cfg!(feature = "gui") {
        return Err("built without native windows");
    }
    let ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if ssh {
        return Err("an SSH session has no desktop to draw on");
    }
    if cfg!(target_os = "linux") {
        let set = |k| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        if !(set("WAYLAND_DISPLAY") || set("DISPLAY")) {
            return Err("no display (DISPLAY / WAYLAND_DISPLAY are not set)");
        }
    }
    if terminal || desktop_launch() {
        Ok(())
    } else {
        Err("neither a terminal nor a desktop launch")
    }
}

/// Started by the desktop rather than a shell: from an app bundle (macOS), with a console of
/// its own (Windows Explorer), or by a desktop entry (Linux, `DESKTOP_STARTUP_ID` /
/// `XDG_ACTIVATION_TOKEN`).
fn desktop_launch() -> bool {
    if cfg!(target_os = "macos") {
        return std::env::current_exe()
            .is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::GetConsoleProcessList;
        let mut ids = [0u32; 2];
        // SAFETY: the buffer is valid for two process ids.
        return unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) } <= 1;
    }
    #[allow(unreachable_code)]
    {
        const KEYS: [&str; 2] = ["DESKTOP_STARTUP_ID", "XDG_ACTIVATION_TOKEN"];
        // GTK 4 stashes and unsets both before `main` (a library constructor, so that child
        // processes do not inherit them): read the environment the process started with too.
        KEYS.iter()
            .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
            || std::fs::read("/proc/self/environ").is_ok_and(|env| started_with(&env, &KEYS))
    }
}

/// Whether a `/proc/PID/environ` block (`KEY=VALUE` entries, NUL-separated) sets any of
/// `keys` to a non-empty value.
fn started_with(environ: &[u8], keys: &[&str]) -> bool {
    environ.split(|b| *b == 0).any(|e| {
        keys.iter().any(|k| {
            e.strip_prefix(k.as_bytes())
                .and_then(|rest| rest.strip_prefix(b"="))
                .is_some_and(|v| !v.is_empty())
        })
    })
}

/// Started from Explorer, Windows gives a console program its own console window: hide it for
/// the window. A console shared with a shell is left alone.
#[cfg(feature = "gui")]
fn detach_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
        let mut ids = [0u32; 2];
        // SAFETY: the buffer is valid for two process ids.
        unsafe {
            if GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) == 1 {
                FreeConsole();
            }
        }
    }
}

/// The help, then why the app did not open (exit 0: nothing went wrong).
fn help(why: &str) -> ExitCode {
    use clap::CommandFactory;
    let _ = Cli::command().print_help();
    println!("\n\n{why}");
    ExitCode::SUCCESS
}

/// A `usage.invalid_argument` error (exit 2), as JSON under `-o json`.
fn refuse(cli: &Cli, message: &str, hint: &str) -> ExitCode {
    use crate::errors::{ErrorCode, OprobeError};
    let format = cli.output_format().unwrap_or_default();
    let body = crate::errors::classify(
        &OprobeError::new(ErrorCode::UsageInvalidArgument, message)
            .with_hint(hint)
            .into(),
        "s3",
    );
    ExitCode::from(crate::output::Output::new(format, true, true).error(&body))
}

#[cfg(test)]
mod tests {
    use super::started_with;

    #[test]
    fn a_desktop_launch_is_read_from_the_starting_environment() {
        let keys = ["DESKTOP_STARTUP_ID", "XDG_ACTIVATION_TOKEN"];
        assert!(started_with(
            b"HOME=/root\0XDG_ACTIVATION_TOKEN=abc\0",
            &keys
        ));
        assert!(!started_with(b"XDG_ACTIVATION_TOKEN=\0PATH=/bin\0", &keys));
        assert!(!started_with(b"DESKTOP_STARTUP_ID_X=1\0", &keys));
        assert!(!started_with(b"", &keys));
    }
}
