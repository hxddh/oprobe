//! The run history: list, show and remove stored runs (`oprobe show`).

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::{App, RunRef};
use crate::errors::{ErrorCode, OprobeError};
use crate::model::Run;
use crate::model::VerdictLevel;
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green, red, yellow};
use crate::report::terminal;
use crate::store::RunSummary;
use crate::util::fmt_ms;

/// One stored prove (`<id>.prove.json`).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ProveSummary {
    pub prove_id: String,
    /// The run whose operator was changed.
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
    pub objective: super::prove::ProveObjective,
    pub verdict: super::prove::ProveVerdict,
    /// The objective's relative change (estimate).
    pub objective_delta_ratio: f64,
    /// The change as `PATH=VALUE` pairs.
    pub change: Vec<String>,
}

/// Result of `oprobe show --list`: the runs you made, newest first, and the proves; prove
/// trials are hidden unless `--all` (U4).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunList {
    pub runs: Vec<RunSummary>,
    /// Runs listed in total before `-n` (the list may be limited).
    pub total_count: u64,
    /// Prove trial runs not listed (`--all` lists them).
    pub hidden_trials_count: u64,
    pub proves: Vec<ProveSummary>,
    /// Proves stored in total before `-n` (the list may be limited).
    pub proves_total_count: u64,
}

impl CommandOutput for RunList {
    fn next(&self) -> Vec<crate::output::NextStep> {
        match self.runs.first() {
            Some(r) => vec![crate::app::next::show(&r.id)],
            None => vec![crate::output::NextStep::new(
                "measure a local lab target",
                "oprobe run lab://demo -d 5s",
            )],
        }
    }

    fn text(&self) -> String {
        if self.runs.is_empty() {
            return format!(
                "no runs yet — try {} or {}\n",
                bold("oprobe demo"),
                bold("oprobe run memory://")
            );
        }
        let now = std::time::SystemTime::now();
        let rows: Vec<Vec<String>> = self
            .runs
            .iter()
            .map(|r| {
                let level = r.verdict_level.label();
                let verdict = match r.verdict_level {
                    VerdictLevel::Ok => green(level),
                    VerdictLevel::Review => level.to_owned(),
                    VerdictLevel::Attention => yellow(level),
                    VerdictLevel::Critical => red(level),
                };
                vec![
                    r.id.clone(),
                    r.target_name
                        .clone()
                        .unwrap_or_else(|| crate::util::display_url(&r.target_display)),
                    when(&r.id, now),
                    format!("{:.0}s", r.window_s),
                    if r.partial.is_some() {
                        format!("{verdict}*")
                    } else {
                        verdict
                    },
                    format!("{:.1}", r.ops_per_s),
                    fmt_ms(r.p99_ms),
                ]
            })
            .collect();
        let mut o = crate::report::style::table(
            &["run", "target", "when", "window", "verdict", "ops/s", "p99"],
            &rows,
            0,
        );
        if self.runs.iter().any(|r| r.partial.is_some()) {
            o += &dim("* stopped early (partial run)\n");
        }
        if !self.proves.is_empty() {
            o += "\n";
            let rows: Vec<Vec<String>> = self
                .proves
                .iter()
                .map(|p| {
                    vec![
                        p.prove_id.clone(),
                        p.run_id.clone(),
                        p.finding.clone().unwrap_or_else(|| "--with".into()),
                        format!("{:?}", p.verdict).to_lowercase(),
                        format!(
                            "{} {:+.1}%",
                            p.objective.label(),
                            p.objective_delta_ratio * 100.0
                        ),
                        // The whole change is in `show PROVE`: a long one would pad every
                        // row of the table to its width.
                        clip(&p.change.join(" "), CHANGE_WIDTH),
                    ]
                })
                .collect();
            o += &crate::report::style::table(
                &["prove", "run", "finding", "verdict", "objective", "change"],
                &rows,
                0,
            );
        }
        if self.hidden_trials_count > 0 {
            o += &dim(&format!(
                "{} trials of proves hidden (--all lists them)\n",
                self.hidden_trials_count
            ));
        }
        if self.total_count as usize > self.runs.len() {
            o += &dim(&format!(
                "{} of {} runs (-n/--limit)\n",
                self.runs.len(),
                self.total_count
            ));
        }
        if self.proves_total_count as usize > self.proves.len() {
            o += &dim(&format!(
                "{} of {} proves (-n/--limit)\n",
                self.proves.len(),
                self.proves_total_count
            ));
        }
        o += &dim("show one: oprobe show RUN  (id, prefix, last, last~1)\n");
        o
    }
}

