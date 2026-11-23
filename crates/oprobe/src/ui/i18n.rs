//! The app's words in English and 简体中文 (design v6 D2): the one message catalog of oprobe.
//! The app follows the system's language; a setting in the app overrides it
//! ([`crate::ui::store::Store::set_lang`]). Numbers, units, codes, operator paths and the
//! text the core writes (a finding's cause and evidence) stay as they are; finding titles are
//! translated here by code.

use std::cell::Cell;
use std::fmt::Display;

/// A language of the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh-CN",
        }
    }

    /// A locale tag or language name: `zh`, `zh-CN`, `zh_CN.UTF-8`, `zh-Hans` → 中文; `en…` →
    /// English; anything else is not one of the app's languages.
    pub fn parse(tag: &str) -> Option<Lang> {
        let t = tag.trim().to_ascii_lowercase();
        if t.starts_with("zh") {
            Some(Lang::Zh)
        } else if t.starts_with("en") {
            Some(Lang::En)
        } else {
            None
        }
    }
}

thread_local! {
    static CURRENT: Cell<Lang> = const { Cell::new(Lang::En) };
}

/// Make `l` the language [`tr`] answers in (the root re-draws the app after it).
pub fn set(l: Lang) {
    CURRENT.with(|c| c.set(l));
}

pub fn current() -> Lang {
    CURRENT.with(|c| c.get())
}

/// The system's language: the platform's preferred languages and locale, else the locale
/// variables (a terminal), else English.
pub fn system() -> Lang {
    let platform = nsh_core::prelude::locale_settings()
        .languages
        .iter()
        .find_map(|l| Lang::parse(l.as_str()))
        .or_else(|| Lang::parse(nsh_core::prelude::locale().as_str()))
        .filter(|l| *l != Lang::En);
    platform
        .or_else(|| {
            ["LC_ALL", "LC_MESSAGES", "LANG"]
                .iter()
                .filter_map(|k| std::env::var(k).ok())
                .find(|v| !v.is_empty())
                .and_then(|v| Lang::parse(&v))
        })
        .unwrap_or(Lang::En)
}

/// `key` in the current language (English, then the key itself, when missing).
pub fn tr(key: &'static str) -> &'static str {
    tr_dyn(key).unwrap_or(key)
}

/// A key made at run time (`verdict.ok`, `finding.throttling`): `None` when the catalog has
/// no entry in either language.
pub fn tr_dyn(key: &str) -> Option<&'static str> {
    find(current(), key).or_else(|| find(Lang::En, key))
}

