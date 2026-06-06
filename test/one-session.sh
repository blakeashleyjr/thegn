#!/usr/bin/env bash
# End-to-end test for the single-session model.
#
# superzej is one zellij session; every repo is a TAB named `{slug}/home` (plus
# `{slug}/{branch}` for worktrees). Opening / selecting a repo is a tab switch,
# never a session switch — so the sidebar/panel stay put and nothing teleports.
#
# Defining properties, asserted here:
#   1. Opening repos from inside the session creates ZERO new sessions.
#   2. Each opened repo becomes its own `{slug}/home` tab in the one session.
#   3. Opening the same repo twice does NOT duplicate its tab (focus, not create).
#   4. Opened repos are registered in the DB (so the sidebar lists them).
#
# The DB lives under $XDG_STATE_HOME; zellij sessions live in the global runtime
# dir. We isolate the DB (throwaway XDG_STATE_HOME) but assert against the global
# session list. HOME stays real so the `{home,worktree}-tab` layouts resolve.
# shellcheck disable=SC2015  # `cond && ok || bad` is intentional; ok never fails
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SZ="$ROOT/target/release/superzej"
CONFIG="$ROOT/config/zellij.kdl"
LAYOUT="$ROOT/layouts/superzej.kdl"
H="sz-1sess-$$" # the single (test) session
TMPHOME="$(mktemp -d)"
TMPSTATE="$TMPHOME/.local/state"
REPOPARENT="$(mktemp -d)"
FAIL=0
ok() { echo "  ✓ $1"; }
bad() {
  echo "  ✗ $1"
  FAIL=1
}

cleanup() {
  zellij delete-session "$H" --force >/dev/null 2>&1
  rm -rf "$TMPHOME" "$REPOPARENT"
}
trap cleanup EXIT

command -v zellij >/dev/null || {
  echo "SKIP: zellij not installed"
  exit 0
}
[ -x "$SZ" ] || {
  echo "FAIL: build first (cargo build --release)"
  exit 1
}

# Run the binary as if it were a pane inside session H, with an isolated DB.
sj() { env XDG_STATE_HOME="$TMPSTATE" SUPERZEJ_CONFIG="$CONFIG" SUPERZEJ_LAYOUT="$LAYOUT" \
  ZELLIJ=0 ZELLIJ_SESSION_NAME="$H" ZELLIJ_PANE_ID=1 SUPERZEJ_SESSION="$H" \
  timeout 20 "$SZ" "$@"; }
tabs_of_H() { ZELLIJ_SESSION_NAME="$H" timeout 5 zellij action query-tab-names 2>/dev/null; }
slug() { basename "$1" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9]\+/-/g; s/^-//; s/-$//'; }

echo "== setup =="
mkA() {
  local d="$REPOPARENT/$1"
  mkdir -p "$d"
  git -C "$d" init -q
  git -C "$d" -c user.email=t@e -c user.name=t commit -q --allow-empty -m init
  echo "$d"
}
REPO_A="$(mkA sz1-alpha-$$)"
REPO_B="$(mkA sz1-bravo-$$)"
SLUG_A="$(slug "$REPO_A")"
SLUG_B="$(slug "$REPO_B")"
ok "two throwaway repos: $SLUG_A, $SLUG_B"

# The single session (plain config so it persists detached; HOME real for layouts).
env XDG_STATE_HOME="$TMPSTATE" zellij attach --create-background "$H" </dev/null >/dev/null 2>&1
sleep 1
zellij list-sessions -s --no-formatting 2>/dev/null | grep -qx "$H" && ok "session '$H' up" || bad "session failed to start"

before="$(zellij list-sessions -s --no-formatting 2>/dev/null | sort)"

echo "== act: open repo A, repo B, then A again — all from inside '$H' =="
sj new-workspace "$REPO_A" >/dev/null 2>&1
sleep 1
sj new-workspace "$REPO_B" >/dev/null 2>&1
sleep 1
sj new-workspace "$REPO_A" >/dev/null 2>&1
sleep 1 # second time: must NOT duplicate

after="$(zellij list-sessions -s --no-formatting 2>/dev/null | sort)"
names="$(tabs_of_H)"

echo "== assert =="
[ "$before" = "$after" ] && ok "opening repos created NO new sessions (one session, no teleport)" ||
  {
    bad "session list changed:"
    diff <(echo "$before") <(echo "$after") | sed 's/^/      /'
  }

echo "$names" | grep -qx "$SLUG_A/home" && ok "repo A is the tab '$SLUG_A/home'" || bad "missing tab '$SLUG_A/home' (got: $(echo "$names" | tr '\n' ' '))"
echo "$names" | grep -qx "$SLUG_B/home" && ok "repo B is the tab '$SLUG_B/home'" || bad "missing tab '$SLUG_B/home'"

dups="$(echo "$names" | grep -cx "$SLUG_A/home")"
[ "$dups" = 1 ] && ok "re-opening repo A focused its tab (no duplicate: count=$dups)" || bad "repo A tab duplicated (count=$dups)"

reg="$(env XDG_STATE_HOME="$TMPSTATE" "$SZ" workspaces 2>/dev/null | cut -f3 | sort)"
echo "$reg" | grep -qx "$REPO_A" && echo "$reg" | grep -qx "$REPO_B" &&
  ok "both repos registered in the DB (sidebar lists them)" || bad "repos not both registered"

echo
if [ "$FAIL" = 0 ]; then echo "PASS"; else
  echo "FAIL"
  exit 1
fi