/// The widest a prove's change prints in the list.
const CHANGE_WIDTH: usize = 48;

/// `s`, cut to `width` characters with an ellipsis when it is longer.
fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `3m ago`, `5h ago`, `2d ago` from a run id's timestamp.
fn when(id: &str, now: std::time::SystemTime) -> String {
    let Some(t) = crate::model::RunId::parse(id).and_then(|r| r.started_at()) else {
        return "?".into();
    };
    let s = now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

impl RunList {
    /// Keep the newest `n` runs and the newest `n` proves (`total_count` and
    /// `proves_total_count` stay the numbers stored).
    pub fn truncate(mut self, n: usize) -> Self {
        self.runs.truncate(n);
        self.proves.truncate(n);
        self
    }
}

/// The stored runs, newest first: the runs you made (and, with `all`, prove trials too),
/// and the proves.
pub fn list(app: &App, all: bool) -> Result<RunList> {
    let store = app.store();
    let every = store.list()?;
    let trials = every.iter().filter(|r| r.parent_id.is_some()).count() as u64;
    let runs: Vec<RunSummary> = every
        .into_iter()
        .filter(|r| all || r.parent_id.is_none())
        .collect();
    let proves: Vec<ProveSummary> = store
        .proves()?
        .into_iter()
        .filter_map(|v| {
            Some(ProveSummary {
                prove_id: v["prove_id"].as_str()?.to_owned(),
                run_id: v["run_id"].as_str()?.to_owned(),
                finding: v["finding"].as_str().map(str::to_owned),
                objective: serde_json::from_value(v["objective"].clone()).ok()?,
                verdict: serde_json::from_value(v["verdict"].clone()).ok()?,
                objective_delta_ratio: v["objective_delta"]["estimate"].as_f64().unwrap_or(0.0),
                change: serde_json::from_value(v["change"].clone()).unwrap_or_default(),
            })
        })
        .collect();
    Ok(RunList {
        total_count: runs.len() as u64,
        hidden_trials_count: if all { 0 } else { trials },
        runs,
        proves_total_count: proves.len() as u64,
        proves,
    })
}

/// A shown run (`oprobe show`): the full report, where it is stored, and its rows for the
/// renderers.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Profile {
    pub path: String,
    #[serde(flatten)]
    pub run: RunReportOnly,
    /// The full run with its trace rows (not serialized; renderers use it).
    #[serde(skip)]
    #[schemars(skip)]
    pub full: Option<Box<Run>>,
    /// `--detail` (text output).
    #[serde(skip)]
    #[schemars(skip)]
    pub detail: bool,
    /// `--by`: the request rows grouped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<ByTable>,
    /// `--since/--until`: the numbers of that time range, recomputed from the trace rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<crate::report::range::RangeView>,
}

/// `show --by`: the request rows grouped (and, by stage, the per-stage latency per API).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ByTable {
    pub by: crate::analysis::slice::By,
    pub groups: Vec<crate::analysis::slice::GroupRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<crate::analysis::slice::StageRow>,
}

