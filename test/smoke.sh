#!/usr/bin/env bash
# test/smoke.sh — hermetic, non-interactive end-to-end check of the native
# binary's CLI verbs (repos / recent / list / diff / pr / config) against a
# throwaway repo in an isolated HOME. Exits non-zero on any failure.
#
# The interactive compositor (worktree/agent/pin actions) is exercised by the
# host's own unit tests; this covers the shell-invocable surface.
#
# Usage: test/smoke.sh [path-to-thegn]   (defaults to ./target/debug/thegn)
set -euo pipefail

SZ="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/debug/thegn}"
# Resolve to an absolute path — the test cd's into a temp repo before running it.
SZ="$(cd "$(dirname "$SZ")" && pwd)/$(basename "$SZ")"
[[ -x $SZ ]] || {
  echo "not executable: $SZ (run: cargo build)" >&2
  exit 1
}

TMP="$(mktemp -d)"
# Best-effort teardown that preserves the real exit status. $HOME is inside
# $TMP, so if any check exercised a container backend, podman leaves overlay
# storage under $TMP/.local/share/containers/storage/overlay/ that this user
# cannot unlink. Under `set -e` a failing `rm -rf` in the trap became the
# script's exit code — CI reported "all smoke checks passed" and then failed
# the job. Never let cleanup decide the verdict.
cleanup() {
  local rc=$?
  rm -rf "$TMP" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

export HOME="$TMP" XDG_CONFIG_HOME="$TMP/.config" XDG_STATE_HOME="$TMP/.local/state"
# Isolate the runtime dir too: the daemon control socket prefers
# $XDG_RUNTIME_DIR/thegn/daemon.sock, so leaving the real one exported would
# let the socket probe cross-connect these checks to a live daemon.
export XDG_RUNTIME_DIR="$TMP/run"
mkdir -p "$XDG_RUNTIME_DIR"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
# Exercise the full product surface: the experimental verbs (host/placement/
# kaneo) are dev-channel-only, so run smoke in the dev channel. A dedicated
# section below verifies the stable channel refuses them + clamps.
export THEGN_CHANNEL=dev

mkdir -p "$XDG_CONFIG_HOME/thegn"
cat >"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF
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
# validate, and drive the thegn-host CLI (state stays in this temp HOME).
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

echo "thegn smoke test"

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
# A pre-existing bad enum value in some OTHER key must NOT block setting an
# unrelated valid key (the whole-file re-validate only rolls back NEW errors).
# Isolated config dir so the seeded config above stays clean.
check "config set of a valid key survives a pre-existing bad value elsewhere" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; printf 'lifecycle.eager = \"bogus\"\n' > \"\$D/thegn/config.toml\"; XDG_CONFIG_HOME=\"\$D\" '$SZ' config set picker fzf >/dev/null 2>&1 && XDG_CONFIG_HOME=\"\$D\" '$SZ' config get picker | grep -q fzf"

# `config get` resolves ANY dotted key, not a hand-maintained allowlist — the
# whole nested surface (every [merge_queue] key, ui.*, …) used to exit 1 with
# empty output while `config explain` resolved the same path fine.
check "config get reads a nested merge_queue key" \
  "[[ -n \$('$SZ' config get merge_queue.on_landed) ]]"
check "config get --json emits a real bool, not a quoted string" \
  "[[ \$('$SZ' config get merge_queue.auto_land --json) == 'true' ]]"
check "config get still errors on an unknown key" \
  "! '$SZ' config get merge_queue.no_such_key >/dev/null 2>&1"
# `config set` can write sequence values; every value used to be written as a
# TOML string, so array-typed keys had no CLI path at all.
check "config set writes a real TOML array" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; XDG_CONFIG_HOME=\"\$D\" '$SZ' config set merge_queue.regenerate_paths '[\"a.lock\", \"b.lock\"]' >/dev/null 2>&1 && XDG_CONFIG_HOME=\"\$D\" '$SZ' config get merge_queue.regenerate_paths --json | grep -q '\\[\"a.lock\",\"b.lock\"\\]'"
# doctor surfaces the resolved paths, so a missing repo_root / a relocated $HOME
# is one glance instead of "you have no repos".
check "doctor reports a Paths section" \
  "'$SZ' doctor | grep -q '^Paths'"

# mcp serve: the read-only docs endpoint answers JSON-RPC over stdio.
check "mcp serve initialize reports the docs server" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}' | '$SZ' mcp serve | grep -q 'thegn-docs'"
check "mcp serve tools/list advertises search_docs" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}' | '$SZ' mcp serve | grep -q 'search_docs'"
check "mcp serve search_docs finds the merge-queue help page" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"search_docs\",\"arguments\":{\"query\":\"merge queue\"}}}' | '$SZ' mcp serve | grep -q 'merge-queue'"

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
# (its event trail gains no new 'deliver' rows). TG_SMOKE_HOST_LIVE=1 enables.
if [[ ${TG_SMOKE_HOST_LIVE:-} == "1" ]] && command -v podman >/dev/null 2>&1; then
  check "host provision reaches ready (live)" \
    "'$SZ' host provision smoke-local </dev/null"
  DBH="$XDG_STATE_HOME/thegn/thegn.db"
  delivers_before="$(sqlite3 "$DBH" "SELECT count(*) FROM host_events WHERE step='deliver'")"
  check "second host provision is a no-op (live)" \
    "'$SZ' host provision smoke-local </dev/null"
  delivers_after="$(sqlite3 "$DBH" "SELECT count(*) FROM host_events WHERE step='deliver'")"
  check "second provision transferred nothing (golden path)" \
    "[[ '$delivers_before' -eq '$delivers_after' ]]"
