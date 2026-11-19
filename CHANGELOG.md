# Changelog

## v1.0.0 — Production White-Box Cloud Storage Microscope & Closed-Loop Auto-Tuner

`v1.0.0` 将 `oprobe` 全面重构并升级为 **100% 纯 Rust、零启发式估算**的生产级云存储栈白盒剖析与闭环调优套件。

### Highlights & Breaking Upgrades

- **100% 纯 Rust 架构与零 C/CMake 依赖跨平台构建**
  - 移除所有外部 C/C++ 编译依赖（彻底剔除 `aws-lc-sys`、`zstd-sys`、`lz4-sys`、`ring` 及原型壳层），全面采用 `hyper` 1.x + `tokio-rustls` + `oxitls-rustcrypto-provider` + `moka` 纯 Rust 技术栈，原生支持 `cargo install oprobe` 与 Linux (`x86_64`/`aarch64`)、macOS (`aarch64`/`x86_64`)、Windows 全平台分发。
- **七段式真实底层白盒可观测性（彻底清除估算逻辑）**
  - 自研 `ProbedHttpConnector` 与 `HyperWhiteboxTransport`：基于物理 Socket `dispatch_seq` 与内核级读写时间戳，精确拆解 **DNS 解析、TCP SYN/ACK 握手、TLS 1.2/1.3 + ALPN 协商、连接池热复用 vs 冷建连、网关首包 TTFB、Body 分块流与 L4 真实收发字节**。
  - 自研 OpenDAL 中间件埋点栈：`ProbedSemaphore`（真实 `ConcurrentLimitLayer` 排队耗时）、`WhiteboxCacheLayer`（纯 Rust `moka` W-TinyLFU 全对象与稀疏 Range 块缓存真实命中/微秒耗时）及 `WhiteboxObserveLayer`（真实 `CompleteMultipartUpload` 分片合并耗时与 `RetryEvent.retry_after` 精确退避统计）。
  - 慢请求异步调用栈实时捕获：后台 `10ms` `AwaitTree` 采样器持续追踪在途协程并保留 Top-5 最慢请求的完整跨层调用栈。
- **闭环流式压测、全路径自动清理与五朵云 FinOps 诊断**
  - 新增 `--duration` / `--warmup` 持续时间窗口流式压测模式与 `--url` 零配置文件直连模式。
  - 新增隔离前缀 `oprobe-run-*` 全路径远端数据自动清理与校验（覆盖正常结束、报错中止及 `Ctrl-C` 信号），并提供 `oprobe cleanup` 独立清扫子命令。
  - 升级多云成本引擎：覆盖 **AWS S3 Standard、Cloudflare R2、Google GCS、Alibaba Cloud OSS、Backblaze B2**，精确量化 Class A/B 请求放大损耗、读放大流出浪费与本地缓存月度节省金额。
  - 升级防 `429/503` 闭环自动寻优器（`oprobe tune`）：支持软/硬限流熔断退避、悬崖下方二分并发探底，产出可 `oprobe run --profile` 直接回环复用的 `[knobs]` TOML 配置与 Rust / Python 生产代码。
- **产品化交互体验与内置 S3 混沌验证台**
  - `oprobe tui` 支持 `100ms` 实时流式动态刷新、流式寻优矩阵、命令历史与 `?` 快捷键浮层。
  - 新增结构化运行时异常诊断提示（自动识别 TLS 证书、401/403 签名/权限、404 桶缺失、429/503 限流与 DNS 故障并给出修复步骤）。
  - 内置 `oprobe serve-s3` 兼容对象存储与弱网/`503 SlowDown` 故障注入实验台。

## v0.1.0 — Initial Prototype Release

- 基于 Apache OpenDAL `0.59` 的初版 CLI（`run` / `tune` / `diff` / `net-probe`）与终端 TUI 原型。
