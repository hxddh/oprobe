//! The app's commands in one list: the menu bar, the command palette (⌘K) and the help all
//! read it, with the same enabled states (design v4 §11, UI-12). The actions live in the
//! shell; this is what each command is called and when it can run.

/// One command: its id, title, menu group and shortcut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmdSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub keys: Option<&'static str>,
}

const fn c(
    id: &'static str,
    label: &'static str,
    group: &'static str,
    keys: Option<&'static str>,
) -> CmdSpec {
    CmdSpec {
        id,
        label,
        group,
        keys,
    }
}

/// Every command, in menu order. Tab commands (`tab-0` … `tab-5`) follow in the View group.
pub const COMMANDS: &[CmdSpec] = &[
    c("new", "New Run…", "File", Some("mod+n")),
    c("open-run", "Open Run…", "File", Some("shift+mod+o")),
    c("open", "Open Run File…", "File", Some("mod+o")),
    c("add-target", "Add Target…", "File", None),
    c("switch-target", "Switch Target…", "File", None),
    c("export", "Export Run…", "File", Some("mod+s")),
    c("export-json", "Export Report as JSON…", "File", None),
    c("export-md", "Export Report as Markdown…", "File", None),
    c("export-csv", "Export Requests as CSV…", "File", None),
    c("copy-link", "Copy Link to This View", "File", Some("shift+mod+l")),
    c("settings", "Settings…", "File", Some("mod+,")),
    c("run", "Run", "Run", Some("mod+r")),
    c("stop", "Stop", "Run", Some("mod+.")),
    c("rerun", "Run Again", "Run", Some("shift+mod+r")),
    c("edit-rerun", "Edit and Run Again…", "Run", None),
    c("verify", "Verify Top Finding", "Run", None),
    c("fix", "Fix: Recommend Settings…", "Run", None),
    c("demo", "Run the Demo", "Run", None),
    c("compare", "Compare Runs", "Run", Some("shift+mod+c")),
    c("tune", "Tune…", "Run", None),
    c("probe", "Probe…", "Run", None),
    c("slowest", "Explain the Slowest Requests", "Run", None),
    c("health", "Target Health", "View", Some("mod+0")),
    c("waterfall", "Waterfall of the Slowest Operation", "View", Some("mod+j")),
    c("sidebar", "Toggle Sidebar", "View", Some("mod+alt+s")),
    c("inspector", "Toggle Inspector", "View", Some("mod+alt+i")),
    c("log", "Logarithmic Latency Axis", "View", None),
    c("palette", "Command Palette…", "View", Some("mod+k")),
    c("tab-0", "Overview", "View", Some("mod+1")),
    c("tab-1", "Requests", "View", Some("mod+2")),
    c("tab-2", "Timeline", "View", Some("mod+3")),
    c("tab-3", "Connections", "View", Some("mod+4")),
    c("tab-4", "Findings", "View", Some("mod+5")),
    c("tab-5", "Cost", "View", Some("mod+6")),
    c("help", "Keyboard Shortcuts", "Help", None),
    c("docs", "oprobe Documentation", "Help", None),
];

/// What the enabled states depend on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CmdCtx {
    /// A job (run, tuning, probe) is running or cleaning up.
    pub busy: bool,
    /// A finished run is shown.
    pub has_run: bool,
    /// The shown run has a finding with a verification.
    pub has_verify: bool,
    /// Runs in the history.
    pub runs: usize,
    /// Configured targets.
    pub targets: usize,
}

/// Whether command `id` can run now. The menu and the palette both ask this.
pub fn enabled(id: &str, x: &CmdCtx) -> bool {
    match id {
        "stop" => x.busy,
        "run" | "demo" | "probe" | "new" => !x.busy,
        "rerun" | "edit-rerun" | "fix" => !x.busy && x.has_run,
        "tune" => !x.busy && (x.has_run || x.targets > 0),
        "verify" => !x.busy && x.has_run && x.has_verify,
        "export" | "export-json" | "export-md" | "export-csv" | "slowest" | "copy-link"
        | "waterfall" => x.has_run,
        "health" => x.runs > 0,
        "compare" => x.runs >= 2,
        "open-run" => x.runs > 0,
        "switch-target" => x.targets > 0,
        _ => true,
    }
}

