//! `oprobe runs list|show|rm|gc` and `oprobe compare` (the 2.x `diff` gate on RUNREFs).

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use crate::errors::{ErrorCode, OprobeError};
use crate::model::Run;
use crate::model::VerdictLevel;
use crate::output::CommandOutput;
use crate::report::diff::{DiffReport, Thresholds, diff};
use crate::report::style::{bold, dim, green, red, yellow};
use crate::report::{markdown, terminal, write_file};
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
                    r.cost_per_month_usd
                        .map(crate::util::fmt_usd)
                        .unwrap_or_else(|| "–".into()),
                ]
            })
            .collect();
        let mut o = crate::report::style::table(
            &[
                "run", "label", "target", "when", "window", "verdict", "ops/s", "p99", "$/month",
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
        o += &dim("show one: oprobe runs show RUNREF  (id, prefix, last, last~1, label)\n");
        o
    }

    fn markdown(&self) -> String {
        let mut o = String::from(
            "| run | label | target | started | verdict | ops/s | p99 | $/month |\n|---|---|---|---|---|---:|---:|---:|\n",
        );
        for r in &self.runs {
            o += &format!(
                "| `{}` | {} | {} | {} | {} | {:.1} | {} | {} |\n",
                r.id,
                r.label.as_deref().unwrap_or(""),
                r.target_name
                    .clone()
                    .unwrap_or_else(|| crate::util::display_url(&r.target_display)),
                r.started_at,
                r.verdict_level.as_str(),
                r.ops_per_s,
                fmt_ms(r.p99_ms),
                r.cost_per_month_usd
                    .map(crate::util::fmt_usd)
                    .unwrap_or_else(|| "–".into()),
            );
        }
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
    /// `--detail` (text output).
    #[serde(skip)]
    #[schemars(skip)]
    pub detail: bool,
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
            Some(run) => terminal::render_run_with(run, self.detail),
            None => terminal::render(&self.run.report),
        }
    }
    fn html(&self) -> Option<String> {
        Some(match &self.full {
            Some(run) => crate::report::html::render(run),
            None => crate::report::html::render_report(&self.run.report),
        })
    }
    fn markdown(&self) -> String {
        match &self.full {
            Some(run) => markdown::render(run),
            None => markdown::render_report(&self.run.report),
        }
    }
}

pub fn cmd_show(a: RunsShowArgs, ctx: &Ctx) -> Result<u8> {
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
        detail: a.detail,
    })
}

/// Result of `oprobe runs open`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct OpenedRun {
    pub path: String,
    /// The app was started (it runs on its own).
    pub launched: bool,
}

impl CommandOutput for OpenedRun {
    fn text(&self) -> String {
        format!("{} opening {} in the app\n", green("●"), self.path)
    }
}

pub fn cmd_open(a: RunRefArg, ctx: &Ctx) -> Result<u8> {
    let path = ctx.store().resolve(&a.run)?;
    let path = path.canonicalize().unwrap_or(path);
    #[cfg(feature = "tui")]
    let window = super::gui_available();
    #[cfg(not(feature = "tui"))]
    let window: std::result::Result<(), &str> = Err("this oprobe was built without native windows");
    if let Err(why) = window {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("cannot open a window: {why}"),
        )
        .with_hint(format!(
            "read it here with `oprobe runs show {}`, or in the terminal workbench: `oprobe top --open {}`",
            a.run,
            path.display()
        ))
        .into());
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("gui")
        .arg("--open")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start the app: {e}"))?;
    ctx.out.finish(&OpenedRun {
        path: path.display().to_string(),
        launched: true,
    })
}

/// `30d`, `2w`, `12h`, `90m`: days and weeks on top of [`crate::util::parse_duration`].
pub fn parse_age(v: &str) -> Result<std::time::Duration> {
    let bad = || -> anyhow::Error {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("invalid age `{v}` (examples: 30d, 2w, 12h)"),
        )
        .into()
    };
    let t = v.trim().to_ascii_lowercase();
    let mult = if t.ends_with('d') {
        86_400.0
    } else if t.ends_with('w') {
        7.0 * 86_400.0
    } else {
        return crate::util::parse_duration(&t).map_err(|_| bad());
    };
    let n: f64 = t[..t.len() - 1].trim().parse().map_err(|_| bad())?;
    if !n.is_finite() || n < 0.0 {
        return Err(bad());
    }
    Ok(std::time::Duration::from_secs_f64(n * mult))
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
    let older = a.older_than.as_deref().map(parse_age).transpose()?;
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
    fn html(&self) -> Option<String> {
        Some(crate::report::html::render_compare(self))
    }
    fn exit_code(&self) -> u8 {
        if self.passed {
            0
        } else {
            crate::errors::exit::GATE
        }
    }
}

/// Load a RUNREF or file for `compare`.
fn load(ctx: &Ctx, r: &str) -> Result<Run> {
    let path = ctx.store().resolve(r)?;
    crate::store::runs::load_path(&path)
}

pub fn cmd_compare(a: CompareArgs, ctx: &Ctx) -> Result<u8> {
    if a.baseline_name.is_some() {
        return Err(OprobeError::not_implemented("compare --baseline").into());
    }
    let Some(base_ref) = a.baseline.as_deref() else {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            "compare needs a baseline run: `oprobe compare A [B]`",
        )
        .with_hint("e.g. `oprobe compare last~1 last` or `oprobe compare base.json cand.json`")
        .into());
    };
    let cand_ref = a.candidate.as_deref().unwrap_or("last");
    let base = load(ctx, base_ref)?;
    let cand = load(ctx, cand_ref)?;
    let t = Thresholds {
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
    };
    let d = diff(&base.report, &cand.report, &t);
    if let Some(p) = &a.markdown {
        write_file(p, &d.render_markdown())?;
    }
    if let Some(p) = &a.junit {
        write_file(p, &d.render_junit())?;
    }
    if let Some(p) = &a.json {
        write_file(p, &serde_json::to_string_pretty(&d)?)?;
    }
    if !d.passed && ctx.out.format == crate::output::OutputFormat::Text {
        ctx.out.info(red(&bold("regression gate failed (exit 3)")));
    }
    ctx.out.finish(&d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_accept_days_and_weeks() {
        assert_eq!(parse_age("30d").unwrap().as_secs(), 30 * 86_400);
        assert_eq!(parse_age("2w").unwrap().as_secs(), 14 * 86_400);
        assert_eq!(parse_age("12h").unwrap().as_secs(), 12 * 3600);
        assert!(parse_age("xd").is_err());
        assert!(parse_age("soon").is_err());
    }

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
