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

# The lazygit-suite git keys must parse and validate.
[git]
override_gpg = true

[[git_commands]]
key = "p"
context = "branches"
command = "git push {{.SelectedBranch.Name | quote}}"
output = "popup"
prompts = [{ type = "input", title = "Remote", key = "Remote" }]

# Per-sandbox VPN config must parse + validate (provider sub-tables included).
[sandbox.vpn]
provider = "tailscale"
mode = "sidecar"
dns = "tunnel"

[sandbox.vpn.tailscale]
auth_key = "env:TS_AUTHKEY"
tags = ["tag:dev"]

# Ingress sharing config must parse + validate (all provider sub-tables).
[share]
provider = "bore"
allow_public = true

[share.frp]
server_addr = "frps.example.com"
subdomain_host = "share.example.com"

[share.tailscale]
funnel = false
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
check "sandbox vpn config parses and surfaces the provider" \
  "'$SZ' config show | grep -q 'tailscale'"
check "config get reads a nested vpn key" \
  "[[ \$('$SZ' config get sandbox.vpn.provider 2>/dev/null) == 'tailscale' || -n \$('$SZ' config show | grep -A2 'sandbox.vpn') ]]"

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

# ci (AV group): detection finds a seeded workflow file; runs/detect degrade
# gracefully with no remote/provider (exit 0, never crash).
mkdir -p "$WT/.github/workflows"
echo "on: push" >"$WT/.github/workflows/ci.yml"
check "ci detect finds the seeded GitHub Actions workflow" \
  "'$SZ' ci detect --worktree '$WT' | grep -q 'GitHub Actions'"
check "ci runs degrades gracefully (exit 0)" \
  "'$SZ' ci runs --worktree '$WT' >/dev/null 2>&1"

# list works against the DB (empty here is fine — must not error).
check "list runs without error" \
  "'$SZ' list >/dev/null 2>&1"
check "recent runs without error" \
  "'$SZ' recent >/dev/null 2>&1"

# Named execution environments: list the library and resolve one for a worktree.
check "env list reports the default env" \
  "'$SZ' env list | grep -q 'default env:'"
check "env show resolves an environment for a worktree" \
  "'$SZ' env show '$WT' | grep -q '^env:'"
check "env set/show round-trips a selection" \
  "'$SZ' env set company-k8s '$WT' >/dev/null 2>&1 && '$SZ' env show '$WT' >/dev/null 2>&1"

# ── ingress sharing (`[share]`) ──────────────────────────────────────────────
# The config parses (all provider sub-tables, exercised by validate above).
check "share config round-trips through config show" \
  "'$SZ' config show | grep -q 'allow_public'"
check "share list runs without error" \
  "'$SZ' share list >/dev/null 2>&1"

# Stubbed providers exercise the subprocess seam: `frpc`/`dumbpipe` on a private
# PATH stand in for the real binaries (each prints its line, then idles).
SHBIN="$TMP/shbin"
mkdir -p "$SHBIN"
cat >"$SHBIN/frpc" <<'STUB'
#!/usr/bin/env bash
echo "frpc started: $*"; sleep 30
STUB
cat >"$SHBIN/dumbpipe" <<'STUB'
#!/usr/bin/env bash
echo "to connect, use: dumbpipe connect-tcp TICKET123" >&2; sleep 30
STUB
chmod +x "$SHBIN/frpc" "$SHBIN/dumbpipe"

# frp: config-derived https subdomain URL + a materialized frpc.toml.
cat >"$TMP/share-frp.toml" <<EOF
[share]
provider = "frp"
[share.frp]
server_addr = "frps.example.com"
subdomain_host = "share.example.com"
EOF
PATH="$SHBIN:$PATH" "$SZ" --config "$TMP/share-frp.toml" share start 3000 --worktree "$WT" \
  >"$TMP/frp.out" 2>&1 &
FRP_PID=$!
for _ in $(seq 1 60); do
  if grep -q '→' "$TMP/frp.out" 2>/dev/null; then break; fi
  sleep 0.1
