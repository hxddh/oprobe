//! Cost model: measured per-operation request mix and bytes × provider list prices,
//! projected to a monthly bill and compared across providers.

use serde::{Deserialize, Serialize};

use crate::config::CostSpec;
use crate::trace::{BillingClass, Snapshot};

/// Provider list prices in USD (request prices per million, data per GB = 1e9 bytes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Internet egress per GB (the first paid tier when `egress_tiers` is given).
    pub egress_per_gb: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Internet egress price tiers by monthly volume (absolute bounds, the free allowance
    /// included in the first); empty: the flat `egress_per_gb`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress_tiers: Vec<EgressTier>,
    /// Internet egress free per month (GB).
    #[serde(default)]
    pub egress_free_gb: f64,
    /// Free egress as a multiple of the stored data (Backblaze B2: 3×).
    #[serde(default)]
    pub egress_free_stored_multiple: f64,
    /// Inter-region transfer per GB.
    #[serde(default)]
    pub inter_region_per_gb: f64,
    /// Storage classes besides standard (`infrequent`, `archive`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage_classes: Vec<StorageClassPrice>,
    /// Requests answered 4xx are billed (most providers).
    #[serde(default = "yes")]
    pub bills_client_errors: bool,
    /// Requests answered 5xx (incl. 503 SlowDown) are billed (the presets assume so; check
    /// your provider's terms and set `false` in `[cost.pricing]` where they are free).
    #[serde(default = "yes")]
    pub bills_server_errors: bool,
}

fn yes() -> bool {
    true
}

/// One egress price tier: the monthly GB up to `up_to_gb` (counted from 0, the free allowance
/// included) cost `per_gb` (the last tier may use a huge bound).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EgressTier {
    pub up_to_gb: f64,
    pub per_gb: f64,
}

/// A storage class: its storage, request and retrieval prices, minimum billable object size
/// and minimum storage duration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StorageClassPrice {
    /// `infrequent` or `archive`.
    pub class: String,
    /// The provider's name for it.
    pub name: String,
    pub storage_gb_month: f64,
    pub write_per_million: f64,
    pub read_per_million: f64,
    /// Data retrieval per GB read.
    pub retrieval_per_gb: f64,
    /// Smaller objects are billed as this size (bytes).
    pub min_billable_bytes: u64,
    /// Objects deleted earlier are billed for the rest of this duration (days).
    pub min_duration_days: f64,
}

