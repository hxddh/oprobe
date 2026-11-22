//! `oprobe probe`, `oprobe lab`, `oprobe cleanup`.

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::input::*;
use crate::config::TargetSpec;
use crate::engine::cleanup::SweepEntry;
use crate::errors::{ErrorCode, OprobeError};
use crate::net::probe::ProbeReport;
use crate::output::{CommandOutput, OutputFormat};
use crate::report::style::{bold, dim, green, red, yellow};
use crate::report::write_file;
use crate::util::parse_duration;

impl CommandOutput for ProbeReport {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![
            crate::app::next::discover(),
            crate::app::next::list_targets(),
        ]
    }

    fn text(&self) -> String {
        super::doctor::render_probe(self)
    }
}

pub async fn probe(_ctx: &Ctx, a: ProbeRequest) -> Result<crate::net::probe::ProbeReport> {
    use crate::net::probe::{ProbeOptions, probe_endpoint};
    let endpoint = if a.endpoint.starts_with("http://") || a.endpoint.starts_with("https://") {
        a.endpoint.clone()
    } else {
        TargetSpec::from_url(&a.endpoint)?
            .normalized()?
            .endpoint()
            .ok_or_else(|| {
                OprobeError::new(
                    ErrorCode::UsageInvalidArgument,
                    format!(
                        "cannot derive an http(s) endpoint from `{}`; pass one explicitly",
                        a.endpoint
                    ),
                )
            })?
    };
    let mut tls = crate::net::TlsClientOptions::default();
    if let Some(p) = &a.ca_cert {
        tls.extra_ca_pem = Some(
            std::fs::read_to_string(p).with_context(|| format!("cannot read `{}`", p.display()))?,
        );
    }
    tls.insecure_skip_verify = a.insecure;
    let opts = ProbeOptions {
        samples: a.samples,
        timeout: parse_duration(&a.timeout)?,
        http: a.http,
        tls,
        proxy: match a.proxy.as_deref() {
            None => None,
            Some("none") => Some(None),
            Some(p) => Some(Some(p.to_owned())),
        },
        certs: true,
        method: if a.get {
            http::Method::GET
        } else {
            http::Method::HEAD
        },
    };
    let r = probe_endpoint(&endpoint, &opts).await?;
    if let Some(p) = &a.json {
        write_file(p, &serde_json::to_string_pretty(&r)?)?;
    }
    Ok(r)
}

/// What `oprobe lab` serves (printed once it listens).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct LabOutput {
    pub endpoint: String,
    pub tls: bool,
    /// A command that benchmarks the lab.
    pub try_command: String,
}

impl CommandOutput for LabOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![
            crate::output::NextStep::new("measure the lab", self.try_command.clone()).with_mcp(
                "start_run",
                serde_json::json!({ "target": "lab://demo", "duration": "5s" }),
            ),
        ]
    }

    fn text(&self) -> String {
        format!(
            "{} S3 lab listening on {}\n  try: {}\n  {}\n",
            green("●"),
            bold(&self.endpoint),
            self.try_command,
            dim("any bucket name works; data lives in memory; Ctrl-C to stop")
        )
    }
}

