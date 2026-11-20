//! `oprobe compare` (repeat-aware regression gate) and `oprobe baseline set|list|rm|show`.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use crate::analysis::compare::{Comparison, compare};
use crate::analysis::stats::{self, Interval};
use crate::errors::{ErrorCode, OprobeError};
use crate::model::Run;
use crate::output::CommandOutput;
use crate::report::diff::Thresholds;
use crate::report::style::{bold, dim, red};
use crate::report::write_file;
use crate::store::{Baseline, Baselines, RunSummary};
use crate::util::fmt_ms;

impl CommandOutput for Comparison {
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

pub(super) fn thresholds(a: &CompareArgs) -> Thresholds {
    Thresholds {
        p99_pct: a.max_p99_increase,
        p50_pct: a.max_p50_increase,
        min_latency_delta_ms: a.min_latency_delta_ms,
        throughput_drop_pct: a.max_throughput_drop,
        error_rate_pp: a.max_error_increase,
        request_amp_pct: a.max_request_amp_increase,
        read_amp_pct: a.max_read_amp_increase,
        cost_pct: a.max_cost_increase,
        fail_on_new_throttling: !a.allow_throttling,
        min_samples: a.min_samples,
        allow_workload_change: a.allow_workload_change,
        allow_target_change: a.allow_target_change,
    }
}

/// Load one side: a RUNREF, a comma-separated list of RUNREFs, or `all:LABEL` (every stored run
/// with that label, e.g. a `--repeat` group).
pub(super) fn load_side(ctx: &Ctx, spec: &str) -> Result<Vec<Run>> {
    let store = ctx.store();
    if let Some(label) = spec.strip_prefix("all:") {
        let ids: Vec<String> = store
            .list()?
            .into_iter()
            .filter(|s| s.label.as_deref() == Some(label))
            .map(|s| s.id)
            .collect();
        if ids.is_empty() {
            return Err(OprobeError::new(
                ErrorCode::UsageUnknownRun,
                format!("no stored run is labelled `{label}`"),
            )
            .into());
        }
        return ids.iter().map(|id| store.load(id)).collect();
    }
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|r| store.load(r))
        .collect()
}

/// The runs of a named baseline (oldest first); missing runs are skipped with a warning.
pub(super) fn baseline_runs(ctx: &Ctx, name: &str) -> Result<Vec<Run>> {
    let b = Baselines::load(&ctx.paths.baselines_file())?;
    let Some(bl) = b.get(name) else {
        let known: Vec<&str> = b.baselines.keys().map(String::as_str).collect();
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("no baseline named `{name}`"),
        )
        .with_hint(if known.is_empty() {
            "create one with `oprobe baseline set NAME RUNREF…` or `oprobe run … --baseline NAME`"
                .to_owned()
        } else {
            format!("baselines: {}", known.join(", "))
        })
        .into());
    };
    let store = ctx.store();
    let mut runs = Vec::new();
    for id in &bl.runs {
        match store.load(id) {
            Ok(r) => runs.push(r),
            Err(_) => ctx
                .out
                .warn(format!("baseline `{name}`: run {id} is no longer stored")),
        }
    }
    if runs.is_empty() {
        return Err(OprobeError::new(
            ErrorCode::UsageUnknownRun,
            format!("baseline `{name}` has no stored runs"),
        )
        .into());
    }
    Ok(runs)
}

/// Write `--markdown` / `--junit` / `--json` files of a comparison.
pub(super) fn write_compare_files(c: &Comparison, a: &CompareArgs) -> Result<()> {
    if let Some(p) = &a.markdown {
        write_file(p, &c.render_markdown())?;
    }
    if let Some(p) = &a.junit {
        write_file(p, &c.render_junit())?;
    }
    if let Some(p) = &a.json {
        write_file(p, &serde_json::to_string_pretty(c)?)?;
    }
    Ok(())
}

