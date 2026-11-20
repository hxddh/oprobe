//! Guided first run: `oprobe run` without a target in an interactive terminal.
//!
//! Offers the demo, the cloud credentials found on this machine (AWS profiles from
//! `~/.aws/config` / `~/.aws/credentials`, Google application default credentials, Azure
//! storage environment variables) or a storage URL, then saves the choice as a named target
//! (`[targets.NAME]` in the config file) so the next run is just `oprobe run NAME`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::Ctx;
use crate::output::OutputFormat;
use crate::report::style::{bold, dim, green};
use crate::store::{ConfigFile, TargetEntry};

/// What the person picked.
pub enum Choice {
    Demo,
    /// A saved target name to run now.
    Target(String),
    /// A target was saved; nothing to measure now.
    Saved,
}

/// Whether to offer the guided choice: an interactive terminal and text output.
pub fn should_offer(ctx: &Ctx) -> bool {
    !ctx.out.non_interactive && !ctx.out.quiet && ctx.out.format == OutputFormat::Text
}

/// Credentials found on this machine.
#[derive(Clone, Debug, PartialEq)]
pub enum Detected {
    AwsProfile {
        name: String,
        region: Option<String>,
        endpoint: Option<String>,
    },
    /// Google application default credentials (`source` = where they were found).
    Gcp { source: String },
    /// `AZURE_STORAGE_ACCOUNT` (and a key, SAS or connection string) in the environment.
    Azure { account: String },
}

impl Detected {
    fn describe(&self) -> String {
        match self {
            Detected::AwsProfile { name, region, .. } => format!(
                "Amazon S3 with AWS profile `{name}`{}",
                region
                    .as_ref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ),
            Detected::Gcp { .. } => {
                "Google Cloud Storage with your application default credentials".into()
            }
            Detected::Azure { account } => format!("Azure Blob Storage, account `{account}`"),
        }
    }
}

/// `(profile, region, endpoint_url)` of every profile in an AWS config (`credentials = false`:
/// `[default]` / `[profile NAME]`) or credentials file (`[NAME]`).
pub fn parse_aws_profiles(
    text: &str,
    credentials: bool,
) -> Vec<(String, Option<String>, Option<String>)> {
    let mut out: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    let mut cur: Option<usize> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if let Some(sec) = l.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            let sec = sec.trim();
            let name = if credentials {
                Some(sec)
            } else if sec == "default" {
                Some("default")
            } else {
                sec.strip_prefix("profile ").map(str::trim)
            };
            cur = name.filter(|n| !n.is_empty()).map(|n| {
                out.push((n.to_owned(), None, None));
                out.len() - 1
            });
            continue;
        }
        let (Some(i), Some((k, v))) = (cur, l.split_once('=')) else {
            continue;
        };
        let v = v.trim().to_owned();
        match k.trim() {
            "region" if !v.is_empty() => out[i].1 = Some(v),
            "endpoint_url" if !v.is_empty() => out[i].2 = Some(v),
            _ => {}
        }
    }
    out
}

/// Look for cloud credentials: `env` reads an environment variable, `home` is the home
/// directory.
pub fn detect(env: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Vec<Detected> {
    let mut found = Vec::new();
    let file = |var: &str, rel: &str| -> Option<PathBuf> {
        env(var)
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(rel)))
    };
    let mut aws: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    for (path, creds) in [
        (file("AWS_CONFIG_FILE", ".aws/config"), false),
        (
            file("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials"),
            true,
        ),
    ] {
        let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
            continue;
        };
        for (name, region, endpoint) in parse_aws_profiles(&text, creds) {
            match aws.iter_mut().find(|p| p.0 == name) {
                Some(p) => {
                    p.1 = p.1.take().or(region);
                    p.2 = p.2.take().or(endpoint);
                }
                None => aws.push((name, region, endpoint)),
            }
        }
    }
    // `default` first, then the files' order.
    aws.sort_by_key(|p| p.0 != "default");
    found.extend(
        aws.into_iter()
            .map(|(name, region, endpoint)| Detected::AwsProfile {
                name,
                region,
                endpoint,
            }),
    );
    let adc = env("GOOGLE_APPLICATION_CREDENTIALS")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            [
                home.map(|h| h.join(".config/gcloud/application_default_credentials.json")),
                env("APPDATA")
                    .map(|a| PathBuf::from(a).join("gcloud/application_default_credentials.json")),
            ]
            .into_iter()
            .flatten()
            .find(|p| p.is_file())
        });
    if let Some(p) = adc {
        found.push(Detected::Gcp {
            source: p.display().to_string(),
        });
    }
    let account = env("AZURE_STORAGE_ACCOUNT").or_else(|| {
        env("AZURE_STORAGE_CONNECTION_STRING").and_then(|c| {
            c.split(';')
                .find_map(|kv| kv.trim().strip_prefix("AccountName=").map(str::to_owned))
        })
    });
    if let Some(account) = account.filter(|a| !a.is_empty()) {
        found.push(Detected::Azure { account });
    }
    found
}

