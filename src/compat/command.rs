//! `oprobe compat TARGET`.

use std::fmt::Write as _;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::{CompatReport, CompatStatus, checksum_check, run_suite};
use crate::cli::Ctx;
use crate::cli::args::{CompatArgs, CostArgs, KnobArgs, WorkloadArgs};
use crate::config::Knobs;
use crate::engine::operator;
use crate::errors::{ErrorCode, OprobeError};
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green, red, yellow};

/// Result of `oprobe compat`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CompatOutput {
    #[serde(flatten)]
    pub report: CompatReport,
    /// `--min-score` (percent), when given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_score_pct: Option<f64>,
}

impl CommandOutput for CompatOutput {
    fn text(&self) -> String {
        let r = &self.report;
        let mut o = format!("{} {}\n", bold("oprobe compat"), r.target);
        let mut category = String::new();
        for c in &r.checks {
            if c.category != category {
                category.clone_from(&c.category);
                let _ = writeln!(o, "  {}", dim(&category));
            }
            let _ = match c.status {
                CompatStatus::Pass => {
                    writeln!(o, "    {} {:<26} {}", green("✓"), c.id, dim(&c.detail))
                }
                CompatStatus::Fail => {
                    writeln!(o, "    {} {:<26} {}", red("✖"), c.id, red(&c.detail))
                }
                CompatStatus::Skipped => {
                    writeln!(o, "    {} {:<26} {}", dim("·"), dim(&c.id), dim(&c.detail))
                }
            };
        }
        let score = format!("{:.0}%", r.score_pct);
        let _ = writeln!(
            o,
            "\n{} score {} — {} passed, {} failed, {} skipped",
            if r.failed_count == 0 {
                green("●")
            } else {
                yellow("●")
            },
            bold(&score),
            r.passed_count,
            r.failed_count,
            r.skipped_count
        );
        let _ = writeln!(
            o,
            "  {}",
            dim(&if r.cleaned_up {
                format!("test objects under {} removed", r.prefix)
            } else {
                format!(
                    "could not verify that {} is empty: run `oprobe cleanup`",
                    r.prefix
                )
            })
        );
        o
    }

    fn exit_code(&self) -> u8 {
        match self.min_score_pct {
            Some(min) if self.report.score_pct < min => crate::errors::exit::GATE,
            _ => 0,
        }
    }
}

pub async fn run(a: CompatArgs, ctx: &Ctx) -> Result<u8> {
    if a.read_only {
        return Err(OprobeError::new(
            ErrorCode::SafetyReadOnly,
            "compat writes, overwrites and deletes test objects (under its own prefix): refused with --read-only",
        )
        .with_hint("use `oprobe doctor --read-only TARGET` for read-only checks")
        .into());
    }
    let (profile, _) = crate::cli::resolve_profile(
        ctx,
        &a.target,
        &WorkloadArgs::default(),
        &KnobArgs::default(),
        &CostArgs::default(),
    )?;
    let mut target = profile.target.normalized()?;
    operator::prepare_target(&mut target, &Knobs::default()).await?;
    let base = operator::base_operator(&target)?;
    if target.is_network() {
        operator::check_reachable(&base, &target).await?;
    }
    let op = operator::preflight_operator(&base, &target)?;
    let prefix = format!("oprobe-compat-{}/", crate::model::RunId::new());
    ctx.out.info(format!(
        "checking {} (objects under {prefix}, removed afterwards)",
        target.display()
    ));
    let mut report = run_suite(&op, &target.display(), &target.scheme, &prefix).await;
    report.push(checksum_check(&target, &prefix).await);
    let admin = operator::admin_operator(&base, &target)?;
    report.cleaned_up = crate::engine::cleanup::delete_prefix(&admin, &prefix)
        .await
        .verified;
    ctx.out.finish(&CompatOutput {
        report,
        min_score_pct: a.min_score,
    })
}
