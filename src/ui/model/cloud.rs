//! "Connect my bucket": which cloud credentials this machine already has (environment
//! variables and the providers' own config files), so onboarding can offer them by name.
//! Only names are read — never a secret's value.

use std::path::{Path, PathBuf};

/// A credential source found on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    /// `aws`, `gcp` or `azure`.
    pub provider: &'static str,
    /// What to show: `AWS profile "dev"`, `AWS environment keys`, ….
    pub label: String,
    /// URL scheme to prefill (`s3://`, `gs://`, `azblob://`).
    pub scheme: &'static str,
    /// Value for the target's `credentials` setting (`profile:dev`, `env`), when it has one.
    pub credentials: Option<String>,
    /// Region found next to it, if any.
    pub region: Option<String>,
}

/// Look at `env` (a lookup) and the files under `home`.
pub fn detect_with(env: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Vec<Detected> {
    let mut out = Vec::new();
    let region = env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION"));
    if env("AWS_ACCESS_KEY_ID").is_some() {
        out.push(Detected {
            provider: "aws",
            label: "AWS keys in the environment".into(),
            scheme: "s3://",
            credentials: Some("env".into()),
            region: region.clone(),
        });
    }
    let mut profiles: Vec<(String, Option<String>)> = Vec::new();
    if let Some(h) = home {
        let cfg = env("AWS_CONFIG_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| h.join(".aws/config"));
        let creds = env("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| h.join(".aws/credentials"));
        for (path, prefixed) in [(cfg, true), (creds, false)] {
            if let Ok(text) = std::fs::read_to_string(&path) {
                for (name, reg) in ini_sections(&text, prefixed) {
                    match profiles.iter_mut().find(|(n, _)| *n == name) {
                        Some(p) => {
                            if p.1.is_none() {
                                p.1 = reg;
                            }
                        }
                        None => profiles.push((name, reg)),
                    }
                }
            }
        }
    }
    let active = env("AWS_PROFILE");
    profiles.sort_by_key(|(n, _)| (Some(n) != active.as_ref(), n != "default", n.clone()));
    for (name, reg) in profiles.into_iter().take(6) {
        out.push(Detected {
            provider: "aws",
            label: format!("AWS profile “{name}”"),
            scheme: "s3://",
            credentials: Some(format!("profile:{name}")),
            region: reg.or_else(|| region.clone()),
        });
    }
    let gcp_file = env("GOOGLE_APPLICATION_CREDENTIALS")
        .map(PathBuf::from)
        .or_else(|| {
            let base = env("CLOUDSDK_CONFIG")
                .map(PathBuf::from)
                .or_else(|| home.map(|h| h.join(".config/gcloud")))?;
            let p = base.join("application_default_credentials.json");
            p.exists().then_some(p)
        });
    if gcp_file.is_some_and(|p| p.exists()) {
        out.push(Detected {
            provider: "gcp",
            label: "Google application default credentials".into(),
            scheme: "gs://",
            credentials: None,
            region: None,
        });
    }
    if env("AZURE_STORAGE_CONNECTION_STRING").is_some()
        || (env("AZURE_STORAGE_ACCOUNT").is_some() && env("AZURE_STORAGE_KEY").is_some())
    {
        out.push(Detected {
            provider: "azure",
            label: match env("AZURE_STORAGE_ACCOUNT") {
                Some(a) => format!("Azure storage account “{a}”"),
                None => "Azure storage connection string".into(),
            },
            scheme: "azblob://",
            credentials: Some("env".into()),
            region: None,
        });
    }
    out
}

/// This machine's credential sources.
pub fn detect() -> Vec<Detected> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    detect_with(
        &|k| std::env::var(k).ok().filter(|v| !v.is_empty()),
        home.as_deref(),
    )
}

/// `[name]` / `[profile name]` sections of an AWS ini file, with their `region`.
fn ini_sections(text: &str, prefixed: bool) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(sec) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let sec = sec.trim();
            let name = if prefixed {
                match sec.strip_prefix("profile ") {
                    Some(n) => n.trim(),
                    None if sec == "default" => "default",
                    None => continue,
                }
            } else {
                sec
            };
            out.push((name.to_owned(), None));
        } else if let Some((k, v)) = line.split_once('=')
            && k.trim() == "region"
            && let Some(last) = out.last_mut()
        {
            last.1 = Some(v.trim().to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_profiles_env_keys_and_other_clouds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".aws")).unwrap();
        std::fs::write(
            dir.path().join(".aws/config"),
            "[default]\nregion = eu-west-1\n[profile dev]\nregion=us-east-2\n[sso-session x]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".aws/credentials"),
            "[dev]\naws_access_key_id=AKIA\n",
        )
        .unwrap();
        let env = |k: &str| match k {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_owned()),
            "AWS_PROFILE" => Some("dev".to_owned()),
            "AZURE_STORAGE_CONNECTION_STRING" => Some("x".to_owned()),
            _ => None,
        };
        let found = detect_with(&env, Some(dir.path()));
        let labels: Vec<&str> = found.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels[0], "AWS keys in the environment");
        assert_eq!(labels[1], "AWS profile “dev”");
        assert_eq!(found[1].region.as_deref(), Some("us-east-2"));
        assert_eq!(found[1].credentials.as_deref(), Some("profile:dev"));
        assert_eq!(labels[2], "AWS profile “default”");
        assert_eq!(found.last().unwrap().provider, "azure");
        assert_eq!(found.len(), 4);
    }
}
