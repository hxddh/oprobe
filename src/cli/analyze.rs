//! `oprobe analyze RUNREF`: re-slice a stored run — filters, group-by tables, per-stage p99,
//! time segments and outlier attribution.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::AnalyzeArgs;
use crate::analysis::outliers::{Attribution, Selection, attribute};
use crate::analysis::segments::{Segments, segments};
use crate::analysis::slice::{By, Filter, GroupRow, StageRow, group, stage_table};
use crate::errors::{ErrorCode, OprobeError};
use crate::model::ReqRow;
use crate::output::CommandOutput;
use crate::report::style::{bold, cyan, dim, table};
use crate::util::fmt_ms;

/// What was selected for outlier attribution.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AttributionOutput {
    /// `slowest 1%`, `window 5..10s`, …
    pub selection: String,
    #[serde(flatten)]
    pub attribution: Attribution,
}

/// Result of `oprobe analyze`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AnalyzeOutput {
    pub run_id: String,
    pub filter: Filter,
    /// Request rows after `--where` / `--window`.
    pub requests_count: u64,
    /// Request rows in the run file.
    pub total_requests_count: u64,
    pub by: String,
    pub groups: Vec<GroupRow>,
    /// Per-stage p50/p99 per API.
    pub stages: Vec<StageRow>,
    pub attribution: AttributionOutput,
    pub segments: Segments,
    /// The run's rows are sampled: counts are of the kept rows.
    pub sampled: bool,
}

impl CommandOutput for AnalyzeOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![
            crate::cli::next::explain(&self.run_id),
            crate::output::NextStep::new(
                "requests by status (failures)",
                format!(
                    "oprobe analyze {} --by status",
                    crate::model::spec::shell_quote(&self.run_id)
                ),
            )
            .with_mcp("analyze_run", serde_json::json!({ "run": self.run_id })),
            crate::cli::next::show(&self.run_id),
        ]
    }

    fn text(&self) -> String {
        let mut o = format!(
            "{} run {} · {} of {} requests{}\n",
            bold("analyze"),
            self.run_id,
            self.requests_count,
            self.total_requests_count,
            if self.sampled {
                dim(" (sampled rows)")
            } else {
                String::new()
            }
        );
        let _ = std::fmt::Write::write_fmt(
            &mut o,
            format_args!(
                "\n{}\n",
                bold(&cyan(&format!("BY {}", self.by.to_uppercase())))
            ),
        );
        let mut rows: Vec<Vec<String>> = self
            .groups
            .iter()
            .take(40)
            .map(|g| {
                vec![
                    g.key.clone(),
                    g.requests_count.to_string(),
                    g.errors_count.to_string(),
                    g.throttled_count.to_string(),
                    g.cold_count.to_string(),
                    fmt_ms(g.p50_ms),
                    fmt_ms(g.p99_ms),
                    fmt_ms(g.max_ms),
                    format!("{:.1}%", g.time_pct),
                ]
            })
            .collect();
        if self.groups.len() > 40 {
            rows.push(vec![format!("… {} more", self.groups.len() - 40)]);
        }
        o += &table(
            &[
                &self.by, "reqs", "err", "thr", "cold", "p50", "p99", "max", "time",
            ],
            &rows,
            2,
        );
        o += &format!("\n{}\n", bold(&cyan("STAGES p50 / p99")));
        let q = |s: &crate::analysis::slice::StageQ| {
            format!("{} / {}", fmt_ms(s.p50_ms), fmt_ms(s.p99_ms))
        };
        let srows: Vec<Vec<String>> = self
            .stages
            .iter()
            .map(|s| {
                vec![
                    s.api.clone(),
                    s.requests_count.to_string(),
                    q(&s.queue),
                    q(&s.connect),
                    q(&s.ttfb),
                    q(&s.body),
                    q(&s.total),
                ]
            })
            .collect();
        o += &table(
            &["api", "reqs", "queue", "connect", "ttfb", "body", "total"],
            &srows,
            2,
        );
        let a = &self.attribution;
        o += &format!(
            "\n{} {} ({} selected vs {} others)\n",
            bold(&cyan("OUTLIERS")),
            a.selection,
            a.attribution.selected_count,
            a.attribution.rest_count
        );
        if a.attribution.factors.is_empty() {
            o += &dim("  no attribute is significantly over-represented\n");
        }
        for f in a.attribution.factors.iter().take(8) {
            o += &format!(
                "  {:<10} {:<22} {:>5.1}× more likely ({:.0}% of selected vs {:.0}%)\n",
                f.attribute,
                f.value,
                f.lift,
                f.selected_ratio * 100.0,
                f.rest_ratio * 100.0
            );
        }
        let s = &self.segments;
        if s.segments.len() > 1 || s.start.is_some() {
            o += &format!("\n{}\n", bold(&cyan("TIME SEGMENTS")));
            for g in &s.segments {
                o += &format!(
                    "  {:>5.0}–{:<5.0}s {:>8.1} ops/s  p50 {:>8}  p99 {:>8}{}\n",
                    g.from_s,
                    g.to_s,
                    g.ops_per_s,
                    fmt_ms(g.p50_ms),
                    fmt_ms(g.p99_ms),
                    if g.throttled_count > 0 {
                        format!("  {} throttled", g.throttled_count)
                    } else {
                        String::new()
                    }
                );
            }
            if let (Some(st), Some(sd)) = (&s.start, &s.steady) {
                o += &dim(&format!(
                    "  first {:.0}s: p50 {} vs steady {}\n",
                    st.to_s,
                    fmt_ms(st.p50_ms),
                    fmt_ms(sd.p50_ms)
                ));
            }
        }
        o
    }
}