impl ByTable {
    fn of(run: &Run, by: crate::analysis::slice::By, range: Option<(f64, f64)>) -> Self {
        use crate::analysis::slice;
        let inside = |t: i64| match range {
            Some((a, b)) => t >= (a * 1e6) as i64 && t < (b * 1e6) as i64,
            None => true,
        };
        let reqs: Vec<&crate::model::ReqRow> = run.reqs.iter().filter(|q| inside(q.t_us)).collect();
        let weights = run.op_weights();
        Self {
            by,
            groups: slice::group_weighted(&reqs, by, &weights),
            stages: if by == slice::By::Stage {
                slice::stage_table_weighted(&reqs, &weights)
            } else {
                Vec::new()
            },
        }
    }

    fn text(&self) -> String {
        let rows: Vec<Vec<String>> = self
            .groups
            .iter()
            .map(|g| {
                vec![
                    g.key.clone(),
                    g.requests_count.to_string(),
                    format!("{:.1}%", g.time_pct),
                    fmt_ms(g.p50_ms),
                    fmt_ms(g.p99_ms),
                    g.errors_count.to_string(),
                    g.throttled_count.to_string(),
                    g.cold_count.to_string(),
                ]
            })
            .collect();
        let mut o = crate::report::style::table(
            &[
                self.by.label(),
                "requests",
                "time",
                "p50",
                "p99",
                "errors",
                "throttled",
                "cold",
            ],
            &rows,
            0,
        );
        if !self.stages.is_empty() {
            let q = |s: &crate::analysis::slice::StageQ| {
                format!("{}/{}", fmt_ms(s.p50_ms), fmt_ms(s.p99_ms))
            };
            let rows: Vec<Vec<String>> = self
                .stages
                .iter()
                .map(|s| {
                    vec![
                        s.api.clone(),
                        s.requests_count.to_string(),
                        q(&s.queue),
                        q(&s.connect),
                        q(&s.send),
                        q(&s.ttfb),
                        q(&s.body),
                        q(&s.total),
                    ]
                })
                .collect();
            o += "\n";
            o += &crate::report::style::table(
                &[
                    "api", "requests", "queue", "connect", "send", "ttfb", "body", "total",
                ],
                &rows,
                0,
            );
            o += &dim("stage columns: p50/p99\n");
            // What the requests recorded beyond their stages: HTTP/2 open streams, the kernel's
            // upload timing, the CDN cache and the region.
            if self.stages.iter().any(|s| s.has_signals()) {
                let opt = |s: &Option<crate::analysis::slice::StageQ>| {
                    s.as_ref().map_or("-".to_owned(), &q)
                };
                let counts = |m: &std::collections::BTreeMap<String, u64>| {
                    if m.is_empty() {
                        "-".to_owned()
                    } else {
                        m.iter()
                            .map(|(k, n)| format!("{k}×{n}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                };
                let rows: Vec<Vec<String>> = self
                    .stages
                    .iter()
                    .map(|s| {
                        vec![
                            s.api.clone(),
                            s.streams_max_count
                                .map_or("-".to_owned(), |n| n.to_string()),
                            opt(&s.send_buffered),
                            opt(&s.upload_acked),
                            counts(&s.cache_status_count),
                            counts(&s.region_count),
                        ]
                    })
                    .collect();
                o += "\n";
                o += &crate::report::style::table(
                    &[
                        "api",
                        "streams",
                        "send buffered",
                        "upload acked",
                        "cache",
                        "region",
                    ],
                    &rows,
                    0,
                );
                o += &dim("request signals: p50/p99 over the requests that had them\n");
            }
        }
        o
    }
}

/// The report of a shown run.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunReportOnly {
    pub report: crate::model::Report,
    pub ops_count: u64,
    pub requests_count: u64,
}

impl CommandOutput for Profile {
    fn next(&self) -> Vec<crate::output::NextStep> {
        crate::app::next::after_run(&self.run.report)
    }

    fn text(&self) -> String {
        if let Some(b) = &self.by {
            let mut o = String::new();
            if let Some(v) = &self.range {
                o += &dim(&format!(
                    "{} of the {:.1} s window · {} ops · {} requests\n",
                    crate::report::terminal::range_span(v.since_s, v.until_s),
                    self.run.report.run.window_s,
                    v.ops_count,
                    v.requests_count
                ));
            }
            o += &b.text();
            return o;
        }
        if let (Some(v), Some(run)) = (&self.range, &self.full) {
            return terminal::range_section_with(run, v, self.detail);
        }
        let mut o = match &self.full {
            Some(run) => terminal::render_run_with(run, self.detail),
            None => terminal::render(&self.run.report),
        };
        // Where the run is: the file to hand a teammate.
        if !self.path.is_empty() {
            if !o.ends_with('\n') {
                o.push('\n');
            }
            o += &dim(&format!("file: {}\n", self.path));
        }
        o
    }
}

impl Profile {
    /// Render every section (`--detail`).
    pub fn with_detail(mut self, detail: bool) -> Self {
        self.detail = detail;
        self
    }

    /// Group the request rows (`--by`). Refused, like `--since`, on a run without request
    /// rows (a report `.json`, a trace saved without its rows): there is nothing to group.
    pub fn with_by(mut self, by: Option<crate::analysis::slice::By>) -> Result<Self> {
        let Some(b) = by else {
            self.by = None;
            return Ok(self);
        };
        let Some(run) = self
            .full
            .as_deref()
            .filter(|r| !r.reqs.is_empty() && kept_rows(r))
        else {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("this run has no request rows to group by {}", b.label()),
            )
            .with_hint(no_rows_hint(&self.run.report, "--by"))
            .into());
        };
        let range = self.range.as_ref().map(|v| (v.since_s, v.until_s));
        self.by = Some(ByTable::of(run, b, range));
        Ok(self)
    }

    /// Only the time range `[since, until)` (seconds into the window; `until` defaults to the
    /// window's end): its numbers, requests by API, verbs and slowest operations from the
    /// trace rows (`--since/--until`). Nothing changes when neither is given.
    pub fn with_range(mut self, since: Option<f64>, until: Option<f64>) -> Result<Self> {
        if since.is_none() && until.is_none() {
            return Ok(self);
        }
        let window = self.run.report.run.window_s;
        let (a, b) = (since.unwrap_or(0.0), until.unwrap_or(window));
        // `--since` past the window first: with no `--until` given, the end defaults to the
        // window's, and blaming an `--until` nobody passed confuses (K12).
        if a >= window {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("--since ({a} s) is past the end of the {window:.1} s measurement window"),
            )
            .with_hint(format!(
                "pass a --since under {window:.1}s (seconds into the window)"
            ))
            .into());
        }
        if b <= a {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("--until ({b} s) is not after --since ({a} s)"),
            )
            .with_hint("pass a range inside the window, e.g. --since 2s --until 6s")
            .into());
        }
        let Some(run) = self
            .full
            .as_deref()
            .filter(|r| !r.ops.is_empty() && kept_rows(r))
        else {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                "this run has no trace rows to recompute a time range from",
            )
            .with_hint(no_rows_hint(&self.run.report, "--since/--until"))
            .into());
        };
        self.range = Some(crate::report::range::RangeView::of(run, a, b));
        Ok(self)
    }
}

