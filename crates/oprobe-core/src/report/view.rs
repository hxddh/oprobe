//! The shared presentation model (design v5 §1): one view per report section, computed once
//! from a [`Report`] and rendered by every front end — the terminal report, Markdown, HTML and
//! the App. A renderer decides layout and markup; what a section *says* (which stages, which
//! labels, which evidence, in which order) is decided here, so the formats cannot drift apart.

use crate::analysis::diagnosis::Severity;
use crate::model::{Finding, Report};

/// The headline numbers (the summary tiles and table).
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryView {
    pub good_ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub failed_ops_count: u64,
    pub error_ratio: f64,
    pub throttled_requests_count: u64,
    pub throttled_request_ratio: f64,
}

impl SummaryView {
    pub fn of(r: &Report) -> Self {
        let s = &r.summary;
        Self {
            good_ops_per_s: s.ops_per_s,
            mib_per_s: s.mib_per_s,
            p50_ms: s.latency.p50,
            p99_ms: s.latency.p99,
            p999_ms: s.latency.p999,
            failed_ops_count: s.failed_ops,
            error_ratio: s.error_rate,
            throttled_requests_count: r.resilience.throttled_responses,
            throttled_request_ratio: s.throttled_request_ratio,
        }
    }
}

/// The SLA verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct SlaView<'a> {
    pub passed: bool,
    pub enforced: bool,
    pub breaches: &'a [String],
}

impl<'a> SlaView<'a> {
    pub fn of(r: &'a Report) -> Self {
        let v = &r.verdict.sla;
        Self {
            passed: v.passed,
            enforced: v.enforced,
            breaches: &v.breaches,
        }
    }
}

/// One layer's share of operation wall time.
#[derive(Clone, Debug, PartialEq)]
pub struct StageShare {
    /// Stable key: `queue`, `connect`, `upload`, `server`, `download`, `retry`, `cache`,
    /// `client`.
    pub key: &'static str,
    /// Short name (`server` is `backend` for in-process targets).
    pub name: &'static str,
    /// Table title (`server (TTFB)`, `retry backoff`).
    pub title: &'static str,
    /// What the layer covers.
    pub description: &'static str,
    /// Share of operation wall time, 0–1.
    pub ratio: f64,
}

/// Where the time goes: every layer, in reading order (queue → client).
#[derive(Clone, Debug, PartialEq)]
pub struct TimeView {
    pub stages: Vec<StageShare>,
}

impl TimeView {
    pub fn of(r: &Report) -> Self {
        let sh = &r.stages.shares;
        let server = if r.target.endpoint.is_some() {
            "server"
        } else {
            "backend"
        };
        let s = |key, name, title, description, pct: f64| StageShare {
            key,
            name,
            title,
            description,
            ratio: pct / 100.0,
        };
        Self {
            stages: vec![
                s(
                    "queue",
                    "queue",
                    "queue",
                    "waiting for permits or connections",
                    sh.queue_pct,
                ),
                s(
                    "connect",
                    "connect",
                    "connect",
                    "DNS + TCP + proxy + TLS (new conns)",
                    sh.connect_pct,
                ),
                s(
                    "upload",
                    "upload",
                    "upload",
                    "sending request bodies",
                    sh.upload_pct,
                ),
                s(
                    "server",
                    server,
                    "server (TTFB)",
                    "time to first byte (server + RTT)",
                    sh.server_pct,
                ),
                s(
                    "download",
                    "download",
                    "download",
                    "receiving response bodies",
                    sh.download_pct,
                ),
                s(
                    "retry",
                    "retry",
                    "retry backoff",
                    "backoff between retry attempts",
                    sh.retry_pct,
                ),
                s(
                    "cache",
                    "cache",
                    "cache",
                    "local cache lookups",
                    sh.cache_pct,
                ),
                s(
                    "client",
                    "client",
                    "client",
                    "OpenDAL / signing / CRC / consumer",
                    sh.client_pct,
                ),
            ],
        }
    }

    /// The largest share (0 when there is none).
    pub fn max_ratio(&self) -> f64 {
        self.stages.iter().map(|s| s.ratio).fold(0.0, f64::max)
    }

    /// The layers in the order of a stacked bar (the order of the request's life: queue,
    /// cache, connect, upload, server, download, retry, client).
    pub fn stacked(&self) -> Vec<&StageShare> {
        [
            "queue", "cache", "connect", "upload", "server", "download", "retry", "client",
        ]
        .iter()
        .filter_map(|k| self.stages.iter().find(|s| s.key == *k))
        .collect()
    }
}

/// One labelled evidence value of a finding.
#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceItem<'a> {
    /// The machine name (`throttled_request_ratio`).
    pub key: &'a str,
    /// Human label (`Throttled requests`).
    pub label: String,
    /// Formatted value with its unit (`4.7%`).
    pub value: String,
}

