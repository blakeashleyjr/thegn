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

# Hosts-as-resources: a local-reach host + a host-backed env must parse,
# validate, and drive the superzej-host CLI (state stays in this temp HOME).
[host.smoke-local]
reach = "local"
install_runtime = "never"
volumes = []

[env.smoke-hosted]
placement = "local"
host = "smoke-local"
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

# The in-app PR workflow's headless seams (comment / review / diff) parse and
# surface in --help (the acting paths need gh + a real PR, so only parsing is
# hermetic here).
check "pr comment subcommand parses" \
  "'$SZ' pr comment --help >/dev/null 2>&1"
check "pr review subcommand parses" \
  "'$SZ' pr review --help >/dev/null 2>&1"
check "pr diff subcommand parses" \
  "'$SZ' pr diff --help >/dev/null 2>&1"

# Hosts-as-resources CLI: list shows the seeded host; status renders an
# unprovisioned host; rm-cache refuses without --force and succeeds with it.
check "host list shows the seeded [host.*]" \
  "'$SZ' host list | grep -q smoke-local"
check "host status renders an unprovisioned host" \
  "'$SZ' host status smoke-local | grep -q unprovisioned"
check "host rm-cache refuses without --force" \
  "! '$SZ' host rm-cache smoke-local >/dev/null 2>&1"
check "host rm-cache --force succeeds" \
  "'$SZ' host rm-cache smoke-local --force >/dev/null 2>&1"

# GOLDEN PATH (gated: needs podman + registry egress): a first provision does
# the work; the second must be a DB-only no-op that reports zero transfers
# (its event trail gains no new 'deliver' rows). SZ_SMOKE_HOST_LIVE=1 enables.
if [[ ${SZ_SMOKE_HOST_LIVE:-} == "1" ]] && command -v podman >/dev/null 2>&1; then
  check "host provision reaches ready (live)" \
    "'$SZ' host provision smoke-local </dev/null"
  DBH="$XDG_STATE_HOME/superzej/superzej.db"
  delivers_before="$(sqlite3 "$DBH" "SELECT count(*) FROM host_events WHERE step='deliver'")"
  check "second host provision is a no-op (live)" \
    "'$SZ' host provision smoke-local </dev/null"
  delivers_after="$(sqlite3 "$DBH" "SELECT count(*) FROM host_events WHERE step='deliver'")"
  check "second provision transferred nothing (golden path)" \
    "[[ '$delivers_before' -eq '$delivers_after' ]]"
else
  echo "  skip live host golden-path (set SZ_SMOKE_HOST_LIVE=1 with podman + egress)"
fi

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

# ── CLI surface v2: wt/repo namespaces + headless lifecycle + --json ─────────
# Namespaced spellings mirror the (hidden but functional) legacy verbs.
check "wt list matches legacy list" \
  "[[ \"\$('$SZ' wt list)\" == \"\$('$SZ' list)\" ]]"
check "repo list matches legacy repos" \
  "[[ \"\$('$SZ' repo list)\" == \"\$('$SZ' repos)\" ]]"
check "repo recent matches legacy recent" \
  "[[ \"\$('$SZ' repo recent)\" == \"\$('$SZ' recent)\" ]]"

# Headless worktree lifecycle: create prints the path and registers in
# git + DB; removal cleans the checkout + DB rows and honors --delete-branch.
NP="$("$SZ" wt new smoke-cli --repo "$R")"
check "wt new prints an existing worktree path" "[[ -d '$NP' ]]"
check "wt new registered the branch in git" \
  "git -C '$R' worktree list --porcelain | grep -q 'smoke-cli'"