/// The outcome of the guided dialog.
#[derive(Debug, PartialEq)]
pub enum Flow {
    Demo,
    /// Save `entry` as `name` (and make it the default target when `default`); run it when
    /// `run_now`.
    Save {
        name: String,
        entry: TargetEntry,
        default: bool,
        run_now: bool,
    },
}

struct Io<'a> {
    input: &'a mut dyn BufRead,
    out: &'a mut dyn Write,
}

impl Io<'_> {
    fn ask(&mut self, prompt: &str, default: &str) -> Result<String> {
        if default.is_empty() {
            write!(self.out, "{prompt}: ")?;
        } else {
            write!(self.out, "{prompt} {}: ", dim(&format!("[{default}]")))?;
        }
        self.out.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            anyhow::bail!("no answer (end of input)");
        }
        let v = line.trim();
        Ok(if v.is_empty() { default } else { v }.to_owned())
    }

    fn yes(&mut self, prompt: &str, default: bool) -> Result<bool> {
        let a = self.ask(prompt, if default { "Y/n" } else { "y/N" })?;
        Ok(match a.to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        })
    }

    /// Ask until `check` accepts the answer (three tries).
    fn ask_valid<T>(
        &mut self,
        prompt: &str,
        default: &str,
        check: impl Fn(&str) -> Result<T, String>,
    ) -> Result<T> {
        for _ in 0..3 {
            let a = self.ask(prompt, default)?;
            match check(&a) {
                Ok(v) => return Ok(v),
                Err(e) => writeln!(self.out, "  {e}")?,
            }
        }
        anyhow::bail!("no valid answer; nothing was saved")
    }
}

fn bucket_path(v: &str) -> Result<String, String> {
    let v = v
        .trim()
        .trim_start_matches("s3://")
        .trim_start_matches("gs://")
        .trim_start_matches("gcs://")
        .trim_start_matches("azblob://")
        .trim_matches('/');
    if v.is_empty() || v.contains(char::is_whitespace) {
        Err("enter a bucket name, optionally with a prefix: my-bucket/oprobe".into())
    } else {
        Ok(v.to_owned())
    }
}

/// A target name derived from a URL's bucket (`s3://my-bucket/x` → `my-bucket`).
fn suggest_name(url: &str, cfg: &ConfigFile) -> String {
    let base: String = url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(40)
        .collect();
    let base = if base.is_empty() {
        "default".to_owned()
    } else {
        base
    };
    if !cfg.targets.contains_key(&base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !cfg.targets.contains_key(n))
        .unwrap_or(base)
}