pub fn cmd_compare(a: CompareArgs, ctx: &Ctx) -> Result<u8> {
    let (base_label, base) = match (&a.baseline_name, &a.baseline) {
        (Some(name), _) => (format!("baseline {name}"), baseline_runs(ctx, name)?),
        (None, Some(r)) => (r.clone(), load_side(ctx, r)?),
        (None, None) => {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                "compare needs a baseline: `oprobe compare A [B]` or `oprobe compare --baseline NAME [B]`",
            )
            .with_hint("e.g. `oprobe compare last~1 last`, `oprobe compare all:main all:pr-42`")
            .into());
        }
    };
    // With `--baseline NAME` the first positional is the candidate.
    let cand_ref = if a.baseline_name.is_some() {
        a.candidate.clone().or_else(|| a.baseline.clone())
    } else {
        a.candidate.clone()
    }
    .unwrap_or_else(|| "last".into());
    let cand = load_side(ctx, &cand_ref)?;
    // A run never compares against itself.
    let base: Vec<Run> = base
        .into_iter()
        .filter(|b| !cand.iter().any(|c| c.id() == b.id()))
        .collect();
    if base.is_empty() {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            "the baseline and the candidate are the same runs",
        )
        .into());
    }
    let c = compare(&base, &cand, &base_label, &cand_ref, &thresholds(&a));
    write_compare_files(&c, &a)?;
    if !c.passed && ctx.out.format == crate::output::OutputFormat::Text {
        ctx.out.info(red(&bold("regression gate failed (exit 3)")));
    }
    ctx.out.finish(&c)
}

// ---------------------------------------------------------------------------------------
// baseline set|list|rm|show
// ---------------------------------------------------------------------------------------

/// Spread of a set of runs: medians across runs with 95 % bootstrap intervals.
#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub struct RunSetStats {
    pub runs_count: u64,
    pub p50_ms: Interval,
    pub p99_ms: Interval,
    pub ops_per_s: Interval,
    pub error_ratio: Interval,
}

impl RunSetStats {
    pub fn of(runs: &[RunSummary]) -> Self {
        let col = |f: &dyn Fn(&RunSummary) -> f64, seed: u64| {
            let v: Vec<f64> = runs.iter().map(f).collect();
            stats::bootstrap(&v, stats::med, seed)
        };
        Self {
            runs_count: runs.len() as u64,
            p50_ms: col(&|r| r.p50_ms, 1),
            p99_ms: col(&|r| r.p99_ms, 2),
            ops_per_s: col(&|r| r.ops_per_s, 3),
            error_ratio: col(&|r| r.error_ratio, 4),
        }
    }

    pub fn text(&self) -> String {
        let iv = |i: &Interval, f: &dyn Fn(f64) -> String| {
            if (i.high - i.low).abs() < 1e-12 {
                f(i.estimate)
            } else {
                format!("{} [{}–{}]", f(i.estimate), f(i.low), f(i.high))
            }
        };
        format!(
            "median of {} runs (95% CI): p50 {} · p99 {} · {} ops/s · errors {}\n",
            self.runs_count,
            iv(&self.p50_ms, &|v| fmt_ms(v)),
            iv(&self.p99_ms, &|v| fmt_ms(v)),
            iv(&self.ops_per_s, &|v| format!("{v:.1}")),
            iv(&self.error_ratio, &|v| format!("{:.2}%", v * 100.0)),
        )
    }
}

fn summaries(ctx: &Ctx, ids: &[String]) -> Result<(Vec<RunSummary>, Vec<String>)> {
    let all = ctx.store().list()?;
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match all.iter().find(|s| &s.id == id) {
            Some(s) => found.push(s.clone()),
            None => missing.push(id.clone()),
        }
    }
    Ok((found, missing))
}

/// Result of `baseline show` / `baseline set`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct BaselineShow {
    pub name: String,
    pub window: u64,
    pub runs: Vec<RunSummary>,
    /// Run ids the baseline lists that are no longer stored.
    pub missing: Vec<String>,
    pub stats: RunSetStats,
}

impl CommandOutput for BaselineShow {
    fn text(&self) -> String {
        let mut o = format!(
            "{} {} {}\n",
            bold("baseline"),
            self.name,
            dim(&format!("(window {} runs)", self.window))
        );
        let rows: Vec<Vec<String>> = self
            .runs
            .iter()
            .map(|r| {
                vec![
                    r.id.clone(),
                    r.label.clone().unwrap_or_default(),
                    format!("{:.1}", r.ops_per_s),
                    fmt_ms(r.p50_ms),
                    fmt_ms(r.p99_ms),
                    format!("{:.2}%", r.error_ratio * 100.0),
                ]
            })
            .collect();
        o += &crate::report::style::table(
            &["run", "label", "ops/s", "p50", "p99", "errors"],
            &rows,
            2,
        );
        if !self.runs.is_empty() {
            let p99: Vec<f64> = self.runs.iter().map(|r| r.p99_ms).collect();
            o += &format!("  p99 trend {}\n  ", stats::sparkline(&p99, 24));
            o += &self.stats.text();
        }
        for m in &self.missing {
            o += &dim(&format!("  run {m} is no longer stored\n"));
        }
        o
    }
}

