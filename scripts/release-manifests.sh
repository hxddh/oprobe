#!/usr/bin/env bash
# Generate the release index from a release's assets and SHA256SUMS:
#   latest.json           machine-readable index: version + every asset's URL, size and SHA-256
#                         (install.sh and install.ps1 read it first)
# usage: scripts/release-manifests.sh <version> <dist-dir> <owner/repo>
set -euo pipefail
version="$1"; dist="$(cd "$2" && pwd)"; repo="$3"
url="https://github.com/$repo/releases/download/v$version"

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
echo "wrote $dist/latest.json"
