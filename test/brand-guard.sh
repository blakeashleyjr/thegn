#!/usr/bin/env bash
# test/brand-guard.sh — the product is thegn; the pre-rename brand (superzej,
# szhost, sj, SZ_*, …) must not reappear. Scans every tracked text file for
# old-brand tokens and fails on any hit outside the pinned allowlist.
#
# Sanctioned survivors:
#   - the migration code (its whole job is naming the old paths),
#   - the smoke test that seeds old-named dirs to exercise that migration,
#   - install.sh's sweep of pre-rename entry-point symlinks,
#   - the migration call-site comments in host main.rs,
#   - `blakeashleyjr/superzej` GitHub URLs in the termite apps' sz-kit git
#     pins (pre-rename tags; retire when tg-kit-v0.1.x is tagged — allowed by
#     line content, not by file, so anything else in those files is guarded).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

re='superzej|SUPERZEJ|Superzej|SuperZej|szhost|SZHOST|szproxy|SZPROXY|sz-kit|sz_kit|\bsj\b|\bsj-tui\b|\bSZ_[A-Z0-9_]+'

allow_files=(
  ':!test/brand-guard.sh'
  ':!crates/thegn-core/src/migrate_brand.rs'
  ':!test/smoke.sh'
  ':!install.sh'
  ':!crates/thegn-host/src/main.rs'
)

hits="$(git grep -InE "$re" -- . "${allow_files[@]}" | grep -v 'blakeashleyjr/superzej' || true)"

if [[ -n $hits ]]; then
  echo "ERROR: pre-rename brand token found — the product is 'thegn' (binary thegn, alias tg, THEGN_* env):" >&2
  printf '%s\n' "$hits" >&2
  echo "(sanctioned exceptions live in test/brand-guard.sh)" >&2
  exit 1
fi
echo "brand-guard: clean"
