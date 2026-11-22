//! `oprobe self-update`: find the release, download this platform's archive, verify its
//! SHA-256, check that the new binary runs, and replace the running one atomically (Windows:
//! rename the running executable aside, then move the new one in).
//!
//! The release is resolved like the installers do (design v4 §10), falling back on any failure:
//! 1. the release's `latest.json` index (`releases/latest/download/latest.json`; no API, no rate
//!    limit), with each asset's URL, size and SHA-256;
//! 2. the GitHub API (`/repos/…/releases/latest`), checksums from `SHA256SUMS`.
//!
//! A GitHub refusal (403, 429, 5xx) is a network error (`net.unreachable`), never a storage one.
//!
//! After the SHA-256 check the download is verified further (design v5 §5), as the installers do:
//! 1. its GitHub build provenance attestation (`gh attestation verify`), when `gh` is installed
//!    and logged in;
//! 2. otherwise the minisign signature over `SHA256SUMS` (`SHA256SUMS.minisig`) with the public
//!    key embedded at build time (`OPROBE_MINISIGN_PUBKEY`, set by the release workflow);
//! 3. otherwise a warning, or a refusal with `--verify require`.
//!
//! A verification that fails always refuses the update.
//!
//! Installs owned by a package manager (Homebrew, apt/dnf, the macOS .pkg or .app, Scoop,
//! winget, the MSI, cargo) are refused with the manager's own update command, so the manager's
//! records stay true.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::Ctx;
use super::args::{SelfUpdateArgs, VerifyPolicy};
use crate::errors::{ErrorCode, OprobeError};
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green, yellow};

/// GitHub repository the releases come from.
pub const REPO: &str = "hxddh/oprobe";
/// API base; `OPROBE_UPDATE_API` overrides it (mirrors, tests).
const DEFAULT_API: &str = "https://api.github.com";
/// Release download base (`…/latest/download/NAME`, `…/download/TAG/NAME`);
/// `OPROBE_RELEASE_BASE` overrides it (mirrors, tests), as for `install.sh` / `install.ps1`.
const DEFAULT_RELEASE_BASE: &str = "https://github.com/hxddh/oprobe/releases";
/// Largest archive accepted.
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
/// The release workflow that builds and attests every asset (`gh attestation verify
/// --signer-workflow`).
const SIGNER_WORKFLOW: &str = "hxddh/oprobe/.github/workflows/release.yml";
/// The minisign public key of the releases, embedded at build time by the release workflow
/// (repository variable `MINISIGN_PUBKEY`; docs/releasing.md). `OPROBE_MINISIGN_PUBKEY` at run
/// time overrides it (mirrors, tests).
const EMBEDDED_MINISIGN_KEY: Option<&str> = option_env!("OPROBE_MINISIGN_PUBKEY");

/// How a download was verified beyond its SHA-256 (`SHA256SUMS` comes from the same release).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerifiedBy {
    /// The GitHub build provenance attestation of the release workflow (`gh attestation verify`).
    GithubAttestation,
    /// The minisign signature over `SHA256SUMS` with the embedded public key.
    Minisign,
    /// Only the SHA-256 checksum (neither check could run here).
    ChecksumOnly,
}

/// The minisign public key to verify `SHA256SUMS.minisig` with, if any.
fn minisign_key() -> Option<String> {
    std::env::var("OPROBE_MINISIGN_PUBKEY")
        .ok()
        .or_else(|| EMBEDDED_MINISIGN_KEY.map(str::to_owned))
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty())
}

