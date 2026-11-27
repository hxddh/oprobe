# Installing oprobe

## Quick

| Platform | Command |
|---|---|
| macOS, Linux | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` |
| macOS, Linux (CLI only) | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh -s -- --cli` |
| Windows | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex`, or the `.msi` |
| Debian, Ubuntu, Fedora, RHEL | the `.deb` / `.rpm` from the release page |
| Rust ≥ 1.97 (from source) | `cargo install --locked --git https://github.com/hxddh/oprobe --tag v9.3.3 oprobe` |

Then try it with no account: `oprobe demo` ([getting started](getting-started.md)).

## The app and the CLI-only build

Every platform has two builds of the same program:

- **the app build** (the default everywhere): the four commands plus the app, in a native window
  on macOS and Windows and in the terminal everywhere ([app](app.md));
- **the CLI-only build** (`oprobe-cli-<ver>-<target>` archives, `install.sh --cli`,
  `install.ps1 -Cli`, `cargo install … --no-default-features --features cli`): the four
  commands, text and JSON, no app. It is smaller and pulls in no UI toolkit, for servers, CI and
  containers.

Both include the common cloud services (S3, GCS, Azure Blob, OSS, COS, OBS, B2, WebDAV) plus
`fs`, `memory` and the lab. `--features all-services` adds the rest.

## The install scripts

`install.sh` (Linux, macOS) and `install.ps1` (Windows) download the archive for your platform,
verify it against `SHA256SUMS`, verify its build provenance when they can
([verifying](#verifying-a-download)) and install `oprobe` (Linux and macOS: `~/.local/bin`;
Windows: `%LOCALAPPDATA%\Programs\oprobe`). `sh install.sh --help` and
`Get-Help ./install.ps1` print every option.

| Option | Variable | Effect |
|---|---|---|
| `--cli` / `-Cli` | `OPROBE_FLAVOR=cli` | install the CLI-only build |
| `--version X.Y.Z` / `-Version` | `OPROBE_VERSION` | install this version instead of the latest |
| `--dir DIR` / `-InstallDir` | `OPROBE_INSTALL_DIR` | install somewhere else |
| `--no-modify-path` | `OPROBE_MODIFY_PATH=0` | never touch `PATH` (`=1`: add it without asking) |
| | `OPROBE_RELEASE_BASE=URL` | release download base (mirrors, tests; default `https://github.com/hxddh/oprobe/releases`) |
| | `OPROBE_VERIFY=auto\|require\|checksum` | after the checksum: verify the provenance when this machine can (default), insist on it, or skip it |

They find the latest version without the rate-limited GitHub API when they can: the release's
`latest.json`, then the `releases/latest` redirect, then the API.

**PATH.** When the install directory is not on `PATH`, `install.sh` offers to append it to your
shell's rc file (`~/.zshrc`, `~/.bashrc` or `~/.bash_profile` on macOS,
`~/.config/fish/config.fish`, else `~/.profile`). Piped from `curl`, it asks on the terminal when
there is one; without a terminal (CI, `docker build`) it changes nothing and prints the line to
add. `install.ps1` adds its directory to the user `PATH` and to the current session. It installs
the native ARM64 build on ARM64 Windows.

**Updating.** Run the install script again, install the newer package, or run `cargo install`
with the newer tag.

## Release assets

| Asset | Platform |
|---|---|
| `oprobe-<ver>-x86_64-unknown-linux-musl.tar.gz` / `aarch64-…-musl` | Linux, static (any distribution); the app in the terminal |
| `oprobe-<ver>-x86_64-unknown-linux-gnu.tar.gz` / `aarch64-…-gnu` | Linux, glibc ≥ 2.35 |
| `oprobe_<ver>-1_amd64.deb` / `_arm64.deb` | Debian, Ubuntu |
| `oprobe-<ver>-1.x86_64.rpm` / `.aarch64.rpm` | Fedora, RHEL, SUSE |
| `oprobe-desktop-<ver>-x86_64-unknown-linux-gnu.tar.gz` | Linux with the window (GTK 4.14+; `install.sh`, `.desktop` entry, icon) |
| `oprobe-<ver>-universal-apple-darwin.tar.gz` | macOS (Apple Silicon and Intel) |
| `oprobe-<ver>-aarch64-apple-darwin.tar.gz` / `x86_64-apple-darwin` | macOS, one architecture |
| `oprobe-<ver>-macos-universal.pkg` | macOS installer: `/usr/local/bin/oprobe` |
| `oprobe-<ver>-macos-universal.dmg` | macOS app: `oprobe.app` (the same binary as the `.pkg`) |
| `oprobe-<ver>-x64.msi` / `-arm64.msi` | Windows installer: `PATH`, Start menu entry, removable in *Apps* |
| `oprobe-<ver>-x86_64-pc-windows-msvc.zip` / `aarch64-…` | Windows portable |
| `oprobe-cli-<ver>-<target>.tar.gz` / `.zip` | the CLI-only build for each target above (`universal-apple-darwin` on macOS) |
| `latest.json` | machine-readable index: version, every asset's URL, size and SHA-256 |
| `install.sh` / `install.ps1` | the install scripts |
| `SHA256SUMS` | checksums of every asset |

