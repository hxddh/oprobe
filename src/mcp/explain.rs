//! `oprobe explain RUNREF|FINDING_CODE`: narrate a stored run (verdict, top findings with
//! evidence and their documentation, outlier attribution of the slowest 1 %, next commands) or
//! document a finding code from the [catalog](super::catalog).

use schemars::JsonSchema;
use serde::Serialize;

use super::catalog::{self, FindingDoc};
use crate::analysis::diagnosis::{Bottleneck, Severity};
use crate::analysis::findings::Command;
use crate::analysis::outliers::{self, Attribution, Selection};
use crate::model::{Evidence, Run, VerdictLevel, Verification};
use crate::output::{CommandOutput, NextStep};
use crate::report::style::{bold, dim, yellow};
use crate::util::fmt_ms;

/// Share of the slowest requests `explain` attributes (percent).
pub const SLOWEST_PCT: f64 = 1.0;

/// A finding of the run with its documentation.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ExplainedFinding {
    pub code: String,
    pub severity: Severity,
    pub cause: String,
    pub evidence: Evidence,
    pub advice: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// Catalog entry of the code (absent for codes this oprobe does not document).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<FindingDoc>,
}

/// One of the slowest operations.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SlowOp {
    pub op_id: u64,
    pub verb: String,
    pub key: String,
    pub latency_ms: f64,
    pub attempts_count: u64,
    pub requests_count: u64,
    /// Stage that dominated its slowest request (`queue`, `connect`, `send`, `ttfb`,
    /// `transfer`, `stall`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dominant_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<crate::errors::ErrorCode>,
}

/// Outlier attribution of the slowest requests.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SlowestAnalysis {
    /// Percent of requests selected (by total time).
    pub slowest_pct: f64,
    pub selection: Selection,
    pub attribution: Attribution,
    pub ops: Vec<SlowOp>,
}

/// Narrated explanation of a stored run.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ExplainRun {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub target: String,
    pub level: VerdictLevel,
    pub headline: String,
    pub exit_code: u8,
    pub bottleneck: Bottleneck,
    pub interrupted: bool,
    /// All findings, most severe first.
    pub findings: Vec<ExplainedFinding>,
    /// Absent when the run file holds no request rows (e.g. tune runs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slowest: Option<SlowestAnalysis>,
    /// The explanation in sentences.
    pub narrative: Vec<String>,
    /// Steps worth taking next, each with its CLI command and MCP call (the JSON envelope's
    /// `next`).
    #[serde(skip)]
    #[schemars(skip)]
    pub next: Vec<NextStep>,
    /// Findings shown in text output.
    #[serde(skip)]
    #[schemars(skip)]
    pub top: usize,
}

/// Result of `oprobe explain`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ExplainOutput {
    /// Documentation of one finding code.
    FindingCode(FindingDoc),
    /// Every finding code (`--list`).
    FindingCodes { codes: Vec<FindingDoc> },
    /// A narrated run.
    Run(ExplainRun),
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

/// Human sentence for an attribution factor.
pub fn factor_sentence(f: &outliers::Factor) -> String {
    let what = match f.attribute.as_str() {
        "connection" => format!("{} connections", f.value),
        "attempt" if f.value == "retry" => "retried attempts".to_owned(),
        "stage" => format!("dominated by {}", f.value),
        "remote" => format!("served by {}", f.value),
        "error" if f.value == "none" => "without errors".to_owned(),
        a => format!("{a}={}", f.value),
    };
    format!(
        "{} ({} of the slow vs {} of the rest, {:.1}× lift)",
        what,
        pct(f.selected_ratio),
        pct(f.rest_ratio),
        f.lift
    )
}

/// Analyze the slowest `pct` percent of a run's requests.
pub fn slowest(run: &Run, pct: f64) -> Option<SlowestAnalysis> {
    if run.reqs.is_empty() {
        return None;
    }
    let selection = Selection::slowest(&run.reqs, pct);
    let attribution = outliers::attribute(&run.reqs, &selection);
    let ops = run
        .report
        .slowest
        .iter()
        .take(5)
        .filter_map(|id| run.op(*id))
        .map(|o| {
            let reqs = run.requests_of(o.op_id);
            let worst = reqs.iter().max_by_key(|r| r.total_us);
            SlowOp {
                op_id: o.op_id,
                verb: o.verb.to_string(),
                key: o.key.clone(),
                latency_ms: o.latency_us as f64 / 1000.0,
                attempts_count: o.attempts as u64,
                requests_count: o.requests as u64,
                dominant_stage: worst.map(|r| outliers::dominant_stage(r).to_owned()),
                error_code: o.error_code,
            }
        })
        .collect();
    Some(SlowestAnalysis {
        slowest_pct: pct,
        selection,
        attribution,
        ops,
    })
}