/// Whether `run` kept rows a range or a grouping can stand on. A trace saved without its rows
/// holds only the slowest operations rebuilt from its aggregates: their numbers would be those
/// of a handful of slow operations, not of the traffic.
fn kept_rows(run: &Run) -> bool {
    let s = &run.report.sampling;
    !(s.kept_ops_count == 0 && (s.seen_ops_count > 0 || run.report.run.sampled))
}

/// The hint of a refused `flag` on a run without rows.
fn no_rows_hint(r: &crate::model::Report, flag: &str) -> String {
    if r.is_trace() {
        format!(
            "the trace holds aggregates only: show it without {flag}, or save the next one with `Handle::save` (it keeps the rows)"
        )
    } else {
        format!("show the whole run without {flag}")
    }
}

/// A prove result as `oprobe show PROVE_ID` prints it: the stored result, rendered as `prove`
/// rendered it (JSON: the prove schema). Showing it is not a gate: exit 0.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ShownProve(pub super::prove::ProveOutput);

impl CommandOutput for ShownProve {
    fn text(&self) -> String {
        self.0.text()
    }
    fn next(&self) -> Vec<crate::output::NextStep> {
        self.0.next()
    }
}

/// The stored prove `r` names (its id, prefix, `last-prove` or `.prove.json` path), or `None`
/// when `r` names no prove. A prove whose run or trials were removed still shows: its result
/// is stored whole.
pub fn show_prove(app: &App, r: &RunRef) -> Result<Option<ShownProve>> {
    let Some(path) = app.store().resolve_prove(&r.0)? else {
        return Ok(None);
    };
    let raw = std::fs::read(&path)?;
    let out = super::prove::ProveOutput::from_json_slice(&raw).map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("prove result `{}` cannot be read: {e}", path.display()),
        )
        .with_hint(unreadable_prove_hint(app, &path))
    })?;
    Ok(Some(ShownProve(out)))
}