/// Public list prices (standard/hot tier, cheapest common region). Verify against your
/// contract; override with `[cost.pricing]` in the profile.
/// Cloudflare R2 and Backblaze B2 were checked against the providers' public pricing pages on
/// 2026-11-20 (as was the AWS GET price); the other tables are list prices as of 2025-06.
pub const PRICING_AS_OF: &str = "2026-11";

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
        egress_tiers: Vec::new(),
        egress_free_gb: 0.0,
        egress_free_stored_multiple: 0.0,
        inter_region_per_gb: 0.0,
        storage_classes: Vec::new(),
        bills_client_errors: true,
        bills_server_errors: true,
    };
    let tiers = |t: &[(f64, f64)]| {
        t.iter()
            .map(|(up_to_gb, per_gb)| EgressTier {
                up_to_gb: *up_to_gb,
                per_gb: *per_gb,
            })
            .collect::<Vec<_>>()
    };
    let class = |class: &str, name: &str, storage, write, read, retrieval, min_kib: u64, days| {
        StorageClassPrice {
            class: class.into(),
            name: name.into(),
            storage_gb_month: storage,
            write_per_million: write,
            read_per_million: read,
            retrieval_per_gb: retrieval,
            min_billable_bytes: min_kib * 1024,
            min_duration_days: days,
        }
    };
    const TB: f64 = 1024.0;
    let with = |mut base: Pricing, f: &dyn Fn(&mut Pricing)| {
        f(&mut base);
        base
    };
    let raw = vec![
        (
            "aws-s3",
            p(
                "AWS S3 Standard",
                0.023,
                5.00,
                5.00,
                0.40,
                0.09,
                "us-east-1; egress: first 100 GB/month free, $0.09/GB to 10 TB, then $0.085, $0.07, $0.05",
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
                0.00695,
                0.0,
                0.0,
                0.0,
                0.01,
                "$6.95/TB-month, API calls free (checked 2026-11); egress free up to 3x stored data",
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
    ];
    raw.into_iter()
        .map(|(k, base)| {
            let full = match k {
                "aws-s3" => with(base, &|p| {
                    p.egress_free_gb = 100.0;
                    p.egress_tiers = tiers(&[(10.0 * TB, 0.09), (50.0 * TB, 0.085), (150.0 * TB, 0.07), (f64::MAX, 0.05)]);
                    p.inter_region_per_gb = 0.02;
                    p.storage_classes = vec![
                        class("infrequent", "S3 Standard-IA", 0.0125, 10.0, 1.0, 0.01, 128, 30.0),
                        class("archive", "S3 Glacier Instant Retrieval", 0.004, 20.0, 10.0, 0.03, 128, 90.0),
                    ];
                }),
                "gcs" => with(base, &|p| {
                    p.egress_tiers = tiers(&[(TB, 0.12), (10.0 * TB, 0.11), (f64::MAX, 0.08)]);
                    p.inter_region_per_gb = 0.02;
                    p.storage_classes = vec![
                        class("infrequent", "GCS Nearline", 0.010, 10.0, 1.0, 0.01, 0, 30.0),
                        class("archive", "GCS Archive", 0.0012, 50.0, 50.0, 0.05, 0, 365.0),
                    ];
                }),
                "azure-blob" => with(base, &|p| {
                    p.egress_free_gb = 100.0;
                    p.egress_tiers = tiers(&[(10.0 * TB, 0.087), (50.0 * TB, 0.083), (150.0 * TB, 0.07), (f64::MAX, 0.05)]);
                    p.inter_region_per_gb = 0.02;
                    p.storage_classes = vec![
                        class("infrequent", "Azure Blob Cool", 0.010, 10.0, 1.0, 0.01, 0, 30.0),
                        class("archive", "Azure Blob Cold", 0.0036, 18.0, 10.0, 0.03, 0, 90.0),
                    ];
                }),
                "cloudflare-r2" => with(base, &|p| {
                    p.storage_classes = vec![class(
                        "infrequent",
                        "R2 Infrequent Access",
                        0.010,
                        9.0,
                        0.9,
                        0.01,
                        0,
                        30.0,
                    )];
                }),
                "backblaze-b2" => with(base, &|p| p.egress_free_stored_multiple = 3.0),
                _ => base,
            };
            (k, full)
        })
        .collect()
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

/// Serialize a count held as `f64` as a whole number.
fn ser_whole<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(if v.is_finite() && *v > 0.0 {
        v.round() as u64
    } else {
        0
    })
}

/// `(assumed 1 TiB stored — set --stored-gib)` when the storage size is the default, else
/// empty: the label every renderer puts next to a bill that includes storage (design v4 §4).
pub fn storage_label(c: &CostReport) -> String {
    if c.storage_assumed {
        format!(
            "(assumed {} stored — set --stored-gib)",
            if c.stored_gb == 1024.0 {
                "1 TiB".to_owned()
            } else {
                format!("{:.0} GiB", c.stored_gb)
            }
        )
    } else {
        String::new()
    }
}

/// Measured usage per logical operation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PerOpUsage {
    #[serde(rename = "write_requests_mean")]
    pub write_requests: f64,
    #[serde(rename = "list_requests_mean")]
    pub list_requests: f64,
    #[serde(rename = "read_requests_mean")]
    pub read_requests: f64,
    #[serde(rename = "delete_requests_mean")]
    pub delete_requests: f64,
    /// Requests that failed or were throttled (billed on most providers, add no value).
    #[serde(rename = "failed_requests_mean")]
    pub failed_requests: f64,
    /// Of `failed_requests`, those answered 5xx (incl. 503 SlowDown): not billed by most
    /// providers.
    #[serde(default, rename = "server_error_requests_mean")]
    pub server_error_requests: f64,
    pub download_bytes: f64,
    pub upload_bytes: f64,
    /// Downloaded bytes the application never asked for (range coalescing / chunk over-fetch).
    pub overread_bytes: f64,
    /// GETs avoided thanks to cache hits.
    #[serde(rename = "cache_saved_reads_mean")]
    pub cache_saved_reads: f64,
    pub cache_saved_bytes: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// The storage class priced (`standard`, or the provider's name of the class).
    #[serde(default)]
    pub storage_class: String,
    /// Monthly data-retrieval fees of the storage class.
    #[serde(default)]
    pub retrieval_usd: f64,
    /// Monthly inter-region transfer.
    #[serde(default)]
    pub inter_region_usd: f64,
    /// Monthly charges for objects deleted before the class's minimum duration.
    #[serde(default)]
    pub early_delete_usd: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CostReport {
    /// A price table applies: the target is a recognised cloud endpoint, or the profile chose a
    /// provider / custom pricing. Otherwise `selected` and `comparison` are empty.
    pub applicable: bool,
    /// Why no cost is projected (when not applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_applicable_reason: Option<String>,
    /// Pricing key of `selected` (e.g. `aws-s3`, `custom`), when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub pricing_as_of: String,
    /// Projected operations per month (a whole number).
    #[serde(rename = "monthly_ops_count", serialize_with = "ser_whole")]
    #[schemars(with = "u64")]
    pub monthly_ops: f64,
    #[serde(rename = "stored_gib")]
    pub stored_gb: f64,
    /// `stored_gib` is the default assumption (1 TiB), not a size the user gave
    /// (`--stored-gib`); renderers label it so.
    #[serde(default)]
    pub storage_assumed: bool,
    #[serde(rename = "egress_ratio")]
    pub egress_fraction: f64,
    pub per_op: PerOpUsage,
    /// The provider matching the target (or the profile's choice / custom pricing).
    pub selected: ProviderCost,
    pub comparison: Vec<ProviderCost>,
    /// Monthly request spend per storage API under the selected prices (largest first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_api: Vec<ApiCost>,
    /// Monthly spend that buys nothing: failed/throttled requests and over-read egress.
    #[serde(default)]
    pub waste_usd: f64,
}

/// Request spend of one storage API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApiCost {
    pub api: String,
    pub billing_class: BillingClass,
    /// Billed requests of this API per logical operation.
    #[serde(rename = "requests_per_op_mean")]
    pub requests_per_op: f64,
    /// Monthly spend at the projected operation volume.
    pub monthly_usd: f64,
    /// Share of the monthly request spend (0–100).
    pub share_pct: f64,
}