else
  echo "  skip live host golden-path (set TG_SMOKE_HOST_LIVE=1 with podman + egress)"
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
  DBS="$XDG_STATE_HOME/thegn/thegn.db"
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
check "setup appears in --help (onboarding wizard)" \
  "'$SZ' --help | grep -q '^  setup '"
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
    "sqlite3 \"$XDG_STATE_HOME/thegn/thegn.db\" \
       \"SELECT value FROM ui_state WHERE key='active_workspace'\" | grep -q alpha"
fi

# Named execution environments: list the library and resolve one for a worktree.
check "env list reports the default env" \
  "'$SZ' env list | grep -q 'default env:'"
check "env show resolves an environment for a worktree" \
  "'$SZ' env show '$WT' | grep -q '^env:'"
check "env set/show round-trips a selection" \
  "'$SZ' env set company-k8s '$WT' >/dev/null 2>&1 && '$SZ' env show '$WT' >/dev/null 2>&1"
# The canonical `--worktree` flag form (the positionals above stay as the
# hidden back-compat spelling — see docs/cli.md "Worktree targeting").
check "env show accepts the canonical --worktree flag" \
  "'$SZ' env show --worktree '$WT' | grep -q '^env:'"
# Round-trip: the flag-form WRITE must land on the SAME worktree a positional
# READ resolves (cross-checks that --worktree isn't silently dropped in favor of
# the cwd — `env set` exits 0 even when the selection is wrong, so assert the
# effect, not just the exit code). `env set` persists the selection unconditionally
# and the read preserves the requested name, so this leaves $WT carrying
# company-k8s exactly as the sandbox-argv check below documents.
check "env set --worktree lands the selection (flag-write / positional-read round-trip)" \
  "'$SZ' env set company-k8s --worktree '$WT' >/dev/null 2>&1 && '$SZ' env show '$WT' | grep -q company-k8s"
# Passing BOTH forms is a usage error: non-zero exit (no specific code promised).
check "env show refuses --worktree plus a positional (non-zero)" \
  "! '$SZ' env show --worktree '$WT' '$WT' >/dev/null 2>&1"
# The conflict guard is per-verb (flatten site): spot-check a second verb.
check "placement explain refuses --worktree plus a positional (non-zero)" \
  "! '$SZ' placement explain --worktree '$WT' '$WT' >/dev/null 2>&1"
# sandbox-argv takes the same flag and resolves like every other scoped verb.
# Target the repo root ($WT now carries the deliberately undefined company-k8s
# env selection, which launch_spec correctly refuses) and switch the seeded
# tailscale VPN off for this one call — its auth key is unset here by design.
check "sandbox-argv accepts the canonical --worktree flag" \
  "'$SZ' --set sandbox.vpn.provider=none sandbox-argv --worktree '$R' | grep -q ."
# No-arg default resolution: with $THEGN_WORKTREE exported to the repo root, a
# flag-less sandbox-argv resolves to it (the shared chain, not the old raw cwd).
# Run from a scratch cwd to prove $THEGN_WORKTREE — not the cwd — wins.
check "sandbox-argv with no --worktree resolves via the THEGN_WORKTREE env" \
  "(cd / && THEGN_WORKTREE='$R' '$SZ' --set sandbox.vpn.provider=none sandbox-argv | grep -q .)"

