//! MCP resources: run reports, events, slowest operations and documents of stored runs;
//! schemas; documentation; presets.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::tools::State;
use crate::errors::{ErrorCode, OprobeError};
use crate::events::{Event, EventKind};
use crate::model::{OpRow, ReqRow, Run};

/// A resource template.
pub struct Template {
    pub uri_template: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
}

/// Every resource template, in a stable order.
pub const TEMPLATES: &[Template] = &[
    Template {
        uri_template: "oprobe://runs/{id}/report",
        name: "run-report",
        description: "Report v3 of a run (JSON, schema oprobe://schema/report)",
        mime_type: "application/json",
    },
    Template {
        uri_template: "oprobe://runs/{id}/events",
        name: "run-events",
        description: "Event stream of a run (NDJSON, oprobe.events/1); synthesized from the report for runs this server did not start",
        mime_type: "application/x-ndjson",
    },
    Template {
        uri_template: "oprobe://runs/{id}/slowest",
        name: "run-slowest",
        description: "The slowest operations of a run with their request rows (JSON)",
        mime_type: "application/json",
    },
    Template {
        uri_template: "oprobe://runs/{id}/html",
        name: "run-document",
        description: "Human-readable report of a run (Markdown until the HTML renderer lands)",
        mime_type: "text/markdown",
    },
    Template {
        uri_template: "oprobe://schema/{name}",
        name: "schema",
        description: "JSON Schema 2020-12 of a machine output (names: oprobe://schema/index)",
        mime_type: "application/schema+json",
    },
    Template {
        uri_template: "oprobe://docs/{topic}",
        name: "docs",
        description: "Documentation: getting-started, safety, findings, finding/{code}",
        mime_type: "text/markdown",
    },
];

/// A concrete resource for `resources/list`.
pub struct Listed {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: &'static str,
}

/// Fixed resources plus the newest stored runs' reports.
pub fn list(st: &State) -> Vec<Listed> {
    let mut v = vec![
        Listed {
            uri: "oprobe://docs/getting-started".into(),
            name: "getting-started".into(),
            description: "How to use oprobe through MCP safely".into(),
            mime_type: "text/markdown",
        },
        Listed {
            uri: "oprobe://docs/safety".into(),
            name: "safety".into(),
            description: "This server's safety rails: write access, budgets, allowed targets"
                .into(),
            mime_type: "text/markdown",
        },
        Listed {
            uri: "oprobe://docs/findings".into(),
            name: "findings".into(),
            description: "Every finding code: meaning, causes, how to verify, related knobs".into(),
            mime_type: "text/markdown",
        },
        Listed {
            uri: "oprobe://presets".into(),
            name: "presets".into(),
            description: "Workload presets with their operation mix and dataset".into(),
            mime_type: "application/json",
        },
        Listed {
            uri: "oprobe://schema/index".into(),
            name: "schemas".into(),
            description: "Names and $ids of every JSON Schema".into(),
            mime_type: "application/json",
        },
    ];
    if let Ok(runs) = st.cfg.paths.run_store().list() {
        for r in runs.into_iter().take(10) {
            v.push(Listed {
                uri: format!("oprobe://runs/{}/report", r.id),
                name: format!("run {}", r.id),
                description: format!("{}: {}", r.verdict_level.as_str(), r.headline),
                mime_type: "application/json",
            });
        }
    }
    v
}

/// The contents of a resource: text and its MIME type.
pub struct Contents {
    pub text: String,
    pub mime_type: &'static str,
}

fn not_found(uri: &str) -> anyhow::Error {
    OprobeError::new(
        ErrorCode::UsageInvalidArgument,
        format!("unknown resource `{uri}`"),
    )
    .with_hint("see resources/list and resources/templates/list")
    .into()
}

fn json(v: &impl Serialize) -> anyhow::Result<Contents> {
    Ok(Contents {
        text: serde_json::to_string_pretty(v)?,
        mime_type: "application/json",
    })
}

