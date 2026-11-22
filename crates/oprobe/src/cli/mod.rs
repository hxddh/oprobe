//! The 3.0 command tree (design §9). Every command returns a typed result
//! ([`crate::output::CommandOutput`]) rendered by [`crate::output::Output`]; failures are
//! classified ([`crate::errors::classify`]) at this boundary and mapped to exit codes.

mod agent;
pub mod args;
mod commands;
pub mod complete;
pub mod convert;
pub mod formats;
pub mod help;
pub mod reference;
#[cfg(test)]
mod replay_tests;
pub(crate) mod self_update;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::OnceLock;

use anyhow::Result;
use clap::CommandFactory;

pub use args::Cli;
use args::*;

use crate::config::Preset;
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::output::{Output, OutputFormat};
use crate::report::style::{self, bold, red, yellow};
use crate::store::{ConfigFile, Paths};

pub use crate::app::run::RunOutput;
pub use crate::app::tune::TuneOutput;

/// Exit code when the SLA is breached, `--fail-on` matched or the compare gate failed.
pub const EXIT_GATE: u8 = exit::GATE;
/// Exit code of a command stopped by Ctrl-C (SIGINT), as shells report it.
pub const EXIT_INTERRUPTED: u8 = exit::INTERRUPTED;

pub use crate::app::Ctx;

/// Run the parsed command line and return the process exit code.
pub async fn main(cli: Cli) -> ExitCode {
    match cli.color {
        ColorChoice::Always => style::set(true),
        ColorChoice::Never => style::set(false),
        ColorChoice::Auto => {}
    }
    let Some(command) = cli.command else {
        return onboarding_without_tty(&cli);
    };
    let paths = Paths::resolve(cli.config.as_deref());
    let mut format = cli.output.map(OutputFormat::from).unwrap_or_else(|| {
        ConfigFile::load(&paths.config_file)
            .ok()
            .and_then(|c| c.defaults.output)
            .and_then(|o| OutputFormat::parse(&o))
            .unwrap_or_default()
    });
    // A default (`OPROBE_OUTPUT`, `[defaults] output`) the command cannot render is text; an
    // explicit `-o` was checked by `parse_cli`.
    if !command_formats().contains(&format) {
        format = OutputFormat::Text;
    }
    let non_interactive = cli.non_interactive || !crate::output::interactive_terminal();
    let mut ctx = Ctx::new(Output::new(format, cli.quiet, non_interactive), paths);
    if let Some(o) = cli.output.map(OutputFormat::from)
        && std::env::var_os("OPROBE_OUTPUT").is_none()
    {
        ctx.flags
            .insert("output".into(), format!("{o:?}").to_ascii_lowercase());
    }
    if cli.color != ColorChoice::Auto {
        ctx.flags.insert(
            "color".into(),
            format!("{:?}", cli.color).to_ascii_lowercase(),
        );
    }
    let scheme_hint = scheme_of(&command);
    // An explicit `--config` that does not exist is a typo, except for the commands that
    // create the file.
    if let Some(p) = &cli.config
        && !p.exists()
        && !matches!(
            command,
            Command::Target(TargetCommand::Add(_))
                | Command::Config(ConfigCommand::Path | ConfigCommand::Edit)
        )
    {
        let body = crate::errors::classify(
            &OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("config file `{}` does not exist", p.display()),
            )
            .with_hint(format!(
                "check the path, create it with `oprobe --config {} target add NAME URL`, or drop --config",
                p.display()
            ))
            .into(),
            "s3",
        );
        return ExitCode::from(ctx.out.error(&body));
    }
    let code = match dispatch(command, &ctx).await {
        Ok(code) => code,
        Err(err) if crate::output::is_broken_pipe(&err) => crate::output::EXIT_BROKEN_PIPE,
        Err(err) => {
            let mut body = crate::errors::classify(&err, &scheme_hint);
            if INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
                && body.code.exit_code() == exit::RUNTIME
            {
                body.code = ErrorCode::RunInterrupted;
            }
            ctx.out.error(&body)
        }
    };
    ExitCode::from(code)
}

