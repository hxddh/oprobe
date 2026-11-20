//! The 3.0 command tree (design §9). Every command returns a typed result
//! ([`crate::output::CommandOutput`]) rendered by [`crate::output::Output`]; failures are
//! classified ([`crate::errors::classify`]) at this boundary and mapped to exit codes.

mod agent;
pub(crate) mod analyze;
pub mod args;
pub(crate) mod compare;
pub(crate) mod demo;
pub(crate) mod doctor;
pub(crate) mod export;
mod live;
pub(crate) mod matrix;
mod measure;
pub(crate) mod misc;
mod onboard;
pub mod reference;
pub(crate) mod repeat;
pub(crate) mod run;
pub(crate) mod runs;
pub(crate) mod self_update;
pub(crate) mod settings;
pub(crate) mod watch;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use clap::CommandFactory;

pub use args::Cli;
use args::*;

use crate::config::{Access, DatasetSpec, Preset, Profile, TargetSpec};
use crate::engine::RunOptions;
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::output::{Output, OutputFormat};
use crate::report::style::{self, bold, red, yellow};
use crate::store::{ConfigFile, Paths};
use crate::util::{ByteSize, HumanDuration, parse_duration};

pub use run::{RunOutput, TuneOutput};

/// Exit code when the SLA is breached, `--fail-on` matched or the compare gate failed.
pub const EXIT_GATE: u8 = exit::GATE;
/// Exit code of a command stopped by Ctrl-C (SIGINT), as shells report it.
pub const EXIT_INTERRUPTED: u8 = exit::INTERRUPTED;

/// What every command gets: output, paths and the (lazily loaded) config file.
pub struct Ctx {
    pub out: Output,
    pub paths: Paths,
    /// Global settings given as flags (for `config show` sources).
    pub flags: std::collections::BTreeMap<String, String>,
    config: OnceLock<std::result::Result<ConfigFile, String>>,
}

impl Ctx {
    pub fn new(out: Output, paths: Paths) -> Self {
        Self {
            out,
            paths,
            flags: Default::default(),
            config: OnceLock::new(),
        }
    }

    /// The config file (empty when missing; an invalid file is a `config.invalid` error).
    pub fn config(&self) -> Result<&ConfigFile> {
        self.config
            .get_or_init(|| ConfigFile::load(&self.paths.config_file).map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|m| OprobeError::new(ErrorCode::ConfigInvalid, m.clone()).into())
    }

    /// The run store.
    pub fn store(&self) -> crate::store::RunStore {
        self.paths.run_store()
    }
}

/// Run the parsed command line and return the process exit code.
pub async fn main(cli: Cli) -> ExitCode {
    match cli.color {
        ColorChoice::Always => style::set(true),
        ColorChoice::Never => style::set(false),
        ColorChoice::Auto => {}
    }
    let Some(command) = cli.command else {
        let _ = Cli::command().print_help();
        return ExitCode::SUCCESS;
    };
    let paths = Paths::resolve(cli.config.as_deref());
    let format = cli.output.unwrap_or_else(|| {
        ConfigFile::load(&paths.config_file)
            .ok()
            .and_then(|c| c.defaults.output)
            .and_then(|o| OutputFormat::parse(&o))
            .unwrap_or_default()
    });
    let non_interactive = cli.non_interactive || !crate::output::interactive_terminal();
    let mut ctx = Ctx::new(Output::new(format, cli.quiet, non_interactive), paths);
    if let Some(o) = cli.output
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
    let code = match dispatch(command, &ctx).await {
        Ok(code) => code,
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
            let _ = Cli::command().print_help();
            return Some(ExitCode::SUCCESS);
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
            return Some(run_window());
        }
        if !interactive {
            eprintln!(
                "{} `oprobe top` needs an interactive terminal; use `oprobe run` for scripts \
                 (or `oprobe gui` for a window).",
                red(&bold("error:"))
            );
            return Some(ExitCode::from(EXIT_USAGE));
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
            Some(Command::Top(_)) | Some(Command::Gui(_)) => {
                eprintln!(
                    "{} this oprobe was built without its interactive UI; reinstall with \
                     `cargo install oprobe` (default features) or use `oprobe run`.",
                    red(&bold("error:"))
                );
                Some(ExitCode::FAILURE)
            }
            None => {
                let _ = Cli::command().print_help();
                Some(ExitCode::SUCCESS)
            }
            Some(_) => None,
        }
    }
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

