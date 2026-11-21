//! `oprobe target discover` / MCP `discover_targets` (design v4 §9, AG-9): the cloud
//! credential sources on this machine — AWS profiles and environment variables, Google
//! application default credentials, Azure storage environment variables — by **name only**
//! (never a key, token or secret), with the configured targets and the command that adds a
//! target for each source. The guided TTY dialog (`cli/onboard.rs`) and `oprobe` without
//! arguments off a terminal use the same detection.

use std::path::Path;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use super::Ctx;
use super::onboard::{self, Detected};
use crate::output::{CommandOutput, NextStep};
use crate::report::style::{bold, dim};

/// Which cloud a credential source is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cloud {
    Aws,
    Gcp,
    Azure,
}

/// One credential source, by name only.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CredentialSource {
    pub cloud: Cloud,
    /// `aws_profile`, `aws_environment`, `aws_web_identity`, `gcp_adc`, `gcp_environment`,
    /// `azure_environment`.
    pub kind: String,
    /// The profile, account or variable set it is known by (never a secret).
    pub name: String,
    /// Where it was found: a file path or environment variable names.
    pub source: String,
    /// The region configured with it, when one is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The endpoint configured with it (S3-compatible services), when one is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The URL scheme it serves (`s3`, `gcs`, `azblob`).
    pub scheme: String,
    /// `oprobe target add` for a bucket of this source (replace BUCKET).
    pub add_command: String,
}

/// A configured target.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct KnownTarget {
    pub name: String,
    pub url: String,
}

/// Result of `oprobe target discover` (MCP `discover_targets`).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Discovery {
    /// Credential sources found, AWS profiles first (`default` first).
    pub sources: Vec<CredentialSource>,
    /// Targets already in the config file.
    pub targets: Vec<KnownTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_target: Option<String>,
    /// The config file the targets come from.
    pub config_file: String,
}

fn env_names(env: &dyn Fn(&str) -> Option<String>, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|n| env(n).is_some())
        .map(|n| (*n).to_owned())
        .collect()
}

/// Every credential source visible through `env` and `home`.
pub fn sources(env: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Vec<CredentialSource> {
    let mut out = Vec::new();
    let aws_region = env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION"));
    // Environment keys win over profiles in the AWS chain: list them first.
    let keys = env_names(
        env,
        &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ],
    );
    if keys.iter().any(|k| k == "AWS_ACCESS_KEY_ID") {
        let endpoint = env("AWS_ENDPOINT_URL_S3").or_else(|| env("AWS_ENDPOINT_URL"));
        out.push(CredentialSource {
            cloud: Cloud::Aws,
            kind: "aws_environment".into(),
            name: "environment".into(),
            source: keys.join(", "),
            region: aws_region.clone(),
            add_command: add_command(
                "aws",
                "s3",
                aws_region.as_deref(),
                endpoint.as_deref(),
                None,
            ),
            endpoint,
            scheme: "s3".into(),
        });
    }
    if env("AWS_WEB_IDENTITY_TOKEN_FILE").is_some() {
        out.push(CredentialSource {
            cloud: Cloud::Aws,
            kind: "aws_web_identity".into(),
            name: env("AWS_ROLE_ARN").unwrap_or_else(|| "web identity".into()),
            source: "AWS_WEB_IDENTITY_TOKEN_FILE".into(),
            region: aws_region.clone(),
            endpoint: None,
            scheme: "s3".into(),
            add_command: add_command("aws", "s3", aws_region.as_deref(), None, None),
        });
    }
    for d in onboard::detect(env, home) {
        match d {
            Detected::AwsProfile {
                name,
                region,
                endpoint,
            } => out.push(CredentialSource {
                cloud: Cloud::Aws,
                kind: "aws_profile".into(),
                add_command: add_command(
                    &name,
                    "s3",
                    region.as_deref(),
                    endpoint.as_deref(),
                    Some(&name),
                ),
                source: env("AWS_CONFIG_FILE")
                    .unwrap_or_else(|| "~/.aws/config, ~/.aws/credentials".into()),
                name,
                region,
                endpoint,
                scheme: "s3".into(),
            }),
            Detected::Gcp { source } => out.push(CredentialSource {
                cloud: Cloud::Gcp,
                kind: if env("GOOGLE_APPLICATION_CREDENTIALS").is_some() {
                    "gcp_environment".into()
                } else {
                    "gcp_adc".into()
                },
                name: "application default credentials".into(),
                source,
                region: None,
                endpoint: None,
                scheme: "gcs".into(),
                add_command: add_command("gcs", "gcs", None, None, None),
            }),
            Detected::Azure { account } => {
                let vars = env_names(
                    env,
                    &[
                        "AZURE_STORAGE_ACCOUNT",
                        "AZURE_STORAGE_KEY",
                        "AZURE_STORAGE_ACCOUNT_KEY",
                        "AZURE_STORAGE_SAS_TOKEN",
                        "AZURE_STORAGE_CONNECTION_STRING",
                    ],
                );
                out.push(CredentialSource {
                    cloud: Cloud::Azure,
                    kind: "azure_environment".into(),
                    add_command: format!(
                        "oprobe target add azure azblob://CONTAINER --option account_name={account}"
                    ),
                    name: account,
                    source: vars.join(", "),
                    region: None,
                    endpoint: None,
                    scheme: "azblob".into(),
                })
            }
        }
    }
    out
}