/// Billing class of a storage API by name: OpenDAL `ServiceOperation` names and the
/// providers' own (S3, GCS JSON, Azure Blob, B2 native); unknown names bill as reads.
pub fn billing_class_of(api: &str) -> BillingClass {
    let a = api.to_ascii_lowercase();
    let has = |xs: &[&str]| xs.iter().any(|x| a.contains(x));
    if has(&["control", "authorize", "token", "session"]) {
        BillingClass::Free
    } else if has(&["delete", "abort", "remove"]) {
        BillingClass::Delete
    } else if has(&["list", "objects.list"]) {
        BillingClass::List
    } else if has(&[
        "put",
        "upload",
        "copy",
        "multipart",
        "create",
        "complete",
        "insert",
        "compose",
        "append",
        "write",
        "post",
    ]) {
        BillingClass::Write
    } else {
        BillingClass::Read
    }
}

/// Request spend per API for usage measured over `ops` operations under `p`.
fn api_costs(s: &Snapshot, p: &Pricing, spec: &CostSpec) -> Vec<ApiCost> {
    let ops = s.summary.ops.max(1) as f64;
    let m = spec.monthly_ops.max(0.0) / 1e6;
    let mut v: Vec<ApiCost> = s
        .apis
        .iter()
        .map(|a| {
            let unsent = a.status.get(&0).copied().unwrap_or(0);
            let n = a.count.saturating_sub(unsent) as f64 / ops;
            let class = billing_class_of(a.api.label());
            let unit = match class {
                BillingClass::Write => p.write_per_million,
                BillingClass::List => p.list_per_million,
                BillingClass::Read => p.read_per_million,
                BillingClass::Delete => p.delete_per_million,
                BillingClass::Free => 0.0,
            };
            ApiCost {
                api: a.api.label().to_owned(),
                billing_class: class,
                requests_per_op: n,
                monthly_usd: n * unit * m,
                share_pct: 0.0,
            }
        })
        .filter(|c| c.requests_per_op > 0.0)
        .collect();
    let total: f64 = v.iter().map(|c| c.monthly_usd).sum();
    for c in &mut v {
        c.share_pct = if total > 0.0 {
            c.monthly_usd * 100.0 / total
        } else {
            0.0
        };
    }
    v.sort_by(|a, b| b.monthly_usd.total_cmp(&a.monthly_usd));
    v
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
        let server: u64 = a
            .status
            .iter()
            .filter(|(code, _)| **code >= 500)
            .map(|(_, c)| *c)
            .sum();
        if a.api.billing_class() != BillingClass::Free {
            u.failed_requests += failed as f64 / ops;
            u.server_error_requests += server as f64 / ops;
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

/// The monthly bill of `gb` internet egress under `p`: the free allowance first (fixed, or a
/// multiple of the stored data), then the price tiers.
pub fn egress_usd(p: &Pricing, gb: f64, stored_gb: f64) -> f64 {
    let free = (p.egress_free_gb + p.egress_free_stored_multiple * stored_gb).max(0.0);
    if p.egress_tiers.is_empty() {
        return (gb - free).max(0.0) * p.egress_per_gb;
    }
    // Paid GB lie in [free, gb]; each tier covers [floor, up_to).
    let (mut usd, mut floor) = (0.0, 0.0f64);
    for t in &p.egress_tiers {
        let lo = floor.max(free);
        let hi = t.up_to_gb.min(gb);
        if hi > lo {
            usd += (hi - lo) * t.per_gb;
        }
        floor = t.up_to_gb;
        if floor >= gb {
            break;
        }
    }
    usd
}

/// The class of `spec.storage_class` in `p` (`None`: standard).
fn class_of<'a>(p: &'a Pricing, spec: &CostSpec) -> Option<&'a StorageClassPrice> {
    let want = spec.storage_class.as_deref()?.to_ascii_lowercase();
    if want == "standard" {
        return None;
    }
    p.storage_classes
        .iter()
        .find(|c| c.class == want || c.name.to_ascii_lowercase() == want)
}

