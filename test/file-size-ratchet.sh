#!/usr/bin/env bash
# file-size-ratchet.sh — stop god-files from growing.
#
# Every Rust source file is capped at HARD_CAP lines. Legacy files already
# over the cap are pinned at their recorded size in test/file-size-ratchet.txt
# and may only stay put or shrink; any growth fails `just lint`. When you
# *reduce* a pinned file, run with --update to lower its ceiling so the win is
# locked in. Adding a new entry to the ratchet file (i.e. letting a new file
# blow past the cap) is a reviewed decision, not something this script does.
#
# Rationale (2026-07 audit): run.rs had reached ~23k lines. Extract new
# Section/feature key handlers into src/handlers/<area>.rs instead of
# appending to run.rs.
#
# Usage: test/file-size-ratchet.sh [--update]
set -euo pipefail

cd "$(dirname "$0")/.."
HARD_CAP=3000
RATCHET_FILE=test/file-size-ratchet.txt

if [[ ${1:-} == "--update" ]]; then
  tmp=$(mktemp)
  {
    echo "# lines path — ceilings for legacy files over ${HARD_CAP} lines (see file-size-ratchet.sh)"
    find crates -path '*/src/*' -name '*.rs' -print0 |
      xargs -0 wc -l |
      awk -v cap="$HARD_CAP" '$2 != "total" && $1 > cap {print $1, $2}' |
      sort -k2
  } >"$tmp"
  mv "$tmp" "$RATCHET_FILE"
  echo "ratchet updated: $RATCHET_FILE"
  exit 0
fi

if [[ ! -f $RATCHET_FILE ]]; then
  echo "ERROR: $RATCHET_FILE missing — run $0 --update and commit it" >&2
  exit 1
fi

fail=0
while read -r lines path; do
  ceiling=$(awk -v p="$path" '$2 == p {print $1}' "$RATCHET_FILE")
  if [[ -n $ceiling ]]; then
    if ((lines > ceiling)); then
      echo "ERROR: $path grew to $lines lines (ratchet ceiling: $ceiling)." >&2
      echo "       Extract code into a sibling module instead of growing this file." >&2
      fail=1
    fi
  elif ((lines > HARD_CAP)); then
    echo "ERROR: $path is $lines lines (hard cap: $HARD_CAP for files not in $RATCHET_FILE)." >&2
    echo "       Split it into modules; do not add new god-files." >&2
    fail=1
  fi
done < <(find crates -path '*/src/*' -name '*.rs' -print0 |
  xargs -0 wc -l | awk '$2 != "total" {print $1, $2}')

# Stale entries (file shrank under the cap or was deleted/split): tell the
# author to lock in the win.
while read -r ceiling path; do
  [[ $ceiling == \#* ]] && continue
  if [[ ! -f $path ]] || (($(wc -l <"$path") <= HARD_CAP)); then
    echo "NOTE: $path is gone or under the hard cap — run $0 --update to drop its ratchet entry."
  fi
done <"$RATCHET_FILE"

exit "$fail"
