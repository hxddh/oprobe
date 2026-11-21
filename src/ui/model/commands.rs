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
        "export" | "export-json" | "export-md" | "export-csv" | "slowest" => x.has_run,
        "compare" => x.runs >= 2,
        "open-run" => x.runs > 0,
        "switch-target" => x.targets > 0,
        _ => true,
    }
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
