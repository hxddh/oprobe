//! The command line (design v6 §8): `run`, `prove`, `show`, `lab` (and `demo`), plus the bare
//! app. Every command calls the service layer ([`crate::app::App`]) and renders its typed
//! result ([`crate::output::CommandOutput`]); failures are classified
//! ([`crate::errors::classify`]) at this boundary and mapped to exit codes.

pub mod args;
mod commands;
pub mod schema;

use std::process::ExitCode;

use anyhow::Result;

pub use args::Cli;
use args::*;

use crate::app::Ctx;
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::output::{Output, OutputFormat, stderr_line};
use crate::report::style::{self, bold, red, yellow};
use crate::store::{ConfigFile, Paths};

/// Exit code of a command stopped by Ctrl-C (SIGINT), SIGTERM, SIGHUP or a stderr whose
/// reader went away, as shells report an interrupted command.
pub const EXIT_INTERRUPTED: u8 = exit::INTERRUPTED;

/// Run the parsed command line and return the process exit code.
pub async fn main(cli: Cli) -> ExitCode {
    match cli.color {
        ColorChoice::Always => style::set(true),
        ColorChoice::Never => style::set(false),
        ColorChoice::Auto => {}
    }
    let Some(command) = cli.command else {
        return crate::launch::bare(&cli, None);
    };
    let paths = Paths::resolve(cli.config.as_deref());
    let format = cli.output.map(OutputFormat::from).unwrap_or_else(|| {
        ConfigFile::load(&paths.config_file)
            .ok()
            .and_then(|c| c.defaults.output)
            .and_then(|o| OutputFormat::parse(&o))
            .unwrap_or_default()
    });
    let non_interactive = cli.non_interactive || !crate::output::interactive_terminal();
    let ctx = Ctx::new(Output::new(format, cli.quiet, non_interactive), paths);
    // An explicit `--config` that does not exist is a typo.
    if let Some(p) = &cli.config
        && !p.exists()
    {
        let body = crate::errors::classify(
            &OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("config file `{}` does not exist", p.display()),
            )
            .with_hint("check the path, or drop --config")
            .into(),
            "s3",
        );
        return ExitCode::from(ctx.out.error(&body));
    }
    let scheme_hint = scheme_of(&command, &ctx);
    let code = match dispatch(command, &ctx).await {
        Ok(code) => code,
        Err(err) if crate::output::is_broken_pipe(&err) => crate::output::EXIT_BROKEN_PIPE,
        Err(err) => {
            let mut body = crate::errors::classify(&err, &scheme_hint);
            if INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
                && body.code.exit_code() == exit::RUNTIME
            {
                body = body.with_code(ErrorCode::RunInterrupted);
            }
            ctx.out.error(&body)
        }
    };
    ExitCode::from(code)
}

/// Set when a measuring command is told to stop (the first signal, or stderr closing).
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn scheme_of(cmd: &Command, ctx: &Ctx) -> String {
    let scheme_from_url = |u: &str| {
        crate::config::TargetSpec::from_url(u)
            .ok()
            .map(|t| t.scheme)
            .or_else(|| u.split_once("://").map(|(s, _)| s.to_owned()))
    };
    match cmd {
        Command::Run(a) => a
            .operators
            .first()
            .and_then(|op| {
                if let Ok(cfg) = ctx.config()
                    && let Ok((spec, _)) = crate::operator::resolve::resolve(op, &cfg)
                {
                    scheme_from_url(&spec.url)
                } else {
                    scheme_from_url(op)
                }
            })
            .unwrap_or_else(|| "s3".into()),
        Command::Demo(_) => "lab".into(),
        // The sidecar names the service: the run itself is not decoded for an error's hint.
        Command::Prove(a) => ctx
            .store()
            .summary(&a.run)
            .ok()
            .and_then(|s| s.scheme().map(str::to_owned))
            .unwrap_or_else(|| "s3".into()),
        _ => "s3".into(),
    }
}

/// Print an error chain with a hint on stderr (before the CLI runtime exists).
pub fn print_error(err: &anyhow::Error, scheme: &str) {
    let body = crate::errors::classify(err, scheme);
    stderr_line(format_args!("{} {}", red(&bold("error:")), body.message));
    if let Some(h) = &body.hint {
        stderr_line(format_args!("{} {h}", yellow(&bold("hint:"))));
    }
}

/// The command line, minus what a desktop launcher adds: macOS LaunchServices passes
/// `-psn_<serial>` to apps opened from Finder on some versions. A bare `oprobe` must reach
/// the app, never a usage error.
pub fn launch_args<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .enumerate()
        .filter(|(i, a)| *i == 0 || !a.to_string_lossy().starts_with("-psn_"))
        .map(|(_, a)| a)
        .collect()
}

