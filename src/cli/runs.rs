//! `oprobe runs list|show|rm|gc` (`compare` lives in `compare.rs`).

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use crate::errors::{ErrorCode, OprobeError};
use crate::model::Run;
use crate::output::CommandOutput;
use crate::report::diff::DiffReport;
use crate::report::style::{bold, dim, yellow};
use crate::report::{markdown, terminal};
use crate::store::RunSummary;
use crate::util::fmt_ms;

/// Result of `oprobe runs list`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunsList {
    pub runs: Vec<RunSummary>,
    /// Runs stored in total (the list may be limited).
    pub total_count: u64,
}

impl CommandOutput for RunsList {
    fn text(&self) -> String {
        if self.runs.is_empty() {
            return format!("no runs yet — try {}\n", bold("oprobe run memory://"));
        }
        let rows: Vec<Vec<String>> = self
            .runs
            .iter()
            .map(|r| {
                vec![
                    r.id.clone(),
                    r.verdict_level.as_str().to_owned(),
                    r.label.clone().unwrap_or_default(),
                    r.target_name
                        .clone()
                        .unwrap_or_else(|| crate::util::display_url(&r.target_display)),
                    format!("{:.1}", r.ops_per_s),
                    fmt_ms(r.p99_ms),
                    format!("{:.2}%", r.error_ratio * 100.0),
                ]
            })
            .collect();
        let mut o = crate::report::style::table(
            &[
                "run", "verdict", "label", "target", "ops/s", "p99", "errors",
            ],
            &rows,
            0,
        );
        if self.total_count as usize > self.runs.len() {
            o += &dim(&format!(
                "{} of {} runs (--limit)\n",
                self.runs.len(),
                self.total_count
            ));
        }
        o
    }
}

pub fn cmd_list(a: RunsListArgs, ctx: &Ctx) -> Result<u8> {
    let mut runs = ctx.store().list()?;
    let total = runs.len() as u64;
    runs.truncate(a.limit);
    ctx.out.finish(&RunsList {
        runs,
        total_count: total,
    })
}

/// Result of `oprobe runs show`: the full report (and where it is stored).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunShow {
    pub path: String,
    #[serde(flatten)]
    pub run: RunReportOnly,
    /// The full run with its trace rows (not serialized; renderers use it).
    #[serde(skip)]
    #[schemars(skip)]
    pub full: Option<Box<Run>>,
}

/// The report of a shown run.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunReportOnly {
    pub report: crate::model::Report,
    pub ops_count: u64,
    pub requests_count: u64,
}

impl CommandOutput for RunShow {
    fn text(&self) -> String {
        match &self.full {
            Some(run) => terminal::render_run(run),
            None => terminal::render(&self.run.report),
        }
    }
    fn markdown(&self) -> String {
        match &self.full {
            Some(run) => markdown::render_run(run),
            None => markdown::render(&self.run.report),
        }
    }
}

pub fn cmd_show(a: RunRefArg, ctx: &Ctx) -> Result<u8> {
    let store = ctx.store();
    let path = store.resolve(&a.run)?;
    let run = crate::store::runs::load_path(&path)?;
    ctx.out.finish(&RunShow {
        path: path.display().to_string(),
        run: RunReportOnly {
            report: run.report.clone(),
            ops_count: run.ops.len() as u64,
            requests_count: run.reqs.len() as u64,
        },
        full: Some(Box::new(run)),
    })
}

/// Result of `oprobe runs rm` / `runs gc`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RemovedRuns {
    pub removed: Vec<String>,
    pub dry_run: bool,
}

impl CommandOutput for RemovedRuns {
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

pub fn cmd_rm(a: RunsRmArgs, ctx: &Ctx) -> Result<u8> {
    let store = ctx.store();
    let mut ids = Vec::new();
    for r in &a.runs {
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
    for id in &ids {
        store.delete(id)?;
    }
    ctx.out.finish(&RemovedRuns {
        removed: ids,
        dry_run: false,
    })
}

pub fn cmd_gc(a: RunsGcArgs, ctx: &Ctx) -> Result<u8> {
    let older = a
        .older_than
        .as_deref()
        .map(crate::util::parse_duration)
        .transpose()?;
    let protect = crate::store::Baselines::load(&ctx.paths.baselines_file())?.referenced();
    let store = ctx.store();
    let ids = store.gc_candidates(a.keep, older, &protect)?;
    if !a.dry_run {
        for id in &ids {
            store.delete(id)?;
        }
    }
    ctx.out.finish(&RemovedRuns {
        removed: ids,
        dry_run: a.dry_run,
    })
}

impl CommandOutput for DiffReport {
    fn text(&self) -> String {
        self.render_terminal()
    }
    fn markdown(&self) -> String {
        self.render_markdown()
    }
    fn exit_code(&self) -> u8 {
        if self.passed {
            0
        } else {
            crate::errors::exit::GATE
        }
    }
}
