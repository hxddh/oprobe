//! `oprobe self-update`: check GitHub Releases, download this platform's archive, verify it
//! against the release's `SHA256SUMS`, check that the new binary runs, and replace the running
//! one atomically (Windows: rename the running executable aside, then move the new one in).
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
use super::args::SelfUpdateArgs;
use crate::errors::{ErrorCode, OprobeError};
use crate::output::CommandOutput;
use crate::report::style::{bold, dim, green, yellow};

/// GitHub repository the releases come from.
pub const REPO: &str = "hxddh/oprobe";
/// API base; `OPROBE_UPDATE_API` overrides it (mirrors, tests).
const DEFAULT_API: &str = "https://api.github.com";
/// Largest archive accepted.
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

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
            Self::Homebrew => "brew upgrade oprobe   (the app: brew upgrade --cask oprobe)",
            Self::SystemPackage => {
                "download the new .deb/.rpm from the release page and install it (sudo apt install ./oprobe_*.deb / sudo dnf install ./oprobe-*.rpm)"
            }
            Self::MacosPkg => "download and open the new oprobe-*-macos-universal.pkg",
            Self::MacosApp => {
                "download the new oprobe-*-macos-universal.dmg and replace oprobe.app"
            }
            Self::Scoop => "scoop update oprobe",
            Self::Winget => "winget upgrade oprobe",
            Self::Msi => "download and run the new oprobe-*.msi (or: winget upgrade oprobe)",
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
                    "verified sha256 {} · release notes: {}",
                    self.sha256.as_deref().unwrap_or("-"),
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
struct Release {
    tag_name: String,
    html_url: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
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
    let resp = req
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        return Err(OprobeError::new(
            ErrorCode::StorageThrottled,
            format!("GitHub refused the request ({status}): rate limited"),
        )
        .with_hint("wait an hour or set GITHUB_TOKEN, then retry")
        .into());
    }
    if status.as_u16() == 404 {
        return Err(OprobeError::new(
            ErrorCode::StorageNotFound,
            format!("not found: {url}"),
        )
        .with_hint("check the tag name (`--to v3.0.0`); releases: https://github.com/hxddh/oprobe/releases")
        .into());
    }
    let resp = resp
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    if resp.content_length().unwrap_or(0) > MAX_ASSET_BYTES {
        anyhow::bail!("{url} is larger than {MAX_ASSET_BYTES} bytes");
    }
    Ok(resp.bytes().await?)
}

pub async fn cmd_self_update(a: SelfUpdateArgs, ctx: &Ctx) -> Result<u8> {
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    #[cfg(windows)]
    let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    let method = install_method(&exe, std::env::consts::OS, &|p| p.exists());
    let api = std::env::var("OPROBE_UPDATE_API")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_API.to_owned());
    let api = api.trim_end_matches('/');
    let url = match &a.to {
        Some(tag) => format!(
            "{api}/repos/{REPO}/releases/tags/{}",
            if tag.starts_with('v') {
                tag.clone()
            } else {
                format!("v{tag}")
            }
        ),
        None => format!("{api}/repos/{REPO}/releases/latest"),
    };
    let http = client()?;
    ctx.out.info(dim("checking GitHub Releases…"));
    let rel: Release = serde_json::from_slice(&get(&http, &url, true).await?)
        .context("unexpected answer from the GitHub API")?;
    let latest = rel.tag_name.trim_start_matches('v').to_owned();
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
                    rel.tag_name,
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
    let sums = rel
        .assets
        .iter()
        .find(|x| x.name == "SHA256SUMS")
        .ok_or_else(|| {
            OprobeError::new(
                ErrorCode::SafetyRefused,
                format!(
                    "release {} has no SHA256SUMS: refusing to install an unverified download",
                    rel.tag_name
                ),
            )
        })?;
    let sums = parse_sums(&String::from_utf8_lossy(
        &get(&http, &sums.browser_download_url, false).await?,
    ));
    let expected = sums
        .iter()
        .find(|(_, n)| n == &asset.name)
        .map(|(h, _)| h.clone())
        .ok_or_else(|| {
            OprobeError::new(
                ErrorCode::SafetyRefused,
                format!("SHA256SUMS does not list {}", asset.name),
            )
        })?;
    ctx.out.info(format!("downloading {}…", asset.name));
    let bytes = get(&http, &asset.browser_download_url, false).await?;
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
    ctx.out.finish(&out)
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