/// Verify the minisign signature `sig` (a `.minisig` file) of `sums` with `pubkey` (base64).
/// Only prehashed (current minisign) signatures are accepted.
pub fn verify_minisign(pubkey: &str, sums: &[u8], sig: &str) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(pubkey.trim())
        .map_err(|e| anyhow::anyhow!("invalid minisign public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig)
        .map_err(|e| anyhow::anyhow!("invalid SHA256SUMS.minisig: {e}"))?;
    pk.verify(sums, &sig, false)
        .map_err(|e| anyhow::anyhow!("the minisign signature of SHA256SUMS does not verify: {e}"))
}

/// `gh` is installed and logged in (so `gh attestation verify` can reach the API).
fn gh_ready() -> bool {
    std::process::Command::new("gh")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `gh attestation verify` on the downloaded archive (written to a temporary file).
fn gh_attestation_verify(name: &str, bytes: &[u8]) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("oprobe-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(name);
    let res = (|| -> Result<std::process::Output> {
        std::fs::write(&file, bytes)?;
        Ok(std::process::Command::new("gh")
            .arg("attestation")
            .arg("verify")
            .arg(&file)
            .args(["--repo", REPO, "--signer-workflow", SIGNER_WORKFLOW])
            .stdin(std::process::Stdio::null())
            .output()?)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    let o = res?;
    if o.status.success() {
        return Ok(());
    }
    Err(OprobeError::new(
        ErrorCode::SafetyRefused,
        format!(
            "the build provenance of {name} does not verify: {}; nothing was installed",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
    )
    .with_hint(format!(
        "download it by hand and check it with `gh attestation verify {name} --repo {REPO}`"
    ))
    .into())
}

/// How this binary was installed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    /// A binary we may replace (install.sh / install.ps1, an extracted archive).
    Standalone,
    Homebrew,
    /// Debian/RPM package (`/usr/bin`).
    SystemPackage,
    /// The macOS installer package (`/usr/local/bin` with a pkgutil receipt).
    MacosPkg,
    /// Inside `oprobe.app` (the disk image).
    MacosApp,
    Scoop,
    Winget,
    /// The Windows installer (Program Files).
    Msi,
    Cargo,
}

impl InstallMethod {
    /// The command (or step) that updates this install.
    pub fn update_instruction(&self) -> &'static str {
        match self {
            Self::Standalone => "oprobe self-update",
            Self::Homebrew => "brew upgrade oprobe   (the app: brew upgrade --cask oprobe-app)",
            Self::SystemPackage => {
                "download the new .deb/.rpm from the release page and install it (sudo apt install ./oprobe_*.deb / sudo dnf install ./oprobe-*.rpm)"
            }
            Self::MacosPkg => "download and open the new oprobe-*-macos-universal.pkg",
            Self::MacosApp => {
                "download the new oprobe-*-macos-universal.dmg and replace oprobe.app"
            }
            Self::Scoop => "scoop update oprobe",
            Self::Winget => "winget upgrade hxddh.oprobe",
            Self::Msi => "download and run the new oprobe-*.msi (or: winget upgrade hxddh.oprobe)",
            Self::Cargo => "cargo install oprobe --locked",
        }
    }
}

/// Classify an install from the executable's path. `exists` checks marker files (receipts).
pub fn install_method(exe: &Path, os: &str, exists: &dyn Fn(&Path) -> bool) -> InstallMethod {
    let p = exe
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if p.contains("/cellar/") || p.contains("/homebrew/") || p.contains("/linuxbrew/") {
        return InstallMethod::Homebrew;
    }
    if p.contains(".app/contents/macos/") {
        return InstallMethod::MacosApp;
    }
    if p.contains("/.cargo/bin/") {
        return InstallMethod::Cargo;
    }
    if p.contains("/scoop/") {
        return InstallMethod::Scoop;
    }
    if p.contains("/microsoft/winget/packages/") || p.contains("/winget/links/") {
        return InstallMethod::Winget;
    }
    match os {
        "windows" if p.contains("/program files") => InstallMethod::Msi,
        "macos"
            if p.starts_with("/usr/local/bin/")
                && exists(Path::new("/var/db/receipts/io.github.hxddh.oprobe.bom")) =>
        {
            InstallMethod::MacosPkg
        }
        "linux" | "freebsd" if p.starts_with("/usr/bin/") || p.starts_with("/usr/sbin/") => {
            InstallMethod::SystemPackage
        }
        _ => InstallMethod::Standalone,
    }
}

/// `(major, minor, patch, pre)` of `v1.2.3[-pre]`; a pre-release sorts before its release.
fn version_key(v: &str) -> Option<(u64, u64, u64, bool, String)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, p.to_owned()),
        None => (v, String::new()),
    };
    let mut it = core.split('.').map(|x| x.parse::<u64>());
    let (a, b, c) = (
        it.next()?.ok()?,
        it.next()?.ok()?,
        it.next().unwrap_or(Ok(0)).ok()?,
    );
    Some((a, b, c, pre.is_empty(), pre))
}

