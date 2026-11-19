# oprobe — Apache OpenDAL Storage Stack Profiler & Auto-Tuner

`oprobe`（**O**penDAL **Probe**）是基于 [Apache OpenDAL](https://opendal.apache.org/)（`0.59`）构建的**存储栈白盒调优与故障显微镜**。它直击黑盒压测工具“只报平均吞吐、掩盖长尾根因与账单放大”的痛点，提供**开箱即用的终端交互工作台（TUI）**、**CI 性能回归门禁 CLI** 以及 **[Native Shell (NSH)](https://github.com/longbridge/nsh) 桌面 GPU / 原生双端壳层**。

## 架构与核心特性

1. **六段式全链路白盒耗时瀑布 + 微秒级 DNS/TCP/TLS 硬探针**
   通过 `tokio::net` + `tokio-rustls` 底层连接探针校准 [`OperationContext::with_http_transport`](https://docs.rs/opendal-core)，将单次对象存储操作拆解为：**并发信号量排队 → Foyer 本地混合缓存 → DNS/TCP/TLS 握手与连接复用 → 远端网关 TTFB → Body 分块流 → Multipart 合并与 CRC32C 校验**。
2. **分操作语义独立 HDR 分位谱与 `AwaitTree` 慢协程栈**
   严格隔离 `stat`、`reader::fetch`、`writer::multipart`、`list` 的 `hdrhistogram`（P50 / P90 / P95 / P99 / P99.9），实时抓取慢请求异步调用树与服务端 `x-amz-request-id` / `cf-ray`。
3. **四类防重删真实负载 + 压测机 CPU/RSS 自证**
   内置 Xoshiro256++ 不可压缩载荷与偏移戳记、CRC32C 读回一致性校验，覆盖 **Parquet 稀疏列点查**、**AI 大模型分片写入**、**海量前缀元数据风暴**、**80/20 混合读写**，实时监控客户端 `MiB/s/Core` 与 RSS 水位。
4. **两阶段防 `503 SlowDown` 自动寻优 + 多云 FinOps 账单精算**
   自动扫描 `concurrent × chunk × gap × Foyer × TailCut` 帕累托前沿，同步测算 AWS S3 Standard、Cloudflare R2、Backblaze B2 月度 API 与流出损耗，直接生成 Rust / Python `Operator` 生产代码。
5. **快照导出与 `oprobe diff` CI 回归门禁**
   支持导出 JSON 基线、Markdown 架构诊断报告与 CSV 试验矩阵，通过 `oprobe diff baseline.json candidate.json` 在流水线中自动拦截 P99 退化与读放大膨胀。

## 安装与快速上手

### 1. 源码构建或下载预编译包

前往 [GitHub Releases (`hxddh/oprobe`)](https://github.com/hxddh/oprobe/releases) 下载 Linux / macOS / Windows 预编译二进制，或直接通过 Cargo 构建（纯 Rust `rustls`，零外部 C 依赖）：

```bash
git clone https://github.com/hxddh/oprobe.git
cd oprobe
cargo build --release
```

### 2. 五大运行模式

```bash
# 模式 A：启动交互式终端工作台（默认零配置内存沙盒，支持 `/` 命令栏动态切 S3/FS）
./target/release/oprobe tui --profile profiles/memory-quickstart.toml

# 模式 B：单点确定性白盒剖析并导出 JSON 基线 + Markdown 诊断报告 + CSV
./target/release/oprobe run --profile profiles/s3-prod.toml \
  --json baseline.json --markdown report.md --csv metrics.csv

# 模式 C：两阶段防 503 自动参数寻优并导出生产级 Rust / Python Operator 代码
./target/release/oprobe tune --profile profiles/s3-prod.toml \
  --json tuned.json --emit-rust tuned_operator.rs --emit-python tuned_operator.py

# 模式 D：CI 性能回归对比门禁（P99 退化超过 15% 时返回退出码 2）
./target/release/oprobe diff baseline.json tuned.json --max-p99-regression-pct 15.0

# 模式 E：独立微秒级 DNS + TCP + TLS 1.3 + ALPN 端点握手探针
./target/release/oprobe net-probe https://s3.ap-northeast-1.amazonaws.com --samples 5
```

### 3. Native Shell (NSH) 桌面 GPU / 双端运行（可选）

仓库根目录包含完整配置的 `nsh.json` 与 `native/` 子工程，安装 [NSH CLI](https://github.com/longbridge/nsh) 后可直接以桌面 GPU 窗口或原生窗口启动：

```bash
nsh run --target gpui
# 或使用系统原生后端 (macOS AppKit / Windows Direct2D / Linux GTK 4)
nsh run
```

## 交互式工作台快捷键与 `/` 命令栏

- **全局快捷键**：`Space` 单点剖析/急停 · `O` 自动寻优 · `1-4` 切换四类负载 · `Tab` 切换底部四页签 · `[` / `]` 调整并发 · `F` / `T` / `K` 开关 Foyer / TailCut / KeepAlive · `E` 一键导出三格式报告 · `/` 或 `:` 唤起命令栏 · `Q` 退出。
- **底部命令栏示例**：
  - `:scheme fs` / `:set root=/tmp/bench`
  - `:scheme s3` / `:set bucket=my-lakehouse` / `:set endpoint=https://s3.amazonaws.com`
  - `:conc 32` / `:chunk 4096` / `:gap 256`
  - `:net https://s3.amazonaws.com`
  - `:export prod_snapshot`

## 许可证

Apache License 2.0
