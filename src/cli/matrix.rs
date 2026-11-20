//! `oprobe matrix`: the same workload across targets and knob values, run one after another,
//! ranked. `--targets a,b --vary concurrency=8,16,32 --vary http=h1,h2
//! --vary addressing=virtual,path`.

use std::collections::BTreeMap;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use super::measure::{Opts, measure};
use crate::engine::Cancel;
use crate::engine::measure::Mode;
use crate::errors::{ErrorCode, OprobeError, exit};
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green};
use crate::util::fmt_ms;

/// Knobs `--vary` accepts.
pub const VARY_KEYS: [&str; 12] = [
    "concurrency",
    "io_concurrency",
    "chunk_size",
    "range_gap",
    "max_inflight",
    "http",
    "addressing",
    "keep_alive",
    "cache",
    "retries",
    "tail_cut",
    "preset",
];

fn parse_vary(s: &str) -> Result<(String, Vec<String>), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("invalid --vary `{s}` (expected KNOB=V1,V2,…)"))?;
    let k = k.trim().replace('-', "_").to_ascii_lowercase();
    if !VARY_KEYS.contains(&k.as_str()) {
        return Err(format!(
            "cannot vary `{k}` (one of: {})",
            VARY_KEYS.join(", ")
        ));
    }
    let vals: Vec<String> = v
        .split(',')
        .map(|x| x.trim().to_owned())
        .filter(|x| !x.is_empty())
        .collect();
    if vals.is_empty() {
        return Err(format!("--vary {k} needs at least one value"));
    }
    Ok((k, vals))
}

fn on(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("expected on/off, got `{v}`")),
    }
}

/// Apply one knob setting to the argument structs.
fn apply(
    k: &str,
    v: &str,
    t: &mut TargetArgs,
    w: &mut WorkloadArgs,
    kn: &mut KnobArgs,
) -> Result<(), String> {
    let num = |v: &str| {
        v.parse::<usize>()
            .map_err(|_| format!("`{k}={v}`: not a number"))
    };
    match k {
        "concurrency" => kn.concurrency = Some(num(v)?.max(1)),
        "io_concurrency" => kn.io_concurrency = Some(num(v)?.max(1)),
        "chunk_size" => kn.chunk_size = Some(v.into()),
        "range_gap" => kn.range_gap = Some(v.into()),
        "max_inflight" => kn.max_inflight = Some(num(v)?),
        "retries" => kn.retries = Some(num(v)?),
        "http" => {
            kn.http = Some(match v.to_ascii_lowercase().as_str() {
                "h1" | "http1" | "1.1" => HttpArg::H1,
                "h2" | "http2" | "2" => HttpArg::H2,
                "auto" => HttpArg::Auto,
                _ => return Err(format!("http must be h1, h2 or auto, got `{v}`")),
            })
        }
        "addressing" => {
            let virt = match v.to_ascii_lowercase().as_str() {
                "virtual" | "vhost" => true,
                "path" => false,
                _ => return Err(format!("addressing must be virtual or path, got `{v}`")),
            };
            t.options
                .retain(|o| !o.starts_with("enable_virtual_host_style="));
            t.options.push(format!("enable_virtual_host_style={virt}"));
        }
        "keep_alive" => kn.no_keep_alive = !on(v)?,
        "tail_cut" => kn.tail_cut = on(v)?,
        "cache" => {
            if on(v).ok() == Some(false) {
                kn.cache = None;
            } else {
                kn.cache = Some(v.into());
            }
        }
        "preset" => w.preset = Some(v.into()),
        _ => return Err(format!("cannot vary `{k}`")),
    }
    Ok(())
}

/// One cell of the matrix.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct MatrixCell {
    pub rank: u64,
    pub target: String,
    /// Knob values of this cell (`--vary`).
    pub settings: BTreeMap<String, String>,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub error_ratio: f64,
    pub throttled_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_per_month_usd: Option<f64>,
    pub verdict: String,
    /// The cell failed to run (error message); its numbers are zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of `oprobe matrix`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct MatrixOutput {
    pub rank_by: String,
    pub cells: Vec<MatrixCell>,
}

