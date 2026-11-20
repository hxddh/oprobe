# Installing oprobe

Every release publishes the same binary in each platform's native format. All assets are listed
with their SHA-256 in `SHA256SUMS`.

| Asset | Platform |
|---|---|
| `oprobe-<ver>-x86_64-unknown-linux-musl.tar.gz` | Linux x86-64, static (any distro) |
| `oprobe-<ver>-aarch64-unknown-linux-musl.tar.gz` | Linux ARM64, static |
| `oprobe-<ver>-x86_64-unknown-linux-gnu.tar.gz` / `aarch64-…-gnu` | Linux, glibc ≥ 2.35 |
| `oprobe_<ver>-1_amd64.deb` / `_arm64.deb` | Debian, Ubuntu |
| `oprobe-<ver>-1.x86_64.rpm` / `.aarch64.rpm` | Fedora, RHEL, SUSE |
| `oprobe-<ver>-universal-apple-darwin.tar.gz` | macOS (Apple Silicon + Intel) |
| `oprobe-<ver>-macos-universal.pkg` | macOS installer (`/usr/local/bin`, man page, zsh completion) |
| `oprobe-<ver>-aarch64-apple-darwin.tar.gz` / `x86_64-apple-darwin` | macOS single-arch |
| `oprobe-<ver>-x64.msi` / `-arm64.msi` | Windows installer (adds to `PATH`, Start menu entry, removable in *Apps*) |
| `oprobe-<ver>-x86_64-pc-windows-msvc.zip` / `aarch64-…` | Windows portable |
| `oprobe-<ver>-macos-universal.dmg` | macOS app bundle (`oprobe.app`, same binary as the `.pkg`) |
| `oprobe-desktop-<ver>-x86_64-unknown-linux-gnu.tar.gz` | Linux build with the window (GTK 4.14+; `install.sh`, `.desktop`, icon) |
| `oprobe.rb` | Homebrew formula |
| `oprobe.json` | Scoop manifest |
| `install.sh` / `install.ps1` | one-line installers (verify checksums) |

Every build has the CLI and the terminal workbench (`oprobe top`); macOS and Windows builds also open a native
window (`oprobe gui`). The Linux `.deb`, `.rpm` and static archives are headless (no GTK needed).

Archives contain `oprobe` (`oprobe.exe`), `README.md`, `LICENSE`, `CHANGELOG.md`, `man/` pages,
`completions/` for bash, zsh, fish, PowerShell and elvish, `profiles/` and `docs/`.

## From source

```bash
cargo install oprobe --locked                  # Rust ≥ 1.97 and a C compiler; no OpenSSL
cargo install oprobe --locked --features gui   # + native window (Linux: libgtk-4-dev)
# or straight from a release tag:
cargo install --locked --git https://github.com/hxddh/oprobe --tag v2.0.1 oprobe
```

## Homebrew

```bash
curl -LO https://github.com/hxddh/oprobe/releases/latest/download/oprobe.rb
brew tap-new "$USER/tools" --no-git
cp oprobe.rb "$(brew --repository "$USER/tools")/Formula/"
brew install "$USER/tools/oprobe"
```

## Shell completions (archives)

```bash
# bash
cp completions/oprobe.bash ~/.local/share/bash-completion/completions/oprobe
# zsh (directory in $fpath)
cp completions/_oprobe ~/.zfunc/
# fish
cp completions/oprobe.fish ~/.config/fish/completions/
# PowerShell
. ./_oprobe.ps1   # add to $PROFILE (Windows archives: next to oprobe.exe)
```

`deb`, `rpm`, `pkg` and Homebrew install completions and man pages automatically.

## Verifying downloads

```bash
sha256sum --ignore-missing -c SHA256SUMS                 # Linux
shasum -a 256 --ignore-missing -c SHA256SUMS             # macOS
Get-FileHash oprobe-2.0.1-x64.msi -Algorithm SHA256       # Windows
```

## Unsigned macOS and Windows packages

The `.pkg`, `.dmg` and `.msi` are not yet signed with a developer certificate or notarized, so the first open asks
for confirmation:

- **macOS 15 (Sequoia) and later**: open `oprobe.app` (or the `.pkg`) once and dismiss the warning, then
  *System Settings → Privacy & Security* → *Open Anyway* next to the oprobe message, and confirm. (Right-click →
  *Open* no longer bypasses the check on macOS 15.)
- **macOS 14**: right-click `oprobe.app` → *Open* → *Open*.
- Either version, from a terminal: `xattr -dr com.apple.quarantine /Applications/oprobe.app`.
- **Windows**: SmartScreen → *More info* → *Run anyway*.

Homebrew, Scoop, the install scripts and `cargo install` are not affected.
