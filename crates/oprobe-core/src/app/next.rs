//! Next steps shared by command results (design v4 §8, AG-3): each with its CLI command and
//! the MCP call doing the same, built from real run ids and targets (no placeholders).

use serde_json::json;

use crate::model::spec::shell_quote;
use crate::output::NextStep;

/// `oprobe explain RUN` / `explain_run`.
pub fn explain(run: &str) -> NextStep {
    NextStep::new(
        "explain the run: causes, evidence, advice",
        format!("oprobe explain {}", shell_quote(run)),
    )
    .with_mcp("explain_run", json!({ "run": run }))
}

/// `oprobe runs show RUN` / `get_run`.
pub fn show(run: &str) -> NextStep {
    NextStep::new(
        "the full report",
        format!("oprobe runs show {}", shell_quote(run)),
    )
    .with_mcp("get_run", json!({ "run": run }))
}

/// `oprobe analyze RUN --by api` / `analyze_run`.
pub fn analyze(run: &str) -> NextStep {
    NextStep::new(
        "attribute the slowest requests",
        format!("oprobe analyze {} --by api", shell_quote(run)),
    )
    .with_mcp("analyze_run", json!({ "run": run }))
}

/// `oprobe compare A B` / `compare_runs`.
pub fn compare(a: &str, b: &str) -> NextStep {
    NextStep::new(
        format!("compare {a} with {b}"),
        format!("oprobe compare {} {}", shell_quote(a), shell_quote(b)),
    )
    .with_mcp("compare_runs", json!({ "baseline": a, "candidate": b }))
}

/// `oprobe run --from RUN` / `start_run {from}`: the same run again.
pub fn replay(run: &str) -> NextStep {
    NextStep::new(
        "run it again (same spec)",
        format!("oprobe run --from {}", shell_quote(run)),
    )
    .with_mcp("start_run", json!({ "from": run }))
}

/// `oprobe fix TARGET` / `recommend_config`.
pub fn fix(target: &str) -> NextStep {
    NextStep::new(
        "find and verify a better client configuration",
        format!("oprobe fix {}", shell_quote(target)),
    )
    .with_mcp("recommend_config", json!({ "target": target }))
}

/// `oprobe doctor TARGET` / `doctor`.
pub fn doctor(target: &str) -> NextStep {
    NextStep::new(
        "check the target",
        format!("oprobe doctor {}", shell_quote(target)),
    )
    .with_mcp("doctor", json!({ "target": target }))
}

/// `oprobe run TARGET --dry-run` / `plan_run`.
pub fn plan(target: &str) -> NextStep {
    NextStep::new(
        "plan a run (nothing touches the target)",
        format!("oprobe run {} --dry-run", shell_quote(target)),
    )
    .with_mcp("plan_run", json!({ "target": target }))
}

/// `oprobe runs list` / `list_runs`.
pub fn list_runs() -> NextStep {
    NextStep::new("list stored runs", "oprobe runs list").with_mcp("list_runs", json!({}))
}

/// `oprobe target list` / `list_targets`.
pub fn list_targets() -> NextStep {
    NextStep::new("list configured targets", "oprobe target list")
        .with_mcp("list_targets", json!({}))
}

/// `oprobe target discover` / `discover_targets`.
pub fn discover() -> NextStep {
    NextStep::new(
        "credential sources on this machine",
        "oprobe target discover",
    )
    .with_mcp("discover_targets", json!({}))
}

/// The steps after a finished run: explain, attribute, verify the top finding, replay.
pub fn after_run(report: &crate::model::Report) -> Vec<NextStep> {
    let id = report.run.id.as_str();
    let mut v = vec![explain(id)];
    if let Some((code, ver)) = report
        .findings
        .iter()
        .find_map(|f| f.verification.as_ref().map(|v| (f.code.as_str(), v)))
    {
        v.push(
            NextStep::new(format!("verify: {}", ver.description), ver.command.clone())
                .with_mcp("verify_finding", json!({ "run": id, "code": code })),
        );
    }
    v.push(analyze(id));
    if !report.findings.is_empty() && report.tune.is_none() {
        v.push(fix(&report.spec.target_ref));
    }
    v.push(replay(id));
    v
}