/// One finding as every renderer shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct FindingView<'a> {
    pub severity: Severity,
    pub code: &'a str,
    pub cause: &'a str,
    pub evidence: &'a str,
    pub advice: &'a str,
    pub items: Vec<EvidenceItem<'a>>,
    /// The command that tests the advice (`oprobe run --from RUN --set …`).
    pub rerun: Option<&'a str>,
    pub op_ids: &'a [u64],
}

impl<'a> FindingView<'a> {
    pub fn of(f: &'a Finding) -> Self {
        Self {
            severity: f.severity,
            code: &f.code,
            cause: &f.cause,
            evidence: &f.evidence.text,
            advice: &f.advice,
            items: f
                .evidence
                .labelled()
                .into_iter()
                .zip(f.evidence.values.keys())
                .map(|((label, value), key)| EvidenceItem {
                    key: key.as_str(),
                    label,
                    value,
                })
                .collect(),
            rerun: f.verification.as_ref().map(|v| v.rerun.as_str()),
            op_ids: &f.evidence.op_ids,
        }
    }
}

/// The findings in report order.
pub fn findings(r: &Report) -> Vec<FindingView<'_>> {
    r.findings.iter().map(FindingView::of).collect()
}

/// The findings, most severe first (stable within a severity).
pub fn findings_by_severity(r: &Report) -> Vec<FindingView<'_>> {
    let mut v = findings(r);
    v.sort_by_key(|f| std::cmp::Reverse(f.severity));
    v
}

/// Bytes, amplification and the cost projection.
#[derive(Clone, Debug, PartialEq)]
pub struct CostView<'a> {
    pub read_amplification: f64,
    pub overread_ratio: f64,
    pub requests_per_op: f64,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    /// `None`: cost does not apply (with the reason).
    pub applicable: bool,
    pub not_applicable_reason: &'a str,
    pub provider: &'a str,
    pub per_million_ops_usd: f64,
    pub requests_usd: f64,
    pub egress_usd: f64,
    pub storage_usd: f64,
    pub monthly_ops: f64,
    pub stored_gib: f64,
    /// `(assumed 1 TiB stored — set --stored-gib)` when the storage is assumed, else empty.
    pub storage_label: String,
}

impl<'a> CostView<'a> {
    pub fn of(r: &'a Report) -> Self {
        let b = &r.bytes;
        let c = &r.cost.selected;
        Self {
            read_amplification: b.read_amplification,
            overread_ratio: b.overread_ratio,
            requests_per_op: b.request_amplification,
            downloaded_bytes: b.backend_down,
            uploaded_bytes: b.backend_up,
            applicable: r.cost.applicable,
            not_applicable_reason: r.cost.not_applicable_reason.as_deref().unwrap_or_default(),
            provider: &c.provider,
            per_million_ops_usd: c.per_million_ops_usd,
            requests_usd: c.requests_usd,
            egress_usd: c.egress_usd,
            storage_usd: c.storage_usd,
            monthly_ops: r.cost.monthly_ops,
            stored_gib: r.cost.stored_gb,
            storage_label: crate::analysis::cost::storage_label(&r.cost),
        }
    }
}

/// Every section of a report, for renderers that show them all.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportView<'a> {
    pub summary: SummaryView,
    pub sla: SlaView<'a>,
    pub time: TimeView,
    pub findings: Vec<FindingView<'a>>,
    pub cost: CostView<'a>,
    /// The run stopped early and no warning says so yet.
    pub partial_note: bool,
    pub warnings: &'a [String],
}

impl<'a> ReportView<'a> {
    pub fn of(r: &'a Report) -> Self {
        Self {
            summary: SummaryView::of(r),
            sla: SlaView::of(r),
            time: TimeView::of(r),
            findings: findings(r),
            cost: CostView::of(r),
            partial_note: r.is_partial()
                && !r.warnings.iter().any(|w| w.starts_with("partial run: ")),
            warnings: &r.warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_cover_every_section_of_the_fixture() {
        let r = crate::report::fixture::report();
        let v = ReportView::of(&r);
        assert_eq!(v.time.stages.len(), 8);
        let sum: f64 = v.time.stages.iter().map(|s| s.ratio).sum();
        assert!(sum > 0.5 && sum < 1.5, "{sum}");
        assert_eq!(v.time.stacked()[1].key, "cache");
        assert_eq!(v.findings.len(), r.findings.len());
        let sorted = findings_by_severity(&r);
        assert!(sorted.windows(2).all(|w| w[0].severity >= w[1].severity));
        for f in &v.findings {
            let src = r.findings.iter().find(|x| x.code == f.code).unwrap();
            assert_eq!(f.items.len(), src.evidence.values.len());
        }
        assert_eq!(v.summary.p99_ms, r.summary.latency.p99);
    }
}
