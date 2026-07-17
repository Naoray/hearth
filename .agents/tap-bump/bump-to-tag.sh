#!/usr/bin/env bash
# bump-to-tag.sh <tag>
#
# Bumps the Hearth formula in Naoray/homebrew-tap to a given release tag.
# Downloads both arch tarballs from the GitHub Release, computes sha256, rewrites
# Formula/hearth.rb, commits to a branch, pushes, and opens a PR.
#
# Does NOT auto-merge. Maintainer approves.
#
# Usage:
#   bump-to-tag.sh v0.3.0
#
# Requires: gh, curl, git, python3, and sha256sum or shasum

set -Eeuo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

TAG="${1:-}"
if [[ -z "$TAG" ]]; then
  fail "usage: $0 <tag>   e.g. $0 v0.3.0"
fi

if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$ ]]; then
  fail "tag must look like vX.Y.Z or vX.Y.Z-rc.N (got: $TAG)"
fi

if [[ -z "${GH_TOKEN:-}" ]]; then
  fail "HOMEBREW_TAP_TOKEN is not configured (GH_TOKEN is empty)"
fi

for command in curl date gh git mktemp python3; do
  command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done

VERSION="${TAG#v}"
SOURCE_REPO="Naoray/hearth"
TAP_REPO="Naoray/homebrew-tap"
FORMULA="Formula/hearth.rb"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
FORMULA_UPDATER="${SCRIPT_DIR}/update_formula.py"

[[ -f "$FORMULA_UPDATER" ]] || fail "formula updater not found: $FORMULA_UPDATER"

TEMP_ROOT="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
[[ -d "$TEMP_ROOT" ]] || fail "temporary directory does not exist: $TEMP_ROOT"
WORK_DIR="$(mktemp -d "${TEMP_ROOT%/}/hearth-tap-bump.XXXXXX")"
trap 'rm -rf -- "$WORK_DIR"' EXIT

TAP_DIR="${TAP_DIR:-${WORK_DIR}/homebrew-tap}"
DOWNLOAD_DIR="${WORK_DIR}/downloads"
mkdir -p "$DOWNLOAD_DIR"

if [[ -e "$TAP_DIR" ]]; then
  fail "tap checkout path already exists; refusing to modify it: $TAP_DIR"
fi

if [[ -n "${GITHUB_RUN_ID:-}" ]]; then
  BRANCH_SUFFIX="${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT:-1}"
else
  BRANCH_SUFFIX="manual-$(date -u +%Y%m%d%H%M%S)-$$"
fi
BRANCH="automation/hearth-${VERSION}-${BRANCH_SUFFIX}"

ARM_ASSET="hearth-${TAG}-aarch64-apple-darwin.tar.gz"
X86_ASSET="hearth-${TAG}-x86_64-apple-darwin.tar.gz"
ARM_URL="https://github.com/${SOURCE_REPO}/releases/download/${TAG}/${ARM_ASSET}"
X86_URL="https://github.com/${SOURCE_REPO}/releases/download/${TAG}/${X86_ASSET}"

download_asset() {
  local asset="$1"
  local url="$2"
  local destination="${DOWNLOAD_DIR}/${asset}"

  echo "==> Downloading ${asset}"
  if ! curl \
    --fail \
    --location \
    --retry 4 \
    --retry-all-errors \
    --retry-delay 2 \
    --show-error \
    --silent \
    --output "$destination" \
    "$url"; then
    fail "release ${TAG} is missing or failed to download asset: ${asset}"
  fi

  [[ -s "$destination" ]] || fail "downloaded release asset is empty: ${asset}"
}

sha256_file() {
  local path="$1"
  local output

  if command -v sha256sum >/dev/null 2>&1; then
    output="$(sha256sum "$path")"
  elif command -v shasum >/dev/null 2>&1; then
    output="$(shasum -a 256 "$path")"
  else
    fail "required command not found: sha256sum or shasum"
  fi

  printf '%s\n' "${output%% *}"
}

