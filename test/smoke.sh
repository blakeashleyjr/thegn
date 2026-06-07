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
# Register workspaces without cold-starting a real zellij session.
export SUPERZEJ_NO_EXEC=1
export HOME="$TMP" XDG_CONFIG_HOME="$TMP/.config" XDG_STATE_HOME="$TMP/.local/state"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

mkdir -p "$XDG_CONFIG_HOME/superzej"
cat >"$XDG_CONFIG_HOME/superzej/config.toml" <<EOF
worktrees_dir = "$TMP/wt"
name_scheme = "numbered"
repo_roots = ["$TMP/code"]

[[pins]]
name = "aerc"
command = "echo aerc"

[[pins]]
name = "logs"
command = "echo logs"
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

"$SZ" new-worktree --name "demo feature" >/dev/null 2>&1
"$SZ" new-worktree >/dev/null 2>&1
check "worktrees created on disk" \
  "[[ \$(git -C '$R' worktree list | grep -c sz-) -eq 2 ]]"

# Collision suffixing: same human name twice -> sz/demo, sz/demo-1.
"$SZ" new-worktree --name demo >/dev/null 2>&1
"$SZ" new-worktree --name demo >/dev/null 2>&1
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

# The panel plugin maps the focused (session, tab) -> worktree path via the DB.
# Worktrees here were recorded under the "default" session (no ZELLIJ_SESSION_NAME);
# tabs are named `{repo_slug}/{branch}` (repo basename is "repo" -> slug "repo").
RESOLVED="$("$SZ" resolve-worktree --session default --tab repo/sz-demo-feature)"
check "resolve-worktree maps (session,tab) -> path" \
  "[[ -n '$RESOLVED' && '$RESOLVED' == \"$WT\" ]]"

# panel-snapshot: the panel's fast first paint — one process returns the resolved
# worktree plus whatever is cached. After a `diff --files` warms the cache, the
# snapshot carries the file list too.
check "panel-snapshot resolves the worktree as JSON" \
  "[[ \$('$SZ' panel-snapshot --session default --tab repo/sz-demo-feature | jq -r '.worktree') == \"$WT\" ]]"
"$SZ" diff --files --worktree "$WT" --base main >/dev/null 2>&1
check "panel-snapshot carries cached diff files after a diff" \
  "'$SZ' panel-snapshot --session default --tab repo/sz-demo-feature | jq -e '.files | length > 0' >/dev/null"
# restore-session is a graceful no-op outside a live session (it needs zellij).
check "restore-session degrades gracefully outside a session (exit 0)" \
  "'$SZ' restore-session >/dev/null 2>&1"

# The data-plane commands degrade gracefully on a repo with no remote / no gh PR
# (must exit 0 so the panel never sees a crash).
check "pr status degrades gracefully (exit 0)" \
  "'$SZ' pr status --worktree '$WT' >/dev/null 2>&1"
check "pr status --json emits a kind tag" \
  "'$SZ' pr status --worktree '$WT' --json | jq -e '.kind' >/dev/null"
check "diff emits without error" \
  "'$SZ' diff --worktree '$WT' --base main >/dev/null 2>&1"

# Theme + activity feeds for the plugins: a valid accent triple, and a complete
# TSV that never errors (the sidebar tolerates an empty map but not a crash).
check "theme prints an R;G;B accent" \
  "'$SZ' theme | grep -Eq '^[0-9]+;[0-9]+;[0-9]+$'"
check "activity exits 0 and is well-formed" \
  "'$SZ' activity >/dev/null 2>&1"

# Resource-monitor command resolution (top-bar stats -> embedded monitor):
# cpu/mem map to the system monitor, gpu to the gpu monitor. Not in a zellij
# session here, so each reports the command it would embed instead of spawning.
check "monitor cpu -> system monitor (default btm)" \
  "'$SZ' monitor cpu 2>&1 | grep -q btm"
check "monitor mem -> system monitor (default btm)" \
  "'$SZ' monitor mem 2>&1 | grep -q btm"
check "monitor gpu -> gpu monitor (default nvtop)" \
  "'$SZ' monitor gpu 2>&1 | grep -q nvtop"
check "monitor rejects an unknown stat (exit non-zero)" \
  "! '$SZ' monitor bogus >/dev/null 2>&1"

# Removal keeps the branch, drops the worktree, reconciles state.
SUPERZEJ_WORKTREE="$WT" "$SZ" close-worktree --force >/dev/null 2>&1
check "worktree removed" "[[ ! -d '$WT' ]]"
check "branch kept after removal" \
  "git -C '$R' show-ref --verify --quiet refs/heads/sz/demo-feature"
check "removed worktree gone from list" \
  "[[ \$('$SZ' list --json | jq -r '[.[] | select(.branch==\"sz/demo-feature\")] | length') -eq 0 ]]"

# ── file-manager drawer (`superzej files`) — hermetic surface only ──────────
# In-session behavior (spawn/close/restore) is covered by test/files-drawer.py;
# here we confirm the command is wired and degrades gracefully off-session.
check "files subcommand exists" "'$SZ' files --help >/dev/null 2>&1"
check "files off-session exits 0 (no zellij side effects)" "'$SZ' files >/dev/null 2>&1"
check "files off-session reports it needs a session" \
  "'$SZ' files 2>&1 | grep -qi 'not in zellij'"
check "files --close off-session is safe" "'$SZ' files --close >/dev/null 2>&1"

# ── pinned programs (`superzej pin`) — hermetic surface only ────────────────
# Launch-or-focus / tab lifecycle is covered in-session by the e2e suite; here
# we confirm config wiring (list) and graceful off-session degradation.
check "pin list emits configured pins (TSV)" \
  "'$SZ' pin list | grep -qE '^1'$'\t''aerc'"
check "pin list --json is an indexed array" \
  "'$SZ' pin list --json | grep -q '\"index\":1,\"name\":\"aerc\"'"
check "pin open off-session exits 0 (no zellij side effects)" \
  "'$SZ' pin open aerc >/dev/null 2>&1"
check "pin open off-session reports it needs a session" \
  "'$SZ' pin open aerc 2>&1 | grep -qi 'not in zellij'"
check "pin open resolves by 1-based index" \
  "'$SZ' pin open 2 2>&1 | grep -q logs"
check "pin open rejects an unknown pin (exit non-zero)" \
  "! '$SZ' pin open nope >/dev/null 2>&1"

echo
if [[ $fail -eq 0 ]]; then
  printf '\033[32mall smoke checks passed\033[0m\n'
else
  printf '\033[31msmoke test FAILED\033[0m\n'
  exit 1
fi
