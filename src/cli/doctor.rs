//! `oprobe doctor` / `oprobe target test`: credentials (and their source), region, clock skew,
//! DNS (every A/AAAA answer), TLS (handshake, resumption, certificate chain and expiry),
//! permissions (list / read / write / stat / delete, tested safely under a run prefix), the
//! provider's capability matrix and the addressing style — then a ready-to-run command.

use std::fmt::Write as _;
use std::time::Instant;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::{CostArgs, DoctorArgs, KnobArgs, WorkloadArgs};
use crate::analysis::hints::hint_for;
use crate::config::TargetSpec;
use crate::engine::operator::{self, RegionSource};
use crate::engine::workloads::CapabilityMatrix;
use crate::net::probe::{ProbeOptions, probe_endpoint};
use crate::net::tlsinfo::CertChain;
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green, red, yellow};
use crate::trace::Quantiles;
use crate::util::fmt_ms;

/// Outcome of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skipped,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Where the credentials come from (as far as it can be told without using them).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CredentialInfo {
    /// `options`, `environment`, `profile`, `web_identity`, `container`, `adc`,
    /// `service_principal`, `workload_identity`, `az_cli`, `instance_metadata`, `anonymous`,
    /// `lab`, `none_needed`, `provider_chain`.
    pub source: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RegionInfo {
    pub region: String,
    pub source: RegionSource,
    /// The region the service reports for the bucket, when it differs from the configured one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DnsInfo {
    pub host: String,
    /// IPv4 answers.
    pub a: Vec<String>,
    /// IPv6 answers.
    pub aaaa: Vec<String>,
    pub lookup: Quantiles,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TlsDoctor {
    pub version: String,
    pub alpn: String,
    pub cipher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Handshakes after the first resumed the session.
    pub resumption: bool,
    pub handshake: Quantiles,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<CertChain>,
}

/// Access tested with real requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Allowed,
    Denied,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Permissions {
    pub list: Access,
    pub read: Access,
    pub write: Access,
    pub stat: Access,
    pub delete: Access,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            list: Access::Skipped,
            read: Access::Skipped,
            write: Access::Skipped,
            stat: Access::Skipped,
            delete: Access::Skipped,
        }
    }
}

/// Result of `oprobe doctor`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DoctorOutput {
    pub target: String,
    pub scheme: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub checks: Vec<DoctorCheck>,
    pub credentials: CredentialInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<RegionInfo>,
    /// S3 bucket addressing: `virtual_host` or `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addressing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsDoctor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_skew_s: Option<f64>,
    pub permissions: Permissions,
    /// What the OpenDAL service supports (drives workloads, the tuner and `compat`).
    pub capabilities: CapabilityMatrix,
    pub failed_count: u64,
    pub warned_count: u64,
    /// No check failed: the target can be benchmarked.
    pub ready: bool,
    /// What to run next (the JSON envelope's `next`).
    #[serde(skip)]
    #[schemars(skip)]
    pub next: Vec<crate::output::NextStep>,
}

impl CommandOutput for DoctorOutput {
    fn next(&self) -> Vec<crate::output::NextStep> {
        self.next.clone()
    }
    fn text(&self) -> String {
        let mut o = format!("{} {}\n", bold("oprobe doctor"), self.target);
        for c in &self.checks {
            let _ = match c.status {
                CheckStatus::Ok => {
                    writeln!(o, "  {} {:<14} {}", green("✓"), c.name, dim(&c.detail))
                }
                CheckStatus::Warn => writeln!(o, "  {} {:<14} {}", yellow("▲"), c.name, c.detail),
                CheckStatus::Fail => {
                    writeln!(o, "  {} {:<14} {}", red("✖"), c.name, red(&c.detail))
                }
                CheckStatus::Skipped => {
                    writeln!(o, "  {} {:<14} {}", dim("·"), dim(&c.name), dim(&c.detail))
                }
            };
            if let Some(h) = &c.hint {
                let _ = writeln!(o, "    {} {h}", yellow("hint:"));
            }
        }
        let (yes, no) = self.capabilities.summary();
        let _ = writeln!(o, "  {} {:<14} {}", dim("·"), "capabilities", yes.join(" "));
        if !no.is_empty() {
            let _ = writeln!(
                o,
                "    {}",
                dim(&format!("not supported: {}", no.join(" ")))
            );
        }
        o.push('\n');
        if self.ready {
            let _ = writeln!(
                o,
                "{} ready to benchmark{}",
                green("●"),
                if self.warned_count > 0 {
                    format!(" ({} warnings)", self.warned_count)
                } else {
                    String::new()
                }
            );
        } else {
            let _ = writeln!(o, "{} {} check(s) failed", red("●"), self.failed_count);
        }
        if let Some(n) = self.next.first() {
            let _ = writeln!(o, "  next: {}", bold(&n.cli));
        }
        o
    }
    fn exit_code(&self) -> u8 {
        if self.ready {
            0
        } else {
            crate::errors::exit::RUNTIME
        }
    }
}

