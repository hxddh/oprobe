//! Command handlers.

pub mod args;
mod doctor;
mod live;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::CommandFactory;

pub use args::Cli;
use args::*;

use crate::analysis::hints::hint_for;
use crate::config::{Access, DatasetSpec, Preset, Profile, TargetSpec};
use crate::engine::{Cancel, Progress, ProgressFn, RunOptions, Session};
use crate::report::diff::{Thresholds, diff};
use crate::report::style::{self, bold, dim, green, red, yellow};
use crate::report::{Report, markdown, terminal, write_file};
use crate::util::{ByteSize, HumanDuration, parse_duration};

/// Exit code when `--fail-on` matched or the diff gate failed.
pub const EXIT_GATE: u8 = 3;

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
    let scheme_hint = scheme_of(&command);
    match dispatch(command).await {
        Ok(code) => code,
        Err(err) => {
            print_error(&err, &scheme_hint);
            ExitCode::FAILURE
        }
    }
}

fn scheme_of(cmd: &Command) -> String {
    let t = match cmd {
        Command::Run(a) => a.target.target.clone(),
        Command::Tune(a) => a.target.target.clone(),
        Command::Top(a) | Command::Gui(a) => a.target.clone(),
        Command::Doctor(a) => a.target.target.clone(),
        Command::Cleanup(a) => a.target.target.clone(),
        _ => None,
    };
    t.and_then(|u| u.split_once("://").map(|(s, _)| s.to_owned()))
        .unwrap_or_else(|| "s3".into())
}

pub fn print_error(err: &anyhow::Error, scheme: &str) {
    let chain = format!("{err:#}");
    eprintln!("{} {}", red(&bold("error:")), err);
    for cause in err.chain().skip(1) {
        eprintln!("  {} {cause}", dim("caused by:"));
    }
    if let Some(h) = hint_for(&chain, scheme) {
        eprintln!("\n{} {}", yellow(&bold("hint:")), bold(&h.title));
        for s in h.steps {
            eprintln!("  • {s}");
        }
    }
}

