//! `oprobe` — Apache OpenDAL 存储栈白盒调优与故障显微镜核心库
//!
//! 核心能力域：
//! - [`net_probe`]：微秒级 DNS 解析、TCP 三次握手、TLS 1.3 握手与 ALPN 协议协商底层探针；
//! - [`telemetry`]：OpenDAL `Service` 语义层 + `HttpTransport` 传输层双切面六段式耗时瀑布、分动词独立 HDR 分位谱与 `AwaitTree` 慢协程栈；
//! - [`workload`]：Xoshiro256++ 防重删数据载荷、CRC32C 端到端一致性校验及四大标准负载（Parquet 稀疏点查、AI 分片写入、元数据风暴、混合读写）；
//! - [`cost`]：AWS S3 Standard、Cloudflare R2、Backblaze B2 请求放大与读放大月度 TCO 精算模型；
//! - [`autotune`]：带 `503 SlowDown` 熔断保护的两阶段坐标下降参数寻优器，一键导出 Rust / Python / TOML 生产配置；
//! - [`report`]：JSON 快照持久化、Markdown 诊断报告、CSV 矩阵导出与 `oprobe diff` CI 性能回归门禁；
//! - [`tui`]：开箱即用的终端交互式工作台（内置 `/` Minibuffer 命令解析器，无需额外外部运行时）。

pub mod autotune;
pub mod cli;
pub mod cost;
pub mod net_probe;
pub mod report;
pub mod telemetry;
pub mod tui;
pub mod workload;