/// Why command `id` cannot run now (`None` when it can): the palette shows it next to a
/// disabled command (U6), the same words in every language.
pub fn disabled_reason(id: &str, x: &CmdCtx) -> Option<&'static str> {
    use crate::ui::model::i18n::tr;
    if enabled(id, x) {
        return None;
    }
    Some(match id {
        "stop" => tr("why.idle"),
        "run" | "demo" | "probe" | "new" => tr("why.busy"),
        "rerun" | "edit-rerun" | "fix" | "verify" | "tune" if x.busy => tr("why.busy"),
        "verify" if x.has_run => tr("why.no_verify"),
        "tune" => tr("why.tune"),
        "compare" => tr("why.two_runs"),
        "open-run" | "health" => tr("why.no_runs"),
        "switch-target" => tr("why.no_targets"),
        _ => tr("why.no_run"),
    })
}

/// How well `query` matches `text` (fuzzy, case-insensitive): every query character in order,
/// scored higher for a prefix, word starts and consecutive runs; `None` when it does not match.
/// `"exm"` finds "Export Report as Markdown…", `"cmp"` finds "Compare Runs" (U6).
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut qi = 0usize;
    let mut prev: Option<usize> = None;
    for (ti, ch) in t.iter().enumerate() {
        if qi == q.len() {
            break;
        }
        if *ch != q[qi] {
            continue;
        }
        let word_start = ti == 0 || !t[ti - 1].is_alphanumeric();
        score += 1;
        if word_start {
            score += 8;
        }
        if ti == 0 {
            score += 6;
        }
        match prev {
            Some(p) if p + 1 == ti => score += 5,
            Some(p) => score -= ((ti - p) as i32).min(6),
            None => score -= (ti as i32).min(6),
        }
        prev = Some(ti);
        qi += 1;
    }
    (qi == q.len()).then_some(score)
}

/// The best score of `query` against a command's title and its extra words.
pub fn match_score(query: &str, label: &str, keywords: &[&str]) -> Option<i32> {
    std::iter::once(label)
        .chain(keywords.iter().copied())
        .enumerate()
        // A keyword counts a little less than the title.
        .filter_map(|(i, t)| fuzzy_score(query, t).map(|s| if i == 0 { s + 2 } else { s }))
        .max()
}

/// Indices of `items` (title, keywords) that match `query`, best first; with an empty query,
/// all of them in their order.
pub fn ranked(query: &str, items: &[(&str, Vec<&str>)]) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..items.len()).collect();
    }
    let mut hits: Vec<(i32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, (l, k))| match_score(query, l, k).map(|s| (s, i)))
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, i)| i).collect()
}

/// Extra words a command is found by (its group, its id's words, common synonyms).
pub fn keywords(c: &CmdSpec) -> Vec<&'static str> {
    let mut k = vec![c.group, c.id];
    k.extend(match c.id {
        "export" | "export-json" | "export-md" | "export-csv" => &["save", "download"][..],
        "compare" => &["diff", "regression", "gate"],
        "verify" => &["check", "hypothesis", "confirm"],
        "fix" => &["recommend", "tune", "settings"],
        "probe" => &["tls", "dns", "handshake", "ttfb"],
        "copy-link" => &["share", "url", "oprobe://"],
        "health" => &["home", "target", "dashboard"],
        "waterfall" => &["trace", "drilldown", "slowest"],
        "help" => &["keys", "shortcuts"],
        _ => &[],
    });
    k
}

/// The commands of `group` with their enabled states.
pub fn group(group: &str, x: &CmdCtx) -> Vec<(CmdSpec, bool)> {
    COMMANDS
        .iter()
        .filter(|c| c.group == group)
        .map(|c| (*c, enabled(c.id, x)))
        .collect()
}

