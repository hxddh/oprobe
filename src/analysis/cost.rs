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

/// The provider price table for a recognised cloud storage endpoint; `None` for everything
/// else (in-process backends, localhost, private addresses, self-hosted or unknown endpoints),
/// whose bill oprobe cannot know.
pub fn detect_provider(scheme: &str, endpoint: Option<&str>) -> Option<&'static str> {
    if matches!(scheme, "memory" | "fs") {
        return None;
    }
    let host = match endpoint {
        Some(ep) => url::Url::parse(ep)
            .ok()?
            .host_str()?
            .trim_end_matches('.')
            .to_ascii_lowercase(),
        // Services whose endpoint is implied by the scheme.
        None => {
            return match scheme {
                "gcs" => Some("gcs"),
                "azblob" => Some("azure-blob"),
                "oss" => Some("aliyun-oss"),
                "b2" => Some("backblaze-b2"),
                _ => None,
            };
        }
    };
    let under = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if under("r2.cloudflarestorage.com") {
        Some("cloudflare-r2")
    } else if under("aliyuncs.com") {
        Some("aliyun-oss")
    } else if under("backblazeb2.com") {
        Some("backblaze-b2")
    } else if under("googleapis.com") {
        Some("gcs")
    } else if under("blob.core.windows.net") {
        Some("azure-blob")
    } else if under("amazonaws.com") {
        Some("aws-s3")
    } else {
        None
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
    /// A price table applies: the target is a recognised cloud endpoint, or the profile chose a
    /// provider / custom pricing. Otherwise `selected` and `comparison` are empty.
    pub applicable: bool,
    /// Why no cost is projected (when not applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_applicable_reason: Option<String>,
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
        // Requests that never reached the service (status 0: connect failure, client timeout
        // before a response) are not billed.
        let unsent = a.status.get(&0).copied().unwrap_or(0);
        let n = a.count.saturating_sub(unsent) as f64 / ops;
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
            .filter(|(code, _)| **code >= 400 && **code != 404)
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

/// Providers' "GB" for egress and storage is a binary gigabyte (2^30 bytes).
const BILLING_GB: f64 = 1024.0 * 1024.0 * 1024.0;

fn price(key: &str, p: &Pricing, u: &PerOpUsage, spec: &CostSpec) -> ProviderCost {
    let m = spec.monthly_ops.max(0.0) / 1e6; // millions of ops
    let req_per_m = u.write_requests * p.write_per_million
        + u.list_requests * p.list_per_million
        + u.read_requests * p.read_per_million
        + u.delete_requests * p.delete_per_million;
    let egress_per_m = u.download_bytes * 1e6 / BILLING_GB * spec.egress_fraction * p.egress_per_gb;
    let avg_req_price = {
        let n = u.write_requests + u.list_requests + u.read_requests + u.delete_requests;
        if n > 0.0 { req_per_m / n } else { 0.0 }
    };
    let stored_gb = spec.stored_gib;
    ProviderCost {
        key: key.to_owned(),
        provider: p.provider.clone(),
        requests_usd: req_per_m * m,
        egress_usd: egress_per_m * m,
        storage_usd: stored_gb * p.storage_gb_month,
        total_usd: (req_per_m + egress_per_m) * m + stored_gb * p.storage_gb_month,
        per_million_ops_usd: req_per_m + egress_per_m,
        failed_requests_usd: u.failed_requests * avg_req_price * m,
        overread_usd: u.overread_bytes * 1e6 / BILLING_GB
            * spec.egress_fraction
            * p.egress_per_gb
            * m,
        cache_savings_usd: (u.cache_saved_reads * p.read_per_million
            + u.cache_saved_bytes * 1e6 / BILLING_GB * spec.egress_fraction * p.egress_per_gb)
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
        .or_else(|| detect_provider(scheme, endpoint).map(str::to_owned));
    let selected = match (&spec.pricing, selected_key.as_deref().and_then(preset)) {
        (Some(custom), _) => Some(price("custom", custom, &u, spec)),
        (None, Some(p)) => Some(price(
            selected_key.as_deref().unwrap_or_default(),
            &p,
            &u,
            spec,
        )),
        (None, None) => None,
    };
    let Some(selected) = selected else {
        return CostReport {
            applicable: false,
            not_applicable_reason: Some(match &selected_key {
                Some(k) => format!("unknown pricing provider `{k}`"),
                None => format!(
                    "{} is not a recognised cloud storage endpoint, so its bill is unknown; set `cost.provider` (or `--provider`) or `[cost.pricing]` to project one",
                    endpoint.unwrap_or(match scheme {
                        "memory" => "the in-memory backend",
                        "fs" => "the local filesystem",
                        _ => "the target",
                    })
                ),
            }),
            pricing_as_of: PRICING_AS_OF.to_owned(),
            monthly_ops: spec.monthly_ops,
            stored_gb: spec.stored_gib,
            egress_fraction: spec.egress_fraction,
            per_op: u,
            selected: ProviderCost::default(),
            comparison: Vec::new(),
        };
    };
    let comparison: Vec<ProviderCost> = presets()
        .iter()
        .map(|(k, p)| price(k, p, &u, spec))
        .collect();
    CostReport {
        applicable: true,
        not_applicable_reason: None,
        pricing_as_of: PRICING_AS_OF.to_owned(),
        monthly_ops: spec.monthly_ops,
        stored_gb: spec.stored_gib,
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
        let r = cost_report(&s, &spec, "s3", Some("https://s3.us-east-1.amazonaws.com"));
        // per million ops: 2M GET * 0.40 + 1M PUT * 5.00 = 5.80 ;
        // egress 1e12 B = 931.32 GiB (billed "GB") * 0.09 = 83.82
        assert!(
            (r.selected.requests_usd - 5.80).abs() < 1e-9,
            "{}",
            r.selected.requests_usd
        );
        let gib = 1e12 / (1u64 << 30) as f64;
        assert!((r.selected.egress_usd - gib * 0.09).abs() < 1e-6);
        assert!((r.selected.overread_usd - gib * 0.09 / 2.0).abs() < 1e-6);
        assert_eq!(r.comparison.len(), presets().len());
        let r2 = r
            .comparison
            .iter()
            .find(|c| c.key == "cloudflare-r2")
            .unwrap();
        assert_eq!(r2.egress_usd, 0.0);
    }

    #[test]
    fn requests_that_never_left_the_client_are_not_billed() {
        let mut s = Snapshot {
            summary: Summary {
                ops: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        s.apis = vec![ApiStats {
            api: Api::GetObject,
            count: 100,
            status: [(0u16, 40u64), (200, 50), (503, 10)].into_iter().collect(),
            ..Default::default()
        }];
        let u = per_op_usage(&s);
        assert!((u.read_requests - 0.6).abs() < 1e-9, "{}", u.read_requests);
        assert!(
            (u.failed_requests - 0.1).abs() < 1e-9,
            "{}",
            u.failed_requests
        );
        let spec = CostSpec {
            stored_gib: 10.0,
            ..Default::default()
        };
        let r = cost_report(&s, &spec, "s3", Some("https://s3.us-east-1.amazonaws.com"));
        assert_eq!(r.stored_gb, 10.0);
    }

    #[test]
    fn detects_providers() {
        assert_eq!(
            detect_provider("s3", Some("https://acc.r2.cloudflarestorage.com")),
            Some("cloudflare-r2")
        );
        assert_eq!(detect_provider("gcs", None), Some("gcs"));
        assert_eq!(
            detect_provider("s3", Some("https://oss-cn-hangzhou.aliyuncs.com")),
            Some("aliyun-oss")
        );
        assert_eq!(
            detect_provider("s3", Some("https://s3.eu-west-1.amazonaws.com")),
            Some("aws-s3")
        );
        for local in [
            "http://127.0.0.1:9000",
            "http://localhost:9000",
            "http://10.0.0.5:9000",
            "https://minio.internal",
            "http://[::1]:9000",
            // A cloud domain in the path or as a prefix is not that cloud.
            "http://192.168.1.2/amazonaws.com",
            "https://amazonaws.com.evil.example",
        ] {
            assert_eq!(detect_provider("s3", Some(local)), None, "{local}");
        }
        assert_eq!(detect_provider("memory", None), None);
        assert_eq!(detect_provider("fs", None), None);
        assert_eq!(detect_provider("cos", None), None);
    }

    #[test]
    fn no_cost_is_projected_for_local_or_unknown_endpoints() {
        let s = Snapshot {
            summary: Summary {
                ops: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let spec = CostSpec::default();
        for (scheme, ep) in [
            ("memory", None),
            ("fs", None),
            ("s3", Some("http://127.0.0.1:9000")),
        ] {
            let r = cost_report(&s, &spec, scheme, ep);
            assert!(!r.applicable, "{scheme} {ep:?}");
            assert!(r.not_applicable_reason.is_some());
            assert!(r.comparison.is_empty());
            assert_eq!(r.selected.total_usd, 0.0);
        }
        // An explicit choice of price table applies anywhere.
        let chosen = CostSpec {
            provider: Some("aws-s3".into()),
            ..Default::default()
        };
        let r = cost_report(&s, &chosen, "s3", Some("http://127.0.0.1:9000"));
        assert!(r.applicable);
        assert_eq!(r.selected.key, "aws-s3");
        let r = cost_report(&s, &spec, "s3", Some("https://s3.us-east-1.amazonaws.com"));
        assert!(r.applicable && r.selected.key == "aws-s3");
    }
}