/// One slow operation with its requests.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SlowEntry {
    pub op: OpRow,
    pub requests: Vec<ReqRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Slowest {
    pub run_id: String,
    pub ops: Vec<SlowEntry>,
}

/// Events of a stored run, reconstructed from its report: `version`, one `interval` per
/// second, the findings and the `result`.
fn synthesized_events(run: &Run) -> Vec<Event> {
    let r = &run.report;
    let ts = r.run.ended_at.clone();
    let ev = |kind| Event {
        ts: ts.clone(),
        kind,
    };
    let mut v = vec![Event {
        ts: r.run.started_at.clone(),
        kind: Event::version().kind,
    }];
    for t in &r.timeline {
        v.push(ev(EventKind::Interval {
            t_s: t.t_s,
            ops_per_s: t.ops_per_s,
            mib_per_s: t.mib_per_s,
            p50_ms: t.p50_ms,
            p99_ms: t.p99_ms,
            errors: t.errors,
            throttled: t.throttled,
        }));
    }
    for f in &r.findings {
        v.push(ev(EventKind::Finding { finding: f.clone() }));
    }
    v.push(ev(EventKind::Result {
        run_id: Some(r.run.id.clone()),
        path: None,
        report: None,
        data: None,
    }));
    v
}

fn ndjson(events: &[Event]) -> anyhow::Result<Contents> {
    let mut s = String::new();
    for e in events {
        s += &serde_json::to_string(e)?;
        s.push('\n');
    }
    Ok(Contents {
        text: s,
        mime_type: "application/x-ndjson",
    })
}

/// Read a resource.
pub fn read(st: &State, uri: &str) -> anyhow::Result<Contents> {
    let rest = uri
        .strip_prefix("oprobe://")
        .ok_or_else(|| not_found(uri))?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["presets"] => json(&super::capabilities::Capabilities::current().presets),
        ["schema", "index"] | ["schema"] => json(&super::schemas::SchemaList::current()),
        ["schema", name] => {
            let name = name.trim_end_matches(".json");
            let v: Value = super::schemas::schema(name).ok_or_else(|| {
                OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("unknown schema `{name}`"),
                )
                .with_hint(format!("names: {}", super::schemas::names().join(", ")))
            })?;
            Ok(Contents {
                text: serde_json::to_string_pretty(&v)?,
                mime_type: "application/schema+json",
            })
        }
        ["docs", topic @ ..] => Ok(Contents {
            text: doc(st, &topic.join("/")).ok_or_else(|| not_found(uri))?,
            mime_type: "text/markdown",
        }),
        ["runs", id, what] => {
            if *what == "events"
                && let Some(job) = st.jobs.get(id)
            {
                return ndjson(&job.events());
            }
            if let Some(job) = st.jobs.get(id)
                && !job.status().is_final()
            {
                return Err(OprobeError::new(
                    ErrorCode::UsageUnknownRun,
                    format!("run `{id}` is still running"),
                )
                .with_hint(
                    "read oprobe://runs/{id}/events, or wait for get_run to report it finished",
                )
                .into());
            }
            let run = st.cfg.paths.run_store().load(id)?;
            match *what {
                "report" => json(&run.report),
                "events" => ndjson(&synthesized_events(&run)),
                "slowest" => json(&Slowest {
                    run_id: run.id().to_owned(),
                    ops: run
                        .report
                        .slowest
                        .iter()
                        .take(20)
                        .filter_map(|id| run.op(*id))
                        .map(|o| SlowEntry {
                            op: o.clone(),
                            requests: run.requests_of(o.op_id).into_iter().cloned().collect(),
                        })
                        .collect(),
                }),
                // TODO(html): serve the HTML report once the renderer exists.
                "html" => Ok(Contents {
                    text: crate::report::markdown::render_run(&run),
                    mime_type: "text/markdown",
                }),
                _ => Err(not_found(uri)),
            }
        }
        _ => Err(not_found(uri)),
    }
}