/// How to get rid of the unreadable prove result at `path`: `show --rm ID` only for a stored
/// prove named by its full id (a shorter stem or a file outside the store could prefix-match a
/// stored run, which `--rm` would delete instead); otherwise delete the file.
fn unreadable_prove_hint(app: &App, path: &std::path::Path) -> String {
    let id = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".prove.json"))
        .filter(|id| crate::model::RunId::parse(id).is_some());
    let in_store = path
        .parent()
        .and_then(|d| std::fs::canonicalize(d).ok())
        .zip(std::fs::canonicalize(app.store().dir()).ok())
        .is_some_and(|(a, b)| a == b);
    match id {
        Some(id) if in_store => format!("remove it with `oprobe show --rm {id}`"),
        _ => format!("delete the file `{}`", path.display()),
    }
}

/// Result of `oprobe show A B [...]`: two or more stored runs or `.oprobe` files compared
/// side by side (JSON: the `run` schema, [`super::run::RunSet`]).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Comparison {
    #[serde(flatten)]
    pub set: super::run::RunSet,
    #[serde(skip)]
    #[schemars(skip)]
    pub detail: bool,
}

impl CommandOutput for Comparison {
    fn next(&self) -> Vec<crate::output::NextStep> {
        self.set.runs.first().map_or_else(
            || vec![crate::app::next::list_runs()],
            |r| vec![crate::app::next::show(r.report.show_ref())],
        )
    }

    fn text(&self) -> String {
        let Some(cmp) = &self.set.comparison else {
            return self.set.text();
        };
        let same_workload = cmp.differences.iter().all(|d| !d.contains("workload"));
        let mut o = format!(
            "{} {} runs{}\n",
            bold("compare"),
            cmp.rows.len(),
            if same_workload {
                " · the same workload"
            } else {
                ""
            }
        );
        let rows: Vec<Vec<String>> = cmp
            .rows
            .iter()
            .map(|r| {
                vec![
                    r.run_id.clone(),
                    r.operator.clone(),
                    format!("{:.1}", r.goodput_ops_per_s),
                    fmt_ms(r.p50_ms),
                    fmt_ms(r.p99_ms),
                    format!("{:.2}%", r.throttled_request_ratio * 100.0),
                    r.failed_ops_count.to_string(),
                    r.top_finding.clone().unwrap_or_else(|| "—".into()),
                ]
            })
            .collect();
        o += &crate::report::style::table(
            &[
                "run",
                "operator",
                "ops/s",
                "p50",
                "p99",
                "throttled",
                "errors",
                "top finding",
            ],
            &rows,
            2,
        );
        if self.detail && !cmp.apis.is_empty() {
            let mut headers = vec!["api".to_owned()];
            for r in &cmp.rows {
                headers.push(format!("{} reqs / ttfb p99", r.run_id));
            }
            let h_refs: Vec<&str> = headers.iter().map(String::as_str).collect();
            let api_rows: Vec<Vec<String>> = cmp
                .apis
                .iter()
                .map(|a| {
                    let mut row = vec![a.api.clone()];
                    for (c, p99) in a.requests_count.iter().zip(&a.ttfb_p99_ms) {
                        if *c == 0 {
                            row.push("—".into());
                        } else {
                            row.push(format!("{c} / {}", fmt_ms(*p99)));
                        }
                    }
                    row
                })
                .collect();
            o += "\n";
            o += &crate::report::style::table(&h_refs, &api_rows, 2);
        }
        for d in &cmp.differences {
            o += &format!("  {}\n", dim(d));
        }
        for r in &self.set.runs {
            if let Some(f) = r.report.findings.first() {
                let next = f.remedy.next.clone().unwrap_or_default();
                o += &format!(
                    "  {} {}: {} {}\n",
                    dim("·"),
                    r.report.show_ref(),
                    f.cause,
                    dim(&next)
                );
            }
        }
        o += &dim("  each run: oprobe show RUN --detail\n");
        o
    }
}

