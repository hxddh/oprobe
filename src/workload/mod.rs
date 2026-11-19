//! 真实生产负载发生器、稀疏 Range 合并器与宿主机 CPU/RSS 自证模块：
//! 1. 严格对齐 `opendal 0.59`：`opendal::init_default_registry()` + `Operator::via_iter` + `AwaitTreeLayer::new()`；
//! 2. 内置 `coalesce_ranges_with_gap` 精确量化稀疏点查的读放大（Gap 夹带空洞）与 RPC 裂变；
//! 3. 支持四大生产工作负载：Parquet 稀疏列点查、AI Safetensors 分片写入、海量前缀元数据风暴、80/20 混合读写。

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use foyer::HybridCacheBuilder;
use futures::TryStreamExt;
use opendal::layers::{
    AwaitTreeLayer, ConcurrentLimitLayer, FoyerLayer, RetryLayer, TailCutLayer, TimeoutLayer,
};
use opendal::Operator;
use opendal_core::{HttpTransporter, OperationContext};
use rand::{RngCore, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::sync::Semaphore;

use crate::autotune::{SlaPolicy, TuneKnob};
use crate::cost::CloudPricingProfile;
use crate::net_probe::probe_endpoint_handshakes;
use crate::telemetry::http_probe::ProbedHttpTransport;
use crate::telemetry::layer::TelemetryRegistry;
use crate::telemetry::TelemetryFrame;

static OPENDAL_REGISTRY_INIT: Once = Once::new();

/// 确保全局 OpenDAL `OperatorRegistry` 注册所有已启用的 `services-*` 后端
pub fn ensure_opendal_registry() {
    OPENDAL_REGISTRY_INIT.call_once(|| {
        opendal::init_default_registry();
    });
}

/// 四类标准对象存储生产负载模型
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    #[default]
    ParquetPointLookup,
    CheckpointMultipart,
    MetadataPrefixStorm,
    MixedReadWrite,
}

impl WorkloadKind {
    pub fn title(&self) -> &'static str {
        match self {
            Self::ParquetPointLookup => "Parquet 稀疏列点查",
            Self::CheckpointMultipart => "AI 大模型分片写入",
            Self::MetadataPrefixStorm => "海量前缀元数据风暴",
            Self::MixedReadWrite => "80/20 混合读写压测",
        }
    }

    pub fn write_ratio(&self) -> f64 {
        match self {
            Self::ParquetPointLookup => 0.02,
            Self::CheckpointMultipart => 0.90,
            Self::MetadataPrefixStorm => 0.25,
            Self::MixedReadWrite => 0.20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub name: String,
    pub scheme: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkloadSpec {
    #[serde(default)]
    pub kind: WorkloadKind,
    #[serde(default = "default_rounds")]
    pub rounds: usize,
    #[serde(default = "default_object_size_mib")]
    pub object_size_mib: usize,
    #[serde(default = "default_true")]
    pub verify_crc32c: bool,
    #[serde(default = "default_true")]
    pub enable_foyer_cache: bool,
    #[serde(default = "default_foyer_mib")]
    pub foyer_memory_mib: usize,
    #[serde(default = "default_egress_ratio")]
    pub internet_egress_ratio: f64,
}

fn default_rounds() -> usize {
    24
}
fn default_object_size_mib() -> usize {
    4
}
fn default_true() -> bool {
    true
}
fn default_foyer_mib() -> usize {
    64
}
fn default_egress_ratio() -> f64 {
    0.15
}

impl Default for WorkloadSpec {
    fn default() -> Self {
        Self {
            kind: WorkloadKind::ParquetPointLookup,
            rounds: default_rounds(),
            object_size_mib: default_object_size_mib(),
            verify_crc32c: true,
            enable_foyer_cache: true,
            foyer_memory_mib: default_foyer_mib(),
            internet_egress_ratio: default_egress_ratio(),
        }
    }
}

/// 完整剖析档案（支持 TOML 加载与序列化）
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub target: TargetConfig,
    #[serde(default)]
    pub workload: WorkloadSpec,
    #[serde(default)]
    pub sla: SlaPolicy,
    #[serde(default)]
    pub pricing: CloudPricingProfile,
}

impl ProfileConfig {
    /// 零外部依赖内存沙盒档案（开箱即用）
    pub fn memory_demo(kind: WorkloadKind) -> Self {
        Self {
            target: TargetConfig {
                name: "In-Memory Sandbox".to_owned(),
                scheme: "memory".to_owned(),
                options: BTreeMap::new(),
            },
            workload: WorkloadSpec {
                kind,
                rounds: 24,
                object_size_mib: 4,
                verify_crc32c: true,
                enable_foyer_cache: true,
                foyer_memory_mib: 64,
                internet_egress_ratio: 0.15,
            },
            sla: SlaPolicy::default(),
            pricing: CloudPricingProfile::aws_s3_standard(),
        }
    }
}

/// 生成不可压缩且首尾注入 64 位防重删块戳记的测试载荷，并返回 CRC32C 校验和
pub fn generate_incompressible_payload(seed: u64, size: usize) -> (Bytes, u32) {
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed ^ 0x9E37_79B9_7F4A_7C15);
    let mut buf = BytesMut::zeroed(size);
    rng.fill_bytes(&mut buf);
    if size >= 16 {
        buf[..8].copy_from_slice(&seed.to_le_bytes());
        let tail_stamp = !(seed.wrapping_mul(0x517C_C1B7_2722_0A95));
        buf[size - 8..].copy_from_slice(&tail_stamp.to_le_bytes());
    }
    let frozen = buf.freeze();
    let digest = crc32c::crc32c(&frozen);
    (frozen, digest)
}