impl CommandOutput for MatrixOutput {
    fn text(&self) -> String {
        let keys: Vec<String> = self
            .cells
            .first()
            .map(|c| c.settings.keys().cloned().collect())
            .unwrap_or_default();
        let mut header: Vec<&str> = vec!["#", "target"];
        header.extend(keys.iter().map(String::as_str));
        header.extend(["ops/s", "MiB/s", "p50", "p99", "errors", "throttled", "run"]);
        let rows: Vec<Vec<String>> = self
            .cells
            .iter()
            .map(|c| {
                let mut r = vec![
                    if c.rank == 1 {
                        green("1")
                    } else {
                        c.rank.to_string()
                    },
                    crate::util::display_url(&c.target),
                ];
                r.extend(
                    keys.iter()
                        .map(|k| c.settings.get(k).cloned().unwrap_or_default()),
                );
                if let Some(e) = &c.error {
                    r.push(format!("failed: {e}"));
                    return r;
                }
                r.extend([
                    format!("{:.1}", c.ops_per_s),
                    format!("{:.1}", c.mib_per_s),
                    fmt_ms(c.p50_ms),
                    fmt_ms(c.p99_ms),
                    format!("{:.2}%", c.error_ratio * 100.0),
                    c.throttled_count.to_string(),
                    c.run_id.clone(),
                ]);
                r
            })
            .collect();
        let mut o = format!("{} ranked by {}\n", bold("matrix"), self.rank_by);
        o += &crate::report::style::table(&header, &rows, 2);
        o += &dim(
            "inspect a cell: oprobe runs show RUN · compare two: oprobe compare RUN_A RUN_B\n",
        );
        o
    }
    fn markdown(&self) -> String {
        let mut md = format!(
            "## oprobe matrix (ranked by {})\n\n| # | target | settings | ops/s | p50 | p99 | errors | run |\n|---:|---|---|---:|---:|---:|---:|---|\n",
            self.rank_by
        );
        for c in &self.cells {
            let s: Vec<String> = c.settings.iter().map(|(k, v)| format!("{k}={v}")).collect();
            md += &format!(
                "| {} | {} | {} | {:.1} | {} | {} | {:.2}% | `{}` |\n",
                c.rank,
                crate::util::display_url(&c.target),
                s.join(" "),
                c.ops_per_s,
                fmt_ms(c.p50_ms),
                fmt_ms(c.p99_ms),
                c.error_ratio * 100.0,
                c.run_id
            );
        }
        md
    }
}

/// One cell of `matrix --dry-run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct MatrixPlanCell {
    pub target: String,
    /// Knob values of this cell (`--vary`).
    pub settings: BTreeMap<String, String>,
    pub plan: crate::guard::Plan,
}

/// Result of `matrix --dry-run`: every cell's plan and the totals. Budgets apply to the whole
/// matrix.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct MatrixPlanOutput {
    pub cells: Vec<MatrixPlanCell>,
    pub estimated_requests_count: u64,
    pub estimated_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    pub eta_s: f64,
}

impl MatrixPlanOutput {
    fn new(cells: Vec<MatrixPlanCell>) -> Self {
        let costs: Vec<f64> = cells
            .iter()
            .filter_map(|c| c.plan.estimated_cost_usd)
            .collect();
        Self {
            estimated_requests_count: cells.iter().map(|c| c.plan.estimated_requests_count).sum(),
            estimated_bytes: cells.iter().map(|c| c.plan.estimated_bytes).sum(),
            estimated_cost_usd: (!costs.is_empty()).then(|| costs.iter().sum()),
            eta_s: cells.iter().map(|c| c.plan.eta_s).sum(),
            cells,
        }
    }
}