#[cfg(feature = "tui")]
fn run_window() -> ExitCode {
    #[cfg(feature = "gui")]
    {
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
        eprintln!(
            "{} this oprobe was built without native windows; use `oprobe top` for the same \
             workbench in this terminal, or install a desktop build (release page, or \
             `cargo install oprobe --features gui`).",
            red(&bold("error:"))
        );
        ExitCode::FAILURE
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
    let pending = |name: &str| -> Result<u8> { Err(OprobeError::not_implemented(name).into()) };
    match cmd {
        Command::Run(a) => run::cmd_run(a, ctx).await,
        Command::Tune(a) => run::cmd_tune(a, ctx).await,
        Command::Compare(a) => compare::cmd_compare(a, ctx),
        Command::Runs(c) => match c {
            RunsCommand::List(a) => runs::cmd_list(a, ctx),
            RunsCommand::Show(a) => runs::cmd_show(a, ctx),
            RunsCommand::Rm(a) => runs::cmd_rm(a, ctx),
            RunsCommand::Gc(a) => runs::cmd_gc(a, ctx),
            RunsCommand::Open(a) => runs::cmd_open(a, ctx),
            RunsCommand::Export(a) => export::cmd_export(a, ctx).await,
        },
        Command::Target(c) => match c {
            TargetCommand::Add(a) => settings::target_add(a, ctx).await,
            TargetCommand::List => settings::target_list(ctx),
            TargetCommand::Show(a) => settings::target_show(a, ctx),
            TargetCommand::Rm(a) => settings::target_rm(a, ctx),
            TargetCommand::Test(a) => settings::target_test(a, ctx).await,
        },
        Command::Config(c) => match c {
            ConfigCommand::Show => settings::config_show(ctx),
            ConfigCommand::Path => settings::config_path(ctx),
            ConfigCommand::Edit => settings::config_edit(ctx),
        },
        Command::Doctor(a) => doctor::run(a, ctx).await,
        Command::Probe(a) => misc::cmd_probe(a, ctx).await,
        Command::Lab(a) => misc::cmd_lab(a, ctx).await,
        Command::Cleanup(a) => misc::cmd_cleanup(a, ctx).await,
        Command::Init(a) => cmd_init(a),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "oprobe", &mut std::io::stdout());
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
        Command::Demo(a) => demo::cmd_demo(a, ctx).await,
        Command::Analyze(a) => analyze::cmd_analyze(a, ctx),
        Command::Explain(a) => agent::cmd_explain(a, ctx),
        Command::Watch(a) => watch::cmd_watch(a, ctx).await,
        Command::Matrix(a) => matrix::cmd_matrix(a, ctx).await,
        Command::Compat(_) => pending("compat"),
        Command::Baseline(c) => compare::cmd_baseline(c, ctx),
        Command::Mcp(a) => agent::cmd_mcp(a, ctx).await,
        Command::Schema(a) => agent::cmd_schema(a, ctx),
        Command::Capabilities(a) => agent::cmd_capabilities(a, ctx),
        Command::SelfUpdate(a) => self_update::cmd_self_update(a, ctx).await,
    }
}

// ---------------------------------------------------------------------------------------
// Profile assembly
// ---------------------------------------------------------------------------------------

