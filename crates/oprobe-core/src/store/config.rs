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
    /// Budgets of every measuring command (`run`, `tune`, `matrix`, `watch`, `compat`, the
    /// app) unless a flag sets them (`[defaults.budgets]`).
    #[serde(default, skip_serializing_if = "BudgetDefaults::is_empty")]
    pub budgets: BudgetDefaults,
}

impl Defaults {
    /// Check every value with the parsers flags use (`crate::input`).
    pub fn validate(&self) -> std::result::Result<(), OprobeError> {
        if let Some(o) = &self.output
            && crate::output::OutputFormat::parse(o).is_none()
        {
            return Err(
                crate::input::invalid(format!("unknown output `{o}`")).with_hint(
                    crate::input::choices_hint(o, &["text", "json", "ndjson", "md", "html"]),
                ),
            );
        }
        if let Some(c) = &self.color {
            crate::input::choice("color", c, &[("auto", ()), ("always", ()), ("never", ())])?;
        }
        if let Some(p) = &self.preset {
            crate::input::preset(p)?;
        }
        if let Some(d) = &self.duration {
            crate::input::duration(d)?;
        }
        if self.concurrency == Some(0) {
            return Err(crate::input::invalid("concurrency must be at least 1"));
        }
        let b = &self.budgets;
        if let Some(v) = &b.max_bytes {
            crate::input::size(v)?;
        }
        if let Some(v) = &b.max_duration {
            crate::input::duration(v)?;
        }
        if let Some(c) = b.max_cost_usd
            && !(c.is_finite() && c >= 0.0)
        {
            return Err(crate::input::invalid("budgets.max_cost_usd must be >= 0"));
        }
        Ok(())
    }
}

/// `[defaults.budgets]`: the `--max-*` safety flags' defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetDefaults {
    /// Estimated request cost (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Backend requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    /// Bytes transferred, e.g. `10GiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<String>,
    /// Wall time, e.g. `5m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration: Option<String>,
}

impl BudgetDefaults {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Fill the budgets `b` leaves unset from these defaults.
    pub fn apply(&self, b: &mut crate::guard::Budgets) -> Result<()> {
        let bad = |k: &str, e: anyhow::Error| -> anyhow::Error {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("invalid [defaults.budgets] {k}: {e:#}"),
            )
            .into()
        };
        if b.max_cost_usd.is_none() {
            if let Some(c) = self.max_cost_usd
                && !(c.is_finite() && c >= 0.0)
            {
                return Err(bad("max_cost_usd", anyhow::anyhow!("must be >= 0")));
            }
            b.max_cost_usd = self.max_cost_usd;
        }
        if b.max_requests_count.is_none() {
            b.max_requests_count = self.max_requests;
        }
        if b.max_bytes.is_none() {
            b.max_bytes = self
                .max_bytes
                .as_deref()
                .map(|v| v.parse::<crate::util::ByteSize>().map(|x| x.0))
                .transpose()
                .map_err(|e| bad("max_bytes", e))?;
        }
        if b.max_duration_s.is_none() {
            b.max_duration_s = self
                .max_duration
                .as_deref()
                .map(|v| crate::util::parse_duration(v).map(|d| d.as_secs_f64()))
                .transpose()
                .map_err(|e| bad("max_duration", e))?;
        }
        Ok(())
    }
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
        cfg.defaults.validate().map_err(|e| {
            let mut o = OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("invalid [defaults] in `{}`: {}", path.display(), e.message),
            );
            o.hint = e.hint;
            o
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

    /// Secrets written inline in the file (`targets.NAME.options.KEY`, never their values):
    /// option names that hold credentials ([`crate::util::is_secret_key`]) whose value is not
    /// an `${ENV}` reference, and URLs or endpoints with embedded credentials. Callers warn:
    /// `${ENV}` references and `credentials = "profile:NAME"` are the safe way (design v5 §5).
    pub fn inline_secrets(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, t) in &self.targets {
            for (k, v) in &t.options {
                let reference = v.trim().starts_with("${") && v.trim().ends_with('}');
                if crate::util::is_secret_key(k) && !v.is_empty() && !reference {
                    out.push(format!("targets.{name}.options.{k}"));
                }
            }
            for (field, v) in [("url", Some(&t.url)), ("endpoint", t.endpoint.as_ref())] {
                if let Some(v) = v
                    && crate::util::strip_url_credentials(v) != *v
                {
                    out.push(format!("targets.{name}.{field}"));
                }
            }
        }
        out
    }

    /// The warning for [`Self::inline_secrets`], if any.
    pub fn inline_secrets_warning(&self, path: &Path) -> Option<String> {
        let found = self.inline_secrets();
        if found.is_empty() {
            return None;
        }
        Some(format!(
            "{} holds secrets inline ({}): use `${{ENV}}` references or `credentials = \"profile:NAME\"` instead",
            path.display(),
            found.join(", ")
        ))
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
        let b = &d.budgets;
        put(
            "budgets.max_cost_usd",
            None,
            b.max_cost_usd.map(|c| c.to_string()),
            "",
        );
        put(
            "budgets.max_requests",
            None,
            b.max_requests.map(|c| c.to_string()),
            "",
        );
        put("budgets.max_bytes", None, b.max_bytes.clone(), "");
        put("budgets.max_duration", None, b.max_duration.clone(), "");
        out
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn inline_secrets_are_found_but_env_references_are_not() {
        let mut c = ConfigFile::default();
        let mut t = TargetEntry {
            url: "s3://b/p".into(),
            ..Default::default()
        };
        t.options
            .insert("secret_access_key".into(), "${AWS_SECRET}".into());
        t.options.insert("region".into(), "us-east-1".into());
        c.targets.insert("ok".into(), t.clone());
        assert!(c.inline_secrets().is_empty());
        t.options
            .insert("secret_access_key".into(), "hunter2".into());
        t.endpoint = Some("https://user:pw@minio.local".into());
        c.targets.insert("bad".into(), t);
        let found = c.inline_secrets();
        assert_eq!(
            found,
            [
                "targets.bad.options.secret_access_key",
                "targets.bad.endpoint"
            ]
        );
        let w = c.inline_secrets_warning(Path::new("c.toml")).unwrap();
        assert!(!w.contains("hunter2") && w.contains("${ENV}"), "{w}");
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
    fn budget_defaults_fill_unset_flags() {
        let d: ConfigFile = toml::from_str(
            "[defaults.budgets]\nmax_requests = 100\nmax_bytes = \"1MiB\"\nmax_duration = \"2m\"\n",
        )
        .unwrap();
        let mut b = crate::guard::Budgets {
            max_requests_count: Some(5),
            ..Default::default()
        };
        d.defaults.budgets.apply(&mut b).unwrap();
        assert_eq!(b.max_requests_count, Some(5), "flags win");
        assert_eq!(b.max_bytes, Some(1 << 20));
        assert_eq!(b.max_duration_s, Some(120.0));
        assert!(b.max_cost_usd.is_none());
        let bad = BudgetDefaults {
            max_bytes: Some("lots".into()),
            ..Default::default()
        };
        let e = bad.apply(&mut Default::default()).unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::ConfigInvalid
        );
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