/// Markdown documentation topics.
pub fn doc(st: &State, topic: &str) -> Option<String> {
    match topic {
        "getting-started" | "" => Some(GETTING_STARTED.to_owned()),
        "safety" => Some(safety(st)),
        "findings" | "finding-codes" => Some(findings_md()),
        t => {
            let code = t.strip_prefix("finding/").or(t.strip_prefix("findings/"))?;
            super::catalog::get(code).map(|d| finding_md(&d))
        }
    }
}

const GETTING_STARTED: &str = "# oprobe over MCP

oprobe measures object storage (S3, GCS, Azure Blob, …) through Apache OpenDAL and traces every
request: queue, DNS/TCP/TLS, send, time-to-first-byte, transfer. Every run is stored and can be
explained, attributed and compared.

## Safe workflow

1. `list_targets` — configured targets, what this server allows, its budgets.
2. `doctor {target}` — reachability, TLS, clock, credentials, permissions.
3. `plan_run {target, …}` — dry run: requests, bytes, objects written, cost, ETA; `allowed`.
4. `start_run {target, duration, max_requests, …}` — returns a run id at once; pass a
   progressToken for progress notifications; poll `get_run {run}`.
5. `explain_run {run}` — verdict, findings with evidence, slowest-1% attribution, next steps.
6. `analyze_run {run, selection}` — why a time window or latency band is slow.
7. Change one knob, run again, `compare_runs {baseline, candidate}`.

Targets: a configured name, or a URL (`memory://` needs no credentials and never leaves the
process). `existing: PREFIX` benchmarks existing objects without writing anything.

Resources: `oprobe://runs/{id}/report|events|slowest|html`, `oprobe://schema/{name}`,
`oprobe://docs/findings`, `oprobe://presets`.
";

fn safety(st: &State) -> String {
    let c = &st.cfg;
    let b = &c.budgets;
    let fmt = |v: Option<String>| v.unwrap_or_else(|| "none".into());
    format!(
        "# Safety rails of this server\n\n\
         - writes: {}\n\
         - allowed targets: {}\n\
         - budgets per run: cost {} · requests {} · bytes {} · duration {}\n\
         - concurrent runs: {}\n\n\
         Runs that would upload a dataset or write objects are refused (`safety.read_only`) unless \
         the server was started with `--allow-write`; `memory://` and `existing: PREFIX` runs are \
         always allowed. A budget stops a run early (`safety.budget_exceeded`); the partial run is \
         still saved. Writes are confined to `oprobe-run-<id>/` under the target prefix and \
         removed when the run ends.\n",
        if c.allow_write {
            "allowed (--allow-write)"
        } else {
            "refused (read-only server)"
        },
        if c.allow_targets.is_empty() {
            "any".to_owned()
        } else {
            c.allow_targets.join(", ")
        },
        fmt(b.max_cost_usd.map(|v| format!("${v}"))),
        fmt(b.max_requests_count.map(|v| v.to_string())),
        fmt(b.max_bytes.map(crate::util::fmt_bytes)),
        fmt(b.max_duration_s.map(|v| format!("{v}s"))),
        c.max_concurrent_runs
    )
}

fn finding_md(d: &super::catalog::FindingDoc) -> String {
    let mut s = format!(
        "## `{}` — {}\n\n{} *({} · usually {})*\n\n",
        d.code,
        d.title,
        d.meaning,
        d.category,
        d.typical_severity.label()
    );
    let list = |s: &mut String, h: &str, items: &[&str]| {
        if !items.is_empty() {
            *s += &format!("**{h}**\n\n");
            for i in items {
                *s += &format!("- {i}\n");
            }
            s.push('\n');
        }
    };
    list(&mut s, "Typical causes", d.causes);
    list(&mut s, "How to verify", d.verify);
    list(&mut s, "Related knobs", d.knobs);
    s
}

/// Markdown of the whole catalog.
pub fn findings_md() -> String {
    let mut s = "# Finding codes\n\n".to_owned();
    for d in super::catalog::all() {
        s += &finding_md(&d);
    }
    s
}
