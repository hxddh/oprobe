//! Cost model: measured per-operation request mix and bytes × provider list prices,
//! projected to a monthly bill and compared across providers.

use serde::{Deserialize, Serialize};

use crate::config::CostSpec;
use crate::trace::{BillingClass, Snapshot};

/// Provider list prices in USD (request prices per million, data per GB = 1e9 bytes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    pub provider: String,
    pub storage_gb_month: f64,
    /// PUT / POST / COPY / multipart part & complete.
    pub write_per_million: f64,
    pub list_per_million: f64,
    /// GET / HEAD.
    pub read_per_million: f64,
    #[serde(default)]
    pub delete_per_million: f64,
    /// Internet egress per GB.
    pub egress_per_gb: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Public list prices (standard/hot tier, cheapest common region). Verify against your
/// contract; override with `[cost.pricing]` in the profile.
pub const PRICING_AS_OF: &str = "2025-06";

pub fn presets() -> Vec<(&'static str, Pricing)> {
    let p = |provider: &str, storage, write, list, read, egress, note: &str| Pricing {
        provider: provider.to_owned(),
        storage_gb_month: storage,
        write_per_million: write,
        list_per_million: list,
        read_per_million: read,
        delete_per_million: 0.0,
        egress_per_gb: egress,
        note: note.to_owned(),
    };
    vec![
        (
            "aws-s3",
            p(
                "AWS S3 Standard",
                0.023,
                5.00,
                5.00,
                0.40,
                0.09,
                "us-east-1, first 50 TB tier",
            ),
        ),
        (
            "gcs",
            p(
                "Google Cloud Storage Standard",
                0.020,
                5.00,
                5.00,
                0.40,
                0.12,
                "us-central1; class A/B operations",
            ),
        ),
        (
            "azure-blob",
            p(
                "Azure Blob Hot (LRS)",
                0.0184,
                6.50,
                6.50,
                0.50,
                0.087,
                "East US, hot tier",
            ),
        ),
        (
            "cloudflare-r2",
            p(
                "Cloudflare R2 Standard",
                0.015,
                4.50,
                4.50,
                0.36,
                0.0,
                "no egress fees",
            ),
        ),
        (
            "backblaze-b2",
            p(
                "Backblaze B2",
                0.006,
                0.0,
                4.00,
                0.40,
                0.01,
                "egress free up to 3x stored data",
            ),
        ),
        (
            "aliyun-oss",
            p(
                "Alibaba Cloud OSS Standard",
                0.017,
                1.00,
                1.00,
                1.00,
                0.074,
                "international pricing, approx.",
            ),
        ),
    ]
}

pub fn preset(name: &str) -> Option<Pricing> {
    let n = name.to_ascii_lowercase();
    presets()
        .into_iter()
        .find(|(k, p)| *k == n || p.provider.to_ascii_lowercase() == n)
        .map(|(_, p)| p)
}

/// Guess the provider preset from scheme and endpoint.
pub fn detect_provider(scheme: &str, endpoint: Option<&str>) -> &'static str {
    let ep = endpoint.unwrap_or_default().to_ascii_lowercase();
    if ep.contains("r2.cloudflarestorage.com") {
        "cloudflare-r2"
    } else if ep.contains("aliyuncs.com") || scheme == "oss" {
        "aliyun-oss"
    } else if ep.contains("backblazeb2.com") || scheme == "b2" {
        "backblaze-b2"
    } else if ep.contains("googleapis.com") || scheme == "gcs" {
        "gcs"
    } else if ep.contains("windows.net") || scheme == "azblob" {
        "azure-blob"
    } else {
        "aws-s3"
    }
}

