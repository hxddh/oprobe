#!/usr/bin/env bash
# Generate the package-manager manifests of a release from its assets and SHA256SUMS:
#   oprobe.rb             Homebrew formula `oprobe` (macOS universal + Linux musl archives);
#                         the release job copies it to Formula/oprobe.rb of hxddh/homebrew-tap
#   oprobe-app.rb         Homebrew cask `oprobe-app` (oprobe.app from the .dmg): Casks/oprobe-app.rb
#                         (a distinct token: a formula and a cask of one tap must not share a name)
#   oprobe.json           Scoop manifest (Windows zip): bucket/oprobe.json of hxddh/scoop-bucket
#   winget-manifests.zip  winget manifests for the MSI (the release job opens a winget-pkgs PR)
#   latest.json           machine-readable index: version + every asset's URL, size and SHA-256
#                         (install.sh and install.ps1 read it first)
# usage: scripts/release-manifests.sh <version> <dist-dir> <owner/repo>
# Missing assets are skipped with a warning (e.g. a partial rebuild), never guessed.
set -euo pipefail
version="$1"; dist="$(cd "$2" && pwd)"; repo="$3"
url="https://github.com/$repo/releases/download/v$version"
desc="A profiler for Apache OpenDAL operators: what your Operator does on the wire, the layer that fixes it, and an A/B test that proves the fix"
sha() { grep " \*\?$1\$" "$dist/SHA256SUMS" | cut -d' ' -f1; }
have() { [ -f "$dist/$1" ] || { echo "warning: $1 missing; skipping what needs it" >&2; return 1; }; }

mac="oprobe-$version-universal-apple-darwin.tar.gz"
lx64="oprobe-$version-x86_64-unknown-linux-musl.tar.gz"
larm="oprobe-$version-aarch64-unknown-linux-musl.tar.gz"
if have "$mac" && have "$lx64" && have "$larm"; then
cat > "$dist/oprobe.rb" <<RB
class Oprobe < Formula
  desc "$desc"
  homepage "https://github.com/$repo"
  version "$version"
  license "Apache-2.0"

  on_macos do
    url "$url/$mac"
    sha256 "$(sha "$mac")"
  end

  on_linux do
    on_intel do
      url "$url/$lx64"
      sha256 "$(sha "$lx64")"
    end
    on_arm do
      url "$url/$larm"
      sha256 "$(sha "$larm")"
    end
  end

  def install
    bin.install "oprobe"
  end

  def caveats
    <<~EOS
      Run oprobe for the app in the terminal, or oprobe demo for a first profile.
      The app in a native window: brew install --cask hxddh/tap/oprobe-app
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oprobe --version")
    ENV["OPROBE_DATA_DIR"] = testpath/"data"
    system bin/"oprobe", "run", "memory://", "-d", "1s", "-q", "-o", "json"
  end
end
RB
fi

dmg="oprobe-$version-macos-universal.dmg"
if have "$dmg"; then
cat > "$dist/oprobe-app.rb" <<RB
cask "oprobe-app" do
  version "$version"
  sha256 "$(sha "$dmg")"

  url "$url/$dmg"
  name "oprobe"
  desc "$desc"
  homepage "https://github.com/$repo"

  depends_on macos: ">= :big_sur"

  app "oprobe.app"

  caveats <<~EOS
    oprobe.app is not signed or notarized (the project has no Apple certificate).
    On first open, macOS 15+ blocks it: open System Settings > Privacy & Security and
    click "Open Anyway". Or run: xattr -dr com.apple.quarantine /Applications/oprobe.app
  EOS
end
RB
fi

wx64="oprobe-$version-x86_64-pc-windows-msvc.zip"
warm="oprobe-$version-aarch64-pc-windows-msvc.zip"
if have "$wx64" && have "$warm"; then
cat > "$dist/oprobe.json" <<JSON
{
  "version": "$version",
  "description": "$desc",
  "homepage": "https://github.com/$repo",
  "license": "Apache-2.0",
  "architecture": {
    "64bit": {
      "url": "$url/$wx64",
      "hash": "$(sha "$wx64")",
      "extract_dir": "oprobe-$version-x86_64-pc-windows-msvc"
    },
    "arm64": {
      "url": "$url/$warm",
      "hash": "$(sha "$warm")",
      "extract_dir": "oprobe-$version-aarch64-pc-windows-msvc"
    }
  },
  "bin": "oprobe.exe",
  "checkver": {
    "url": "https://github.com/$repo/releases/latest/download/latest.json",
    "jsonpath": "\$.version"
  },
  "autoupdate": {
    "architecture": {
      "64bit": { "url": "https://github.com/$repo/releases/download/v\$version/oprobe-\$version-x86_64-pc-windows-msvc.zip", "extract_dir": "oprobe-\$version-x86_64-pc-windows-msvc" },
      "arm64": { "url": "https://github.com/$repo/releases/download/v\$version/oprobe-\$version-aarch64-pc-windows-msvc.zip", "extract_dir": "oprobe-\$version-aarch64-pc-windows-msvc" }
    },
    "hash": {
      "url": "https://github.com/$repo/releases/download/v\$version/SHA256SUMS",
      "regex": "\$sha256\\\\s+\\\\*?\$basename"
    }
  }
}
JSON
fi