/// Resolve a target *name* (from the config file) into URL arguments; URLs pass through.
/// Flags given on the command line override the target's values. Returns the target name.
pub fn resolve_target(t: &TargetArgs, cfg: &ConfigFile) -> Result<(TargetArgs, Option<String>)> {
    let mut t = t.clone();
    if t.profile.is_some() {
        return Ok((t, None));
    }
    let given = t.target.clone().or_else(|| cfg.defaults.target.clone());
    let Some(given) = given else {
        return Ok((t, None));
    };
    if given.contains("://") {
        t.target = Some(given);
        return Ok((t, None));
    }
    let Some(entry) = cfg.target(&given) else {
        let known: Vec<&str> = cfg.targets.keys().map(String::as_str).collect();
        return Err(OprobeError::new(
            ErrorCode::UsageUnknownTarget,
            format!("unknown target `{given}`"),
        )
        .with_hint(if known.is_empty() {
            "pass a storage URL (s3://bucket/prefix, memory://) or add a target with `oprobe target add NAME URL`".to_owned()
        } else {
            format!("configured targets: {}", known.join(", "))
        })
        .into());
    };
    entry.validate()?;
    t.target = Some(entry.url.clone());
    if t.endpoint.is_none() {
        t.endpoint = entry.endpoint.clone();
    }
    if t.region.is_none() {
        t.region = entry.region.clone();
    }
    let mut opts: Vec<String> = entry
        .options
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    opts.append(&mut t.options);
    t.options = opts;
    if entry.credentials.as_deref() == Some("anonymous") {
        t.anonymous = true;
    }
    // `credentials = "profile:NAME"`: the AWS shared-config profile (S3-compatible targets).
    if let Some(p) = entry
        .credentials
        .as_deref()
        .and_then(|c| c.strip_prefix("profile:"))
        && !t.options.iter().any(|o| o.starts_with("profile="))
        && TargetSpec::from_url(&entry.url).is_ok_and(|s| s.scheme == "s3")
    {
        t.options.insert(0, format!("profile={p}"));
    }
    Ok((t, Some(given)))
}

/// Apply `[defaults]` for workload/knob values not given on the command line.
fn apply_defaults(w: &mut WorkloadArgs, k: &mut KnobArgs, cfg: &ConfigFile) {
    let d = &cfg.defaults;
    if w.preset.is_none() {
        w.preset = d.preset.clone();
    }
    if w.duration.is_none() {
        w.duration = d.duration.clone();
    }
    if k.concurrency.is_none() {
        k.concurrency = d.concurrency;
    }
}

/// [`build_profile`] with configured targets and `[defaults]` applied.
pub fn resolve_profile(
    ctx: &Ctx,
    t: &TargetArgs,
    w: &WorkloadArgs,
    k: &KnobArgs,
    c: &CostArgs,
) -> Result<(Profile, Option<String>)> {
    let cfg = ctx.config()?;
    let (t, name) = resolve_target(t, cfg)?;
    let (mut w, mut k) = (w.clone(), k.clone());
    if t.profile.is_none() {
        apply_defaults(&mut w, &mut k, cfg);
    }
    Ok((build_profile(&t, &w, &k, c)?, name))
}

