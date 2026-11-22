//! CLI text in the App's prose (U12): findings' advice and hints are written for the terminal
//! ("`oprobe analyze RUN --by status`", "`[cost.pricing]`"). The App shows the sentence without
//! the command and gives each command a button — the in-app action that does the same, or a
//! copy of the command where the App has none.

/// What a command in a hint does in the App.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliAction {
    /// `oprobe analyze …`: the Requests tab, with a filter when the command slices by one.
    Requests(String),
    /// `oprobe verify …`: verify the finding.
    Verify,
    /// `oprobe tune …` / `oprobe fix …`.
    Tune,
    /// `oprobe compare …`.
    Compare,
    /// `oprobe explain …`: the Findings tab.
    Findings,
    /// `oprobe probe …`.
    Probe,
    /// Anything else: copy the command.
    Copy(String),
}

impl CliAction {
    /// The button's title.
    pub fn label(&self) -> &'static str {
        match self {
            CliAction::Requests(_) => "Show in Requests",
            CliAction::Verify => "Verify",
            CliAction::Tune => "Tune",
            CliAction::Compare => "Compare",
            CliAction::Findings => "Show Findings",
            CliAction::Probe => "Probe",
            CliAction::Copy(_) => "Copy Command",
        }
    }
}

/// The App's action for command `cmd`.
pub fn action(cmd: &str) -> CliAction {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let sub = words.get(1).copied().unwrap_or("");
    let flag = |name: &str| {
        words
            .iter()
            .position(|w| *w == name)
            .and_then(|i| words.get(i + 1))
            .map(|v| v.to_string())
    };
    match sub {
        "analyze" => CliAction::Requests(match flag("--by").as_deref() {
            Some("status") => "error".into(),
            Some("api") | Some("remote") | Some("conn") => String::new(),
            _ => flag("--filter").unwrap_or_default(),
        }),
        "verify" => CliAction::Verify,
        "tune" | "fix" => CliAction::Tune,
        "compare" => CliAction::Compare,
        "explain" => CliAction::Findings,
        "probe" => CliAction::Probe,
        _ => CliAction::Copy(cmd.to_owned()),
    }
}

/// Split hint text into its prose (commands and config sections replaced by words) and the
/// commands it names, in order.
pub fn split(text: &str) -> (String, Vec<String>) {
    let mut prose = String::new();
    let mut cmds = Vec::new();
    let mut rest = text;
    while let Some(a) = rest.find('`') {
        let Some(len) = rest[a + 1..].find('`') else {
            break;
        };
        let code = &rest[a + 1..a + 1 + len];
        prose.push_str(&rest[..a]);
        if code.starts_with("oprobe ") {
            cmds.push(code.to_owned());
            prose.push_str("(see below)");
        } else if let Some(sec) = code.strip_prefix('[').and_then(|c| c.strip_suffix(']')) {
            prose.push_str(&format!("the {} settings", sec.replace(['.', '_'], " ")));
        } else {
            prose.push_str(code);
        }
        rest = &rest[a + 1 + len + 1..];
    }
    prose.push_str(rest);
    // "Look at the failing APIs (see below)" reads fine; tidy what the removal left.
    let prose = prose
        .replace("((see below))", "(see below)")
        .replace(" (see below))", ")")
        .replace("  ", " ");
    (prose.trim().to_owned(), cmds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_leave_the_prose_and_become_actions() {
        let (p, c) =
            split("Look at the failing APIs and status codes (`oprobe analyze RUN --by status`)");
        assert_eq!(p, "Look at the failing APIs and status codes (see below)");
        assert_eq!(c, vec!["oprobe analyze RUN --by status"]);
        assert_eq!(action(&c[0]), CliAction::Requests("error".into()));
        let (p, c) = split("Set prices in `[cost.pricing]`");
        assert_eq!(p, "Set prices in the cost pricing settings");
        assert!(c.is_empty());
        assert_eq!(
            action("oprobe compat s3://b"),
            CliAction::Copy("oprobe compat s3://b".into())
        );
        assert_eq!(action("oprobe verify last").label(), "Verify");
        assert_eq!(split("no code here").0, "no code here");
        assert_eq!(split("odd `tick").0, "odd `tick");
    }
}
