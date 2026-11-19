//! `/` & `:` Minibuffer command parser for the interactive `oprobe tui` workbench.

use std::time::Duration;

use crate::autotune::TuneKnob;
use crate::cost::CloudPricingProfile;
use crate::workload::{parse_duration_str, ProfileConfig, WorkloadKind};

#[derive(Clone, Debug, PartialEq)]
pub enum MinibufferCommand {
    Workload(WorkloadKind),
    Scheme(String),
    Url(String),
    SetOption { key: String, value: String },
    Concurrency(usize),
    ChunkKiB(usize),
    GapKiB(usize),
    DurationWindow(Option<Duration>),
    Rounds(usize),
    Foyer(bool),
    TailCut(bool),
    KeepAlive(bool),
    PricingPreset(String),
    ProbeOnce,
    StreamRun(Duration),
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

/// Parse a minibuffer command string (leading `/` or `:` is optional).
pub fn parse_minibuffer_command(input: &str) -> Result<MinibufferCommand, String> {
    let trimmed = input.trim().trim_start_matches(['/', ':']).trim();
    if trimmed.is_empty() {
        return Err("empty command (type `help` or press `?` for syntax)".to_owned());
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.next().map(str::trim).unwrap_or_default();

    match verb.as_str() {
        "q" | "quit" | "exit" => Ok(MinibufferCommand::Quit),
        "h" | "help" | "?" => Ok(MinibufferCommand::Help),
        "probe" | "run" => Ok(MinibufferCommand::ProbeOnce),
        "stream" => {
            let dur = if rest.is_empty() {
                Duration::from_secs(10)
            } else {
                parse_duration_str(rest).map_err(|e| e.to_string())?
            };
            Ok(MinibufferCommand::StreamRun(dur))
        }
        "tune" | "autotune" => Ok(MinibufferCommand::AutoTune),
        "w" | "workload" => WorkloadKind::from_slug(rest)
            .map(MinibufferCommand::Workload)
            .map_err(|e| e.to_string()),
        "url" | "target" => {
            if rest.is_empty() {
                Err("usage: url <s3://bucket/prefix | fs:///tmp/bench | memory://>".to_owned())
            } else {
                Ok(MinibufferCommand::Url(rest.to_owned()))
            }
        }
        "scheme" | "backend" => {
            if rest.is_empty() {
                Err("usage: scheme <memory|fs|s3|gcs|b2|oss|azblob|cos|webdav|http>".to_owned())
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
        "duration" | "dur" => {
            if rest.eq_ignore_ascii_case("off") || rest.eq_ignore_ascii_case("none") {
                Ok(MinibufferCommand::DurationWindow(None))
            } else {
                let d = parse_duration_str(rest).map_err(|e| e.to_string())?;
                Ok(MinibufferCommand::DurationWindow(Some(d)))
            }
        }
        "rounds" => {
            let r = rest
                .parse::<usize>()
                .map_err(|_| format!("invalid rounds `{rest}`"))?;
            Ok(MinibufferCommand::Rounds(r.clamp(1, 100_000)))
        }
        "foyer" | "cache" => Ok(MinibufferCommand::Foyer(parse_bool_flag(rest)?)),
        "tailcut" | "hedge" => Ok(MinibufferCommand::TailCut(parse_bool_flag(rest)?)),
        "keepalive" | "pool" => Ok(MinibufferCommand::KeepAlive(parse_bool_flag(rest)?)),
        "pricing" | "cloud" => {
            if rest.is_empty() {
                Err("usage: pricing <s3|r2|gcs|oss|b2>".to_owned())
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
        other => Err(format!("unknown command `{other}` (type `:help` or press `?`)")),
    }
}

/// Apply synchronous configuration mutations to [`ProfileConfig`] and [`TuneKnob`].
pub fn apply_state_mutation(
    cmd: &MinibufferCommand,
    profile: &mut ProfileConfig,
    knob: &mut TuneKnob,
) -> Option<String> {
    match cmd {
        MinibufferCommand::Workload(kind) => {
            profile.workload.kind = *kind;
            Some(format!("Switched workload -> {}", kind.title()))
        }
        MinibufferCommand::Url(u) => match ProfileConfig::from_storage_url(u, None, None, false, None) {
            Ok(next) => {
                profile.target = next.target;
                profile.pricing = next.pricing;
                Some(format!("Configured target from URL -> {u}"))
            }
            Err(e) => Some(format!("Invalid URL `{u}`: {e}")),
        },
        MinibufferCommand::Scheme(s) => {
            profile.target.scheme = s.clone();
            profile.target.name = format!("{s} target");
            if s == "fs" && !profile.target.options.contains_key("root") {
                profile
                    .target
                    .options
                    .insert("root".to_owned(), "/tmp/oprobe_fs_bench".to_owned());
            }
            Some(format!("Switched storage backend -> scheme={s}"))
        }
        MinibufferCommand::SetOption { key, value } => {
            profile.target.options.insert(key.clone(), value.clone());
            Some(format!("Updated target option {key}={value}"))
        }
        MinibufferCommand::Concurrency(n) => {
            knob.concurrent = *n;
            Some(format!("Updated concurrency limit -> concurrent={n}"))
        }
        MinibufferCommand::ChunkKiB(kib) => {
            knob.chunk_bytes = kib * 1024;
            Some(format!("Updated I/O chunk size -> chunk={kib} KiB"))
        }
        MinibufferCommand::GapKiB(kib) => {
            knob.range_gap_bytes = kib * 1024;
            Some(format!("Updated range coalesce gap -> gap={kib} KiB"))
        }
        MinibufferCommand::DurationWindow(dur) => {
            profile.workload.duration_secs = dur.map(|d| d.as_secs_f64());
            match dur {
                Some(d) => Some(format!("Set streaming window -> {:.1}s", d.as_secs_f64())),
                None => Some("Cleared duration window (using deterministic rounds)".to_owned()),
            }
        }
        MinibufferCommand::Rounds(r) => {
            profile.workload.rounds = *r;
            profile.workload.duration_secs = None;
            Some(format!("Set deterministic rounds -> {r}"))
        }
        MinibufferCommand::Foyer(on) => {
            knob.enable_foyer = *on;
            Some(format!(
                "Local W-TinyLFU Cache -> {}",
                if *on { "ON" } else { "OFF" }
            ))
        }
        MinibufferCommand::TailCut(on) => {
            knob.enable_tail_cut = *on;
            Some(format!(
                "TailCutLayer Straggler Hedging -> {}",
                if *on { "ON" } else { "OFF" }
            ))
        }
        MinibufferCommand::KeepAlive(on) => {
            knob.keep_alive = *on;
            Some(format!(
                "HTTP Connection Pool Reuse -> {}",
                if *on { "ON" } else { "OFF" }
            ))
        }
        MinibufferCommand::PricingPreset(p) => {
            profile.pricing = match p.as_str() {
                "r2" | "cloudflare" => CloudPricingProfile::cloudflare_r2(),
                "gcs" | "google" => CloudPricingProfile::google_gcs_standard(),
                "oss" | "alibaba" | "aliyun" => CloudPricingProfile::alibaba_oss_standard(),
                "b2" | "backblaze" => CloudPricingProfile::backblaze_b2(),
                _ => CloudPricingProfile::aws_s3_standard(),
            };
            Some(format!(
                "Switched FinOps baseline -> {}",
                profile.pricing.provider_name
            ))
        }
        MinibufferCommand::Help => Some(
            "Commands: url <uri> | scheme <s3|fs|memory> | set k=v | conc N | chunk KiB | gap KiB | stream 10s | tune | net <url> | export <name> | quit"
                .to_owned(),
        ),
        _ => None,
    }
}