/// Side-by-side comparison of two or more stored runs or `.oprobe` files (`oprobe show A B`).
pub fn compare(app: &App, refs: &[RunRef], detail: bool) -> Result<Comparison> {
    let mut runs = Vec::with_capacity(refs.len());
    for r in refs {
        let path = app.store().resolve(&r.0)?;
        let run = crate::store::runs::load_path(&path)?;
        runs.push(super::run::RunOutput {
            run_id: run.report.run.id.clone(),
            path: Some(path.display().to_string()),
            report: run.report.clone(),
            full: Some(Box::new(run)),
            breach: None,
        });
    }
    Ok(Comparison {
        set: super::run::RunSet::new(runs),
        detail,
    })
}

/// The run `r` names, with its rows.
pub fn show(app: &App, r: &RunRef) -> Result<Profile> {
    let path = app.store().resolve(&r.0)?;
    let run = crate::store::runs::load_path(&path)?;
    Ok(Profile {
        path: path.display().to_string(),
        run: RunReportOnly {
            report: run.report.clone(),
            ops_count: run.ops.len() as u64,
            requests_count: run.reqs.len() as u64,
        },
        by: None,
        full: Some(Box::new(run)),
        detail: false,
        range: None,
    })
}

/// Result of `oprobe show --rm`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RemovedRuns {
    pub removed: Vec<String>,
    pub dry_run: bool,
}

impl CommandOutput for RemovedRuns {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![crate::app::next::list_runs()]
    }

    fn text(&self) -> String {
        if self.removed.is_empty() {
            return "nothing to remove\n".into();
        }
        let verb = if self.dry_run {
            yellow("would remove")
        } else {
            "removed".into()
        };
        self.removed
            .iter()
            .map(|id| format!("  {verb} {id}\n"))
            .collect()
    }
}

/// The ids of the stored runs (or proves) `refs` name (a file outside the store is refused).
pub fn stored_ids(app: &App, refs: &[RunRef]) -> Result<Vec<String>> {
    let store = app.store();
    let mut ids: Vec<String> = Vec::new();
    // A run named twice (`show --rm X X`, an id and `last`) is one run: once in the result.
    let push = |ids: &mut Vec<String>, id: String| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };
    for RunRef(r) in refs {
        if let Some(p) = store.resolve_prove(r)? {
            if p.parent() != Some(store.dir()) {
                return Err(OprobeError::new(
                    ErrorCode::SafetyRefused,
                    format!("`{r}` is not a stored prove; delete files yourself"),
                )
                .into());
            }
            if let Some(id) = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".prove.json"))
            {
                push(&mut ids, id.to_owned());
                continue;
            }
        }
        let p = store.resolve(r)?;
        if p.parent() != Some(store.dir()) {
            return Err(OprobeError::new(
                ErrorCode::SafetyRefused,
                format!("`{r}` is not a stored run; delete files yourself"),
            )
            .into());
        }
        let id = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        push(&mut ids, id);
    }
    Ok(ids)
}

