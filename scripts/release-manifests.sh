#!/usr/bin/env bash
# Generate a Homebrew formula and a Scoop manifest for a release.
# usage: scripts/release-manifests.sh <version> <dist-dir> <owner/repo>
set -euo pipefail
version="$1"; dist="$2"; repo="$3"
url="https://github.com/$repo/releases/download/v$version"
sha() { grep " $1\$" "$dist/SHA256SUMS" | cut -d' ' -f1; }

mac="oprobe-$version-universal-apple-darwin.tar.gz"
lx64="oprobe-$version-x86_64-unknown-linux-musl.tar.gz"
larm="oprobe-$version-aarch64-unknown-linux-musl.tar.gz"
cat > "$dist/oprobe.rb" <<RB
class Oprobe < Formula
  desc "White-box cloud storage microscope built on Apache OpenDAL"
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
    man1.install Dir["man/*.1"]
    bash_completion.install "completions/oprobe.bash" => "oprobe"
    zsh_completion.install "completions/_oprobe"
    fish_completion.install "completions/oprobe.fish"
    pkgshare.install "profiles"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oprobe --version")
    system bin/"oprobe", "run", "memory://", "-d", "1s", "--warmup", "0s", "-q", "--format", "json"
  end
end
RB

wx64="oprobe-$version-x86_64-pc-windows-msvc.zip"
warm="oprobe-$version-aarch64-pc-windows-msvc.zip"
cat > "$dist/oprobe.json" <<JSON
{
  "version": "$version",
  "description": "White-box cloud storage microscope built on Apache OpenDAL",
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
  "checkver": "github",
  "autoupdate": {
    "architecture": {
      "64bit": { "url": "https://github.com/$repo/releases/download/v\$version/oprobe-\$version-x86_64-pc-windows-msvc.zip", "extract_dir": "oprobe-\$version-x86_64-pc-windows-msvc" },
      "arm64": { "url": "https://github.com/$repo/releases/download/v\$version/oprobe-\$version-aarch64-pc-windows-msvc.zip", "extract_dir": "oprobe-\$version-aarch64-pc-windows-msvc" }
    }
  }
}
JSON
echo "wrote $dist/oprobe.rb and $dist/oprobe.json"
