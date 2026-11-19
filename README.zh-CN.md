# oprobe

**一条命令看清对象存储 I/O 慢在哪一层、贵在哪里——然后直接修好它。**

`oprobe` 基于 [Apache OpenDAL](https://opendal.apache.org/)，对 S3、GCS、Azure Blob、R2、OSS、COS、B2、
MinIO 及本地存储施加真实业务负载，并在传输层追踪**每一个后端请求**：排队等待、新建连接时的
DNS / TCP / 代理 CONNECT / TLS、请求发送、首字节等待（TTFB）、响应体传输，以及数据等待应用消费的时间。
随后告诉你时间和钱花在哪里，在不触发 429/503 限流的前提下找出最优客户端配置，并把这一切变成 CI 回归门禁。

[English](README.md) · [指标与方法](docs/metrics.md) · [负载与 Profile](docs/workloads.md) · [调优](docs/tuning.md) · [CI 门禁](docs/ci.md) · [成本模型](docs/cost.md) · [故障排查](docs/troubleshooting.md) · [安装](docs/install.md)

## 它回答的问题

| 问题 | oprobe 如何回答（全部来自实测，不做估算） |
|---|---|
| 慢在网络、服务端还是客户端？ | 基于真实请求时间线，把每个操作的耗时精确拆分为 **排队 / 建连 / 上传 / 服务端 TTFB / 下载 / 客户端** |
| 为什么 p99 远差于 p50？ | 保留最慢操作的**全部后端请求分段耗时、冷/热连接、状态码、请求 ID**，以及卡住时采样的**异步调用栈**（含 OpenDAL 并发子任务） |
| 连接复用了吗？TLS 或代理慢吗？ | 每次建连的 DNS / TCP / 代理 CONNECT / TLS 直方图，ALPN、TLS 版本、加密套件、复用率、峰值在途请求 |
| 是否为没用到的字节付费？ | 逻辑字节 vs 后端字节 vs 线上字节：**读放大、range 合并导致的过读、每操作请求数** |
| 每月会花多少钱？ | 实测请求构成 × 各云厂商价格（S3、GCS、Azure、R2、B2、OSS），并量化失败/限流请求与过读的浪费、缓存节省 |
| 上线该用什么配置？ | `oprobe tune` 真实试验、**识别 429/503 限流悬崖**并退避，细调 chunk / gap / 并行度 / 缓存 / HTTP 版本，输出 Profile 与 Rust/Python OpenDAL 代码 |
| 这次改动让存储 I/O 变差了吗？ | `oprobe diff` 带噪声阈值对比 JSON 报告并以非零码退出；输出 PR 评论用 Markdown 与 CI 用 JUnit |

## 安装

| 平台 | 方式 |
|---|---|
| 任意平台（Rust ≥ 1.91） | `cargo install oprobe --locked` |
| macOS / Linux 脚本 | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` |
| macOS 安装包 | `oprobe-<版本>-macos-universal.pkg`（Apple Silicon + Intel 通用） |
| Debian / Ubuntu | `sudo apt install ./oprobe_<版本>-1_amd64.deb`（或 `_arm64`） |
| Fedora / RHEL / SUSE | `sudo dnf install ./oprobe-<版本>-1.x86_64.rpm`（或 `.aarch64`） |
| Windows 安装器 | `oprobe-<版本>-x64.msi` / `-arm64.msi`，自动加入 `PATH` |
| Windows Scoop | `scoop install https://github.com/hxddh/oprobe/releases/latest/download/oprobe.json` |
| Homebrew | 每个 Release 附带 `oprobe.rb`，见 [docs/install.md](docs/install.md) |

纯 Rust 实现，无 OpenSSL、无 C 工具链依赖；Linux musl 版本为完全静态链接。压缩包内含 man 手册、
bash/zsh/fish/PowerShell/elvish 补全脚本与示例 Profile，所有产物的校验和见 `SHA256SUMS`。

## 快速开始

```bash
# 1. 零依赖体验：内存后端
oprobe run memory://

# 2. 在本地复现存储问题：带延迟与限流的 S3 兼容实验台
oprobe lab --latency 20ms --straggler-every 50 --throttle-above 16 &
AWS_ACCESS_KEY_ID=lab AWS_SECRET_ACCESS_KEY=lab \
  oprobe run s3://bench/demo --endpoint http://127.0.0.1:9000 --proxy none -c 32

# 3. 你的存储桶（凭据沿用 AWS/GCP/Azure 常规环境变量）
oprobe doctor s3://my-bucket/oprobe            # 连通性、TLS、时钟偏差、读写删权限逐项检查
oprobe run    s3://my-bucket/oprobe --preset parquet -d 30s --json baseline.json
oprobe run    s3://my-bucket --existing warehouse/events/   # 只读压测你已有的真实对象
oprobe top    s3://my-bucket/oprobe            # 实时终端工作台

# 4. 调优并落地
oprobe tune s3://my-bucket/oprobe --preset parquet --p99-ms 250 \
  --emit-profile tuned.toml --emit-rust src/storage.rs --emit-python storage.py

# 5. CI 回归门禁
oprobe run --profile tuned.toml --json candidate.json
oprobe diff baseline.json candidate.json --junit oprobe.xml --markdown oprobe.md
```

所有测试数据写入唯一的 `oprobe-run-<时间戳>-<id>/` 前缀，运行结束（包括报错与 Ctrl-C）后自动删除并
重新列举校验为空；被强制终止的运行可用 `oprobe cleanup` 清扫。

## 命令一览

| 命令 | 作用 |
|---|---|
| `oprobe run` | 运行负载并输出白盒报告；`--json` / `--markdown` 导出；`--fail-on` 用于 CI |
| `oprobe top` | 交互式终端工作台：实时吞吐/延迟、耗时分解、请求分段、连接、最慢操作与诊断；单键调整参数，`t` 一键调优 |
| `oprobe tune` | 感知限流的闭环调优，`--objective throughput\|latency\|cost` |
| `oprobe diff` | 回归门禁（回归时退出码 3），输出 JSON / Markdown / JUnit |
| `oprobe doctor` | 配置、凭据、DNS/代理/TCP/TLS、HTTP、时钟偏差、list/write/read/stat/delete 逐项诊断 |
| `oprobe probe` | 握手显微镜：DNS、TCP、代理 CONNECT、TLS、冷/热连接 TTFB、时钟偏差 |
| `oprobe lab` | 本地 S3 兼容服务（HTTP/1.1、HTTP/2、TLS 1.3），可注入延迟、长尾、带宽限制、慢合并与 429/503 |
| `oprobe cleanup` | 清理中断运行遗留的测试数据（`--dry-run`、`--older-than`、`--datasets`） |
| `oprobe init` | 输出带完整注释的 Profile 模板 |

## 负载模型

五种预设覆盖典型生产访问模式，均可通过操作配比、数据集形态、键热度分布（`uniform`、`zipf:1.1`、
`hotset:0.2/0.8`）、闭环并发或开环到达速率（延迟从“计划开始时间”计算，避免协同遗漏）定制：

| 预设 | 场景 |
|---|---|
| `parquet` | 湖仓扫描：stat + footer + 带 gap 合并的列块向量化范围读 |
| `checkpoint` | AI 训练 Checkpoint：大对象分片上传 + 全量恢复读取 |
| `small-files` | 海量小文件：整对象 GET/PUT 与 HEAD |
| `metadata` | 分区发现：LIST + HEAD 风暴 |
| `mixed` | 通用：70% 读（范围+整读）/ 20% 写 / 10% stat |

## 许可证

Apache-2.0。oprobe 基于 Apache OpenDAL 构建，与 Apache 软件基金会无隶属关系。
