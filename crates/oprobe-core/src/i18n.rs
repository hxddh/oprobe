//! The message catalog (design v5 §6 "i18n"): English and 简体中文 for the text brief, the
//! findings' cause and advice, errors and hints, shared by the CLI and the App.
//!
//! - The language comes from `--lang` (or `OPROBE_LANG`), else `LC_ALL` / `LC_MESSAGES` /
//!   `LANG` ([`detect`]); the App also asks the OS ([`detect_os`]).
//! - Machine output (JSON, NDJSON, schemas, MCP) is always English: only text renderers call
//!   [`t`] / [`tf`].
//! - A key missing from a language falls back to English; a key missing everywhere renders as
//!   the key itself, so a typo is visible instead of silent. A unit test checks that both
//!   tables have the same keys and the same `{placeholders}`.
//!
//! ```
//! use oprobe_core::i18n::{Lang, tf_in};
//! assert_eq!(tf_in(Lang::En, "brief.more_findings", &[("n", &4), ("cmd", &"oprobe explain last")]),
//!            "… 4 more findings: oprobe explain last");
//! ```

use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};

/// A language of the catalog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Lang {
    #[default]
    En,
    /// 简体中文.
    ZhCn,
}

impl Lang {
    /// Every language, English first.
    pub const ALL: [Lang; 2] = [Lang::En, Lang::ZhCn];

    /// A language from a tag or locale: `en`, `en_US.UTF-8`, `zh-CN`, `zh_CN.UTF-8`, `zh-Hans`,
    /// `zh`. Traditional Chinese locales (`zh_TW`, `zh-Hant`) are not covered and give `None`.
    pub fn parse(s: &str) -> Option<Lang> {
        let s = s.trim();
        let base = s
            .split(['.', '@'])
            .next()
            .unwrap_or("")
            .replace('_', "-")
            .to_ascii_lowercase();
        if base.is_empty() {
            return None;
        }
        if base == "c" || base == "posix" || base == "en" || base.starts_with("en-") {
            return Some(Lang::En);
        }
        if base == "zh"
            || base == "zh-cn"
            || base == "zh-sg"
            || base.starts_with("zh-hans")
            || base == "chinese"
        {
            return Some(Lang::ZhCn);
        }
        None
    }

    /// BCP 47 tag (`en`, `zh-CN`).
    pub fn tag(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhCn => "zh-CN",
        }
    }

    fn table(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Lang::En => EN,
            Lang::ZhCn => ZH_CN,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Set the process language (the CLI does this once, from [`detect`]).
pub fn set_lang(l: Lang) {
    CURRENT.store(l as u8, Ordering::Relaxed);
}

/// The process language (English until [`set_lang`]).
pub fn lang() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::ZhCn,
        _ => Lang::En,
    }
}

/// The language from `flag` (`--lang`; `auto` or `None` means the environment), then
/// `LC_ALL`, `LC_MESSAGES`, `LANG` (POSIX precedence). Unknown values mean English.
pub fn detect(flag: Option<&str>) -> Lang {
    detect_from(flag, |k| std::env::var(k).ok())
}

/// [`detect`] with an injectable environment (tests).
pub fn detect_from(flag: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Lang {
    if let Some(f) = flag.filter(|f| !f.eq_ignore_ascii_case("auto")) {
        return Lang::parse(f).unwrap_or_default();
    }
    for k in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = env(k).filter(|v| !v.trim().is_empty()) {
            // The first variable that is set decides, even when it names another language.
            return Lang::parse(&v).unwrap_or_default();
        }
    }
    Lang::En
}