/// Delete the stored runs `refs` name; returns their ids.
pub fn rm(app: &App, refs: &[RunRef]) -> Result<RemovedRuns> {
    let ids = stored_ids(app, refs)?;
    let store = app.store();
    for id in &ids {
        // A prove goes with its trial runs.
        if !store.delete_prove(id)? {
            store.delete(id)?;
        }
    }
    Ok(RemovedRuns {
        removed: ids,
        dry_run: false,
    })
}

/// How long runs stay in the trash (`<data>/trash/`) before [`trash`] removes them for good.
pub const TRASH_KEEP: std::time::Duration = std::time::Duration::from_secs(7 * 86_400);

/// Runs moved to the trash by [`App::trash`]: their ids and the trash folder that holds their
/// files, which [`App::restore`] puts back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct TrashedRuns {
    pub ids: Vec<String>,
    /// The folder under `<data>/trash/` that holds their files.
    pub folder: std::path::PathBuf,
}

/// Move the stored runs `refs` name (a prove with its trial runs) into a new folder of
/// `<data>/trash/`; folders older than [`TRASH_KEEP`] go for good.
pub fn trash(app: &App, refs: &[RunRef]) -> Result<TrashedRuns> {
    let ids = stored_ids(app, refs)?;
    let root = app.paths.data_dir.join("trash");
    empty_trash(&root, TRASH_KEEP);
    let folder = root.join(crate::model::RunId::new().to_string());
    app.store().move_to(&ids, &folder)?;
    Ok(TrashedRuns { ids, folder })
}

/// Put runs [`trash`] moved back into the store.
pub fn restore(app: &App, t: &TrashedRuns) -> Result<()> {
    let root = app.paths.data_dir.join("trash");
    // A folder `trash` made: directly under `<data>/trash/`, named by a run id, no `..`.
    let named = t
        .folder
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| crate::model::RunId::parse(n).is_some());
    let plain = t.folder.components().all(|c| {
        !matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    });
    if !named || !plain || t.folder.parent() != Some(root.as_path()) {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!("`{}` is not a trash folder of oprobe", t.folder.display()),
        )
        .into());
    }
    app.store().restore_from(&t.folder)
}