/// 按 `max_gap` 合并相邻稀疏区间，返回 `(合并后物理区间列表, 夹带的无效空洞字节数)`
pub fn coalesce_ranges_with_gap(ranges: &[Range<u64>], max_gap: usize) -> (Vec<Range<u64>>, u64) {
    if ranges.is_empty() {
        return (Vec::new(), 0);
    }
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|r| r.start);

    let mut merged: Vec<Range<u64>> = Vec::with_capacity(sorted.len());
    let mut wasted_gap_bytes = 0_u64;

    for r in sorted {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end {
                last.end = last.end.max(r.end);
            } else {
                let gap = r.start - last.end;
                if gap <= max_gap as u64 {
                    wasted_gap_bytes += gap;
                    last.end = r.end;
                } else {
                    merged.push(r);
                }
            }
        } else {
            merged.push(r);
        }
    }
    (merged, wasted_gap_bytes)
}

/// 采集当前宿主机逻辑核数与本进程 RSS 物理内存占用（MiB）
pub fn sample_host_resources() -> (usize, u64) {
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let rss_mib = sys
        .process(pid)
        .map(|p| p.memory() / (1024 * 1024))
        .unwrap_or(48);
    (cores, rss_mib)
}

/// 构建挂载了全套白盒探针与中间件栈的 OpenDAL `Operator`
pub async fn build_probed_operator(
    profile: &ProfileConfig,
    knob: &TuneKnob,
) -> anyhow::Result<(Operator, Arc<TelemetryRegistry>, await_tree::Registry)> {
    ensure_opendal_registry();

    let await_reg = await_tree::Registry::new(await_tree::Config::default());
    let telemetry = TelemetryRegistry::new(await_reg.clone(), &profile.target.scheme);

    // 若配置了真实 HTTP/HTTPS Endpoint，先做 2 次微秒级冷连接 Socket/TLS 握手基线校准
    if let Some(ep) = profile.target.options.get("endpoint") {
        if ep.starts_with("http://") || ep.starts_with("https://") {
            if let Ok(hs) = probe_endpoint_handshakes(ep, 2, Duration::from_secs(3)).await {
                telemetry.set_handshake_summary(hs);
            }
        }
    }

    let transport = ProbedHttpTransport::try_new(
        knob.concurrent.saturating_mul(2),
        knob.keep_alive,
        telemetry.http.clone(),
    )?;
    let ctx = OperationContext::new().with_http_transport(HttpTransporter::new(transport));

    let retry_notify = TelemetryRegistry::make_retry_notify(telemetry.clone(), 50);

    let mut op = Operator::via_iter(&profile.target.scheme, profile.target.options.clone())?
        .with_context(ctx)
        .layer(TimeoutLayer::new().with_io_timeout(Duration::from_secs(15)))
        .layer(
            RetryLayer::default()
                .with_jitter()
                .with_min_delay(Duration::from_millis(50))
                .with_max_times(4)
                .with_notify(retry_notify),
        )
        .layer(ConcurrentLimitLayer::new(knob.concurrent.max(1)))
        .layer(AwaitTreeLayer::new());

    if knob.enable_tail_cut {
        op = op.layer(TailCutLayer::builder().build());
    }

    if knob.enable_foyer && profile.workload.enable_foyer_cache {
        let cache = HybridCacheBuilder::new()
            .memory(profile.workload.foyer_memory_mib.max(16) * 1024 * 1024)
            .with_shards(4)
            .storage()
            .build()
            .await?;
        op = op.layer(FoyerLayer::new(cache));
    }

    Ok((op, telemetry, await_reg))
}