/// `a` is newer than `b`.
pub fn newer(a: &str, b: &str) -> bool {
    match (version_key(a), version_key(b)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

/// Release archive names of this platform, best first.
pub fn asset_names(version: &str) -> Vec<String> {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => vec![format!("oprobe-{version}-universal-apple-darwin.tar.gz")],
        "windows" => vec![format!("oprobe-{version}-{arch}-pc-windows-msvc.zip")],
        _ => {
            let env = if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            };
            let mut v = Vec::new();
            if cfg!(feature = "gui") && env == "gnu" {
                v.push(format!(
                    "oprobe-desktop-{version}-{arch}-unknown-linux-gnu.tar.gz"
                ));
            }
            v.push(format!(
                "oprobe-{version}-{arch}-unknown-linux-{env}.tar.gz"
            ));
            v
        }
    }
}

/// `hash  name` lines of a `SHA256SUMS` file.
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let h = it.next()?;
            let n = it.next()?.trim_start_matches('*');
            (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| (h.to_ascii_lowercase(), n.to_owned()))
        })
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The `oprobe` executable inside a release archive.
pub fn extract_binary(archive_name: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let exe = if cfg!(windows) {
        "oprobe.exe"
    } else {
        "oprobe"
    };
    if archive_name.ends_with(".tar.gz") {
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for entry in ar.entries()? {
            let mut e = entry?;
            let path = e.path()?.into_owned();
            if e.header().entry_type().is_file() && path.file_name().is_some_and(|n| n == exe) {
                let mut out = Vec::new();
                e.read_to_end(&mut out)?;
                return Ok(out);
            }
        }
        anyhow::bail!("`{archive_name}` holds no `{exe}`")
    }
    #[cfg(windows)]
    if archive_name.ends_with(".zip") {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
        for i in 0..z.len() {
            let mut f = z.by_index(i)?;
            // Archive member names use `/`; tolerate `\` from Windows-built archives.
            let matches = f.is_file() && f.name()?.rsplit(['/', '\\']).next() == Some(exe);
            if matches {
                let mut out = Vec::new();
                f.read_to_end(&mut out)?;
                return Ok(out);
            }
        }
        anyhow::bail!("`{archive_name}` holds no `{exe}`")
    }
    anyhow::bail!("unsupported archive `{archive_name}`")
}

