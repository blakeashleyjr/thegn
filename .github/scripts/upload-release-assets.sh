#!/usr/bin/env bash
set -euo pipefail

if (($# < 1)); then
  echo "usage: upload-release-assets.sh <asset>..." >&2
  exit 2
fi

tag="${TAG:?TAG is required}"
repository="${GH_REPO:-${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}}"
python="${PYTHON:-python3}"
export GH_TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:?GITHUB_TOKEN is required}}"

"$python" packaging/release.py require-draft \
  --is-draft "$(gh release view "$tag" --json isDraft --jq .isDraft)"
release_id="$(gh api "repos/$repository/releases/tags/$tag" --jq .id)"
[[ $release_id =~ ^[0-9]+$ ]] || {
  echo "::error::could not resolve the draft release ID"
  exit 1
}
asset_inventory="$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/release-assets.XXXXXX.json")"
trap 'rm -f -- "$asset_inventory"' EXIT
gh api --paginate --slurp \
  "repos/$repository/releases/$release_id/assets?per_page=100" >"$asset_inventory"

plan_args=(plan-upload --assets-json "$asset_inventory")
for asset in "$@"; do
  plan_args+=(--asset "$asset")
done
plan="$("$python" packaging/release.py "${plan_args[@]}")"
mapfile -t upload_assets < <(jq -r '.[]' <<<"$plan")
if ((${#upload_assets[@]})); then
  # No --clobber: a concurrent same-name upload fails closed.
  gh release upload "$tag" "${upload_assets[@]}"
else
  echo "::notice::all requested release assets already match their SHA-256 digests"
fi
