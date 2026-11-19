#!/usr/bin/env bash
set -euo pipefail

REPO_HTTPS="https://github.com/hxddh/oprobe.git"
REPO_SSH="git@github.com:hxddh/oprobe.git"
TAG="v0.1.0"

REMOTE_URL="${OPROBE_REPO_URL:-$REPO_HTTPS}"
SKIP_VERIFY=0

for arg in "$@"; do
  case "$arg" in
    --ssh) REMOTE_URL="$REPO_SSH" ;;
    --skip-verify) SKIP_VERIFY=1 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

if [[ "${SKIP_VERIFY}" -eq 0 ]] && command -v cargo >/dev/null 2>&1; then
  echo "==> Running local Cargo verification & generating Cargo.lock before tagging ${TAG}..."
  cargo test --all-targets
fi

echo "==> Initializing / updating Git repository in ${PROJECT_ROOT}"
if [[ ! -d .git ]]; then
  git init -b main
fi

if git remote get-url origin >/dev/null 2>&1; then
  git remote set-url origin "${REMOTE_URL}"
else
  git remote add origin "${REMOTE_URL}"
fi

git add -A
if ! git diff --cached --quiet; then
  git commit -m "feat: release oprobe ${TAG} (OpenDAL white-box storage stack profiler & auto-tuner)"
fi

git branch -M main

echo "==> Checking remote state on ${REMOTE_URL}"
if git ls-remote --exit-code --heads origin main >/dev/null 2>&1; then
  echo "==> Merging existing remote main history if present..."
  git fetch origin main
  git merge origin/main --allow-unrelated-histories --no-edit || true
fi

git tag -f -a "${TAG}" -m "Release ${TAG}: Apache OpenDAL Storage Stack Profiler & SLA Auto-Tuner"

echo "==> Pushing main branch and tag ${TAG} to ${REMOTE_URL}"
git push -u origin main
git push -f origin "${TAG}"

echo ""
echo "Successfully pushed ${TAG} to https://github.com/hxddh/oprobe"
echo "Track multi-platform binary build & automatic GitHub Release at:"
echo "  https://github.com/hxddh/oprobe/actions"
echo "  https://github.com/hxddh/oprobe/releases/tag/${TAG}"