/// Set by the first Ctrl-C of `run` / `tune`.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn scheme_of(cmd: &Command) -> String {
    let t = match cmd {
        Command::Run(a) => a.target.target.clone(),
        Command::Tune(a) => a.target.target.clone(),
        Command::Top(a) | Command::Gui(a) => a.target.clone(),
        Command::Doctor(a) => a.target.target.clone(),
        Command::Watch(a) => a.target.target.clone(),
        Command::Matrix(a) => a.target.target.clone(),
        Command::Cleanup(a) => a.target.target.clone(),
        Command::Target(TargetCommand::Sample(a)) => a.target.target.clone(),
        _ => None,
    };
    t.and_then(|u| u.split_once("://").map(|(s, _)| s.to_owned()))
        .unwrap_or_else(|| "s3".into())
}

/// Print an error chain with a hint on stderr (interactive surfaces before the CLI runtime).
pub fn print_error(err: &anyhow::Error, scheme: &str) {
    let body = crate::errors::classify(err, scheme);
    eprintln!("{} {}", red(&bold("error:")), body.message);
    if let Some(h) = &body.hint {
        eprintln!("{} {h}", yellow(&bold("hint:")));
    }
}

/// The command line, minus what a desktop launcher adds: macOS LaunchServices passes
/// `-psn_<serial>` to apps opened from Finder on some versions. A bare `oprobe` (what
/// `oprobe.app` runs) must reach [`ui_entry`] and open the workbench, never a usage error.
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

/// Parse the command line. Help and `--version` behave as clap's (exit 0); an argument error
/// is a `usage.invalid_argument` error (exit 2): JSON on stdout when `-o json|ndjson` (or
/// `OPROBE_OUTPUT`) asks for machine output, clap's text on stderr otherwise.
pub fn parse_cli(args: Vec<std::ffi::OsString>) -> std::result::Result<Cli, ExitCode> {
    use clap::FromArgMatches;
    use clap::error::ErrorKind;
    // `--help` shows the common flags, `--help-all` every flag (design v5 §6).
    let all = help::wants_help_all(&args);
    let args = if all {
        help::help_all_args(&args)
    } else {
        args
    };
    let parsed = help::command(all)
        .try_get_matches_from(&args)
        .and_then(|m| Cli::from_arg_matches(&m));
    let e = match parsed {
        Ok(cli) => {
            crate::i18n::set_lang(crate::i18n::detect(cli.lang.as_deref()));
            return check_output_format(cli, &args);
        }
        Err(e) => e,
    };
    let machine = machine_output(&args);
    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) || machine.is_none() {
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
    let out = Output::new(machine.unwrap_or_default(), true, true);
    Err(ExitCode::from(out.error(&body)))
}

/// The command path of this process (`runs show`), set by [`parse_cli`].
static COMMAND_PATH: OnceLock<String> = OnceLock::new();

/// The formats the parsed command offers.
fn command_formats() -> &'static [OutputFormat] {
    match COMMAND_PATH.get() {
        Some(p) => formats::formats_of(p),
        None => crate::output::WITH_MD_HTML,
    }
}

/// Refuse an explicit `-o` the command cannot render (design v4 §8): a usage error before
/// anything runs. `OPROBE_OUTPUT` and the config's default fall back to text instead.
fn check_output_format(
    cli: Cli,
    args: &[std::ffi::OsString],
) -> std::result::Result<Cli, ExitCode> {
    let path = Cli::command()
        .try_get_matches_from(args)
        .map(|m| formats::command_path(&m))
        .unwrap_or_default();
    let path = match path.as_str() {
        "tui" => "top".to_owned(),
        "app" => "gui".to_owned(),
        "serve-s3" => "lab".to_owned(),
        _ => path,
    };
    let _ = COMMAND_PATH.set(path.clone());
    let Some(given) = formats::explicit_output(args).and_then(|v| OutputFormat::parse(&v)) else {
        return Ok(cli);
    };
    let offered = formats::formats_of(&path);
    if offered.contains(&given) || cli.command.is_none() {
        return Ok(cli);
    }
    let name = formats::names(&[given])[0];
    let label = match given {
        OutputFormat::Text => "text",
        OutputFormat::Json => "JSON",
        OutputFormat::Ndjson => "NDJSON",
        OutputFormat::Md => "Markdown",
        OutputFormat::Html => "HTML",
    };
    let mut choices: Vec<String> = formats::names(offered)
        .iter()
        .map(|f| format!("-o {f}"))
        .collect();
    let last = choices.pop().unwrap_or_default();
    let hint = if choices.is_empty() {
        format!("use {last} (or no -o)")
    } else {
        format!("use {} or {last}", choices.join(", "))
    };
    let err: anyhow::Error = OprobeError::new(
        ErrorCode::UsageInvalidArgument,
        format!("`oprobe {path}` has no {label} output (-o {name})"),
    )
    .with_hint(hint)
    .into();
    let body = crate::errors::classify(&err, "s3");
    // The error itself as JSON when JSON was asked for and the command offers none (man,
    // completions), so a script still gets a parseable answer.
    let out = Output::new(
        if given.is_machine() {
            given
        } else {
            OutputFormat::Text
        },
        true,
        true,
    );
    Err(ExitCode::from(out.error(&body)))
}