fn show(ctx: &Ctx, name: &str, b: &Baseline) -> Result<BaselineShow> {
    let (runs, missing) = summaries(ctx, &b.runs)?;
    Ok(BaselineShow {
        name: name.to_owned(),
        window: b.window as u64,
        stats: RunSetStats::of(&runs),
        runs,
        missing,
    })
}

/// Result of `baseline list`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct BaselineList {
    pub baselines: Vec<BaselineEntry>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct BaselineEntry {
    pub name: String,
    pub window: u64,
    pub runs_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_run: Option<String>,
}

impl CommandOutput for BaselineList {
    fn text(&self) -> String {
        if self.baselines.is_empty() {
            return format!(
                "no baselines — create one with {}\n",
                bold("oprobe baseline set NAME --last 3")
            );
        }
        let rows: Vec<Vec<String>> = self
            .baselines
            .iter()
            .map(|b| {
                vec![
                    b.name.clone(),
                    b.runs_count.to_string(),
                    b.window.to_string(),
                    b.newest_run.clone().unwrap_or_default(),
                ]
            })
            .collect();
        crate::report::style::table(&["baseline", "runs", "window", "newest"], &rows, 0)
    }
}

/// Result of `baseline rm`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct BaselineRemoved {
    pub name: String,
}

impl CommandOutput for BaselineRemoved {
    fn text(&self) -> String {
        format!("removed baseline {}\n", self.name)
    }
}

fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("invalid baseline name `{name}` (letters, digits, - _ .)"),
        )
        .into())
    }
}

pub fn cmd_baseline(c: BaselineCommand, ctx: &Ctx) -> Result<u8> {
    let file = ctx.paths.baselines_file();
    match c {
        BaselineCommand::Set(a) => {
            valid_name(&a.name)?;
            let store = ctx.store();
            let mut ids: Vec<String> = Vec::new();
            if let Some(n) = a.last {
                let mut all = store.list()?;
                all.truncate(n);
                ids.extend(all.into_iter().rev().map(|s| s.id));
            }
            for r in &a.runs {
                for run in load_side(ctx, r)? {
                    ids.push(run.id().to_owned());
                }
            }
            if ids.is_empty() {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    "name the baseline's runs: `baseline set NAME RUNREF…` or `--last N`",
                )
                .into());
            }
            let mut b = Baselines::load(&file)?;
            let entry = b.baselines.entry(a.name.clone()).or_default();
            if !a.append {
                entry.runs.clear();
            }
            if let Some(w) = a.window {
                entry.window = w.max(1);
            } else if ids.len() > entry.window {
                entry.window = ids.len();
            }
            for id in &ids {
                entry.push(id);
            }
            let snapshot = entry.clone();
            b.save(&file)?;
            ctx.out.finish(&show(ctx, &a.name, &snapshot)?)
        }
        BaselineCommand::List => {
            let b = Baselines::load(&file)?;
            ctx.out.finish(&BaselineList {
                baselines: b
                    .baselines
                    .iter()
                    .map(|(name, bl)| BaselineEntry {
                        name: name.clone(),
                        window: bl.window as u64,
                        runs_count: bl.runs.len() as u64,
                        newest_run: bl.runs.last().cloned(),
                    })
                    .collect(),
            })
        }
        BaselineCommand::Rm(a) => {
            let mut b = Baselines::load(&file)?;
            if b.remove(&a.name).is_none() {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("no baseline named `{}`", a.name),
                )
                .into());
            }
            b.save(&file)?;
            ctx.out.finish(&BaselineRemoved { name: a.name })
        }
        BaselineCommand::Show(a) => {
            let b = Baselines::load(&file)?;
            let Some(bl) = b.get(&a.name) else {
                return Err(OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!("no baseline named `{}`", a.name),
                )
                .into());
            };
            ctx.out.finish(&show(ctx, &a.name, bl)?)
        }
    }
}