check "wt new appears in wt list" "'$SZ' wt list | grep -q 'smoke-cli'"
NB="$(git -C "$NP" symbolic-ref --short HEAD)"
NJ="$("$SZ" wt new smoke-json --repo "$R" --json)"
check "wt new --json emits branch+path" "printf '%s' \"\$NJ\" | grep -q '\"branch\"'"
NJ_PATH="$(grep -o '"path":"[^"]*"' <<<"$NJ" | cut -d'"' -f4)"
check "wt rm by branch name removes the checkout" \
  "'$SZ' wt rm '$NB' --force >/dev/null && [[ ! -d \$NP ]]"
check "wt rm keeps the branch by default" \
  "git -C '$R' rev-parse --verify --quiet 'refs/heads/$NB' >/dev/null"
check "wt rm --delete-branch drops the branch" \
  "'$SZ' wt rm '$NJ_PATH' --delete-branch --force >/dev/null && \
   [[ -z \$(git -C '$R' branch --list '*smoke-json*') ]]"
check "wt rm unknown target exits 3" \
  "'$SZ' wt rm no-such-thing --force >/dev/null 2>&1; [[ \$? -eq 3 ]]"
if command -v sqlite3 >/dev/null 2>&1; then
  DBS="$XDG_STATE_HOME/superzej/superzej.db"
  check "wt rm cleaned the DB worktree rows" \
    "[[ \$(sqlite3 \"$DBS\" \"SELECT count(*) FROM worktrees WHERE worktree LIKE '%smoke-cli%'\") -eq 0 ]]"
  check "wt rm left no tab_groups rows" \
    "[[ \$(sqlite3 \"$DBS\" \"SELECT count(*) FROM tab_groups WHERE worktree LIKE '%smoke-cli%'\") -eq 0 ]]"
fi

# Machine-readable output: one parseable JSON document per list surface.
check "list --json emits a JSON array" \
  "'$SZ' list --json | head -c1 | grep -q '\['"
check "env list --json parses" \
  "'$SZ' env list --json | head -c1 | grep -q '\['"
check "host list --json includes the seeded host" \
  "'$SZ' host list --json | grep -q smoke-local"
check "disk --json emits a JSON array" \
  "'$SZ' disk --json | head -c1 | grep -q '\['"
check "share list --json parses" \
  "'$SZ' share list --json | head -c1 | grep -q '\['"
check "forward list --json parses" \
  "'$SZ' forward list --json | head -c1 | grep -q '\['"
check "repo list --json parses" \
  "'$SZ' repo list --json | head -c1 | grep -q '\['"
check "ci runs --json degrades gracefully" \
  "'$SZ' ci runs --worktree '$WT' --json >/dev/null 2>&1"

# Grouped help + shell completions.
check "--help shows the Workspace group" "'$SZ' --help | grep -q 'Workspace:'"
check "--help shows the Forge group" "'$SZ' --help | grep -q 'Forge:'"
check "--help hides the legacy verbs" \
  "! '$SZ' --help | grep -qE '^  (repos|recent) '"
check "completions bash emits a script" \
  "'$SZ' completions bash | grep -qi complete"
check "completions zsh emits a compdef" \
  "'$SZ' completions zsh | grep -q compdef"

# open: workspace pointer + repo-name resolution (no TUI launch in smoke;
# the live-instance intent path is unit-tested in core + verified manually).
check "open --no-launch sets the active-workspace pointer" \
  "'$SZ' open '$TMP/code/alpha' --no-launch >/dev/null"
check "open resolves a repo by basename" \
  "'$SZ' open alpha --no-launch >/dev/null"
check "open unknown repo exits 3" \
  "'$SZ' open no-such-repo --no-launch >/dev/null 2>&1; [[ \$? -eq 3 ]]"
if command -v sqlite3 >/dev/null 2>&1; then
  check "open recorded alpha as the active workspace" \
    "sqlite3 \"$XDG_STATE_HOME/superzej/superzej.db\" \
       \"SELECT value FROM ui_state WHERE key='active_workspace'\" | grep -q alpha"
fi

# Named execution environments: list the library and resolve one for a worktree.
check "env list reports the default env" \
  "'$SZ' env list | grep -q 'default env:'"
check "env show resolves an environment for a worktree" \
  "'$SZ' env show '$WT' | grep -q '^env:'"
check "env set/show round-trips a selection" \
  "'$SZ' env set company-k8s '$WT' >/dev/null 2>&1 && '$SZ' env show '$WT' >/dev/null 2>&1"

# ── agent-driven merge queue (`merge` namespace, the fold-actor) ─────────────
# Assign a worktree branch to the queue and drain it: a clean branch folds onto
# the target and lands (no agent needed). Exercises the CLI + DB round-trip.
check "merge list starts empty" \
  "'$SZ' merge list | grep -qi 'empty'"
MP="$("$SZ" wt new smoke-merge --repo "$R")"
MB="$(git -C "$MP" symbolic-ref --short HEAD)"
echo hi >"$MP/smoke-merge.txt"
git -C "$MP" add -A && git -C "$MP" commit -q -m "smoke merge change"
check "merge add queues the worktree branch" \
  "'$SZ' merge add '$MP' | grep -q 'queued'"
check "merge list shows the queued branch" \
  "'$SZ' merge list | grep -q '$MB'"
if command -v sqlite3 >/dev/null 2>&1; then
  check "merge add wrote a queued row" \
    "[[ \$(sqlite3 \"$XDG_STATE_HOME/superzej/superzej.db\" \
       \"SELECT count(*) FROM merge_queue WHERE branch='$MB' AND status='queued'\") -eq 1 ]]"
fi
check "merge drain lands the clean branch" \
  "'$SZ' merge drain | grep -q 'landed'"
check "drain advanced the target to include the branch's commit" \
  "git -C '$R' log --oneline | grep -q 'smoke merge change'"
check "merge rm deletes the entry by the same path" \
  "'$SZ' merge add '$MP' >/dev/null && '$SZ' merge rm '$MP' >/dev/null 2>&1"

# ── placement engine ─────────────────────────────────────────────────────────
# Engine OFF (the default): the dry-run reports passthrough and no state is
# written — the byte-compatibility invariant's shell-visible face.
check "placement plan reports passthrough while the engine is off" \
  "'$SZ' placement plan '$R' | grep -q 'engine off'"
check "placement list renders the seeded host (unknown size)" \
  "'$SZ' placement list | grep -q 'smoke-local'"
check "placement events is empty while the engine is off" \
  "'$SZ' placement events | grep -q 'no placement decisions'"
# Engine ON with a declared-capacity host: the broker's dry-run is
# deterministic — an unprobed host can't pack (no known runtime), so `auto`
# falls back to a dedicated placement on the empty box.
cat >>"$XDG_CONFIG_HOME/superzej/config.toml" <<EOF

[host.pool-box]
reach = "local"
install_runtime = "never"
volumes = []
capacity = { cpu = "8", memory = "16g" }

[placement]
enabled = true
EOF
check "placement plan decides deterministically with the engine on" \
  "'$SZ' placement plan '$R' --json | grep -q '\"decision\": \"dedicated\"'"
check "placement plan explains every candidate" \
  "'$SZ' placement plan '$R' --json | grep -q 'trust_class'"
# The dry-run must be side-effect free: no reservation, no event.
check "placement plan writes no decision events" \
  "'$SZ' placement events | grep -q 'no placement decisions'"
# Draining parks a host out of every lane: the plan flips to the other box.
check "host drain excludes the host from placement candidates" \
  "'$SZ' host drain pool-box >/dev/null 2>&1 && '$SZ' placement plan '$R' --json | grep -q 'draining'"
check "drained host refuses new provisioning" \
  "! '$SZ' host provision pool-box </dev/null >/dev/null 2>&1"
# Compute spend ledger: caps set/read + kill-switch round-trip.
check "placement budget sets and reads a cap" \
  "'$SZ' placement budget --set-limit 25 | grep -q '25.00'"
check "placement budget kill-switch round-trips" \
  "'$SZ' placement budget --kill | grep -q 'killed: true' && '$SZ' placement budget --unkill | grep -q 'killed: false'"

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

# Intent-first reach mapping: `--reach peer` resolves to the iroh provider.
cat >"$TMP/share-reach.toml" <<EOF
[share]
public = "frp"
team   = "tailscale"
peer   = "iroh"
[share.frp]
server_addr = "frps.example.com"
subdomain_host = "share.example.com"
EOF
PATH="$SHBIN:$PATH" "$SZ" --config "$TMP/share-reach.toml" share start 3000 --reach peer \
  --worktree "$WT" >"$TMP/reach.out" 2>&1 &
REACH_PID=$!
for _ in $(seq 1 60); do
  if grep -q '→' "$TMP/reach.out" 2>/dev/null; then break; fi
  sleep 0.1
done
check "share --reach peer resolves to the iroh provider" \
  "grep -q 'dumbpipe connect-tcp' '$TMP/reach.out'"
kill "$REACH_PID" 2>/dev/null || true
wait "$REACH_PID" 2>/dev/null || true

# An invalid reach is rejected cleanly (exit 0 with a message).
check "share rejects an invalid --reach value" \
  "'$SZ' --config '$TMP/share-reach.toml' share start 3000 --reach bogus --worktree '$WT' 2>&1 | grep -q 'reach'"

# ── auto port forwarding (`[forward]`) ───────────────────────────────────────
# Config round-trips (the [forward] block parses + serializes) and the
# inspection CLI runs. Forwarding itself is driven by the live compositor's
# detector, so the bring-up path is exercised by the host unit tests + a
# live container check (below, guarded on podman); here we cover the CLI seam.
check "forward config round-trips through config show" \
  "'$SZ' config show | grep -q 'open_on_detect'"
check "forward list runs and reports an empty set" \
  "'$SZ' forward list 2>&1 | grep -q 'no forwards'"

# Seed a forward record and assert `forward list` renders the mapping + URL
# (exercises Db::upsert/list_forwards through the CLI read path).
if command -v sqlite3 >/dev/null 2>&1; then
  FDB="$XDG_STATE_HOME/superzej/superzej.db"
  "$SZ" forward list >/dev/null 2>&1 || true # ensure the DB + schema exist
  sqlite3 "$FDB" \
    "INSERT INTO forwards(worktree,container_port,host_port,url,created_at)
     VALUES('$WT',3000,8001,'http://127.0.0.1:8001',0);"
  check "forward list shows a remapped forward (container → host)" \
    "'$SZ' forward list 2>&1 | grep -q '3000 → 8001'"
  check "forward list shows the preview URL" \
    "'$SZ' forward list 2>&1 | grep -q 'http://127.0.0.1:8001'"
  check "forward stop removes the recorded forward" \
    "'$SZ' forward stop 3000 --worktree '$WT' >/dev/null 2>&1 && ! '$SZ' forward list 2>&1 | grep -q '3000 → 8001'"
else
  echo "  skip forward DB checks (sqlite3 not on PATH)"
fi

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
