#!/usr/bin/env bash
# Recheck a signed release manifest against its successful build run and live tag.
# This helper must run from the workflow SHA checked out by the publisher job.
set -euo pipefail

tag=${1:?usage: verify-release-publication-identity.sh <tag> <manifest>}
manifest=${2:?usage: verify-release-publication-identity.sh <tag> <manifest>}

[[ ${GITHUB_REF:-} == refs/heads/main ]] || {
  echo 'publication identity check must run from a main-branch dispatch' >&2
  exit 1
}
workflow_checkout_sha=$(git -C "$GITHUB_WORKSPACE" rev-parse --verify HEAD)
expected_workflow_sha=${EXPECTED_WORKFLOW_SHA:-${GITHUB_SHA:-}}
[[ $workflow_checkout_sha == "$expected_workflow_sha" ]] || {
  echo 'publication helper checkout does not match the workflow revision' >&2
  exit 1
}

run_id="$(jq -er '.run_id | select(type == "string" and test("^[0-9]{1,20}$"))' "$manifest")"
workflow_run="$RUNNER_TEMP/release-workflow-run.json"
gh api "repos/$GITHUB_REPOSITORY/actions/runs/$run_id" >"$workflow_run"
run_head_sha="$(jq -er \
  --arg run_id "$run_id" --arg repo "$GITHUB_REPOSITORY" \
  'select((.id | tostring) == $run_id
    and .status == "completed" and .conclusion == "success"
    and .path == ".github/workflows/release.yml"
    and .head_repository.full_name == $repo) | .head_sha' \
  "$workflow_run")"

remote="$(git -C "$GITHUB_WORKSPACE" ls-remote origin "refs/tags/$tag" "refs/tags/$tag^{}")"
tag_object="$(awk -v ref="refs/tags/$tag" '$2 == ref {print $1}' <<<"$remote")"
peeled="$(awk -v ref="refs/tags/$tag^{}" '$2 == ref {print $1}' <<<"$remote")"
[[ -n $tag_object && -n $peeled ]] || {
  echo 'release tag is missing or no longer annotated' >&2
  exit 1
}

verification="$RUNNER_TEMP/release-manifest-attestation.json"
gh attestation verify "$manifest" \
  --repo "$GITHUB_REPOSITORY" \
  --signer-workflow "$GITHUB_REPOSITORY/.github/workflows/release.yml" \
  --source-digest "$run_head_sha" \
  --deny-self-hosted-runners \
  --format json >"$verification"
python3 "$GITHUB_WORKSPACE/packaging/release.py" verify-publication-identity \
  --release-manifest "$manifest" \
  --verification "$verification" \
  --workflow-run "$workflow_run" \
  --tag "$tag" \
  --tag-object "$tag_object" \
  --commit "$peeled" \
  --repository "$GITHUB_REPOSITORY" \
  --server-url "$GITHUB_SERVER_URL"
jq -cn --arg tag_object "$tag_object" --arg commit "$peeled" \
  '{tag_object: $tag_object, commit: $commit}' \
  >"$RUNNER_TEMP/release-tag-identity.json"