fn price(key: &str, p: &Pricing, u: &PerOpUsage, spec: &CostSpec) -> ProviderCost {
    let m = spec.monthly_ops.max(0.0) / 1e6; // millions of ops
    let class = class_of(p, spec);
    let (write_pm, read_pm, storage_pg) = match class {
        Some(c) => (c.write_per_million, c.read_per_million, c.storage_gb_month),
        None => (p.write_per_million, p.read_per_million, p.storage_gb_month),
    };
    // Error responses the provider does not bill come off the request count.
    let unbilled = |n: f64, total: f64| {
        if total <= 0.0 {
            return 0.0;
        }
        let server = u.server_error_requests.min(u.failed_requests);
        let client = u.failed_requests - server;
        let mut skip = 0.0;
        if !p.bills_server_errors {
            skip += server;
        }
        if !p.bills_client_errors {
            skip += client;
        }
        n * (skip / total).clamp(0.0, 1.0)
    };
    let total_req = u.write_requests + u.list_requests + u.read_requests + u.delete_requests;
    let billed = |n: f64| n - unbilled(n, total_req);
    let req_per_m = billed(u.write_requests) * write_pm
        + billed(u.list_requests) * p.list_per_million
        + billed(u.read_requests) * read_pm
        + billed(u.delete_requests) * p.delete_per_million;
    let stored_gb = spec.stored_gib;
    let monthly_egress_gb = u.download_bytes * spec.monthly_ops.max(0.0) / BILLING_GB * spec.egress_fraction;
    let egress = egress_usd(p, monthly_egress_gb, stored_gb);
    let egress_per_m = if m > 0.0 { egress / m } else { 0.0 };
    let avg_req_price = if total_req > 0.0 { req_per_m / total_req } else { 0.0 };
    // Minimum billable size: small objects are billed as the minimum.
    let object_bytes = if u.read_requests > 0.0 && u.download_bytes > 0.0 {
        u.download_bytes / u.read_requests
    } else if u.write_requests > 0.0 && u.upload_bytes > 0.0 {
        u.upload_bytes / u.write_requests
    } else {
        0.0
    };
    let size_factor = match class {
        Some(c) if c.min_billable_bytes > 0 && object_bytes > 0.0 => {
            (c.min_billable_bytes as f64 / object_bytes).max(1.0)
        }
        _ => 1.0,
    };
    let storage_usd = stored_gb * storage_pg * size_factor;
    let retrieval_usd = class.map_or(0.0, |c| {
        u.download_bytes * spec.monthly_ops.max(0.0) / BILLING_GB * c.retrieval_per_gb
    });
    // Deletes before the minimum duration: on average half of it is still billed.
    let early_delete_usd = class.map_or(0.0, |c| {
        let deleted_gb = u.delete_requests * spec.monthly_ops.max(0.0) * object_bytes.max(1.0)
            / BILLING_GB;
        deleted_gb * c.storage_gb_month * size_factor * (c.min_duration_days / 30.0) * 0.5
    });
    let inter_region_usd = u.download_bytes * spec.monthly_ops.max(0.0) / BILLING_GB
        * spec.inter_region_fraction
        * p.inter_region_per_gb;
    let extra = retrieval_usd + early_delete_usd + inter_region_usd;
    ProviderCost {
        key: key.to_owned(),
        provider: p.provider.clone(),
        requests_usd: req_per_m * m,
        egress_usd: egress,
        storage_usd,
        total_usd: req_per_m * m + egress + storage_usd + extra,
        per_million_ops_usd: req_per_m + egress_per_m + if m > 0.0 { extra / m } else { 0.0 },
        failed_requests_usd: {
            let server = u.server_error_requests.min(u.failed_requests);
            let client = u.failed_requests - server;
            let n = if p.bills_client_errors { client } else { 0.0 }
                + if p.bills_server_errors { server } else { 0.0 };
            n.min(total_req) * avg_req_price * m
        },
        overread_usd: if u.download_bytes > 0.0 {
            egress * (u.overread_bytes / u.download_bytes).clamp(0.0, 1.0)
        } else {
            0.0
        },
        cache_savings_usd: (u.cache_saved_reads * read_pm
            + u.cache_saved_bytes * 1e6 / BILLING_GB * spec.egress_fraction * p.egress_per_gb)
            * m,
        storage_class: class.map_or_else(|| "standard".to_owned(), |c| c.name.clone()),
        retrieval_usd,
        inter_region_usd,
        early_delete_usd,
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
    // The stored size is the default unless the profile or `--stored-gib` changed it (the
    // same rule `RunSpec` uses to decide what a replay must repeat).
    let storage_assumed = spec.stored_gib == CostSpec::default().stored_gib;
    let selected_key = spec
        .provider
        .clone()
        .or_else(|| detect_provider(scheme, endpoint).map(str::to_owned));
    let table = match (&spec.pricing, selected_key.as_deref().and_then(preset)) {
        (Some(custom), _) => Some(("custom".to_owned(), custom.clone())),
        (None, Some(p)) => Some((selected_key.clone().unwrap_or_default(), p)),
        (None, None) => None,
    };
    let selected = table.as_ref().map(|(k, p)| price(k, p, &u, spec));
    let by_api = table
        .as_ref()
        .map(|(_, p)| api_costs(s, p, spec))
        .unwrap_or_default();
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
            provider: None,
            pricing_as_of: PRICING_AS_OF.to_owned(),
            monthly_ops: spec.monthly_ops,
            stored_gb: spec.stored_gib,
            storage_assumed,
            egress_fraction: spec.egress_fraction,
            per_op: u,
            selected: ProviderCost::default(),
            comparison: Vec::new(),
            by_api: Vec::new(),
            waste_usd: 0.0,
        };
    };
    let comparison: Vec<ProviderCost> = presets()
        .iter()
        .map(|(k, p)| price(k, p, &u, spec))
        .collect();
    CostReport {
        applicable: true,
        not_applicable_reason: None,
        provider: Some(selected.key.clone()),
        pricing_as_of: PRICING_AS_OF.to_owned(),
        monthly_ops: spec.monthly_ops,
        stored_gb: spec.stored_gib,
        storage_assumed,
        egress_fraction: spec.egress_fraction,
        per_op: u,
        waste_usd: selected.failed_requests_usd + selected.overread_usd,
        selected,
        comparison,
        by_api,
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
        // egress 1e12 B = 931.32 GiB (billed "GB"): the first 100 GB/month are free, the rest
        // at $0.09 (first tier, A14)
        assert!(
            (r.selected.requests_usd - 5.80).abs() < 1e-9,
            "{}",
            r.selected.requests_usd
        );
        let gib = 1e12 / (1u64 << 30) as f64;
        assert!((r.selected.egress_usd - (gib - 100.0) * 0.09).abs() < 1e-6);
        assert!((r.selected.overread_usd - (gib - 100.0) * 0.09 / 2.0).abs() < 1e-6);
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
    fn per_api_spend_and_waste() {
        for (name, class) in [
            ("GetObject", BillingClass::Read),
            ("HeadObject", BillingClass::Read),
            ("PutObject", BillingClass::Write),
            ("CompleteMultipartUpload", BillingClass::Write),
            ("ListObjectsV2", BillingClass::List),
            ("DeleteObjects", BillingClass::Delete),
            ("Control", BillingClass::Free),
        ] {
            assert_eq!(billing_class_of(name), class, "{name}");
        }
        let mut s = Snapshot {
            summary: Summary {
                ops: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        s.apis = vec![
            ApiStats {
                api: Api::PutObject,
                count: 100,
                status: [(200u16, 100u64)].into_iter().collect(),
                ..Default::default()
            },
            ApiStats {
                api: Api::GetObject,
                count: 100,
                status: [(200u16, 50u64), (503, 50)].into_iter().collect(),
                ..Default::default()
            },
        ];
        let spec = CostSpec {
            monthly_ops: 1e6,
            ..Default::default()
        };
        let r = cost_report(&s, &spec, "s3", Some("https://s3.us-east-1.amazonaws.com"));
        assert_eq!(r.by_api[0].api, "PutObject");
        assert!((r.by_api[0].monthly_usd - 5.0).abs() < 1e-9);
        assert!(r.by_api[0].share_pct > 90.0);
        assert!(r.waste_usd > 0.0);
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

#[cfg(test)]
mod bill_tests {
    use super::*;

    #[test]
    fn egress_tiers_and_free_allowances() {
        let aws = preset("aws-s3").unwrap();
        assert_eq!(egress_usd(&aws, 50.0, 0.0), 0.0);
        let ten_tb = 10.0 * 1024.0;
        let want = (ten_tb - 100.0) * 0.09 + 1024.0 * 0.085;
        assert!((egress_usd(&aws, ten_tb + 1024.0, 0.0) - want).abs() < 1e-6);
        let b2 = preset("backblaze-b2").unwrap();
        assert_eq!(egress_usd(&b2, 300.0, 100.0), 0.0);
        assert!((egress_usd(&b2, 400.0, 100.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn storage_classes_bill_minimum_sizes_and_retrieval() {
        let p = preset("aws-s3").unwrap();
        // 16 KiB objects read once each: Standard-IA bills 128 KiB each and a retrieval fee.
        let u = PerOpUsage {
            read_requests: 1.0,
            download_bytes: 16.0 * 1024.0,
            ..Default::default()
        };
        let std = CostSpec {
            monthly_ops: 1e6,
            stored_gib: 100.0,
            ..Default::default()
        };
        let ia = CostSpec {
            storage_class: Some("infrequent".into()),
            ..std.clone()
        };
        let a = price("aws-s3", &p, &u, &std);
        let b = price("aws-s3", &p, &u, &ia);
        assert_eq!(a.storage_class, "standard");
        assert_eq!(b.storage_class, "S3 Standard-IA");
        assert!((b.storage_usd - 100.0 * 0.0125 * 8.0).abs() < 1e-9, "{}", b.storage_usd);
        assert!(b.retrieval_usd > 0.0 && a.retrieval_usd == 0.0);
        assert!(b.requests_usd > a.requests_usd);
    }

    #[test]
    fn throttled_responses_are_not_billed_where_the_provider_does_not() {
        let mut p = preset("aws-s3").unwrap();
        assert!(p.bills_server_errors, "billed unless a table says otherwise");
        p.bills_server_errors = false;
        let u = PerOpUsage {
            read_requests: 2.0,
            failed_requests: 1.0,
            server_error_requests: 1.0,
            ..Default::default()
        };
        let s = CostSpec {
            monthly_ops: 1e6,
            ..Default::default()
        };
        let c = price("aws-s3", &p, &u, &s);
        assert!((c.requests_usd - 0.40).abs() < 1e-9, "{}", c.requests_usd);
        assert_eq!(c.failed_requests_usd, 0.0);
    }
}