/// The dialog itself, on any reader/writer (tested without a terminal).
pub fn dialog(
    detected: &[Detected],
    cfg: &ConfigFile,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<Flow> {
    let mut io = Io { input, out };
    writeln!(
        io.out,
        "\n{}\nNo target given and none configured yet. Pick a starting point:\n",
        bold("Welcome to oprobe.")
    )?;
    writeln!(
        io.out,
        "  1) Demo: a local lab with throttling and stragglers (no account, ~15 s)"
    )?;
    for (i, d) in detected.iter().enumerate() {
        writeln!(io.out, "  {}) {}", i + 2, d.describe())?;
    }
    let url_choice = detected.len() + 2;
    writeln!(
        io.out,
        "  {url_choice}) Enter a storage URL (s3://, gs://, azblob://, oss://, cos://, b2://, fs://)"
    )?;
    if detected.is_empty() {
        writeln!(
            io.out,
            "  {}",
            dim("(no AWS profiles, Google credentials or Azure variables found on this machine)")
        )?;
    }
    writeln!(io.out)?;
    let max = url_choice;
    let pick = io.ask_valid("Choice", "1", |a| match a.parse::<usize>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(format!("enter a number from 1 to {max}")),
    })?;
    if pick == 1 {
        return Ok(Flow::Demo);
    }
    let mut entry = TargetEntry::default();
    if pick == url_choice {
        entry.url = io.ask_valid("Storage URL", "", |a| {
            crate::config::TargetSpec::from_url(a)
                .map(|_| a.to_owned())
                .map_err(|e| format!("{e:#}"))
        })?;
        if entry.url.starts_with("s3") {
            let region = io.ask("Region (empty: from your AWS configuration)", "")?;
            entry.region = Some(region).filter(|r| !r.is_empty());
        }
    } else {
        match &detected[pick - 2] {
            Detected::AwsProfile {
                name,
                region,
                endpoint,
            } => {
                let b = io.ask_valid("S3 bucket (and optional prefix)", "", bucket_path)?;
                entry.url = format!("s3://{b}");
                entry.credentials = Some(format!("profile:{name}"));
                entry.region = match region {
                    Some(r) => Some(r.clone()),
                    None => Some(io.ask("Region", "us-east-1")?),
                };
                entry.endpoint = endpoint.clone();
            }
            Detected::Gcp { .. } => {
                let b = io.ask_valid("GCS bucket (and optional prefix)", "", bucket_path)?;
                entry.url = format!("gcs://{b}");
            }
            Detected::Azure { account } => {
                let c = io.ask_valid("Container (and optional prefix)", "", bucket_path)?;
                entry.url = format!("azblob://{c}");
                entry.endpoint = Some(format!("https://{account}.blob.core.windows.net"));
                entry.credentials = Some("env".into());
            }
        }
    }
    let suggested = suggest_name(&entry.url, cfg);
    let name = io.ask_valid("Name for this target", &suggested, |a| {
        ConfigFile::check_target_name(a)
            .map(|_| a.to_owned())
            .map_err(|e| format!("{e:#}"))
    })?;
    let default = io.yes("Use it when `oprobe run` gets no target?", true)?;
    writeln!(
        io.out,
        "\n{} measuring writes a small test dataset under {}/oprobe-run-<id>/ and deletes it \
         afterwards (read-only alternative: `oprobe run {name} --existing PREFIX`).",
        dim("note:"),
        entry.url.trim_end_matches('/')
    )?;
    let run_now = io.yes("Measure it now?", true)?;
    Ok(Flow::Save {
        name,
        entry,
        default,
        run_now,
    })
}

