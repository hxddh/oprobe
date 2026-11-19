# oprobe — Cloud Storage Stack White-Box Microscope & SLA Auto-Tuner

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Pure Rust](https://img.shields.io/badge/100%25-Pure%20Rust-orange.svg)](Cargo.toml)
[![Powered by Apache OpenDAL](https://img.shields.io/badge/Apache%20OpenDAL-0.59-00A3E0.svg)](https://opendal.apache.org/)

`oprobe`（**O**penDAL **Probe**）是基于 [Apache OpenDAL](https://opendal.apache.org/) 构建的生产级**云存储栈白盒调优与故障显微镜**。

不同于只输出平均吞吐的黑盒打流脚本，`oprobe` 在底层 Socket / TLS 握手连接器、HTTP/1.1 & HTTP/2 传输层与 OpenDAL 中间件栈植入**零估算、微秒级真实埋点**——让开发者用**一条命令**看清存储 I/O 慢在哪一层、贵在哪里，自动规避 `429/503` 限流悬崖，并直接产出可上线的 `Operator` 配置与 CI 性能门禁。

---

## 核心特性（v1.0.0）

1. **七段式真实白盒耗时瀑布（零启发式估算）**
   直接在 `hyper` 自定义 `ProbedHttpConnector`、`HyperWhiteboxTransport` 与 OpenDAL `Layer` 边界采样，精确拆解单次存储操作全生命周期：
   - **Stage 1 · 客户端并发排队（`ProbedSemaphore`）**：`ConcurrentLimitLayer` 信号量真实等待耗时；
   - **Stage 2 · 本地混合缓存（`WhiteboxCacheLayer`）**：纯 Rust `moka` W-TinyLFU 全对象/稀疏 Range 块缓存微秒级查找与切片耗时；
   - **Stage 3 · DNS / TCP / TLS 握手与连接池检出（`ProbedHttpConnector`）**：按底层物理连接 `dispatch_seq` 100% 精确区分**冷建连**（独立统计 DNS 解析、TCP SYN/ACK RTT、TLS 1.2/1.3 + ALPN 握手）与**热连接复用 / H2 多路复用**；
   - **Stage 4 · 存储网关首包 TTFB**：从请求帧写入内核 Socket 完成至响应头首字节到达的真实服务端延迟；
   - **Stage 5 · 流式分块传输（`ProbedIncomingStream`）**：响应体分片流传输耗时与 L4 内核级真实收发字节数；
   - **Stage 6 · Multipart 服务端合并提交（`WhiteboxObserveLayer`）**：独立捕获 `CompleteMultipartUpload`（`Writer::close`）服务端合并屏障耗时；
   - **Stage 7 · 端到端数据完整性校验（`CRC32C`）**：Xoshiro256++ 防重删首尾块戳记载荷的读回校验耗时。

2. **慢请求异步调用栈显微镜（`AwaitTree` 10ms 实时采样）**
   后台采样器每 `10ms` 抓取在途协程树，保留 Top-5 最慢请求的完整跨层调用栈（直达 `layer::semaphore_acquire`、`net::tls_handshake`、`http::await_ttfb`、`storage::multipart_complete_close`），并提取网关 `x-amz-request-id` / `cf-ray` / `x-oss-request-id`。

3. **时间窗口流式压测 + 全路径远端测试数据自动清理**
   - 支持**持续时间窗口流式压测**（`--duration 15s --warmup 2s`）与确定性轮次压测（`--rounds 64`）；
   - 四类防重删生产负载：**Parquet 稀疏列点查**、**AI Checkpoint 多分片写入**、**海量前缀元数据风暴（List + Stat）**、**80/20 混合读写**；
   - 每次运行分配独立隔离前缀 `oprobe-run-<ts>-<pid>-<rand>/`，无论正常结束、中途 `Ctrl-C` 还是触发远端报错，均强制执行清理并校验 `0` 残留对象（另附 `oprobe cleanup` 一键清扫子命令）。

4. **读写/RPC 放大精算与五朵云 FinOps 账单对比**
   精确分离缓存未命中的存储合并读放大（`Read Amplification`）、净网络读取比（`Net Wire/Logical Ratio`）与请求裂变放大（`Class A` / `Class B` RPC 放大），横向测算 **AWS S3 Standard、Cloudflare R2、Google GCS、Alibaba Cloud OSS、Backblaze B2** 的月度账单、放大浪费与本地缓存节省金额。

5. **防 `429/503` 闭环自动寻优与 `oprobe diff` CI 回归门禁**
   - `oprobe tune` 自动感知软/硬 `429 TooManyRequests` 与 `503 SlowDown`，触发熔断退避并在限流悬崖下方执行二分探底，直接导出**可回环复用的 TOML Profile（含 `[knobs]`）**、**Rust `Operator` 代码**与 **Python `AsyncOperator` 代码**；
   - `oprobe diff baseline.json candidate.json` 同时守护分动词 P99、总吞吐、读放大、RPC 放大、`429/503` 限流与月度云成本，超阈值时返回退出码 `2` 阻断劣化提交。

---

## 安装指南（100% 纯 Rust · 零外部 C/OpenSSL 依赖）

`oprobe` 全栈采用纯 Rust 实现（`rustls` + `oxitls-rustcrypto-provider` + `hyper` + `moka`），无需安装 `cmake`、`openssl`、`zstd` 等任何 C/C++ 库，支持全平台一键编译安装：

```bash
# 方式 1：通过 Cargo 一键安装（推荐）
cargo install --git https://github.com/hxddh/oprobe.git

# 若已发布至 crates.io：
cargo install oprobe

# 方式 2：前往 GitHub Releases 下载预编译二进制
# 支持 x86_64/aarch64 Linux, Apple Silicon/Intel macOS, x86_64 Windows
```

---

## 一分钟快速上手

### 1. 零配置一条命令直接剖析任意存储后端（`oprobe run`）

```bash
# 本地内存零依赖快速体验（2 秒流式窗口）
oprobe run --url memory:// --duration 2s --workload parquet

# 直连 AWS S3 / MinIO / Ceph RGW / R2（自动读取 AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY）
oprobe run \
  --url s3://my-lakehouse-bucket/bench \
  --endpoint https://s3.ap-northeast-1.amazonaws.com \
  --region ap-northeast-1 \
  --workload parquet \
  --warmup 2s --duration 15s \
  --concurrent 32 --chunk-kib 1024 --gap-kib 256 \
  --json baseline.json --markdown report.md
```

### 2. 防 `429/503` 闭环自动调优并生成可上线配置（`oprobe tune`）

```bash
oprobe tune \
  --profile profiles/s3-prod.toml \
  --emit-profile tuned-s3.toml \
  --emit-rust src/storage_operator.rs \
  --emit-python storage_operator.py \
  --json tuned.json

# 直接回环验证生成的最优配置 Profile：
oprobe run --profile tuned-s3.toml --duration 10s
```

### 3. 启动 20 FPS 实时流式交互终端工作台（`oprobe tui`）

```bash
oprobe tui --profile profiles/s3-prod.toml
```

- **全局快捷键**：
  - `Space`：启动单轮剖析 / 优雅停止（自动触发远端清理）
  - `S`：启动 10 秒持续流式压测窗口（实时刷新瀑布图、Sparkline 与慢协程树）
  - `O`：启动防 `429/503` 自动寻优（实时流式展示每组 Trial 得分）
  - `1` / `2` / `3` / `4`：切换 Parquet / Checkpoint / Metadata / Mixed 四类负载
  - `[` / `]`：下调 / 上调并发数 · `-` / `+`：减半 / 翻倍分块大小
  - `F` / `T` / `K`：实时开关本地缓存、`TailCutLayer` 长尾对冲、HTTP Keep-Alive 连接复用
  - `Tab` / `Shift+Tab`：切换底部五大白盒检查器（`AwaitTree` 慢栈 / 寻优代码 / 五朵云 FinOps / Socket-TLS 握手 / 清理审计与配置）
  - `/` 或 `:`：唤起底部命令栏（支持 `Up`/`Down` 历史记录） · `?`：查看快捷键帮助 · `E`：一键导出全套报告 · `Q`：退出

### 4. 独立微秒级冷/热握手与协议显微镜（`oprobe net-probe`）

```bash
oprobe net-probe https://s3.ap-northeast-1.amazonaws.com --samples 5
```

独立输出 DNS 解析 P50/P99、TCP SYN/ACK RTT、TLS 1.3 握手耗时、ALPN 协商结果（`h2` vs `http/1.1`），并在同一条连接上对比 **Cold HEAD 全链路耗时** 与 **Warm Keep-Alive 复用 TTFB**。

### 5. 本地内置 TLS 1.3 + `503 SlowDown` 混沌对象存储实验台（`oprobe serve-s3`）

无需云账号即可在本地复现弱网高延迟、分片合并慢、长尾抖动与 `503 SlowDown` 分区限流：

```bash
# 终端 1：启动带 TLS 1.3 + H2、15ms TTFB、每 5 次请求 120ms 长尾抖动、并发 >8 触发 503 SlowDown 的 S3 实验台
oprobe serve-s3 --bind 127.0.0.1:9000 --tls --ttfb-ms 15 --straggler-every 5 --straggler-ms 120 --max-inflight-503 8 --write-ca-cert /tmp/oprobe-ca.pem

# 终端 2：运行自动寻优器，观察其自动识别 503 悬崖并收敛至安全最优并发
AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test \
  oprobe tune --url s3://chaos-bucket/lab --endpoint https://localhost:9000 --ca-cert /tmp/oprobe-ca.pem
```

---

## CI/CD 性能与成本回归门禁集成示例（GitHub Actions）

```yaml
- name: Run oprobe white-box storage benchmark
  run: |
    oprobe run --profile profiles/s3-prod.toml --duration 15s --warmup 2s --json candidate.json --quiet
    oprobe diff benchmarks/s3-baseline.json candidate.json \
      --max-p99-regression-pct 15.0 \
      --min-throughput-ratio 0.90 \
      --max-read-amp-delta 0.20 \
      --max-cost-increase-pct 15.0 \
      --markdown pr-storage-diff.md
```

当候选版本出现分动词 P99 劣化、吞吐跌落、读/RPC 放大飙升、新增 `429/503` 限流或 CRC32C 校验失败时，`oprobe diff` 将输出详细归因报告并以退出码 `2` 阻断流水线。

---

## 预置配置模板（`profiles/`）

| 模板路径 | 适用场景 |
| :--- | :--- |
| `profiles/memory-quickstart.toml` | 零外部依赖内存沙盒快速体验与 CI 冒烟测试 |
| `profiles/s3-prod.toml` | AWS S3 Standard 数据湖 Parquet 稀疏列点查（流式窗口 + 本地缓存） |
| `profiles/r2-zero-egress.toml` | Cloudflare R2 零流出费 AI 大模型 Checkpoint 多分片并发写入 |
| `profiles/gcs-lakehouse.toml` | Google Cloud Storage 海量分区元数据扫描（List + Stat） |
| `profiles/oss-ai-checkpoint.toml` | 阿里云 OSS 大对象分片流式上传与合并剖析 |
| `profiles/b2-archive.toml` | Backblaze B2 S3 兼容接口 80/20 混合读写压测 |

---

## 许可证

Apache License 2.0