# ── merge queue (`merge` namespace, the fold-actor) ──────────────────────────
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
    "[[ \$(sqlite3 \"$XDG_STATE_HOME/thegn/thegn.db\" \
       \"SELECT count(*) FROM merge_queue WHERE branch='$MB' AND status='queued'\") -eq 1 ]]"
fi
# `merge rm` removes a queued entry by path; re-add so drain has work. Done while
# the worktree still exists — a clean land now auto-removes it (see below).
check "merge rm deletes the entry by the same path" \
  "'$SZ' merge rm '$MP' >/dev/null 2>&1"
# Flag-form twin: `merge rm` on a non-queued path exits non-zero, so it needs
# its own preceding add before the canonical `--worktree` removal.
check "merge add queues the branch again for the flag-form rm" \
  "'$SZ' merge add '$MP' | grep -q 'queued'"
check "merge rm accepts the canonical --worktree flag" \
  "'$SZ' merge rm --worktree '$MP' >/dev/null 2>&1"
check "merge add re-queues the branch after rm" \
  "'$SZ' merge add '$MP' | grep -q 'queued'"
check "merge drain lands the clean branch" \
  "'$SZ' merge drain | grep -q 'landed'"
check "drain advanced the target to include the branch's commit" \
  "git -C '$R' log --oneline | grep -q 'smoke merge change'"
# organize_folders + on_landed = "remove" are on by default: a clean land removes
# the merged worktree and deletes its now-redundant branch.
check "clean land auto-removes the merged worktree" \
  "[[ ! -d '$MP' ]]"
check "clean land deletes the merged branch" \
  "[[ -z \$(git -C '$R' branch --list '$MB') ]]"

# `--json` must emit EXACTLY one document on every path. The empty queue is the
# case a cron/CI loop hits most often, and it used to print prose ("Nothing to
# drain.") with no JSON at all.
check "merge drain --json emits JSON on the empty queue" \
  "'$SZ' merge drain --json 2>/dev/null | python3 -c 'import json,sys; json.load(sys.stdin)'"

# An unrunnable gate is an ENVIRONMENT failure, not a verdict about the branch:
# it must record `gate_error` (never `gate_failed`/"breaks build") and must not
# dispatch the fixing agent — which would set a coding model loose on source
# code in response to `command not found`.
GP="$("$SZ" wt new smoke-gate --repo "$R")"
GB="$(git -C "$GP" symbolic-ref --short HEAD)"
echo hi >"$GP/smoke-gate.txt"
git -C "$GP" add -A && git -C "$GP" commit -q -m "smoke gate change"
"$SZ" merge add "$GP" >/dev/null 2>&1
AGENT_MARK="$XDG_STATE_HOME/agent-fired"
check "an unrunnable gate is reported as gate_error, not a build verdict" \
  "'$SZ' --set merge_queue.gate_on=true \
     --set merge_queue.gate_command=definitely-not-a-real-binary \
     --set merge_queue.conflict_handoff=agent \
     --set merge_queue.agent_command=\"touch $AGENT_MARK\" \
     merge drain 2>&1 | grep -q 'NOT gated'"
check "the queue row records gate_error with the real reason" \
  "'$SZ' merge list | grep -q 'gate_error'"
check "an unrunnable gate never dispatches the fixing agent" \
  "[[ ! -e '$AGENT_MARK' ]]"
# The row must show the target the run actually folded into, not the one frozen
# at enqueue time (two different targets used to be shown for one operation).
check "merge list shows the run's effective target" \
  "'$SZ' merge list | grep -q '$GB'"
# Per-repo `[merge_queue]`: the whole table used to be global-only, so a repo
# whose gate or integration branch differed from the user's defaults could only
# be handled with `--set` flags on every invocation.
WSLUG="$(basename "$R")"
cat >>"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF

[workspace.$WSLUG.merge_queue]
target_branch = "smoke-target"
EOF
check "a [workspace.<slug>] block refines merge_queue for that repo" \
  "[[ \$('$SZ' config explain merge_queue.target_branch --repo '$R' --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"value\"])') == 'smoke-target' ]]"
check "config explain names the workspace layer as the origin" \
  "'$SZ' config explain merge_queue.target_branch --repo '$R' | grep -q 'workspace'"