impl CommandOutput for MatrixPlanOutput {
    fn text(&self) -> String {
        let rows: Vec<Vec<String>> = self
            .cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                vec![
                    (i + 1).to_string(),
                    crate::util::display_url(&c.target),
                    c.settings
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    c.plan.estimated_requests_count.to_string(),
                    crate::util::fmt_bytes(c.plan.estimated_bytes),
                    c.plan
                        .estimated_cost_usd
                        .map(crate::util::fmt_usd)
                        .unwrap_or_else(|| "n/a".into()),
                    format!("{:.0}s", c.plan.eta_s),
                ]
            })
            .collect();
        let mut o = format!(
            "{} {} cells would run\n",
            bold("matrix plan"),
            self.cells.len()
        );
        o += &crate::report::style::table(
            &[
                "#", "target", "settings", "requests", "bytes", "cost", "ETA",
            ],
            &rows,
            2,
        );
        o += &format!(
            "  total ~{} requests · ~{} · cost {} · ETA {:.0}s\n",
            self.estimated_requests_count,
            crate::util::fmt_bytes(self.estimated_bytes),
            self.estimated_cost_usd
                .map(crate::util::fmt_usd)
                .unwrap_or_else(|| "not applicable".into()),
            self.eta_s
        );
        o += &dim("dry run: nothing was sent to the target; budgets cover the whole matrix\n");
        o
    }
}

fn score(c: &MatrixCell, by: &str) -> f64 {
    if c.error.is_some() {
        return f64::NEG_INFINITY;
    }
    // Higher is better; errors and throttling push a cell down whatever the objective.
    let penalty = if c.error_ratio > 0.01 { 1e12 } else { 0.0 };
    match by {
        "p99" | "latency" => -c.p99_ms - penalty,
        "p50" => -c.p50_ms - penalty,
        "cost" => -c.cost_per_month_usd.unwrap_or(f64::MAX / 4.0) - penalty,
        "mib" | "bandwidth" => c.mib_per_s - penalty,
        _ => c.ops_per_s - penalty,
    }
}