/// The language for the App: [`detect`], then the OS user locale (macOS `AppleLocale`)
/// when no locale variable is set (apps opened from Finder or a launcher get none).
pub fn detect_os(flag: Option<&str>) -> Lang {
    let has_env = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
    if flag.is_some_and(|f| !f.eq_ignore_ascii_case("auto")) || has_env {
        return detect(flag);
    }
    os_locale()
        .and_then(|l| Lang::parse(&l))
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn os_locale() -> Option<String> {
    let o = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

#[cfg(not(target_os = "macos"))]
fn os_locale() -> Option<String> {
    None
}

/// The message `key` in the process language.
pub fn t(key: &'static str) -> &'static str {
    t_in(lang(), key)
}

/// The message `key` in `l` (English, then the key itself, when missing).
pub fn t_in(l: Lang, key: &'static str) -> &'static str {
    lookup(l, key)
        .or_else(|| lookup(Lang::En, key))
        .unwrap_or(key)
}

/// `key` in `l`, if that table has it (no fallback).
pub fn lookup(l: Lang, key: &str) -> Option<&'static str> {
    l.table().iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// The message `key` with `{name}` placeholders filled, in the process language.
pub fn tf(key: &'static str, args: &[(&str, &dyn Display)]) -> String {
    tf_in(lang(), key, args)
}

/// [`tf`] in `l`.
pub fn tf_in(l: Lang, key: &'static str, args: &[(&str, &dyn Display)]) -> String {
    let mut s = t_in(l, key).to_owned();
    for (name, v) in args {
        s = s.replace(&format!("{{{name}}}"), &v.to_string());
    }
    s
}

/// A finding's cause and advice in the process language: `Some((cause, advice))` for a known
/// code in a language other than English (English text comes from the finding itself, which
/// carries run-specific detail).
pub fn finding(code: &str) -> Option<(&'static str, &'static str)> {
    let l = lang();
    if l == Lang::En {
        return None;
    }
    let cause = l.table().iter().find(|(k, _)| {
        k.strip_prefix("finding.")
            .and_then(|r| r.strip_suffix(".cause"))
            == Some(code)
    })?;
    let advice = l.table().iter().find(|(k, _)| {
        k.strip_prefix("finding.")
            .and_then(|r| r.strip_suffix(".advice"))
            == Some(code)
    })?;
    Some((cause.1, advice.1))
}

/// A short description of an error code (`net.unreachable`) in the process language, for text
/// output next to the English message; `None` in English.
pub fn error_code(code: &str) -> Option<&'static str> {
    let l = lang();
    if l == Lang::En {
        return None;
    }
    l.table()
        .iter()
        .find(|(k, _)| k.strip_prefix("error.") == Some(code))
        .map(|(_, v)| *v)
}

/// English. Every key used anywhere must be here.
static EN: &[(&str, &str)] = &[
    // labels
    ("label.error", "error:"),
    ("label.hint", "hint:"),
    ("label.next", "next:"),
    ("label.warning", "warning:"),
    // brief
    ("brief.partial", "partial: the run was stopped early"),
    ("brief.more_findings", "… {n} more findings: {cmd}"),
    (
        "brief.numbers",
        "{ops} ops/s · {mib} MiB/s · p50 {p50} · p99 {p99} · errors {errors}%",
    ),
    ("brief.throttled", "{n} throttled ({ratio}% of requests)"),
    ("brief.per_month", "{usd} /month"),
    ("brief.sla_met", "SLA met"),
    (
        "brief.sla_met_unchecked_throttle",
        "SLA met (throttling not checked: --max-throttle-ratio)",
    ),
    ("brief.sla_breached", "SLA BREACHED"),
    ("brief.sla_breached_soft", "SLA breached (not enforced)"),
    ("brief.full_report", "full report: {cmd}"),
    // verdict levels
    ("verdict.ok", "OK"),
    ("verdict.attention", "ATTENTION"),
    ("verdict.critical", "CRITICAL"),
    // findings (code → cause, advice); English text comes from the finding itself
    (
        "finding.integrity.cause",
        "Data read back differs from data written",
    ),
    (
        "finding.integrity.advice",
        "Check proxies and gateways on the path; run `oprobe compat` on the target",
    ),
    ("finding.errors.cause", "Operations failed"),
    (
        "finding.errors.advice",
        "Look at the failing APIs and status codes (`oprobe analyze RUN --by status`)",
    ),
    (
        "finding.throttling.cause",
        "The service rate-limited requests (429/503)",
    ),
    (
        "finding.throttling.advice",
        "Lower concurrency or rate, or spread keys over more prefixes",
    ),
    (
        "finding.retries.cause",
        "Operations spend their time waiting to retry",
    ),
    (
        "finding.retries.advice",
        "Find the temporary errors behind the retries before tuning backoff",
    ),
    (
        "finding.tail_stragglers.cause",
        "A few requests wait far longer for the first byte",
    ),
    (
        "finding.tail_stragglers.advice",
        "Hedge or cut stragglers (--tail-cut) and verify with `oprobe verify`",
    ),
    (
        "finding.tail_latency.cause",
        "p99 latency is far above the median",
    ),
    (
        "finding.tail_latency.advice",
        "Attribute the slowest requests by stage (`oprobe analyze RUN --slowest 1`)",
    ),
    (
        "finding.schedule_backlog.cause",
        "The open-loop schedule fell behind: some arrivals never started",
    ),
    (
        "finding.schedule_backlog.advice",
        "Raise --concurrency, lower --rate, or check report.self for a saturated client",
    ),
    (
        "finding.client_queue.cause",
        "Operations wait for in-flight request permits",
    ),
    (
        "finding.client_queue.advice",
        "Raise --max-inflight or lower --concurrency",
    ),
    (
        "finding.cold_connections.cause",
        "Connection setup costs a large share of request time",
    ),
    (
        "finding.cold_connections.advice",
        "Keep connections alive and reuse them; check DNS, proxy and TLS",
    ),
    (
        "finding.http1_connections.cause",
        "HTTP/1.1 needs one connection per in-flight request",
    ),
    (
        "finding.http1_connections.advice",
        "Try HTTP/2 (--http h2) if the service supports it",
    ),
    (
        "finding.remote_skew.cause",
        "One remote address is much slower than the others",
    ),
    (
        "finding.remote_skew.advice",
        "Report the slow front-end address to the provider; compare per remote",
    ),
    (
        "finding.tcp_retransmits.cause",
        "Packets were lost while requests were in flight",
    ),
    (
        "finding.tcp_retransmits.advice",
        "Check the network path (MTU, congestion, NIC); compare from another host",
    ),
    ("finding.high_rtt.cause", "The network round trip is long"),
    (
        "finding.high_rtt.advice",
        "Run closer to the bucket's region, or hide latency with more parallelism",
    ),
    (
        "finding.consumer_stall.cause",
        "Response bodies wait for the consumer",
    ),
    (
        "finding.consumer_stall.advice",
        "The application, not storage, is the limit: speed up consumption",
    ),
    (
        "finding.overread.cause",
        "Downloaded bytes were never requested",
    ),
    (
        "finding.overread.advice",
        "Lower --range-gap or --chunk-size",
    ),
    (
        "finding.small_objects.cause",
        "Small requests: round trips, not bandwidth, bound throughput",
    ),
    (
        "finding.small_objects.advice",
        "Raise parallelism or batch small objects together",
    ),
    (
        "finding.suffix_read.cause",
        "Use suffix reads: saves one HEAD per file",
    ),
    (
        "finding.suffix_read.advice",
        "Read footers with a suffix range instead of HEAD + range",
    ),
    (
        "finding.cache_opportunity.cause",
        "The same objects are read repeatedly",
    ),
    (
        "finding.cache_opportunity.advice",
        "Add a read cache (--cache 256MiB) and verify the hit rate",
    ),
    (
        "finding.cost_hotspot.cause",
        "One API or wasted requests dominate the bill",
    ),
    (
        "finding.cost_hotspot.advice",
        "Cut the dominant API's calls or the failed and throttled requests",
    ),
    (
        "finding.slow_start.cause",
        "The first seconds are much slower than the steady state",
    ),
    (
        "finding.slow_start.advice",
        "Use a longer --warmup or longer runs before comparing",
    ),
    (
        "finding.server_latency.cause",
        "Latency-bound on server time-to-first-byte",
    ),
    (
        "finding.server_latency.advice",
        "Raise parallelism; throughput scales with concurrent requests",
    ),
    (
        "finding.download_bound.cause",
        "Bandwidth-bound on downloads",
    ),
    (
        "finding.download_bound.advice",
        "Check the per-stream and aggregate bandwidth before adding streams",
    ),
    ("finding.upload_bound.cause", "Bandwidth-bound on uploads"),
    (
        "finding.upload_bound.advice",
        "Check upload bandwidth; larger parts help only below the cap",
    ),
    (
        "finding.client_cpu.cause",
        "Load generator CPU is saturated",
    ),
    (
        "finding.client_cpu.advice",
        "Use a bigger machine or more workers; results understate the storage",
    ),
    (
        "finding.client_overhead.cause",
        "Time is spent outside network requests",
    ),
    (
        "finding.client_overhead.advice",
        "Profile client-side work (signing, buffering, CRC)",
    ),
    (
        "finding.local_backend.cause",
        "Local backend service time dominates",
    ),
    (
        "finding.local_backend.advice",
        "Results reflect local disk or memory, not network storage",
    ),
    (
        "finding.cache_ineffective.cause",
        "The read cache rarely hits",
    ),
    (
        "finding.cache_ineffective.advice",
        "Disable the cache or size it to the working set",
    ),
    ("finding.sla_p99.cause", "p99 latency exceeds the SLA"),
    (
        "finding.sla_p99.advice",
        "Find the slow stage of the slowest requests, then tune it",
    ),
    (
        "finding.rate_not_sustained.cause",
        "The target arrival rate was not sustained",
    ),
    (
        "finding.rate_not_sustained.advice",
        "Raise --concurrency or lower --rate",
    ),
    (
        "finding.run_invalid.cause",
        "The run cannot be judged on performance",
    ),
    (
        "finding.run_invalid.advice",
        "Fix the failures or lengthen the run, then measure again",
    ),
    ("finding.run_partial.cause", "The run stopped early"),
    (
        "finding.run_partial.advice",
        "Run again to its end before comparing",
    ),
    // error codes (short description next to the English message)
    ("error.usage.invalid_argument", "invalid argument"),
    ("error.usage.unknown_target", "unknown target"),
    ("error.usage.unknown_run", "unknown run"),
    ("error.usage.unknown_baseline", "unknown baseline"),
    ("error.config.invalid", "invalid configuration"),
    ("error.config.unknown_option", "unknown option"),
    ("error.auth.no_credentials", "no credentials"),
    ("error.auth.denied", "access denied"),
    ("error.net.unreachable", "endpoint unreachable"),
    ("error.net.dns", "DNS resolution failed"),
    ("error.net.tls", "TLS handshake failed"),
    ("error.net.timeout", "timed out"),
    ("error.net.rate_limited", "rate limited"),
    ("error.net.http", "HTTP error"),
    ("error.storage.not_found", "not found"),
    ("error.storage.throttled", "throttled by the service"),
    ("error.storage.unsupported", "not supported by the service"),
    ("error.safety.budget_exceeded", "budget exceeded"),
    ("error.safety.read_only", "refused by read-only mode"),
    ("error.safety.refused", "refused for safety"),
    ("error.run.sla_failed", "SLA breached"),
    ("error.run.regression", "regression"),
    ("error.run.interrupted", "interrupted"),
    ("error.internal", "internal error"),
];

/// 简体中文.
static ZH_CN: &[(&str, &str)] = &[
    ("label.error", "错误："),
    ("label.hint", "提示："),
    ("label.next", "下一步："),
    ("label.warning", "警告："),
    ("brief.partial", "部分结果：运行被提前停止"),
    ("brief.more_findings", "… 另有 {n} 条发现：{cmd}"),
    (
        "brief.numbers",
        "{ops} 次操作/秒 · {mib} MiB/s · p50 {p50} · p99 {p99} · 错误 {errors}%",
    ),
    ("brief.throttled", "{n} 次限流（占请求的 {ratio}%）"),
    ("brief.per_month", "{usd} /月"),
    ("brief.sla_met", "满足 SLA"),
    (
        "brief.sla_met_unchecked_throttle",
        "满足 SLA（未检查限流：--max-throttle-ratio）",
    ),
    ("brief.sla_breached", "违反 SLA"),
    ("brief.sla_breached_soft", "违反 SLA（未强制）"),
    ("brief.full_report", "完整报告：{cmd}"),
    ("verdict.ok", "正常"),
    ("verdict.attention", "注意"),
    ("verdict.critical", "严重"),
    ("finding.integrity.cause", "读回的数据与写入的数据不一致"),
    (
        "finding.integrity.advice",
        "检查链路上的代理和网关；对目标运行 `oprobe compat`",
    ),
    ("finding.errors.cause", "有操作失败"),
    (
        "finding.errors.advice",
        "查看失败的 API 和状态码（`oprobe analyze RUN --by status`）",
    ),
    ("finding.throttling.cause", "服务对请求限流（429/503）"),
    (
        "finding.throttling.advice",
        "降低并发或速率，或把键分散到更多前缀",
    ),
    ("finding.retries.cause", "操作的时间耗在等待重试上"),
    (
        "finding.retries.advice",
        "先找出导致重试的临时错误，再调整退避",
    ),
    (
        "finding.tail_stragglers.cause",
        "少数请求等待首字节的时间远长于其他请求",
    ),
    (
        "finding.tail_stragglers.advice",
        "对拖尾请求进行对冲或截断（--tail-cut），并用 `oprobe verify` 验证",
    ),
    ("finding.tail_latency.cause", "p99 延迟远高于中位数"),
    (
        "finding.tail_latency.advice",
        "按阶段归因最慢的请求（`oprobe analyze RUN --slowest 1`）",
    ),
    (
        "finding.schedule_backlog.cause",
        "开环调度落后：部分到达请求从未开始",
    ),
    (
        "finding.schedule_backlog.advice",
        "提高 --concurrency、降低 --rate，或检查 report.self 中客户端是否饱和",
    ),
    ("finding.client_queue.cause", "操作在等待在途请求许可"),
    (
        "finding.client_queue.advice",
        "提高 --max-inflight 或降低 --concurrency",
    ),
    (
        "finding.cold_connections.cause",
        "建立连接占用了大量请求时间",
    ),
    (
        "finding.cold_connections.advice",
        "保持并复用连接；检查 DNS、代理和 TLS",
    ),
    (
        "finding.http1_connections.cause",
        "HTTP/1.1 每个在途请求都需要一个连接",
    ),
    (
        "finding.http1_connections.advice",
        "如果服务支持，尝试 HTTP/2（--http h2）",
    ),
    ("finding.remote_skew.cause", "某个远端地址明显慢于其他地址"),
    (
        "finding.remote_skew.advice",
        "向服务商报告慢的前端地址；按远端地址对比",
    ),
    ("finding.tcp_retransmits.cause", "请求进行中发生了丢包"),
    (
        "finding.tcp_retransmits.advice",
        "检查网络路径（MTU、拥塞、网卡）；从另一台主机对比",
    ),
    ("finding.high_rtt.cause", "网络往返时间较长"),
    (
        "finding.high_rtt.advice",
        "在离存储桶区域更近的地方运行，或用更高并行度掩盖延迟",
    ),
    ("finding.consumer_stall.cause", "响应体在等待消费方"),
    (
        "finding.consumer_stall.advice",
        "瓶颈在应用而非存储：加快数据消费",
    ),
    ("finding.overread.cause", "下载了从未被请求的字节"),
    (
        "finding.overread.advice",
        "调小 --range-gap 或 --chunk-size",
    ),
    (
        "finding.small_objects.cause",
        "小请求：吞吐受往返次数而非带宽限制",
    ),
    ("finding.small_objects.advice", "提高并行度，或把小对象合并"),
    (
        "finding.suffix_read.cause",
        "使用后缀读取：每个文件省一次 HEAD",
    ),
    (
        "finding.suffix_read.advice",
        "用后缀范围读取文件尾，而不是 HEAD 加范围读取",
    ),
    ("finding.cache_opportunity.cause", "同样的对象被反复读取"),
    (
        "finding.cache_opportunity.advice",
        "添加读缓存（--cache 256MiB）并验证命中率",
    ),
    (
        "finding.cost_hotspot.cause",
        "某个 API 或无效请求占据了大部分费用",
    ),
    (
        "finding.cost_hotspot.advice",
        "减少主要 API 的调用，或减少失败和被限流的请求",
    ),
    ("finding.slow_start.cause", "开始几秒明显慢于稳定状态"),
    (
        "finding.slow_start.advice",
        "比较前使用更长的 --warmup 或更长的运行",
    ),
    ("finding.server_latency.cause", "受服务端首字节时间限制"),
    (
        "finding.server_latency.advice",
        "提高并行度；吞吐随并发请求数增长",
    ),
    ("finding.download_bound.cause", "受下载带宽限制"),
    (
        "finding.download_bound.advice",
        "增加流之前先检查单流和总带宽",
    ),
    ("finding.upload_bound.cause", "受上传带宽限制"),
    (
        "finding.upload_bound.advice",
        "检查上传带宽；只有低于上限时更大的分片才有帮助",
    ),
    ("finding.client_cpu.cause", "压测机 CPU 已饱和"),
    (
        "finding.client_cpu.advice",
        "使用更大的机器或更多 worker；当前结果低估了存储能力",
    ),
    ("finding.client_overhead.cause", "时间花在网络请求之外"),
    (
        "finding.client_overhead.advice",
        "分析客户端开销（签名、缓冲、CRC）",
    ),
    ("finding.local_backend.cause", "本地后端的服务时间占主导"),
    (
        "finding.local_backend.advice",
        "结果反映的是本地磁盘或内存，而非网络存储",
    ),
    ("finding.cache_ineffective.cause", "读缓存很少命中"),
    (
        "finding.cache_ineffective.advice",
        "关闭缓存，或按工作集大小设置缓存",
    ),
    ("finding.sla_p99.cause", "p99 延迟超过 SLA"),
    (
        "finding.sla_p99.advice",
        "找出最慢请求的慢阶段，然后针对性调优",
    ),
    ("finding.rate_not_sustained.cause", "未能维持目标到达速率"),
    (
        "finding.rate_not_sustained.advice",
        "提高 --concurrency 或降低 --rate",
    ),
    ("finding.run_invalid.cause", "无法据此运行判断性能"),
    (
        "finding.run_invalid.advice",
        "先解决失败或延长运行时间，再重新测量",
    ),
    ("finding.run_partial.cause", "运行被提前停止"),
    ("finding.run_partial.advice", "比较之前请完整地再运行一次"),
    ("error.usage.invalid_argument", "参数无效"),
    ("error.usage.unknown_target", "未知目标"),
    ("error.usage.unknown_run", "未知运行"),
    ("error.usage.unknown_baseline", "未知基线"),
    ("error.config.invalid", "配置无效"),
    ("error.config.unknown_option", "未知选项"),
    ("error.auth.no_credentials", "没有凭据"),
    ("error.auth.denied", "拒绝访问"),
    ("error.net.unreachable", "无法连接端点"),
    ("error.net.dns", "DNS 解析失败"),
    ("error.net.tls", "TLS 握手失败"),
    ("error.net.timeout", "超时"),
    ("error.net.rate_limited", "被限速"),
    ("error.net.http", "HTTP 错误"),
    ("error.storage.not_found", "未找到"),
    ("error.storage.throttled", "被服务限流"),
    ("error.storage.unsupported", "服务不支持"),
    ("error.safety.budget_exceeded", "超出预算"),
    ("error.safety.read_only", "只读模式拒绝"),
    ("error.safety.refused", "出于安全原因拒绝"),
    ("error.run.sla_failed", "违反 SLA"),
    ("error.run.regression", "性能回退"),
    ("error.run.interrupted", "已中断"),
    ("error.internal", "内部错误"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(s: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = s;
        while let Some(i) = rest.find('{') {
            let Some(j) = rest[i..].find('}') else { break };
            out.insert(rest[i + 1..i + j].to_owned());
            rest = &rest[i + j + 1..];
        }
        out
    }

    #[test]
    fn tables_have_the_same_keys_and_placeholders() {
        let en: BTreeSet<_> = EN.iter().map(|(k, _)| *k).collect();
        let zh: BTreeSet<_> = ZH_CN.iter().map(|(k, _)| *k).collect();
        assert_eq!(en.len(), EN.len(), "duplicate English keys");
        assert_eq!(zh.len(), ZH_CN.len(), "duplicate Chinese keys");
        let missing: Vec<_> = en.difference(&zh).collect();
        let extra: Vec<_> = zh.difference(&en).collect();
        assert!(missing.is_empty(), "zh-CN lacks {missing:?}");
        assert!(extra.is_empty(), "zh-CN has keys English lacks: {extra:?}");
        for (k, v) in EN {
            let z = lookup(Lang::ZhCn, k).unwrap();
            assert_eq!(placeholders(v), placeholders(z), "placeholders of {k}");
        }
    }

    #[test]
    fn every_finding_and_error_code_is_translated() {
        for d in crate::analysis::catalog::all() {
            for part in ["cause", "advice"] {
                let key = format!("finding.{}.{part}", d.code);
                assert!(lookup(Lang::En, &key).is_some(), "missing {key}");
            }
        }
        for c in crate::errors::ErrorCode::ALL {
            let key = format!("error.{}", c.as_str());
            assert!(lookup(Lang::En, &key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn languages_come_from_the_flag_then_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(detect_from(Some("zh-CN"), env(&[])), Lang::ZhCn);
        assert_eq!(
            detect_from(Some("en"), env(&[("LANG", "zh_CN.UTF-8")])),
            Lang::En
        );
        assert_eq!(
            detect_from(Some("auto"), env(&[("LANG", "zh_CN.UTF-8")])),
            Lang::ZhCn
        );
        assert_eq!(
            detect_from(None, env(&[("LANG", "zh_CN.UTF-8")])),
            Lang::ZhCn
        );
        // LC_ALL wins over LANG, even for a language without a catalog.
        assert_eq!(
            detect_from(
                None,
                env(&[("LC_ALL", "de_DE.UTF-8"), ("LANG", "zh_CN.UTF-8")])
            ),
            Lang::En
        );
        assert_eq!(detect_from(None, env(&[("LANG", "C")])), Lang::En);
        assert_eq!(detect_from(None, env(&[])), Lang::En);
        assert_eq!(Lang::parse("zh-Hans-CN"), Some(Lang::ZhCn));
        assert_eq!(Lang::parse("zh_TW.UTF-8"), None);
        assert_eq!(Lang::parse("en_GB"), Some(Lang::En));
    }

    #[test]
    fn formatting_and_fallbacks() {
        assert_eq!(
            tf_in(
                Lang::ZhCn,
                "brief.full_report",
                &[("cmd", &"oprobe runs show x")]
            ),
            "完整报告：oprobe runs show x"
        );
        assert_eq!(t_in(Lang::ZhCn, "no.such.key"), "no.such.key");
        assert_eq!(t_in(Lang::En, "label.hint"), "hint:");
    }
}
