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

# `sha256sum` is GNU coreutils; macOS ships `shasum` instead. Any stable digest
# will do — this only compares one pass against the next.
if command -v sha256sum >/dev/null 2>&1; then
  digest() { sha256sum; }
elif command -v shasum >/dev/null 2>&1; then
  digest() { shasum -a 256; }
else
  echo "prettier-stable: need sha256sum or shasum on PATH" >&2
  exit 1
fi

prev=$(cat "$@" | digest)
for _ in 1 2 3 4 5; do
  prettier --write --log-level warn "$@"
  cur=$(cat "$@" | digest)
  if [ "$cur" = "$prev" ]; then
    exit 0
  fi
  prev=$cur
done

echo "prettier-stable: no fixed point after 5 passes: $*" >&2
exit 1
