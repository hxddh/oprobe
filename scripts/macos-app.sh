#!/bin/sh
# Build oprobe.app around a (universal or single-arch) binary, sign it ad hoc and check that it
# really opens as a user would open it: through LaunchServices (`open`), with no arguments.
#   scripts/macos-app.sh <binary> <version> <out-dir>     -> <out-dir>/oprobe.app
# Used by ci.yml (gui job) and release.yml (macos-universal job).
set -eu
bin="$1"; version="$2"; out="$3"
here=$(cd "$(dirname "$0")/.." && pwd)
app="$out/oprobe.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/oprobe"
sed "s/@VERSION@/$version/g" "$here/packaging/desktop/Info.plist" > "$app/Contents/Info.plist"
iconset=$(mktemp -d)/oprobe.iconset
mkdir -p "$iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s "$here/packaging/desktop/oprobe.png" --out "$iconset/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$here/packaging/desktop/oprobe.png" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/oprobe.icns"
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

# Smoke launch: a bare launch must open the workbench window and keep running. A usage error,
# a crash or an early exit fails the build (v2.0.0 shipped an app that exited with a usage
# error when opened from Finder).
log=$(mktemp -d)
open --stdout "$log/out" --stderr "$log/err" "$app"
alive=no
for _ in 1 2 3 4 5 6 7 8; do
  sleep 1
  if pgrep -f "$app/Contents/MacOS/oprobe" >/dev/null; then alive=yes; else alive=no; break; fi
done
pkill -f "$app/Contents/MacOS/oprobe" 2>/dev/null || true
if [ "$alive" != yes ]; then
  echo "oprobe.app did not stay open when launched through LaunchServices" >&2
  echo "--- stdout"; cat "$log/out" >&2 || true
  echo "--- stderr"; cat "$log/err" >&2 || true
  exit 1
fi
echo "oprobe.app $version: signed (ad hoc) and opens through LaunchServices"
