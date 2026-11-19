//! `oprobe.report/v2`: the persisted result of a run (JSON), plus renderers and the diff gate.

pub mod diff;
pub mod markdown;
pub mod style;
pub mod terminal;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::analysis::cost::{CostReport, cost_report};
use crate::analysis::diagnosis::{Diagnosis, diagnose};
use crate::config::{Knobs, Profile, Sla, WorkloadSpec};
use crate::engine::CleanupSummary;
use crate::trace::Snapshot;

pub const SCHEMA: &str = "oprobe.report/v2";

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
    pub sla: Sla,
    pub result: Snapshot,
    pub diagnosis: Diagnosis,
    pub cost: CostReport,
    pub cleanup: CleanupSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tune: Option<TuneSummary>,
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
        let cost = cost_report(&snapshot, &profile.cost, &t.scheme, endpoint.as_deref());
        let mut workload = profile.workload.clone();
        workload.dataset = Some(workload.effective_dataset());
        workload.mix = workload.effective_mix();
        let mut options = crate::util::redact_options(&t.options);
        options.remove("ca_cert_pem");
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
                proxy: snapshot.connections.proxy.clone(),
            },
            workload,
            knobs: knobs.clone(),
            sla: profile.sla.clone(),
            result: snapshot,
            diagnosis,
            cost,
            cleanup,
            warnings,
            tune: None,
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
                "`{}` uses report schema `{other}`; this oprobe reads `{SCHEMA}`",
                path.display()
            ),
            None if v.get("schema_version").is_some() => bail!(
                "`{}` is an oprobe v1 report; re-run the baseline with oprobe v2 (`oprobe run --json`)",
                path.display()
            ),
            None => bail!("`{}` is not an oprobe report", path.display()),
        }
        serde_json::from_value(v)
            .with_context(|| format!("cannot parse report `{}`", path.display()))
    }
}

/// Pretty JSON of a report (what `--json` writes).
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