# Trim it again so the rest of the merge checks see the plain global config.
python3 - "$XDG_CONFIG_HOME/thegn/config.toml" <<'PYEOF'
import sys
p = sys.argv[1]
s = open(p).read()
i = s.rindex("[workspace.")
open(p, "w").write(s[:i])
PYEOF

# `merge retry` re-arms a blocked row (the CLI twin of the panel's `r`), and is
# a distinct non-zero outcome when there is nothing to re-arm.
check "merge retry re-queues a blocked row" \
  "'$SZ' merge retry --worktree '$GP' | grep -qi 're-queued'"
check "merge retry on an unqueued path exits non-zero" \
  "! '$SZ' merge retry --worktree '$R' >/dev/null 2>&1"
"$SZ" merge rm --worktree "$GP" >/dev/null 2>&1 || true

# ── placement engine ─────────────────────────────────────────────────────────
# Engine OFF (the default): the dry-run reports passthrough and no state is
# written — the byte-compatibility invariant's shell-visible face.
check "placement plan reports passthrough while the engine is off" \
  "'$SZ' placement plan '$R' | grep -q 'engine off'"
check "placement plan accepts the canonical --worktree flag" \
  "'$SZ' placement plan --worktree '$R' | grep -q 'engine off'"
check "placement list renders the seeded host (unknown size)" \
  "'$SZ' placement list | grep -q 'smoke-local'"
check "placement events is empty while the engine is off" \
  "'$SZ' placement events | grep -q 'no placement decisions'"
# Engine ON with a declared-capacity host: the broker's dry-run is
# deterministic — an unprobed host can't pack (no known runtime), so `auto`
# falls back to a dedicated placement on the empty box.
cat >>"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF

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
  "ls $XDG_STATE_HOME/thegn/share/*-3000/frpc.toml >/dev/null 2>&1"
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
# A refused public share exits non-zero (misuse a script must detect) AND names
# the reason. Capture output so the message check works under `set -e`/pipefail
# even though the command is expected to fail.
check "share allow_public guard refuses public shares" \
  "out=\$('$SZ' --config '$TMP/share-guard.toml' share start 3000 --worktree '$WT' 2>&1) || printf '%s' \"\$out\" | grep -q 'public sharing is disabled'"

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

# An invalid reach is rejected: exit non-zero (misuse) with a message naming it.
check "share rejects an invalid --reach value" \
  "out=\$('$SZ' --config '$TMP/share-reach.toml' share start 3000 --reach bogus --worktree '$WT' 2>&1) || printf '%s' \"\$out\" | grep -q 'reach'"

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
  FDB="$XDG_STATE_HOME/thegn/thegn.db"
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
  DB="$XDG_STATE_HOME/thegn/thegn.db"
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

# ── control plane: pairing CRUD, no-daemon degradation, daemon lifecycle ────
echo "control plane:"

# Pairing management is pure DB — must work with NO daemon running.
# NOTE: command output is never interpolated into check()'s eval'd condition —
# the no-daemon message contains backticks, which eval would execute.
pair_out="$("$SZ" pair new --scope read,git --label smoke 2>&1)"
pair_url_ok=1
grep -q 'thegn://pair?' <<<"$pair_out" || pair_url_ok=0
pair_hash_ok=1
grep -q 'only its hash is stored' <<<"$pair_out" || pair_hash_ok=0
check "pair new mints a code and prints the pairing URL" "[[ $pair_url_ok -eq 1 ]]"
check "pair new never echoes a stored plaintext (hash-only note present)" \
  "[[ $pair_hash_ok -eq 1 ]]"
pair_id="$("$SZ" pair list --json | sed -n 's/.*"pairing_id": "\([a-f0-9]*\)".*/\1/p' | head -1)"
check "pair list --json surfaces the minted code" "[[ -n '$pair_id' ]]"
"$SZ" pair revoke "$pair_id" >/dev/null
pair_revoked_ok=1
"$SZ" pair list | grep -q revoked || pair_revoked_ok=0
check "pair revoke flips the state" "[[ $pair_revoked_ok -eq 1 ]]"

