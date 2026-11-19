# Changelog

## v0.1.0 — First Complete Release

`oprobe`（**O**penDAL **Probe**）是基于 [Apache OpenDAL](https://opendal.apache.org/)（`0.59`）构建的存储栈白盒调优、底层握手探针、多云成本精算与 SLA 自动寻优工具，同时提供零依赖开箱即用的 Ratatui 终端工作台（`oprobe tui`）、完整 CI 命令行套件（`run` / `tune` / `diff` / `net-probe`）以及 [Native Shell (NSH)](https://github.com/longbridge/nsh) 双端桌面扩展壳层（`native/`）。

### Core Capabilities

- **微秒级底层 Socket + TLS 1.3 握手探针（`oprobe net-probe`）**：独立测量 DNS 解析、TCP SYN/ACK RTT、TLS 握手耗时、协商 ALPN 协议（`h2` / `http/1.1`）与密码套件，校准传输层冷热连接拆分基准。
- **双切面六段式耗时归因（`Service` + `HttpTransport::fetch`）**：精确拆分信号量排队、[`FoyerLayer`](https://docs.rs/opendal-layer-foyer) 本地混合缓存、连接复用/TLS、远端网关 TTFB、Body 分块流与 Multipart 合并/CRC32C 六个阶段，并挂载 [`AwaitTreeLayer`](https://opendal.apache.org/docs/rust/opendal_layer_await_tree/struct.AwaitTreeLayer.html) 导出在途慢协程调用树。
- **四类防重删生产负载与端到端校验**：内置 Xoshiro256++ 首尾防重删块戳记与 CRC32C 校验，覆盖 Parquet 稀疏列点查（精确计算 Gap 空洞读放大与 RPC 裂变）、AI Safetensors 分片写入、海量前缀元数据风暴及 80/20 混合读写。
- **多云 FinOps 成本精算与两阶段防 `503` 自动寻优（`oprobe tune`）**：内置 AWS S3 Standard、Cloudflare R2、Backblaze B2 计费模型，自动量化读写放大造成的月度账单损耗，并一键导出 Rust / Python / TOML 生产配置。
- **快照持久化与 CI 回归门禁（`oprobe diff`）**：支持导出 `.json` / `.md` / `.csv` 完整报告，对比基线与候选快照的分动词 P99、吞吐与读放大，超阈值时返回退出码 `2` 阻断退化变更。
