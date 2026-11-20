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

/// Requests the suite sends at most (multipart parts included), for `--dry-run`.
pub const SUITE_MAX_REQUESTS: u64 = 120;

/// Result of `compat --dry-run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CompatPlan {
    pub target: String,
    /// Prefix the suite writes under (`oprobe-run-<id>/`; `oprobe cleanup` sweeps it).
    pub run_prefix: String,
    /// Upper bound of the requests the suite sends.
    pub estimated_requests_count: u64,
    /// Upper bound of the bytes uploaded (multipart checks write 3 parts of the minimum size).
    pub estimated_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    pub budgets: crate::guard::Budgets,
}

impl CommandOutput for CompatPlan {
    fn text(&self) -> String {
        format!(
            "{} {} · writes, overwrites and deletes test objects under {}\n  ≤{} requests · ≤{} uploaded · cost {}\n{}",
            bold("compat plan"),
            self.target,
            self.run_prefix,
            self.estimated_requests_count,
            crate::util::fmt_bytes(self.estimated_bytes),
            self.estimated_cost_usd
                .map(crate::util::fmt_usd)
                .unwrap_or_else(|| "not applicable".into()),
            dim("dry run: nothing was sent to the target\n")
        )
    }
}

pub async fn run(a: CompatArgs, ctx: &Ctx) -> Result<u8> {
    if a.safety.read_only {
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
    let budgets = crate::cli::safety_budgets(ctx, &a.safety)?;
    // Writes stay under the standard run prefix, so `oprobe cleanup` and
    // `--idempotency-key` apply as for every other command.
    let run_prefix = match &a.safety.idempotency_key {
        Some(k) => crate::model::ids::idempotent_run_prefix(k),
        None => crate::model::RunId::new().run_prefix(),
    };
    if a.safety.dry_run {
        let min_part = 5u64 * 1024 * 1024;
        let price = crate::guard::usd_per_request(&profile);
        return ctx.out.finish(&CompatPlan {
            target: profile.target.display(),
            run_prefix,
            estimated_requests_count: SUITE_MAX_REQUESTS,
            estimated_bytes: 4 * min_part + 1024 * 1024,
            estimated_cost_usd: price.map(|p| p * SUITE_MAX_REQUESTS as f64),
            budgets,
        });
    }
    let mut target = profile.target.normalized()?;
    operator::prepare_target(&mut target, &Knobs::default()).await?;
    let base = operator::base_operator(&target)?;
    if target.is_network() {
        operator::check_reachable(&base, &target).await?;
    }
    // Every request of the suite feeds the guard (budgets stop the suite; cleanup still runs).
    let cancel = crate::engine::Cancel::new();
    let guard = crate::engine::measure::guard_for(&budgets, &profile);
    let recorder = crate::trace::Recorder::new();
    if let Some(g) = &guard {
        g.arm(cancel.clone());
        recorder.set_guard(g.clone());
    }
    let op = operator::preflight_operator_with(&base, &target, Some(recorder))?;
    ctx.out.info(format!(
        "checking {} (objects under {run_prefix}, removed afterwards)",
        target.display()
    ));
    let suite = async {
        let mut report = run_suite(&op, &target.display(), &target.scheme, &run_prefix).await;
        report.push(checksum_check(&target, &run_prefix).await);
        report
    };
    let report = tokio::select! {
        r = suite => Some(r),
        _ = cancel.cancelled() => None,
    };
    let admin = operator::admin_operator(&base, &target)?;
    let cleaned_up = crate::engine::cleanup::delete_prefix(&admin, &run_prefix)
        .await
        .verified;
    if let Some(b) = guard.as_ref().and_then(|g| g.breach()) {
        if !cleaned_up {
            ctx.out.warn(format!(
                "could not verify that {run_prefix} is empty: run `oprobe cleanup`"
            ));
        }
        return Err(b.to_error());
    }
    let mut report = report
        .ok_or_else(|| OprobeError::new(ErrorCode::RunInterrupted, "compat was interrupted"))?;
    report.cleaned_up = cleaned_up;
    ctx.out.finish(&CompatOutput {
        report,
        min_score_pct: a.min_score,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compat_writes_under_the_run_prefix_and_honours_budgets() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            crate::output::Output::silent(),
            crate::store::Paths {
                config_file: dir.path().join("config.toml"),
                data_dir: dir.path().join("data"),
            },
        );
        let args = |max_requests: Option<u64>, dry_run: bool| CompatArgs {
            target: crate::cli::args::TargetArgs {
                target: Some("memory://".into()),
                ..Default::default()
            },
            safety: crate::cli::args::SafetyArgs {
                dry_run,
                max_requests,
                idempotency_key: Some("compat-test".into()),
                ..Default::default()
            },
            min_score: None,
        };
        assert_eq!(run(args(None, true), &ctx).await.unwrap(), 0);
        assert_eq!(run(args(None, false), &ctx).await.unwrap(), 0);
        let e = run(args(Some(3), false), &ctx).await.unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "memory").code,
            ErrorCode::SafetyBudgetExceeded
        );
        assert!(crate::model::ids::idempotent_run_prefix("compat-test").starts_with("oprobe-run-"));
    }
}
