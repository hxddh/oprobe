# Releasing (maintainers)

## How a release happens

`.github/workflows/release.yml` builds and publishes a release when:

- a commit lands on `main` whose `Cargo.toml` version has no `v<version>` tag yet (the workflow
  tags it);
- a `v*` tag is pushed by hand;
- the workflow is run manually (optionally naming an existing tag to rebuild).

## Checklist

1. Set the version in one place, `workspace.package.version` of the root `Cargo.toml`. Run
   `cargo build` (updates `Cargo.lock`).
2. Title and date the section at the top of `CHANGELOG.md` (`## Unreleased` becomes
   `## vX.Y.Z — <date>`): it becomes the release notes.
3. Refresh generated files if anything changed: `OPROBE_BLESS=1 cargo test --test cli` (schemas)
   and `OPROBE_BLESS=1 cargo test --test docs` (`llms-full.txt`).
4. Check locally: `cargo package -p oprobe-trace` and
   `cargo bench -p oprobe-trace --bench overhead -- --quick`.
5. Merge. The workflow tests, builds every platform (the app build and the CLI-only build of each
   target), writes `latest.json`, attests every asset and publishes the GitHub release; then
   `oprobe-trace` goes to crates.io when `CARGO_REGISTRY_TOKEN` is set (without it the job ends
   with a warning, not a failure). No package manager is updated: the release assets and the
   install scripts are the only channel of the binary.

## Crates

| Crate | Published | Promise |
|---|---|---|
| `oprobe-trace` | yes, when the release has `CARGO_REGISTRY_TOKEN` | semver for the documented API; an `oprobe.trace/9` file is read by the version that wrote it, with no promise across versions |
| `oprobe-core` | no (`publish = false`) | none: internal, documented as such |
| `oprobe` | no (`publish = false`): release assets, or `cargo install --git` | the CLI and the JSON output are stable across 9.x; `.oprobe` / `.prove.json` files are read by the version that wrote them, with no promise across versions |

The crates-io job publishes `oprobe-trace` only; a version already on crates.io is skipped, so a
re-run is safe. CI runs `cargo package -p oprobe-trace` on every pull request.

## Secrets

| Secret | Job | What it needs |
|---|---|---|
| `CARGO_REGISTRY_TOKEN` | crates-io | a crates.io API token with publish rights for `oprobe-trace` |
| `MINISIGN_KEY` | publish (sign `SHA256SUMS`) | an unencrypted minisign secret key, see [Signing](#signing) |

Repository **variable** (not a secret): `MINISIGN_PUBKEY`, the public half of `MINISIGN_KEY` (one
base64 line). The release embeds it in `install.sh` and `install.ps1`.

## Signing

Nothing is code-signed by an OS vendor, but downloads are verifiable:

- every asset gets a SLSA build provenance attestation (`actions/attest-build-provenance`), which
  `install.sh` and `install.ps1` check with `gh attestation verify --signer-workflow
  hxddh/oprobe/.github/workflows/release.yml` when the GitHub CLI is logged in;
- with `MINISIGN_KEY` set, the publish job signs `SHA256SUMS` into `SHA256SUMS.minisig`, and with
  `MINISIGN_PUBKEY` set the installers embed the public key and verify the signature when `gh` is
  unavailable. Without them the release is unsigned, the installers check no signature, and the
  release notes say so. Do not describe a minisign fallback anywhere unless both are set.

Create the key once, offline:

```bash
minisign -G -W -p minisign.pub -s minisign.key   # -W: no password (the workflow cannot type one)
gh secret set MINISIGN_KEY < minisign.key        # then delete minisign.key or keep it offline
gh variable set MINISIGN_PUBKEY --body "$(tail -n1 minisign.pub)"
```

## Supply chain

- Every action in `.github/workflows/` is pinned by commit SHA (the tag in a comment); Dependabot
  (`.github/dependabot.yml`) proposes updates weekly. CI fails on an unpinned `uses:`.
- No release job restores a cache: every release binary is built from a clean checkout.
- `cargo deny check` (`deny.toml`) runs on every pull request: advisories, licenses, bans and
  sources. Ignored advisories are listed in `deny.toml` with the reason.

## Assets

For each target: the app archive `oprobe-<ver>-<target>` and the CLI-only archive
`oprobe-cli-<ver>-<target>` (`--no-default-features --features cli`: the cloud services without
the app), `.tar.gz` on Linux and macOS, `.zip` on Windows. Plus the `.deb` and `.rpm` (Linux gnu
targets), the macOS universal archives, `.pkg` and `.dmg` (with `oprobe.app`, smoke-launched
through LaunchServices in CI), the `.msi` (WiX 5), the Linux desktop tarball, `latest.json` (written by
`scripts/release-manifests.sh`), `install.sh`, `install.ps1`, `SHA256SUMS` (covering everything)
and `SHA256SUMS.minisig` (when signed).

`latest.json` is load-bearing: `install.sh` and `install.ps1` read
`releases/latest/download/latest.json` before the rate-limited API. Keep its `version` field and
its asset `name` / `url` / `sha256` fields stable, and keep the archive names
(`oprobe-<ver>-<target>`, `oprobe-cli-<ver>-<target>`): the installers build them. CI
(`install-script` jobs) runs both installers against a local fake release
(`scripts/test-install.sh`, `scripts/test-install.ps1`).