/// Write `bin` next to `exe` and swap it in.
pub fn replace_binary(exe: &Path, bin: &[u8]) -> Result<()> {
    let dir = exe.parent().context("the executable has no directory")?;
    let tmp = dir.join(format!(".oprobe-update-{}", std::process::id()));
    std::fs::write(&tmp, bin).with_context(|| {
        format!(
            "cannot write to {} (permissions? re-run with the rights that installed oprobe)",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    let swap = || -> Result<()> {
        #[cfg(windows)]
        {
            // A running .exe cannot be overwritten, but it can be renamed.
            let old = exe.with_extension("exe.old");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(exe, &old)?;
            if let Err(e) = std::fs::rename(&tmp, exe) {
                let _ = std::fs::rename(&old, exe);
                return Err(e.into());
            }
        }
        #[cfg(not(windows))]
        std::fs::rename(&tmp, exe)?;
        Ok(())
    };
    swap().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Result of `oprobe self-update`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct UpdateOutput {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    /// The new version was installed.
    pub installed: bool,
    pub install_method: InstallMethod,
    /// How to update this install.
    pub update_command: String,
    pub executable: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub release_url: String,
    /// Where the release was found.
    pub release_source: ReleaseSource,
    /// How the installed download was verified beyond its SHA-256 (set when installed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<VerifiedBy>,
}

/// Where `self-update` found the release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseSource {
    /// The release's `latest.json` index.
    LatestJson,
    /// The GitHub REST API.
    Api,
}

impl CommandOutput for UpdateOutput {
    fn text(&self) -> String {
        if self.installed {
            return format!(
                "{} updated oprobe {} → {} {}\n  {}\n",
                green("✓"),
                self.current_version,
                bold(&self.latest_version),
                dim(&format!("({})", self.executable)),
                dim(&format!(
                    "verified sha256 {}{} · release notes: {}",
                    self.sha256.as_deref().unwrap_or("-"),
                    match self.verified_by {
                        Some(VerifiedBy::GithubAttestation) => " and the build provenance",
                        Some(VerifiedBy::Minisign) => " and the minisign signature",
                        _ => "",
                    },
                    self.release_url
                ))
            );
        }
        if !self.update_available {
            return format!(
                "{} oprobe {} is the latest release\n",
                green("✓"),
                self.current_version
            );
        }
        format!(
            "{} oprobe {} is available (this is {})\n  update: {}\n  notes:  {}\n",
            yellow("●"),
            bold(&self.latest_version),
            self.current_version,
            self.update_command,
            self.release_url
        )
    }
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// `latest.json` of a release (`scripts/release-manifests.sh`, schema `oprobe.release/1`).
#[derive(Deserialize)]
struct IndexDoc {
    version: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    release_url: Option<String>,
    assets: Vec<IndexAsset>,
}

#[derive(Deserialize)]
struct IndexAsset {
    name: String,
    url: String,
    #[serde(default)]
    size_bytes: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
}

/// A release, wherever it was found.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub html_url: String,
    pub assets: Vec<Asset>,
    pub source: ReleaseSource,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// From `latest.json`; API releases verify against `SHA256SUMS`.
    pub sha256: Option<String>,
}

/// A release from its `latest.json` index.
pub fn parse_index(bytes: &[u8]) -> Result<Release> {
    let d: IndexDoc = serde_json::from_slice(bytes).context("unexpected latest.json")?;
    anyhow::ensure!(
        version_key(&d.version).is_some(),
        "latest.json names no version"
    );
    let tag = d
        .tag
        .unwrap_or_else(|| format!("v{}", d.version.trim_start_matches('v')));
    Ok(Release {
        html_url: d
            .release_url
            .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/tag/{tag}")),
        tag,
        assets: d
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.url,
                size: a.size_bytes.unwrap_or(0),
                sha256: a
                    .sha256
                    .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
                    .map(|h| h.to_ascii_lowercase()),
            })
            .collect(),
        source: ReleaseSource::LatestJson,
    })
}

/// A release from the GitHub API.
pub fn parse_api(bytes: &[u8]) -> Result<Release> {
    let r: ApiRelease =
        serde_json::from_slice(bytes).context("unexpected answer from the GitHub API")?;
    Ok(Release {
        tag: r.tag_name,
        html_url: r.html_url,
        assets: r
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
                size: a.size,
                sha256: None,
            })
            .collect(),
        source: ReleaseSource::Api,
    })
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!(
            "oprobe/",
            env!("CARGO_PKG_VERSION"),
            " (self-update)"
        ))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(600))
        .build()?)
}

/// The error of a GitHub answer with `status` (not 2xx) for `url`. A refusal (403 — GitHub's
/// rate limit or a blocked network —, 429, 5xx) is `net.unreachable` (retryable), never
/// `storage.throttled`: GitHub is not the storage being measured.
pub fn status_error(status: u16, url: &str) -> OprobeError {
    match status {
        403 | 429 => OprobeError::new(
            ErrorCode::NetUnreachable,
            format!("GitHub refused the request ({status}) for {url}: rate limited or blocked"),
        )
        .with_hint(
            "set GITHUB_TOKEN, wait for the rate limit to reset, or download the release from \
             https://github.com/hxddh/oprobe/releases",
        ),
        404 => OprobeError::new(ErrorCode::UsageInvalidArgument, format!("not found: {url}"))
            .with_hint(
                "check the tag name (`--to v4.0.0`); releases: https://github.com/hxddh/oprobe/releases",
            ),
        _ => OprobeError::new(
            ErrorCode::NetUnreachable,
            format!("GitHub answered {status} for {url}"),
        )
        .with_hint("retry later; releases: https://github.com/hxddh/oprobe/releases"),
    }
}

