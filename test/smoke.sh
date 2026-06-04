#!/usr/bin/env bash
# test/smoke.sh — hermetic, non-interactive end-to-end check of the git/worktree
# logic. Runs superzej against a throwaway repo in an isolated HOME, with zellij
# disabled so nothing spawns in a live session. Exits non-zero on any failure.
#
# Usage: test/smoke.sh [path-to-superzej]   (defaults to ./bin/superzej)
set -euo pipefail

SZ="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/debug/superzej}"
# Resolve to an absolute path — the test cd's into a temp repo before running it.
SZ="$(cd "$(dirname "$SZ")" && pwd)/$(basename "$SZ")"
[[ -x $SZ ]] || {
  echo "not executable: $SZ (run: cargo build)" >&2
  exit 1
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

unset ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID
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
R="$TMP/repo" # this one is OUTSIDE SZ_REPO_ROOTS
git init -q "$R"
git -C "$R" commit -q --allow-empty -m init
git -C "$R" branch -M main
cd "$R"

echo "superzej smoke test"

# Directory-agnostic repo discovery: finds the two repos under the scan root,
# and not the one outside it — regardless of $PWD.
check "repos discovers repos under SZ_REPO_ROOTS" \
  "[[ \$('$SZ' repos | wc -l) -eq 2 ]]"
check "discovery is scoped to SZ_REPO_ROOTS (excludes outside repos)" \
  "! '$SZ' repos | grep -q '/repo$'"

# Explicit path avoids the interactive picker (no tty in the test).
"$SZ" new-workspace "$R" >/dev/null 2>&1
check "new-workspace records repo in history (recents)" \
  "'$SZ' recent | grep -qx '$R'"
# Open another so recents ordering is exercised; most-recent first.
"$SZ" new-workspace "$TMP/code/alpha" >/dev/null 2>&1
check "most-recently-opened repo is first in recents" \
  "[[ \$('$SZ' recent | head -1) == '$TMP/code/alpha' ]]"

"$SZ" new-pane --name "demo feature" >/dev/null 2>&1
"$SZ" new-pane >/dev/null 2>&1
check "worktrees created on disk" \
  "[[ \$(git -C '$R' worktree list | grep -c sz-) -eq 2 ]]"

# Collision suffixing: same human name twice -> sz/demo, sz/demo-1.
"$SZ" new-pane --name demo >/dev/null 2>&1
"$SZ" new-pane --name demo >/dev/null 2>&1
check "branch collisions are suffixed (-1)" \
  "git -C '$R' show-ref --verify --quiet refs/heads/sz/demo-1"

# Dirty / ahead counts surface in list --json.
WT="$("$SZ" list --json | jq -r '[.[] | select(.branch=="sz/demo-feature")][0].path')"
echo change >"$WT/f.txt"
git -C "$WT" add -A
git -C "$WT" commit -q -m work
echo more >>"$WT/f.txt"
AHEAD="$("$SZ" list --json | jq -r '[.[] | select(.branch=="sz/demo-feature")][0].ahead')"
DIRTY="$("$SZ" list --json | jq -r '[.[] | select(.branch=="sz/demo-feature")][0].dirty')"
check "ahead count is correct (1)" "[[ '$AHEAD' -eq 1 ]]"
check "dirty count is correct (1)" "[[ '$DIRTY' -eq 1 ]]"

# Removal keeps the branch, drops the worktree, reconciles state.
SUPERZEJ_WORKTREE="$WT" "$SZ" close-pane --remove-worktree --force >/dev/null 2>&1
check "worktree removed" "[[ ! -d '$WT' ]]"
check "branch kept after removal" \
  "git -C '$R' show-ref --verify --quiet refs/heads/sz/demo-feature"
check "removed worktree gone from list" \
  "[[ \$('$SZ' list --json | jq -r '[.[] | select(.branch==\"sz/demo-feature\")] | length') -eq 0 ]]"

echo
if [[ $fail -eq 0 ]]; then
  printf '\033[32mall smoke checks passed\033[0m\n'
else
  printf '\033[31msmoke test FAILED\033[0m\n'
  exit 1
fi