/// Trace rows kept per run: `$OPROBE_MAX_REQUESTS` > `[defaults] max_requests` > built-in.
pub fn trace_max_requests(ctx: &Ctx) -> usize {
    std::env::var("OPROBE_MAX_REQUESTS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| ctx.config().ok().and_then(|c| c.defaults.max_requests))
        .unwrap_or(crate::trace::DEFAULT_MAX_REQUESTS)
}

fn parse_size(flag: &str, v: &str) -> Result<ByteSize> {
    v.parse().with_context(|| format!("invalid {flag}"))
}

pub fn build_profile(
    t: &TargetArgs,
    w: &WorkloadArgs,
    k: &KnobArgs,
    c: &CostArgs,
) -> Result<Profile> {
    let mut p = match (&t.profile, &t.target) {
        (Some(path), _) => Profile::load(path)?,
        (None, Some(url)) => Profile::from_target(TargetSpec::from_url(url)?),
        (None, None) => {
            return Err(OprobeError::new(ErrorCode::UsageUnknownTarget, "no target given")
                .with_hint(
                    "try `oprobe run memory://` for a local demo, `oprobe run s3://bucket/prefix`, \
                     a configured target name, or `oprobe lab` to start a local S3 server",
                )
                .into());
        }
    };
    if t.profile.is_some()
        && let Some(url) = &t.target
    {
        let mut nt = TargetSpec::from_url(url)?;
        nt.insecure = p.target.insecure;
        nt.ca_cert = p.target.ca_cert.clone();
        nt.proxy = p.target.proxy.clone();
        p.target = nt;
    }
    let tg = &mut p.target;
    if let Some(e) = &t.endpoint {
        tg.options.insert("endpoint".into(), e.clone());
        if tg.scheme == "s3" || tg.url.as_deref().is_some_and(|u| u.starts_with("s3")) {
            tg.options
                .entry("enable_virtual_host_style".into())
                .or_insert_with(|| "false".into());
        }
    }
    if let Some(r) = &t.region {
        tg.options.insert("region".into(), r.clone());
    }
    for kv in &t.options {
        let (key, val) = kv
            .split_once('=')
            .with_context(|| format!("invalid --option `{kv}` (expected KEY=VALUE)"))?;
        tg.options
            .insert(key.trim().to_owned(), val.trim().to_owned());
    }
    if t.anonymous {
        let scheme = tg.service_scheme()?;
        for (k, v) in crate::config::anonymous_options(&scheme)? {
            tg.options.insert((*k).into(), (*v).into());
        }
    }
    if let Some(ca) = &t.ca_cert {
        tg.ca_cert = Some(ca.clone());
    }
    if t.insecure {
        tg.insecure = true;
    }
    if let Some(px) = &t.proxy {
        tg.proxy = Some(px.clone());
    }

    let wl = &mut p.workload;
    if let Some(pr) = &w.preset {
        wl.preset = Preset::parse(pr)?;
        wl.mix.clear();
        wl.dataset = None;
    }
    if let Some(d) = &w.duration {
        wl.duration = HumanDuration(parse_duration(d)?);
    }
    if let Some(d) = &w.warmup {
        wl.warmup = HumanDuration(parse_duration(d)?);
    }
    if w.max_ops.is_some() {
        wl.max_ops = w.max_ops;
    }
    if w.rate.is_some() {
        wl.rate = w.rate;
    }
    if let Some(a) = &w.access {
        wl.access = a.parse::<Access>()?;
    }
    if w.existing.is_some() || w.objects.is_some() || w.object_size.is_some() || w.persist_dataset {
        let mut ds: DatasetSpec = wl.effective_dataset();
        if let Some(e) = &w.existing {
            ds.existing_prefix = Some(e.clone());
        }
        if let Some(n) = w.objects {
            ds.objects = n;
        }
        if let Some(s) = &w.object_size {
            ds.object_size = parse_size("--object-size", s)?;
        }
        if w.persist_dataset {
            ds.persist = true;
        }
        wl.dataset = Some(ds);
    }
    if w.no_verify {
        wl.verify = false;
    }
    if w.keep_data {
        wl.cleanup = false;
    }

    let kn = &mut p.knobs;
    if let Some(v) = k.concurrency {
        anyhow::ensure!(v >= 1, "concurrency must be at least 1");
        kn.concurrency = v;
    }
    if let Some(v) = k.io_concurrency {
        anyhow::ensure!(v >= 1, "I/O concurrency must be at least 1");
        kn.io_concurrency = v;
    }
    if let Some(v) = &k.chunk_size {
        kn.chunk_size = parse_size("--chunk-size", v)?;
    }
    if let Some(v) = &k.range_gap {
        kn.range_gap = parse_size("--range-gap", v)?;
    }
    if let Some(v) = k.max_inflight {
        kn.max_inflight_requests = v;
    }
    if let Some(h) = k.http {
        kn.http_version = h.into();
    }
    if k.no_keep_alive {
        kn.keep_alive = false;
    }
    if k.microscope {
        kn.transport = crate::config::TransportMode::Microscope;
    }
    if let Some(sz) = &k.cache {
        kn.cache = true;
        kn.cache_size = parse_size("--cache", sz)?;
    }
    if let Some(r) = k.retries {
        kn.retries = r;
    }
    if k.tail_cut {
        kn.tail_cut = true;
    }

    let cs = &mut p.cost;
    if let Some(v) = &c.provider {
        anyhow::ensure!(
            crate::analysis::cost::preset(v).is_some(),
            "unknown pricing provider `{v}` (aws-s3, gcs, azure-blob, cloudflare-r2, backblaze-b2, aliyun-oss)"
        );
        cs.provider = Some(v.clone());
    }
    if let Some(v) = c.monthly_ops {
        cs.monthly_ops = v;
    }
    if let Some(v) = c.egress_fraction {
        cs.egress_fraction = v;
    }
    if let Some(v) = c.stored_gib {
        cs.stored_gib = v;
    }
    p.validate()?;
    Ok(p)
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

/// A share given as `1%`, `0.5 %` or a fraction (`0.01`).
fn parse_rate(flag: &str, v: &str) -> Result<f64> {
    let t = v.trim();
    let r = match t.strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>().map(|x| x / 100.0),
        None => t.parse::<f64>(),
    }
    .map_err(|_| anyhow::anyhow!("invalid {flag} `{v}` (examples: 1%, 0.01)"))?;
    anyhow::ensure!(
        (0.0..=1.0).contains(&r),
        "{flag} must be between 0% and 100%"
    );
    Ok(r)
}

/// Apply `--sla`, `--max-error-rate`, `--max-throttle-rate` and `--p99-ms` to the profile.
pub fn apply_sla(p: &mut Profile, a: &SlaArgs) -> Result<()> {
    if let Some(m) = a.sla {
        p.sla.enforce = m == SlaMode::Enforce;
    }
    if let Some(v) = &a.max_error_rate {
        p.sla.max_error_rate = parse_rate("--max-error-rate", v)?;
    }
    if let Some(v) = &a.max_throttle_rate {
        p.sla.max_throttle_rate = Some(parse_rate("--max-throttle-rate", v)?);
    }
    if let Some(ms) = a.p99_ms {
        p.sla.p99_ms = Some(ms);
    }
    p.sla.validate()
}

/// The tuner's search settings, as text the user typed (flags or the app's Tune page).
#[derive(Clone, Copy, Debug)]
pub struct TuneSettings<'a> {
    pub trial_duration: &'a str,
    pub trial_warmup: &'a str,
    pub max_trials: usize,
    pub max_concurrency: usize,
    pub cooldown: &'a str,
}