struct Checks {
    list: Vec<DoctorCheck>,
    scheme: String,
}

impl Checks {
    fn push(&mut self, name: &str, status: CheckStatus, detail: &str, hint: Option<String>) {
        self.list.push(DoctorCheck {
            name: name.to_owned(),
            status,
            detail: detail.to_owned(),
            hint,
        });
    }
    fn ok(&mut self, name: &str, detail: &str) {
        self.push(name, CheckStatus::Ok, detail, None);
    }
    fn warn(&mut self, name: &str, detail: &str) {
        self.push(name, CheckStatus::Warn, detail, None);
    }
    fn fail(&mut self, name: &str, err: &str) {
        let hint = hint_for(err, &self.scheme).map(|h| {
            if h.steps.is_empty() {
                h.title
            } else {
                format!("{}: {}", h.title, h.steps.join("; "))
            }
        });
        self.push(name, CheckStatus::Fail, err, hint);
    }
}

fn env_set(k: &str) -> bool {
    std::env::var(k).is_ok_and(|v| !v.is_empty())
}

fn home_file(rel: &str) -> bool {
    dirs::home_dir().is_some_and(|h| h.join(rel).exists())
}

/// The credential source the service's own chain will use (first match, in its order).
pub fn credential_source(t: &TargetSpec) -> CredentialInfo {
    let has = |k: &str| t.options.get(k).is_some_and(|v| !v.is_empty());
    let info = |s: &str, d: &str| CredentialInfo {
        source: s.into(),
        detail: d.into(),
    };
    if t.url.as_deref().is_some_and(|u| u.starts_with("lab://")) {
        return info("lab", "built-in lab credentials");
    }
    if ["skip_signature", "allow_anonymous"]
        .iter()
        .any(|k| t.options.get(*k).is_some_and(|v| v == "true"))
    {
        return info("anonymous", "unsigned requests");
    }
    match t.scheme.as_str() {
        "memory" | "fs" => info("none_needed", "in-process backend"),
        "s3" => {
            if has("access_key_id") {
                info("options", "access_key_id from the target options")
            } else if env_set("AWS_ACCESS_KEY_ID") {
                info("environment", "AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY")
            } else if has("role_arn") {
                info("options", "assume role_arn from the target options")
            } else if env_set("AWS_WEB_IDENTITY_TOKEN_FILE") {
                info("web_identity", "AWS_WEB_IDENTITY_TOKEN_FILE (IRSA / OIDC)")
            } else if env_set("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
                || env_set("AWS_CONTAINER_CREDENTIALS_FULL_URI")
            {
                info("container", "ECS / EKS pod identity endpoint")
            } else if env_set("AWS_PROFILE") || has("profile") {
                let p = t
                    .options
                    .get("profile")
                    .cloned()
                    .or_else(|| std::env::var("AWS_PROFILE").ok())
                    .unwrap_or_default();
                info(
                    "profile",
                    &format!("profile `{p}` (~/.aws/config, ~/.aws/credentials)"),
                )
            } else if home_file(".aws/credentials") || home_file(".aws/config") {
                info("profile", "default profile (~/.aws/credentials)")
            } else {
                info("instance_metadata", "EC2 instance metadata (IMDS), if any")
            }
        }
        "gcs" => {
            if has("credential") || has("credential_path") || has("token") {
                info("options", "credential from the target options")
            } else if env_set("GOOGLE_APPLICATION_CREDENTIALS") {
                info(
                    "environment",
                    "GOOGLE_APPLICATION_CREDENTIALS (service account file)",
                )
            } else if home_file(".config/gcloud/application_default_credentials.json") {
                info("adc", "gcloud application default credentials")
            } else {
                info("instance_metadata", "GCE metadata server, if any")
            }
        }
        "azblob" | "azdls" | "azfile" => {
            if has("account_key") || has("sas_token") {
                info("options", "account key / SAS token from the target options")
            } else if env_set("AZURE_STORAGE_ACCOUNT_KEY") || env_set("AZURE_STORAGE_SAS_TOKEN") {
                info(
                    "environment",
                    "AZURE_STORAGE_ACCOUNT_KEY / AZURE_STORAGE_SAS_TOKEN",
                )
            } else if env_set("AZURE_FEDERATED_TOKEN_FILE") {
                info("workload_identity", "AZURE_FEDERATED_TOKEN_FILE")
            } else if env_set("AZURE_CLIENT_SECRET") {
                info("service_principal", "AZURE_CLIENT_ID / AZURE_CLIENT_SECRET")
            } else if home_file(".azure/azureProfile.json") {
                info("az_cli", "az CLI login (~/.azure)")
            } else {
                info("instance_metadata", "Azure managed identity (IMDS), if any")
            }
        }
        _ => {
            if [
                "access_key_id",
                "account_key",
                "password",
                "token",
                "secret_access_key",
            ]
            .iter()
            .any(|k| has(k))
            {
                info("options", "credentials from the target options")
            } else {
                info(
                    "provider_chain",
                    "the service's own environment / config lookup",
                )
            }
        }
    }
}

/// Check `target` (the doctor behind `oprobe doctor`, `oprobe target test` and MCP).
/// `target_arg` is how the user named it (for the suggested command).
pub async fn doctor(mut target: TargetSpec, read_only: bool, target_arg: &str) -> DoctorOutput {
    let mut c = Checks {
        list: Vec::new(),
        scheme: target.scheme.clone(),
    };
    let mut out = DoctorOutput {
        target: target.display(),
        scheme: target.scheme.clone(),
        endpoint: target.endpoint().filter(|_| target.is_network()),
        checks: Vec::new(),
        credentials: credential_source(&target),
        region: None,
        addressing: (target.scheme == "s3").then(|| {
            if target
                .options
                .get("enable_virtual_host_style")
                .is_some_and(|v| v == "true")
            {
                "virtual_host".to_owned()
            } else {
                "path".to_owned()
            }
        }),
        dns: None,
        tls: None,
        clock_skew_s: None,
        permissions: Permissions::default(),
        capabilities: CapabilityMatrix::default(),
        failed_count: 0,
        warned_count: 0,
        ready: false,
        next: Vec::new(),
    };
    run_checks(&mut target, read_only, &mut c, &mut out).await;
    out.failed_count = c
        .list
        .iter()
        .filter(|x| x.status == CheckStatus::Fail)
        .count() as u64;
    out.warned_count = c
        .list
        .iter()
        .filter(|x| x.status == CheckStatus::Warn)
        .count() as u64;
    out.ready = out.failed_count == 0;
    out.checks = c.list;
    let arg = if target_arg.contains(' ') {
        format!("'{target_arg}'")
    } else {
        target_arg.to_owned()
    };
    use crate::output::NextStep;
    let target = serde_json::json!({ "target": target_arg });
    out.next = vec![if !out.ready {
        NextStep::new(
            "check again after fixing the failed checks",
            format!("oprobe doctor {arg}"),
        )
        .with_mcp("doctor", target)
    } else if read_only || out.permissions.write != Access::Allowed {
        NextStep::new(
            "read-only: measure your own objects",
            format!("oprobe run {arg} --existing <PREFIX>"),
        )
        .with_mcp(
            "start_run",
            serde_json::json!({ "target": target_arg, "existing": "<PREFIX>" }),
        )
    } else {
        NextStep::new(
            "measure a mixed workload",
            format!("oprobe run {arg} --preset mixed --duration 30s"),
        )
        .with_mcp(
            "start_run",
            serde_json::json!({ "target": target_arg, "preset": "mixed", "duration": "30s" }),
        )
    }];
    out
}

async fn run_checks(
    target: &mut TargetSpec,
    read_only: bool,
    c: &mut Checks,
    out: &mut DoctorOutput,
) {
    // 1. Configuration
    match target.resolved_options() {
        Ok(opts) => {
            let shown: Vec<String> = crate::util::redact_options(&opts)
                .into_iter()
                .filter(|(k, _)| k != "ca_cert_pem")
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            c.ok(
                "configuration",
                &format!("{} · {}", target.scheme, shown.join(" ")),
            );
        }
        Err(e) => {
            c.fail("configuration", &format!("{e:#}"));
            return;
        }
    }
    let cr = &out.credentials;
    if cr.source == "instance_metadata" {
        c.warn(
            "credentials",
            &format!("none configured: relying on {}", cr.detail),
        );
    } else {
        c.ok("credentials", &format!("{} ({})", cr.source, cr.detail));
    }
    if let Some(a) = &out.addressing {
        c.ok(
            "addressing",
            &format!(
                "{} style{}",
                a.replace('_', "-"),
                if a == "path" {
                    " (enable_virtual_host_style=false)"
                } else {
                    ""
                }
            ),
        );
    }

    // 2. Network path: DNS, TCP, TLS (chain, resumption), HTTP, clock.
    if let Some(ep) = target.endpoint().filter(|_| target.is_network()) {
        let mut tls = match operator::tls_options(target) {
            Ok(t) => t,
            Err(e) => {
                c.fail("tls", &format!("{e:#}"));
                return;
            }
        };
        tls.alpn_protocols.clear();
        let opts = ProbeOptions {
            samples: 3,
            tls,
            proxy: operator::proxy_setting(target),
            certs: true,
            ..Default::default()
        };
        match probe_endpoint(&ep, &opts).await {
            Ok(r) => {
                let (aaaa, a): (Vec<String>, Vec<String>) =
                    r.resolved.iter().cloned().partition(|ip| ip.contains(':'));
                match &r.proxy {
                    Some(p) => c.ok(
                        "proxy",
                        &format!(
                            "{p} → CONNECT {}:{} in {}",
                            r.host,
                            r.port,
                            fmt_ms(r.proxy_connect.p50)
                        ),
                    ),
                    None => c.ok(
                        "dns",
                        &format!(
                            "{} → {} A{} in {} (p50 of 3)",
                            r.host,
                            a.len(),
                            if aaaa.is_empty() {
                                String::new()
                            } else {
                                format!(" + {} AAAA", aaaa.len())
                            },
                            fmt_ms(r.dns.p50)
                        ),
                    ),
                }
                out.dns = Some(DnsInfo {
                    host: r.host.clone(),
                    a,
                    aaaa,
                    lookup: r.dns,
                });
                c.ok("tcp", &format!("port {} in {}", r.port, fmt_ms(r.tcp.p50)));
                if r.tls {
                    let first = r.handshakes.first();
                    let resumption = r.handshakes.iter().skip(1).any(|h| h.resumed);
                    let group = first.and_then(|h| h.group.clone());
                    c.ok(
                        "tls",
                        &format!(
                            "{} {} {}{} in {} · {}",
                            r.tls_version,
                            r.alpn,
                            r.cipher,
                            group
                                .as_deref()
                                .map(|g| format!(" {g}"))
                                .unwrap_or_default(),
                            fmt_ms(r.tls_handshake.p50),
                            if resumption {
                                "sessions resume"
                            } else {
                                "no resumption"
                            }
                        ),
                    );
                    if let Some(chain) = &r.cert_chain {
                        let leaf = chain.certs.first();
                        let detail = format!(
                            "{} cert(s), {} B · {} issued by {} · expires {}",
                            chain.certs.len(),
                            chain.total_bytes,
                            leaf.map(|l| l.subject.as_str()).unwrap_or("?"),
                            leaf.map(|l| l.issuer.as_str()).unwrap_or("?"),
                            leaf.map(|l| l.not_after.as_str()).unwrap_or("?")
                        );
                        match chain.min_expiry_days() {
                            Some(d) if d < 0 => {
                                c.fail("certificate", &format!("expired: {detail}"))
                            }
                            Some(d) if d < 14 => {
                                c.warn("certificate", &format!("expires in {d} days: {detail}"))
                            }
                            _ => c.ok("certificate", &detail),
                        }
                    }
                    out.tls = Some(TlsDoctor {
                        version: r.tls_version.clone(),
                        alpn: r.alpn.clone(),
                        cipher: r.cipher.clone(),
                        group,
                        resumption,
                        handshake: r.tls_handshake,
                        chain: r.cert_chain.clone(),
                    });
                }
                c.ok(
                    "http",
                    &format!(
                        "status {} · cold TTFB {} · warm {}",
                        r.samples.first().map(|s| s.status).unwrap_or(0),
                        fmt_ms(r.cold_ttfb.p50),
                        fmt_ms(r.warm_ttfb.p50)
                    ),
                );
                out.clock_skew_s = r.clock_skew_s;
                match r.clock_skew_s {
                    Some(s) if s.abs() > 900.0 => c.fail(
                        "clock",
                        &format!(
                            "RequestTimeTooSkewed risk: local clock differs from server by {s:.0}s"
                        ),
                    ),
                    Some(s) if s.abs() > 60.0 => c.warn(
                        "clock",
                        &format!("local clock differs from server by {s:.0}s"),
                    ),
                    Some(s) => c.ok("clock", &format!("skew {s:+.1}s")),
                    None => {}
                }
            }
            Err(e) => {
                c.fail("network", &format!("{e:#}"));
                return;
            }
        }
    }

    // 3. Region (S3): configured, from the environment, or detected from the service.
    if let Some((region, source)) = operator::s3_region(target).await {
        let detected = if source == RegionSource::Detected {
            None
        } else {
            operator::detect_s3_region(target)
                .await
                .filter(|d| *d != region)
        };
        match (&source, &detected) {
            (_, Some(d)) => c.warn(
                "region",
                &format!("configured {region}, but the bucket reports {d}: requests may be redirected (PermanentRedirect)"),
            ),
            (RegionSource::Default, None) => c.warn(
                "region",
                &format!("{region} (default: not configured and not detected)"),
            ),
            _ => c.ok(
                "region",
                &format!("{region} ({})", serde_json::to_value(&source).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()),
            ),
        }
        if source == RegionSource::Detected {
            target.options.insert("region".into(), region.clone());
        }
        out.region = Some(RegionInfo {
            region,
            source,
            detected,
        });
    }

    // 4. Capabilities and permissions.
    let base = match operator::base_operator(target) {
        Ok(b) => b,
        Err(e) => {
            c.fail("backend", &format!("{e:#}"));
            return;
        }
    };
    out.capabilities = CapabilityMatrix::from(&base.info().capability());
    let admin = match operator::preflight_operator(&base, target) {
        Ok(a) => a,
        Err(e) => {
            c.fail("backend", &format!("{e:#}"));
            return;
        }
    };
    let access = |e: &opendal::Error| {
        if e.kind() == opendal::ErrorKind::PermissionDenied {
            Access::Denied
        } else {
            Access::Failed
        }
    };
    let t = Instant::now();
    match admin.list_with("").limit(5).await {
        Ok(entries) => {
            out.permissions.list = Access::Allowed;
            c.ok(
                "list",
                &format!(
                    "{} entries visible at root in {}",
                    entries.len(),
                    fmt_ms(t.elapsed().as_secs_f64() * 1000.0)
                ),
            )
        }
        Err(e) => {
            out.permissions.list = access(&e);
            c.fail("list", &e.to_string())
        }
    }
    if read_only {
        c.push(
            "write",
            CheckStatus::Skipped,
            "skipping write/read/delete checks (--read-only)",
            None,
        );
        return;
    }
    let key = format!(
        "oprobe-run-{}-doctor/probe.bin",
        crate::util::compact_utc_now()
    );
    let (data, crc) = crate::engine::dataset::generate_payload(7, 256 * 1024);
    let t = Instant::now();
    match admin.write(&key, data).await {
        Ok(_) => {
            out.permissions.write = Access::Allowed;
            c.ok(
                "write",
                &format!("256 KiB in {}", fmt_ms(t.elapsed().as_secs_f64() * 1000.0)),
            );
            let t = Instant::now();
            match admin.read(&key).await {
                Ok(buf) => {
                    out.permissions.read = Access::Allowed;
                    let got = buf
                        .clone()
                        .fold(0u32, |a, ch| crate::util::crc32c_append(a, &ch));
                    if got == crc {
                        c.ok(
                            "read + crc32c",
                            &format!("verified in {}", fmt_ms(t.elapsed().as_secs_f64() * 1000.0)),
                        );
                    } else {
                        c.fail("read + crc32c", "data read back differs from data written");
                    }
                }
                Err(e) => {
                    out.permissions.read = access(&e);
                    c.fail("read", &e.to_string())
                }
            }
            match admin.stat(&key).await {
                Ok(m) if m.content_length() == 256 * 1024 => {
                    out.permissions.stat = Access::Allowed;
                    c.ok("stat", "size matches")
                }
                Ok(m) => {
                    out.permissions.stat = Access::Allowed;
                    c.warn("stat", &format!("unexpected size {}", m.content_length()))
                }
                Err(e) => {
                    out.permissions.stat = access(&e);
                    c.fail("stat", &e.to_string())
                }
            }
            match admin.delete(&key).await {
                Ok(_) => {
                    out.permissions.delete = Access::Allowed;
                    c.ok("delete", "test object removed")
                }
                Err(e) => {
                    out.permissions.delete = access(&e);
                    c.fail("delete", &e.to_string())
                }
            }
            let _ = admin
                .delete(key.rsplit_once('/').map(|(d, _)| d).unwrap_or(&key))
                .await;
        }
        Err(e) => {
            out.permissions.write = access(&e);
            c.fail("write", &e.to_string())
        }
    }
}

pub async fn run(a: DoctorArgs, ctx: &Ctx) -> Result<u8> {
    let (profile, _) = super::resolve_profile(
        ctx,
        &a.target,
        &WorkloadArgs::default(),
        &KnobArgs::default(),
        &CostArgs::default(),
    )?;
    let target = profile.target.normalized()?;
    let arg = a.target.target.clone().unwrap_or_else(|| target.display());
    let out = doctor(target, a.read_only, &arg).await;
    ctx.out.finish(&out)
}

pub fn render_probe(r: &crate::net::probe::ProbeReport) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{} {}", bold("oprobe probe"), r.endpoint);
    match &r.proxy {
        Some(p) => {
            let _ = writeln!(
                o,
                "  {} {}:{} via proxy {p} {}",
                dim("target"),
                r.host,
                r.port,
                dim("(the proxy resolves the name; dns/tcp below are to the proxy)")
            );
        }
        None => {
            let _ = writeln!(
                o,
                "  {} {}:{} → {}",
                dim("target"),
                r.host,
                r.port,
                r.resolved.join(", ")
            );
        }
    }
    if r.tls {
        let group = r
            .handshakes
            .first()
            .and_then(|h| h.group.as_deref())
            .map(|g| format!(" · {g}"))
            .unwrap_or_default();
        let resumed = r.handshakes.iter().filter(|h| h.resumed).count();
        let _ = writeln!(
            o,
            "  {} {} · ALPN {} · {}{group} · {resumed}/{} handshakes resumed",
            dim("tls   "),
            r.tls_version,
            r.alpn,
            r.cipher,
            r.handshakes.len()
        );
        if let Some(leaf) = r.cert_chain.as_ref().and_then(|c| c.certs.first()) {
            let _ = writeln!(
                o,
                "  {} {} by {} · expires {} ({} days) · chain {} B",
                dim("cert  "),
                leaf.subject,
                leaf.issuer,
                leaf.not_after,
                leaf.expires_in_days,
                r.cert_chain.as_ref().map_or(0, |c| c.total_bytes)
            );
        }
    }
    let _ = writeln!(
        o,
        "\n  {:<18} {:>9} {:>9} {:>9}",
        dim("stage"),
        dim("p50"),
        dim("p99"),
        dim("max")
    );
    let row = |o: &mut String, name: &str, q: &crate::trace::Quantiles| {
        if q.count > 0 {
            let _ = writeln!(
                o,
                "  {:<18} {:>9} {:>9} {:>9}",
                name,
                fmt_ms(q.p50),
                fmt_ms(q.p99),
                fmt_ms(q.max)
            );
        }
    };
    row(&mut o, "dns", &r.dns);
    row(&mut o, "tcp connect", &r.tcp);
    row(&mut o, "proxy CONNECT", &r.proxy_connect);
    row(&mut o, "tls handshake", &r.tls_handshake);
    row(&mut o, "ttfb (cold conn)", &r.cold_ttfb);
    row(&mut o, "ttfb (warm conn)", &r.warm_ttfb);
    let _ = writeln!(
        o,
        "\n  a request on a {} connection costs {} vs {} on a {} one ({:.1}×)",
        bold("new"),
        bold(&fmt_ms(r.cold_request_ms)),
        bold(&fmt_ms(r.warm_request_ms)),
        bold("pooled"),
        if r.warm_request_ms > 0.0 {
            r.cold_request_ms / r.warm_request_ms
        } else {
            0.0
        }
    );
    if let Some(s) = r.clock_skew_s {
        let txt = format!("clock skew vs server: {s:+.1}s");
        let _ = writeln!(
            o,
            "  {}",
            if s.abs() > 900.0 {
                red(&txt)
            } else {
                dim(&txt)
            }
        );
    }
    o
}
