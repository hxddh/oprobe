//! `/` Minibuffer 命令栏解析器（由内置 Ratatui 终端工作台与 `native/` NSH 双端工作台共同复用）：
//! 支持动态切换后端 Scheme、注入桶/Endpoint 参数、修改并发与分块旋钮、发起网络微秒探测与落盘报告。

use crate::autotune::TuneKnob;
use crate::cost::CloudPricingProfile;
use crate::workload::{ProfileConfig, WorkloadKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MinibufferCommand {
    Workload(WorkloadKind),
    Scheme(String),
    SetOption { key: String, value: String },
    Concurrency(usize),
    ChunkKiB(usize),
    GapKiB(usize),
    Foyer(bool),
    TailCut(bool),
    KeepAlive(bool),
    PricingPreset(String),
    ProbeOnce,
    AutoTune,
    NetProbe(String),
    ExportReport(String),
    Help,
    Quit,
}

fn parse_bool_flag(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!("expected on|off, got `{other}`")),
    }
}

/// 解析用户在 `/` 或 `:` 命令栏输入的指令字符串
pub fn parse_minibuffer_command(input: &str) -> Result<MinibufferCommand, String> {
    let trimmed = input.trim().trim_start_matches(['/', ':']).trim();
    if trimmed.is_empty() {
        return Err("empty command (type `help` for available commands)".to_owned());
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.next().map(str::trim).unwrap_or_default();

    match verb.as_str() {
        "q" | "quit" | "exit" => Ok(MinibufferCommand::Quit),
        "h" | "help" | "?" => Ok(MinibufferCommand::Help),
        "probe" | "run" => Ok(MinibufferCommand::ProbeOnce),
        "tune" | "autotune" => Ok(MinibufferCommand::AutoTune),
        "w" | "workload" => match rest.to_ascii_lowercase().as_str() {
            "1" | "parquet" => Ok(MinibufferCommand::Workload(WorkloadKind::ParquetPointLookup)),
            "2" | "ckpt" | "multipart" => {
                Ok(MinibufferCommand::Workload(WorkloadKind::CheckpointMultipart))
            }
            "3" | "meta" | "list" => {
                Ok(MinibufferCommand::Workload(WorkloadKind::MetadataPrefixStorm))
            }
            "4" | "mixed" | "rw" => Ok(MinibufferCommand::Workload(WorkloadKind::MixedReadWrite)),
            other => Err(format!(
                "unknown workload `{other}` (use parquet | ckpt | meta | mixed)"
            )),
        },
        "scheme" | "backend" => {
            if rest.is_empty() {
                Err("usage: scheme <memory|fs|s3|gcs|b2|oss|azblob|cos|webdav>".to_owned())
            } else {
                Ok(MinibufferCommand::Scheme(rest.to_ascii_lowercase()))
            }
        }
        "set" => {
            let (k, v) = rest
                .split_once('=')
                .ok_or_else(|| "usage: set <key>=<value> (e.g. set bucket=my-bucket)".to_owned())?;
            let key = k.trim();
            if key.is_empty() {
                return Err("option key cannot be empty".to_owned());
            }
            Ok(MinibufferCommand::SetOption {
                key: key.to_owned(),
                value: v.trim().to_owned(),
            })
        }
        "conc" | "concurrent" => {
            let n = rest
                .parse::<usize>()
                .map_err(|_| format!("invalid concurrency `{rest}`"))?;
            Ok(MinibufferCommand::Concurrency(n.clamp(1, 256)))
        }
        "chunk" => {
            let kib = rest
                .parse::<usize>()
                .map_err(|_| format!("invalid chunk KiB `{rest}`"))?;
            Ok(MinibufferCommand::ChunkKiB(kib.clamp(64, 65_536)))
        }
        "gap" => {
            let kib = rest
                .parse::<usize>()
                .map_err(|_| format!("invalid gap KiB `{rest}`"))?;
            Ok(MinibufferCommand::GapKiB(kib.clamp(0, 16_384)))
        }
        "foyer" | "cache" => Ok(MinibufferCommand::Foyer(parse_bool_flag(rest)?)),
        "tailcut" | "hedge" => Ok(MinibufferCommand::TailCut(parse_bool_flag(rest)?)),
        "keepalive" | "pool" => Ok(MinibufferCommand::KeepAlive(parse_bool_flag(rest)?)),
        "pricing" | "cloud" => {
            if rest.is_empty() {
                Err("usage: pricing <s3|r2|b2>".to_owned())
            } else {
                Ok(MinibufferCommand::PricingPreset(rest.to_ascii_lowercase()))
            }
        }
        "net" | "netprobe" => {
            if rest.is_empty() {
                Err("usage: net <https://s3.amazonaws.com>".to_owned())
            } else {
                Ok(MinibufferCommand::NetProbe(rest.to_owned()))
            }
        }
        "export" | "save" => {
            let prefix = if rest.is_empty() {
                "oprobe_report"
            } else {
                rest
            };
            Ok(MinibufferCommand::ExportReport(prefix.to_owned()))
        }
        other => Err(format!("unknown command `{other}` (type `:help` for syntax)")),
    }
}

/// 将不涉及异步任务的纯状态修改应用到 `ProfileConfig` 与 `TuneKnob`
pub fn apply_state_mutation(
    cmd: &MinibufferCommand,
    profile: &mut ProfileConfig,
    knob: &mut TuneKnob,
) -> Option<String> {
    match cmd {
        MinibufferCommand::Workload(kind) => {
            profile.workload.kind = *kind;
            Some(format!("切换负载模型 -> {}", kind.title()))
        }
        MinibufferCommand::Scheme(s) => {
            profile.target.scheme = s.clone();
            profile.target.name = format!("{s} target");
            if s == "fs" && !profile.target.options.contains_key("root") {
                profile
                    .target
                    .options
                    .insert("root".to_owned(), "/tmp/oprobe_fs_bench".to_owned());
            }
            Some(format!("切换存储后端 -> scheme={s}"))
        }
        MinibufferCommand::SetOption { key, value } => {
            profile.target.options.insert(key.clone(), value.clone());
            Some(format!("已写入后端参数 {key}={value}"))
        }
        MinibufferCommand::Concurrency(n) => {
            knob.concurrent = *n;
            Some(format!("已更新并发上限 concurrent={n}"))
        }
        MinibufferCommand::ChunkKiB(kib) => {
            knob.chunk_bytes = kib * 1024;
            Some(format!("已更新分块尺寸 chunk={kib} KiB"))
        }
        MinibufferCommand::GapKiB(kib) => {
            knob.range_gap_bytes = kib * 1024;
            Some(format!("已更新稀疏合并窗口 gap={kib} KiB"))
        }
        MinibufferCommand::Foyer(on) => {
            knob.enable_foyer = *on;
            Some(format!("FoyerLayer 本地混合缓存 -> {}", if *on { "ON" } else { "OFF" }))
        }
        MinibufferCommand::TailCut(on) => {
            knob.enable_tail_cut = *on;
            Some(format!("TailCutLayer 长尾对冲 -> {}", if *on { "ON" } else { "OFF" }))
        }
        MinibufferCommand::KeepAlive(on) => {
            knob.keep_alive = *on;
            Some(format!("HTTP 连接池复用 -> {}", if *on { "ON" } else { "OFF" }))
        }
        MinibufferCommand::PricingPreset(p) => {
            profile.pricing = match p.as_str() {
                "r2" | "cloudflare" => CloudPricingProfile::cloudflare_r2(),
                "b2" | "backblaze" => CloudPricingProfile::backblaze_b2(),
                _ => CloudPricingProfile::aws_s3_standard(),
            };
            Some(format!("切换成本模型 -> {}", profile.pricing.provider_name))
        }
        MinibufferCommand::Help => Some(
            "指令: workload parquet|ckpt|meta|mixed | scheme s3|fs|memory | set k=v | conc N | chunk KiB | gap KiB | net <url> | export <name> | tune | quit"
                .to_owned(),
        ),
        _ => None,
    }
}
