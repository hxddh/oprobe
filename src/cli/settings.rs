//! `oprobe config show|path` and `oprobe target add|list|show|rm`.

use std::collections::BTreeMap;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::args::*;
use crate::errors::{ErrorCode, OprobeError};
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green};
use crate::store::config::Setting;
use crate::store::{ConfigFile, TargetEntry};

/// Result of `oprobe config path`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConfigPaths {
    pub config_file: String,
    pub config_exists: bool,
    pub data_dir: String,
    pub runs_dir: String,
}

impl CommandOutput for ConfigPaths {
    fn text(&self) -> String {
        format!(
            "config  {}{}\ndata    {}\nruns    {}\n",
            self.config_file,
            if self.config_exists {
                String::new()
            } else {
                dim(" (not created yet)")
            },
            self.data_dir,
            self.runs_dir
        )
    }
}

pub fn config_path(ctx: &Ctx) -> Result<u8> {
    let p = &ctx.paths;
    ctx.out.finish(&ConfigPaths {
        config_file: p.config_file.display().to_string(),
        config_exists: p.config_file.is_file(),
        data_dir: p.data_dir.display().to_string(),
        runs_dir: p.runs_dir().display().to_string(),
    })
}

/// Result of `oprobe config show`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConfigShow {
    pub config_file: String,
    pub settings: Vec<Setting>,
    pub targets: Vec<String>,
}

impl CommandOutput for ConfigShow {
    fn text(&self) -> String {
        let rows: Vec<Vec<String>> = self
            .settings
            .iter()
            .map(|s| {
                vec![
                    s.key.clone(),
                    if s.value.is_empty() {
                        dim("(none)")
                    } else {
                        s.value.clone()
                    },
                    dim(&format!("{:?}", s.source).to_ascii_lowercase()),
                ]
            })
            .collect();
        let mut o = format!("{} {}\n", bold("config"), self.config_file);
        o += &crate::report::style::table(&["setting", "value", "source"], &rows, 2);
        if !self.targets.is_empty() {
            o += &format!("  targets: {}\n", self.targets.join(", "));
        }
        o
    }
}

pub fn config_show(ctx: &Ctx) -> Result<u8> {
    let cfg = ctx.config()?;
    ctx.out.finish(&ConfigShow {
        config_file: ctx.paths.config_file.display().to_string(),
        settings: cfg.effective(&ctx.flags),
        targets: cfg.targets.keys().cloned().collect(),
    })
}

/// One configured target.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TargetShow {
    pub name: String,
    #[serde(flatten)]
    pub target: TargetEntry,
}

impl CommandOutput for TargetShow {
    fn text(&self) -> String {
        let t = &self.target;
        let mut o = format!("{} {}\n", bold(&self.name), t.url);
        for (k, v) in [
            ("region", &t.region),
            ("endpoint", &t.endpoint),
            ("credentials", &t.credentials),
        ] {
            if let Some(v) = v {
                o += &format!("  {k:<12} {v}\n");
            }
        }
        for (k, v) in crate::util::redact_options(&t.options) {
            o += &format!("  {:<12} {k}={v}\n", "option");
        }
        o
    }
}

/// Result of `oprobe target list`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TargetList {
    pub targets: Vec<TargetShow>,
}

impl CommandOutput for TargetList {
    fn text(&self) -> String {
        if self.targets.is_empty() {
            return "no targets — add one with `oprobe target add NAME URL`\n".into();
        }
        let rows: Vec<Vec<String>> = self
            .targets
            .iter()
            .map(|t| vec![t.name.clone(), t.target.url.clone()])
            .collect();
        crate::report::style::table(&["name", "url"], &rows, 0)
    }
}

/// Redacted copy of a target (secrets in options are masked).
fn redacted(name: &str, t: &TargetEntry) -> TargetShow {
    TargetShow {
        name: name.to_owned(),
        target: TargetEntry {
            options: crate::util::redact_options(&t.options),
            ..t.clone()
        },
    }
}

pub fn target_add(a: TargetAddArgs, ctx: &Ctx) -> Result<u8> {
    ConfigFile::check_target_name(&a.name)?;
    let spec = crate::config::TargetSpec::from_url(&a.url).map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("invalid target URL `{}`: {e:#}", a.url),
        )
    })?;
    let _ = spec;
    let mut options = BTreeMap::new();
    for kv in &a.options {
        let (k, v) = kv.split_once('=').ok_or_else(|| {
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("invalid --option `{kv}` (expected KEY=VALUE)"),
            )
        })?;
        options.insert(k.trim().to_owned(), v.trim().to_owned());
    }
    let entry = TargetEntry {
        url: a.url,
        region: a.region,
        endpoint: a.endpoint,
        credentials: a.credentials,
        options,
    };
    entry.validate()?;
    let mut cfg = ctx.config()?.clone();
    cfg.targets.insert(a.name.clone(), entry.clone());
    cfg.save(&ctx.paths.config_file)?;
    if ctx.out.format == crate::output::OutputFormat::Text {
        ctx.out.info(format!(
            "{} saved target {} in {}",
            green("✓"),
            bold(&a.name),
            ctx.paths.config_file.display()
        ));
    }
    ctx.out.finish(&redacted(&a.name, &entry))
}

pub fn target_list(ctx: &Ctx) -> Result<u8> {
    let cfg = ctx.config()?;
    ctx.out.finish(&TargetList {
        targets: cfg.targets.iter().map(|(n, t)| redacted(n, t)).collect(),
    })
}

fn unknown_target(name: &str) -> anyhow::Error {
    OprobeError::new(
        ErrorCode::UsageUnknownTarget,
        format!("unknown target `{name}`"),
    )
    .with_hint("list targets with `oprobe target list`")
    .into()
}

pub fn target_show(a: TargetNameArg, ctx: &Ctx) -> Result<u8> {
    let cfg = ctx.config()?;
    let t = cfg.target(&a.name).ok_or_else(|| unknown_target(&a.name))?;
    ctx.out.finish(&redacted(&a.name, t))
}

pub fn target_rm(a: TargetNameArg, ctx: &Ctx) -> Result<u8> {
    let mut cfg = ctx.config()?.clone();
    let t = cfg
        .targets
        .remove(&a.name)
        .ok_or_else(|| unknown_target(&a.name))?;
    cfg.save(&ctx.paths.config_file)?;
    ctx.out.finish(&redacted(&a.name, &t))
}
