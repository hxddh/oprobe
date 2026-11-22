//! Next steps shared by command results: each with its CLI command, built from real run ids
//! (no placeholders).

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

/// `oprobe prove RUN`.
pub fn prove(run: &str) -> NextStep {
    NextStep::new(
        "prove the remedy of the top finding with an A/B test",
        format!("oprobe prove {}", shell_quote(run)),
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

/// The steps after a finished run: prove the top finding, the profile by stage, the details.
pub fn after_run(report: &crate::model::Report) -> Vec<NextStep> {
    let id = report.run.id.as_str();
    let mut v = Vec::new();
    if !report.findings.is_empty() {
        v.push(prove(id));
    }
    v.push(by_stage(id));
    v.push(show(id));
    v
}