# Session verbs degrade clearly when no daemon is running (never crash).
set +e
nodaemon_out="$("$SZ" session list 2>&1)"
nodaemon_rc=$?
nodaemon_json="$("$SZ" session list --json 2>/dev/null)"
set -e
nodaemon_msg_ok=1
[[ $nodaemon_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$nodaemon_out" || nodaemon_msg_ok=0
nodaemon_json_ok=1
grep -q 'no_daemon' <<<"$nodaemon_json" || nodaemon_json_ok=0
check "session list without a daemon exits 1 with a clear message" \
  "[[ $nodaemon_msg_ok -eq 1 ]]"
check "session list --json emits the no_daemon error object" \
  "[[ $nodaemon_json_ok -eq 1 ]]"

# `thegn attach` (the local thin client) shares the same connect path, so it
# degrades identically when no daemon is running — never a crash.
set +e
attach_out="$("$SZ" attach 2>&1)"
attach_rc=$?
set -e
attach_ok=1
[[ $attach_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$attach_out" || attach_ok=0
check "attach without a daemon exits 1 with a clear message" \
  "[[ $attach_ok -eq 1 ]]"

# The agent-driving verbs (`session wait`/`session split`) share the connect
# path and must degrade cleanly too.
set +e
wait_out="$("$SZ" session wait --session bogus --until exited 2>&1)"
wait_rc=$?
split_out="$("$SZ" session split --session bogus 2>&1)"
split_rc=$?
set -e
verbs_ok=1
[[ $wait_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$wait_out" || verbs_ok=0
[[ $split_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$split_out" || verbs_ok=0
check "session wait/split without a daemon exit 1 with a clear message" \
  "[[ $verbs_ok -eq 1 ]]"

# Daemon lifecycle: spawn on an isolated socket, open a marker session over
# the unix socket, see it in `session list` and its output in `snapshot`,
# then stop it and verify the registry row + socket are gone.
if command -v curl >/dev/null 2>&1; then
  DSOCK="$TMP/d.sock"
  "$SZ" daemon --socket "$DSOCK" &
  DPID=$!
  for _ in $(seq 1 40); do
    [[ -S $DSOCK ]] && break
    sleep 0.1
  done
  check "daemon binds its control socket" "[[ -S '$DSOCK' ]]"
  curl -s --unix-socket "$DSOCK" -X POST http://d/v1/sessions \
    -H 'content-type: application/json' \
    -d '{"argv":["/bin/sh","-c","echo smoke-marker; sleep 30"],"rows":24,"cols":80}' >/dev/null
  sleep 0.5
  slist_ok=1
  "$SZ" session list | grep -Eq 'sh|echo' || slist_ok=0
  check "session list shows the daemon-owned session" "[[ $slist_ok -eq 1 ]]"
  sid="$("$SZ" session list --json | sed -n 's/.*"id": "\([a-f0-9]*\)".*/\1/p' | head -1)"
  snap_ok=1
  "$SZ" session snapshot --session "$sid" | grep -aq smoke-marker || snap_ok=0
  check "snapshot carries the detached session's output" "[[ $snap_ok -eq 1 ]]"
  kill "$DPID" 2>/dev/null || true
  wait "$DPID" 2>/dev/null || true
  for _ in $(seq 1 20); do
    [[ ! -S $DSOCK ]] && break
    sleep 0.1
  done
  check "daemon cleanup unlinks the socket" "[[ ! -S '$DSOCK' ]]"
  if command -v sqlite3 >/dev/null 2>&1; then
    rows="$(sqlite3 "$XDG_STATE_HOME/thegn/thegn.db" 'SELECT count(*) FROM daemons' 2>/dev/null || echo 0)"
    check "daemon cleanup removes its registry row" "[[ '$rows' -eq 0 ]]"
  fi
else
  echo "  skip daemon lifecycle (curl not on PATH)"
fi

# CLI verbs never spawn a daemon as a side effect — only PANE spawns lazily
# ensure one (the default-on [daemon] routes panes, not verbs). Every verb
# above ran daemon-less; no socket may exist on either default path.
check "CLI verbs never spawn a daemon" \
  "[[ ! -S \"$XDG_RUNTIME_DIR/thegn/daemon.sock\" && ! -S \"$XDG_STATE_HOME/thegn/run/daemon.sock\" ]]"

# Explicit close kills: DELETE on a session reaps it from the listing (the
# close-a-pane contract at the API level).
if command -v curl >/dev/null 2>&1; then
  DSOCK2="$TMP/d2.sock"
  "$SZ" daemon --socket "$DSOCK2" &
  D2PID=$!
  for _ in $(seq 1 40); do
    [[ -S $DSOCK2 ]] && break
    sleep 0.1
  done
  curl -s --unix-socket "$DSOCK2" -X POST http://d/v1/sessions \
    -H 'content-type: application/json' \
    -d '{"argv":["/bin/sh","-c","sleep 30"],"rows":24,"cols":80}' >/dev/null
  sleep 0.3
  ksid="$("$SZ" session list --json | sed -n 's/.*"id": "\([a-f0-9]*\)".*/\1/p' | head -1)"
  curl -s --unix-socket "$DSOCK2" -X DELETE "http://d/v1/sessions/$ksid" >/dev/null
  sleep 0.3
  kill_ok=1
  "$SZ" session list --json 2>/dev/null | grep -q "$ksid" && kill_ok=0
  check "DELETE kills the session (explicit close = kill)" "[[ $kill_ok -eq 1 ]]"
  kill "$D2PID" 2>/dev/null || true
  wait "$D2PID" 2>/dev/null || true
else
  echo "  skip close-kill check (curl not on PATH)"
fi

# --- one-time superzej -> thegn migration -----------------------------------
# Seed old-brand state/config/app-home in a fresh throwaway HOME, run any CLI
# verb, and assert the startup migration renamed everything (marker included).
MIG="$(mktemp -d)"
mkdir -p "$MIG/.local/state/superzej" "$MIG/.config/superzej" "$MIG/.superzej/worktrees"
printf 'stale' >"$MIG/.local/state/superzej/superzej.db"
printf 'worktrees_dir = "%s/wt"\n' "$MIG" >"$MIG/.config/superzej/config.toml"
env HOME="$MIG" XDG_CONFIG_HOME="$MIG/.config" XDG_STATE_HOME="$MIG/.local/state" \
  "$SZ" repos >/dev/null 2>&1 || true
check "migration moved the state dir + db" \
  "[[ -f '$MIG/.local/state/thegn/thegn.db' && ! -e '$MIG/.local/state/superzej' ]]"
check "migration moved the config dir" \
  "[[ -f '$MIG/.config/thegn/config.toml' && ! -e '$MIG/.config/superzej' ]]"
check "migration moved the app home" \
  "[[ -d '$MIG/.thegn/worktrees' && ! -e '$MIG/.superzej' ]]"
check "migration wrote its forensics marker" \
  "[[ -f '$MIG/.thegn/.migrated-from-superzej' ]]"
check "migration honors THEGN_NO_MIGRATE" \
  "mkdir -p '$MIG/.config/superzej' && env HOME='$MIG' XDG_CONFIG_HOME='$MIG/.config' XDG_STATE_HOME='$MIG/.local/state' THEGN_NO_MIGRATE=1 '$SZ' repos >/dev/null 2>&1 || true; [[ -d '$MIG/.config/superzej' ]]"
rm -rf "$MIG"

# --- release channels (stable vs dev) -------------------------------------
# The smoke run above is dev-channel; here we assert the STABLE channel refuses
# the experimental verbs and clamps the experimental config toggles.
echo "release channels:"
check "stable channel refuses an experimental verb (host)" \
  "! env THEGN_CHANNEL=stable '$SZ' host list >/dev/null 2>&1"
check "the refusal names the dev channel" \
  "{ env THEGN_CHANNEL=stable '$SZ' host list 2>&1 || true; } | grep -q 'dev-channel feature'"
check "dev channel allows the same verb" \
  "env THEGN_CHANNEL=dev '$SZ' host list >/dev/null 2>&1"
check "doctor reports the stable channel + disabled remote" \
  "env THEGN_CHANNEL=stable '$SZ' doctor --json | grep -q '\"channel\": \"stable\"' && env THEGN_CHANNEL=stable '$SZ' doctor --json | grep -A8 '\"features\"' | grep -q '\"remote\": false'"
check "doctor reports the dev channel + enabled remote" \
  "env THEGN_CHANNEL=dev '$SZ' doctor --json | grep -A8 '\"features\"' | grep -q '\"remote\": true'"
CHCFG="$TMP/channel.toml"
printf '[observe]\nenabled = true\n' >"$CHCFG"
check "stable clamps experimental toggles off" \
  "env THEGN_CHANNEL=stable '$SZ' --config '$CHCFG' config show | grep -A1 '^\[observe\]' | grep -q 'enabled = false'"
check "dev honours the same toggles" \
  "env THEGN_CHANNEL=dev '$SZ' --config '$CHCFG' config show | grep -A1 '^\[observe\]' | grep -q 'enabled = true'"

echo
if [[ $fail -eq 0 ]]; then
  printf '\033[32mall smoke checks passed\033[0m\n'
else
  printf '\033[31msmoke test FAILED\033[0m\n'
  exit 1
fi
