#!/bin/sh
# oprobe installer for Linux and macOS. Run `sh install.sh --help` for the options.
#   curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh -s -- --cli
#
# Verification, after the archive's SHA-256 matched SHA256SUMS:
#   1. the GitHub build provenance attestation (`gh attestation verify`), when `gh` is installed
#      and logged in (GH_TOKEN / `gh auth login`);
#   2. otherwise a minisign signature over SHA256SUMS (SHA256SUMS.minisig), but only when this
#      script embeds a public key (or OPROBE_MINISIGN_PUBKEY is set), `minisign` is installed
#      and the release is signed;
#   3. otherwise a warning (an error with OPROBE_VERIFY=require).
#
# The latest version is resolved without the GitHub API when possible, falling back on any failure:
#   1. <base>/latest/download/latest.json
#   2. the <base>/latest redirect (its Location header names the tag)
#   3. the GitHub API (rate limited to 60 anonymous requests per hour)
set -eu

usage() {
  cat <<'EOF'
oprobe installer for Linux and macOS

Usage:
  curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh
  curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh -s -- [OPTIONS]
  sh install.sh [OPTIONS]

Options:
  --cli              install the CLI-only build (no app)            (OPROBE_FLAVOR=cli)
  --version X.Y.Z    install this version instead of the latest     (OPROBE_VERSION)
  --dir DIR          install into DIR (default ~/.local/bin)        (OPROBE_INSTALL_DIR)
  --no-modify-path   never touch the shell rc files                 (OPROBE_MODIFY_PATH=0)
  -h, --help         print this help and exit

Environment:
  OPROBE_VERSION=X.Y.Z        install this version instead of the latest
  OPROBE_INSTALL_DIR=DIR      where to put the binary (default ~/.local/bin)
  OPROBE_FLAVOR=app|cli       the build to install (default app)
  OPROBE_MODIFY_PATH=1|0      add DIR to PATH in your shell rc file without asking (1) or never (0);
                              unset: ask when a terminal is available, otherwise only print the line
  OPROBE_RELEASE_BASE=URL     release download base (mirrors, tests);
                              default https://github.com/hxddh/oprobe/releases
  OPROBE_API=URL              GitHub API base (default https://api.github.com)
  OPROBE_VERIFY=auto|require|checksum
                              after the SHA-256 check (default auto):
                              auto     also verify the build provenance when `gh` is logged in
                              require  fail unless the provenance (or a signature) verified
                              checksum only the SHA-256 check (SHA256SUMS from the same release)
  OPROBE_MINISIGN_PUBKEY=KEY  verify SHA256SUMS.minisig with this minisign public key; without it
                              (and without a key embedded in this script) no signature is checked

The SHA-256 is always checked; the GitHub build provenance is checked when `gh` is installed
and logged in. Docs: https://github.com/hxddh/oprobe/blob/main/docs/install.md
EOF
}

flavor="${OPROBE_FLAVOR:-app}"
while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --cli) flavor=cli ;;
    --app) flavor=app ;;
    --version) [ $# -ge 2 ] || { echo "oprobe-install: --version needs a value" >&2; exit 2; }; OPROBE_VERSION="$2"; shift ;;
    --version=*) OPROBE_VERSION="${1#--version=}" ;;
    --dir) [ $# -ge 2 ] || { echo "oprobe-install: --dir needs a value" >&2; exit 2; }; OPROBE_INSTALL_DIR="$2"; shift ;;
    --dir=*) OPROBE_INSTALL_DIR="${1#--dir=}" ;;
    --no-modify-path) OPROBE_MODIFY_PATH=0 ;;
    *) echo "oprobe-install: unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done
case "$flavor" in app|cli) ;; *) echo "oprobe-install: OPROBE_FLAVOR must be app or cli" >&2; exit 2 ;; esac