/// `-o json|ndjson` on the raw command line, else `OPROBE_OUTPUT`, when it is a machine format.
fn machine_output(args: &[std::ffi::OsString]) -> Option<OutputFormat> {
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
    }?;
    f.is_machine().then_some(f)
}

/// Exit code when the environment cannot run the requested interface (no terminal, no display).
pub const EXIT_USAGE: u8 = 2;

/// Interactive commands run on the main thread, before (and without) the CLI's async runtime:
/// native toolkits require the main thread, and the UI runs the engine on a runtime of its own.
/// Returns `None` for every other command.
pub fn ui_entry(cli: &Cli) -> Option<ExitCode> {
    #[cfg(feature = "tui")]
    {
        let (args, window) = match &cli.command {
            Some(Command::Top(a)) => (Some(a), false),
            Some(Command::Gui(a)) => (Some(a), true),
            None => (None, gui_available().is_ok()),
            Some(_) => return None,
        };
        let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if cli.command.is_none() && !window && !interactive {
            return Some(onboarding_without_tty(cli));
        }
        let monochrome = match cli.color {
            ColorChoice::Always => false,
            ColorChoice::Never => true,
            ColorChoice::Auto => std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
        };
        let mut launch = crate::ui::Launch {
            monochrome,
            ..Default::default()
        };
        if let Some(a) = args {
            launch = crate::ui::Launch {
                target: a.target.clone(),
                endpoint: a.endpoint.clone(),
                region: a.region.clone(),
                preset: a.preset.clone(),
                duration: a.duration.clone(),
                concurrency: a.concurrency.map(|c| c.to_string()),
                ca_cert: a.ca_cert.clone(),
                insecure: a.insecure,
                proxy: a.proxy.clone(),
                anonymous: a.anonymous,
                monochrome,
                open: a.open.clone(),
                ..Default::default()
            };
        }
        crate::ui::set_launch(launch);
        if window {
            return Some(run_window(cli));
        }
        if !interactive {
            return Some(ui_refusal(
                cli,
                "`oprobe top` needs an interactive terminal",
                "use `oprobe run` for scripts (or `oprobe gui` for a window)",
            ));
        }
        Some(match crate::ui::run_tui() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                print_error(&anyhow::Error::new(e).context("terminal UI failed"), "s3");
                ExitCode::FAILURE
            }
        })
    }
    #[cfg(not(feature = "tui"))]
    {
        match &cli.command {
            Some(Command::Top(_)) | Some(Command::Gui(_)) => Some(ui_refusal(
                cli,
                "this oprobe was built without its interactive UI",
                "reinstall with `cargo install oprobe` (default features) or use `oprobe run`",
            )),
            None => Some(onboarding_without_tty(cli)),
            Some(_) => None,
        }
    }
}

/// `oprobe` without a command off a terminal (scripts, agents; AG-9): the help, then the
/// credential sources found on this machine and what to run with them — the same discovery
/// as `oprobe target discover`. With `-o json` only the discovery result is printed.
fn onboarding_without_tty(cli: &Cli) -> ExitCode {
    let format = cli
        .output_format()
        .filter(|f| {
            matches!(
                f,
                OutputFormat::Text | OutputFormat::Json | OutputFormat::Ndjson
            )
        })
        .unwrap_or_default();
    let paths = Paths::resolve(cli.config.as_deref());
    let ctx = Ctx::new(Output::new(format, cli.quiet, true), paths);
    if format == OutputFormat::Text {
        let _ = Cli::command().print_help();
        println!("\n");
    }
    let code = match crate::app::discover::discover(&ctx) {
        Ok(d) => ctx.out.finish(&d).unwrap_or(exit::RUNTIME),
        Err(e) => ctx.out.error(&crate::errors::classify(&e, "s3")),
    };
    if format == OutputFormat::Text {
        println!(
            "\nnext: `oprobe demo` (local lab, 30 s) · `oprobe target add NAME URL` · `oprobe run lab://demo -o json`"
        );
    }
    ExitCode::from(code)
}

