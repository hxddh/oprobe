//! `oprobe.report/v3`: the persisted result of a run (JSON), plus renderers and the diff gate.

pub mod diff;
pub mod markdown;
pub mod style;
pub mod terminal;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::analysis::cost::{CostReport, cost_report};
use crate::analysis::diagnosis::{Diagnosis, SlaVerdict, check_sla, diagnose};
use crate::config::{Knobs, Profile, Sla, WorkloadSpec};
use crate::engine::CleanupSummary;
use crate::trace::Snapshot;

pub const SCHEMA: &str = "oprobe.report/v3";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub os: String,
    pub arch: String,
    pub cpus: usize,
}

impl HostInfo {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub display: String,
    pub scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Backend options with secrets redacted.
    pub options: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// Summary of an auto-tuning session embedded in the winner's report.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TuneSummary {
    pub objective: String,
    pub trials: Vec<crate::tune::Trial>,
    pub baseline_trial: usize,
    pub winner_trial: usize,
    /// Throughput change of the winner vs baseline (fraction, e.g. 0.42 = +42%).
    pub improvement: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_ceiling: Option<usize>,
    /// No trial met the SLA: the winner is the least-bad configuration, not a recommendation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_feasible_trial: bool,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema: String,
    pub oprobe_version: String,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub host: HostInfo,
    pub target: TargetInfo,
    pub workload: WorkloadSpec,
    pub knobs: Knobs,
    /// What OpenDAL actually used (e.g. the write chunk after the service's part-size limits).
    pub effective_knobs: crate::trace::EffectiveKnobs,
    pub sla: Sla,
    /// The result checked against `sla` (`oprobe run` exits 3 when enforced and breached).
    pub sla_verdict: SlaVerdict,
    pub result: Snapshot,
    pub diagnosis: Diagnosis,
    pub cost: CostReport,
    pub cleanup: CleanupSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tune: Option<TuneSummary>,
    /// The run was stopped early (Ctrl-C / Stop): results cover a shorter window than
    /// configured. `oprobe diff` warns about such reports.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}

impl Report {
    pub fn build(
        profile: &Profile,
        knobs: &Knobs,
        snapshot: Snapshot,
        cleanup: CleanupSummary,
        warnings: Vec<String>,
    ) -> Self {
        let t = &profile.target;
        let endpoint = t.endpoint();
        let diagnosis = diagnose(&snapshot, knobs, &profile.sla, t.is_network());
        let sla_verdict = check_sla(&snapshot, &profile.sla);
        let cost = cost_report(&snapshot, &profile.cost, &t.scheme, endpoint.as_deref());
        let mut workload = profile.workload.clone();
        workload.dataset = Some(workload.effective_dataset());
        workload.mix = workload.effective_mix();
        let mut options = crate::util::redact_options(&t.options);
        options.remove("ca_cert_pem");
        let interrupted = snapshot.interrupted;
        let effective_knobs = snapshot.effective_knobs.clone();
        let mut warnings = warnings;
        let chunked_writes = workload
            .mix
            .iter()
            .any(|m| m.weight > 0 && matches!(m.op, crate::config::OpKind::MultipartWrite { .. }));
        if chunked_writes && effective_knobs.write_chunk_clamped() {
            warnings.push(format!(
                "chunk_size {} is outside {}'s multipart part-size limits: OpenDAL wrote {} parts (the requested size was not measured)",
                crate::util::ByteSize(effective_knobs.requested_chunk_size),
                t.scheme,
                crate::util::ByteSize(effective_knobs.write_chunk_size)
            ));
        }
        Self {
            schema: SCHEMA.to_owned(),
            oprobe_version: env!("CARGO_PKG_VERSION").to_owned(),
            generated_at: crate::util::now_rfc3339(),
            label: profile.name.clone(),
            host: HostInfo::current(),
            target: TargetInfo {
                display: t.display(),
                scheme: t.scheme.clone(),
                endpoint: if t.is_network() { endpoint } else { None },
                options,
                proxy: snapshot
                    .connections
                    .proxy
                    .as_deref()
                    .map(crate::util::strip_url_credentials),
            },
            workload,
            knobs: knobs.clone(),
            effective_knobs,
            sla: profile.sla.clone(),
            sla_verdict,
            result: snapshot,
            diagnosis,
            cost,
            cleanup,
            warnings,
            tune: None,
            interrupted,
        }
    }

    pub fn save_json(&self, path: &Path) -> Result<()> {
        write_file(path, &serde_json::to_string_pretty(self)?)
    }

    pub fn load_json(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read report `{}`", path.display()))?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("`{}` is not valid JSON", path.display()))?;
        match v.get("schema").and_then(|s| s.as_str()) {
            Some(SCHEMA) => {}
            Some(other) => bail!(
                "`{}` uses report schema `{other}`; this oprobe reads `{SCHEMA}`: re-run it with this oprobe (`oprobe run --json`)",
                path.display()
            ),
            None if v.get("schema_version").is_some() => bail!(
                "`{}` is an oprobe v1 report; re-run the baseline with this oprobe (`oprobe run --json`)",
                path.display()
            ),
            None => bail!("`{}` is not an oprobe report", path.display()),
        }
        serde_json::from_value(v)
            .with_context(|| format!("cannot parse report `{}`", path.display()))
    }
}

/// Pretty JSON of a report (what `--json` writes).
/// `http/1.1 · TLSv1.3 · TLS13_AES_128_GCM_SHA256`, skipping parts the transport did not report.
pub fn connection_protocol(c: &crate::trace::ConnStats) -> String {
    let mut parts = vec![if c.alpn.is_empty() {
        "http/1.1".to_owned()
    } else {
        c.alpn.clone()
    }];
    parts.push(if c.tls_version.is_empty() {
        "plain".to_owned()
    } else {
        c.tls_version.clone()
    });
    if !c.cipher.is_empty() {
        parts.push(c.cipher.clone());
    }
    parts.join(" · ")
}

/// `dns 1.2ms/3.4ms · tcp … · proxy … · tls …` (p50/p99), only for phases that took time.
pub fn dial_phases(c: &crate::trace::ConnStats) -> String {
    use crate::util::fmt_ms;
    [
        ("dns", &c.dns),
        ("tcp", &c.tcp),
        ("proxy", &c.proxy_connect),
        ("tls", &c.tls),
    ]
    .iter()
    .filter(|(_, q)| q.count > 0 && q.p99 > 0.0)
    .map(|(n, q)| format!("{n} {}/{}", fmt_ms(q.p50), fmt_ms(q.p99)))
    .collect::<Vec<_>>()
    .join(" · ")
}

pub fn to_json(r: &Report) -> String {
    serde_json::to_string_pretty(r).unwrap_or_default()
}

pub fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create directory `{}`", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("cannot write `{}`", path.display()))
}