/// Explain a loaded run.
pub fn explain_run(run: &Run, top: usize) -> ExplainRun {
    let r = &run.report;
    let id = r.run.id.clone();
    let target = r
        .target
        .name
        .clone()
        .unwrap_or_else(|| r.target.display.clone());
    let mut findings: Vec<ExplainedFinding> = r
        .findings
        .iter()
        .map(|f| ExplainedFinding {
            code: f.code.clone(),
            severity: f.severity,
            cause: f.cause.clone(),
            evidence: f.evidence.clone(),
            advice: f.advice.clone(),
            verification: f.verification.clone(),
            doc: catalog::get(&f.code),
        })
        .collect();
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    let slow = slowest(run, SLOWEST_PCT);

    let mut narrative = vec![format!(
        "Run {id} against {target}: verdict {} — {}",
        r.verdict.level.as_str(),
        r.verdict.headline
    )];
    if r.run.interrupted {
        narrative.push(
            "The run stopped early (Ctrl-C or a budget); numbers cover only the part that ran."
                .into(),
        );
    }
    if findings.is_empty() {
        narrative.push("The diagnosis found nothing worth changing.".into());
    }
    for f in findings.iter().take(top.max(1)) {
        let mut s = format!("[{}] {}.", f.code, f.cause.trim_end_matches('.'));
        if !f.evidence.text.is_empty() {
            s += &format!(" Evidence: {}.", f.evidence.text.trim_end_matches('.'));
        }
        if let Some(d) = &f.doc {
            s += &format!(" Meaning: {}", d.meaning);
        }
        s += &format!(" Advice: {}", f.advice);
        narrative.push(s);
    }
    match &slow {
        Some(a) if !a.attribution.factors.is_empty() => {
            let min = a.selection.min_total_us.unwrap_or(0) as f64 / 1000.0;
            let factors: Vec<String> = a
                .attribution
                .factors
                .iter()
                .take(3)
                .map(factor_sentence)
                .collect();
            narrative.push(format!(
                "The slowest {}% of requests ({} requests, ≥ {}) are mostly {}.",
                a.slowest_pct,
                a.attribution.selected_count,
                fmt_ms(min),
                factors.join("; ")
            ));
        }
        Some(a) => narrative.push(format!(
            "The slowest {}% of requests ({}) share no distinctive attribute: the tail looks like ordinary service variance.",
            a.slowest_pct, a.attribution.selected_count
        )),
        None => narrative.push(
            "The run file has no request rows (tune runs keep none), so outliers cannot be attributed."
                .into(),
        ),
    }

    let target_ref = r.spec.target_ref.clone();
    let mut next = vec![
        NextStep::new("the full report", format!("oprobe runs show {id}")).with_mcp(
            "get_run",
            serde_json::json!({ "run": id, "include_report": true }),
        ),
    ];
    for f in findings.iter().take(top.max(1)) {
        if let Some(v) = &f.verification {
            next.push(NextStep::new(
                format!("test it: {}", v.description),
                v.rerun.clone(),
            ));
        }
        let step = match f.code.as_str() {
            "throttling" | "sla_p99" | "server_latency" | "tail_latency" | "tail_stragglers" => {
                Some(
                    NextStep::new(
                        "plan a tuner search",
                        Command::for_target("tune", &r.target)
                            .set("--dry-run", None)
                            .to_string(),
                    )
                    .with_mcp("plan_run", serde_json::json!({ "target": target_ref })),
                )
            }
            "errors" | "cold_connections" | "high_rtt" | "tcp_retransmits" => Some(
                NextStep::new(
                    "check the target",
                    Command::for_target("doctor", &r.target).to_string(),
                )
                .with_mcp("doctor", serde_json::json!({ "target": target_ref })),
            ),
            _ => None,
        };
        if let Some(s) = step
            && !next.iter().any(|n| n.cli == s.cli)
        {
            next.push(s);
        }
    }
    next.push(
        NextStep::new("requests by API", format!("oprobe analyze {id} --by api"))
            .with_mcp("analyze_run", serde_json::json!({ "run": id })),
    );
    next.push(
        NextStep::new(
            "compare with the previous run",
            format!("oprobe compare last~1 {id}"),
        )
        .with_mcp(
            "compare_runs",
            serde_json::json!({ "baseline": "last~1", "candidate": id }),
        ),
    );

    ExplainRun {
        run_id: id,
        label: r.run.label.clone(),
        target,
        level: r.verdict.level,
        headline: r.verdict.headline.clone(),
        exit_code: r.verdict.exit_code,
        bottleneck: r.verdict.bottleneck,
        interrupted: r.run.interrupted,
        findings,
        slowest: slow,
        narrative,
        next,
        top,
    }
}

