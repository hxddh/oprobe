# Installing oprobe

## Quick

| Platform | Command |
|---|---|
| macOS, Linux | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` |
| Windows | `winget install hxddh.oprobe` |
| Windows (no winget) | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex` |
| Rust ≥ 1.97 | `cargo install oprobe --locked` (`--features gui` adds the window on Linux; needs `libgtk-4-dev`) |

The install scripts download the archive for your platform, verify it against `SHA256SUMS` and put `oprobe`
in `~/.local/bin` (`OPROBE_INSTALL_DIR` to change). Update later with `oprobe self-update`.

## Release assets

Every release publishes the same program in each platform's native format:

| Asset | Platform |
|---|---|
| `oprobe-<ver>-x86_64-unknown-linux-musl.tar.gz` / `aarch64-…-musl` | Linux, static (any distribution) |
| `oprobe-<ver>-x86_64-unknown-linux-gnu.tar.gz` / `aarch64-…-gnu` | Linux, glibc ≥ 2.35 |
| `oprobe_<ver>-1_amd64.deb` / `_arm64.deb` | Debian, Ubuntu |
| `oprobe-<ver>-1.x86_64.rpm` / `.aarch64.rpm` | Fedora, RHEL, SUSE |
| `oprobe-desktop-<ver>-x86_64-unknown-linux-gnu.tar.gz` | Linux with the window (GTK 4.14+; `install.sh`, `.desktop` entry, icon) |
| `oprobe-<ver>-universal-apple-darwin.tar.gz` | macOS (Apple Silicon + Intel) |
| `oprobe-<ver>-aarch64-apple-darwin.tar.gz` / `x86_64-apple-darwin` | macOS single-architecture |
| `oprobe-<ver>-macos-universal.pkg` | macOS installer: `/usr/local/bin/oprobe`, man page, zsh completion |
| `oprobe-<ver>-macos-universal.dmg` | macOS app: `oprobe.app` (same binary as the `.pkg`) |
| `oprobe-<ver>-x64.msi` / `-arm64.msi` | Windows installer: `PATH`, Start menu entry, removable in *Apps* |
| `oprobe-<ver>-x86_64-pc-windows-msvc.zip` / `aarch64-…` | Windows portable |
| `oprobe.rb` / `oprobe.cask.rb` | Homebrew formula (CLI) and cask (`oprobe.app` from the `.dmg`) |
| `oprobe.json` | Scoop manifest |
| `winget-manifests.zip` | winget manifests (version, installer, locale) for the `.msi` |
| `latest.json` | machine-readable index: version, every asset's URL, size and SHA-256 |
| `install.sh` / `install.ps1` | the install scripts |
| `SHA256SUMS` | checksums of every asset |

Every build has the CLI and the terminal workbench (`oprobe top`). macOS and Windows builds also open the native
window (`oprobe gui`); the Linux `.deb`, `.rpm` and plain archives are headless (no GTK needed). Archives contain
`oprobe` (`oprobe.exe`), `README.md`, `LICENSE`, `CHANGELOG.md`, `man/`, `completions/` (bash, zsh, fish,
PowerShell, elvish), `profiles/` and `docs/`.

## Homebrew

Each release ships a formula (the CLI) and a cask (the app). Until they are in a public tap, install them from a
local tap:

```bash
brew tap-new "$USER/tools" --no-git
base=https://github.com/hxddh/oprobe/releases/latest/download
curl -fsSL "$base/oprobe.rb" -o "$(brew --repository "$USER/tools")/Formula/oprobe.rb"
curl -fsSL "$base/oprobe.cask.rb" --create-dirs -o "$(brew --repository "$USER/tools")/Casks/oprobe.rb"
brew install "$USER/tools/oprobe"            # the command line
brew install --cask "$USER/tools/oprobe"     # oprobe.app
```

Update with `brew upgrade oprobe` (`brew upgrade --cask oprobe`).

## Windows

```powershell
winget install hxddh.oprobe        # the MSI; update: winget upgrade oprobe
scoop install https://github.com/hxddh/oprobe/releases/latest/download/oprobe.json
```

Maintainers submit each release to [winget-pkgs](https://github.com/microsoft/winget-pkgs) — see
[releasing.md](releasing.md#winget).

## Debian, Ubuntu, Fedora

```bash
sudo apt install ./oprobe_<ver>-1_amd64.deb
sudo dnf install ./oprobe-<ver>-1.x86_64.rpm
```

## From source

```bash
cargo install oprobe --locked
cargo install oprobe --locked --features gui
cargo install --locked --git https://github.com/hxddh/oprobe --tag v3.0.0 oprobe
```

## Updating

`oprobe self-update` checks GitHub Releases, downloads the archive for this platform, verifies it against
`SHA256SUMS`, checks that the new binary starts, and replaces the running one in place
(`oprobe self-update --check` only reports; `--to v3.0.1` picks a release; `-o json` for scripts). When
oprobe was installed by a package manager (Homebrew, apt/dnf, the `.pkg` or `.app`, Scoop, winget, the MSI,
cargo), it refuses and prints that manager's update command instead.

`SHA256SUMS` comes from the same GitHub release as the archive, so the check proves the download is
intact, not who built it: oprobe releases are not code-signed (no signing certificates), and the
checksums are as trustworthy as the GitHub release itself. `OPROBE_UPDATE_API` replaces the GitHub API
base URL (default `https://api.github.com`); it exists for internal mirrors and tests — point it only at a
server you trust, since it decides which binary is installed.

## Verifying downloads

The checksums detect corrupted or truncated downloads; they are not a signature (see above).

```bash
sha256sum --ignore-missing -c SHA256SUMS                    # Linux
shasum -a 256 --ignore-missing -c SHA256SUMS                # macOS
Get-FileHash oprobe-3.0.0-x64.msi -Algorithm SHA256          # Windows
```

Every asset also has a [SLSA build provenance](https://slsa.dev/) attestation signed by GitHub Actions, which
proves it was built by this repository's release workflow from a given commit:

```bash
gh attestation verify oprobe-3.0.0-x86_64-unknown-linux-musl.tar.gz --repo hxddh/oprobe
```

## First open of unsigned packages

The project has no Apple or Microsoft code-signing certificate, so the `.pkg`, `.dmg` and `.msi` are **not
signed or notarized**. The operating system asks for one confirmation the first time:

- **macOS 15 (Sequoia) and later**: open `oprobe.app` (or the `.pkg`) once and close the warning; then open
  *System Settings → Privacy & Security*, scroll to the message about oprobe, click **Open Anyway** and
  confirm with your password. (Right-click → *Open* no longer skips the check on macOS 15.)
- **macOS 14 and earlier**: right-click `oprobe.app` → *Open* → *Open*.
- **Either, from a terminal**: `xattr -dr com.apple.quarantine /Applications/oprobe.app`.
- **Windows**: SmartScreen says "Windows protected your PC": click **More info** → **Run anyway**.

Homebrew, Scoop, winget, the install scripts, `cargo install` and the plain archives used from a terminal are
not affected. Verify the download (above) before you confirm.

## Shell completions (archives)

```bash
cp completions/oprobe.bash ~/.local/share/bash-completion/completions/oprobe   # bash
cp completions/_oprobe ~/.zfunc/                                               # zsh (a directory in $fpath)
cp completions/oprobe.fish ~/.config/fish/completions/                         # fish
. ./_oprobe.ps1                                                                # PowerShell ($PROFILE)
```

Packages (deb, rpm, pkg, Homebrew) install completions and man pages themselves. `oprobe completions SHELL`
and `oprobe man` generate them on any install.
