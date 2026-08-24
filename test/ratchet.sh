#!/usr/bin/env bash
# test/ratchet.sh — a shrink-only, file-level grep ratchet.
#
#   test/ratchet.sh <name> <grep -E pattern> <pathspec…>
#
# Reads test/<name>-ratchet.txt (one repo-relative file per line; `#` lines and
# blanks ignored), greps the pathspecs for the pattern (comment-only lines are
# dropped, so prose naming a forbidden pattern never trips it), and fails on:
#   - a file that matches but is not pinned      → new violation
#   - a pinned file that no longer matches       → stale entry (lists only shrink)
#
# RATCHET_UPDATE=1 rewrites the allowlist from the current hit set, keeping the
# leading `#` header block verbatim (that is where the rule and the burn-down
# target are explained — write them there). `just ratchet-update` runs every
# ratchet this way. The Rust twin for predicates that need more than a regex is
# `thegn_core::test_support::ratchet::file_ratchet`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ $# -lt 3 ]]; then
  echo "usage: test/ratchet.sh <name> <grep -E pattern> <pathspec…>" >&2
  exit 2
fi
name="$1"
pat="$2"
shift 2
file="test/${name}-ratchet.txt"

# Files with at least one non-comment match.
mapfile -t hits < <(git grep --untracked -InE "$pat" -- "$@" |
  grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' |
  cut -d: -f1 | sort -u)

if [[ ${RATCHET_UPDATE:-} == 1 ]]; then
  header=""
  if [[ -f $file ]]; then
    header="$(awk '/^[[:space:]]*(#|$)/ { print; next } { exit }' "$file")"
  fi
  {
    if [[ -n $header ]]; then
      printf '%s\n\n' "$header"
    fi
    printf '%s\n' "${hits[@]}"
  } >"$file"
  echo "ratchet($name): rewrote $file (${#hits[@]} pinned)"
  exit 0
fi

mapfile -t allow < <(grep -vE '^[[:space:]]*(#|$)' "$file" 2>/dev/null | sed 's/[[:space:]]*$//' | sort -u || true)
reason="$(grep -m1 '^#' "$file" 2>/dev/null | sed 's/^#[[:space:]]*//' || true)"

status=0
mapfile -t new < <(comm -23 <(printf '%s\n' "${hits[@]}") <(printf '%s\n' "${allow[@]}"))
mapfile -t stale < <(comm -13 <(printf '%s\n' "${hits[@]}") <(printf '%s\n' "${allow[@]}"))
for f in "${new[@]}"; do
  [[ -z $f ]] && continue
  echo "ERROR: ratchet($name): new violation in $f — $reason" >&2
  echo "       fix the file, or pin it in $file with a reason (the list only shrinks)" >&2
  status=1
done
for f in "${stale[@]}"; do
  [[ -z $f ]] && continue
  echo "ERROR: ratchet($name): stale entry $f — it no longer matches; delete it from $file (shrink-only)" >&2
  status=1
done
if [[ $status == 0 ]]; then
  echo "ratchet($name): clean (${#allow[@]} pinned)"
fi
exit $status