repo="hxddh/oprobe"
base="${OPROBE_RELEASE_BASE:-https://github.com/$repo/releases}"
base="${base%/}"
api="${OPROBE_API:-https://api.github.com}"
api="${api%/}"
dir="${OPROBE_INSTALL_DIR:-$HOME/.local/bin}"
verify="${OPROBE_VERIFY:-auto}"
# The release's minisign public key. The release workflow fills it in only when the repository
# variable MINISIGN_PUBKEY is set (docs/releasing.md#signing); empty: no signature is checked.
embedded_pubkey=""
pubkey="${OPROBE_MINISIGN_PUBKEY:-$embedded_pubkey}"
case "$verify" in auto|require|checksum) ;; *) echo "oprobe-install: OPROBE_VERIFY must be auto, require or checksum" >&2; exit 1 ;; esac

err() { echo "oprobe-install: $*" >&2; exit 1; }
say() { echo "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1"; }
need uname; need tar; need mkdir; need sed; need grep

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
  # The final URL of a redirect chain (empty on failure).
  final_url() { curl -fsSLI -o /dev/null -w '%{url_effective}' "$1" 2>/dev/null || true; }
  # The Location header of the first answer (empty when there is none).
  location() { curl -fsSI "$1" 2>/dev/null | tr -d '\r' | sed -n 's/^[Ll]ocation: *//p' | tail -n1; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q "$1" -O "$2"; }
  final_url() { :; }
  location() { wget -S --spider --max-redirect=0 "$1" 2>&1 | tr -d '\r' | sed -n 's/^ *[Ll]ocation: *//p' | tail -n1; }
else
  err "need curl or wget"
fi

os=$(uname -s); arch=$(uname -m)
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target=x86_64-unknown-linux-musl ;;
      aarch64|arm64) target=aarch64-unknown-linux-musl ;;
      *) err "unsupported architecture: $arch" ;;
    esac ;;
  Darwin) target=universal-apple-darwin ;;
  *) err "unsupported OS: $os (Windows: use install.ps1 or the MSI)" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# A version from a tag-like string (v4.0.0, .../tag/v4.0.0) or nothing.
version_of() { printf '%s\n' "$1" | sed -n 's|.*/tag/v\{0,1\}\([0-9][^/?#]*\).*|\1|p' | head -n1; }

