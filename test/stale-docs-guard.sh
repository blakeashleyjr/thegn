#!/usr/bin/env bash
# test/stale-docs-guard.sh — architecture claims that were once true and keep
# creeping back into docs and doc-comments. Each token below names something
# that is NOT the case any more:
#   Vt100Emulator / "vt100 crate"  — the emulator is alacritty_terminal
#   russh                          — no native ssh backend ever landed; ssh is the CLI
#   "no IPC"                       — the pane daemon + control API are IPC
#   "CI, every push"               — muse e2e is opt-in in CI ([ci-e2e])
#   "file-size ratchet"/"size ratchet" — removed; test/*-ratchet.txt are the gates
# Bare `vt100` is deliberately NOT a token: it is a real TERM name used in
# config, termcaps and their tests.
#
# Sanctioned survivors: this script, CHANGELOG.md (history), dated design
# records under docs/superpowers/, openspec change folders (proposals quote
# the stale text they fix), and the openspec archive.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

re='Vt100Emulator|vt100 crate|\brussh\b|\bno IPC\b|CI, every push|file-size ratchet|size ratchet'

allow_files=(
  ':!test/stale-docs-guard.sh'
  ':!CHANGELOG.md'
  ':!docs/superpowers/**'
  ':!openspec/changes/**'
  ':!deny.toml'
  ':!crates/thegn-core/tests/crate_boundaries.rs'
)

# Lines that *describe the ban* are the one legitimate mention of the banned
# names (the architecture doc, the architecture-gates spec, deny.toml).
hits="$(git grep --untracked -InE "$re" -- . "${allow_files[@]}" | grep -vE 'banned outright|ban .* outright|\[\[bans\.deny\]\]' || true)"

if [[ -n $hits ]]; then
  echo "ERROR: stale architecture claim — see the token list in test/stale-docs-guard.sh:" >&2
  printf '%s\n' "$hits" >&2
  exit 1
fi
echo "stale-docs-guard: clean"