done
check "share frp derives the per-worktree https URL" \
  "grep -q 'https://feature-3000.share.example.com' '$TMP/frp.out'"
check "share frp materializes frpc.toml in the state dir" \
  "ls $XDG_STATE_HOME/superzej/share/*-3000/frpc.toml >/dev/null 2>&1"
kill "$FRP_PID" 2>/dev/null || true
wait "$FRP_PID" 2>/dev/null || true

# iroh: scrape the dumbpipe ticket into a copyable connect command.
printf '[share]\nprovider = "iroh"\n' >"$TMP/share-iroh.toml"
PATH="$SHBIN:$PATH" "$SZ" --config "$TMP/share-iroh.toml" share start 3000 --worktree "$WT" \
  >"$TMP/iroh.out" 2>&1 &
IROH_PID=$!
for _ in $(seq 1 60); do
  if grep -q '→' "$TMP/iroh.out" 2>/dev/null; then break; fi
  sleep 0.1
done
check "share iroh scrapes the dumbpipe ticket into a connect command" \
  "grep -q 'dumbpipe connect-tcp TICKET123' '$TMP/iroh.out'"
kill "$IROH_PID" 2>/dev/null || true
wait "$IROH_PID" 2>/dev/null || true

# allow_public safety guard: a public share is refused unless opted in.
cat >"$TMP/share-guard.toml" <<EOF
[share]
provider = "frp"
allow_public = false
[share.frp]
server_addr = "x"
subdomain_host = "y"
EOF
check "share allow_public guard refuses public shares" \
  "'$SZ' --config '$TMP/share-guard.toml' share start 3000 --worktree '$WT' 2>&1 | grep -q 'public sharing is disabled'"

# v5 → v6 layout migration: seed a legacy flat tab_layout (pages as " ·N" name
# suffixes) into the state DB, open it once, and assert it transformed into
# worktree groups (tabs-within-a-worktree) with the legacy table dropped.
if command -v sqlite3 >/dev/null 2>&1; then
  DB="$XDG_STATE_HOME/superzej/superzej.db"
  mkdir -p "$(dirname "$DB")"
  sqlite3 "$DB" <<'SQL'
PRAGMA user_version = 5;
CREATE TABLE IF NOT EXISTS tab_layout (
  session_name TEXT, tab_name TEXT, kind TEXT, worktree TEXT,
  pane_tree TEXT, ordinal INTEGER, focused_pane INTEGER,
  PRIMARY KEY (session_name, tab_name));
INSERT INTO tab_layout VALUES
  ('/r', 'app/home',    'home',     '/r',       '{"leaf":0}', 0, 0),
  ('/r', 'app/feat',    'worktree', '/wt/feat', '{"leaf":1}', 1, 0),
  ('/r', 'app/feat ·2', 'worktree', '/wt/feat', '{"leaf":2}', 2, 0);
SQL
  "$SZ" list >/dev/null 2>&1 || true
  groups="$(sqlite3 "$DB" "SELECT count(*) FROM tab_groups WHERE session_name='/r'")"
  feat_tabs="$(sqlite3 "$DB" "SELECT count(*) FROM group_tabs WHERE session_name='/r' AND group_name='app/feat'")"
  legacy="$(sqlite3 "$DB" "SELECT count(*) FROM sqlite_master WHERE name='tab_layout'")"
  check "v5 tab_layout migrates into worktree groups (v6)" "[[ '$groups' -eq 2 ]]"
  check "page suffixes become tabs within the worktree" "[[ '$feat_tabs' -eq 2 ]]"
  check "legacy tab_layout is dropped after migration" "[[ '$legacy' -eq 0 ]]"
else
  echo "  skip v5→v6 migration check (sqlite3 not on PATH)"
fi

echo
if [[ $fail -eq 0 ]]; then
  printf '\033[32mall smoke checks passed\033[0m\n'
else
  printf '\033[31msmoke test FAILED\033[0m\n'
  exit 1
fi
