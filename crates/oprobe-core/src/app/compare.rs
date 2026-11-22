//! `oprobe compare` (repeat-aware regression gate) and `oprobe baseline set|list|rm|show`.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::*;
use crate::analysis::compare::{Comparison, compare_with_noise};
use crate::analysis::stats::{self, Interval};
use crate::engine::Cancel;
use crate::errors::{ErrorCode, OprobeError};
use crate::model::Run;
use crate::output::CommandOutput;
use crate::report::diff::GateVerdict;
use crate::report::diff::Thresholds;
use crate::report::style::yellow;
use crate::report::style::{bold, dim, red};
use crate::report::write_file;
use crate::store::{Baseline, Baselines, RunSummary};
use crate::util::fmt_ms;

impl CommandOutput for Comparison {
    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD_HTML
    }

    fn text(&self) -> String {
        self.render_terminal()
    }
    fn markdown(&self) -> String {
        self.render_markdown()
    }
    fn html(&self) -> Option<String> {
        Some(crate::report::html::render_compare(&self.gate))
    }
    fn exit_code(&self) -> u8 {
        Comparison::exit_code(self)
    }
    fn warnings(&self) -> Vec<String> {
        self.gate.warnings.clone()
    }
    fn next(&self) -> Vec<crate::output::NextStep> {
        if self.comparable {
            return Vec::new();
        }
        let base = &self.baseline.representative;
        vec![crate::output::NextStep::new(
            "replay the baseline for a comparable candidate",
            format!("oprobe run --from {base}"),
        )]
    }
}

pub fn thresholds(a: &CompareRequest) -> Thresholds {
    Thresholds {
        p99_increase_ratio: a.max_p99_increase / 100.0,
        p50_increase_ratio: a.max_p50_increase / 100.0,
        min_latency_delta_ms: a.min_latency_delta_ms,
        throughput_drop_ratio: a.max_throughput_drop / 100.0,
        error_increase_ratio: a.max_error_increase / 100.0,
        request_amp_increase_ratio: a.max_request_amp_increase / 100.0,
        read_amp_increase_ratio: a.max_read_amp_increase / 100.0,
        cost_increase_ratio: a.max_cost_increase / 100.0,
        fail_on_new_throttling: !a.allow_throttling,
        min_samples_count: a.min_samples,
        allow_workload_change: a.allow_workload_change,
        allow_target_change: a.allow_target_change,
        strict: a.strict,
        fail_on_warn: a.fail_on_warn,
        ..Thresholds::default()
    }
}

/// The noise floors learned for the target and workload of `r` (none when unknown).
pub fn noise_for(ctx: &Ctx, r: &Run) -> Option<crate::store::baselines::NoiseEntry> {
    let file = crate::store::baselines::noise_file(&ctx.paths);
    crate::store::baselines::NoiseFloors::load(&file)
        .ok()?
        .of(&r.report)
        .cloned()
}

