//! `oprobe probe`, `oprobe lab`, `oprobe cleanup`.

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use crate::config::TargetSpec;
use crate::engine::cleanup::SweepEntry;
use crate::errors::{ErrorCode, OprobeError};
use crate::net::probe::ProbeReport;
use crate::output::{CommandOutput, OutputFormat};
use crate::report::style::{bold, dim, green, red, yellow};
use crate::report::write_file;
use crate::util::parse_duration;

impl CommandOutput for ProbeReport {
    fn text(&self) -> String {
        super::doctor::render_probe(self)
    }
}

pub async fn cmd_probe(a: ProbeArgs, ctx: &Ctx) -> Result<u8> {
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
        http: a.http.into(),
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
    ctx.out.finish(&r)
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

pub async fn cmd_lab(a: LabArgs, ctx: &Ctx) -> Result<u8> {
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
    ctx.out.finish(&LabOutput {
        endpoint: lab.endpoint_url.clone(),
        tls: a.tls,
        try_command: format!(
            "AWS_ACCESS_KEY_ID=lab AWS_SECRET_ACCESS_KEY=lab oprobe run s3://bench/demo --endpoint {} --proxy none{tls_flag}",
            lab.endpoint_url
        ),
    })?;
    tokio::signal::ctrl_c().await?;
    ctx.out.info(format!(
        "{} served {} requests",
        dim("lab stopped:"),
        lab.requests_served()
    ));
    Ok(0)
}

/// Result of `oprobe cleanup`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CleanupOutput {
    pub target: String,
    pub dry_run: bool,
    pub older_than: String,
    pub entries: Vec<SweepEntry>,
}

impl CommandOutput for CleanupOutput {
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
        o
    }
}

pub async fn cmd_cleanup(a: CleanupArgs, ctx: &Ctx) -> Result<u8> {
    let (p, _) = super::resolve_profile(
        ctx,
        &a.target,
        &WorkloadArgs::default(),
        &KnobArgs::default(),
        &CostArgs::default(),
    )?;
    let target = p.target.normalized()?;
    let base = crate::engine::operator::base_operator(&target)?;
    let admin = crate::engine::operator::admin_operator(&base, &target)?;
    let older = parse_duration(&a.older_than)?;
    let dry_run = a.dry_run || a.read_only;
    if a.read_only && !a.dry_run && ctx.out.format == OutputFormat::Text {
        ctx.out
            .info(dim("--read-only: listing only, nothing is deleted"));
    }
    let entries = crate::engine::cleanup::sweep(&admin, older, a.datasets, dry_run).await?;
    ctx.out.finish(&CleanupOutput {
        target: target.display(),
        dry_run,
        older_than: a.older_than,
        entries,
    })
}