async fn get(c: &reqwest::Client, url: &str, api: bool) -> Result<bytes::Bytes> {
    let mut req = c.get(url);
    if api {
        req = req.header("accept", "application/vnd.github+json");
        if let Ok(t) = std::env::var("GITHUB_TOKEN")
            && !t.is_empty()
            && url.starts_with(DEFAULT_API)
        {
            req = req.bearer_auth(t);
        }
    }
    let resp = req.send().await.map_err(|e| {
        OprobeError::new(
            if e.is_timeout() {
                ErrorCode::NetTimeout
            } else {
                ErrorCode::NetUnreachable
            },
            format!("cannot reach {url}: {e}"),
        )
        .with_hint("check the network (or HTTPS_PROXY), then retry")
    })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(status_error(status.as_u16(), url).into());
    }
    if resp.content_length().unwrap_or(0) > MAX_ASSET_BYTES {
        anyhow::bail!("{url} is larger than {MAX_ASSET_BYTES} bytes");
    }
    Ok(resp.bytes().await?)
}

fn env_url(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| !v.is_empty())
}

/// `v`-prefixed tag of a `--to` value.
fn tag_of(t: &str) -> String {
    if t.starts_with('v') {
        t.to_owned()
    } else {
        format!("v{t}")
    }
}

/// Resolve the release: `latest.json` first, then the API (design v4 §10). The API's error is
/// returned when both fail (it is the more specific one: a 404 for an unknown `--to` tag).
async fn resolve_release(http: &reqwest::Client, to: Option<&str>, ctx: &Ctx) -> Result<Release> {
    let api = env_url("OPROBE_UPDATE_API");
    // A configured API mirror without a release base: do not reach github.com behind its back.
    let base = env_url("OPROBE_RELEASE_BASE")
        .or_else(|| api.is_none().then(|| DEFAULT_RELEASE_BASE.to_owned()));
    if let Some(base) = base {
        let url = match to {
            Some(t) => format!("{base}/download/{}/latest.json", tag_of(t)),
            None => format!("{base}/latest/download/latest.json"),
        };
        match get(http, &url, false).await.and_then(|b| parse_index(&b)) {
            Ok(r) => return Ok(r),
            Err(e) => ctx.out.info(dim(&format!(
                "latest.json unavailable ({}); asking the GitHub API",
                crate::errors::classify(&e, "").message
            ))),
        }
    }
    let api = api.unwrap_or_else(|| DEFAULT_API.to_owned());
    let url = match to {
        Some(t) => format!("{api}/repos/{REPO}/releases/tags/{}", tag_of(t)),
        None => format!("{api}/repos/{REPO}/releases/latest"),
    };
    parse_api(&get(http, &url, true).await?)
}