echo "==> Verifying release assets for ${TAG}"
download_asset "$ARM_ASSET" "$ARM_URL"
download_asset "$X86_ASSET" "$X86_URL"

ARM_SHA="$(sha256_file "${DOWNLOAD_DIR}/${ARM_ASSET}")"
X86_SHA="$(sha256_file "${DOWNLOAD_DIR}/${X86_ASSET}")"

[[ "$ARM_SHA" =~ ^[0-9a-f]{64}$ ]] || fail "invalid arm64 sha256: $ARM_SHA"
[[ "$X86_SHA" =~ ^[0-9a-f]{64}$ ]] || fail "invalid x86_64 sha256: $X86_SHA"

echo "    arm64 sha256: $ARM_SHA"
echo "    x86_64 sha256: $X86_SHA"

echo "==> Verifying token access to ${TAP_REPO}"
if ! gh api "repos/${TAP_REPO}" --silent >/dev/null 2>&1; then
  fail "HOMEBREW_TAP_TOKEN cannot access ${TAP_REPO}"
fi

ASKPASS="${WORK_DIR}/git-askpass.sh"
cat >"$ASKPASS" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  *Username*) printf '%s\n' 'x-access-token' ;;
  *Password*) printf '%s\n' "${GH_TOKEN:?}" ;;
  *) exit 1 ;;
esac
EOF
chmod 700 "$ASKPASS"
export GIT_ASKPASS="$ASKPASS"
export GIT_TERMINAL_PROMPT=0

echo "==> Cloning tap repo ${TAP_REPO} -> ${TAP_DIR}"
if ! git clone --branch main --depth 1 "https://github.com/${TAP_REPO}.git" "$TAP_DIR"; then
  fail "failed to clone ${TAP_REPO} with HOMEBREW_TAP_TOKEN"
fi

cd "$TAP_DIR"
[[ -f "$FORMULA" ]] || fail "formula not found in tap repo: $FORMULA"

git config user.name "${GIT_AUTHOR_NAME:-github-actions[bot]}"
git config user.email "${GIT_AUTHOR_EMAIL:-41898282+github-actions[bot]@users.noreply.github.com}"

echo "==> Rewriting ${FORMULA}"
python3 "$FORMULA_UPDATER" "$FORMULA" "$TAG" "$ARM_SHA" "$X86_SHA"

git diff --check
if git diff --quiet -- "$FORMULA"; then
  fail "formula already matches ${TAG}; refusing to open an empty PR"
fi

echo "==> Diff preview"
git --no-pager diff -- "$FORMULA"

echo "==> Creating branch ${BRANCH}"
git checkout -b "$BRANCH"
git add "$FORMULA"
git commit -m "hearth: bump to ${TAG}"

echo "==> Pushing branch ${BRANCH}"
if ! git push --set-upstream origin "$BRANCH"; then
  fail "failed to push ${BRANCH}; verify HOMEBREW_TAP_TOKEN has contents write access to ${TAP_REPO}"
fi

echo "==> Opening PR"
# shellcheck disable=SC2016 # Backticks are intentional Markdown delimiters.
PR_BODY="$(printf 'Automated bump to [`%s`](https://github.com/%s/releases/tag/%s).\n\n- arm64 sha256: `%s`\n- x86_64 sha256: `%s`\n\nGenerated by `.agents/tap-bump/bump-to-tag.sh` in Naoray/hearth.\nDo NOT auto-merge — maintainer approves.' \
  "$TAG" "$SOURCE_REPO" "$TAG" "$ARM_SHA" "$X86_SHA")"

if ! PR_URL="$(gh pr create \
  --repo "$TAP_REPO" \
  --base main \
  --head "$BRANCH" \
  --title "hearth: bump to ${TAG}" \
  --body "$PR_BODY")"; then
  fail "branch ${BRANCH} was pushed, but opening the pull request failed; verify HOMEBREW_TAP_TOKEN has Pull requests write access"
fi

echo "==> Done. PR opened on ${TAP_REPO}; maintainer must approve and merge."
echo "$PR_URL"