/// `key` with each `{}` filled by the next of `args`.
pub fn trf(key: &'static str, args: &[&dyn Display]) -> String {
    let mut out = String::new();
    let mut rest = tr(key);
    for a in args {
        match rest.split_once("{}") {
            Some((head, tail)) => {
                out.push_str(head);
                out.push_str(&a.to_string());
                rest = tail;
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// The title of a finding by its code (the code in words when the catalog has none).
pub fn finding_title(code: &str) -> String {
    tr_dyn(&format!("finding.{code}"))
        .map(str::to_owned)
        .unwrap_or_else(|| code.replace('_', " "))
}

fn find(l: Lang, key: &str) -> Option<&'static str> {
    let table = match l {
        Lang::En => EN,
        Lang::Zh => ZH,
    };
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

static EN: &[(&str, &str)] = &[
    ("app.dismiss", "Dismiss"),
    ("menu.file", "File"),
    ("menu.edit", "Edit"),
    ("menu.run", "Run"),
    ("menu.view", "View"),
    ("menu.window", "Window"),
    ("menu.help", "Help"),
    ("menu.settings", "Settings…"),
    ("menu.new_run", "New Run…"),
    ("menu.open_file", "Open Run File…"),
    ("menu.copy_id", "Copy Run ID"),
    ("menu.copy_command", "Copy Command"),
    ("menu.run_again", "Run Again"),
    ("menu.stop", "Stop"),
    ("menu.prove", "Prove…"),
    ("menu.compare", "Compare Selected"),
    ("menu.compare_n", "Compare {} Runs"),
    ("menu.compare_with", "Compare With Selected"),
    ("menu.delete", "Delete…"),
    ("menu.open", "Open"),
    ("menu.hide_sidebar", "Hide Sidebar"),
    ("menu.show_sidebar", "Show Sidebar"),
    ("menu.docs", "oprobe Documentation"),
    ("menu.primary", "Main Menu"),
    ("side.running", "Running"),
    ("runs.blank", " "),
    ("settings.title", "Settings"),
    ("settings.language", "Language"),
    ("settings.data", "Runs are stored in"),
    ("dialog.cancel", "Cancel"),
    ("dialog.new_run", "New run"),
    ("dialog.done", "Done"),
    ("delete.title", "Delete runs"),
    ("delete.one", "Delete run {}?"),
    ("delete.many", "Delete {} runs?"),
    (
        "delete.detail",
        "Their files and trial runs are removed. This cannot be undone.",
    ),
    ("app.failed", "That did not work; see the log"),
    ("app.lang_auto", "System"),
    ("app.loading", "Loading…"),
    ("app.running", "running"),
    ("nav.runs", "Runs"),
    ("nav.profile", "Profile"),
    ("nav.operator", "Operator"),
    ("nav.compare", "Compare"),
    // Runs
    ("runs.new", "New run"),
    ("runs.operator", "Operator"),
    (
        "runs.operator_hint",
        "s3://bucket/prefix, op.toml, a configured name, or lab://demo",
    ),
    ("runs.start", "Run"),
    ("runs.stop", "Stop"),
    ("runs.lab", "Lab"),
    ("runs.preset", "Workload"),
    ("runs.duration", "Duration"),
    ("runs.concurrency", "Concurrency"),
    (
        "runs.several",
        "Several operators separated by spaces run one after another on the same workload and open side by side in Compare.",
    ),
    ("runs.live", "Live"),
    ("runs.history", "History · {} runs"),
    ("runs.empty", "No runs yet"),
    (
        "runs.empty_hint",
        "Profile the local lab: an S3 with faults, no credentials, in seconds.",
    ),
    ("runs.open", "Open run {}"),
    ("runs.pick", "Compare {}"),
    ("runs.delete", "Delete {}"),
    ("runs.compare_picked", "Compare {} picked"),
    ("runs.pick_hint", "+ picks a run for Compare"),
    (
        "runs.need_operator",
        "Enter an operator first: a URL, an op.toml file or lab://demo",
    ),
    (
        "runs.bad_concurrency",
        "Concurrency is a whole number of at least 1",
    ),
    (
        "lab.demo",
        "Throttling above 24 requests in flight and 2.5 % slow stragglers.",
    ),
    ("lab.throttle", "503 SlowDown above 8 requests in flight."),
    (
        "lab.stragglers",
        "3 % of requests wait 400 ms before answering.",
    ),
    ("lab.bandwidth", "All responses share 16 MiB/s."),
    ("lab.flaky", "4 % server errors and 2 % connection resets."),
    ("lab.ratelimit", "503 SlowDown above 300 requests/s."),
    ("phase.idle", "Idle"),
    ("phase.preparing dataset", "Preparing the dataset"),
    ("phase.warming up", "Warming up"),
    ("phase.measuring", "Measuring"),
    ("phase.verifying", "Verifying"),
    ("phase.cleaning up", "Cleaning up"),
    ("phase.cleanup", "Cleaning up test data"),
    ("phase.done", "Done"),
    // Shared vocabulary
    ("metric.goodput", "Goodput"),
    ("metric.bandwidth", "Bandwidth"),
    ("metric.p50", "p50"),
    ("metric.p99", "p99"),
    ("metric.errors", "Errors"),
    ("metric.throttled", "Throttled"),
    ("verdict.ok", "Healthy"),
    ("verdict.review", "Review"),
    ("verdict.attention", "Attention"),
    ("verdict.critical", "Critical"),
    ("severity.info", "Info"),
    ("severity.warning", "Warning"),
    ("severity.critical", "Critical"),
    ("stage.queue", "Queue"),
    ("stage.cache", "Cache"),
    ("stage.connect", "Connect"),
    ("stage.upload", "Upload"),
    ("stage.server", "Server (TTFB)"),
    ("stage.download", "Download"),
    ("stage.retry", "Retry backoff"),
    ("stage.client", "Client"),
    ("req.queue", "queue"),
    ("req.connect", "connect"),
    ("req.send", "send"),
    ("req.ttfb", "Ttfb"),
    ("req.body", "body"),
    ("req.stall", "stall"),
    ("req.client", "client"),
    // Profile
    ("profile.none", "No run open"),
    ("profile.none_hint", "Open a run from Runs, or start one."),
    ("profile.to_operator", "Open in Operator"),
    ("profile.pick", "Compare…"),
    ("profile.time", "Where the time went"),
    ("profile.findings", "Findings · {}"),
    (
        "profile.no_findings",
        "Nothing to fix: no layer change is suggested for this run.",
    ),
    ("profile.remedy", "Remedy"),
    ("profile.advice", "Advice"),
    ("profile.try_remedy", "Try it in Operator"),
    ("profile.try_remedy_of", "Try the remedy of {}"),
    ("profile.slowest", "Slowest operations · request waterfall"),
    ("profile.hover", "Point at a request to see its stages"),
    ("profile.timeline", "Timeline"),
    ("profile.latency", "Latency"),
    ("profile.heatmap", "Request latency over time"),
    ("profile.connections", "Connections"),
    ("profile.warnings", "Warnings"),
    ("conn.dialed", "Connections dialed"),
    ("conn.reuse", "Requests on reused connections"),
    ("conn.protocols", "Protocols"),
    ("conn.peak", "Peak requests in flight"),
    // Operator
    ("operator.none", "No operator yet"),
    (
        "operator.none_hint",
        "Open a run first: its operator is the one you change and prove.",
    ),
    ("operator.of_run", "The operator of run {}"),
    ("operator.remedies", "Remedies of the findings"),
    (
        "operator.no_remedies",
        "No finding of this run names a layer change.",
    ),
    ("operator.apply", "Apply"),
    ("operator.section.layers", "Layers"),
    ("operator.section.read", "Read"),
    ("operator.section.write", "Write"),
    ("operator.section.http", "HTTP client"),
    ("operator.default", "default"),
    ("operator.changed", "changed"),
    ("operator.remedy_of", "Remedy for {}"),
    ("operator.changes", "Changes"),
    (
        "operator.no_changes",
        "No changes yet: switch a layer or change a value above.",
    ),
    ("operator.diff", "The change as Rust and as op.toml"),
    ("operator.copy_rust", "Copy Rust"),
    ("operator.copy_toml", "Copy TOML"),
    ("operator.reset", "Reset"),
    ("operator.prove", "Prove"),
    (
        "operator.prove_hint",
        "Interleaved A/B replays of this run decide whether the change helps on your objective. Without changes, the top finding's remedy is proved.",
    ),
    ("operator.objective", "Objective"),
    ("operator.budget", "Budget"),
    ("operator.prove_run", "Prove"),
    ("operator.replays", "{} replays finished"),
    ("operator.pair", "Pair {}"),
    ("operator.pairs_count", "{} A/B pairs"),
    (
        "operator.secondary",
        "Secondary, not decisive (shares as percentage points, pp): {}",
    ),
    ("operator.proven_diff", "The proven diff"),
    ("operator.stopped", "Stopped."),
    ("prove.improved", "proved"),
    ("prove.worse", "refuted"),
    ("prove.inconclusive", "inconclusive"),
    // Compare
    ("compare.pick", "Runs to compare"),
    ("compare.none", "Select two or more runs"),
    (
        "compare.none_hint",
        "Hold ⌘ or Ctrl and click runs in the sidebar, or run several operators at once.",
    ),
    ("compare.of", "{} runs compared"),
    ("compare.findings", "Findings"),
    ("compare.operator", "Operator vs first"),
    ("compare.baseline", "(the first)"),
    ("compare.same", "same operator"),
    ("profile.distribution", "Latency distribution"),
    ("profile.verbs", "By verb"),
    ("plot.cdf", "Share of operations at or below a latency"),
    (
        "plot.cdf_buckets",
        "Share of operations at or below a latency (from the heatmap's buckets)",
    ),
    ("plot.ops", "operations"),
    ("plot.throttled_secs", "throttled"),
    ("plot.error_secs", "errors"),
    ("plot.throttled_rate", "Throttled responses per second"),
    ("plot.errors_rate", "Failed operations per second"),
    ("plot.per_s", "per second"),
    ("plot.by_inflight", "Share throttled by requests in flight"),
    ("plot.throttled_share", "throttled"),
    ("plot.limit_interval", "limit interval"),
    ("plot.limit", "limit ≈ {}"),
    ("plot.convergence", "Change of the objective, pair by pair"),
    ("plot.pair_delta", "each pair"),
    ("plot.estimate", "estimate"),
    ("plot.interval", "95 % interval"),
    ("plot.noise", "noise floor (A/A)"),
    ("plot.evidence", "Evidence"),
    ("verb.ops", "{} ops"),
    ("compare.cdf", "Latency distribution"),
    ("compare.goodput", "Goodput over time"),
    // Findings by code
    ("finding.throttling", "Throttling"),
    ("finding.rate_limit", "Request rate limit"),
    ("finding.key_limit", "Per-key limit"),
    ("finding.bandwidth_cap", "Bandwidth cap"),
    ("finding.tail_stragglers", "Server stragglers"),
    ("finding.hung_requests", "Hung requests"),
    ("finding.small_ranges", "Small range reads"),
    ("finding.slow_multipart", "Slow multipart writes"),
    ("finding.repeated_reads", "Re-reading the same bytes"),
    ("finding.cold_connections", "Cold connections"),
    ("finding.client_bound", "Client-bound"),
    ("finding.errors", "Errors"),
    ("finding.invalid_run", "Invalid run"),
    ("finding.integrity", "Corrupted data"),
    ("finding.retries", "Retries"),
    ("finding.tail_latency", "Tail latency"),
    ("finding.client_queue", "Client queueing"),
    ("finding.http1_connections", "HTTP/1.1 connections"),
    ("finding.server_latency", "Server latency"),
    ("finding.download_bound", "Download-bound"),
    ("finding.upload_bound", "Upload-bound"),
    ("finding.client_cpu", "Client CPU"),
    ("finding.overread", "Over-reading"),
    ("finding.cache_opportunity", "Cacheable reads"),
    ("finding.run_partial", "Partial run"),
    ("finding.run_invalid", "Invalid run"),
];

static ZH: &[(&str, &str)] = &[
    ("app.dismiss", "关闭"),
    ("menu.file", "文件"),
    ("menu.edit", "编辑"),
    ("menu.run", "运行"),
    ("menu.view", "显示"),
    ("menu.window", "窗口"),
    ("menu.help", "帮助"),
    ("menu.settings", "设置…"),
    ("menu.new_run", "新建运行…"),
    ("menu.open_file", "打开运行文件…"),
    ("menu.copy_id", "拷贝运行 ID"),
    ("menu.copy_command", "拷贝命令"),
    ("menu.run_again", "再次运行"),
    ("menu.stop", "停止"),
    ("menu.prove", "证明…"),
    ("menu.compare", "对比所选"),
    ("menu.compare_n", "对比 {} 次运行"),
    ("menu.compare_with", "与所选对比"),
    ("menu.delete", "删除…"),
    ("menu.open", "打开"),
    ("menu.hide_sidebar", "隐藏边栏"),
    ("menu.show_sidebar", "显示边栏"),
    ("menu.docs", "oprobe 文档"),
    ("menu.primary", "主菜单"),
    ("side.running", "正在运行"),
    ("runs.blank", " "),
    ("settings.title", "设置"),
    ("settings.language", "语言"),
    ("settings.data", "运行记录存放在"),
    ("dialog.cancel", "取消"),
    ("dialog.new_run", "新建运行"),
    ("dialog.done", "完成"),
    ("delete.title", "删除运行"),
    ("delete.one", "删除运行 {}？"),
    ("delete.many", "删除 {} 次运行？"),
    (
        "delete.detail",
        "它们的文件和试验运行都会被删除，且无法撤销。",
    ),
    ("app.failed", "操作未完成，请查看日志"),
    ("app.lang_auto", "跟随系统"),
    ("app.loading", "加载中…"),
    ("app.running", "运行中"),
    ("nav.runs", "运行"),
    ("nav.profile", "剖析"),
    ("nav.operator", "Operator"),
    ("nav.compare", "对比"),
    ("runs.new", "新建运行"),
    ("runs.operator", "Operator"),
    (
        "runs.operator_hint",
        "s3://bucket/prefix、op.toml、配置中的名称，或 lab://demo",
    ),
    ("runs.start", "运行"),
    ("runs.stop", "停止"),
    ("runs.lab", "实验室"),
    ("runs.preset", "负载"),
    ("runs.duration", "时长"),
    ("runs.concurrency", "并发"),
    (
        "runs.several",
        "以空格分隔多个 operator，它们会在同一负载上依次运行，并在“对比”中并排打开。",
    ),
    ("runs.live", "实时"),
    ("runs.history", "历史 · {} 次运行"),
    ("runs.empty", "还没有运行"),
    (
        "runs.empty_hint",
        "先剖析本地实验室：带故障的 S3，无需凭证，几秒出结果。",
    ),
    ("runs.open", "打开运行 {}"),
    ("runs.pick", "对比 {}"),
    ("runs.delete", "删除 {}"),
    ("runs.compare_picked", "对比已选的 {} 个"),
    ("runs.pick_hint", "点 + 把运行加入对比"),
    (
        "runs.need_operator",
        "请先输入 operator：URL、op.toml 文件或 lab://demo",
    ),
    ("runs.bad_concurrency", "并发须为不小于 1 的整数"),
    ("lab.demo", "在途请求超过 24 个时限流，2.5 % 的慢请求。"),
    ("lab.throttle", "在途请求超过 8 个时返回 503 SlowDown。"),
    ("lab.stragglers", "3 % 的请求要等 400 ms 才响应。"),
    ("lab.bandwidth", "所有响应共享 16 MiB/s。"),
    ("lab.flaky", "4 % 服务端错误，2 % 连接重置。"),
    ("lab.ratelimit", "超过 300 次请求/秒时返回 503 SlowDown。"),
    ("phase.idle", "空闲"),
    ("phase.preparing dataset", "准备数据集"),
    ("phase.warming up", "预热中"),
    ("phase.measuring", "测量中"),
    ("phase.verifying", "校验中"),
    ("phase.cleaning up", "清理中"),
    ("phase.cleanup", "正在清理测试数据"),
    ("phase.done", "完成"),
    ("metric.goodput", "有效吞吐"),
    ("metric.bandwidth", "带宽"),
    ("metric.p50", "p50"),
    ("metric.p99", "p99"),
    ("metric.errors", "错误"),
    ("metric.throttled", "被限流"),
    ("verdict.ok", "健康"),
    ("verdict.review", "值得一看"),
    ("verdict.attention", "需要关注"),
    ("verdict.critical", "严重"),
    ("severity.info", "提示"),
    ("severity.warning", "警告"),
    ("severity.critical", "严重"),
    ("stage.queue", "排队"),
    ("stage.cache", "缓存"),
    ("stage.connect", "建连"),
    ("stage.upload", "上传"),
    ("stage.server", "服务端 (TTFB)"),
    ("stage.download", "下载"),
    ("stage.retry", "重试退避"),
    ("stage.client", "客户端"),
    ("req.queue", "排队"),
    ("req.connect", "建连"),
    ("req.send", "发送"),
    ("req.ttfb", "首字节"),
    ("req.body", "响应体"),
    ("req.stall", "停顿"),
    ("req.client", "客户端"),
    ("profile.none", "没有打开的运行"),
    ("profile.none_hint", "在“运行”中打开一次运行，或新建一次。"),
    ("profile.to_operator", "在 Operator 中打开"),
    ("profile.pick", "对比…"),
    ("profile.time", "时间花在哪里"),
    ("profile.findings", "发现 · {}"),
    (
        "profile.no_findings",
        "无需修复：这次运行没有建议的层改动。",
    ),
    ("profile.remedy", "改法"),
    ("profile.advice", "建议"),
    ("profile.try_remedy", "在 Operator 中试试"),
    ("profile.try_remedy_of", "试试 {} 的改法"),
    ("profile.slowest", "最慢的操作 · 请求瀑布图"),
    ("profile.hover", "指向一个请求以查看各阶段"),
    ("profile.timeline", "时间线"),
    ("profile.latency", "延迟"),
    ("profile.heatmap", "请求延迟随时间分布"),
    ("profile.connections", "连接"),
    ("profile.warnings", "警告"),
    ("conn.dialed", "新建连接数"),
    ("conn.reuse", "复用连接的请求"),
    ("conn.protocols", "协议"),
    ("conn.peak", "在途请求峰值"),
    ("operator.none", "还没有 operator"),
    (
        "operator.none_hint",
        "请先打开一次运行：要修改和验证的就是它的 operator。",
    ),
    ("operator.of_run", "运行 {} 的 operator"),
    ("operator.remedies", "各项发现的改法"),
    ("operator.no_remedies", "这次运行的发现都不涉及层的改动。"),
    ("operator.apply", "应用"),
    ("operator.section.layers", "层"),
    ("operator.section.read", "读取"),
    ("operator.section.write", "写入"),
    ("operator.section.http", "HTTP 客户端"),
    ("operator.default", "默认"),
    ("operator.changed", "已修改"),
    ("operator.remedy_of", "改法：{}"),
    ("operator.changes", "改动"),
    (
        "operator.no_changes",
        "还没有改动：在上方切换一个层或修改一个值。",
    ),
    ("operator.diff", "改动的 Rust 与 op.toml 形式"),
    ("operator.copy_rust", "复制 Rust"),
    ("operator.copy_toml", "复制 TOML"),
    ("operator.reset", "重置"),
    ("operator.prove", "证明"),
    (
        "operator.prove_hint",
        "交替进行的 A/B 回放会判断这项改动是否改善你的目标。没有改动时，验证首要发现的改法。",
    ),
    ("operator.objective", "目标"),
    ("operator.budget", "预算"),
    ("operator.prove_run", "证明"),
    ("operator.replays", "已完成 {} 次回放"),
    ("operator.pair", "第 {} 对"),
    ("operator.pairs_count", "{} 对 A/B"),
    (
        "operator.secondary",
        "次要指标，不决定结论（占比为百分点差，pp）：{}",
    ),
    ("operator.proven_diff", "已证明的改动"),
    ("operator.stopped", "已停止。"),
    ("prove.improved", "已证实"),
    ("prove.worse", "被否定"),
    ("prove.inconclusive", "无定论"),
    ("compare.pick", "要对比的运行"),
    ("compare.none", "请选择两次或更多运行"),
    (
        "compare.none_hint",
        "在边栏中按住 ⌘ 或 Ctrl 点选运行，或一次运行多个 operator。",
    ),
    ("compare.of", "对比 {} 次运行"),
    ("compare.findings", "发现"),
    ("compare.operator", "与第一个的 operator 差异"),
    ("compare.baseline", "（第一个）"),
    ("compare.same", "operator 相同"),
    ("profile.distribution", "延迟分布"),
    ("profile.verbs", "按操作类型"),
    ("plot.cdf", "不超过该延迟的操作占比"),
    (
        "plot.cdf_buckets",
        "不超过该延迟的操作占比（来自热力图分桶）",
    ),
    ("plot.ops", "操作"),
    ("plot.throttled_secs", "被限流"),
    ("plot.error_secs", "出错"),
    ("plot.throttled_rate", "每秒被限流的响应"),
    ("plot.errors_rate", "每秒失败的操作"),
    ("plot.per_s", "每秒"),
    ("plot.by_inflight", "按在途请求数的被限流比例"),
    ("plot.throttled_share", "被限流"),
    ("plot.limit_interval", "上限区间"),
    ("plot.limit", "上限 ≈ {}"),
    ("plot.convergence", "目标指标的变化（逐对）"),
    ("plot.pair_delta", "每一对"),
    ("plot.estimate", "估计"),
    ("plot.interval", "95 % 区间"),
    ("plot.noise", "噪声底线（A/A）"),
    ("plot.evidence", "证据"),
    ("verb.ops", "{} 次"),
    ("compare.cdf", "延迟分布"),
    ("compare.goodput", "吞吐量随时间"),
    ("finding.throttling", "限流"),
    ("finding.rate_limit", "请求速率限制"),
    ("finding.key_limit", "单键限制"),
    ("finding.bandwidth_cap", "带宽上限"),
    ("finding.tail_stragglers", "服务端慢请求"),
    ("finding.hung_requests", "挂起的请求"),
    ("finding.small_ranges", "过小的范围读"),
    ("finding.slow_multipart", "分片写入缓慢"),
    ("finding.repeated_reads", "重复读取相同数据"),
    ("finding.cold_connections", "冷连接"),
    ("finding.client_bound", "受限于客户端"),
    ("finding.errors", "错误"),
    ("finding.invalid_run", "无效运行"),
    ("finding.integrity", "数据损坏"),
    ("finding.retries", "重试"),
    ("finding.tail_latency", "尾延迟"),
    ("finding.client_queue", "客户端排队"),
    ("finding.http1_connections", "HTTP/1.1 连接"),
    ("finding.server_latency", "服务端延迟"),
    ("finding.download_bound", "受限于下载"),
    ("finding.upload_bound", "受限于上传"),
    ("finding.client_cpu", "客户端 CPU"),
    ("finding.overread", "过量读取"),
    ("finding.cache_opportunity", "可缓存的读取"),
    ("finding.run_partial", "部分运行"),
    ("finding.run_invalid", "无效运行"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_languages_have_every_key_once() {
        let keys = |t: &'static [(&'static str, &'static str)]| {
            t.iter().map(|(k, _)| *k).collect::<Vec<_>>()
        };
        let (en, zh) = (keys(EN), keys(ZH));
        let mut sorted = en.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), en.len(), "a key twice in EN");
        let missing: Vec<_> = en.iter().filter(|k| !zh.contains(k)).collect();
        let extra: Vec<_> = zh.iter().filter(|k| !en.contains(k)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "zh missing {missing:?}, extra {extra:?}"
        );
        // The placeholders agree.
        for (k, v) in EN {
            let z = find(Lang::Zh, k).unwrap();
            assert_eq!(v.matches("{}").count(), z.matches("{}").count(), "{k}");
        }
    }

    #[test]
    fn every_key_the_views_use_is_in_the_catalog() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let mut sources = Vec::new();
        for d in [dir.clone(), dir.join("views")] {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.extension().is_some_and(|x| x == "rs") {
                    sources.push(std::fs::read_to_string(p).unwrap());
                }
            }
        }
        let mut used = Vec::new();
        for src in &sources {
            for call in ["tr(\"", "fill(\"", "trf(\"", "t(\""] {
                for (i, _) in src.match_indices(call) {
                    // A call of this name, not the end of another (`insert("…`).
                    if src[..i]
                        .chars()
                        .last()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
                    {
                        continue;
                    }
                    let part = &src[i + call.len()..];
                    if let Some(k) = part.split('"').next()
                        && k.contains('.')
                        && !k.contains(' ')
                    {
                        used.push(k.to_owned());
                    }
                }
            }
        }
        assert!(used.len() > 50, "{}", used.len());
        let missing: Vec<_> = used
            .iter()
            .filter(|k| find(Lang::En, k).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "keys missing from the catalog: {missing:?}"
        );
    }

    #[test]
    fn languages_parse_and_fill() {
        assert_eq!(Lang::parse("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh-Hans"), Some(Lang::Zh));
        assert_eq!(Lang::parse("en-US"), Some(Lang::En));
        assert_eq!(Lang::parse("C"), None);
        set(Lang::Zh);
        assert_eq!(trf("runs.history", &[&3]), "历史 · 3 次运行");
        assert_eq!(finding_title("throttling"), "限流");
        set(Lang::En);
        assert_eq!(finding_title("some_new_code"), "some new code");
    }
}
