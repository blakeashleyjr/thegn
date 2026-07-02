#!/usr/bin/env bash
# treefmt shim for prettier (see treefmt.toml).
#
# prettier's markdown printer (observed on 3.8.3) is not idempotent: when a
# code span wraps across a line break, the reformatted continuation line can
# re-parse as a link-reference definition, so each pass moves the text again.
# Under pre-commit's --fail-on-change that fails a commit several times in a
# row. Re-run prettier until the output stops changing so a single treefmt
# invocation always lands on the fixed point (worst case seen: 3 passes).
set -euo pipefail

[ "$#" -gt 0 ] || exit 0

prev=$(cat "$@" | sha256sum)
for _ in 1 2 3 4 5; do
  prettier --write --log-level warn "$@"
  cur=$(cat "$@" | sha256sum)
  if [ "$cur" = "$prev" ]; then
    exit 0
  fi
  prev=$cur
done

echo "prettier-stable: no fixed point after 5 passes: $*" >&2
exit 1