/// Measured usage per logical operation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PerOpUsage {
    pub write_requests: f64,
    pub list_requests: f64,
    pub read_requests: f64,
    pub delete_requests: f64,
    /// Requests that failed or were throttled (billed on most providers, add no value).
    pub failed_requests: f64,
    pub download_bytes: f64,
    pub upload_bytes: f64,
    /// Downloaded bytes the application never asked for (range coalescing / chunk over-fetch).
    pub overread_bytes: f64,
    /// GETs avoided thanks to cache hits.
    pub cache_saved_reads: f64,
    pub cache_saved_bytes: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderCost {
    pub key: String,
    pub provider: String,
    pub requests_usd: f64,
    pub egress_usd: f64,
    pub storage_usd: f64,
    pub total_usd: f64,
    /// Requests + egress per million logical operations.
    pub per_million_ops_usd: f64,
    /// Monthly spend on failed/throttled requests.
    pub failed_requests_usd: f64,
    /// Monthly egress spend on over-read bytes.
    pub overread_usd: f64,
    /// Monthly spend avoided by the local cache.
    pub cache_savings_usd: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostReport {
    pub pricing_as_of: String,
    pub monthly_ops: f64,
    pub stored_gb: f64,
    pub egress_fraction: f64,
    pub per_op: PerOpUsage,
    /// The provider matching the target (or the profile's choice / custom pricing).
    pub selected: ProviderCost,
    pub comparison: Vec<ProviderCost>,
}

pub fn per_op_usage(s: &Snapshot) -> PerOpUsage {
    let ops = s.summary.ops.max(1) as f64;
    let mut u = PerOpUsage::default();
    for a in &s.apis {
        let n = a.count as f64 / ops;
        match a.api.billing_class() {
            BillingClass::Write => u.write_requests += n,
            BillingClass::List => u.list_requests += n,
            BillingClass::Read => u.read_requests += n,
            BillingClass::Delete => u.delete_requests += n,
            BillingClass::Free => {}
        }
        let failed: u64 = a
            .status
            .iter()
            .filter(|(code, _)| **code == 0 || (**code >= 400 && **code != 404))
            .map(|(_, c)| *c)
            .sum();
        if a.api.billing_class() != BillingClass::Free {
            u.failed_requests += failed as f64 / ops;
        }
    }
    let b = &s.bytes;
    u.download_bytes = b.backend_down as f64 / ops;
    u.upload_bytes = b.backend_up as f64 / ops;
    let wanted = b.logical_read.saturating_sub(b.cache_hit_bytes);
    u.overread_bytes = b.backend_down.saturating_sub(wanted) as f64 / ops;
    u.cache_saved_reads = s.cache.hits as f64 / ops;
    u.cache_saved_bytes = b.cache_hit_bytes as f64 / ops;
    u
}

fn price(key: &str, p: &Pricing, u: &PerOpUsage, spec: &CostSpec) -> ProviderCost {
    let m = spec.monthly_ops.max(0.0) / 1e6; // millions of ops
    let req_per_m = u.write_requests * p.write_per_million
        + u.list_requests * p.list_per_million
        + u.read_requests * p.read_per_million
        + u.delete_requests * p.delete_per_million;
    let egress_per_m = u.download_bytes * 1e6 / 1e9 * spec.egress_fraction * p.egress_per_gb;
    let avg_req_price = {
        let n = u.write_requests + u.list_requests + u.read_requests + u.delete_requests;
        if n > 0.0 {
            req_per_m / n
        } else {
            0.0
        }
    };
    let stored_gb = spec.stored_gib * 1.073_741_824;
    ProviderCost {
        key: key.to_owned(),
        provider: p.provider.clone(),
        requests_usd: req_per_m * m,
        egress_usd: egress_per_m * m,
        storage_usd: stored_gb * p.storage_gb_month,
        total_usd: (req_per_m + egress_per_m) * m + stored_gb * p.storage_gb_month,
        per_million_ops_usd: req_per_m + egress_per_m,
        failed_requests_usd: u.failed_requests * avg_req_price * m,
        overread_usd: u.overread_bytes * 1e6 / 1e9 * spec.egress_fraction * p.egress_per_gb * m,
        cache_savings_usd: (u.cache_saved_reads * p.read_per_million
            + u.cache_saved_bytes * 1e6 / 1e9 * spec.egress_fraction * p.egress_per_gb)
            * m,
    }
}

/// Build the cost report for a measured snapshot.
pub fn cost_report(
    s: &Snapshot,
    spec: &CostSpec,
    scheme: &str,
    endpoint: Option<&str>,
) -> CostReport {
    let u = per_op_usage(s);
    let selected_key = spec
        .provider
        .clone()
        .unwrap_or_else(|| detect_provider(scheme, endpoint).to_owned());
    let comparison: Vec<ProviderCost> = presets()
        .iter()
        .map(|(k, p)| price(k, p, &u, spec))
        .collect();
    let selected = match (&spec.pricing, preset(&selected_key)) {
        (Some(custom), _) => price("custom", custom, &u, spec),
        (None, Some(p)) => price(&selected_key, &p, &u, spec),
        (None, None) => price("aws-s3", &preset("aws-s3").expect("preset"), &u, spec),
    };
    CostReport {
        pricing_as_of: PRICING_AS_OF.to_owned(),
        monthly_ops: spec.monthly_ops,
        stored_gb: spec.stored_gib * 1.073_741_824,
        egress_fraction: spec.egress_fraction,
        per_op: u,
        selected,
        comparison,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Api, ApiStats, Snapshot, StageStats, Summary};

    #[test]
    fn prices_measured_request_mix() {
        let mut s = Snapshot {
            summary: Summary {
                ops: 1000,
                ..Default::default()
            },
            ..Default::default()
        };
        s.apis = vec![
            ApiStats {
                api: Api::GetObject,
                count: 2000,
                stages: StageStats::default(),
                ..Default::default()
            },
            ApiStats {
                api: Api::PutObject,
                count: 1000,
                ..Default::default()
            },
        ];
        s.bytes.backend_down = 1_000_000_000; // 1 MB per op
        s.bytes.logical_read = 500_000_000;
        let spec = CostSpec {
            monthly_ops: 1e6,
            egress_fraction: 1.0,
            stored_gib: 0.0,
            ..Default::default()
        };
        let r = cost_report(&s, &spec, "s3", None);
        // per million ops: 2M GET * 0.40 + 1M PUT * 5.00 = 5.80 ; egress 1 TB * 0.09 = 90
        assert!(
            (r.selected.requests_usd - 5.80).abs() < 1e-9,
            "{}",
            r.selected.requests_usd
        );
        assert!((r.selected.egress_usd - 90.0).abs() < 1e-6);
        assert!((r.selected.overread_usd - 45.0).abs() < 1e-6);
        assert_eq!(r.comparison.len(), presets().len());
        let r2 = r
            .comparison
            .iter()
            .find(|c| c.key == "cloudflare-r2")
            .unwrap();
        assert_eq!(r2.egress_usd, 0.0);
    }

    #[test]
    fn detects_providers() {
        assert_eq!(
            detect_provider("s3", Some("https://acc.r2.cloudflarestorage.com")),
            "cloudflare-r2"
        );
        assert_eq!(detect_provider("gcs", None), "gcs");
        assert_eq!(
            detect_provider("s3", Some("https://oss-cn-hangzhou.aliyuncs.com")),
            "aliyun-oss"
        );
    }
}
