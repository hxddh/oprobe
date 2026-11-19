//! 白盒遥测数据契约与八维瓶颈自动定责引擎：
//! 将单次对象存储 I/O 全生命周期拆解为六段微秒级瀑布，并输出可执行调优处方。

pub mod http_probe;
pub mod layer;

use serde::{Deserialize, Serialize};

use crate::net_probe::EndpointHandshakeSummary;

/// 六段式全栈耗时瀑布（单位：微秒）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StageWaterfall {
    pub semaphore_wait_us: u64,
    pub cache_lookup_us: u64,
    pub conn_tls_sign_us: u64,
    pub remote_ttfb_us: u64,
    pub body_stream_us: u64,
    pub finalize_crc_us: u64,
}

impl StageWaterfall {
    pub fn total_us(&self) -> u64 {
        self.semaphore_wait_us
            + self.cache_lookup_us
            + self.conn_tls_sign_us
            + self.remote_ttfb_us
            + self.body_stream_us
            + self.finalize_crc_us
    }

    pub fn total_ms(&self) -> f64 {
        self.total_us() as f64 / 1000.0
    }

    /// 返回六个阶段的 `(名称, 毫秒耗时, 占比百分比)`
    pub fn breakdown_pct(&self) -> [(&'static str, f64, f64); 6] {
        let tot = self.total_us().max(1) as f64;
        let pct = |v: u64| (v as f64 / tot) * 100.0;
        let ms = |v: u64| v as f64 / 1000.0;
        [
            ("1.Semaphore排队", ms(self.semaphore_wait_us), pct(self.semaphore_wait_us)),
            ("2.Foyer混合缓存", ms(self.cache_lookup_us), pct(self.cache_lookup_us)),
            ("3.DNS/TCP/TLS", ms(self.conn_tls_sign_us), pct(self.conn_tls_sign_us)),
            ("4.远端网关TTFB", ms(self.remote_ttfb_us), pct(self.remote_ttfb_us)),
            ("5.分块流传输", ms(self.body_stream_us), pct(self.body_stream_us)),
            ("6.合并与CRC32C", ms(self.finalize_crc_us), pct(self.finalize_crc_us)),
        ]
    }
}

/// 分操作语义独立 HDR 分位统计行（杜绝 `stat` 稀释 `read/write` 长尾）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VerbMetricsRow {
    pub verb: String,
    pub count: u64,
    pub ops_per_sec: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub max_ms: f64,
    pub stddev_ms: f64,
    pub single_stream_mib_s: f64,
    pub throttle_429_503_pct: f64,
}

/// 100ms 滑动窗口时序采样点（驱动终端实时 Sparkline）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeSeriesPoint {
    pub elapsed_ms: u64,
    pub throughput_mib_s: f64,
    pub p99_ms: f64,
    pub inflight: usize,
}

/// 传输层、放大系数与宿主机资源自证快照
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TransportAndHostSnapshot {
    pub socket_reuse_pct: f64,
    pub active_inflight: usize,
    pub total_http_requests: u64,
    pub http_200_count: u64,
    pub http_206_range_count: u64,
    pub http_403_auth_count: u64,
    pub http_429_count: u64,
    pub http_500_count: u64,
    pub http_503_slowdown_count: u64,
    pub retry_attempts: u64,
    pub retry_backoff_ms: f64,
    pub cache_hit_ratio_pct: f64,
    pub logical_bytes: u64,
    pub physical_wire_bytes: u64,
    pub coalesced_gap_waste_bytes: u64,
    pub read_amplification: f64,
    pub rpc_amplification: f64,
    pub crc32c_verified_ok: u64,
    pub crc32c_corrupted: u64,
    pub aggregate_mib_s: f64,
    pub mib_per_cpu_core: f64,
    pub cpu_cores: usize,
    pub process_rss_mib: u64,
    pub recent_request_ids: Vec<String>,
}

/// 诊断严重等级
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Healthy,
    Advisory,
    Warning,
    Critical,
}

impl Severity {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Healthy => "HEALTHY",
            Self::Advisory => "ADVISORY",
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
        }
    }
}

/// 自动根因诊断结论与调优建议
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BottleneckDiagnosis {
    pub severity: Severity,
    pub primary_stage: String,
    pub headline: String,
    pub recommendation: String,
    pub findings: Vec<String>,
}

