//! Help in two tiers (design v5 §6 "Help", review C13): `--help` shows the common flags of the
//! large commands (`run` has more than 60), `--help-all` shows every flag. The hidden flags
//! still parse; the help names the headings they live under.
//!
//! The man pages, `docs/cli.md` and shell completions use the full command
//! ([`crate::cli::Cli`]'s `CommandFactory`), never the tiered one.

use std::collections::BTreeSet;

use clap::{Command, CommandFactory};

use super::Cli;

/// A command with more options than this gets the short tier.
const TIER_ABOVE: usize = 14;

/// Flags (argument ids) every tier shows: what a first run, a CI gate or a tuning session
/// needs. Everything else of a large command is listed by `--help-all`.
const COMMON: &[&str] = &[
    // target
    "target",
    "profile",
    "endpoint",
    "region",
    "anonymous",
    // workload
    "preset",
    "duration",
    "rate",
    "existing",
    // knobs
    "concurrency",
    // safety
    "dry_run",
    "read_only",
    "max_cost_usd",
    "max_duration",
    // SLA and output
    "p99_ms",
    "max_error_rate",
    "max_throttle_rate",
    "label",
    "json",
    // run, verify, fix, tune
    "repeat",
    "baseline",
    "from",
    "set",
    "code",
    "all",
    "objective",
    "budget",
    "emit_profile",
    "write_profile",
    // watch, matrix
    "interval",
    "slo",
    "listen",
    "otlp_endpoint",
    "count",
    "targets",
    "vary",
];

/// `--help-all` on the command line (before `--`).
pub fn wants_help_all(args: &[std::ffi::OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--help-all")
}

/// The command line with `--help-all` replaced by `--help` (clap prints the full tier).
pub fn help_all_args(args: &[std::ffi::OsString]) -> Vec<std::ffi::OsString> {
    let mut past = false;
    args.iter()
        .map(|a| {
            if a == "--" {
                past = true;
            }
            if !past && a == "--help-all" {
                "--help".into()
            } else {
                a.clone()
            }
        })
        .collect()
}

/// The command tree to parse with: tiered unless `all`.
pub fn command(all: bool) -> Command {
    let cmd = Cli::command();
    if all { cmd } else { tier(cmd, "oprobe") }
}

/// Hide the uncommon flags of `cmd` (when it has many) and of its subcommands.
fn tier(mut cmd: Command, path: &str) -> Command {
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();
    for n in names {
        let sub_path = format!("{path} {n}");
        cmd = cmd.mut_subcommand(&n, |s| tier(s, &sub_path));
    }
    let options: Vec<_> = cmd
        .get_arguments()
        .filter(|a| !a.is_positional() && !a.is_global_set() && !a.is_hide_set())
        .collect();
    if options.len() <= TIER_ABOVE {
        return cmd;
    }
    let mut headings = BTreeSet::new();
    let mut hidden = Vec::new();
    for a in options {
        let id = a.get_id().as_str();
        if COMMON.contains(&id) || matches!(id, "help" | "version") {
            continue;
        }
        headings.insert(a.get_help_heading().unwrap_or("Options").to_owned());
        hidden.push(id.to_owned());
    }
    if hidden.is_empty() {
        return cmd;
    }
    for id in &hidden {
        cmd = cmd.mut_arg(id.as_str(), |a| a.hide(true));
    }
    let more = format!(
        "{} more flags ({}): `{path} --help-all`",
        hidden.len(),
        headings.into_iter().collect::<Vec<_>>().join(", ")
    );
    let after = match cmd.get_after_help() {
        Some(a) => format!("{a}\n\n{more}"),
        None => more,
    };
    cmd.after_help(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn help(cmd: &mut Command, path: &[&str]) -> String {
        let mut c = cmd.clone();
        for p in path {
            c = c.find_subcommand(p).unwrap().clone();
        }
        c.render_help().to_string()
    }

    #[test]
    fn run_help_is_short_and_help_all_is_complete() {
        let mut short = command(false);
        let mut full = command(true);
        let s = help(&mut short, &["run"]);
        let f = help(&mut full, &["run"]);
        let (sn, fnn) = (s.lines().count(), f.lines().count());
        assert!(sn * 2 < fnn, "{sn} vs {fnn} lines:\n{s}");
        assert!(s.contains("--preset") && s.contains("--dry-run") && s.contains("--concurrency"));
        assert!(!s.contains("--chunk-size"), "{s}");
        assert!(f.contains("--chunk-size"));
        assert!(s.contains("oprobe run --help-all"), "{s}");
        assert!(
            s.contains("Client knobs"),
            "the footer names the headings: {s}"
        );
    }

    #[test]
    fn hidden_flags_still_parse() {
        use clap::FromArgMatches;
        let m = command(false)
            .try_get_matches_from(["oprobe", "run", "memory://", "--chunk-size", "8MiB"])
            .unwrap();
        let cli = Cli::from_arg_matches(&m).unwrap();
        match cli.command {
            Some(super::super::args::Command::Run(a)) => {
                assert_eq!(a.knobs.chunk_size.as_deref(), Some("8MiB"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn small_commands_are_untouched() {
        let mut short = command(false);
        let mut full = command(true);
        assert_eq!(
            help(&mut short, &["runs", "list"]),
            help(&mut full, &["runs", "list"])
        );
    }

    #[test]
    fn help_all_is_rewritten_before_the_separator_only() {
        let a: Vec<std::ffi::OsString> = ["oprobe", "run", "--help-all", "--", "--help-all"]
            .iter()
            .map(Into::into)
            .collect();
        assert!(wants_help_all(&a));
        let b = help_all_args(&a);
        assert_eq!(b[2], "--help");
        assert_eq!(b[4], "--help-all");
        let c: Vec<std::ffi::OsString> = ["oprobe", "run", "--", "--help-all"]
            .iter()
            .map(Into::into)
            .collect();
        assert!(!wants_help_all(&c));
    }

    #[test]
    fn every_common_flag_exists_somewhere() {
        let mut ids = BTreeSet::new();
        fn walk(c: &Command, ids: &mut BTreeSet<String>) {
            for a in c.get_arguments() {
                ids.insert(a.get_id().as_str().to_owned());
            }
            for s in c.get_subcommands() {
                walk(s, ids);
            }
        }
        walk(&Cli::command(), &mut ids);
        for c in COMMON {
            assert!(ids.contains(*c), "COMMON names `{c}`, which no command has");
        }
    }
}
