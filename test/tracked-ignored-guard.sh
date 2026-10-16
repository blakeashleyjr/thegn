#!/usr/bin/env bash
# test/tracked-ignored-guard.sh — the THE-718 change-generation claim requires
# no tracked files to match ignore rules; diff_watch.rs::no_tracked_ignored
# withdraws that claim when one does.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

offenders_file="$(mktemp)"
trap 'rm -f "$offenders_file"' EXIT

git -c core.excludesFile=/dev/null ls-files -z --cached --ignored \
  --exclude-per-directory=.gitignore >"$offenders_file"

if [[ -s $offenders_file ]]; then
  echo "ERROR: tracked files match committed ignore rules (THE-718 claim is unavailable):" >&2
  while IFS= read -r -d '' path; do
    printf '  %s\n' "$path" >&2
  done <"$offenders_file"
  exit 1
fi

echo "tracked-ignored-guard: clean"
