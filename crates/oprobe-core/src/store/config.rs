//! The config file: `[defaults]` and named operators `[operators.NAME]` (design v6 §8).
//!
//! An `OPERATOR` argument that is neither a URL nor an operator file names an operator of this
//! file. Precedence of every setting: flags > `OPROBE_*` environment > `[defaults]` > built-in.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::{ErrorCode, OprobeError};
use crate::operator::OperatorSpec;

/// Settings applied to every command unless overridden.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// `text` or `json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// `auto`, `always`, `never`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
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
    /// Budgets of every measuring command (`run`, `prove`, the app) unless a flag sets them
    /// (`[defaults.budgets]`).
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
                    crate::input::choices_hint(o, &["text", "json"]),
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
        Ok(())
    }
}

/// `[defaults.budgets]`: the `--max-*` safety flags' defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetDefaults {
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
/// `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub defaults: Defaults,
    /// Named operators (`oprobe run NAME`), in `op.toml` form.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operators: BTreeMap<String, OperatorSpec>,
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
        for (name, op) in &cfg.operators {
            op.validate().map_err(|e| {
                let mut o = OprobeError::new(
                    ErrorCode::ConfigInvalid,
                    format!("operator `{name}` in `{}`: {}", path.display(), e.message),
                );
                o.hint = e.hint;
                o
            })?;
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("cannot serialize the config")?;
        super::atomic_write(path, text.as_bytes())
    }

    /// A configured operator by name.
    pub fn operator(&self, name: &str) -> Option<&OperatorSpec> {
        self.operators.get(name)
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
        let mut op = crate::operator::OperatorSpec::from_url("s3://bucket/prefix");
        op.options.insert("region".into(), "eu-west-1".into());
        op.layers.tail_cut = Some(Default::default());
        c.operators.insert("prod".into(), op);
        c.save(&p).unwrap();
        assert_eq!(ConfigFile::load(&p).unwrap(), c);
        std::fs::write(&p, "[defaults]\nnope = 1\n").unwrap();
        let e = ConfigFile::load(&p).unwrap_err();
        assert_eq!(
            crate::errors::classify(&e, "s3").code,
            ErrorCode::ConfigInvalid
        );
        std::fs::write(&p, "[operators.x]\nurl = \"memory://\"\nbogus = 1\n").unwrap();
        assert!(ConfigFile::load(&p).is_err());
        std::fs::write(
            &p,
            "[operators.x]\nurl = \"memory://\"\n[operators.x.layers]\ntail_cut = { percentile = 100 }\n",
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
}