/// A command by id.
pub fn spec(id: &str) -> Option<CmdSpec> {
    COMMANDS.iter().find(|c| c.id == id).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_has_the_v4_commands_once() {
        for id in [
            "verify",
            "open-run",
            "switch-target",
            "add-target",
            "fix",
            "export",
        ] {
            assert!(spec(id).is_some(), "{id}");
        }
        let mut ids: Vec<&str> = COMMANDS.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate command ids");
        let mut keys: Vec<&str> = COMMANDS.iter().filter_map(|c| c.keys).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate shortcuts");
    }

    #[test]
    fn fuzzy_matching_ranks_word_starts_and_prefixes_first() {
        assert!(fuzzy_score("exm", "Export Report as Markdown…").is_some());
        assert!(fuzzy_score("cmp", "Compare Runs").is_some());
        assert!(fuzzy_score("zzz", "Compare Runs").is_none());
        let items: Vec<(&str, Vec<&str>)> = COMMANDS
            .iter()
            .map(|c| (c.label, keywords(c)))
            .collect();
        let first = |q: &str| COMMANDS[ranked(q, &items)[0]].id;
        assert_eq!(first("compare"), "compare");
        assert_eq!(first("export csv"), "export-csv");
        assert_eq!(first("stop"), "stop");
        assert_eq!(first("save"), "export");
        assert_eq!(first("wf"), "waterfall");
        assert_eq!(ranked("", &items).len(), COMMANDS.len());
    }

    #[test]
    fn every_disabled_command_says_why() {
        let idle = CmdCtx::default();
        for c in COMMANDS {
            let why = disabled_reason(c.id, &idle);
            assert_eq!(why.is_some(), !enabled(c.id, &idle), "{}", c.id);
        }
        assert_eq!(disabled_reason("stop", &idle), Some("nothing is running"));
        let busy = CmdCtx {
            busy: true,
            has_run: true,
            ..idle
        };
        assert_eq!(
            disabled_reason("rerun", &busy),
            Some("a job is running: stop it first")
        );
        let shown = CmdCtx {
            has_run: true,
            ..idle
        };
        assert_eq!(
            disabled_reason("verify", &shown),
            Some("the shown run has no finding to verify")
        );
    }

    #[test]
    fn enabled_states_follow_the_context() {
        let idle = CmdCtx::default();
        assert!(enabled("new", &idle) && enabled("demo", &idle) && enabled("add-target", &idle));
        for id in [
            "stop",
            "rerun",
            "verify",
            "fix",
            "export",
            "compare",
            "open-run",
            "switch-target",
            "tune",
        ] {
            assert!(!enabled(id, &idle), "{id} with nothing to act on");
        }
        let shown = CmdCtx {
            has_run: true,
            has_verify: true,
            runs: 2,
            targets: 1,
            ..idle
        };
        for id in [
            "rerun",
            "verify",
            "fix",
            "export",
            "compare",
            "open-run",
            "switch-target",
            "tune",
        ] {
            assert!(enabled(id, &shown), "{id}");
        }
        assert!(!enabled(
            "verify",
            &CmdCtx {
                has_verify: false,
                ..shown
            }
        ));
        let busy = CmdCtx {
            busy: true,
            ..shown
        };
        assert!(enabled("stop", &busy));
        for id in ["run", "rerun", "verify", "fix", "demo", "tune", "new"] {
            assert!(!enabled(id, &busy), "{id} while a job runs");
        }
        // Reading the shown run stays possible while a job runs.
        assert!(enabled("export", &busy) && enabled("compare", &busy));
        let run_menu = group("Run", &busy);
        assert!(run_menu.iter().any(|(c, on)| c.id == "stop" && *on));
        assert!(run_menu.iter().any(|(c, on)| c.id == "verify" && !*on));
    }
}