/// Run the dialog on the terminal (prompts on stderr), save the target and say what to do next.
pub fn choose(ctx: &Ctx) -> Result<Choice> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let detected = detect(&env, dirs::home_dir().as_deref());
    let mut cfg = ctx.config()?.clone();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut err = std::io::stderr();
    match dialog(&detected, &cfg, &mut input, &mut err)? {
        Flow::Demo => Ok(Choice::Demo),
        Flow::Save {
            name,
            entry,
            default,
            run_now,
        } => {
            entry.validate()?;
            cfg.targets.insert(name.clone(), entry);
            if default {
                cfg.defaults.target = Some(name.clone());
            }
            cfg.save(&ctx.paths.config_file)?;
            ctx.out.info(format!(
                "{} saved target {} in {}",
                green("✓"),
                bold(&name),
                ctx.paths.config_file.display()
            ));
            if run_now {
                Ok(Choice::Target(name))
            } else {
                ctx.out.info(format!(
                    "next: {}  ·  {}",
                    bold(&format!("oprobe target test {name}")),
                    bold(&format!("oprobe run {name}"))
                ));
                Ok(Choice::Saved)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "\
[default]
region = eu-west-1
[profile prod]
region = us-east-2
endpoint_url = https://minio.local:9000
[sso-session corp]
sso_region = us-east-1
";

    #[test]
    fn aws_profiles_are_parsed() {
        let p = parse_aws_profiles(CONFIG, false);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], ("default".into(), Some("eu-west-1".into()), None));
        assert_eq!(p[1].0, "prod");
        assert_eq!(p[1].2.as_deref(), Some("https://minio.local:9000"));
        let c = parse_aws_profiles("[dev]\naws_access_key_id = x\n", true);
        assert_eq!(c, vec![("dev".into(), None, None)]);
    }

    #[test]
    fn detects_profiles_adc_and_azure() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".aws")).unwrap();
        std::fs::write(d.path().join(".aws/config"), CONFIG).unwrap();
        std::fs::write(
            d.path().join(".aws/credentials"),
            "[dev]\naws_access_key_id=x\n[prod]\n",
        )
        .unwrap();
        std::fs::create_dir_all(d.path().join(".config/gcloud")).unwrap();
        std::fs::write(
            d.path()
                .join(".config/gcloud/application_default_credentials.json"),
            "{}",
        )
        .unwrap();
        let env = |k: &str| match k {
            "AZURE_STORAGE_CONNECTION_STRING" => {
                Some("DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=k".into())
            }
            _ => None,
        };
        let found = detect(&env, Some(d.path()));
        let names: Vec<String> = found
            .iter()
            .map(|f| match f {
                Detected::AwsProfile { name, .. } => format!("aws:{name}"),
                Detected::Gcp { .. } => "gcp".into(),
                Detected::Azure { account } => format!("azure:{account}"),
            })
            .collect();
        assert_eq!(
            names,
            vec!["aws:default", "aws:prod", "aws:dev", "gcp", "azure:acct"]
        );
        assert!(detect(&|_| None, None).is_empty());
    }

    fn run_dialog(detected: &[Detected], answers: &str) -> (Result<Flow>, String) {
        let mut input = std::io::Cursor::new(answers.as_bytes().to_vec());
        let mut out = Vec::new();
        let r = dialog(detected, &ConfigFile::default(), &mut input, &mut out);
        (r, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn dialog_offers_demo_profiles_and_urls() {
        let det = vec![Detected::AwsProfile {
            name: "prod".into(),
            region: Some("us-east-2".into()),
            endpoint: None,
        }];
        let (r, text) = run_dialog(&det, "\n");
        assert_eq!(r.unwrap(), Flow::Demo);
        assert!(text.contains("AWS profile `prod`"), "{text}");

        let (r, _) = run_dialog(&det, "2\nmy-bucket/bench\n\n\n\n");
        match r.unwrap() {
            Flow::Save {
                name,
                entry,
                default,
                run_now,
            } => {
                assert_eq!(name, "my-bucket");
                assert_eq!(entry.url, "s3://my-bucket/bench");
                assert_eq!(entry.credentials.as_deref(), Some("profile:prod"));
                assert_eq!(entry.region.as_deref(), Some("us-east-2"));
                assert!(default && run_now);
            }
            other => panic!("{other:?}"),
        }

        // URL path: a bad URL is asked again; custom name; no default; not now.
        let (r, text) = run_dialog(&[], "2\nftp://x\nmemory://\nlocal\nn\nn\n");
        assert!(text.contains("unsupported storage scheme"), "{text}");
        match r.unwrap() {
            Flow::Save {
                name,
                entry,
                default,
                run_now,
            } => {
                assert_eq!((name.as_str(), entry.url.as_str()), ("local", "memory://"));
                assert!(!default && !run_now);
            }
            other => panic!("{other:?}"),
        }
        let (r, _) = run_dialog(&[], "");
        assert!(r.is_err(), "end of input");
    }
}
