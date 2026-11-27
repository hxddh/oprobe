<p align="center"><img src="packaging/desktop/oprobe.svg" width="112" alt="oprobe"></p>

# oprobe

> **oprobe 是 OpenDAL operator 的性能剖析器（profiler）。**
> 它告诉你 `Operator` 在网络上实际做了什么，指出能修好它的那一层（layer），并在你上线之前证明这个修复有效。

程序有 `perf` 和火焰图；[Apache OpenDAL](https://opendal.apache.org/) 的 operator 有 oprobe。它记录每个操作之下的每一个后端请求，并按阶段拆开（排队、建连、发送、首字节、传输、重试退避），把发现解释为**一个 OpenDAL 改动**，再用交错 A/B 测试在你关心的目标上证明这个改动。

[English](README.md) · [快速开始](docs/getting-started.md) · [概念](docs/concepts.md) ·
[op.toml](docs/operator.md) · [命令行](docs/cli.md) · [读懂 profile](docs/reading-the-profile.md) ·
[嵌入](docs/embedding.md) · [应用](docs/app.md) · [安装](docs/install.md) ·
[排错](docs/troubleshooting.md)（文档为英文）

## 闭环：Profile → Explain → Prove

1. **Profile（剖析）。** 让一个工作负载流经某个 operator（`oprobe run s3://bucket/prefix`），或者打开 [`oprobe-trace`](docs/embedding.md) 在生产环境写下的 `.oprobe` 文件。
2. **Explain（解释）。** 每条发现都给出原因、作为证据的请求本身，以及**唯一一个** OpenDAL 补救措施。它不给数字：单次运行没有改变负载，无法为改动定量。
3. **Prove（证明）。** `oprobe prove` 交错运行当前 operator 与加上补救措施后的 operator（A B A B …），在补救措施有大小时扫描这一个维度，并按你的目标（默认 goodput，或 p99）做出判断。结果是一份可以直接复制的**已证明的 layer diff**。

## 60 秒上手

```bash
# 安装（macOS / Linux；Windows、软件包与 cargo 见 docs/install.md）
curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh

oprobe demo                      # 剖析本机上一个“行为不良”的 S3 实验室，无需任何账号
oprobe prove last                # 对首要发现的补救措施做 A/B 测试（约一分钟）
oprobe                           # 应用：运行记录、Profile、Operator、对比
```

`oprobe demo` 即 `oprobe run lab://demo`：一个本地 S3，让每第 40 个请求变成等待 250 ms 的拖尾（并在超过 40 个并发请求时限流）。简报最多列出三条发现，每条附上以 `op.toml` 路径表示的补救措施和下一条命令。`prove last` 交替运行当前 operator（A）与补救后的 operator（B）的试验，最后给出 diff（这次用时 54 秒）：

```text
improved on goodput: +33.3 %, 95 % interval [+26.9, +40.1] (3 pairs of trials, noise ±2.0 %) · lab://demo (local lab)
secondary (not decisive; 95 % intervals): p99 -68.5 % [-70.0, -66.9] · throttled 0.00 % → 0.00 % · errors 0.00 % → 0.00 %
note: TailCutLayer re-issues through RetryLayer: its first backoff is shortened so a cut request is retried at once; trials warm up 11.5 s because the layer ignores its first 10 s
+ TailCutLayer::builder().safety_factor(1.5).min_samples(32).min_deadline(Duration::from_millis(50)).max_deadline(Duration::from_secs(60)).build()
~ RetryLayer::new().with_min_delay(Duration::from_millis(20))
op.toml:
[layers]
retry = { min_delay = "20ms" }
tail_cut = true
```

然后换成你自己的存储：

```bash
oprobe run s3://my-bucket/oprobe --set options.region=eu-west-1
oprobe run op.toml --preset range-reads           # 你的 operator，类 parquet 的读取负载
oprobe run s3://a/x r2://b/x fs:///nvme/x         # 多个 operator 跑同一负载：一场比赛
```

写入都落在全新的 `oprobe-run-<id>/` 前缀下并会被清理；`--dry-run` 打印计划，`--read-only` 拒绝任何写入，`--max-requests/--max-bytes/--max-duration` 是硬性预算。

## 你的 operator：`op.toml`

一切的单位都是 OpenDAL 的 `Operator`：一个服务、一组 layer、以及读写选项。oprobe 从 URL 或文件 `op.toml`（推荐的描述方式）读取它。

```toml
url = "s3://bucket/prefix"
[options]                       # OpenDAL 服务选项
region = "eu-west-1"
[layers]                        # 每个键对应一个 OpenDAL layer；不写即不在栈中
concurrent_limit = { http = 24 }
retry = { max_times = 3, jitter = true }
timeout = { op = "30s", io = "10s" }
[read]
chunk = "8MiB"
concurrent = 4
[http]
version = "auto"                # auto | h1 | h2
```

`--set layers.retry.max_times=5` 或 `--set read.gap=1MiB` 修改单个值。两个 operator 之间的差异会渲染成 Rust 和 TOML。[参考 →](docs/operator.md)

## 发现与补救措施

| 发现 | 原因 | 补救措施（`op.toml` 路径） |
|---|---|---|
| `throttling` | 并发超过某个水平后出现 429/503 | `layers.concurrent_limit.http`，并开启 `layers.retry.jitter` |
| `rate_limit` | 请求速率上限 | `layers.concurrent_limit.http` |
| `key_limit` | 热点前缀或热点 key | key 的布局（建议，而非 layer） |
| `bandwidth_cap` | 带宽上限 | 上行受限（写入）：`layers.throttle.bandwidth`；下行受限：不是 layer |
| `tail_stragglers` | 少量极慢的服务端响应 | `layers.tail_cut` |
| `hung_requests` | 永不结束的请求 | `layers.timeout.io` |
| `small_ranges` | 大量小范围读取 | `read.gap` / `read.concurrent` |
| `slow_multipart` | 分段上传很慢 | `write.chunk` / `write.concurrent` |
| `repeated_reads` | 同样的字节被反复读取 | `layers.cache`（`FoyerLayer`） |
| `cold_connections` | 握手落在关键路径上、HTTP 版本 | `http.keep_alive` / `http.version` |
| `client_bound` | 瓶颈在你的进程 | 存储侧无补救，如实报告 |
| `errors` | 重试后恢复的 5xx 或断开的连接 | `layers.retry.min_delay`，加 `layers.retry.jitter` |
| `errors` | 应用侧看到的失败 | 无：修复其原因（`show RUN --by api`） |

`run` 只给出补救措施；只有 `prove` 会给出数字。

## 剖析生产环境：`oprobe-trace`

给你自己的 operator 加一层，保存一份 profile，工作台会像打开普通运行一样打开它：

```toml
[dependencies]
oprobe-trace = { git = "https://github.com/hxddh/oprobe", tag = "v9.3.4" }   # 尚未发布到 crates.io
```

```rust
let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = op.layer(layer);
handle.save("prod.oprobe")?;          // 之后：oprobe show prod.oprobe
```

`oprobe-trace` 是一个 OpenDAL layer 加上一个带观测的 `HttpFetch`（就是 OpenDAL 自己的 reqwest 客户端）。内存有上界，开销有基准测试（预算 ≤ 每个操作 2 µs），`Exporter` 钩子能看到每个请求（`otel` 特性提供 OpenTelemetry 指标）。[嵌入 →](docs/embedding.md)

## 应用

不带参数运行 `oprobe`：有显示器时是原生窗口，否则在终端中打开同一个应用（SSH 下也可用）。四个视图：**运行记录**、**Profile**（结论、时间去向、最慢操作的请求瀑布图、时间线、热力图）、**Operator**（你的 layer 栈与每条补救措施并列：切换一个 layer，点 *Prove*，看 A/B 对实时流入，复制 diff）以及**对比**。界面支持简体中文和英文，跟随系统语言，也可在应用内切换。[更多 →](docs/app.md)

## 脚本与智能体

每个命令都可以用 `-o json` 输出一个 JSON 文档（stdout 只有结果，进度写到 stderr）；错误带有稳定的错误码和退出码（0 成功 · 1 运行时 · 2 用法 · 3 prove 结果为 worse · 4 预算 · 5 安全 · 130 中断）；JSON Schema 在 [`schemas/`](schemas)。命令行与 JSON 输出只有英文。智能体请看 [`skills/oprobe/SKILL.md`](skills/oprobe/SKILL.md) 和 [llms.txt](llms.txt)。

CI 中支持两种用法：

```sh
# Profile 为 critical 时让任务失败。
oprobe run s3://bucket/bench --existing data/ --max-requests 20000 -o json \
  | jq -e '.runs[0].report.verdict.level != "critical"'
# 改动让 operator 变差时让任务失败：prove 以 3 退出。
oprobe prove last --with layers.retry.max_times=5 --max-duration 5m -o json > prove.json
```

## 安装

| 平台 | |
|---|---|
| macOS / Linux | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh`（`-s -- --cli` 只装命令行，不含应用） |
| Windows | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex` 或 `.msi` |
| Debian / Ubuntu、Fedora / RHEL | 发布页上的 `.deb` / `.rpm` |
| Rust（从源码） | `cargo install --locked --git https://github.com/hxddh/oprobe --tag v9.3.4 oprobe`（`--no-default-features --features cli` 只装命令行；Linux 上 `--features gui` 启用原生窗口） |

macOS 和 Windows 安装包**没有代码签名**：首次打开需要确认一次（[方法](docs/install.md#first-open-of-unsigned-packages)）。每个文件在 `SHA256SUMS` 中都有 SHA-256，并带有 SLSA 构建来源证明；安装脚本会校验 SHA-256，并在 GitHub CLI 已登录时校验来源证明（[校验方法](docs/install.md#verifying-a-download)）。

oprobe 不发送任何遥测，只与你指定的存储通信。[隐私 →](docs/privacy.md)

## 许可证

Apache-2.0
