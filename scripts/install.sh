#!/bin/sh
# oprobe installer for Linux and macOS.
#   curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh
# Options (env): OPROBE_VERSION=2.0.1  OPROBE_INSTALL_DIR=~/.local/bin
set -eu

repo="hxddh/oprobe"
dir="${OPROBE_INSTALL_DIR:-$HOME/.local/bin}"

err() { echo "oprobe-install: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1"; }
need uname; need tar; need mkdir
if command -v curl >/dev/null 2>&1; then fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then fetch() { wget -q "$1" -O "$2"; }
else err "need curl or wget"; fi

os=$(uname -s); arch=$(uname -m)
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target=x86_64-unknown-linux-musl ;;
      aarch64|arm64) target=aarch64-unknown-linux-musl ;;
      *) err "unsupported architecture: $arch" ;;
    esac ;;
  Darwin) target=universal-apple-darwin ;;
  *) err "unsupported OS: $os (Windows: use install.ps1, the MSI or scoop)" ;;
esac

if [ -n "${OPROBE_VERSION:-}" ]; then
  version="${OPROBE_VERSION#v}"
  base="https://github.com/$repo/releases/download/v$version"
else
  need sed
  tmpv=$(mktemp)
  fetch "https://api.github.com/repos/$repo/releases/latest" "$tmpv" || err "cannot query latest release"
  version=$(sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' "$tmpv" | head -n1)
  rm -f "$tmpv"
  [ -n "$version" ] || err "cannot determine latest version"
  base="https://github.com/$repo/releases/download/v$version"
fi

name="oprobe-$version-$target"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
echo "Downloading $name.tar.gz"
fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" || err "download failed: $base/$name.tar.gz"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || err "cannot download SHA256SUMS"
expected=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1); fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || err "checksum mismatch for $name.tar.gz"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/$name/oprobe" "$dir/oprobe" 2>/dev/null || { cp "$tmp/$name/oprobe" "$dir/oprobe"; chmod 755 "$dir/oprobe"; }
echo "Installed oprobe $version to $dir/oprobe"
case ":$PATH:" in *":$dir:"*) ;; *) echo "Add $dir to your PATH to run oprobe." ;; esac
