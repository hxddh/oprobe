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
| `oprobe-<ver>-x64.msi` / `-arm64.msi` | Windows installer (adds to `PATH`, removable in *Apps*) |
| `oprobe-<ver>-x86_64-pc-windows-msvc.zip` / `aarch64-…` | Windows portable |
| `oprobe-app-<ver>-macos-universal.dmg` | desktop app for macOS (`oprobe.app`, Apple Silicon + Intel) |
| `oprobe-app-<ver>-x64.msi` / `oprobe-app-<ver>-x86_64-pc-windows-msvc.zip` | desktop app for Windows (Start menu shortcut) |
| `oprobe-app-<ver>-x86_64-unknown-linux-gnu.tar.gz` | desktop app for Linux (GTK 4.14+; `install.sh`, `.desktop`, icon) |
| `oprobe.rb` | Homebrew formula |
| `oprobe.json` | Scoop manifest |
| `install.sh` / `install.ps1` | one-line installers (verify checksums) |

Archives contain `oprobe` (`oprobe.exe`), `README.md`, `LICENSE`, `CHANGELOG.md`, `man/` pages,
`completions/` for bash, zsh, fish, PowerShell and elvish, `profiles/` and `docs/`.

## From source

```bash
cargo install oprobe --locked     # Rust ≥ 1.91; pure Rust, no C compiler or OpenSSL needed
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
. ./_oprobe.ps1   # add to $PROFILE
```

`deb`, `rpm`, `pkg` and Homebrew install completions and man pages automatically.

## Verifying downloads

```bash
sha256sum --ignore-missing -c SHA256SUMS                 # Linux
shasum -a 256 --ignore-missing -c SHA256SUMS             # macOS
Get-FileHash oprobe-2.0.0-x64.msi -Algorithm SHA256       # Windows
```