/// `oprobe lab`: start the server (it serves until the returned handle is dropped).
pub async fn lab(_ctx: &Ctx, a: LabRequest) -> Result<(crate::lab::LabHandle, LabOutput)> {
    use crate::lab::{LabConfig, LabServer, lab_ca_pem};
    if let Some(p) = &a.write_ca {
        write_file(p, lab_ca_pem())?;
    }
    let mut cfg = match &a.scenario {
        Some(s) => crate::lab::Scenario::parse(s)?.config(),
        None => LabConfig::default(),
    };
    // Flags override the scenario where they were given (differ from their defaults).
    cfg.enable_tls = a.tls;
    cfg.enable_http2 = !a.h1_only;
    cfg.throttle_status_code = a.throttle_status;
    cfg.throttle_early_close |= a.throttle_early_close;
    let latency = parse_duration(&a.latency)?;
    if !latency.is_zero() {
        cfg.base_ttfb_delay = latency;
    }
    let complete = parse_duration(&a.complete_delay)?;
    if !complete.is_zero() {
        cfg.multipart_merge_delay = complete;
    }
    if a.straggler_every > 0 {
        cfg.straggler_every_n = a.straggler_every;
        cfg.straggler_delay = parse_duration(&a.straggler)?;
    }
    cfg.max_inflight_before_503 = a.throttle_above.or(cfg.max_inflight_before_503);
    cfg.throttle_every_n_requests = a.throttle_every.or(cfg.throttle_every_n_requests);
    cfg.stream_bandwidth_kib_s = a.bandwidth_kib.or(cfg.stream_bandwidth_kib_s);
    cfg.error_every_n = a.error_every.or(cfg.error_every_n);
    cfg.reset_every_n = a.reset_every.or(cfg.reset_every_n);
    let lab = LabServer::spawn(&a.bind, cfg).await?;
    let tls_flag = if a.tls {
        match &a.write_ca {
            Some(p) => format!(" --ca-cert {}", p.display()),
            None => " --insecure".into(),
        }
    } else {
        String::new()
    };
    let out = LabOutput {
        endpoint: lab.endpoint_url.clone(),
        tls: a.tls,
        try_command: format!(
            "AWS_ACCESS_KEY_ID=lab AWS_SECRET_ACCESS_KEY=lab oprobe run s3://bench/demo --endpoint {} --proxy none{tls_flag}",
            lab.endpoint_url
        ),
    };
    Ok((lab, out))
}

/// Result of `oprobe cleanup`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CleanupOutput {
    pub target: String,
    pub dry_run: bool,
    pub older_than: String,
    pub entries: Vec<SweepEntry>,
    /// The target as given (for next steps).
    #[serde(skip)]
    pub target_arg: String,
}

impl CommandOutput for CleanupOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        use crate::output::NextStep;
        let t = crate::model::spec::shell_quote(&self.target_arg);
        if self.dry_run && !self.entries.is_empty() {
            vec![
                NextStep::new(
                    "delete the prefixes listed",
                    format!("oprobe cleanup {t} --older-than {} --yes", self.older_than),
                )
                .with_mcp(
                    "cleanup",
                    serde_json::json!({
                        "target": self.target_arg,
                        "older_than": self.older_than,
                        "confirm": true
                    }),
                ),
            ]
        } else {
            vec![
                NextStep::new("check the target", format!("oprobe doctor {t}"))
                    .with_mcp("doctor", serde_json::json!({ "target": self.target_arg })),
            ]
        }
    }

    fn text(&self) -> String {
        if self.entries.is_empty() {
            return format!(
                "{} nothing to clean under {} (older than {})\n",
                green("✓"),
                self.target,
                self.older_than
            );
        }
        let mut o = String::new();
        for e in &self.entries {
            let age = e
                .age_s
                .map(|s| format!("{}h{:02}m old", s / 3600, (s % 3600) / 60))
                .unwrap_or_else(|| "dataset".into());
            let status = if self.dry_run {
                yellow("would delete")
            } else if e.deleted {
                green("deleted")
            } else {
                red("partially deleted")
            };
            o += &format!(
                "  {status} {} {}\n",
                e.prefix,
                dim(&format!("({} objects, {age})", e.objects))
            );
        }
        if self.dry_run {
            o += &dim("dry run: nothing was deleted; add --yes to delete\n");
        }
        o
    }
}

pub async fn cleanup(ctx: &Ctx, a: CleanupRequest) -> Result<CleanupOutput> {
    let (p, _) = super::profile::resolve_profile(
        ctx,
        &a.target,
        &WorkloadInput::default(),
        &KnobInput::default(),
        &CostInput::default(),
    )?;
    let target = p.target.normalized()?;
    let base = crate::engine::operator::base_operator(&target)?;
    let admin = crate::engine::operator::admin_operator(&base, &target)?;
    let older = parse_duration(&a.older_than)?;
    // A dry run unless --yes (design v4 §10): cleanup deletes only when asked to.
    let dry_run = !a.yes || a.dry_run || a.read_only;
    if a.read_only && ctx.out.format == OutputFormat::Text {
        ctx.out
            .info(dim("--read-only: listing only, nothing is deleted"));
    }
    let entries = crate::engine::cleanup::sweep(&admin, older, a.datasets, dry_run).await?;
    Ok(CleanupOutput {
        target: target.display(),
        dry_run,
        older_than: a.older_than,
        entries,
        target_arg: a.target.target.clone().unwrap_or_else(|| target.display()),
    })
}