pub async fn cmd_self_update(a: SelfUpdateArgs, ctx: &Ctx) -> Result<u8> {
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    #[cfg(windows)]
    let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    let method = install_method(&exe, std::env::consts::OS, &|p| p.exists());
    let http = client()?;
    ctx.out.info(dim("checking GitHub Releases…"));
    let rel = resolve_release(&http, a.to.as_deref(), ctx).await?;
    let latest = rel.tag.trim_start_matches('v').to_owned();
    let available = newer(&latest, &current) || (a.to.is_some() && latest != current);
    let mut out = UpdateOutput {
        current_version: current.clone(),
        latest_version: latest.clone(),
        update_available: available,
        installed: false,
        install_method: method.clone(),
        update_command: method.update_instruction().to_owned(),
        executable: exe.display().to_string(),
        asset: None,
        sha256: None,
        release_url: rel.html_url.clone(),
        release_source: rel.source,
        verified_by: None,
    };
    if a.check || (!available && !a.force) {
        return ctx.out.finish(&out);
    }
    if method != InstallMethod::Standalone {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!(
                "this oprobe is managed by {} ({}); self-update would bypass it",
                format!("{method:?}").to_ascii_lowercase(),
                exe.display()
            ),
        )
        .with_hint(format!("update with: {}", method.update_instruction()))
        .into());
    }
    let names = asset_names(&latest);
    let asset = names
        .iter()
        .find_map(|n| rel.assets.iter().find(|x| &x.name == n))
        .ok_or_else(|| {
            OprobeError::new(
                ErrorCode::StorageNotFound,
                format!(
                    "release {} has no archive for this platform (looked for {})",
                    rel.tag,
                    names.join(", ")
                ),
            )
            .with_hint(format!("download it by hand: {}", rel.html_url))
        })?;
    if asset.size > MAX_ASSET_BYTES {
        anyhow::bail!(
            "{} is unexpectedly large ({} bytes)",
            asset.name,
            asset.size
        );
    }
    let expected = match &asset.sha256 {
        Some(h) => h.clone(),
        None => {
            let sums = rel
                .assets
                .iter()
                .find(|x| x.name == "SHA256SUMS")
                .ok_or_else(|| {
                    OprobeError::new(
                        ErrorCode::SafetyRefused,
                        format!(
                            "release {} has no SHA256SUMS: refusing to install an unverified download",
                            rel.tag
                        ),
                    )
                })?;
            let sums = parse_sums(&String::from_utf8_lossy(
                &get(&http, &sums.url, false).await?,
            ));
            sums.iter()
                .find(|(_, n)| n == &asset.name)
                .map(|(h, _)| h.clone())
                .ok_or_else(|| {
                    OprobeError::new(
                        ErrorCode::SafetyRefused,
                        format!("SHA256SUMS does not list {}", asset.name),
                    )
                })?
        }
    };
    ctx.out.info(format!("downloading {}…", asset.name));
    let bytes = get(&http, &asset.url, false).await?;
    let got = sha256_hex(&bytes);
    if got != expected {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!(
                "checksum mismatch for {}: expected {expected}, got {got}; nothing was installed",
                asset.name
            ),
        )
        .into());
    }
    let verified_by =
        verify_download(&http, &rel, &asset.name, &bytes, &got, a.verify, ctx).await?;
    let bin = extract_binary(&asset.name, &bytes)?;
    // The new binary must run here before it replaces this one.
    let probe = exe.with_file_name(format!(".oprobe-check-{}", std::process::id()));
    let probe = if cfg!(windows) {
        probe.with_extension("exe")
    } else {
        probe
    };
    check_runs(&probe, &bin, &latest)?;
    replace_binary(&exe, &bin)?;
    out.installed = true;
    out.asset = Some(asset.name.clone());
    out.sha256 = Some(got);
    out.verified_by = Some(verified_by);
    ctx.out.finish(&out)
}

/// The provenance or signature check after the SHA-256 matched (see the module docs).
async fn verify_download(
    http: &reqwest::Client,
    rel: &Release,
    name: &str,
    bytes: &[u8],
    sha256: &str,
    policy: VerifyPolicy,
    ctx: &Ctx,
) -> Result<VerifiedBy> {
    if policy == VerifyPolicy::Checksum {
        return Ok(VerifiedBy::ChecksumOnly);
    }
    if gh_ready() {
        ctx.out.info(dim(
            "verifying the build provenance (gh attestation verify)…",
        ));
        gh_attestation_verify(name, bytes)?;
        return Ok(VerifiedBy::GithubAttestation);
    }
    if let Some(key) = minisign_key() {
        let url_of = |file: &str| {
            rel.assets
                .iter()
                .find(|x| x.name == file)
                .map(|x| x.url.clone())
                .unwrap_or_else(|| {
                    let base = env_url("OPROBE_RELEASE_BASE")
                        .unwrap_or_else(|| DEFAULT_RELEASE_BASE.to_owned());
                    format!("{base}/download/{}/{file}", rel.tag)
                })
        };
        // An unsigned release (no MINISIGN_KEY secret at release time) is not an error here.
        if let Ok(sig) = get(http, &url_of("SHA256SUMS.minisig"), false).await {
            let sums = get(http, &url_of("SHA256SUMS"), false).await?;
            verify_minisign(&key, &sums, &String::from_utf8_lossy(&sig)).map_err(|e| {
                OprobeError::new(
                    ErrorCode::SafetyRefused,
                    format!("{e:#}; nothing was installed"),
                )
            })?;
            let listed = parse_sums(&String::from_utf8_lossy(&sums))
                .into_iter()
                .any(|(h, n)| n == name && h == sha256);
            if !listed {
                return Err(OprobeError::new(
                    ErrorCode::SafetyRefused,
                    format!("the signed SHA256SUMS does not list {name} with sha256 {sha256}; nothing was installed"),
                )
                .into());
            }
            return Ok(VerifiedBy::Minisign);
        }
    }
    if policy == VerifyPolicy::Require {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            format!("cannot verify the provenance or signature of {name}; nothing was installed"),
        )
        .with_hint("install the GitHub CLI and run `gh auth login`, or use `--verify auto`")
        .into());
    }
    ctx.out.warn(format!(
        "only the SHA-256 of {name} was verified (from the same release); install the GitHub CLI and \
         run `gh auth login` to check its build provenance too"
    ));
    Ok(VerifiedBy::ChecksumOnly)
}

