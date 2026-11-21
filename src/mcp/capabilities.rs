//! `oprobe capabilities`: what this build can do, machine-readable first.

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::{Preset, Profile, TargetSpec};
use crate::errors::{ErrorCode, exit};
use crate::model::{DatasetInfo, KnobValues, MixEntry};
use crate::output::CommandOutput;
use crate::report::style::{bold, dim};

/// Schema id of [`Capabilities`].
pub const CAPABILITIES_SCHEMA: &str = "oprobe.capabilities/1";

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Service {
    /// Canonical scheme (`s3`, `gcs`, …).
    pub scheme: String,
    /// Other URL schemes accepted for it.
    pub aliases: Vec<String>,
    pub example: String,
    /// Network service (false: in-process backend).
    pub network: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PresetInfo {
    pub name: String,
    pub description: String,
    pub mix: Vec<MixEntry>,
    pub dataset: DatasetInfo,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ProviderInfo {
    /// `--provider` value.
    pub key: String,
    pub name: String,
    pub storage_gb_month_usd: f64,
    pub write_per_million_usd: f64,
    pub read_per_million_usd: f64,
    pub list_per_million_usd: f64,
    pub delete_per_million_usd: f64,
    pub egress_per_gb_usd: f64,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct NamedInfo {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ExitCodeInfo {
    pub code: u8,
    pub meaning: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ErrorCodeInfo {
    pub code: ErrorCode,
    pub exit_code: u8,
    pub retryable: bool,
    pub docs: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FindingCodeInfo {
    pub code: String,
    pub title: String,
    pub category: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    /// Writes test data (needs `oprobe mcp --allow-write`).
    pub writes: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct McpInfo {
    pub transports: Vec<String>,
    pub protocol_versions: Vec<String>,
    pub tools: Vec<McpToolInfo>,
    /// URI templates.
    pub resources: Vec<String>,
    pub prompts: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Defaults {
    pub preset: String,
    pub duration_s: f64,
    pub warmup_s: f64,
    pub knobs: KnobValues,
    /// Trace rows kept per run before reservoir sampling.
    pub trace_max_requests_count: u64,
    pub tune_trial_duration: String,
    pub tune_max_trials_count: u64,
    pub tune_max_concurrency_count: u64,
}

/// One command and the output formats it offers (`-o`).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CommandInfo {
    /// The command path, e.g. `runs show`.
    pub command: String,
    pub formats: Vec<crate::output::OutputFormat>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Features {
    /// `oprobe top` (terminal workbench).
    pub tui: bool,
    /// `oprobe gui` (native windows).
    pub gui: bool,
}

/// Result of `oprobe capabilities`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Capabilities {
    /// Always [`CAPABILITIES_SCHEMA`].
    pub schema: String,
    pub oprobe_version: String,
    pub services: Vec<Service>,
    pub presets: Vec<PresetInfo>,
    /// Pricing presets (`--provider`).
    pub providers: Vec<ProviderInfo>,
    /// When the list prices were taken (YYYY-MM).
    pub pricing_as_of: String,
    pub transports: Vec<NamedInfo>,
    pub http_versions: Vec<String>,
    /// Every output format; `commands` says which command offers which.
    pub output_formats: Vec<String>,
    /// Every command with its output formats (`-o` outside them is a usage error).
    pub commands: Vec<CommandInfo>,
    pub exit_codes: Vec<ExitCodeInfo>,
    pub error_codes: Vec<ErrorCodeInfo>,
    pub finding_codes: Vec<FindingCodeInfo>,
    pub schemas: Vec<String>,
    pub mcp: McpInfo,
    pub defaults: Defaults,
    pub features: Features,
}

fn svc(scheme: &str, aliases: &[&str], example: &str, network: bool) -> Service {
    Service {
        scheme: scheme.into(),
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        example: example.into(),
        network,
    }
}

impl Capabilities {
    pub fn current() -> Self {
        let services = vec![
            svc("s3", &["s3a", "r2", "minio"], "s3://bucket/prefix", true),
            svc("gcs", &["gs"], "gcs://bucket/prefix", true),
            svc(
                "azblob",
                &["az", "azure"],
                "azblob://container/prefix",
                true,
            ),
            svc("oss", &[], "oss://bucket/prefix", true),
            svc("cos", &[], "cos://bucket/prefix", true),
            svc("b2", &[], "b2://bucket/prefix", true),
            svc("webdav", &["webdavs"], "webdavs://host/path", true),
            svc("fs", &["file"], "fs:///tmp/bench", false),
            svc("memory", &["mem"], "memory://", false),
        ];
        let presets = Preset::ALL
            .iter()
            .map(|p| {
                let mut prof = Profile::from_target(
                    TargetSpec::from_url("memory://").expect("memory:// parses"),
                );
                prof.workload.preset = *p;
                let w = crate::model::workload_info(&prof);
                PresetInfo {
                    name: p.slug().into(),
                    description: p.describe().into(),
                    mix: w.mix,
                    dataset: w.dataset,
                }
            })
            .collect();
        let providers = crate::analysis::cost::presets()
            .into_iter()
            .map(|(k, p)| ProviderInfo {
                key: k.into(),
                name: p.provider,
                storage_gb_month_usd: p.storage_gb_month,
                write_per_million_usd: p.write_per_million,
                read_per_million_usd: p.read_per_million,
                list_per_million_usd: p.list_per_million,
                delete_per_million_usd: p.delete_per_million,
                egress_per_gb_usd: p.egress_per_gb,
                note: p.note,
            })
            .collect();
        let exit_codes = [
            (exit::OK, "ok"),
            (
                exit::RUNTIME,
                "runtime failure (auth, network, storage, internal)",
            ),
            (exit::USAGE, "usage or configuration error"),
            (exit::GATE, "SLA failed or regression"),
            (exit::BUDGET, "a budget stopped the run"),
            (exit::SAFETY, "a safety rail refused the command"),
            (exit::INTERRUPTED, "interrupted (Ctrl-C)"),
        ]
        .into_iter()
        .map(|(code, m)| ExitCodeInfo {
            code,
            meaning: m.into(),
        })
        .collect();
        let error_codes = ErrorCode::ALL
            .iter()
            .map(|c| ErrorCodeInfo {
                code: *c,
                exit_code: c.exit_code(),
                retryable: c.retryable(),
                docs: c.docs(),
            })
            .collect();
        let finding_codes = super::catalog::all()
            .into_iter()
            .map(|d| FindingCodeInfo {
                code: d.code.into(),
                title: d.title.into(),
                category: d.category.into(),
            })
            .collect();
        let prof = Profile::from_target(TargetSpec::from_url("memory://").expect("parses"));
        let defaults = Defaults {
            preset: prof.workload.preset.slug().into(),
            duration_s: prof.workload.duration.get().as_secs_f64(),
            warmup_s: prof.workload.warmup.get().as_secs_f64(),
            knobs: KnobValues::from(&prof.knobs),
            trace_max_requests_count: crate::trace::DEFAULT_MAX_REQUESTS as u64,
            tune_trial_duration: crate::cli::args::tune_defaults::TRIAL_DURATION.into(),
            tune_max_trials_count: crate::cli::args::tune_defaults::MAX_TRIALS as u64,
            tune_max_concurrency_count: crate::cli::args::tune_defaults::MAX_CONCURRENCY as u64,
        };
        Self {
            schema: CAPABILITIES_SCHEMA.into(),
            oprobe_version: env!("CARGO_PKG_VERSION").into(),
            services,
            presets,
            providers,
            pricing_as_of: crate::analysis::cost::PRICING_AS_OF.into(),
            transports: vec![
                NamedInfo {
                    name: "opendal".into(),
                    description: "OpenDAL's own HTTP client (what production runs), observed through its extension points".into(),
                },
                NamedInfo {
                    name: "microscope".into(),
                    description: "oprobe's instrumented connection pool (--microscope): exact queueing, per-dial DNS/TCP/TLS, HTTP/2 streams".into(),
                },
            ],
            http_versions: vec!["auto".into(), "h1".into(), "h2".into()],
            output_formats: ["text", "json", "ndjson", "md", "html"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            commands: command_infos(),
            exit_codes,
            error_codes,
            finding_codes,
            schemas: super::schemas::names().iter().map(|s| s.to_string()).collect(),
            mcp: McpInfo {
                transports: vec!["stdio".into(), "streamable-http".into()],
                protocol_versions: super::server::protocol_versions(),
                tools: super::tools::SPECS
                    .iter()
                    .map(|t| McpToolInfo {
                        name: t.name.into(),
                        description: t.description.into(),
                        writes: t.writes,
                    })
                    .collect(),
                resources: super::resources::TEMPLATES
                    .iter()
                    .map(|t| t.uri_template.to_string())
                    .collect(),
                prompts: super::prompts::PROMPTS
                    .iter()
                    .map(|p| p.name.to_string())
                    .collect(),
            },
            defaults,
            features: Features {
                tui: cfg!(feature = "tui"),
                gui: cfg!(feature = "gui"),
            },
        }
    }
}

/// Every command of the CLI (from its definition) with its formats.
fn command_infos() -> Vec<CommandInfo> {
    use clap::CommandFactory;
    let cmd = crate::cli::Cli::command();
    let mut out = Vec::new();
    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        if name == "help" {
            continue;
        }
        let mut paths = Vec::new();
        if sub.has_subcommands() {
            for s in sub.get_subcommands() {
                if s.get_name() != "help" {
                    paths.push(format!("{name} {}", s.get_name()));
                }
            }
        } else {
            paths.push(name.to_owned());
        }
        for p in paths {
            out.push(CommandInfo {
                formats: crate::cli::formats::formats_of(&p).to_vec(),
                command: p,
            });
        }
    }
    out
}

fn short_exit(code: u8) -> &'static str {
    match code {
        exit::OK => "ok",
        exit::RUNTIME => "runtime",
        exit::USAGE => "usage",
        exit::GATE => "gate",
        exit::BUDGET => "budget",
        exit::SAFETY => "safety",
        _ => "interrupted",
    }
}

impl CommandOutput for Capabilities {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![
            crate::cli::next::discover(),
            crate::output::NextStep::new("the JSON Schemas of every output", "oprobe schema"),
        ]
    }

    fn text(&self) -> String {
        let mut o = format!("{} {}\n", bold("oprobe"), self.oprobe_version);
        let join = |v: Vec<String>| v.join(", ");
        o += &format!(
            "  {:<12} {}\n",
            "services",
            join(self.services.iter().map(|s| s.scheme.clone()).collect())
        );
        o += &format!(
            "  {:<12} {}\n",
            "presets",
            join(self.presets.iter().map(|p| p.name.clone()).collect())
        );
        o += &format!(
            "  {:<12} {} {}\n",
            "providers",
            join(self.providers.iter().map(|p| p.key.clone()).collect()),
            dim(&format!("(list prices as of {})", self.pricing_as_of))
        );
        o += &format!(
            "  {:<12} {}\n",
            "transports",
            join(self.transports.iter().map(|t| t.name.clone()).collect())
        );
        o += &format!("  {:<12} {}\n", "outputs", self.output_formats.join(", "));
        o += &format!(
            "  {:<12} {}\n",
            "exit codes",
            join(
                self.exit_codes
                    .iter()
                    .map(|e| format!("{} {}", e.code, short_exit(e.code)))
                    .collect()
            )
        );
        o += &format!(
            "  {:<12} {} codes {}\n",
            "errors",
            self.error_codes.len(),
            dim("(oprobe capabilities -o json)")
        );
        o += &format!(
            "  {:<12} {} codes {}\n",
            "findings",
            self.finding_codes.len(),
            dim("(oprobe explain --list)")
        );
        o += &format!(
            "  {:<12} {}\n",
            "mcp tools",
            join(self.mcp.tools.iter().map(|t| t.name.clone()).collect())
        );
        o += &format!(
            "  {:<12} {} {}\n",
            "schemas",
            self.schemas.len(),
            dim("(oprobe schema)")
        );
        o += &format!(
            "  {:<12} preset {} · {:.0}s window · {} workers · trace cap {}\n",
            "defaults",
            self.defaults.preset,
            self.defaults.duration_s,
            self.defaults.knobs.concurrency_count,
            self.defaults.trace_max_requests_count
        );
        o
    }
}
