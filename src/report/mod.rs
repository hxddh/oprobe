//! Renderers of the Report v3 (`crate::model::Report`) and the regression gate.

pub mod brief;

pub mod diff;
pub mod markdown;
pub mod style;
pub mod terminal;

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::model::{REPORT_SCHEMA as SCHEMA, Report, Run};

/// Summary of an auto-tuning session (the tuner's own form; reports embed
/// [`crate::model::TuneReport`]).
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

/// Load a report from a JSON file (a bare report or a `-o json` command result).
pub fn load_json(path: &Path) -> Result<Report> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read report `{}`", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("`{}` is not valid JSON", path.display()))?;
    Report::from_json_value(v).with_context(|| format!("cannot load `{}`", path.display()))
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

/// Write `content` to `path` atomically (temporary file + rename), creating parent directories.
pub fn write_file(path: &Path, content: &str) -> Result<()> {
    crate::store::atomic_write(path, content.as_bytes())
}
