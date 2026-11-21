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
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![
            crate::cli::next::list_targets(),
            crate::cli::next::discover(),
        ]
    }

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
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![
            crate::cli::next::list_targets(),
            crate::cli::next::discover(),
        ]
    }

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
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![
            crate::cli::next::doctor(&self.name),
            crate::cli::next::plan(&self.name),
        ]
    }

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
    fn next(&self) -> Vec<crate::output::NextStep> {
        match self.targets.first() {
            Some(t) => vec![
                crate::cli::next::doctor(&t.name),
                crate::cli::next::plan(&t.name),
            ],
            None => vec![crate::cli::next::discover()],
        }
    }

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

pub async fn target_add(a: TargetAddArgs, ctx: &Ctx) -> Result<u8> {
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
    let name = a.name.clone();
    let no_test = a.no_test;
    let entry = TargetEntry {
        url: a.url,
        region: a.region,
        endpoint: a.endpoint,
        credentials: a.credentials,
        options,
    };
    entry.validate()?;
    let mut cfg = ctx.config()?.clone();
    if cfg.targets.contains_key(&name) && !a.replace {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("target `{name}` already exists"),
        )
        .with_hint(format!(
            "pass --replace to overwrite it, or remove it first with `oprobe target rm {name}`"
        ))
        .into());
    }
    cfg.targets.insert(a.name.clone(), entry.clone());
    cfg.save(&ctx.paths.config_file)?;
    let text = ctx.out.format == crate::output::OutputFormat::Text;
    if text {
        ctx.out.info(format!(
            "{} saved target {} in {}",
            green("✓"),
            bold(&name),
            ctx.paths.config_file.display()
        ));
    }
    let code = ctx.out.finish(&redacted(&name, &entry))?;
    if no_test {
        return Ok(code);
    }
    if !text {
        ctx.out
            .info(format!("check it with `oprobe target test {name}`"));
        return Ok(code);
    }
    // A quick read-only doctor: reachability, TLS, clock, credentials and listing.
    ctx.out.info(dim(
        "quick check (read-only; `oprobe target test --write-probe` also writes a probe object):",
    ));
    // A fresh context: the config was loaded (and cached) before the target was added.
    let fresh = Ctx::new(ctx.out.clone(), ctx.paths.clone());
    let checked = super::doctor::run(
        DoctorArgs {
            target: TargetArgs {
                target: Some(name.clone()),
                ..Default::default()
            },
            read_only: true,
            ..Default::default()
        },
        &fresh,
    )
    .await;
    match checked {
        Ok(0) => Ok(code),
        Ok(c) => {
            ctx.out.warn(format!(
                "the target is saved, but the check failed: fix it, then `oprobe target test {name}`"
            ));
            Ok(c)
        }
        Err(e) => {
            ctx.out.warn(format!(
                "the target is saved, but the check failed: fix it, then `oprobe target test {name}`"
            ));
            Err(e)
        }
    }
}

/// `oprobe target test NAME`: the doctor on a configured target (or a URL).
pub async fn target_test(a: TargetTestArgs, ctx: &Ctx) -> Result<u8> {
    super::doctor::run(
        DoctorArgs {
            target: TargetArgs {
                target: Some(a.name),
                ..Default::default()
            },
            read_only: a.read_only,
            write_probe: a.write_probe,
            verbose: false,
        },
        ctx,
    )
    .await
}

/// A commented starting point for a new config file.
pub const CONFIG_TEMPLATE: &str = r#"# oprobe configuration (docs: https://github.com/hxddh/oprobe/blob/main/docs/concepts.md)
# Precedence: command-line flags > OPROBE_* environment > target > [defaults] > built-in.
# `oprobe config show` prints every effective value and where it comes from.

[defaults]
# output = "text"          # text | json | ndjson | md | html
# target = "prod"          # used when `oprobe run` is given no target
# preset = "parquet"       # parquet | checkpoint | small-files | metadata | mixed
# duration = "10s"
# concurrency = 16
# max_requests = 1000000   # trace rows kept per run (sampled beyond)

# [targets.prod]
# url = "s3://my-bucket/oprobe"
# region = "eu-west-1"
# credentials = "profile:prod"   # env | profile:NAME | anonymous (default: provider chain)
# options = { enable_virtual_host_style = "true" }
"#;

/// Result of `oprobe config edit`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConfigEdited {
    pub config_file: String,
    pub created: bool,
    pub targets: Vec<String>,
}

impl CommandOutput for ConfigEdited {
    fn next(&self) -> Vec<crate::output::NextStep> {
        vec![crate::cli::next::list_targets()]
    }

    fn text(&self) -> String {
        format!(
            "{} {} is valid{}\n",
            green("✓"),
            self.config_file,
            if self.targets.is_empty() {
                String::new()
            } else {
                format!(" · targets: {}", self.targets.join(", "))
            }
        )
    }
}

/// The editor command: `$VISUAL`, `$EDITOR`, else the platform's default.
pub fn editor_command(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let raw = env("VISUAL")
        .or_else(|| env("EDITOR"))
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    raw.split_whitespace().map(str::to_owned).collect()
}

pub fn config_edit(ctx: &Ctx) -> Result<u8> {
    let path = &ctx.paths.config_file;
    if ctx.out.non_interactive {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            "`oprobe config edit` needs an interactive terminal",
        )
        .with_hint(format!("edit {} directly", path.display()))
        .into());
    }
    let created = !path.exists();
    if created {
        crate::store::atomic_write(path, CONFIG_TEMPLATE.as_bytes())?;
    }
    let env = |k: &str| std::env::var(k).ok();
    let cmd = editor_command(&env);
    let status = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .arg(path)
        .status()
        .map_err(|e| {
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("cannot start the editor `{}`: {e}", cmd.join(" ")),
            )
            .with_hint("set $EDITOR (e.g. `export EDITOR=nano`)")
        })?;
    if !status.success() {
        ctx.out.warn(format!(
            "the editor exited with {status}; checking the file anyway"
        ));
    }
    let cfg = ConfigFile::load(path).map_err(|e| {
        OprobeError::new(ErrorCode::ConfigInvalid, format!("{e:#}"))
            .with_hint("fix it with `oprobe config edit` (the file was kept as you saved it)")
    })?;
    ctx.out.finish(&ConfigEdited {
        config_file: path.display().to_string(),
        created,
        targets: cfg.targets.keys().cloned().collect(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_a_valid_config() {
        let c: ConfigFile = toml::from_str(CONFIG_TEMPLATE).unwrap();
        assert_eq!(c, ConfigFile::default());
    }

    #[test]
    fn editor_prefers_visual_then_editor() {
        let env = |k: &str| match k {
            "VISUAL" => Some("code -w".to_owned()),
            "EDITOR" => Some("nano".to_owned()),
            _ => None,
        };
        assert_eq!(editor_command(&env), vec!["code", "-w"]);
        let env = |k: &str| (k == "EDITOR").then(|| "nano".to_owned());
        assert_eq!(editor_command(&env), vec!["nano"]);
        assert!(!editor_command(&|_| None).is_empty());
    }
}
