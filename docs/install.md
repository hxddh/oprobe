# Installing oprobe

## Quick

| Platform | Command |
|---|---|
| macOS, Linux | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` |
| Homebrew | `brew install hxddh/tap/oprobe` (the app: `brew install --cask hxddh/tap/oprobe-app`) |
| Windows | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex`, or the `.msi` |
| Windows, Scoop | `scoop bucket add hxddh https://github.com/hxddh/scoop-bucket` then `scoop install oprobe` |
| Windows, winget | `winget install hxddh.oprobe` — only once the package is accepted into winget-pkgs ([below](#windows)) |
| Rust ≥ 1.97 | `cargo install oprobe --locked` (`--features gui` adds the window on Linux; needs `libgtk-4-dev`) |

Then try it with no account: `oprobe demo` ([getting started](getting-started.md)).

### The install scripts

`install.sh` and `install.ps1` download the archive for your platform, verify it against `SHA256SUMS`, check
its build provenance or signature ([verifying](#verifying-a-download)) and install `oprobe` (Linux and macOS: `~/.local/bin`; Windows: `%LOCALAPPDATA%\Programs\oprobe`). Update later
with `oprobe self-update`.

They find the latest version without the rate-limited GitHub API when they can, trying in order and falling
back on any failure: the release's `latest.json`, the `releases/latest` redirect, then the API.

| Variable | Effect |
|---|---|
| `OPROBE_VERSION=5.0.0` | install this version instead of the latest |
| `OPROBE_INSTALL_DIR=DIR` | install somewhere else |
| `OPROBE_MODIFY_PATH=1` / `0` | add the directory to `PATH` without asking / never touch `PATH` |
| `OPROBE_RELEASE_BASE=URL` | release download base (mirrors, tests; default `https://github.com/hxddh/oprobe/releases`) |
| `OPROBE_VERIFY=auto` / `require` / `checksum` | after the checksum: verify the provenance or signature when this machine can (default), insist on it, or skip it |
| `OPROBE_MINISIGN_PUBKEY=KEY` | the minisign public key to verify `SHA256SUMS.minisig` with (overrides the embedded one) |

**PATH.** When the install directory is not on `PATH`, `install.sh` offers to append it to your shell's rc
file: `~/.zshrc` (zsh), `~/.bashrc` (bash; `~/.bash_profile` on macOS), `~/.config/fish/config.fish` (fish),
else `~/.profile`. Piped from `curl`, the script asks on the terminal (`/dev/tty`) when there is one; without
a terminal (CI, `docker build`) it changes nothing and prints the line to add, unless `OPROBE_MODIFY_PATH=1`.
`install.ps1` adds its directory to the user `PATH` and to the current session's `$env:Path`, so `oprobe`
works in the same window (`OPROBE_MODIFY_PATH=0` to skip). It detects ARM64 Windows from the OS (also under
x64 emulation) and installs the native ARM64 build.

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
| `oprobe.rb` / `oprobe-app.rb` | Homebrew formula `oprobe` (CLI) and cask `oprobe-app` (`oprobe.app` from the `.dmg`) |
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

The `hxddh/homebrew-tap` tap (`github.com/hxddh/homebrew-tap`) is updated by the release workflow:

```bash
brew install hxddh/tap/oprobe               # the command line (macOS and Linux)
brew install --cask hxddh/tap/oprobe-app    # oprobe.app, the native window (macOS)
```

Update with `brew upgrade oprobe` (`brew upgrade --cask oprobe-app`). The formula and the cask have different
names, so both can be installed. Each release also attaches them (`oprobe.rb`, `oprobe-app.rb`), for a
local tap if you cannot use this one.

## Windows

```powershell
irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 | iex   # no admin rights
scoop bucket add hxddh https://github.com/hxddh/scoop-bucket; scoop install oprobe
```

The `.msi` installs for all users (Start menu entry, `PATH`, removable in *Apps*).

**winget:** each release opens a pull request to [winget-pkgs](https://github.com/microsoft/winget-pkgs);
`winget install hxddh.oprobe` works only after Microsoft's review accepts the package. Until then (check with
`winget search hxddh.oprobe`), use one of the commands above or the `.msi`.

## Debian, Ubuntu, Fedora

```bash
sudo apt install ./oprobe_<ver>-1_amd64.deb
sudo dnf install ./oprobe-<ver>-1.x86_64.rpm
```

## From source

```bash
cargo install oprobe --locked
cargo install oprobe --locked --features gui
cargo install --locked --git https://github.com/hxddh/oprobe --tag v5.0.0 oprobe
```

## Updating

`oprobe self-update` finds the release (the release's `latest.json` first, then the GitHub API), downloads
the archive for this platform, verifies its SHA-256 (from `latest.json`, or the release's `SHA256SUMS`), then
its build provenance or signature like the install scripts (`--verify auto|require|checksum`, or
`OPROBE_VERIFY`; the result's `verified_by` says which check passed), checks that the new binary starts, and
replaces the running one in place (`oprobe self-update --check` only reports; `--to v5.0.1` picks a release;
`-o json` for scripts). A GitHub refusal (403, rate limit) is a
network error (`net.unreachable`, retryable): set `GITHUB_TOKEN` or retry later. When
oprobe was installed by a package manager (Homebrew, apt/dnf, the `.pkg` or `.app`, Scoop, winget, the MSI,
cargo), it refuses and prints that manager's update command instead.

`SHA256SUMS` comes from the same GitHub release as the archive, so the checksum alone proves the download
is intact, not who built it; the provenance attestation and the minisign signature below prove that. `OPROBE_RELEASE_BASE` replaces the release
download base (default `https://github.com/hxddh/oprobe/releases`) and `OPROBE_UPDATE_API` the GitHub API base
(default `https://api.github.com`; set alone, `latest.json` is not fetched from github.com). They exist for
internal mirrors and tests — point them only at a server you trust, since they decide which binary is
installed.

## Verifying a download

Three checks, from weakest to strongest. The install scripts and `oprobe self-update` run them
automatically (design v5 §5): always the checksum, then the provenance when the GitHub CLI is installed and
logged in, otherwise the minisign signature when `minisign` (or, for self-update, the embedded verifier) and
the release's signature are available. A check that runs and fails always stops the install; a check that
cannot run is a warning, or an error with `OPROBE_VERIFY=require` / `--verify require`.

**Checksum.** Detects corrupted or truncated downloads; `SHA256SUMS` comes from the same release.

```bash
sha256sum --ignore-missing -c SHA256SUMS                    # Linux
shasum -a 256 --ignore-missing -c SHA256SUMS                # macOS
Get-FileHash oprobe-5.0.0-x64.msi -Algorithm SHA256          # Windows
```

**Build provenance.** Every asset has a [SLSA build provenance](https://slsa.dev/) attestation signed by
GitHub Actions, which proves it was built by this repository's release workflow from a given commit:

```bash
gh attestation verify oprobe-5.0.0-x86_64-unknown-linux-musl.tar.gz --repo hxddh/oprobe \
  --signer-workflow hxddh/oprobe/.github/workflows/release.yml
```

**minisign.** When the release is signed, `SHA256SUMS.minisig` is a [minisign](https://jedisct1.github.io/minisign/)
signature over `SHA256SUMS`, made with a key held only by the release workflow. The public key is embedded in
`install.sh`, `install.ps1` and the `oprobe` binary of the release (and printed in the release notes):

```bash
minisign -Vm SHA256SUMS -P <public key>     # then check the archive against SHA256SUMS as above
```

These signatures say who built a file; they are not operating-system code signing (below).

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
not affected. Verify the download ([above](#verifying-a-download)) before you confirm.

## Shell completions (archives)

```bash
cp completions/oprobe.bash ~/.local/share/bash-completion/completions/oprobe   # bash
cp completions/_oprobe ~/.zfunc/                                               # zsh (a directory in $fpath)
cp completions/oprobe.fish ~/.config/fish/completions/                         # fish
. ./_oprobe.ps1                                                                # PowerShell ($PROFILE)
```

Packages (deb, rpm, pkg, Homebrew) install completions and man pages themselves. `oprobe completions SHELL`
and `oprobe man` generate them on any install. The scripts are dynamic: they ask `oprobe` for candidates as
you type, so target names, run ids, labels and finding codes complete (`oprobe` must be on `PATH`);
`oprobe completions SHELL --static` prints the fixed script of earlier versions.
