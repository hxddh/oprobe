//! The output formats of every command (design v4 §8, AG-4): each command offers what its
//! result type renders ([`CommandOutput::formats`]); `-o` outside that set is refused before
//! the command runs, and `oprobe capabilities` lists the formats per command.

use crate::output::{CommandOutput, DEFAULT_FORMATS, OutputFormat};

/// Commands whose output is text only (generated files, interactive interfaces).
pub const TEXT_ONLY: &[OutputFormat] = &[OutputFormat::Text];

fn f<T: CommandOutput>() -> &'static [OutputFormat] {
    T::formats()
}

/// `(command path, formats)` of every command, in `--help` order. A command missing here
/// (a newer one) gets the default formats.
pub fn table() -> Vec<(&'static str, &'static [OutputFormat])> {
    use crate::app;
    vec![
        ("run", f::<app::RunOutput>()),
        ("demo", f::<app::demo::DemoOutput>()),
        ("tune", f::<app::TuneOutput>()),
        ("verify", f::<app::verify::VerifyResult>()),
        ("fix", f::<app::fix::FixOutput>()),
        ("compare", f::<crate::analysis::compare::Comparison>()),
        ("runs list", f::<app::runs::RunsList>()),
        ("runs show", f::<app::runs::RunShow>()),
        ("runs open", DEFAULT_FORMATS),
        ("runs export", DEFAULT_FORMATS),
        ("runs rm", f::<app::runs::RemovedRuns>()),
        ("runs gc", f::<app::runs::RemovedRuns>()),
        ("analyze", f::<app::analyze::AnalyzeOutput>()),
        ("explain", f::<crate::app::explain::ExplainOutput>()),
        ("target add", f::<app::settings::TargetShow>()),
        ("target list", f::<app::settings::TargetList>()),
        ("target show", f::<app::settings::TargetShow>()),
        ("target rm", DEFAULT_FORMATS),
        ("target test", f::<app::doctor::DoctorOutput>()),
        ("target discover", f::<app::discover::Discovery>()),
        ("target sample", f::<app::sample::ObjectSample>()),
        ("doctor", f::<app::doctor::DoctorOutput>()),
        ("probe", f::<crate::net::probe::ProbeReport>()),
        ("watch", f::<app::watch::WatchOutput>()),
        ("matrix", f::<app::matrix::MatrixOutput>()),
        ("compat", f::<app::compat::CompatOutput>()),
        ("baseline set", f::<app::compare::BaselineShow>()),
        ("baseline list", f::<app::compare::BaselineList>()),
        ("baseline rm", DEFAULT_FORMATS),
        ("baseline show", f::<app::compare::BaselineShow>()),
        ("baseline calibrate", f::<app::compare::CalibrateOutput>()),
        ("lab", f::<app::misc::LabOutput>()),
        ("worker", f::<crate::distributed::WorkerOutput>()),
        ("mcp", DEFAULT_FORMATS),
        ("schema", f::<crate::mcp::schemas::SchemaList>()),
        (
            "capabilities",
            f::<crate::mcp::capabilities::Capabilities>(),
        ),
        ("config show", f::<app::settings::ConfigShow>()),
        ("config path", f::<app::settings::ConfigPaths>()),
        ("config edit", DEFAULT_FORMATS),
        ("cleanup", f::<app::misc::CleanupOutput>()),
        ("self-update", f::<crate::cli::self_update::UpdateOutput>()),
        ("top", TEXT_ONLY),
        ("gui", TEXT_ONLY),
        ("completions", TEXT_ONLY),
        ("man", TEXT_ONLY),
        ("init", TEXT_ONLY),
    ]
}

/// The formats of the command at `path` (`runs show`; aliases such as `tui` resolved by the
/// caller).
pub fn formats_of(path: &str) -> &'static [OutputFormat] {
    table()
        .into_iter()
        .find(|(p, _)| *p == path)
        .map(|(_, f)| f)
        .unwrap_or(DEFAULT_FORMATS)
}

/// The command path of parsed arguments (`runs show`), from clap's matches.
pub fn command_path(m: &clap::ArgMatches) -> String {
    let mut parts = Vec::new();
    let mut cur = m;
    while let Some((name, sub)) = cur.subcommand() {
        parts.push(name.to_owned());
        cur = sub;
    }
    parts.join(" ")
}

/// The names of formats, for messages.
pub fn names(f: &[OutputFormat]) -> Vec<&'static str> {
    f.iter()
        .map(|x| match x {
            OutputFormat::Text => "text",
            OutputFormat::Json => "json",
            OutputFormat::Ndjson => "ndjson",
            OutputFormat::Md => "md",
            OutputFormat::Html => "html",
        })
        .collect()
}

/// The `-o`/`--output` value given on the command line itself (not `OPROBE_OUTPUT`).
pub fn explicit_output(args: &[std::ffi::OsString]) -> Option<String> {
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
    given
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_files_are_text_only_and_documents_offer_md() {
        assert_eq!(formats_of("completions"), TEXT_ONLY);
        assert_eq!(formats_of("man"), TEXT_ONLY);
        assert!(formats_of("runs show").contains(&OutputFormat::Html));
        assert!(formats_of("doctor").contains(&OutputFormat::Json));
        assert!(!formats_of("doctor").contains(&OutputFormat::Html));
        assert_eq!(formats_of("no-such-command"), DEFAULT_FORMATS);
        let paths: Vec<&str> = table().iter().map(|(p, _)| *p).collect();
        let mut dedup = paths.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), paths.len());
    }

    #[test]
    fn every_command_is_in_the_table() {
        use clap::CommandFactory;
        let cmd = crate::cli::Cli::command();
        let mut missing = Vec::new();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            if sub.has_subcommands() {
                for s in sub.get_subcommands() {
                    let p = format!("{name} {}", s.get_name());
                    if !table().iter().any(|(t, _)| *t == p) && s.get_name() != "help" {
                        missing.push(p);
                    }
                }
            } else if !table().iter().any(|(t, _)| *t == name) && name != "help" {
                missing.push(name.to_owned());
            }
        }
        // Commands added by other streams fall back to the default formats; list them here
        // so capabilities shows them.
        assert!(
            missing
                .iter()
                .all(|m| ["verify", "fix"].contains(&m.as_str())),
            "commands without formats: {missing:?}"
        );
    }
}