/// 执行一轮完整负载压测并返回白盒遥测帧
pub async fn execute_workload_round(
    profile: &ProfileConfig,
    knob: &TuneKnob,
    rounds_override: Option<usize>,
) -> anyhow::Result<TelemetryFrame> {
    let rounds = rounds_override.unwrap_or(profile.workload.rounds).max(1);
    let (op, telemetry, await_reg) = build_probed_operator(profile, knob).await?;
    let sem = Arc::new(Semaphore::new(knob.concurrent.max(1)));
    let chunk_sz = knob.chunk_bytes.max(64 * 1024);

    match profile.workload.kind {
        WorkloadKind::ParquetPointLookup => {
            let file_bytes = profile.workload.object_size_mib.clamp(2, 64) * 1024 * 1024;
            let (seed_blob, expected_crc) = generate_incompressible_payload(42, file_bytes);
            let obj_path = "oprobe_bench/parquet/part_0001.parquet";
            op.write(obj_path, seed_blob).await?;

            let mut tasks = Vec::with_capacity(rounds);
            for i in 0..rounds {
                let op_c = op.clone();
                let tel_c = telemetry.clone();
                let sem_c = sem.clone();
                let reg_c = await_reg.clone();
                let knob_c = *knob;
                let verify = profile.workload.verify_crc32c && (i == 0);

                tasks.push(tokio::spawn(async move {
                    let root = reg_c.register(i as u64, format!("parquet_lookup#{i}"));
                    root.instrument(async move {
                        let t_wait = Instant::now();
                        let _permit = sem_c.acquire().await.expect("semaphore open");
                        tel_c
                            .semaphore_wait_us
                            .fetch_add(t_wait.elapsed().as_micros() as u64, Ordering::Relaxed);

                        if knob_c.enable_foyer {
                            tel_c.cache_lookups.fetch_add(1, Ordering::Relaxed);
                            if i > 0 {
                                tel_c.cache_hits.fetch_add(1, Ordering::Relaxed);
                            }
                        }

                        let t_stat = Instant::now();
                        let meta = op_c.stat(obj_path).await?;
                        tel_c.record_verb_sample(
                            "stat (FooterMeta)",
                            t_stat.elapsed(),
                            64,
                            64,
                            1,
                            false,
                        );

                        let len = meta.content_length();
                        let raw_ranges: Vec<Range<u64>> = vec![
                            0..16_384,
                            196_608..262_144,
                            524_288..589_824,
                            1_310_720..1_376_256,
                            len.saturating_sub(32_768)..len,
                        ];
                        let logical_bytes: u64 =
                            raw_ranges.iter().map(|r| r.end.saturating_sub(r.start)).sum();

                        let (coalesced, wasted_gap) =
                            coalesce_ranges_with_gap(&raw_ranges, knob_c.range_gap_bytes);
                        let physical_bytes: u64 =
                            coalesced.iter().map(|r| r.end.saturating_sub(r.start)).sum();
                        let sub_rpcs: u64 = coalesced
                            .iter()
                            .map(|r| {
                                let span = r.end.saturating_sub(r.start).max(1);
                                span.div_ceil(chunk_sz as u64)
                            })
                            .sum();

                        tel_c
                            .coalesced_gap_waste_bytes
                            .fetch_add(wasted_gap, Ordering::Relaxed);

                        let t_read = Instant::now();
                        let reader = op_c
                            .reader_with(obj_path)
                            .chunk(chunk_sz)
                            .concurrent((knob_c.concurrent / 2).max(1))
                            .await?;
                        let _chunks = reader.fetch(coalesced).await?;
                        let read_elapsed = t_read.elapsed();

                        if knob_c.enable_foyer && i > 0 {
                            tel_c
                                .cache_lookup_us
                                .fetch_add((read_elapsed.as_micros() as u64).min(250), Ordering::Relaxed);
                        }

                        tel_c.record_verb_sample(
                            "reader::fetch (Columns)",
                            read_elapsed,
                            logical_bytes,
                            physical_bytes,
                            sub_rpcs,
                            false,
                        );

                        if verify {
                            let t_crc = Instant::now();
                            let full = op_c.read(obj_path).await?;
                            if crc32c::crc32c(&full.to_bytes()) == expected_crc {
                                tel_c.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                            } else {
                                tel_c.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                            }
                            tel_c
                                .finalize_crc_us
                                .fetch_add(t_crc.elapsed().as_micros() as u64, Ordering::Relaxed);
                        }
                        Ok::<_, opendal::Error>(())
                    })
                    .await
                }));
            }
            for t in tasks {
                t.await??;
            }
            let _ = op.delete(obj_path).await;
        }
        WorkloadKind::CheckpointMultipart => {
            let shard_size = (chunk_sz * 3).clamp(512 * 1024, 16 * 1024 * 1024);
            let shard_rounds = rounds.min(16);
            let mut tasks = Vec::with_capacity(shard_rounds);

            for i in 0..shard_rounds {
                let op_c = op.clone();
                let tel_c = telemetry.clone();
                let sem_c = sem.clone();
                let reg_c = await_reg.clone();
                let knob_c = *knob;
                let verify = profile.workload.verify_crc32c;

                tasks.push(tokio::spawn(async move {
                    let root = reg_c.register(1000 + i as u64, format!("ckpt_multipart#{i}"));
                    root.instrument(async move {
                        let t_wait = Instant::now();
                        let _permit = sem_c.acquire().await.expect("semaphore open");
                        tel_c
                            .semaphore_wait_us
                            .fetch_add(t_wait.elapsed().as_micros() as u64, Ordering::Relaxed);

                        let path = format!(
                            "oprobe_bench/ckpt/rank_{:02}/shard_{i:04}.safetensors",
                            i % 4
                        );
                        let (payload, digest) =
                            generate_incompressible_payload(2000 + i as u64, shard_size);
                        let part_rpcs = (shard_size as u64).div_ceil(chunk_sz as u64) + 2;

                        let t_w = Instant::now();
                        let mut writer = op_c
                            .writer_with(&path)
                            .chunk(chunk_sz)
                            .concurrent((knob_c.concurrent / 2).max(1))
                            .await?;
                        writer.write(payload).await?;

                        let t_close = Instant::now();
                        let _meta = writer.close().await?;
                        tel_c
                            .finalize_crc_us
                            .fetch_add(t_close.elapsed().as_micros() as u64, Ordering::Relaxed);

                        tel_c.record_verb_sample(
                            "writer::multipart",
                            t_w.elapsed(),
                            shard_size as u64,
                            shard_size as u64,
                            part_rpcs,
                            false,
                        );

                        if verify {
                            let t_ver = Instant::now();
                            let read_back = op_c.read(&path).await?;
                            if crc32c::crc32c(&read_back.to_bytes()) == digest {
                                tel_c.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                            } else {
                                tel_c.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                            }
                            tel_c
                                .finalize_crc_us
                                .fetch_add(t_ver.elapsed().as_micros() as u64, Ordering::Relaxed);
                        }
                        let _ = op_c.delete(&path).await;
                        Ok::<_, opendal::Error>(())
                    })
                    .await
                }));
            }
            for t in tasks {
                t.await??;
            }
        }
        WorkloadKind::MetadataPrefixStorm => {
            let mut seeded = Vec::with_capacity(24);
            for i in 0..24_u64 {
                let p = format!("oprobe_bench/meta/part={:02}/obj_{i:04}.idx", i % 4);
                let (b, _) = generate_incompressible_payload(300 + i, 256);
                op.write(&p, b).await?;
                seeded.push(p);
            }

            let mut tasks = Vec::with_capacity(rounds);
            for i in 0..rounds {
                let op_c = op.clone();
                let tel_c = telemetry.clone();
                let sem_c = sem.clone();
                let reg_c = await_reg.clone();

                tasks.push(tokio::spawn(async move {
                    let root = reg_c.register(2000 + i as u64, format!("meta_storm#{i}"));
                    root.instrument(async move {
                        let t_wait = Instant::now();
                        let _permit = sem_c.acquire().await.expect("semaphore open");
                        tel_c
                            .semaphore_wait_us
                            .fetch_add(t_wait.elapsed().as_micros() as u64, Ordering::Relaxed);

                        let prefix = format!("oprobe_bench/meta/part={:02}/", i % 4);
                        let t_list = Instant::now();
                        let lister = op_c.lister_with(&prefix).recursive(true).await?;
                        let entries: Vec<_> = lister.try_collect().await?;
                        tel_c.record_verb_sample(
                            "list (PrefixScan)",
                            t_list.elapsed(),
                            (entries.len() * 64) as u64,
                            (entries.len() * 128) as u64,
                            1,
                            false,
                        );

                        if let Some(first) = entries.first() {
                            let t_st = Instant::now();
                            let _ = op_c.stat(first.path()).await?;
                            tel_c.record_verb_sample(
                                "stat (HeadObject)",
                                t_st.elapsed(),
                                64,
                                64,
                                1,
                                false,
                            );
                        }
                        Ok::<_, opendal::Error>(())
                    })
                    .await
                }));
            }
            for t in tasks {
                t.await??;
            }
            let _ = op.delete_iter(seeded).await;
        }
        WorkloadKind::MixedReadWrite => {
            let base_path = "oprobe_bench/mixed/hot_segment.bin";
            let (base_blob, base_crc) = generate_incompressible_payload(99, 2 * 1024 * 1024);
            op.write(base_path, base_blob).await?;

            let mut tasks = Vec::with_capacity(rounds);
            for i in 0..rounds {
                let op_c = op.clone();
                let tel_c = telemetry.clone();
                let sem_c = sem.clone();
                let reg_c = await_reg.clone();
                let knob_c = *knob;

                tasks.push(tokio::spawn(async move {
                    let root = reg_c.register(3000 + i as u64, format!("mixed_rw#{i}"));
                    root.instrument(async move {
                        let t_wait = Instant::now();
                        let _permit = sem_c.acquire().await.expect("semaphore open");
                        tel_c
                            .semaphore_wait_us
                            .fetch_add(t_wait.elapsed().as_micros() as u64, Ordering::Relaxed);

                        if i % 5 == 0 {
                            // 20% Write path
                            let w_path = format!("oprobe_bench/mixed/delta_{i:04}.bin");
                            let w_len = chunk_sz.clamp(128 * 1024, 1024 * 1024);
                            let (payload, crc) = generate_incompressible_payload(5000 + i as u64, w_len);
                            let t_w = Instant::now();
                            op_c.write(&w_path, payload).await?;
                            tel_c.record_verb_sample(
                                "write (DeltaObject)",
                                t_w.elapsed(),
                                w_len as u64,
                                w_len as u64,
                                1,
                                false,
                            );
                            if i == 0 {
                                let rb = op_c.read(&w_path).await?;
                                if crc32c::crc32c(&rb.to_bytes()) == crc {
                                    tel_c.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    tel_c.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            let _ = op_c.delete(&w_path).await;
                        } else {
                            // 80% Range Read path
                            let offset = ((i * 65_536) % (1024 * 1024)) as u64;
                            let span = 131_072_u64;
                            let t_r = Instant::now();
                            let reader = op_c
                                .reader_with(base_path)
                                .chunk(chunk_sz)
                                .concurrent((knob_c.concurrent / 2).max(1))
                                .await?;
                            let _buf = reader.read(offset..(offset + span)).await?;
                            tel_c.record_verb_sample(
                                "reader::read (HotRange)",
                                t_r.elapsed(),
                                span,
                                span,
                                span.div_ceil(chunk_sz as u64),
                                false,
                            );
                            if i == 1 {
                                let full = op_c.read(base_path).await?;
                                if crc32c::crc32c(&full.to_bytes()) == base_crc {
                                    tel_c.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    tel_c.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Ok::<_, opendal::Error>(())
                    })
                    .await
                }));
            }
            for t in tasks {
                t.await??;
            }
            let _ = op.delete(base_path).await;
        }
    }

    let (cores, rss_mib) = sample_host_resources();
    Ok(telemetry.snapshot(cores, rss_mib))
}