Archives contain `oprobe` (`oprobe.exe`), `README.md`, `LICENSE` and `CHANGELOG.md`.

## Windows

```powershell
irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 | iex   # no admin rights
& ([scriptblock]::Create((irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1))) -Cli
```

The `.msi` installs for all users (Start menu entry, `PATH`, removable in *Apps*).

## Debian, Ubuntu, Fedora

```bash
sudo apt install ./oprobe_<ver>-1_amd64.deb
sudo dnf install ./oprobe-<ver>-1.x86_64.rpm
```

## From source

```bash
cargo install --locked --git https://github.com/hxddh/oprobe --tag v9.3.3 oprobe                                         # the app in the terminal
cargo install --locked --git https://github.com/hxddh/oprobe --tag v9.3.3 oprobe --features gui                          # plus native windows (Linux: libgtk-4-dev)
cargo install --locked --git https://github.com/hxddh/oprobe --tag v9.3.3 oprobe --no-default-features --features cli    # the CLI only
```

oprobe is not on crates.io. To embed oprobe's instrumentation in your own program use
[`oprobe-trace`](embedding.md).

## Verifying a download

The install scripts always check the SHA-256 and, when the GitHub CLI is installed and logged in,
the build provenance. A check that runs and fails stops the install; a check that cannot run is a
warning, or an error with `OPROBE_VERIFY=require`.

**Checksum.** Detects corrupted or truncated downloads. `SHA256SUMS` comes from the same release
as the archive, so it proves the download is intact, not who built it.

```bash
sha256sum --ignore-missing -c SHA256SUMS                    # Linux
shasum -a 256 --ignore-missing -c SHA256SUMS                # macOS
Get-FileHash oprobe-9.3.2-x64.msi -Algorithm SHA256          # Windows
```

**Build provenance.** Every asset has a [SLSA build provenance](https://slsa.dev/) attestation
signed by GitHub Actions, which proves it was built by this repository's release workflow from a
given commit:

```bash
gh attestation verify oprobe-9.3.2-x86_64-unknown-linux-musl.tar.gz --repo hxddh/oprobe \
  --signer-workflow hxddh/oprobe/.github/workflows/release.yml
```

**minisign.** The release workflow can also sign `SHA256SUMS` with minisign
(`SHA256SUMS.minisig`) and embed the public key in the install scripts, which then verify the
signature when `gh` is not available. This happens only for releases made with a signing key
configured; a release without `SHA256SUMS.minisig` is not signed, and its install scripts check
no signature. The release notes say which applies.

These say who built a file; they are not operating-system code signing (below).

## First open of unsigned packages

The project has no Apple or Microsoft code-signing certificate, so the `.pkg`, `.dmg` and `.msi`
are **not signed or notarized**. The operating system asks for one confirmation the first time:

- **macOS 15 (Sequoia) and later**: open `oprobe.app` (or the `.pkg`) once and close the warning;
  then open *System Settings → Privacy & Security*, scroll to the message about oprobe, click
  **Open Anyway** and confirm with your password.
- **macOS 14 and earlier**: right-click `oprobe.app` → *Open* → *Open*.
- **Either, from a terminal**: `xattr -dr com.apple.quarantine /Applications/oprobe.app`.
- **Windows**: SmartScreen says "Windows protected your PC": click **More info** → **Run anyway**.

The install scripts, `cargo install` and the plain archives used from a terminal are not affected.
The app is signed ad hoc, so macOS may refuse to verify its developer, but never reports it as
damaged. Verify the download ([above](#verifying-a-download)) before you
confirm.
