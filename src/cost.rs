//! Multi-cloud object storage FinOps & amplification cost analyzer:
//! Quantifies monthly storage, Class A/B API fees, internet egress, amplification waste,
//! and local cache savings across AWS S3, Cloudflare R2, Google GCS, Alibaba OSS, and Backblaze B2.

use serde::{Deserialize, Serialize};

use crate::telemetry::TransportAndHostSnapshot;

fn default_stored_tb() -> f64 {
    10.0
}
fn default_monthly_access_tb() -> f64 {
    50.0
}

/// Cloud object storage pricing profile (USD) and monthly scale assumptions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloudPricingProfile {
    pub provider_name: String,
    pub storage_usd_per_gb_month: f64,
    pub class_a_put_list_usd_per_million: f64,
    pub class_b_get_head_usd_per_million: f64,
    pub egress_usd_per_gb: f64,
    #[serde(default = "default_stored_tb")]
    pub baseline_stored_tb: f64,
    #[serde(default = "default_monthly_access_tb")]
    pub monthly_logical_access_tb: f64,
}

impl Default for CloudPricingProfile {
    fn default() -> Self {
        Self::aws_s3_standard()
    }
}

impl CloudPricingProfile {
    /// AWS S3 Standard (`us-east-1` public list pricing).
    pub fn aws_s3_standard() -> Self {
        Self {
            provider_name: "AWS S3 Standard".to_owned(),
            storage_usd_per_gb_month: 0.023,
            class_a_put_list_usd_per_million: 5.00,
            class_b_get_head_usd_per_million: 0.40,
            egress_usd_per_gb: 0.09,
            baseline_stored_tb: 10.0,
            monthly_logical_access_tb: 50.0,
        }
    }

    /// Cloudflare R2 (Zero Internet Egress).
    pub fn cloudflare_r2() -> Self {
        Self {
            provider_name: "Cloudflare R2".to_owned(),
            storage_usd_per_gb_month: 0.015,
            class_a_put_list_usd_per_million: 4.50,
            class_b_get_head_usd_per_million: 0.36,
            egress_usd_per_gb: 0.00,
            baseline_stored_tb: 10.0,
            monthly_logical_access_tb: 50.0,
        }
    }

    /// Google Cloud Storage (GCS Standard `us-central1`).
    pub fn google_gcs_standard() -> Self {
        Self {
            provider_name: "Google GCS Standard".to_owned(),
            storage_usd_per_gb_month: 0.020,
            class_a_put_list_usd_per_million: 5.00,
            class_b_get_head_usd_per_million: 0.40,
            egress_usd_per_gb: 0.12,
            baseline_stored_tb: 10.0,
            monthly_logical_access_tb: 50.0,
        }
    }

    /// Alibaba Cloud OSS Standard (International standard pricing).
    pub fn alibaba_oss_standard() -> Self {
        Self {
            provider_name: "Alibaba Cloud OSS".to_owned(),
            storage_usd_per_gb_month: 0.020,
            class_a_put_list_usd_per_million: 1.40,
            class_b_get_head_usd_per_million: 1.40,
            egress_usd_per_gb: 0.07,
            baseline_stored_tb: 10.0,
            monthly_logical_access_tb: 50.0,
        }
    }

    /// Backblaze B2 Pay-As-You-Go.
    pub fn backblaze_b2() -> Self {
        Self {
            provider_name: "Backblaze B2".to_owned(),
            storage_usd_per_gb_month: 0.006,
            class_a_put_list_usd_per_million: 0.00,
            class_b_get_head_usd_per_million: 0.40,
            egress_usd_per_gb: 0.01,
            baseline_stored_tb: 10.0,
            monthly_logical_access_tb: 50.0,
        }
    }

    /// Standard 5-cloud comparison catalog.
    pub fn standard_catalog_with_scale(stored_tb: f64, access_tb: f64) -> [Self; 5] {
        let mut cat = [
            Self::aws_s3_standard(),
            Self::cloudflare_r2(),
            Self::google_gcs_standard(),
            Self::alibaba_oss_standard(),
            Self::backblaze_b2(),
        ];
        for c in &mut cat {
            c.baseline_stored_tb = stored_tb.max(0.1);
            c.monthly_logical_access_tb = access_tb.max(0.1);
        }
        cat
    }

    pub fn standard_catalog() -> [Self; 5] {
        Self::standard_catalog_with_scale(10.0, 50.0)
    }
}

/// Monthly FinOps cost breakdown & amplification impact for a single cloud provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostProjection {
    pub provider_name: String,
    pub monthly_storage_usd: f64,
    pub monthly_class_a_usd: f64,
    pub monthly_class_b_usd: f64,
    pub monthly_api_requests_usd: f64,
    pub monthly_egress_usd: f64,
    pub total_monthly_usd: f64,
    pub waste_from_amplification_usd: f64,
    pub saved_by_cache_usd: f64,
    pub usd_per_logical_tb_scanned: f64,
}