impl BottleneckDiagnosis {
    pub fn evaluate(wf: &StageWaterfall, tp: &TransportAndHostSnapshot) -> Self {
        let mut findings = Vec::new();
        let total = wf.total_us().max(1) as f64;

        if tp.crc32c_corrupted > 0 {
            return Self {
                severity: Severity::Critical,
                primary_stage: "DataIntegrity".to_owned(),
                headline: format!("检测到 {} 次端到端 CRC32C 校验不一致", tp.crc32c_corrupted),
                recommendation: "立即排查自定义代理、对象存储网关分片合并顺序或本地内存位翻转。".to_owned(),
                findings: vec!["数据完整性红线告警：读回载荷摘要与写入种子不匹配。".to_owned()],
            };
        }

        if tp.http_503_slowdown_count > 0 || tp.http_429_count > 0 {
            let throttled = tp.http_503_slowdown_count + tp.http_429_count;
            findings.push(format!(
                "远端返回 {throttled} 次 HTTP 429/503 SlowDown 限流，触发 {} 次重试。",
                tp.retry_attempts
            ));
            return Self {
                severity: Severity::Critical,
                primary_stage: "RemotePartitionThrottle".to_owned(),
                headline: format!("触发远端存储分区限流 (429/503 共 {throttled} 次)"),
                recommendation: "下调 ConcurrentLimitLayer 并发上限、打散对象 Key 哈希前缀，并启用指数退避 Jitter。".to_owned(),
                findings,
            };
        }

        if tp.total_http_requests >= 4 && tp.socket_reuse_pct < 65.0 {
            findings.push(format!(
                "HTTP 连接池复用率仅 {:.1}%，大量耗时浪费在重复 TCP/TLS 握手。",
                tp.socket_reuse_pct
            ));
            return Self {
                severity: Severity::Warning,
                primary_stage: "SocketPoolThrashing".to_owned(),
                headline: format!("连接池严重抖动 (Socket 复用率 {:.1}%)", tp.socket_reuse_pct),
                recommendation: "开启 HTTP Keep-Alive 并将 pool_max_idle_per_host 调至工作并发度的 2 倍以上。".to_owned(),
                findings,
            };
        }

        let sem_share = wf.semaphore_wait_us as f64 / total;
        if sem_share > 0.38 && wf.semaphore_wait_us > 500 {
            findings.push(format!(
                "客户端信号量排队占总耗时 {:.1}% ({:.2} ms)。",
                sem_share * 100.0,
                wf.semaphore_wait_us as f64 / 1000.0
            ));
            return Self {
                severity: Severity::Warning,
                primary_stage: "ClientSemaphoreStarvation".to_owned(),
                headline: format!("客户端并发许可饥饿 (排队占 {:.0}%)", sem_share * 100.0),
                recommendation: "逐步上调 ConcurrentLimitLayer 与 Reader/Writer 并发数，直至远端 TTFB 开始拐点抬升。".to_owned(),
                findings,
            };
        }

        if tp.read_amplification > 2.20 {
            findings.push(format!(
                "物理读取字节为逻辑有效列数据的 {:.2}x（Gap 合并夹带 {} KiB 无效空洞）。",
                tp.read_amplification,
                tp.coalesced_gap_waste_bytes / 1024
            ));
            return Self {
                severity: Severity::Advisory,
                primary_stage: "HighReadAmplification".to_owned(),
                headline: format!("稀疏读放大偏高 ({:.2}x 带宽浪费)", tp.read_amplification),
                recommendation: "收窄 ReadOptions gap 合并阈值（如从 2MiB 降至 256KiB~512KiB），避免跨列全量拉取。".to_owned(),
                findings,
            };
        }

        if tp.rpc_amplification > 4.50 {
            findings.push(format!(
                "单次逻辑操作裂变为 {:.2} 次底层请求，Class A/B API 计费与头部开销成倍膨胀。",
                tp.rpc_amplification
            ));
            return Self {
                severity: Severity::Advisory,
                primary_stage: "HighRpcAmplification".to_owned(),
                headline: format!("请求裂变放大偏高 (RPC 放大 {:.2}x)", tp.rpc_amplification),
                recommendation: "增大 chunk 分块尺寸（S3/R2 推荐 4MiB~16MiB）或适度调高 Parquet range gap 合并邻近块。".to_owned(),
                findings,
            };
        }

        let ttfb_share = wf.remote_ttfb_us as f64 / total;
        if ttfb_share > 0.55 && wf.remote_ttfb_us > 15_000 {
            findings.push(format!(
                "远端首包 TTFB 占比 {:.1}% ({:.2} ms)，客户端与网络建连均正常。",
                ttfb_share * 100.0,
                wf.remote_ttfb_us as f64 / 1000.0
            ));
            return Self {
                severity: Severity::Advisory,
                primary_stage: "RemoteGatewayLatency".to_owned(),
                headline: format!("延迟主因在远端网关 TTFB ({:.1} ms)", wf.remote_ttfb_us as f64 / 1000.0),
                recommendation: "开启 TailCutLayer 对冲 P95 慢节点长尾，对热点只读前缀启用 FoyerLayer 本地混合缓存。".to_owned(),
                findings,
            };
        }

        findings.push("连接复用率、读写放大系数与客户端 CPU/RSS 水位均处于健康区间。".to_owned());
        Self {
            severity: Severity::Healthy,
            primary_stage: "BalancedPipeline".to_owned(),
            headline: "全栈流水线均衡 · 无明显单点阻塞".to_owned(),
            recommendation: "当前并发度与分块参数匹配良好，可直接导出生产 Operator 配置或纳入基线快照。".to_owned(),
            findings,
        }
    }
}

/// 单帧完整遥测快照（可直接用于 TUI/NSH 渲染与 JSON 报告落盘）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetryFrame {
    pub waterfall: StageWaterfall,
    pub handshake: EndpointHandshakeSummary,
    pub verbs: Vec<VerbMetricsRow>,
    pub timeseries: Vec<TimeSeriesPoint>,
    pub transport: TransportAndHostSnapshot,
    pub diagnosis: BottleneckDiagnosis,
    pub await_tree_dump: String,
}
