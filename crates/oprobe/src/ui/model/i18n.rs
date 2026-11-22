//! The App's words in the user's language (design v5 §9, U15: 简体中文): the language is
//! [`crate::i18n::lang`] — set by the CLI from `--lang` / `OPROBE_LANG` / the locale, and by the
//! App from the OS ([`crate::i18n::detect_os`]) when nothing else names one. The App's own keys
//! (`app.*`, `cmd.*`, `tab.*`) live here; shared keys (`verdict.*`, `finding.*`, `error.*`) come
//! from the core catalog, so the CLI and the App say the same thing.
//!
//! Lookup: this table in the language, the core catalog in the language, this table in English,
//! the core catalog in English, then the key itself (a typo stays visible).

use std::fmt::Display;

use crate::i18n::{Lang, lang};

/// The App's language at start: the language the CLI set (`--lang`, `OPROBE_LANG`, the locale
/// variables), else the OS user locale (apps opened from Finder or a launcher get no locale).
pub fn init() {
    let flag = std::env::var("OPROBE_LANG").ok();
    if lang() == Lang::En {
        crate::i18n::set_lang(crate::i18n::detect_os(flag.as_deref()));
    }
}

/// `key` in the App's language.
pub fn tr(key: &'static str) -> &'static str {
    tr_in(lang(), key)
}

/// `key` in `l`.
pub fn tr_in(l: Lang, key: &'static str) -> &'static str {
    find(l, key)
        .or_else(|| crate::i18n::lookup(l, key))
        .or_else(|| find(Lang::En, key))
        .or_else(|| crate::i18n::lookup(Lang::En, key))
        .unwrap_or(key)
}

/// `key` with `{name}` placeholders filled.
pub fn trf(key: &'static str, args: &[(&str, &dyn Display)]) -> String {
    trf_in(lang(), key, args)
}

/// [`trf`] in `l`.
pub fn trf_in(l: Lang, key: &'static str, args: &[(&str, &dyn Display)]) -> String {
    let mut s = tr_in(l, key).to_owned();
    for (name, v) in args {
        s = s.replace(&format!("{{{name}}}"), &v.to_string());
    }
    s
}

/// A command's title (`cmd.<id>`), else `fallback` (its English label).
pub fn cmd_label(id: &str, fallback: &'static str) -> &'static str {
    let l = lang();
    table(l)
        .iter()
        .chain(table(Lang::En))
        .find(|(k, _)| k.strip_prefix("cmd.") == Some(id))
        .map(|(_, v)| *v)
        .unwrap_or(fallback)
}

fn find(l: Lang, key: &str) -> Option<&'static str> {
    table(l).iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn table(l: Lang) -> &'static [(&'static str, &'static str)] {
    match l {
        Lang::En => EN,
        Lang::ZhCn => ZH_CN,
    }
}

static EN: &[(&str, &str)] = &[
    // tabs
    ("tab.overview", "Overview"),
    ("tab.requests", "Requests"),
    ("tab.timeline", "Timeline"),
    ("tab.connections", "Connections"),
    ("tab.findings", "Findings"),
    ("tab.cost", "Cost"),
    // one vocabulary (U9): run verdicts, finding severities, net verdicts, check states
    ("level.healthy", "Healthy"),
    ("level.review", "Review"),
    ("level.warning", "Warning"),
    ("level.critical", "Critical"),
    ("severity.info", "info"),
    ("severity.warning", "warning"),
    ("severity.critical", "critical"),
    ("net.improved", "improved"),
    ("net.regressed", "regressed"),
    ("net.traded", "traded"),
    ("net.no_change", "no change"),
    ("net.inconclusive", "inconclusive"),
    ("check.improved", "improved"),
    ("check.regressed", "regressed"),
    ("check.inconclusive", "inconclusive"),
    ("check.within", "within limits"),
    ("check.skipped", "skipped"),
    ("verify.confirmed", "confirmed"),
    ("verify.refuted", "refuted"),
    ("verify.inconclusive", "inconclusive"),
    // metrics in prose (U12: no machine ids)
    ("metric.ops_per_s", "Throughput"),
    ("metric.mib_per_s", "Bandwidth"),
    ("metric.p50_ms", "Median latency"),
    ("metric.p99_ms", "p99 latency"),
    ("metric.p999_ms", "p99.9 latency"),
    ("metric.error_ratio", "Errors"),
    ("metric.throttled_request_ratio", "Throttled requests"),
    ("metric.requests_per_op", "Requests per operation"),
    ("metric.read_amplification", "Read amplification"),
    ("metric.cost_per_million_ops", "Cost per million operations"),
    ("metric.cost_per_month", "Cost per month"),
    // the Health screen
    ("app.health", "Health"),
    ("app.health_of", "Health of {target}"),
    ("app.tile.ops", "OPS/S"),
    ("app.tile.p99", "P99"),
    ("app.tile.throttled", "THROTTLED"),
    ("app.tile.cost", "$/MONTH"),
    ("app.tile.baseline", "baseline {lo}–{hi}"),
    ("app.tile.last_runs", "last {n} runs"),
    ("app.hypothesis", "Likely limit"),
    (
        "app.no_hypothesis",
        "No limit identified: the run was not throttled, saturated or failing.",
    ),
    ("app.primary_fix", "Fix"),
    ("app.predicted", "predicted: {what}"),
    ("app.history", "HISTORY"),
    ("app.open_latest", "Open Latest Run"),
    ("app.no_runs_for_target", "No runs of this target yet"),
    ("app.annotation.config", "config changed: {what}"),
    ("app.annotation.verify", "verify of {run}"),
    ("app.annotation.tuned", "tuned settings"),
    // live
    ("app.provisional", "PROVISIONAL FINDINGS"),
    ("app.confidence", "confidence {pct}"),
    ("app.early_numbers", "early numbers: {n} operations so far"),
    ("app.eta", "about {eta} left"),
    ("app.done_title", "{what} finished"),
    // waterfall
    ("app.waterfall", "WATERFALL"),
    ("app.critical_path", "critical path"),
    ("app.backoff", "backoff"),
    ("app.typical_op", "Compare with a typical op"),
    (
        "app.typical_line",
        "a typical {verb} takes {total} with {requests} requests",
    ),
    ("app.slower_by", "{x}× slower than typical"),
    // run rows
    ("app.run_row", "{when} · {preset} · {verdict}"),
    // palette
    ("app.palette.placeholder", "Type a command"),
    ("app.palette.empty", "Nothing matches"),
    // disabled reasons (U6)
    ("why.busy", "a job is running: stop it first"),
    ("why.idle", "nothing is running"),
    ("why.no_run", "open a finished run first"),
    ("why.no_verify", "the shown run has no finding to verify"),
    ("why.two_runs", "needs two runs"),
    ("why.no_runs", "no runs yet"),
    ("why.no_targets", "no targets configured"),
    ("why.tune", "add a target or open a run first"),
];

static ZH_CN: &[(&str, &str)] = &[
    ("tab.overview", "概览"),
    ("tab.requests", "请求"),
    ("tab.timeline", "时间线"),
    ("tab.connections", "连接"),
    ("tab.findings", "发现"),
    ("tab.cost", "成本"),
    ("level.healthy", "健康"),
    ("level.review", "待查看"),
    ("level.warning", "警告"),
    ("level.critical", "严重"),
    ("severity.info", "提示"),
    ("severity.warning", "警告"),
    ("severity.critical", "严重"),
    ("net.improved", "改善"),
    ("net.regressed", "退化"),
    ("net.traded", "有得有失"),
    ("net.no_change", "无变化"),
    ("net.inconclusive", "无定论"),
    ("check.improved", "改善"),
    ("check.regressed", "退化"),
    ("check.inconclusive", "无定论"),
    ("check.within", "在限值内"),
    ("check.skipped", "已跳过"),
    ("verify.confirmed", "已证实"),
    ("verify.refuted", "已推翻"),
    ("verify.inconclusive", "无定论"),
    ("metric.ops_per_s", "吞吐量"),
    ("metric.mib_per_s", "带宽"),
    ("metric.p50_ms", "中位延迟"),
    ("metric.p99_ms", "p99 延迟"),
    ("metric.p999_ms", "p99.9 延迟"),
    ("metric.error_ratio", "错误"),
    ("metric.throttled_request_ratio", "被限流的请求"),
    ("metric.requests_per_op", "每次操作的请求数"),
    ("metric.read_amplification", "读放大"),
    ("metric.cost_per_million_ops", "每百万次操作成本"),
    ("metric.cost_per_month", "每月成本"),
    ("app.health", "健康状况"),
    ("app.health_of", "{target} 的健康状况"),
    ("app.tile.ops", "操作/秒"),
    ("app.tile.p99", "P99"),
    ("app.tile.throttled", "限流"),
    ("app.tile.cost", "美元/月"),
    ("app.tile.baseline", "基线 {lo}–{hi}"),
    ("app.tile.last_runs", "最近 {n} 次运行"),
    ("app.hypothesis", "可能的瓶颈"),
    (
        "app.no_hypothesis",
        "未发现瓶颈：运行没有被限流、饱和或失败。",
    ),
    ("app.primary_fix", "修复"),
    ("app.predicted", "预计：{what}"),
    ("app.history", "历史"),
    ("app.open_latest", "打开最近一次运行"),
    ("app.no_runs_for_target", "这个目标还没有运行"),
    ("app.annotation.config", "配置已更改：{what}"),
    ("app.annotation.verify", "验证 {run}"),
    ("app.annotation.tuned", "调优后的设置"),
    ("app.provisional", "初步发现"),
    ("app.confidence", "置信度 {pct}"),
    ("app.early_numbers", "早期数据：目前 {n} 次操作"),
    ("app.eta", "约剩 {eta}"),
    ("app.done_title", "{what} 已完成"),
    ("app.waterfall", "瀑布图"),
    ("app.critical_path", "关键路径"),
    ("app.backoff", "退避"),
    ("app.typical_op", "与典型操作比较"),
    (
        "app.typical_line",
        "典型的 {verb} 耗时 {total}，{requests} 个请求",
    ),
    ("app.slower_by", "比典型慢 {x} 倍"),
    ("app.run_row", "{when} · {preset} · {verdict}"),
    ("app.palette.placeholder", "输入命令"),
    ("app.palette.empty", "没有匹配项"),
    ("why.busy", "有任务正在运行：请先停止"),
    ("why.idle", "没有正在运行的任务"),
    ("why.no_run", "请先打开一次已完成的运行"),
    ("why.no_verify", "当前运行没有可验证的发现"),
    ("why.two_runs", "需要两次运行"),
    ("why.no_runs", "还没有运行"),
    ("why.no_targets", "尚未配置目标"),
    ("why.tune", "请先添加目标或打开一次运行"),
    ("cmd.new", "新建运行…"),
    ("cmd.open-run", "打开运行…"),
    ("cmd.open", "打开运行文件…"),
    ("cmd.add-target", "添加目标…"),
    ("cmd.switch-target", "切换目标…"),
    ("cmd.export", "导出运行…"),
    ("cmd.export-json", "导出报告为 JSON…"),
    ("cmd.export-md", "导出报告为 Markdown…"),
    ("cmd.export-csv", "导出请求为 CSV…"),
    ("cmd.copy-link", "复制视图链接"),
    ("cmd.settings", "设置…"),
    ("cmd.run", "运行"),
    ("cmd.stop", "停止"),
    ("cmd.rerun", "再次运行"),
    ("cmd.edit-rerun", "编辑后再次运行…"),
    ("cmd.verify", "验证首要发现"),
    ("cmd.fix", "修复：推荐设置…"),
    ("cmd.demo", "运行演示"),
    ("cmd.compare", "比较运行"),
    ("cmd.tune", "调优…"),
    ("cmd.probe", "探测…"),
    ("cmd.slowest", "解释最慢的请求"),
    ("cmd.health", "目标健康状况"),
    ("cmd.waterfall", "最慢操作的瀑布图"),
    ("cmd.sidebar", "显示/隐藏侧栏"),
    ("cmd.inspector", "显示/隐藏检查器"),
    ("cmd.log", "对数延迟坐标轴"),
    ("cmd.palette", "命令面板…"),
    ("cmd.tab-0", "概览"),
    ("cmd.tab-1", "请求"),
    ("cmd.tab-2", "时间线"),
    ("cmd.tab-3", "连接"),
    ("cmd.tab-4", "发现"),
    ("cmd.tab-5", "成本"),
    ("cmd.help", "键盘快捷键"),
    ("cmd.docs", "oprobe 文档"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholders(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s
            .split('{')
            .skip(1)
            .filter_map(|p| p.split_once('}').map(|(n, _)| n))
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn every_translation_has_an_english_key_with_the_same_placeholders() {
        for (k, v) in ZH_CN {
            if k.starts_with("cmd.") {
                let id = &k[4..];
                assert!(
                    crate::ui::model::commands::spec(id).is_some(),
                    "{k}: no such command"
                );
                continue;
            }
            let en = find(Lang::En, k).unwrap_or_else(|| panic!("{k} has no English text"));
            assert_eq!(placeholders(en), placeholders(v), "{k}");
        }
        for (k, _) in EN {
            assert!(find(Lang::ZhCn, k).is_some(), "{k} has no 简体中文 text");
        }
    }

    #[test]
    fn lookup_falls_back_to_the_core_catalog_then_english_then_the_key() {
        assert_eq!(tr_in(Lang::ZhCn, "tab.cost"), "成本");
        assert_eq!(tr_in(Lang::En, "tab.cost"), "Cost");
        // A shared key comes from the core catalog.
        assert_eq!(tr_in(Lang::En, "verdict.ok"), "OK");
        assert_eq!(tr_in(Lang::En, "no.such.key"), "no.such.key");
        assert_eq!(
            trf_in(Lang::ZhCn, "app.health_of", &[("target", &"prod")]),
            "prod 的健康状况"
        );
        assert_eq!(cmd_label("export", "Export Run…"), "Export Run…");
    }
}