pub fn cmd_analyze(a: AnalyzeArgs, ctx: &Ctx) -> Result<u8> {
    let bad = |m: String| -> anyhow::Error {
        OprobeError::new(ErrorCode::UsageInvalidArgument, m).into()
    };
    let by = match a.by.as_deref() {
        Some(s) => By::parse(s).ok_or_else(|| {
            bad(format!(
                "invalid --by `{s}` (api, conn, remote, stage, second, status)"
            ))
        })?,
        None => By::Api,
    };
    let mut where_only = Filter::default();
    for w in &a.r#where {
        where_only.parse_where(w).map_err(bad)?;
    }
    let mut full = where_only.clone();
    if let Some(w) = &a.window {
        full.parse_window(w).map_err(bad)?;
    }
    if let Some(p) = a.slowest
        && !(p > 0.0 && p <= 100.0)
    {
        return Err(bad("--slowest must be a percentage in (0, 100]".into()));
    }
    let run = ctx.store().load(&a.run)?;
    if run.reqs.is_empty() {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("run {} has no request rows to analyze", run.id()),
        )
        .with_hint("tuner runs and report-only .json files keep no rows; analyze a run made by `oprobe run`")
        .into());
    }
    let filtered: Vec<&ReqRow> = full.apply(&run.reqs);
    // Attribution: the slowest PCT % of the filtered rows, or the window against the rest of
    // the run (same `--where`), or by default the slowest 1 %.
    let (rows, sel, label): (Vec<ReqRow>, Selection, String) = match (a.slowest, &a.window) {
        (Some(p), _) => {
            let rows: Vec<ReqRow> = filtered.iter().map(|r| (*r).clone()).collect();
            let sel = Selection::slowest(&rows, p);
            (rows, sel, format!("slowest {p}%"))
        }
        (None, Some(w)) => {
            let rows: Vec<ReqRow> = where_only.apply(&run.reqs).into_iter().cloned().collect();
            let sel = Selection {
                from_us: full.from_s.map(|s| (s * 1e6) as i64),
                to_us: full.to_s.map(|s| (s * 1e6) as i64),
                ..Default::default()
            };
            (rows, sel, format!("window {w}s vs the rest of the run"))
        }
        (None, None) => {
            let rows: Vec<ReqRow> = filtered.iter().map(|r| (*r).clone()).collect();
            let sel = Selection::slowest(&rows, 1.0);
            (rows, sel, "slowest 1%".into())
        }
    };
    let attribution = attribute(&rows, &sel);
    let ops: Vec<_> = if full.is_empty() {
        run.ops.clone()
    } else {
        let ids: std::collections::HashSet<u64> = filtered.iter().map(|r| r.op_id).collect();
        run.ops
            .iter()
            .filter(|o| ids.contains(&o.op_id))
            .cloned()
            .collect()
    };
    ctx.out.finish(&AnalyzeOutput {
        run_id: run.id().to_owned(),
        requests_count: filtered.len() as u64,
        total_requests_count: run.reqs.len() as u64,
        by: by.label().to_owned(),
        groups: group(&filtered, by),
        stages: stage_table(&filtered),
        attribution: AttributionOutput {
            selection: label,
            attribution,
        },
        segments: segments(&ops, run.report.run.window_s),
        sampled: run.report.run.sampled,
        filter: full,
    })
}