# winget: one version manifest, one installer manifest (both MSIs), one default locale.
mx64="oprobe-$version-x64.msi"
marm="oprobe-$version-arm64.msi"
if have "$mx64" && have "$marm"; then
  id="hxddh.oprobe"
  w="$(mktemp -d)"
  d="$w/manifests/h/hxddh/oprobe/$version"
  mkdir -p "$d"
  upgrade_code="{6F1B4C2A-9E57-4C31-8D0B-2A7E0C9B5D11}"   # packaging/windows/oprobe.wxs
  up() { echo "$1" | tr '[:lower:]' '[:upper:]'; }
  cat > "$d/$id.yaml" <<YAML
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json
PackageIdentifier: $id
PackageVersion: $version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
YAML
  cat > "$d/$id.installer.yaml" <<YAML
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json
PackageIdentifier: $id
PackageVersion: $version
InstallerType: wix
Scope: machine
UpgradeBehavior: install
Commands:
- oprobe
AppsAndFeaturesEntries:
- UpgradeCode: '$upgrade_code'
Installers:
- Architecture: x64
  InstallerUrl: $url/$mx64
  InstallerSha256: $(up "$(sha "$mx64")")
- Architecture: arm64
  InstallerUrl: $url/$marm
  InstallerSha256: $(up "$(sha "$marm")")
ManifestType: installer
ManifestVersion: 1.6.0
YAML
  cat > "$d/$id.locale.en-US.yaml" <<YAML
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.6.0.schema.json
PackageIdentifier: $id
PackageVersion: $version
PackageLocale: en-US
Publisher: oprobe contributors
PublisherUrl: https://github.com/$repo
PublisherSupportUrl: https://github.com/$repo/issues
PackageName: oprobe
PackageUrl: https://github.com/$repo
License: Apache-2.0
LicenseUrl: https://github.com/$repo/blob/main/LICENSE
ShortDescription: $desc
Description: oprobe is a profiler for Apache OpenDAL operators. It traces every backend request of an Operator stage by stage (queue, connect, send, time to first byte, body, retry backoff), names the OpenDAL layer that fixes what it finds, and proves the fix with an interleaved A/B test.
Moniker: oprobe
Tags:
- s3
- object-storage
- profiler
- opendal
- observability
ReleaseNotesUrl: https://github.com/$repo/releases/tag/v$version
ManifestType: defaultLocale
ManifestVersion: 1.6.0
YAML
  (cd "$w" && zip -qr "$dist/winget-manifests.zip" manifests)
  rm -rf "$w"
fi

# latest.json: every asset listed in SHA256SUMS with its URL and size.
python3 - "$version" "$dist" "$url" "$repo" <<'PY'
import json, os, sys
version, dist, url, repo = sys.argv[1:5]
assets = []
for line in open(os.path.join(dist, "SHA256SUMS")):
    parts = line.split()
    if len(parts) != 2:
        continue
    digest, name = parts[0], parts[1].lstrip("*")
    path = os.path.join(dist, name)
    assets.append({
        "name": name,
        "url": f"{url}/{name}",
        "size_bytes": os.path.getsize(path) if os.path.exists(path) else None,
        "sha256": digest,
    })
doc = {
    "schema": "oprobe.release/1",
    "version": version,
    "tag": f"v{version}",
    "release_url": f"https://github.com/{repo}/releases/tag/v{version}",
    "assets": assets,
}
with open(os.path.join(dist, "latest.json"), "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
PY
echo "wrote manifests to $dist:"
for f in oprobe.rb oprobe-app.rb oprobe.json winget-manifests.zip latest.json; do
  [ -f "$dist/$f" ] && echo "$f"
done
exit 0