/// Whether a native window can be shown: built with `gui`, and a display is present.
/// The error says why not.
#[cfg(feature = "tui")]
fn gui_available() -> std::result::Result<(), &'static str> {
    if !cfg!(feature = "gui") {
        return Err("this oprobe was built without native windows");
    }
    let ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if cfg!(target_os = "linux") {
        let display = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
            || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
        return if display {
            Ok(())
        } else if ssh {
            Err("no display in this SSH session (DISPLAY / WAYLAND_DISPLAY are not set)")
        } else {
            Err("no display (DISPLAY / WAYLAND_DISPLAY are not set)")
        };
    }
    // macOS / Windows: a remote shell has no session to draw on.
    if ssh {
        Err("no desktop session to draw on in an SSH session")
    } else {
        Ok(())
    }
}

/// An interactive command this environment cannot run: a `usage.invalid_argument` error
/// (exit 2), as JSON on stdout under `-o json|ndjson`.
fn ui_refusal(cli: &Cli, message: &str, hint: &str) -> ExitCode {
    let format = cli
        .output_format()
        .or_else(|| {
            std::env::var("OPROBE_OUTPUT")
                .ok()
                .and_then(|v| OutputFormat::parse(&v))
        })
        .unwrap_or_default();
    let body = crate::errors::classify(
        &OprobeError::new(ErrorCode::UsageInvalidArgument, message)
            .with_hint(hint)
            .into(),
        "s3",
    );
    ExitCode::from(Output::new(format, true, true).error(&body))
}

#[cfg(feature = "tui")]
fn run_window(cli: &Cli) -> ExitCode {
    #[cfg(feature = "gui")]
    {
        let _ = cli;
        if let Err(why) = gui_available() {
            let msg = format!(
                "cannot open a window: {why}. Use `oprobe top` for the same workbench in this \
                 terminal."
            );
            eprintln!("{} {msg}", red(&bold("error:")));
            crate::ui::log_fatal(&msg);
            return ExitCode::from(EXIT_USAGE);
        }
        detach_console();
        crate::ui::run_gui()
    }
    #[cfg(not(feature = "gui"))]
    {
        ui_refusal(
            cli,
            "this oprobe was built without native windows",
            "use `oprobe top` for the same workbench in this terminal, or install a desktop build \
             (release page, or `cargo install oprobe --features gui`)",
        )
    }
}

/// When started from Explorer, Windows gives a console program its own console window; hide it
/// for the GUI. A console shared with a shell (more than one attached process) is left alone.
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

