#!/usr/bin/env bash
set -euo pipefail

tag=${TAG:?TAG is required}
export GH_TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:?GITHUB_TOKEN is required}}"

state=$(gh release view "$tag" --json isDraft --jq .isDraft)
case "$state" in
true)
  gh release edit "$tag" --draft=false --verify-tag
  state=$(gh release view "$tag" --json isDraft --jq .isDraft)
  [[ $state == false ]] || {
    echo "::error::release publication did not complete"
    exit 1
  }
  echo "::notice::published verified release $tag"
  ;;
false)
  echo "::notice::verified release $tag is already published; replay is read-only"
  ;;
*)
  echo "::error::GitHub returned an invalid release draft state"
  exit 1
  ;;
esac
