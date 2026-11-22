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

/// Result of `oprobe runs list`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunList {
    pub runs: Vec<RunSummary>,
    /// Runs stored in total (the list may be limited).
    pub total_count: u64,
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
                let verdict = match r.verdict_level {
                    VerdictLevel::Ok => green("ok"),
                    VerdictLevel::Review => "review".to_owned(),
                    VerdictLevel::Attention => yellow("attention"),
                    VerdictLevel::Critical => red("critical"),
                };
                vec![
                    r.id.clone(),
                    cut(r.label.as_deref().unwrap_or(""), 12),
                    cut(
                        &r.target_name
                            .clone()
                            .unwrap_or_else(|| crate::util::display_url(&r.target_display)),
                        22,
                    ),
                    when(&r.id, now),
                    format!("{:.0}s", r.window_s),
                    if r.interrupted {
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
            &[
                "run", "label", "target", "when", "window", "verdict", "ops/s", "p99",
            ],
            &rows,
            0,
        );
        if self.runs.iter().any(|r| r.interrupted) {
            o += &dim("* stopped early (partial run)\n");
        }
        if self.total_count as usize > self.runs.len() {
            o += &dim(&format!(
                "{} of {} runs (-n/--limit)\n",
                self.runs.len(),
                self.total_count
            ));
        }
        o += &dim("show one: oprobe show RUN  (id, prefix, last, last~1, label)\n");
        o
    }
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut o: String = s.chars().take(max - 1).collect();
        o.push('…');
        o
    }
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
    /// Keep the newest `n` runs (`total_count` stays the number stored).
    pub fn truncate(mut self, n: usize) -> Self {
        self.runs.truncate(n);
        self
    }
}

/// The stored runs, newest first.
pub fn list(app: &App) -> Result<RunList> {
    let runs = app.store().list()?;
    Ok(RunList {
        total_count: runs.len() as u64,
        runs,
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
    fn of(run: &Run, by: crate::analysis::slice::By) -> Self {
        use crate::analysis::slice;
        let reqs: Vec<&crate::model::ReqRow> = run.reqs.iter().collect();
        Self {
            by,
            groups: slice::group(&reqs, by),
            stages: if by == slice::By::Stage {
                slice::stage_table(&reqs)
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
            return b.text();
        }
        match &self.full {
            Some(run) => terminal::render_run_with(run, self.detail),
            None => terminal::render(&self.run.report),
        }
    }
}

impl Profile {
    /// Render every section (`--detail`).
    pub fn with_detail(mut self, detail: bool) -> Self {
        self.detail = detail;
        self
    }

    /// Group the request rows (`--by`).
    pub fn with_by(mut self, by: Option<crate::analysis::slice::By>) -> Self {
        self.by = match (by, &self.full) {
            (Some(b), Some(run)) => Some(ByTable::of(run, b)),
            _ => None,
        };
        self
    }
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

/// The ids of the stored runs `refs` name (a file outside the store is refused).
pub fn stored_ids(app: &App, refs: &[RunRef]) -> Result<Vec<String>> {
    let store = app.store();
    let mut ids = Vec::new();
    for RunRef(r) in refs {
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
        ids.push(id);
    }
    Ok(ids)
}

/// Delete the stored runs `refs` name; returns their ids.
pub fn rm(app: &App, refs: &[RunRef]) -> Result<RemovedRuns> {
    let ids = stored_ids(app, refs)?;
    let store = app.store();
    for id in &ids {
        store.delete(id)?;
    }
    Ok(RemovedRuns {
        removed: ids,
        dry_run: false,
    })
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
}
