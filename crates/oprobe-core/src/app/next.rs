//! Next steps shared by command results: each with its CLI command, built from real run ids
//! (no placeholders), and only real 6.0 commands (`run`, `prove`, `show`, `lab`).

use crate::model::spec::shell_quote;
use crate::output::NextStep;

/// `oprobe show RUN --detail`.
pub fn show(run: &str) -> NextStep {
    NextStep::new(
        "the full profile",
        format!("oprobe show {} --detail", shell_quote(run)),
    )
}

/// `oprobe show RUN --by stage`.
pub fn by_stage(run: &str) -> NextStep {
    NextStep::new(
        "where the time went, stage by stage",
        format!("oprobe show {} --by stage", shell_quote(run)),
    )
}

/// `oprobe run OPERATOR --dry-run`.
pub fn plan(operator: &str) -> NextStep {
    NextStep::new(
        "plan a run (nothing touches the operator's storage)",
        format!("oprobe run {} --dry-run", shell_quote(operator)),
    )
}

/// `oprobe show --list`.
pub fn list_runs() -> NextStep {
    NextStep::new("list stored runs", "oprobe show --list")
}

/// The steps after a finished run: the top finding's next command (`oprobe prove RUN CODE`),
/// the profile by stage, the details.
pub fn after_run(report: &crate::model::Report) -> Vec<NextStep> {
    let id = report.show_ref();
    let mut v = Vec::new();
    if let Some((f, next)) = report
        .findings
        .iter()
        .find_map(|f| f.remedy.next.as_ref().map(|n| (f, n)))
        && next.starts_with("oprobe prove")
    {
        v.push(NextStep::new(
            format!("prove the {} remedy with an A/B test", f.code),
            next.clone(),
        ));
    }
    v.push(by_stage(id));
    v.push(show(id));
    v
}