/// Interactive commands run on the main thread, before (and without) the CLI's async runtime:
/// native toolkits require the main thread, and the UI runs the engine on a runtime of its own.
/// Returns `None` for every other command.
pub fn ui_entry(cli: &Cli) -> Option<ExitCode> {
    #[cfg(feature = "tui")]
    {
        let (args, window) = match &cli.command {
            Some(Command::Top(a)) => (Some(a), false),
            Some(Command::Gui(a)) => (Some(a), true),
            None => (None, gui_available()),
            Some(_) => return None,
        };
        if cli.command.is_none() && !window && !std::io::stdout().is_terminal() {
            let _ = Cli::command().print_help();
            return Some(ExitCode::SUCCESS);
        }
        if let Some(a) = args {
            crate::ui::set_launch(crate::ui::Launch {
                target: a.target.clone(),
                endpoint: a.endpoint.clone(),
                preset: a.preset.clone(),
                duration: a.duration.clone(),
                concurrency: a.concurrency.map(|c| c.to_string()),
                insecure: a.insecure,
                anonymous: a.anonymous,
            });
        }
        if window {
            return Some(run_window());
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
#[cfg(feature = "tui")]
fn gui_available() -> bool {
    if !cfg!(feature = "gui") {
        return false;
    }
    if cfg!(target_os = "linux") {
        return std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some();
    }
    // macOS / Windows: a remote shell has no session to draw on.
    std::env::var_os("SSH_CONNECTION").is_none()
}

#[cfg(feature = "tui")]
fn run_window() -> ExitCode {
    #[cfg(feature = "gui")]
    {
        detach_console();
        crate::ui::run_gui();
        ExitCode::SUCCESS
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

async fn dispatch(cmd: Command) -> Result<ExitCode> {
    match cmd {
        Command::Run(a) => cmd_run(a).await,
        Command::Tune(a) => cmd_tune(a).await,
        Command::Diff(a) => cmd_diff(a),
        Command::Top(_) | Command::Gui(_) => {
            unreachable!("interactive commands are handled by ui_entry")
        }
        Command::Doctor(a) => doctor::run(a).await,
        Command::Probe(a) => cmd_probe(a).await,
        Command::Lab(a) => cmd_lab(a).await,
        Command::Cleanup(a) => cmd_cleanup(a).await,
        Command::Init(a) => cmd_init(a),
        Command::Report(a) => {
            let r = Report::load_json(&a.file)?;
            emit(&r, a.format)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "oprobe", &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
        Command::Man { out_dir } => {
            let cmd = Cli::command();
            match out_dir {
                Some(dir) => {
                    std::fs::create_dir_all(&dir)?;
                    clap_mangen::generate_to(cmd, &dir)?;
                }
                None => clap_mangen::Man::new(cmd).render(&mut std::io::stdout())?,
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Profile assembly
// ---------------------------------------------------------------------------------------

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
        (None, None) => bail!(
            "no target given\n\n  Try `oprobe run memory://` for a local demo, `oprobe run s3://bucket/prefix`, \
             or `oprobe lab` to start a local S3 server."
        ),
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
        tg.options.insert("allow_anonymous".into(), "true".into());
        tg.options
            .insert("disable_config_load".into(), "true".into());
        tg.options.insert("skip_signature".into(), "true".into());
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
        kn.concurrency = v.max(1);
    }
    if let Some(v) = k.io_concurrency {
        kn.io_concurrency = v.max(1);
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

// ---------------------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------------------

/// Install a Ctrl-C handler: first press cancels gracefully (cleanup still runs), second exits.
fn install_ctrl_c(cancel: Cancel) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
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

fn emit(r: &Report, format: Format) -> Result<()> {
    match format {
        Format::Text => print!("{}", terminal::render(r)),
        Format::Json => println!("{}", serde_json::to_string_pretty(r)?),
        Format::Markdown => println!("{}", markdown::render(r)),
    }
    Ok(())
}

fn save_outputs(r: &Report, o: &OutputArgs) -> Result<()> {
    if let Some(p) = &o.json {
        r.save_json(p)?;
    }
    if let Some(p) = &o.markdown {
        write_file(p, &markdown::render(r))?;
    }
    Ok(())
}

fn gate_exit(r: &Report, o: &OutputArgs) -> ExitCode {
    use crate::analysis::diagnosis::Severity;
    if let Some(level) = o.fail_on {
        let min = match level {
            FailOn::Warning => Severity::Warning,
            FailOn::Critical => Severity::Critical,
        };
        if r.diagnosis.worst() >= min {
            return ExitCode::from(EXIT_GATE);
        }
    }
    ExitCode::SUCCESS
}

fn progress_for(quiet: bool, total: Duration) -> (Option<ProgressFn>, Option<Arc<live::LiveLine>>) {
    if quiet || !std::io::stderr().is_terminal() {
        let msg: ProgressFn = Arc::new(move |p| {
            if let Progress::Message(m) = p
                && !quiet
            {
                eprintln!("{m}");
            }
        });
        return (Some(msg), None);
    }
    let line = Arc::new(live::LiveLine::new(total));
    let l2 = line.clone();
    (Some(Arc::new(move |p| l2.update(p))), Some(line))
}

async fn cmd_run(a: RunArgs) -> Result<ExitCode> {
    let mut profile = build_profile(&a.target, &a.workload, &a.knobs, &a.cost)?;
    if a.output.label.is_some() {
        profile.name = a.output.label.clone();
    }
    let cancel = Cancel::new();
    install_ctrl_c(cancel.clone());
    let (progress, line) = progress_for(a.output.quiet, profile.workload.duration.get());
    let progress = progress.expect("progress callback");
    if !a.output.quiet {
        eprintln!(
            "{} {} {}",
            bold("oprobe"),
            profile.target.display(),
            dim(&format!(
                "· {} · {}",
                profile.workload.preset.slug(),
                profile.knobs.summary()
            ))
        );
    }
    let session = Session::open(profile, cancel.clone(), progress.clone()).await?;
    if a.output.format != Format::Text {
        for w in &session.warnings {
            eprintln!("{} {w}", yellow("warning:"));
        }
    }
    let knobs = session.profile.knobs.clone();
    let res = session
        .run(
            &knobs,
            &RunOptions::from_profile(&session.profile),
            Some(progress.clone()),
        )
        .await;
    if let Some(l) = &line {
        l.finish();
    }
    let (ok, bad) = if res.is_ok() && session.profile.workload.verify {
        session.verify_writes(None).await
    } else {
        (0, 0)
    };
    if !a.output.quiet {
        eprintln!("{}", dim("cleaning up…"));
    }
    let cleanup = session.close().await;
    let mut snap = res?;
    snap.integrity.verified += ok;
    snap.integrity.corrupted += bad;
    let report = Report::build(
        &session.profile,
        &knobs,
        snap,
        cleanup,
        session.warnings.clone(),
    );
    save_outputs(&report, &a.output)?;
    emit(&report, a.output.format)?;
    if cancel.is_cancelled() {
        eprintln!(
            "{}",
            yellow("run interrupted: results cover the partial window")
        );
    }
    Ok(gate_exit(&report, &a.output))
}

// ---------------------------------------------------------------------------------------
// tune
// ---------------------------------------------------------------------------------------

async fn cmd_tune(a: TuneArgs) -> Result<ExitCode> {
    use crate::tune::{Objective, TuneEvent, TuneOptions, codegen, tune};
    let mut profile = build_profile(&a.target, &a.workload, &a.knobs, &a.cost)?;
    if let Some(p) = a.p99_ms {
        profile.sla.p99_ms = Some(p);
    }
    let cancel = Cancel::new();
    install_ctrl_c(cancel.clone());
    let quiet = a.output.quiet;
    let msg: ProgressFn = Arc::new(move |p| {
        if let Progress::Message(m) = p
            && !quiet
        {
            eprintln!("{} {m}", dim("·"));
        }
    });
    let session = Session::open(profile, cancel.clone(), msg).await?;
    let opts = TuneOptions {
        objective: match a.objective {
            ObjectiveArg::Throughput => Objective::Throughput,
            ObjectiveArg::Latency => Objective::Latency,
            ObjectiveArg::Cost => Objective::Cost,
        },
        trial: RunOptions {
            duration: parse_duration(&a.trial_duration)?,
            warmup: parse_duration(&a.trial_warmup)?,
            max_ops: None,
            rate: session.profile.workload.rate,
            trace_slow_ops: false,
        },
        max_concurrency: a.max_concurrency,
        max_trials: a.max_trials.max(2),
        cooldown: parse_duration(&a.cooldown)?,
    };
    if !quiet {
        eprintln!(
            "{} {} {}",
            bold("oprobe tune"),
            session.profile.target.display(),
            dim(&format!(
                "· {} · objective {} · ≤{} trials of {}",
                session.profile.workload.preset.slug(),
                opts.objective.label(),
                opts.max_trials,
                a.trial_duration
            ))
        );
        eprintln!(
            "{}",
            dim(
                "   #  stage            conc  io  chunk   gap     cache  http     ops/s    MiB/s     p99       result"
            )
        );
    }
    let on_event: Arc<dyn Fn(TuneEvent) + Send + Sync> = Arc::new(move |ev| {
        if quiet {
            return;
        }
        match ev {
            TuneEvent::Started { .. } => {}
            TuneEvent::Message(m) => eprintln!("   {}", dim(&m)),
            TuneEvent::Finished(t) => {
                let k = &t.knobs;
                let verdict = if t.throttled() {
                    red(&format!("throttled ({})", t.throttled_responses))
                } else if t.feasible {
                    green("ok")
                } else {
                    yellow(&t.note)
                };
                eprintln!(
                    "  {:>2}  {:<15} {:>5} {:>3}  {:<7} {:<7} {:<6} {:<6} {:>8.1} {:>8.1} {:>9}  {verdict}",
                    t.id,
                    t.stage,
                    k.concurrency,
                    k.io_concurrency,
                    k.chunk_size.to_string(),
                    k.range_gap.to_string(),
                    if k.cache { "on" } else { "off" },
                    format!("{:?}", k.http_version).to_ascii_lowercase(),
                    t.ops_per_s,
                    t.mib_per_s,
                    crate::util::fmt_ms(t.p99_ms),
                );
            }
        }
    });
    let outcome = tune(&session, &opts, on_event).await;
    let cleanup = session.close().await;
    let outcome = outcome?;
    let mut report = Report::build(
        &session.profile,
        &outcome.winner,
        outcome.winner_snapshot.clone(),
        cleanup,
        session.warnings.clone(),
    );
    report.tune = Some(outcome.summary.clone());

    let header = format!(
        "objective {} · {:+.0}% vs baseline · {}",
        outcome.summary.objective,
        outcome.summary.improvement * 100.0,
        crate::util::now_rfc3339()
    );
    if let Some(p) = &a.emit_profile {
        write_file(
            p,
            &codegen::profile_toml(&session.profile, &outcome.winner, &header)?,
        )?;
    }
    if let Some(p) = &a.emit_rust {
        write_file(
            p,
            &codegen::rust_snippet(&session.profile.target.scheme, &outcome.winner),
        )?;
    }
    if let Some(p) = &a.emit_python {
        write_file(
            p,
            &codegen::python_snippet(&session.profile.target.scheme, &outcome.winner),
        )?;
    }
    save_outputs(&report, &a.output)?;
    match a.output.format {
        Format::Text => {
            let s = &outcome.summary;
            println!();
            println!(
                "{} {}",
                style::badge("TUNED", "42;30"),
                bold(&format!(
                    "{} → {:+.0}% {} vs baseline",
                    outcome.winner.summary(),
                    s.improvement * 100.0,
                    outcome.summary.objective
                ))
            );
            if let Some(c) = s.throttle_ceiling {
                println!("  {} throttling observed at concurrency {c}", yellow("▲"));
            }
            for n in &s.notes {
                println!("  {} {n}", dim("·"));
            }
            for (flag, path) in [
                ("profile", &a.emit_profile),
                ("rust", &a.emit_rust),
                ("python", &a.emit_python),
            ] {
                if let Some(p) = path {
                    println!("  {} wrote {flag}: {}", green("✓"), p.display());
                }
            }
            println!(
                "\n{}",
                dim("Confirmation run of the winning configuration:")
            );
            print!("{}", terminal::render(&report));
        }
        f => emit(&report, f)?,
    }
    Ok(gate_exit(&report, &a.output))
}

// ---------------------------------------------------------------------------------------
// diff / probe / lab / cleanup / init
// ---------------------------------------------------------------------------------------

fn cmd_diff(a: DiffArgs) -> Result<ExitCode> {
    let base = Report::load_json(&a.baseline)?;
    let cand = Report::load_json(&a.candidate)?;
    let t = Thresholds {
        p99_pct: a.max_p99_increase,
        p50_pct: a.max_p50_increase,
        min_latency_delta_ms: a.min_latency_delta_ms,
        throughput_drop_pct: a.max_throughput_drop,
        error_rate_pp: a.max_error_increase,
        request_amp_pct: a.max_request_amp_increase,
        read_amp_pct: a.max_read_amp_increase,
        cost_pct: a.max_cost_increase,
        fail_on_new_throttling: !a.allow_throttling,
        min_samples: a.min_samples,
    };
    let d = diff(&base, &cand, &t);
    if let Some(p) = &a.markdown {
        write_file(p, &d.render_markdown())?;
    }
    if let Some(p) = &a.junit {
        write_file(p, &d.render_junit())?;
    }
    if let Some(p) = &a.json {
        write_file(p, &serde_json::to_string_pretty(&d)?)?;
    }
    match a.format {
        Format::Text => print!("{}", d.render_terminal()),
        Format::Json => println!("{}", serde_json::to_string_pretty(&d)?),
        Format::Markdown => println!("{}", d.render_markdown()),
    }
    Ok(if d.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_GATE)
    })
}

async fn cmd_probe(a: ProbeArgs) -> Result<ExitCode> {
    use crate::net::probe::{ProbeOptions, probe_endpoint};
    let endpoint = if a.endpoint.starts_with("http://") || a.endpoint.starts_with("https://") {
        a.endpoint.clone()
    } else {
        TargetSpec::from_url(&a.endpoint)?
            .normalized()?
            .endpoint()
            .with_context(|| {
                format!(
                    "cannot derive an http(s) endpoint from `{}`; pass one explicitly",
                    a.endpoint
                )
            })?
    };
    let mut tls = crate::net::TlsClientOptions::default();
    if let Some(p) = &a.ca_cert {
        tls.extra_ca_pem = Some(
            std::fs::read_to_string(p).with_context(|| format!("cannot read `{}`", p.display()))?,
        );
    }
    tls.insecure_skip_verify = a.insecure;
    let opts = ProbeOptions {
        samples: a.samples,
        timeout: parse_duration(&a.timeout)?,
        http: a.http.into(),
        tls,
        proxy: match a.proxy.as_deref() {
            None => None,
            Some("none") => Some(None),
            Some(p) => Some(Some(p.to_owned())),
        },
        method: if a.get {
            http::Method::GET
        } else {
            http::Method::HEAD
        },
    };
    let r = probe_endpoint(&endpoint, &opts).await?;
    if let Some(p) = &a.json {
        write_file(p, &serde_json::to_string_pretty(&r)?)?;
    }
    match a.format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&r)?),
        _ => print!("{}", doctor::render_probe(&r)),
    }
    Ok(ExitCode::SUCCESS)
}

async fn cmd_lab(a: LabArgs) -> Result<ExitCode> {
    use crate::lab::{LAB_CA_PEM, LabConfig, LabServer};
    if let Some(p) = &a.write_ca {
        write_file(p, LAB_CA_PEM)?;
    }
    let cfg = LabConfig {
        enable_tls: a.tls,
        enable_http2: !a.h1_only,
        base_ttfb_delay: parse_duration(&a.latency)?,
        multipart_merge_delay: parse_duration(&a.complete_delay)?,
        straggler_every_n: a.straggler_every,
        straggler_delay: parse_duration(&a.straggler)?,
        max_inflight_before_503: a.throttle_above,
        throttle_every_n_requests: a.throttle_every,
        throttle_status_code: a.throttle_status,
        stream_bandwidth_kib_s: a.bandwidth_kib,
        throttle_early_close: a.throttle_early_close,
    };
    let lab = LabServer::spawn(&a.bind, cfg).await?;
    println!(
        "{} S3 lab listening on {}",
        green("●"),
        bold(&lab.endpoint_url)
    );
    let tls_flag = if a.tls {
        match &a.write_ca {
            Some(p) => format!(" --ca-cert {}", p.display()),
            None => " --insecure".into(),
        }
    } else {
        String::new()
    };
    println!(
        "  try: AWS_ACCESS_KEY_ID=lab AWS_SECRET_ACCESS_KEY=lab oprobe run s3://bench/demo --endpoint {} --proxy none{tls_flag}",
        lab.endpoint_url
    );
    println!(
        "  {}",
        dim("any bucket name works; data lives in memory; Ctrl-C to stop")
    );
    tokio::signal::ctrl_c().await?;
    println!(
        "{} served {} requests",
        dim("lab stopped:"),
        lab.requests_served()
    );
    Ok(ExitCode::SUCCESS)
}

async fn cmd_cleanup(a: CleanupArgs) -> Result<ExitCode> {
    let p = build_profile(
        &a.target,
        &WorkloadArgs::default(),
        &KnobArgs::default(),
        &CostArgs::default(),
    )?;
    let target = p.target.normalized()?;
    let base = crate::engine::operator::base_operator(&target)?;
    let admin = crate::engine::operator::admin_operator(&base, &target)?;
    let older = parse_duration(&a.older_than)?;
    let found = crate::engine::cleanup::sweep(&admin, older, a.datasets, a.dry_run).await?;
    if found.is_empty() {
        println!(
            "{} nothing to clean under {} (older than {})",
            green("✓"),
            target.display(),
            a.older_than
        );
        return Ok(ExitCode::SUCCESS);
    }
    for e in &found {
        let age = e
            .age_s
            .map(|s| format!("{}h{:02}m old", s / 3600, (s % 3600) / 60))
            .unwrap_or_else(|| "dataset".into());
        let status = if a.dry_run {
            yellow("would delete")
        } else if e.deleted {
            green("deleted")
        } else {
            red("partially deleted")
        };
        println!(
            "  {status} {} {}",
            e.prefix,
            dim(&format!("({} objects, {age})", e.objects))
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_init(a: InitArgs) -> Result<ExitCode> {
    let preset = Preset::parse(&a.preset)?;
    let text = crate::config::template(
        preset,
        a.target.as_deref().unwrap_or("s3://my-bucket/oprobe"),
    );
    match &a.output {
        Some(p) => {
            if p.exists() {
                bail!("`{}` already exists", p.display());
            }
            write_file(p, &text)?;
            eprintln!(
                "{} wrote {} — edit it, then `oprobe run --profile {}`",
                green("✓"),
                p.display(),
                p.display()
            );
        }
        None => print!("{text}"),
    }
    Ok(ExitCode::SUCCESS)
}
