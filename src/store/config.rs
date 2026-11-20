//! The config file: `[defaults]` and named `[targets.NAME]` (design §3).
//!
//! Precedence of every setting: flags > `OPROBE_*` environment > target > `[defaults]` >
//! built-in. [`ConfigFile::effective`] reports each value with its source (`oprobe config show`).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::{ErrorCode, OprobeError};

/// Settings applied to every command unless overridden.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// `text`, `json`, `ndjson`, `md`, `html`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// `auto`, `always`, `never`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Target used when a command is given none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// e.g. `30s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// Trace rows kept per run (`trace.max_requests`, default 1 000 000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<usize>,
}

/// A named storage endpoint.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetEntry {
    /// Storage URL, e.g. `s3://bucket/prefix`.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `env`, `profile:NAME` or `anonymous` (default: the provider's credential chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
    /// OpenDAL options, validated against the service when the target is used.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

impl TargetEntry {
    /// Validate the credential source.
    pub fn validate(&self) -> Result<()> {
        match self.credentials.as_deref() {
            None | Some("env") | Some("anonymous") => Ok(()),
            Some(p) if p.starts_with("profile:") && p.len() > "profile:".len() => Ok(()),
            Some(other) => Err(OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("invalid credentials `{other}` (expected env, profile:NAME or anonymous)"),
            )
            .into()),
        }
    }
}

/// `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, TargetEntry>,
}

/// Where an effective value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Flag,
    Env,
    Target,
    Defaults,
    Builtin,
}

/// One effective setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub source: Source,
}

impl ConfigFile {
    /// Load `path`; a missing file is an empty config.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read `{}`", path.display()));
            }
        };
        let cfg: Self = toml::from_str(&raw).map_err(|e| {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("invalid config file `{}`: {e}", path.display()),
            )
        })?;
        for (name, t) in &cfg.targets {
            t.validate()
                .with_context(|| format!("target `{name}` in `{}`", path.display()))?;
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("cannot serialize the config")?;
        super::atomic_write(path, text.as_bytes())
    }

    /// A configured target by name.
    pub fn target(&self, name: &str) -> Option<&TargetEntry> {
        self.targets.get(name)
    }

    /// Valid target names: letters, digits, `-`, `_`, `.`; not URL-like.
    pub fn check_target_name(name: &str) -> Result<()> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if ok {
            Ok(())
        } else {
            Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("invalid target name `{name}` (letters, digits, `-`, `_`, `.`)"),
            )
            .into())
        }
    }

    /// Settings with their sources. `flags` are values given on the command line.
    pub fn effective(&self, flags: &BTreeMap<String, String>) -> Vec<Setting> {
        let d = &self.defaults;
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let mut out = Vec::new();
        let mut put = |key: &str, env_key: Option<&str>, def: Option<String>, builtin: &str| {
            let (value, source) = if let Some(v) = flags.get(key) {
                (v.clone(), Source::Flag)
            } else if let Some(v) = env_key.and_then(env) {
                (v, Source::Env)
            } else if let Some(v) = def {
                (v, Source::Defaults)
            } else {
                (builtin.to_owned(), Source::Builtin)
            };
            out.push(Setting {
                key: key.to_owned(),
                value,
                source,
            });
        };
        put("output", Some("OPROBE_OUTPUT"), d.output.clone(), "text");
        put("color", Some("OPROBE_COLOR"), d.color.clone(), "auto");
        put("target", Some("OPROBE_TARGET"), d.target.clone(), "");
        put("preset", None, d.preset.clone(), "parquet");
        put("duration", None, d.duration.clone(), "10s");
        put(
            "concurrency",
            None,
            d.concurrency.map(|c| c.to_string()),
            "16",
        );
        put(
            "max_requests",
            Some("OPROBE_MAX_REQUESTS"),
            d.max_requests.map(|c| c.to_string()),
            &crate::trace::DEFAULT_MAX_REQUESTS.to_string(),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_unknown_keys() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        assert_eq!(ConfigFile::load(&p).unwrap(), ConfigFile::default());
        let mut c = ConfigFile::default();
        c.defaults.output = Some("json".into());
        c.targets.insert(
            "prod".into(),
            TargetEntry {
                url: "s3://bucket/prefix".into(),
                region: Some("eu-west-1".into()),
                credentials: Some("profile:prod".into()),
                ..Default::default()
            },
        );
        c.save(&p).unwrap();
        assert_eq!(ConfigFile::load(&p).unwrap(), c);
        std::fs::write(&p, "[defaults]\nnope = 1\n").unwrap();
        let e = ConfigFile::load(&p).unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::ConfigInvalid
        );
        std::fs::write(
            &p,
            "[targets.x]\nurl = \"memory://\"\ncredentials = \"magic\"\n",
        )
        .unwrap();
        assert!(ConfigFile::load(&p).is_err());
    }

    #[test]
    fn effective_values_name_their_source() {
        let mut c = ConfigFile::default();
        c.defaults.preset = Some("mixed".into());
        let mut flags = BTreeMap::new();
        flags.insert("duration".to_owned(), "5s".to_owned());
        let e = c.effective(&flags);
        let get = |k: &str| e.iter().find(|s| s.key == k).unwrap();
        assert_eq!(get("preset").source, Source::Defaults);
        assert_eq!(get("preset").value, "mixed");
        assert_eq!(get("duration").source, Source::Flag);
        assert_eq!(get("concurrency").source, Source::Builtin);
    }
}