/// Write `bin` to `path`, run `--version` and expect `version` in its output.
fn check_runs(path: &PathBuf, bin: &[u8], version: &str) -> Result<()> {
    std::fs::write(path, bin)
        .with_context(|| format!("cannot write next to the executable ({})", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    let res = std::process::Command::new(path).arg("--version").output();
    let _ = std::fs::remove_file(path);
    let o = res.context("the downloaded oprobe does not start")?;
    let v = String::from_utf8_lossy(&o.stdout);
    anyhow::ensure!(
        o.status.success() && v.contains(version),
        "the downloaded oprobe reports `{}` instead of {version}; nothing was installed",
        v.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(newer("3.0.1", "3.0.0"));
        assert!(newer("v3.1.0", "3.0.9"));
        assert!(newer("3.0.0", "3.0.0-rc.1"));
        assert!(!newer("3.0.0", "3.0.0"));
        assert!(!newer("2.9.9", "3.0.0"));
        assert!(!newer("garbage", "3.0.0"));
    }

    #[test]
    fn package_managed_installs_are_recognised() {
        let no = |_: &Path| false;
        let yes = |_: &Path| true;
        let m = |p: &str, os: &str| install_method(Path::new(p), os, &no);
        assert_eq!(
            m("/opt/homebrew/Cellar/oprobe/3.0.0/bin/oprobe", "macos"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            m("/Applications/oprobe.app/Contents/MacOS/oprobe", "macos"),
            InstallMethod::MacosApp
        );
        assert_eq!(m("/usr/bin/oprobe", "linux"), InstallMethod::SystemPackage);
        assert_eq!(
            m("/home/u/.cargo/bin/oprobe", "linux"),
            InstallMethod::Cargo
        );
        assert_eq!(
            m(
                r"C:\Users\u\scoop\apps\oprobe\current\oprobe.exe",
                "windows"
            ),
            InstallMethod::Scoop
        );
        assert_eq!(
            m(r"C:\Program Files\oprobe\oprobe.exe", "windows"),
            InstallMethod::Msi
        );
        assert_eq!(
            m("/home/u/.local/bin/oprobe", "linux"),
            InstallMethod::Standalone
        );
        assert_eq!(
            m("/usr/local/bin/oprobe", "macos"),
            InstallMethod::Standalone
        );
        assert_eq!(
            install_method(Path::new("/usr/local/bin/oprobe"), "macos", &yes),
            InstallMethod::MacosPkg
        );
    }

    #[test]
    fn sums_and_archives() {
        let h = sha256_hex(b"abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let sums = format!("{h}  oprobe-3.0.0-x.tar.gz\nnot a line\n{h} *other.zip\n");
        let p = parse_sums(&sums);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].1, "other.zip");

        // A release-shaped archive: oprobe-<ver>-<target>/oprobe plus docs.
        let mut tarball = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
            let mut b = tar::Builder::new(gz);
            for (path, data) in [
                ("oprobe-3.0.0-x/README.md", &b"readme"[..]),
                ("oprobe-3.0.0-x/oprobe", &b"\x7fELF binary"[..]),
            ] {
                let mut hdr = tar::Header::new_gnu();
                hdr.set_size(data.len() as u64);
                hdr.set_mode(0o755);
                hdr.set_cksum();
                b.append_data(&mut hdr, path, data).unwrap();
            }
            b.into_inner().unwrap().finish().unwrap();
        }
        if !cfg!(windows) {
            let bin = extract_binary("oprobe-3.0.0-x.tar.gz", &tarball).unwrap();
            assert_eq!(bin, b"\x7fELF binary");
        }
        assert!(extract_binary("x.rar", &tarball).is_err());
        assert!(!asset_names("3.0.0").is_empty());
    }

    #[test]
    fn github_refusals_are_network_errors() {
        for s in [403, 429, 502] {
            let e = status_error(s, "https://api.github.com/x");
            assert_eq!(e.code, ErrorCode::NetUnreachable, "{s}");
            assert!(e.code.retryable());
        }
        assert_ne!(status_error(403, "u").code, ErrorCode::StorageThrottled);
        assert!(
            status_error(403, "u")
                .hint
                .unwrap_or_default()
                .contains("GITHUB_TOKEN")
        );
        assert_eq!(status_error(404, "u").code, ErrorCode::UsageInvalidArgument);
    }

    #[test]
    fn releases_parse_from_latest_json_and_the_api() {
        let h = sha256_hex(b"x");
        let idx = format!(
            r#"{{"schema":"oprobe.release/1","version":"4.0.0","tag":"v4.0.0",
               "release_url":"https://example.invalid/r",
               "assets":[{{"name":"a.tar.gz","url":"https://example.invalid/a.tar.gz","size_bytes":3,"sha256":"{h}"}},
                         {{"name":"b.zip","url":"https://example.invalid/b.zip","size_bytes":null,"sha256":"bad"}}]}}"#
        );
        let r = parse_index(idx.as_bytes()).unwrap();
        assert_eq!(r.tag, "v4.0.0");
        assert_eq!(r.source, ReleaseSource::LatestJson);
        assert_eq!(r.assets[0].sha256.as_deref(), Some(h.as_str()));
        assert_eq!(r.assets[1].sha256, None);
        assert!(parse_index(br#"{"version":"","assets":[]}"#).is_err());
        let api = br#"{"tag_name":"v4.0.1","html_url":"h","assets":[{"name":"SHA256SUMS","browser_download_url":"u","size":1}]}"#;
        let r = parse_api(api).unwrap();
        assert_eq!((r.tag.as_str(), r.source), ("v4.0.1", ReleaseSource::Api));
        assert_eq!(r.assets[0].url, "u");
        assert_eq!(tag_of("4.0.0"), "v4.0.0");
        assert_eq!(tag_of("v4.0.0"), "v4.0.0");
    }

    #[test]
    fn minisign_signatures_verify_sha256sums() {
        // A throwaway key made for this test (prehashed signature, as `minisign -S` writes).
        const PK: &str = "RWT5BelBCpdB7JZFeDj9viCtmo6Tow9Vzc2nDqm095pRgF+Xxu8IBcqR";
        const SIG: &str = "untrusted comment: signature from minisign secret key
RUT5BelBCpdB7K6uKmM0T/3YNhamXtGJ5BH7k9uv5EgDmqZw6qljOKYxYFu1e3bWFu35L917AlG9SufjYEYA6CoXkuxCfzcA9QA=
trusted comment: timestamp:1795000000\tfile:SHA256SUMS\tprehashed
SgikQTFX/Syln1hI+LDybO9kMP7+PBjqgdwu7DQBQ/JbllOgTMfZxOBhKm49HpLxsylv4HxymuGry0ivJoVSDg==";
        let sums = b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  oprobe-5.0.0-x.tar.gz\n";
        verify_minisign(PK, sums, SIG).unwrap();
        let mut tampered = sums.to_vec();
        tampered[0] = b'c';
        assert!(verify_minisign(PK, &tampered, SIG).is_err());
        // Another key (the minisign-verify crate's test key) is refused.
        assert!(
            verify_minisign(
                "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
                sums,
                SIG
            )
            .is_err()
        );
        assert!(verify_minisign("not a key", sums, SIG).is_err());
        assert!(verify_minisign(PK, sums, "garbage").is_err());
    }

    #[test]
    fn replaces_a_binary_in_place() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join(if cfg!(windows) {
            "oprobe.exe"
        } else {
            "oprobe"
        });
        std::fs::write(&exe, b"old").unwrap();
        replace_binary(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let left: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".oprobe-update"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}