fn add_command(
    name: &str,
    scheme: &str,
    region: Option<&str>,
    endpoint: Option<&str>,
    profile: Option<&str>,
) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut c = format!("oprobe target add {name} {scheme}://BUCKET/oprobe");
    if let Some(r) = region {
        c += &format!(" --region {r}");
    }
    if let Some(e) = endpoint {
        c += &format!(" --endpoint {e}");
    }
    if let Some(p) = profile {
        c += &format!(" --credentials profile:{p}");
    }
    c
}

/// Discover on this machine (environment and home directory) and read the config file.
pub fn discover(ctx: &Ctx) -> Result<Discovery> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let cfg = ctx.config()?;
    Ok(Discovery {
        sources: sources(&env, dirs::home_dir().as_deref()),
        targets: cfg
            .targets
            .iter()
            .map(|(n, t)| KnownTarget {
                name: n.clone(),
                url: t.url.clone(),
            })
            .collect(),
        default_target: cfg.defaults.target.clone(),
        config_file: ctx.paths.config_file.display().to_string(),
    })
}

impl CommandOutput for Discovery {
    fn text(&self) -> String {
        let mut o = format!("{}\n", bold("credential sources on this machine"));
        if self.sources.is_empty() {
            o += "  none found (AWS profiles or environment, GCP application default credentials, \
                  Azure storage environment)\n";
        }
        for s in &self.sources {
            o += &format!(
                "  {:<6} {:<18} {}{}\n",
                format!("{:?}", s.cloud).to_ascii_lowercase(),
                s.name,
                dim(&s.source),
                s.region
                    .as_deref()
                    .map(|r| dim(&format!(" · {r}")))
                    .unwrap_or_default()
            );
            o += &format!("         {}\n", dim(&s.add_command));
        }
        if !self.targets.is_empty() {
            o += &format!("{}\n", bold("configured targets"));
            for t in &self.targets {
                let d = if self.default_target.as_deref() == Some(&t.name) {
                    " (default)"
                } else {
                    ""
                };
                o += &format!("  {:<18} {}{d}\n", t.name, t.url);
            }
        }
        o
    }

    fn markdown(&self) -> String {
        let mut o = String::from("## Credential sources\n\n");
        if self.sources.is_empty() {
            o += "None found.\n";
        } else {
            o += "| Cloud | Name | Found in | Add a target |\n|---|---|---|---|\n";
            for s in &self.sources {
                o += &format!(
                    "| {:?} | {} | {} | `{}` |\n",
                    s.cloud, s.name, s.source, s.add_command
                );
            }
        }
        if !self.targets.is_empty() {
            o += "\n## Configured targets\n\n| Name | URL |\n|---|---|\n";
            for t in &self.targets {
                o += &format!("| {} | `{}` |\n", t.name, t.url);
            }
        }
        o
    }

    fn formats() -> &'static [crate::output::OutputFormat] {
        crate::output::WITH_MD
    }

    fn next(&self) -> Vec<NextStep> {
        let mut next = Vec::new();
        for t in self.targets.iter().take(3) {
            next.push(
                NextStep::new(
                    format!("check target {}", t.name),
                    format!("oprobe doctor {}", t.name),
                )
                .with_mcp("doctor", serde_json::json!({ "target": t.name })),
            );
        }
        if let Some(s) = self.sources.first() {
            next.push(NextStep::new(
                format!("save a target for {} (replace BUCKET)", s.name),
                s.add_command.clone(),
            ));
        }
        next.push(
            NextStep::new(
                "try a local lab target first",
                "oprobe run lab://demo -d 5s",
            )
            .with_mcp(
                "start_run",
                serde_json::json!({ "target": "lab://demo", "duration": "5s" }),
            ),
        );
        next
    }
}

/// `oprobe target discover`.
pub fn cmd_discover(ctx: &Ctx) -> Result<u8> {
    let d = discover(ctx)?;
    ctx.out.finish(&d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_names_never_secrets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".aws")).unwrap();
        std::fs::write(
            dir.path().join(".aws/credentials"),
            "[default]\naws_access_key_id = AKIASECRET\naws_secret_access_key = s3cr3t\n",
        )
        .unwrap();
        let env = |k: &str| match k {
            "AWS_ACCESS_KEY_ID" => Some("AKIAENVSECRET".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some("envsecret".to_owned()),
            "AWS_REGION" => Some("eu-west-1".to_owned()),
            "AZURE_STORAGE_ACCOUNT" => Some("acct".to_owned()),
            "AZURE_STORAGE_KEY" => Some("azsecret".to_owned()),
            _ => None,
        };
        let s = sources(&env, Some(dir.path()));
        let json = serde_json::to_string(&s).unwrap();
        for secret in [
            "AKIASECRET",
            "s3cr3t",
            "AKIAENVSECRET",
            "envsecret",
            "azsecret",
        ] {
            assert!(!json.contains(secret), "{secret} leaked: {json}");
        }
        assert_eq!(s[0].kind, "aws_environment");
        assert_eq!(s[0].source, "AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY");
        assert_eq!(s[0].region.as_deref(), Some("eu-west-1"));
        assert!(
            s.iter()
                .any(|x| x.kind == "aws_profile" && x.name == "default")
        );
        let az = s.iter().find(|x| x.cloud == Cloud::Azure).unwrap();
        assert_eq!(az.name, "acct");
        assert!(az.source.contains("AZURE_STORAGE_KEY"));
        assert!(
            s.iter()
                .find(|x| x.kind == "aws_profile")
                .unwrap()
                .add_command
                .contains("--credentials profile:default")
        );
    }
}
