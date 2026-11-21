#!/bin/sh
# End-to-end test of scripts/install.sh against a local fake release (CI: `install-script` job).
# Serves a fake release with python's http.server and checks every version-resolution path
# (latest.json, the releases/latest redirect, the API, OPROBE_VERSION) and the PATH handling.
#   scripts/test-install.sh            (needs python3, tar, sha256sum or shasum)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
installer="$here/install.sh"
work=$(mktemp -d)
pid=""
cleanup() { [ -n "$pid" ] && kill "$pid" 2>/dev/null; rm -rf "$work"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64|Linux-amd64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-*) target=universal-apple-darwin ;;
  *) fail "unsupported platform $(uname -s) $(uname -m)" ;;
esac

# A fake release: <root>/releases/download/v<ver>/{archive,SHA256SUMS}, latest.json, an API answer.
mkrel() {
  v="$1"; name="oprobe-$v-$target"
  mkdir -p "$work/stage/$name" "$work/root/releases/download/v$v"
  printf '#!/bin/sh\necho "oprobe %s"\n' "$v" > "$work/stage/$name/oprobe"
  chmod 755 "$work/stage/$name/oprobe"
  tar -C "$work/stage" -czf "$work/root/releases/download/v$v/$name.tar.gz" "$name"
  (cd "$work/root/releases/download/v$v" &&
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$name.tar.gz"; else shasum -a 256 "$name.tar.gz"; fi > SHA256SUMS)
}
mkrel 9.1.0   # latest.json
mkrel 9.2.0   # the redirect
mkrel 9.3.0   # the API
mkrel 9.0.0   # OPROBE_VERSION
mkdir -p "$work/root/releases/latest/download" "$work/root/api/repos/hxddh/oprobe/releases"
printf '{"schema":"oprobe.release/1","version":"9.1.0","tag":"v9.1.0","assets":[]}\n' \
  > "$work/root/releases/latest/download/latest.json"
printf '{"tag_name": "v9.3.0", "html_url": "x", "assets": []}\n' \
  > "$work/root/api/repos/hxddh/oprobe/releases/latest"

# http.server plus GitHub's redirect: /releases/latest -> /releases/tag/v<REDIRECT_VERSION>
# (only while the file `redirect` exists).
cat > "$work/server.py" <<'PY'
import http.server, os, sys
root = sys.argv[2]
class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=root, **k)
    def log_message(self, *a):
        pass
    def redirect(self):
        if self.path.rstrip("/") == "/releases/latest" and os.path.exists(os.path.join(root, "redirect")):
            self.send_response(302)
            self.send_header("Location", "http://127.0.0.1:%s/releases/tag/v9.2.0" % sys.argv[1])
            self.end_headers()
            return True
        return False
    def do_GET(self):
        if not self.redirect():
            super().do_GET()
    def do_HEAD(self):
        if not self.redirect():
            super().do_HEAD()
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 "$work/server.py" "$port" "$work/root" &
pid=$!
i=0
until python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:$port/releases/latest/download/latest.json')" 2>/dev/null; do
  i=$((i + 1)); [ "$i" -lt 50 ] || fail "the fake release server did not start"; sleep 0.1
done
base="http://127.0.0.1:$port/releases"

# install CASE EXPECTED_VERSION [ENV=VALUE...]: a fresh HOME, no terminal (stdin from /dev/null).
install_case() {
  case_name="$1"; want="$2"; shift 2
  home="$work/home-$case_name"
  mkdir -p "$home"
  env -u XDG_CONFIG_HOME HOME="$home" SHELL=/bin/bash OPROBE_RELEASE_BASE="$base" OPROBE_API="http://127.0.0.1:$port/api" \
    "$@" sh "$installer" </dev/null > "$work/out-$case_name" 2>&1 ||
    { cat "$work/out-$case_name"; fail "$case_name: install.sh failed"; }
  got=$("$home/.local/bin/oprobe")
  [ "$got" = "oprobe $want" ] || { cat "$work/out-$case_name"; fail "$case_name: installed '$got', want $want"; }
  echo "ok: $case_name -> $got"
}

install_case latest-json 9.1.0 OPROBE_MODIFY_PATH=0
grep -q "is not on your PATH" "$work/out-latest-json" || fail "latest-json: no PATH advice"
[ ! -e "$work/home-latest-json/.bashrc" ] || fail "OPROBE_MODIFY_PATH=0 wrote a shell rc file"

rm "$work/root/releases/latest/download/latest.json"
touch "$work/root/redirect"
install_case redirect 9.2.0 OPROBE_MODIFY_PATH=1
rc="$work/home-redirect/.bashrc"
[ "$(uname -s)" = Darwin ] && rc="$work/home-redirect/.bash_profile"
grep -q "home-redirect/.local/bin" "$rc" || fail "OPROBE_MODIFY_PATH=1 did not update $rc"

rm "$work/root/redirect"
install_case api 9.3.0 OPROBE_MODIFY_PATH=1 SHELL=/usr/bin/fish
grep -q "fish_add_path" "$work/home-api/.config/fish/config.fish" || fail "fish config not updated"

install_case pinned 9.0.0 OPROBE_VERSION=v9.0.0 OPROBE_MODIFY_PATH=0

# A tampered archive is refused.
echo "0000000000000000000000000000000000000000000000000000000000000000  oprobe-9.0.0-$target.tar.gz" \
  > "$work/root/releases/download/v9.0.0/SHA256SUMS"
if env HOME="$work/home-bad" OPROBE_RELEASE_BASE="$base" OPROBE_VERSION=9.0.0 OPROBE_MODIFY_PATH=0 \
  sh "$installer" </dev/null >/dev/null 2>&1; then
  fail "a checksum mismatch was installed"
fi
echo "ok: checksum mismatch refused"
echo "install.sh: all cases passed"