/// A desktop opening a file: `oprobe PATH`, where `PATH` is the only argument and an existing
/// `.oprobe` or `.json` file (a double click, Open With, a file dropped on the program). The
/// bare app opens it; nothing else is a file argument.
pub fn launch_file(args: &[std::ffi::OsString]) -> Option<std::path::PathBuf> {
    let [_, arg] = args else {
        return None;
    };
    let path = std::path::Path::new(arg);
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if !matches!(ext.as_str(), "oprobe" | "json") || !path.is_file() {
        return None;
    }
    Some(std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Parse the command line. Help and `--version` behave as clap's (exit 0); an argument error
/// is a `usage.invalid_argument` error (exit 2): JSON on stdout when `-o json` (or
/// `OPROBE_OUTPUT=json`) asks for it, clap's text on stderr otherwise.
pub fn parse_cli(args: Vec<std::ffi::OsString>) -> std::result::Result<Cli, ExitCode> {
    use clap::Parser;
    use clap::error::ErrorKind;
    let e = match Cli::try_parse_from(&args) {
        Ok(cli) => return Ok(cli),
        Err(e) => e,
    };
    let machine = machine_output(&args);
    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) || !machine {
        let _ = e.print();
        return Err(ExitCode::from(e.exit_code().clamp(0, 255) as u8));
    }
    let rendered = e.render().to_string();
    let message = rendered
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("invalid arguments")
        .trim_start_matches("error:")
        .trim()
        .to_owned();
    let usage = rendered
        .lines()
        .find_map(|l| l.trim().strip_prefix("Usage:"))
        .map(|u| u.trim().to_owned());
    let hint = match usage {
        Some(u) => format!("usage: {u} (see `--help`)"),
        None => "see `oprobe --help`".to_owned(),
    };
    // A value rejected by one of `crate::input`'s parsers keeps its code and hint.
    let typed = std::error::Error::source(&e).and_then(|c| c.downcast_ref::<OprobeError>());
    let (code, hint) = match typed {
        Some(t) => (t.code, t.hint.clone().unwrap_or(hint)),
        None => (ErrorCode::UsageInvalidArgument, hint),
    };
    let err: anyhow::Error = OprobeError::new(code, message).with_hint(hint).into();
    let body = crate::errors::classify(&err, "s3");
    let out = Output::new(OutputFormat::Json, true, true);
    Err(ExitCode::from(out.error(&body)))
}

/// Whether `-o json` is on the raw command line (else `OPROBE_OUTPUT=json`).
fn machine_output(args: &[std::ffi::OsString]) -> bool {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut given = None;
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            break;
        }
        if a == "-o" || a == "--output" {
            given = args.get(i + 1).cloned();
            i += 1;
        } else if let Some(v) = a.strip_prefix("--output=") {
            given = Some(v.to_owned());
        } else if let Some(v) = a.strip_prefix("-o").filter(|v| !v.is_empty()) {
            given = Some(v.trim_start_matches('=').to_owned());
        }
        i += 1;
    }
    let f = match given {
        Some(v) => OutputFormat::parse(&v),
        None => std::env::var("OPROBE_OUTPUT")
            .ok()
            .and_then(|v| OutputFormat::parse(&v)),
    };
    f.is_some_and(OutputFormat::is_machine)
}

async fn dispatch(cmd: Command, ctx: &Ctx) -> Result<u8> {
    use commands as c;
    match cmd {
        Command::Run(a) => c::run(a.operators, a.flags, ctx).await,
        Command::Demo(a) => c::run(vec!["lab://demo".to_owned()], a.flags, ctx).await,
        Command::Prove(a) => c::prove(a, ctx).await,
        Command::Show(a) => c::show(a, ctx),
        Command::Lab(a) => c::lab(a, ctx).await,
    }
}

/// The signals that stop a command: Ctrl-C (SIGINT) everywhere, and on Unix also what a
/// supervisor (SIGTERM: `kill`, a container runtime, CI) and a closing terminal or SSH session
/// (SIGHUP) send. Their default action kills the process on the spot, with its test data and
/// its multipart uploads left on the target.
struct StopSignals {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
}

impl StopSignals {
    /// Handle the signals from now on (a signal that cannot be handled keeps its default).
    fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                terminate: signal(SignalKind::terminate()).ok(),
                hangup: signal(SignalKind::hangup()).ok(),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// The next of them; `false` when none can be received any more.
    async fn next(&mut self) -> bool {
        #[cfg(unix)]
        {
            async fn recv(s: &mut Option<tokio::signal::unix::Signal>) -> Option<()> {
                match s {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            }
            tokio::select! {
                r = tokio::signal::ctrl_c() => r.is_ok(),
                r = recv(&mut self.terminate) => r.is_some(),
                r = recv(&mut self.hangup) => r.is_some(),
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await.is_ok()
    }
}

/// Install the stop handler: the first Ctrl-C (or SIGTERM, SIGHUP) cancels gracefully — what is
/// in flight is cut, multipart uploads are aborted and cleanup still runs — the second exits.
///
/// A stderr whose reader is gone stops the command the same way: the terminal or SSH session
/// closed (with or without a SIGHUP reaching this process), or `2>&1 | head` read enough.
/// Nobody is left to read the progress, and the run must still clean up and be saved.
fn install_ctrl_c(cancel: crate::engine::Cancel) {
    let stop = move || {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        cancel.cancel();
    };
    crate::output::on_stderr_closed(stop.clone());
    let mut signals = StopSignals::install();
    tokio::spawn(async move {
        if signals.next().await {
            stop();
            stderr_line(format_args!(
                "\n{} stopping… cleaning up test data (press Ctrl-C again to abort immediately)",
                yellow("■")
            ));
            if signals.next().await {
                stderr_line(format_args!(
                    "{} aborted; test data may be left under oprobe-run-*/",
                    red("■")
                ));
                std::process::exit(EXIT_INTERRUPTED.into());
            }
        }
    });
}