async fn dispatch(cmd: Command, ctx: &Ctx) -> Result<u8> {
    use commands as c;
    match cmd {
        Command::Run(a) => c::run(a, ctx).await,
        Command::Tune(a) => c::tune(a, ctx).await,
        Command::Verify(a) => c::verify(a, ctx).await,
        Command::Fix(a) => c::fix(a, ctx).await,
        Command::Compare(a) => c::compare(a, ctx),
        Command::Runs(r) => match r {
            RunsCommand::List(a) => ctx.out.finish(&ctx.app().runs_list(a.into())?),
            RunsCommand::Show(a) => ctx.out.finish(&ctx.app().runs_show(a.into())?),
            RunsCommand::Rm(a) => ctx.out.finish(&ctx.app().runs_rm(a.into())?),
            RunsCommand::Gc(a) => ctx.out.finish(&ctx.app().runs_gc(a.into())?),
            RunsCommand::Open(a) => c::runs_open(a, ctx),
            RunsCommand::Export(a) => c::export(a, ctx).await,
        },
        Command::Target(t) => match t {
            TargetCommand::Add(a) => c::target_add(a, ctx).await,
            TargetCommand::List => ctx.out.finish(&ctx.app().targets()?),
            TargetCommand::Show(a) => ctx.out.finish(&ctx.app().target_show(&a.name)?),
            TargetCommand::Rm(a) => ctx.out.finish(&ctx.app().target_rm(&a.name)?),
            TargetCommand::Test(a) => ctx
                .out
                .finish(&ctx.app().target_test(a.into(), &ctx.out).await?),
            TargetCommand::Discover => ctx.out.finish(&ctx.app().discover()?),
            TargetCommand::Sample(a) => {
                ctx.out.finish(&ctx.app().sample(a.into(), &ctx.out).await?)
            }
        },
        Command::Config(k) => match k {
            ConfigCommand::Show => ctx.out.finish(&crate::app::settings::config_show(ctx)?),
            ConfigCommand::Path => ctx.out.finish(&crate::app::settings::config_path(ctx)?),
            ConfigCommand::Edit => ctx.out.finish(&crate::app::settings::config_edit(ctx)?),
        },
        Command::Doctor(a) => ctx.out.finish(&ctx.app().doctor(a.into(), &ctx.out).await?),
        Command::Probe(a) => ctx.out.finish(&ctx.app().probe(a.into(), &ctx.out).await?),
        Command::Lab(a) => c::lab(a, ctx).await,
        Command::Cleanup(a) => ctx
            .out
            .finish(&ctx.app().cleanup(a.into(), &ctx.out).await?),
        Command::Init(a) => cmd_init(a),
        Command::Completions {
            shell,
            static_script: true,
        } => {
            clap_complete::generate(shell, &mut Cli::command(), "oprobe", &mut std::io::stdout());
            Ok(exit::OK)
        }
        Command::Completions { shell, .. } => {
            use std::io::Write;
            std::io::stdout().write_all(&complete::registration(shell, "oprobe")?)?;
            Ok(exit::OK)
        }
        Command::Man { markdown: true, .. } => {
            print!("{}", reference::markdown());
            Ok(exit::OK)
        }
        Command::Man { out_dir, .. } => {
            let cmd = Cli::command();
            match out_dir {
                Some(dir) => {
                    std::fs::create_dir_all(&dir)?;
                    clap_mangen::generate_to(cmd, &dir)?;
                }
                None => clap_mangen::Man::new(cmd).render(&mut std::io::stdout())?,
            }
            Ok(exit::OK)
        }
        Command::Top(_) | Command::Gui(_) => {
            unreachable!("interactive commands are handled by ui_entry")
        }
        Command::Demo(a) => c::demo(a.into(), ctx).await,
        Command::Analyze(a) => ctx.out.finish(&ctx.app().analyze(a.into())?),
        Command::Explain(a) => ctx.out.finish(&ctx.app().explain(a.into())?),
        Command::Watch(a) => c::watch(a, ctx).await,
        Command::Matrix(a) => c::matrix(a, ctx).await,
        Command::Compat(a) => c::compat(a, ctx).await,
        Command::Baseline(BaselineCommand::Calibrate(a)) => c::calibrate(a, ctx).await,
        Command::Baseline(b) => match b {
            BaselineCommand::Set(a) => ctx.out.finish(&ctx.app().baseline_set(a.into(), &ctx.out)?),
            BaselineCommand::List => ctx.out.finish(&ctx.app().baselines()?),
            BaselineCommand::Rm(a) => ctx.out.finish(&ctx.app().baseline_rm(&a.name)?),
            BaselineCommand::Show(a) => ctx.out.finish(&ctx.app().baseline_show(&a.name)?),
            BaselineCommand::Calibrate(_) => unreachable!("handled above"),
        },
        Command::Mcp(a) => agent::cmd_mcp(a, ctx).await,
        Command::Schema(a) => agent::cmd_schema(a, ctx),
        Command::Capabilities(a) => agent::cmd_capabilities(a, ctx),
        Command::SelfUpdate(a) => self_update::cmd_self_update(a, ctx).await,
    }
}

/// Install a Ctrl-C handler: first press cancels gracefully (cleanup still runs), second exits.
fn install_ctrl_c(cancel: crate::engine::Cancel) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
            cancel.cancel();
            eprintln!(
                "\n{} stopping… cleaning up test data (press Ctrl-C again to abort immediately)",
                yellow("■")
            );
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!(
                    "{} aborted; leftover data can be removed with `oprobe cleanup`",
                    red("■")
                );
                std::process::exit(130);
            }
        }
    });
}

fn cmd_init(a: InitArgs) -> Result<u8> {
    let preset = Preset::parse(&a.preset)?;
    let text = crate::config::template(
        preset,
        a.target.as_deref().unwrap_or("s3://my-bucket/oprobe"),
    );
    match &a.file {
        Some(p) => {
            if p.exists() {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("`{}` already exists", p.display()),
                )
                .with_hint("pass another --out path, or delete the file first")
                .into());
            }
            crate::report::write_file(p, &text)?;
            eprintln!(
                "{} wrote {} — edit it, then `oprobe run --profile {}`",
                crate::report::style::green("✓"),
                p.display(),
                p.display()
            );
        }
        None => print!("{text}"),
    }
    Ok(exit::OK)
}