/// Project monthly cloud bill strictly from measured Class A/B RPC counts, read amplification, and cache savings.
pub fn estimate_monthly_finops(
    pricing: &CloudPricingProfile,
    tp: &TransportAndHostSnapshot,
    fallback_write_ratio: f64,
    egress_ratio: f64,
) -> CostProjection {
    let stored_gb = pricing.baseline_stored_tb.max(0.1) * 1_000.0;
    let logical_monthly_gb = pricing.monthly_logical_access_tb.max(0.1) * 1_000.0;
    let egress_ratio = egress_ratio.clamp(0.0, 1.0);

    let read_amp = tp.read_amplification.max(1.0);
    let rpc_amp = tp.rpc_amplification.max(1.0);

    // Derive empirical Class A share directly from observed RPC mix when available
    let total_measured_rpcs = tp.class_a_rpcs + tp.class_b_rpcs;
    let class_a_share = if total_measured_rpcs > 0 {
        (tp.class_a_rpcs as f64 / total_measured_rpcs as f64).clamp(0.0, 1.0)
    } else {
        fallback_write_ratio.clamp(0.0, 1.0)
    };
    let class_b_share = 1.0 - class_a_share;

    let avg_logical_op_bytes = if tp.logical_ops > 0 && tp.logical_bytes > 0 {
        (tp.logical_bytes as f64 / tp.logical_ops as f64).max(4_096.0)
    } else {
        262_144.0
    };

    let base_logical_ops_millions =
        (logical_monthly_gb * 1_000_000_000.0 / avg_logical_op_bytes) / 1_000_000.0;

    // Account for local cache eliminating backend Class B RPCs on hits
    let cache_miss_factor = if tp.cache_lookups > 0 {
        (1.0 - tp.cache_hit_ratio_pct / 100.0).clamp(0.0, 1.0)
    } else {
        1.0
    };

    let uncached_rpc_millions = base_logical_ops_millions * rpc_amp;
    let actual_class_a_millions = uncached_rpc_millions * class_a_share;
    let actual_class_b_millions = if tp.total_http_requests > 0 && tp.logical_ops > 0 {
        // `rpc_amp` already reflects actual backend HTTP requests per logical op (after cache hits!)
        uncached_rpc_millions * class_b_share
    } else {
        uncached_rpc_millions * class_b_share * cache_miss_factor
    };

    let monthly_class_a_usd = actual_class_a_millions * pricing.class_a_put_list_usd_per_million;
    let monthly_class_b_usd = actual_class_b_millions * pricing.class_b_get_head_usd_per_million;
    let actual_api_usd = monthly_class_a_usd + monthly_class_b_usd;

    // Ideal unamplified API cost (1 RPC per logical operation, no split/retry amplification)
    let ideal_api_usd = (base_logical_ops_millions
        * class_a_share
        * pricing.class_a_put_list_usd_per_million)
        + (base_logical_ops_millions
            * class_b_share
            * cache_miss_factor
            * pricing.class_b_get_head_usd_per_million);

    // Egress cost reflects net wire read factor (`net_wire_read_ratio` includes both gap overfetch & cache savings)
    let effective_egress_factor = if tp.logical_read_bytes > 0 {
        tp.net_wire_read_ratio.max(0.0)
    } else {
        read_amp * cache_miss_factor
    };
    let read_share_bytes = if tp.logical_bytes > 0 {
        (tp.logical_read_bytes as f64 / tp.logical_bytes as f64).clamp(0.0, 1.0)
    } else {
        1.0 - fallback_write_ratio.clamp(0.0, 1.0)
    };

    let logical_read_monthly_gb = logical_monthly_gb * read_share_bytes;
    let ideal_egress_usd =
        logical_read_monthly_gb * cache_miss_factor * egress_ratio * pricing.egress_usd_per_gb;
    let actual_egress_usd =
        logical_read_monthly_gb * effective_egress_factor * egress_ratio * pricing.egress_usd_per_gb;
    let no_cache_egress_usd =
        logical_read_monthly_gb * read_amp * egress_ratio * pricing.egress_usd_per_gb;

    let storage_usd = stored_gb * pricing.storage_usd_per_gb_month;
    let total_usd = storage_usd + actual_api_usd + actual_egress_usd;
    let ideal_usd = storage_usd + ideal_api_usd + ideal_egress_usd;

    let no_cache_class_b_usd = if cache_miss_factor > 0.01 {
        monthly_class_b_usd / cache_miss_factor
    } else {
        base_logical_ops_millions * class_b_share * pricing.class_b_get_head_usd_per_million
    };
    let no_cache_total =
        storage_usd + monthly_class_a_usd + no_cache_class_b_usd + no_cache_egress_usd;

    CostProjection {
        provider_name: pricing.provider_name.clone(),
        monthly_storage_usd: storage_usd,
        monthly_class_a_usd,
        monthly_class_b_usd,
        monthly_api_requests_usd: actual_api_usd,
        monthly_egress_usd: actual_egress_usd,
        total_monthly_usd: total_usd,
        waste_from_amplification_usd: (total_usd - ideal_usd).max(0.0),
        saved_by_cache_usd: (no_cache_total - total_usd).max(0.0),
        usd_per_logical_tb_scanned: (actual_api_usd + actual_egress_usd)
            / pricing.monthly_logical_access_tb.max(0.1),
    }
}

/// Compare all 5 standard cloud storage providers at the given scale.
pub fn compare_multi_cloud_finops_scaled(
    tp: &TransportAndHostSnapshot,
    write_ratio: f64,
    egress_ratio: f64,
    stored_tb: f64,
    access_tb: f64,
) -> Vec<CostProjection> {
    CloudPricingProfile::standard_catalog_with_scale(stored_tb, access_tb)
        .iter()
        .map(|p| estimate_monthly_finops(p, tp, write_ratio, egress_ratio))
        .collect()
}

/// Compare all 5 standard cloud storage providers at default scale (10 TB stored / 50 TB monthly logical access).
pub fn compare_multi_cloud_finops(
    tp: &TransportAndHostSnapshot,
    write_ratio: f64,
    egress_ratio: f64,
) -> Vec<CostProjection> {
    compare_multi_cloud_finops_scaled(tp, write_ratio, egress_ratio, 10.0, 50.0)
}
