//! The Add Target sheet's values: a name, the storage URL (with its bucket), region,
//! endpoint and the credentials source — validated as `oprobe target add` validates them and
//! saved through the config file API (`[targets.NAME]`).

use crate::store::{ConfigFile, TargetEntry};
use crate::ui::model::cloud::Detected;

/// One credentials source the sheet offers: the value saved (`""` = the provider's default
/// chain, `env`, `profile:NAME`, `anonymous`) and what it is called. Names only, no secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredChoice {
    pub value: String,
    pub title: String,
}

/// The credentials sources to offer: the default chain, those found on this machine, and
/// anonymous access.
pub fn cred_choices(found: &[Detected]) -> Vec<CredChoice> {
    let mut out = vec![CredChoice {
        value: String::new(),
        title: "Default credential chain".into(),
    }];
    for d in found {
        let value = d.credentials.clone().unwrap_or_default();
        if !out.iter().any(|c| c.value == value) {
            out.push(CredChoice {
                value,
                title: d.label.clone(),
            });
        }
    }
    out.push(CredChoice {
        value: "anonymous".into(),
        title: "Anonymous (public bucket)".into(),
    });
    out
}

/// What the user typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetDraft {
    pub name: String,
    pub url: String,
    pub region: String,
    pub endpoint: String,
    /// See [`CredChoice::value`].
    pub credentials: String,
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

impl TargetDraft {
    /// Start from a credentials source found on this machine: its scheme (the bucket is left
    /// for the user — a source has no bucket), credentials and region.
    pub fn from_detected(d: &Detected) -> Self {
        Self {
            url: d.scheme.to_owned(),
            region: d.region.clone().unwrap_or_default(),
            credentials: d.credentials.clone().unwrap_or_default(),
            ..Default::default()
        }
    }

    /// A name for the target from its URL: the bucket (`s3://logs/x` → `logs`).
    pub fn suggested_name(&self) -> String {
        let rest = self
            .url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or_default();
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        host.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .take(64)
            .collect()
    }

    /// The config entry, or what to fix (the bucket of a bucket-less URL first).
    pub fn entry(&self) -> Result<(String, TargetEntry), String> {
        let url = self.url.trim();
        if url.is_empty() {
            return Err("Enter a storage URL, e.g. s3://my-bucket/oprobe.".into());
        }
        if let Err(e) = crate::input::storage_url(url) {
            return Err(match e.hint {
                Some(h) => format!("{} — {h}", e.message),
                None => e.message,
            });
        }
        let name = opt(&self.name).unwrap_or_else(|| self.suggested_name());
        ConfigFile::check_target_name(&name).map_err(|e| format!("{e:#}"))?;
        let entry = TargetEntry {
            url: url.to_owned(),
            region: opt(&self.region),
            endpoint: opt(&self.endpoint),
            credentials: opt(&self.credentials),
            options: Default::default(),
        };
        entry.validate().map_err(|e| format!("{e:#}"))?;
        Ok((name, entry))
    }

    /// Add the target to `cfg` (`replace` overwrites one of the same name).
    pub fn add_to(&self, cfg: &mut ConfigFile, replace: bool) -> Result<String, String> {
        let (name, entry) = self.entry()?;
        if cfg.targets.contains_key(&name) && !replace {
            return Err(format!(
                "a target named `{name}` already exists: choose another name"
            ));
        }
        cfg.targets.insert(name.clone(), entry);
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_without_a_bucket_asks_for_one() {
        let d = Detected {
            provider: "aws",
            label: "AWS keys in the environment".into(),
            scheme: "s3://",
            credentials: Some("env".into()),
            region: Some("eu-west-1".into()),
        };
        let mut t = TargetDraft::from_detected(&d);
        assert_eq!(t.credentials, "env");
        let e = t.entry().unwrap_err();
        assert!(e.contains("bucket"), "{e}");
        t.url = "s3://logs/bench".into();
        let (name, entry) = t.entry().unwrap();
        assert_eq!(name, "logs");
        assert_eq!(entry.region.as_deref(), Some("eu-west-1"));
        assert_eq!(entry.credentials.as_deref(), Some("env"));
        let mut cfg = ConfigFile::default();
        assert_eq!(t.add_to(&mut cfg, false).unwrap(), "logs");
        assert!(t.add_to(&mut cfg, false).is_err());
        t.name = "bad name".into();
        assert!(t.entry().is_err());
    }

    #[test]
    fn choices_are_names_only_and_deduplicated() {
        let d = |label: &str, cred: Option<&str>| Detected {
            provider: "aws",
            label: label.into(),
            scheme: "s3://",
            credentials: cred.map(Into::into),
            region: None,
        };
        let c = cred_choices(&[
            d("AWS keys in the environment", Some("env")),
            d("Azure", Some("env")),
            d("AWS profile “dev”", Some("profile:dev")),
        ]);
        let values: Vec<&str> = c.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["", "env", "profile:dev", "anonymous"]);
    }
}
