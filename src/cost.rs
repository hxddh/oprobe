//! 多云对象存储 TCO 与请求/流量放大成本精算器：
//! 内置 AWS S3 Standard、Cloudflare R2、Backblaze B2 标准公开费率，
//! 根据实测 `rpc_amplification` 与 `read_amplification` 计算每扫描 100 TiB 逻辑数据的真实账单与放大损耗。

use serde::{Deserialize, Serialize};

use crate::telemetry::TransportAndHostSnapshot;

/// 云存储计费单价模型（美元）
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloudPricingProfile {
    pub provider_name: String,
    pub storage_usd_per_gb_month: f64,
    pub class_a_put_list_usd_per_million: f64,
    pub class_b_get_head_usd_per_million: f64,
    pub egress_usd_per_gb: f64,
}

impl Default for CloudPricingProfile {
    fn default() -> Self {
        Self::aws_s3_standard()
    }
}

impl CloudPricingProfile {
    /// AWS S3 Standard (us-east-1 公开目录价)
    pub fn aws_s3_standard() -> Self {
        Self {
            provider_name: "AWS S3 Standard".to_owned(),
            storage_usd_per_gb_month: 0.023,
            class_a_put_list_usd_per_million: 5.00,
            class_b_get_head_usd_per_million: 0.40,
            egress_usd_per_gb: 0.09,
        }
    }

    /// Cloudflare R2 (零公网流出费)
    pub fn cloudflare_r2() -> Self {
        Self {
            provider_name: "Cloudflare R2".to_owned(),
            storage_usd_per_gb_month: 0.015,
            class_a_put_list_usd_per_million: 4.50,
            class_b_get_head_usd_per_million: 0.36,
            egress_usd_per_gb: 0.00,
        }
    }

    /// Backblaze B2 Reserve/Pay-As-You-Go
    pub fn backblaze_b2() -> Self {
        Self {
            provider_name: "Backblaze B2".to_owned(),
            storage_usd_per_gb_month: 0.006,
            class_a_put_list_usd_per_million: 0.00,
            class_b_get_head_usd_per_million: 0.40,
            egress_usd_per_gb: 0.01,
        }
    }

    pub fn standard_catalog() -> [Self; 3] {
        [
            Self::aws_s3_standard(),
            Self::cloudflare_r2(),
            Self::backblaze_b2(),
        ]
    }
}

/// 单云厂商月度归一化成本预测（按默认存储 10 TB、月度逻辑访问 50 TB 折算）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostProjection {
    pub provider_name: String,
    pub monthly_storage_usd: f64,
    pub monthly_api_requests_usd: f64,
    pub monthly_egress_usd: f64,
    pub total_monthly_usd: f64,
    pub waste_from_amplification_usd: f64,
    pub usd_per_logical_tb_scanned: f64,
}

/// 基于当前实测读写放大与单次请求平均载荷估算月度云账单（基准规模：10 TB 驻留数据，月度逻辑处理 50 TB，公网流出占比 `egress_ratio`）
pub fn estimate_monthly_finops(
    pricing: &CloudPricingProfile,
    tp: &TransportAndHostSnapshot,
    write_ratio: f64,
    egress_ratio: f64,
) -> CostProjection {
    let stored_gb = 10_000.0;
    let logical_monthly_gb = 50_000.0;
    let write_ratio = write_ratio.clamp(0.0, 1.0);
    let egress_ratio = egress_ratio.clamp(0.0, 1.0);

    let read_amp = tp.read_amplification.max(1.0);
    let rpc_amp = tp.rpc_amplification.max(1.0);

    // 根据实测平均每次顶层逻辑操作的字节数推算 50TB 对应的逻辑操作数
    let avg_logical_op_bytes = if tp.total_http_requests > 0 {
        ((tp.logical_bytes as f64) / ((tp.total_http_requests as f64) / rpc_amp)).max(16_384.0)
    } else {
        262_144.0
    };

    let base_logical_ops_millions =
        (logical_monthly_gb * 1_000_000_000.0 / avg_logical_op_bytes) / 1_000_000.0;
    let actual_rpc_millions = base_logical_ops_millions * rpc_amp;

    let class_a_millions = actual_rpc_millions * write_ratio;
    let class_b_millions = actual_rpc_millions * (1.0 - write_ratio);

    let base_api_usd = (base_logical_ops_millions * write_ratio * pricing.class_a_put_list_usd_per_million)
        + (base_logical_ops_millions * (1.0 - write_ratio) * pricing.class_b_get_head_usd_per_million);
    let actual_api_usd = (class_a_millions * pricing.class_a_put_list_usd_per_million)
        + (class_b_millions * pricing.class_b_get_head_usd_per_million);

    let base_egress_usd = logical_monthly_gb * egress_ratio * pricing.egress_usd_per_gb;
    let actual_egress_usd = logical_monthly_gb * read_amp * egress_ratio * pricing.egress_usd_per_gb;

    let storage_usd = stored_gb * pricing.storage_usd_per_gb_month;
    let total_usd = storage_usd + actual_api_usd + actual_egress_usd;
    let ideal_usd = storage_usd + base_api_usd + base_egress_usd;

    CostProjection {
        provider_name: pricing.provider_name.clone(),
        monthly_storage_usd: storage_usd,
        monthly_api_requests_usd: actual_api_usd,
        monthly_egress_usd: actual_egress_usd,
        total_monthly_usd: total_usd,
        waste_from_amplification_usd: (total_usd - ideal_usd).max(0.0),
        usd_per_logical_tb_scanned: (actual_api_usd + actual_egress_usd) / 50.0,
    }
}

/// 生成三朵主流云（AWS S3 / Cloudflare R2 / Backblaze B2）在当前实测参数下的横向对比表
pub fn compare_multi_cloud_finops(
    tp: &TransportAndHostSnapshot,
    write_ratio: f64,
    egress_ratio: f64,
) -> Vec<CostProjection> {
    CloudPricingProfile::standard_catalog()
        .iter()
        .map(|p| estimate_monthly_finops(p, tp, write_ratio, egress_ratio))
        .collect()
}