fn wrap(s: &str, indent: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for w in s.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + w.chars().count() > width {
            out += &format!("{indent}{line}\n");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line += w;
    }
    if !line.is_empty() {
        out += &format!("{indent}{line}\n");
    }
    out
}

fn doc_text(d: &FindingDoc) -> String {
    let mut o = format!(
        "{} {}  {}\n",
        bold(d.code),
        d.title,
        dim(&format!(
            "({} · usually {})",
            d.category,
            d.typical_severity.label()
        ))
    );
    o += &wrap(d.meaning, "  ", 76);
    let list = |o: &mut String, head: &str, items: &[&str]| {
        if items.is_empty() {
            return;
        }
        *o += &format!("{}\n", bold(head));
        for i in items {
            *o += &wrap(&format!("· {i}"), "  ", 76);
        }
    };
    list(&mut o, "Typical causes", d.causes);
    list(&mut o, "How to verify", d.verify);
    list(&mut o, "Related knobs", d.knobs);
    o += &dim(&format!("{}\n", d.docs));
    o
}

impl CommandOutput for ExplainRun {
    fn text(&self) -> String {
        ExplainOutput::Run(self.clone()).text()
    }
    fn next(&self) -> Vec<NextStep> {
        self.next.clone()
    }
}

impl CommandOutput for ExplainOutput {
    fn next(&self) -> Vec<NextStep> {
        match self {
            Self::Run(e) => e.next.clone(),
            _ => Vec::new(),
        }
    }
    fn text(&self) -> String {
        match self {
            Self::FindingCode(d) => doc_text(d),
            Self::FindingCodes { codes } => {
                let w = codes.iter().map(|d| d.code.len()).max().unwrap_or(0);
                let mut o = String::new();
                for d in codes {
                    o += &format!("{:<w$}  {:<12} {}\n", d.code, d.category, d.title);
                }
                o += &dim("details: oprobe explain CODE\n");
                o
            }
            Self::Run(e) => {
                let mut o = format!(
                    "{} {}\n",
                    bold(&format!("{} · {}", e.run_id, e.level.as_str())),
                    e.headline
                );
                if e.interrupted {
                    o += &yellow("  partial: the run was stopped early\n");
                }
                for f in e.findings.iter().take(e.top.max(1)) {
                    o += &format!(
                        "  {} {} {}\n",
                        crate::report::terminal::severity_icon(f.severity),
                        bold(&f.code),
                        f.cause
                    );
                    if !f.evidence.text.is_empty() {
                        o += &format!("    {}\n", dim(&f.evidence.text));
                    }
                    o += &format!("    → {}\n", f.advice);
                }
                if let Some(s) = &e.slowest {
                    o += &format!(
                        "  slowest {}% ({} requests):\n",
                        s.slowest_pct, s.attribution.selected_count
                    );
                    if s.attribution.factors.is_empty() {
                        o += &dim("    no distinctive attribute\n");
                    }
                    for f in s.attribution.factors.iter().take(3) {
                        o += &format!("    · {}\n", factor_sentence(f));
                    }
                }
                o += &format!("  {}\n", bold("next:"));
                for c in e.next.iter().take(4) {
                    o += &format!("    {}\n", c.cli);
                }
                o
            }
        }
    }
}

/// `explain SUBJECT`: a finding code when the catalog knows it, else a RUNREF.
pub fn explain_subject(
    store: &crate::store::RunStore,
    subject: &str,
    top: usize,
) -> anyhow::Result<ExplainOutput> {
    if let Some(d) = catalog::get(subject) {
        return Ok(ExplainOutput::FindingCode(d));
    }
    match store.resolve(subject) {
        Ok(path) => {
            let run = crate::store::runs::load_path(&path)?;
            Ok(ExplainOutput::Run(explain_run(&run, top)))
        }
        Err(e) => {
            let looks_like_code = subject
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
                && !subject.starts_with("last");
            let sugg = catalog::suggest(subject);
            if looks_like_code && !sugg.is_empty() {
                return Err(crate::errors::OprobeError::new(
                    crate::errors::ErrorCode::UsageInvalidArgument,
                    format!("`{subject}` is neither a run nor a finding code"),
                )
                .with_hint(format!("did you mean: {}", sugg.join(", ")))
                .into());
            }
            Err(e.context(format!(
                "`{subject}` is neither a finding code (oprobe explain --list) nor a stored run"
            )))
        }
    }
}