/// Load one side: a RUNREF, a comma-separated list of RUNREFs, or `all:LABEL` (every stored run
/// with that label, e.g. a `--repeat` group).
pub fn load_side(ctx: &Ctx, spec: &str) -> Result<Vec<Run>> {
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
pub fn baseline_runs(ctx: &Ctx, name: &str) -> Result<Vec<Run>> {
    let b = Baselines::load(&ctx.paths.baselines_file())?;
    let Some(bl) = b.get(name) else {
        let known: Vec<&str> = b.baselines.keys().map(String::as_str).collect();
        return Err(OprobeError::new(
            ErrorCode::UsageUnknownBaseline,
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
pub fn write_compare_files(c: &Comparison, a: &CompareRequest) -> Result<()> {
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

/// `oprobe compare`: load both sides, gate, write the requested files.
pub fn compare(ctx: &Ctx, a: CompareRequest) -> Result<Comparison> {
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
    // A candidate run that is also in the baseline set (e.g. it already joined a named
    // baseline) is left out of the baseline side — unless that would empty it (comparing a
    // run with itself is allowed and passes).
    let others: Vec<Run> = base
        .iter()
        .filter(|b| !cand.iter().any(|c| c.id() == b.id()))
        .cloned()
        .collect();
    let base = if others.is_empty() { base } else { others };
    let noise = noise_for(ctx, &base[0]);
    let c = compare_with_noise(
        &base,
        &cand,
        &base_label,
        &cand_ref,
        &thresholds(&a),
        noise.as_ref(),
    );
    write_compare_files(&c, &a)?;
    if ctx.out.format == crate::output::OutputFormat::Text {
        match c.verdict {
            GateVerdict::Regression => {
                ctx.out.info(red(&bold("regression gate failed (exit 3)")));
            }
            GateVerdict::NotComparable => ctx.out.info(yellow(&bold(&format!(
                "not comparable: no regression verdict{}",
                if a.strict { " (--strict: exit 2)" } else { "" }
            )))),
            GateVerdict::Warned => ctx.out.info(yellow(&bold(&format!(
                "passed with warnings: a check is significantly worse within its limit{}",
                if a.fail_on_warn { " (--fail-on-warn: exit 3)" } else { "" }
            )))),
            GateVerdict::Inconclusive => ctx.out.info(yellow(&bold(
                "inconclusive: a check could not rule out a regression (repeat the runs)",
            ))),
            GateVerdict::Passed => {}
        }
    }
    Ok(c)
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
    #[serde(rename = "window_count")]
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
    #[serde(rename = "window_count")]
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

/// Result of `baseline calibrate`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CalibrateOutput {
    /// Target identity the floor is stored under.
    pub target: String,
    pub workload_hash: String,
    /// The identical runs measured.
    pub run_ids: Vec<String>,
    /// A/A pairs added.
    pub pairs_count: u64,
    /// A/A pairs known for this target and workload (all calibrations and verifications).
    pub total_pairs_count: u64,
    /// The noise floor per metric (`p50_ms`, `p99_ms`, `ops_per_s`, `mib_per_s`): relative
    /// changes below it are not significant.
    pub noise_floor_ratio: std::collections::BTreeMap<String, f64>,
    /// Where it is stored.
    pub path: String,
}

impl CommandOutput for CalibrateOutput {
    fn text(&self) -> String {
        let mut o = format!(
            "{} {} {}\n",
            bold("noise floor"),
            self.target,
            dim(&format!(
                "· workload {} · {} new A/A pair(s), {} in total",
                self.workload_hash, self.pairs_count, self.total_pairs_count
            ))
        );
        for (m, v) in &self.noise_floor_ratio {
            o += &format!("  {m:<10} ±{:.1}%\n", v * 100.0);
        }
        o += &dim(&format!("stored in {}\n", self.path));
        o
    }
}

/// `oprobe baseline calibrate TARGET [--repeat N]`: identical runs, consecutive pairs feed the
/// noise floor (`<data>/noise.toml`) that compare and verify use.
/// What [`calibrate`] produced.
#[derive(Clone, Debug)]
pub enum CalibrateOutcome {
    /// `safety.dry_run`: the plan.
    Plan(Box<super::run::PlanOutput>),
    Calibrated(Box<CalibrateOutput>),
}

pub async fn calibrate(ctx: &Ctx, a: CalibrateRequest, cancel: Cancel) -> Result<CalibrateOutcome> {
    use crate::store::baselines::{NOISE_METRICS, NoiseFloors, noise_file, noise_ratio};
    let mut runs: Vec<Run> = Vec::new();
    if let Some(from) = &a.from {
        let base = ctx.store().load(from)?;
        let replayer =
            super::verify::Replayer::of_run(ctx, &base, &a.safety, a.no_save, cancel.clone())?;
        if a.safety.dry_run {
            let rails = super::run::rails(ctx, &replayer.base, &a.safety)?;
            let mut plan = rails.plan(&replayer.base);
            plan.eta_s *= a.repeat as f64;
            plan.assumptions.push(format!(
                "{} replays of {} (this plan is one of them)",
                a.repeat,
                base.id()
            ));
            return Ok(CalibrateOutcome::Plan(Box::new(super::run::PlanOutput {
                plan,
            })));
        }
        for i in 0..a.repeat {
            ctx.out.info(dim(&format!(
                "calibrate: run {}/{} (replay of {})",
                i + 1,
                a.repeat,
                base.id()
            )));
            runs.push(
                replayer
                    .replay(
                        ctx,
                        &crate::model::Overrides::new(),
                        &format!("calibrate-{}", base.id()),
                    )
                    .await?,
            );
        }
    } else {
        let (profile, target_name) = super::profile::resolve_profile(
            ctx,
            &a.target,
            &a.workload,
            &a.knobs,
            &CostInput::default(),
        )?;
        let output = OutputOptions {
            no_save: a.no_save,
            label: Some(format!("calibrate-{}", crate::model::RunId::new())),
            ..OutputOptions::default()
        };
        if a.safety.dry_run {
            let rails = super::run::rails(ctx, &profile, &a.safety)?;
            let mut plan = rails.plan(&profile);
            plan.eta_s *= a.repeat as f64;
            plan.assumptions.push(format!(
                "{} identical runs (this plan is one of them)",
                a.repeat
            ));
            return Ok(CalibrateOutcome::Plan(Box::new(super::run::PlanOutput {
                plan,
            })));
        }
        // One guard for the whole command: the budgets cap all calibration runs together.
        let guard = super::run::guard_for(ctx, &profile, &super::run::budgets(ctx, &a.safety)?);
        for i in 0..a.repeat {
            ctx.out
                .info(dim(&format!("calibrate: run {}/{}", i + 1, a.repeat)));
            match super::run::measure_guarded(
                ctx,
                profile.clone(),
                target_name.clone(),
                &a.safety,
                &output,
                guard.clone(),
                &cancel,
            )
            .await?
            {
                super::run::Measured::Run(o) => runs.push(
                    o.full
                        .map(|r| *r)
                        .ok_or_else(|| anyhow::anyhow!("the calibration run kept no rows"))?,
                ),
                super::run::Measured::Plan(_) => unreachable!("not a dry run"),
            }
        }
    }
    let file = noise_file(&ctx.paths);
    let mut floors = NoiseFloors::load(&file)?;
    let mut pairs = 0u64;
    for w in runs.windows(2) {
        floors.add_pair(&w[0].report, &w[1].report);
        pairs += 1;
    }
    floors.save(&file)?;
    let first = &runs[0].report;
    let entry = floors.of(first).cloned();
    Ok(CalibrateOutcome::Calibrated(Box::new(CalibrateOutput {
        target: crate::report::diff::target_identity(first),
        workload_hash: first.spec.workload_hash(),
        run_ids: runs.iter().map(|r| r.id().to_owned()).collect(),
        pairs_count: pairs,
        total_pairs_count: entry.as_ref().map_or(0, |e| e.pairs_count),
        noise_floor_ratio: NOISE_METRICS
            .iter()
            .map(|m| ((*m).to_owned(), noise_ratio(entry.as_ref(), m)))
            .collect(),
        path: file.display().to_string(),
    })))
}

fn unknown_baseline(name: &str) -> anyhow::Error {
    OprobeError::new(
        ErrorCode::UsageUnknownBaseline,
        format!("no baseline named `{name}`"),
    )
    .into()
}

/// `baseline set NAME RUNREF… | --last N`.
pub fn baseline_set(ctx: &Ctx, a: BaselineSetRequest) -> Result<BaselineShow> {
    let file = ctx.paths.baselines_file();
    valid_name(&a.name)?;
    let store = ctx.store();
    let mut ids: Vec<String> = Vec::new();
    if let Some(n) = a.last {
        // The newest N complete runs: partial ones cannot be baselines.
        let mut all: Vec<_> = store
            .list()?
            .into_iter()
            .filter(|s| !s.interrupted)
            .collect();
        all.truncate(n);
        ids.extend(all.into_iter().rev().map(|s| s.id));
    }
    for r in &a.runs {
        for run in load_side(ctx, r)? {
            crate::store::baselines::check_baseline_run(&run.report)?;
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
    show(ctx, &a.name, &snapshot)
}

/// `baseline list`.
pub fn baseline_list(ctx: &Ctx) -> Result<BaselineList> {
    let b = Baselines::load(&ctx.paths.baselines_file())?;
    Ok(BaselineList {
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

/// `baseline rm NAME`.
pub fn baseline_rm(ctx: &Ctx, name: &str) -> Result<BaselineRemoved> {
    let file = ctx.paths.baselines_file();
    let mut b = Baselines::load(&file)?;
    if b.remove(name).is_none() {
        return Err(unknown_baseline(name));
    }
    b.save(&file)?;
    Ok(BaselineRemoved { name: name.into() })
}

/// `baseline show NAME`.
pub fn baseline_show(ctx: &Ctx, name: &str) -> Result<BaselineShow> {
    let b = Baselines::load(&ctx.paths.baselines_file())?;
    let Some(bl) = b.get(name) else {
        return Err(unknown_baseline(name));
    };
    show(ctx, name, bl)
}
