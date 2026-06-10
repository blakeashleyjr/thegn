#!/usr/bin/env bash
# test/smoke.sh — hermetic, non-interactive end-to-end check of the native
# binary's CLI verbs (repos / recent / list / diff / pr / config) against a
# throwaway repo in an isolated HOME. Exits non-zero on any failure.
#
# The interactive compositor (worktree/agent/pin actions) is exercised by the
# host's own unit tests; this covers the shell-invocable surface.
#
# Usage: test/smoke.sh [path-to-szhost]   (defaults to ./target/debug/szhost)
set -euo pipefail

SZ="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/debug/szhost}"
# Resolve to an absolute path — the test cd's into a temp repo before running it.
SZ="$(cd "$(dirname "$SZ")" && pwd)/$(basename "$SZ")"
[[ -x $SZ ]] || {
  echo "not executable: $SZ (run: cargo build)" >&2
  exit 1
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

export HOME="$TMP" XDG_CONFIG_HOME="$TMP/.config" XDG_STATE_HOME="$TMP/.local/state"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

mkdir -p "$XDG_CONFIG_HOME/superzej"
cat >"$XDG_CONFIG_HOME/superzej/config.toml" <<EOF
worktrees_dir = "$TMP/wt"
name_scheme = "numbered"
repo_roots = ["$TMP/code"]
EOF

fail=0
ok() { printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  fail=1
}
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# Two repos under the scan root, plus one outside it.
mkdir -p "$TMP/code"
for n in alpha beta; do
  git init -q "$TMP/code/$n"
  git -C "$TMP/code/$n" commit -q --allow-empty -m init
done
R="$TMP/repo" # this one is OUTSIDE repo_roots
git init -q "$R"
git -C "$R" commit -q --allow-empty -m init
git -C "$R" branch -M main
cd "$R"

echo "superzej smoke test"

# Directory-agnostic repo discovery: finds the two repos under the scan root,
# and not the one outside it — regardless of $PWD.
check "repos discovers repos under repo_roots" \
  "[[ \$('$SZ' repos | wc -l) -eq 2 ]]"
check "discovery is scoped to repo_roots (excludes outside repos)" \
  "! '$SZ' repos | grep -q '/repo$'"

# config: effective value lookup + validation.
check "config get returns a known key" \
  "[[ -n \$('$SZ' config get picker) ]]"
check "config validate succeeds on the seeded config" \
  "'$SZ' config validate >/dev/null 2>&1"
check "config show emits TOML" \
  "'$SZ' config show | grep -q 'worktrees_dir'"

# A hand-built worktree exercises diff/pr/list against real git state without
# the interactive host (worktree creation is a compositor action now).
WT="$TMP/wt/feature"
git -C "$R" worktree add -q -b feature "$WT" main
echo change >"$WT/f.txt"
git -C "$WT" add -A
git -C "$WT" commit -q -m work
echo more >>"$WT/f.txt"

check "diff emits without error" \
  "'$SZ' diff --worktree '$WT' --base main >/dev/null 2>&1"
check "diff --stat emits without error" \
  "'$SZ' diff --worktree '$WT' --base main --stat >/dev/null 2>&1"

# pr status degrades gracefully on a repo with no remote / no gh PR (exit 0).
check "pr status degrades gracefully (exit 0)" \
  "'$SZ' pr status --worktree '$WT' >/dev/null 2>&1"

# list works against the DB (empty here is fine — must not error).
check "list runs without error" \
  "'$SZ' list >/dev/null 2>&1"
check "recent runs without error" \
  "'$SZ' recent >/dev/null 2>&1"

echo
if [[ $fail -eq 0 ]]; then
  printf '\033[32mall smoke checks passed\033[0m\n'
else
  printf '\033[31msmoke test FAILED\033[0m\n'
  exit 1
fi