pub async fn cmd_matrix(a: MatrixArgs, ctx: &Ctx) -> Result<u8> {
    let bad = |m: String| -> anyhow::Error {
        OprobeError::new(ErrorCode::UsageInvalidArgument, m).into()
    };
    let rank_by = a.rank_by.to_ascii_lowercase();
    if ![
        "throughput",
        "ops",
        "mib",
        "bandwidth",
        "p99",
        "latency",
        "p50",
        "cost",
    ]
    .contains(&rank_by.as_str())
    {
        return Err(bad(format!(
            "invalid --rank-by `{}` (throughput, mib, p50, p99, cost)",
            a.rank_by
        )));
    }
    let mut targets: Vec<Option<String>> = a
        .targets
        .iter()
        .flat_map(|t| t.split(','))
        .map(|t| Some(t.trim().to_owned()))
        .filter(|t| t.as_deref().is_some_and(|s| !s.is_empty()))
        .collect();
    if targets.is_empty() {
        targets.push(a.target.target.clone());
    }
    let vary: Vec<(String, Vec<String>)> = a
        .vary
        .iter()
        .map(|v| parse_vary(v))
        .collect::<Result<_, _>>()
        .map_err(bad)?;
    // Cartesian product of the --vary values.
    let mut combos: Vec<Vec<(String, String)>> = vec![Vec::new()];
    for (k, vals) in &vary {
        combos = combos
            .into_iter()
            .flat_map(|c| {
                vals.iter().map(move |v| {
                    let mut c = c.clone();
                    c.push((k.clone(), v.clone()));
                    c
                })
            })
            .collect();
    }
    let cells_n = targets.len() * combos.len();
    if cells_n < 2 {
        return Err(bad(
            "a matrix needs at least two cells: give --targets a,b and/or --vary KNOB=V1,V2".into(),
        ));
    }
    if cells_n > 64 {
        return Err(bad(format!("{cells_n} cells is too many (at most 64)")));
    }
    // Validate every cell before measuring anything.
    let mut plans = Vec::new();
    for t in &targets {
        for combo in &combos {
            let (mut ta, mut wa, mut ka) = (a.target.clone(), a.workload.clone(), a.knobs.clone());
            ta.target = t.clone();
            for (k, v) in combo {
                apply(k, v, &mut ta, &mut wa, &mut ka).map_err(bad)?;
            }
            let (mut profile, name) = super::resolve_profile(ctx, &ta, &wa, &ka, &a.cost)?;
            super::apply_sla(&mut profile, &a.sla)?;
            plans.push((profile, name, combo.clone()));
        }
    }
    let mut rails = Vec::new();
    for (p, _, _) in &plans {
        rails.push(super::run::rails(ctx, p, &a.safety)?);
    }
    if a.safety.dry_run {
        let cells = plans
            .iter()
            .zip(&rails)
            .map(|((p, _, combo), r)| MatrixPlanCell {
                target: p.target.display(),
                settings: combo.iter().cloned().collect(),
                plan: r.plan(p),
            })
            .collect();
        return ctx.out.finish(&MatrixPlanOutput::new(cells));
    }
    // One guard for the whole matrix: the budgets cap all cells together.
    let guard = plans
        .first()
        .zip(rails.first())
        .and_then(|((p, _, _), r)| super::run::guard_for(ctx, p, &r.budgets));
    let cancel = Cancel::new();
    super::install_ctrl_c(cancel.clone());
    let mut cells = Vec::new();
    let label = a
        .label
        .clone()
        .unwrap_or_else(|| format!("matrix-{}", crate::model::RunId::new()));
    let output = OutputArgs {
        label: Some(label.clone()),
        ..Default::default()
    };
    for (i, ((mut profile, name, combo), rails)) in plans.into_iter().zip(rails).enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        profile.name = Some(label.clone());
        let target = profile.target.display();
        let settings: BTreeMap<String, String> = combo.into_iter().collect();
        ctx.out.info(dim(&format!(
            "cell {}/{cells_n}: {} {}",
            i + 1,
            crate::util::display_url(&target),
            settings
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        )));
        let mut cell = MatrixCell {
            rank: 0,
            target,
            settings,
            run_id: String::new(),
            path: None,
            ops_per_s: 0.0,
            mib_per_s: 0.0,
            p50_ms: 0.0,
            p99_ms: 0.0,
            error_ratio: 0.0,
            throttled_count: 0,
            cost_per_month_usd: None,
            verdict: String::new(),
            error: None,
        };
        let opts = Opts {
            target_name: name,
            rails,
            guard: guard.clone(),
            output: &output,
            cancel: &cancel,
            mode: Mode::Run,
        };
        match measure(ctx, profile, opts).await {
            Ok(m) => {
                let r = &m.run.report;
                cell.run_id = r.run.id.clone();
                cell.path = m.path.map(|p| p.display().to_string());
                cell.ops_per_s = r.summary.ops_per_s;
                cell.mib_per_s = r.summary.mib_per_s;
                cell.p50_ms = r.summary.latency.p50;
                cell.p99_ms = r.summary.latency.p99;
                cell.error_ratio = r.summary.error_rate;
                cell.throttled_count = r.resilience.throttled_responses;
                cell.cost_per_month_usd = r.cost.applicable.then_some(r.cost.selected.total_usd);
                cell.verdict = r.verdict.level.as_str().into();
            }
            Err(e) => {
                let body = crate::errors::classify(&e, "s3");
                // Budgets and safety stop the whole matrix.
                if matches!(body.code.exit_code(), exit::BUDGET | exit::SAFETY) {
                    return Err(e);
                }
                ctx.out
                    .warn(format!("cell {} failed: {}", i + 1, body.message));
                cell.error = Some(body.message);
            }
        }
        cells.push(cell);
    }
    cells.sort_by(|x, y| score(y, &rank_by).total_cmp(&score(x, &rank_by)));
    for (i, c) in cells.iter_mut().enumerate() {
        c.rank = i as u64 + 1;
    }
    ctx.out.finish(&MatrixOutput { rank_by, cells })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vary_values_apply_to_the_arguments() {
        let (k, v) = parse_vary("concurrency=8,16").unwrap();
        assert_eq!((k.as_str(), v.len()), ("concurrency", 2));
        assert!(parse_vary("bogus=1").is_err());
        let (mut t, mut w, mut kn): (TargetArgs, WorkloadArgs, KnobArgs) = Default::default();
        apply("addressing", "path", &mut t, &mut w, &mut kn).unwrap();
        apply("addressing", "virtual", &mut t, &mut w, &mut kn).unwrap();
        assert_eq!(t.options, vec!["enable_virtual_host_style=true".to_owned()]);
        apply("http", "h2", &mut t, &mut w, &mut kn).unwrap();
        assert!(matches!(kn.http, Some(HttpArg::H2)));
        apply("keep_alive", "off", &mut t, &mut w, &mut kn).unwrap();
        assert!(kn.no_keep_alive);
        assert!(apply("http", "h3", &mut t, &mut w, &mut kn).is_err());
    }
}
