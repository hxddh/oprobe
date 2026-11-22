//! Dynamic shell completions (design v5 §6 "Completions"): target names from the config and
//! run ids and labels from the run store, completed at the moment `TAB` is pressed.
//!
//! `oprobe completions SHELL` prints a small registration script that calls back into the
//! binary (`OPROBE_COMPLETE=SHELL oprobe -- …`); `--static` prints the 4.x static script.
//! Completers never fail and never print: a missing config or run store means no candidates.

use std::ffi::OsStr;

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use clap_complete::env::{Bash, Elvish, EnvCompleter, Fish, Powershell, Zsh};

use crate::store::{ConfigFile, Paths};

/// The environment variable the registration scripts set to ask for completions.
pub const VAR: &str = "OPROBE_COMPLETE";

/// Answer a completion request (when [`VAR`] is set) and exit; otherwise return.
pub fn handle_request() {
    clap_complete::CompleteEnv::with_factory(<super::Cli as clap::CommandFactory>::command)
        .var(VAR)
        .complete();
}

/// The registration script of `shell`, calling `bin` for candidates.
pub fn registration(shell: clap_complete::Shell, bin: &str) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let c: &dyn EnvCompleter = match shell {
        clap_complete::Shell::Bash => &Bash,
        clap_complete::Shell::Zsh => &Zsh,
        clap_complete::Shell::Fish => &Fish,
        clap_complete::Shell::PowerShell => &Powershell,
        clap_complete::Shell::Elvish => &Elvish,
        _ => &Bash,
    };
    c.write_registration(VAR, "oprobe", "oprobe", bin, &mut buf)?;
    Ok(buf)
}

fn paths() -> Paths {
    Paths::resolve(None)
}

fn matching<'a>(
    current: &OsStr,
    items: impl IntoIterator<Item = (String, Option<String>)> + 'a,
) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    let mut seen = std::collections::BTreeSet::new();
    items
        .into_iter()
        .filter(|(v, _)| v.starts_with(cur.as_ref()) && seen.insert(v.clone()))
        .map(|(v, help)| CompletionCandidate::new(v).help(help.map(Into::into)))
        .collect()
}

/// Configured target names (with their URL), then the storage URL schemes.
pub fn target_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let mut items: Vec<(String, Option<String>)> = ConfigFile::load(&paths().config_file)
        .map(|c| {
            c.targets
                .iter()
                .map(|(name, t)| (name.clone(), Some(t.url.clone())))
                .collect()
        })
        .unwrap_or_default();
    for s in [
        "s3://",
        "gcs://",
        "azblob://",
        "oss://",
        "cos://",
        "obs://",
        "b2://",
        "webdav://",
        "fs://",
        "memory://",
        "lab://demo",
    ] {
        items.push((s.to_owned(), None));
    }
    matching(current, items)
}

/// `last`, `last~1`, run ids (newest first) and labels.
pub fn run_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let store = crate::store::RunStore::new(paths().runs_dir());
    let mut items: Vec<(String, Option<String>)> = vec![
        ("last".into(), Some("the newest run".into())),
        ("last~1".into(), Some("the run before it".into())),
    ];
    // Sidecars are small JSON files: labels for the newest runs only, to stay instant.
    let summaries = store.list().unwrap_or_default();
    for s in summaries.iter().take(200) {
        let help = match &s.label {
            Some(l) => format!("{l} · {}", s.target_display),
            None => s.target_display.clone(),
        };
        items.push((s.id.clone(), Some(help)));
    }
    for s in summaries.iter().take(200) {
        if let Some(l) = &s.label {
            items.push((l.clone(), Some(format!("label of {}", s.id))));
        }
    }
    matching(current, items)
}

/// Run refs, then finding codes (`oprobe explain RUNREF|CODE`).
pub fn explain_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let mut c = run_candidates(current);
    let cur = current.to_string_lossy();
    for code in crate::analysis::catalog::codes() {
        if code.starts_with(cur.as_ref()) {
            c.push(CompletionCandidate::new(code).help(Some("finding code".into())));
        }
    }
    c
}

/// Completer of TARGET arguments.
pub fn targets() -> ArgValueCompleter {
    ArgValueCompleter::new(target_candidates)
}

/// Completer of RUNREF arguments.
pub fn runs() -> ArgValueCompleter {
    ArgValueCompleter::new(run_candidates)
}

/// Completer of `explain`'s subject.
pub fn explain() -> ArgValueCompleter {
    ArgValueCompleter::new(explain_candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_scripts_call_back_into_the_binary() {
        for sh in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
            clap_complete::Shell::Elvish,
        ] {
            let s = String::from_utf8(registration(sh, "oprobe").unwrap()).unwrap();
            assert!(s.contains(VAR), "{sh:?}: {s}");
        }
    }

    #[test]
    fn candidates_filter_by_prefix() {
        let c = matching(
            OsStr::new("pr"),
            vec![
                ("prod".to_owned(), None),
                ("staging".to_owned(), None),
                ("prod".to_owned(), None),
            ],
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].get_value(), "prod");
    }
}