resolve_latest() {
  # 1. latest.json (a release asset; no API, no rate limit).
  if fetch "$base/latest/download/latest.json" "$tmp/latest.json" 2>/dev/null; then
    v=$(sed -n 's/.*"version": *"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n1)
    if [ -n "$v" ]; then echo "$v"; return 0; fi
  fi
  # 2. The releases/latest redirect: its Location (or the final URL) ends in /tag/vX.Y.Z.
  v=$(version_of "$(location "$base/latest")")
  [ -n "$v" ] || v=$(version_of "$(final_url "$base/latest")")
  if [ -n "$v" ]; then echo "$v"; return 0; fi
  # 3. The GitHub API.
  if fetch "$api/repos/$repo/releases/latest" "$tmp/release.json" 2>/dev/null; then
    v=$(sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp/release.json" | head -n1)
    if [ -n "$v" ]; then echo "$v"; return 0; fi
  fi
  return 1
}

if [ -n "${OPROBE_VERSION:-}" ]; then
  version="${OPROBE_VERSION#v}"
else
  version=$(resolve_latest) || err "cannot determine the latest version (set OPROBE_VERSION=X.Y.Z, or download from https://github.com/$repo/releases)"
fi
url="$base/download/v$version"

# The app build is `oprobe-<ver>-<target>`, the CLI-only build `oprobe-cli-<ver>-<target>`.
if [ "$flavor" = cli ]; then name="oprobe-cli-$version-$target"; else name="oprobe-$version-$target"; fi
say "Downloading $name.tar.gz"
fetch "$url/$name.tar.gz" "$tmp/$name.tar.gz" || err "download failed: $url/$name.tar.gz"
fetch "$url/SHA256SUMS" "$tmp/SHA256SUMS" || err "cannot download $url/SHA256SUMS"
expected=$(grep " \*\{0,1\}$name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1); fi
{ [ -n "$expected" ] && [ "$expected" = "$actual" ]; } || err "checksum mismatch for $name.tar.gz"

# Provenance or signature (see the header). A failed verification is always fatal; a check
# this machine cannot run is a warning, or fatal with OPROBE_VERIFY=require.
verified=""
if [ "$verify" != checksum ]; then
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    if gh attestation verify "$tmp/$name.tar.gz" --repo "$repo" \
         --signer-workflow "$repo/.github/workflows/release.yml" >"$tmp/attest.log" 2>&1; then
      verified="GitHub build provenance (gh attestation verify)"
    else
      cat "$tmp/attest.log" >&2
      err "the build provenance of $name.tar.gz does not verify; nothing was installed"
    fi
  elif [ -n "$pubkey" ] && command -v minisign >/dev/null 2>&1 \
       && fetch "$url/SHA256SUMS.minisig" "$tmp/SHA256SUMS.minisig" 2>/dev/null; then
    if minisign -Vq -P "$pubkey" -m "$tmp/SHA256SUMS" -x "$tmp/SHA256SUMS.minisig" >/dev/null 2>&1; then
      verified="minisign signature over SHA256SUMS"
    else
      err "the minisign signature of SHA256SUMS does not verify; nothing was installed"
    fi
  fi
  if [ -z "$verified" ]; then
    why="install gh and log in (gh auth login), or install minisign"
    [ -n "$pubkey" ] || why="install gh and log in (gh auth login)"
    if [ "$verify" = require ]; then
      err "cannot verify the provenance or signature of $name.tar.gz ($why), or set OPROBE_VERIFY=auto"
    fi
    echo "oprobe-install: warning: only the SHA-256 checksum was verified; to check the provenance too, $why" >&2
  else
    say "Verified: $verified"
  fi
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/$name/oprobe" "$dir/oprobe" 2>/dev/null || { cp "$tmp/$name/oprobe" "$dir/oprobe"; chmod 755 "$dir/oprobe"; }
if [ "$flavor" = cli ]; then say "Installed oprobe $version (CLI only) to $dir/oprobe"
else say "Installed oprobe $version to $dir/oprobe"; fi

# PATH: offer to add the directory to the shell's rc file.
case ":$PATH:" in
  *":$dir:"*) exit 0 ;;
esac
shell_name=$(basename "${SHELL:-sh}")
case "$shell_name" in
  zsh) rc="${ZDOTDIR:-$HOME}/.zshrc"; line="export PATH=\"$dir:\$PATH\"" ;;
  bash)
    if [ "$os" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi
    line="export PATH=\"$dir:\$PATH\"" ;;
  fish) rc="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"; line="fish_add_path \"$dir\"" ;;
  *) rc="$HOME/.profile"; line="export PATH=\"$dir:\$PATH\"" ;;
esac

modify="${OPROBE_MODIFY_PATH:-}"
if [ -z "$modify" ]; then
  # Usually piped from curl: stdin is the script, so ask on the terminal when there is one.
  # (A subshell: failing to open /dev/tty, e.g. without a controlling terminal, must not exit.)
  if [ -r /dev/tty ] && [ -w /dev/tty ] && (exec </dev/tty) 2>/dev/null; then
    printf '%s is not on your PATH. Add it to %s? [Y/n] ' "$dir" "$rc" >/dev/tty
    answer=""
    read -r answer </dev/tty || answer=n
    case "$answer" in
      ""|y|Y|yes|YES|Yes) modify=1 ;;
      *) modify=0 ;;
    esac
  else
    modify=0
  fi
fi

if [ "$modify" = 1 ]; then
  mkdir -p "$(dirname "$rc")"
  if [ -f "$rc" ] && grep -F "$line" "$rc" >/dev/null 2>&1; then
    say "$rc already adds $dir to PATH; open a new shell to use it."
  else
    printf '\n# Added by the oprobe installer\n%s\n' "$line" >> "$rc"
    say "Added $dir to PATH in $rc; open a new shell (or run: $line)."
  fi
else
  say "$dir is not on your PATH. Add it with:"
  say "  echo '$line' >> $rc"
fi