/// Remove the trash folders under `root` older than `keep` (best effort).
fn empty_trash(root: &std::path::Path, keep: std::time::Duration) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > keep);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        let id = "20261120-143005-a1b2";
        let t = crate::model::RunId::parse(id)
            .unwrap()
            .started_at()
            .unwrap();
        let s = std::time::Duration::from_secs;
        assert_eq!(when(id, t + s(5)), "just now");
        assert_eq!(when(id, t + s(600)), "10m ago");
        assert_eq!(when(id, t + s(3 * 86_400)), "3d ago");
    }

    /// `show --rm X X` failed on the second X (the run was gone by then): a run named twice,
    /// by its id or another way, is removed once.
    #[test]
    fn a_run_named_twice_is_removed_once() {
        let d = tempfile::tempdir().unwrap();
        let app = App::new(crate::store::Paths {
            config_file: d.path().join("config.toml"),
            data_dir: d.path().join("data"),
        });
        let (a, b) = ("20261120-143005-a1b2", "20261120-143006-c3d4");
        for id in [a, b] {
            app.store()
                .save(&crate::store::runs::tests::sample_run(id, None))
                .unwrap();
        }
        let twice = [
            RunRef::from(a),
            RunRef::from(a),
            RunRef::from("20261120-143005"),
        ];
        assert_eq!(app.run_ids(&twice).unwrap(), [a]);
        let removed = rm(&app, &twice).unwrap();
        assert_eq!(removed.removed, [a]);
        let left: Vec<String> = app.list().unwrap().runs.into_iter().map(|r| r.id).collect();
        assert_eq!(left, [b]);
        // The trash takes a run named twice once too.
        let t = app.trash(&[RunRef::from(b), RunRef::from("last")]).unwrap();
        assert_eq!(t.ids, [b]);
    }

    /// `show --list` padded every row of the proves table to the longest `change` (300
    /// columns for a change of a dozen values): a long change is cut in the list.
    #[test]
    fn a_long_change_does_not_widen_the_list() {
        crate::report::style::set(false);
        let prove = |id: &str, change: Vec<String>| ProveSummary {
            prove_id: id.into(),
            run_id: "20261120-143005-a1b2".into(),
            finding: Some("throttling".into()),
            objective: crate::app::ProveObjective::Goodput,
            verdict: crate::app::ProveVerdict::Improved,
            objective_delta_ratio: 0.12,
            change,
        };
        let long: Vec<String> = (0..12)
            .map(|i| format!("options.some_long_option_{i}=a-rather-long-value-{i}"))
            .collect();
        let list = RunList {
            runs: vec![RunSummary::from(
                &crate::store::runs::tests::sample_run("20261120-143005-a1b2", None).report,
            )],
            total_count: 1,
            hidden_trials_count: 0,
            proves: vec![
                prove(
                    "20261120-150000-0001",
                    vec!["layers.retry.jitter=true".into()],
                ),
                prove("20261120-150100-0002", long),
            ],
            proves_total_count: 2,
        };
        let text = list.text();
        let widest = text.lines().map(|l| l.chars().count()).max().unwrap();
        assert!(widest <= 140, "{widest} columns:\n{text}");
        assert!(text.contains("layers.retry.jitter=true"), "{text}");
        assert!(
            text.contains("options.some_long_option_0=a-rather-long-value-…"),
            "{text}"
        );
        assert_eq!(clip("short", 48), "short");
        assert_eq!(clip("中文中文中文", 4), "中文中…");
    }

    #[test]
    fn trashed_runs_come_back() {
        let d = tempfile::tempdir().unwrap();
        let app = App::new(crate::store::Paths {
            config_file: d.path().join("config.toml"),
            data_dir: d.path().join("data"),
        });
        let store = app.store();
        for id in ["20261120-143005-a1b2", "20261120-143006-c3d4"] {
            store
                .save(&crate::store::runs::tests::sample_run(id, None))
                .unwrap();
        }
        let t = app.trash(&[RunRef::from("20261120-143005-a1b2")]).unwrap();
        assert_eq!(t.ids, ["20261120-143005-a1b2"]);
        assert!(t.folder.starts_with(d.path().join("data/trash")));
        let left: Vec<String> = app.list().unwrap().runs.into_iter().map(|r| r.id).collect();
        assert_eq!(left, ["20261120-143006-c3d4"]);
        // Undo: back in the list, the trash folder gone.
        app.restore(&t).unwrap();
        assert_eq!(app.list().unwrap().runs.len(), 2);
        assert!(!t.folder.exists());
        // A folder outside the trash is refused; an unknown run is an error.
        let bad = TrashedRuns {
            ids: vec![],
            folder: d.path().join("elsewhere"),
        };
        assert!(app.restore(&bad).is_err());
        // `<data>/trash/..` is the data dir: never restored from (it would be removed).
        for folder in [
            d.path().join("data/trash/.."),
            d.path().join("data/trash/notarun"),
        ] {
            let sneaky = TrashedRuns {
                ids: vec![],
                folder,
            };
            assert!(app.restore(&sneaky).is_err(), "{:?}", sneaky.folder);
        }
        assert!(d.path().join("data").exists());
        assert!(app.trash(&[RunRef::from("20261120-000000-ffff")]).is_err());
        // A trash of several runs that fails part-way moves nothing.
        assert!(
            app.trash(&[
                RunRef::from("20261120-143005-a1b2"),
                RunRef::from("20261120-000000-ffff"),
            ])
            .is_err()
        );
        assert_eq!(app.list().unwrap().runs.len(), 2);
    }
}