impl Default for TuneSettings<'static> {
    /// `oprobe tune`'s defaults.
    fn default() -> Self {
        Self {
            trial_duration: tune_defaults::TRIAL_DURATION,
            trial_warmup: tune_defaults::TRIAL_WARMUP,
            max_trials: tune_defaults::MAX_TRIALS,
            max_concurrency: tune_defaults::MAX_CONCURRENCY,
            cooldown: tune_defaults::COOLDOWN,
        }
    }
}

/// Validate tuner settings and build the options `oprobe tune` and the app both run.
pub fn tune_options(
    objective: crate::tune::Objective,
    s: &TuneSettings<'_>,
    rate: Option<f64>,
) -> Result<crate::tune::TuneOptions> {
    let duration = parse_duration(s.trial_duration).context("invalid trial duration")?;
    anyhow::ensure!(
        !duration.is_zero(),
        "the trial duration must be greater than zero"
    );
    anyhow::ensure!(
        s.max_trials >= tune_defaults::MIN_TRIALS,
        "max trials must be at least {}",
        tune_defaults::MIN_TRIALS
    );
    anyhow::ensure!(s.max_concurrency >= 1, "max concurrency must be at least 1");
    Ok(crate::tune::TuneOptions {
        objective,
        trial: RunOptions {
            duration,
            warmup: parse_duration(s.trial_warmup).context("invalid trial warmup")?,
            max_ops: None,
            rate,
            trace_slow_ops: false,
        },
        max_concurrency: s.max_concurrency,
        max_trials: s.max_trials,
        cooldown: parse_duration(s.cooldown).context("invalid cooldown")?,
    })
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
                bail!("`{}` already exists", p.display());
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
